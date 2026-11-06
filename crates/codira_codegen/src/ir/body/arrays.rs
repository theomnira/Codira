//! Copyright (c) 2026 Omnira CJSC
//! Date: November 5, 2026
//!
//! Functionality:
//! - Code generation for the builtin methods of arrays (`push`, `pop`,
//!   `insert`, `sort`, `map`, `filter`, `fold`, ...; see
//!   `codira_hir::ArrayMethod`) and for slicing (`xs[lo..hi]`).
//!
//! # Layout and growth
//!
//! An array value is a handle to a GC object whose first word points at the
//! payload: the length, the capacity, then the elements (`ir::array`).
//! Growing moves the payload to a bigger buffer through the runtime
//! (`codira.array_grow`), which updates that first word; the handle, and so
//! every reference to the array, stays the same. Generated code therefore
//! never keeps an element address across anything that can grow the array:
//! every access reloads the payload pointer from the handle after the last
//! piece of user code it depends on has run.
//!
//! `push` is inline: a length and capacity load, a compare, the store and
//! the new length, with the call to the runtime on a cold path when the
//! buffer is full (the capacity at least doubles, so `n` pushes copy O(n)
//! elements in total).
//!
//! # Closures compiled in place
//!
//! `map`, `filter`, `fold`, `any` and `all` take a function. When the
//! argument is a closure literal without `return`
//! ([`codira_hir::is_inlined_closure_literal`]), its body is generated
//! inside the loop with its parameters bound to the element (and the
//! accumulator): no function value is created and nothing is called, so the
//! loop is what the same loop written by hand compiles to. A named function
//! is called directly (LLVM may inline it). Any other function value is
//! evaluated once, before the loop, and called through on every element.
//!
//! # Changing the array while iterating it
//!
//! The loops (`for x in xs` included) read the length once, before the first
//! iteration, and stop at the smaller of that and the *current* length: an
//! element pushed during the loop is not visited, and when the loop body
//! shortens the array the loop ends at the new end. So every element read
//! is below the current length, and every loop is bounded by the length it
//! started with.

use codira_hir::{ArrayMethod, Expr, ExprId, Ty, TyKind};
use inkwell::{
    types::BasicType,
    values::{BasicMetadataValueEnum, BasicValueEnum, IntValue, PointerValue},
    FloatPredicate, IntPredicate,
};

use super::BodyIrGenerator;
use crate::{
    intrinsics,
    ir::{
        array::RuntimeArrayValue,
        array_tbaa::{self, ArrayAccess},
    },
};

/// How a closure argument is called on each element.
pub(super) enum Callee<'ink> {
    /// A closure literal whose body is generated in place.
    Inline(ExprId),
    /// A named function, called directly.
    Direct(codira_hir::Function),
    /// A function value, evaluated once, called through.
    Value(BasicValueEnum<'ink>, Ty),
}

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// Generates `receiver.method(args)` for an array method.
    pub(super) fn gen_array_method(
        &mut self,
        call: ExprId,
        method: ArrayMethod,
        receiver: ExprId,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let receiver_value = self.gen_expr(receiver)?;
        let array = self.array_value(receiver, receiver_value);
        let elem_ty = self.infer[receiver]
            .as_array()
            .expect("an array method has an array receiver")
            .clone();
        let usize_ty = array.length_ty();
        match method {
            ArrayMethod::Push => {
                let value = self.gen_expr(args[0])?;
                let len = self.array_len(array);
                let new_len = self
                    .builder
                    .build_int_nuw_add(len, usize_ty.const_int(1, false), "len.next")
                    .expect("failed to add to a length");
                self.ensure_capacity(array, new_len);
                // One load of the payload pointer for both stores: nothing
                // between them can move the payload, and reloading it after
                // the element store (which LLVM cannot tell apart from the
                // object header without the alias classes) costs a load per
                // push.
                let payload = array.get_payload_ptr(&self.builder);
                let elements = array.elements_in(&self.builder, payload);
                // Safety: `len < capacity`, made so just above.
                let slot = unsafe {
                    self.builder
                        .build_gep(array.element_ty(), elements, &[len], "push.slot")
                        .expect("failed to build GEP for an element")
                };
                let store = self
                    .builder
                    .build_store(slot, value)
                    .expect("failed to store an element");
                array_tbaa::tag(store, ArrayAccess::Element);
                let store = self
                    .builder
                    .build_store(array.length_ptr_in(&self.builder, payload), new_len)
                    .expect("failed to store an array's length");
                array_tbaa::tag(store, ArrayAccess::Length);
                Some(self.gen_empty())
            }
            ArrayMethod::Pop | ArrayMethod::Last => {
                let optional_ty = self.infer[call].clone();
                let len = self.array_len(array);
                let present = self
                    .builder
                    .build_int_compare(IntPredicate::NE, len, usize_ty.const_zero(), "nonempty")
                    .expect("failed to compare a length");
                Some(self.gen_optional_if(&optional_ty, present, |this| {
                    let last = this
                        .builder
                        .build_int_sub(len, usize_ty.const_int(1, false), "last")
                        .expect("failed to subtract from a length");
                    let value = this.load_element(array, last);
                    if method == ArrayMethod::Pop {
                        this.set_array_len(array, last);
                    }
                    value
                }))
            }
            ArrayMethod::Insert => {
                let index = self.gen_expr(args[0])?.into_int_value();
                let value = self.gen_expr(args[1])?;
                let len = self.array_len(array);
                let index = self.gen_checked_index(call, args[0], index, len, true);
                let new_len = self
                    .builder
                    .build_int_nuw_add(len, usize_ty.const_int(1, false), "len.next")
                    .expect("failed to add to a length");
                self.ensure_capacity(array, new_len);
                let tail = self
                    .builder
                    .build_int_sub(len, index, "tail")
                    .expect("failed to subtract indices");
                let next = self
                    .builder
                    .build_int_add(index, usize_ty.const_int(1, false), "next")
                    .expect("failed to add to an index");
                let src = self.element_ptr(array, index);
                let dest = self.element_ptr(array, next);
                self.gen_element_move(array, dest, src, tail);
                self.store_element(array, index, value);
                self.set_array_len(array, new_len);
                Some(self.gen_empty())
            }
            ArrayMethod::Remove => {
                let index = self.gen_expr(args[0])?.into_int_value();
                let len = self.array_len(array);
                let index = self.gen_checked_index(call, args[0], index, len, false);
                let value = self.load_element(array, index);
                let next = self
                    .builder
                    .build_int_add(index, usize_ty.const_int(1, false), "next")
                    .expect("failed to add to an index");
                let tail = self
                    .builder
                    .build_int_sub(len, next, "tail")
                    .expect("failed to subtract indices");
                let src = self.element_ptr(array, next);
                let dest = self.element_ptr(array, index);
                self.gen_element_move(array, dest, src, tail);
                let new_len = self
                    .builder
                    .build_int_sub(len, usize_ty.const_int(1, false), "len.prev")
                    .expect("failed to subtract from a length");
                self.set_array_len(array, new_len);
                Some(value)
            }
            ArrayMethod::Clear => {
                self.set_array_len(array, usize_ty.const_zero());
                Some(self.gen_empty())
            }
            ArrayMethod::Capacity => Some(self.array_capacity(array).into()),
            ArrayMethod::IsEmpty => {
                let len = self.array_len(array);
                Some(
                    self.builder
                        .build_int_compare(IntPredicate::EQ, len, usize_ty.const_zero(), "is_empty")
                        .expect("failed to compare a length")
                        .into(),
                )
            }
            ArrayMethod::Reserve => {
                let extra = self.gen_expr(args[0])?.into_int_value();
                let extra = self.index_to_length_type(args[0], extra, usize_ty);
                let len = self.array_len(array);
                let needed = self.saturating_add(len, extra);
                self.ensure_capacity(array, needed);
                Some(self.gen_empty())
            }
            ArrayMethod::Append => {
                let other_value = self.gen_expr(args[0])?;
                let other = self.array_value(args[0], other_value);
                let count = self.array_len(other);
                let len = self.array_len(array);
                let needed = self.saturating_add(len, count);
                self.ensure_capacity(array, needed);
                // Both after the growth: `xs.extend(xs)` reads the new buffer.
                let src = self.element_ptr(other, usize_ty.const_zero());
                let dest = self.element_ptr(array, len);
                self.gen_element_move(array, dest, src, count);
                self.set_array_len(array, needed);
                Some(self.gen_empty())
            }
            ArrayMethod::Contains | ArrayMethod::IndexOf => {
                let key = self.gen_expr(args[0])?;
                let len = self.array_len(array);
                let (found, index) = self.gen_find(array, &elem_ty, key, len);
                Some(if method == ArrayMethod::Contains {
                    found.into()
                } else {
                    let optional_ty = self.infer[call].clone();
                    self.gen_optional_select(&optional_ty, found, index.into())
                })
            }
            ArrayMethod::Reverse => {
                let len = self.array_len(array);
                let data = self.element_ptr(array, usize_ty.const_zero());
                let stride = self.element_stride(array);
                self.gen_runtime_call(
                    &intrinsics::array_reverse,
                    &[data.into(), len.into(), stride.into()],
                );
                Some(self.gen_empty())
            }
            ArrayMethod::Sort => {
                let len = self.array_len(array);
                let data = self.element_ptr(array, usize_ty.const_zero());
                let kind = self.sort_kind(&elem_ty);
                self.gen_runtime_call(
                    &intrinsics::array_sort,
                    &[data.into(), len.into(), kind.into()],
                );
                Some(self.gen_empty())
            }
            ArrayMethod::BinarySearch => {
                let key = self.gen_expr(args[0])?;
                let slot = self
                    .new_alloca_builder()
                    .build_alloca(key.get_type(), "search.key")
                    .expect("failed to allocate a search key");
                self.builder
                    .build_store(slot, key)
                    .expect("failed to store a search key");
                let len = self.array_len(array);
                let data = self.element_ptr(array, usize_ty.const_zero());
                let kind = self.sort_kind(&elem_ty);
                let found = self
                    .gen_runtime_call_value(
                        &intrinsics::array_binary_search,
                        &[data.into(), len.into(), kind.into(), slot.into()],
                    )
                    .into_int_value();
                let present = self
                    .builder
                    .build_int_compare(
                        IntPredicate::SGE,
                        found,
                        found.get_type().const_zero(),
                        "found",
                    )
                    .expect("failed to compare a search result");
                let index = self
                    .builder
                    .build_int_truncate_or_bit_cast(found, usize_ty, "index")
                    .expect("failed to convert a search result");
                let optional_ty = self.infer[call].clone();
                Some(self.gen_optional_select(&optional_ty, present, index.into()))
            }
            ArrayMethod::Copy => {
                let len = self.array_len(array);
                let copy = self.alloc_array(call, len);
                let src = self.element_ptr(array, usize_ty.const_zero());
                let dest = self.element_ptr(copy, usize_ty.const_zero());
                self.gen_element_move(array, dest, src, len);
                Some(copy.into())
            }
            ArrayMethod::Sum => {
                let len = self.array_len(array);
                let zero = array.element_ty().const_zero();
                let total = self.gen_reduce(array, len, zero, |this, acc, x| match (acc, x) {
                    (BasicValueEnum::IntValue(a), BasicValueEnum::IntValue(b)) => this
                        .builder
                        .build_int_add(a, b, "sum")
                        .expect("failed to add")
                        .into(),
                    (BasicValueEnum::FloatValue(a), BasicValueEnum::FloatValue(b)) => this
                        .builder
                        .build_float_add(a, b, "sum")
                        .expect("failed to add")
                        .into(),
                    _ => unreachable!("inference allows `sum` on numbers only"),
                });
                Some(total)
            }
            ArrayMethod::Min | ArrayMethod::Max => {
                let optional_ty = self.infer[call].clone();
                let len = self.array_len(array);
                let present = self
                    .builder
                    .build_int_compare(IntPredicate::NE, len, usize_ty.const_zero(), "nonempty")
                    .expect("failed to compare a length");
                let signed = matches!(
                    elem_ty.interned(),
                    TyKind::Int(int) if int.signedness == codira_hir::Signedness::Signed
                );
                let is_min = method == ArrayMethod::Min;
                Some(self.gen_optional_if(&optional_ty, present, |this| {
                    let first = this.load_element(array, usize_ty.const_zero());
                    this.gen_reduce(array, len, first, |this, best, x| {
                        let better = match (best, x) {
                            (BasicValueEnum::IntValue(b), BasicValueEnum::IntValue(v)) => {
                                let predicate = match (is_min, signed) {
                                    (true, true) => IntPredicate::SLT,
                                    (true, false) => IntPredicate::ULT,
                                    (false, true) => IntPredicate::SGT,
                                    (false, false) => IntPredicate::UGT,
                                };
                                this.builder
                                    .build_int_compare(predicate, v, b, "better")
                                    .expect("failed to compare")
                            }
                            (BasicValueEnum::FloatValue(b), BasicValueEnum::FloatValue(v)) => {
                                let predicate = if is_min {
                                    FloatPredicate::OLT
                                } else {
                                    FloatPredicate::OGT
                                };
                                this.builder
                                    .build_float_compare(predicate, v, b, "better")
                                    .expect("failed to compare")
                            }
                            _ => unreachable!("inference allows `min`/`max` on numbers only"),
                        };
                        this.builder
                            .build_select(better, x, best, "best")
                            .expect("failed to select")
                    })
                }))
            }
            ArrayMethod::Map => {
                let callee = self.prepare_callee(args[0])?;
                let len = self.array_len(array);
                let out = self.alloc_array(call, len);
                let count = self.gen_element_loop(array, len, |this, index, element| {
                    let mapped = this.call_callee(&callee, &[element], args[0])?;
                    this.store_element(out, index, mapped);
                    Some(())
                });
                self.set_array_len(out, count);
                Some(out.into())
            }
            ArrayMethod::Filter => {
                let callee = self.prepare_callee(args[0])?;
                let len = self.array_len(array);
                let out = self.alloc_array(call, len);
                let kept = self
                    .new_alloca_builder()
                    .build_alloca(usize_ty, "filter.kept")
                    .expect("failed to allocate a counter");
                self.builder
                    .build_store(kept, usize_ty.const_zero())
                    .expect("failed to initialize a counter");
                self.gen_element_loop(array, len, |this, _, element| {
                    let keep = this
                        .call_callee(&callee, &[element], args[0])?
                        .into_int_value();
                    let keep_block = this
                        .context
                        .append_basic_block(this.fn_value, "filter.keep");
                    let next_block = this
                        .context
                        .append_basic_block(this.fn_value, "filter.next");
                    this.builder
                        .build_conditional_branch(keep, keep_block, next_block)
                        .expect("failed to branch on a filter");
                    this.builder.position_at_end(keep_block);
                    let k = this
                        .builder
                        .build_load(usize_ty, kept, "kept")
                        .expect("failed to load a counter")
                        .into_int_value();
                    this.store_element(out, k, element);
                    let k = this
                        .builder
                        .build_int_add(k, usize_ty.const_int(1, false), "kept.next")
                        .expect("failed to add");
                    this.builder
                        .build_store(kept, k)
                        .expect("failed to store a counter");
                    this.builder
                        .build_unconditional_branch(next_block)
                        .expect("failed to branch");
                    this.builder.position_at_end(next_block);
                    Some(())
                });
                let count = self
                    .builder
                    .build_load(usize_ty, kept, "kept")
                    .expect("failed to load a counter")
                    .into_int_value();
                self.set_array_len(out, count);
                Some(out.into())
            }
            ArrayMethod::Fold => {
                let init = self.gen_expr(args[0])?;
                let callee = self.prepare_callee(args[1])?;
                let acc = self
                    .new_alloca_builder()
                    .build_alloca(init.get_type(), "fold.acc")
                    .expect("failed to allocate an accumulator");
                self.builder
                    .build_store(acc, init)
                    .expect("failed to store an accumulator");
                let acc_ty = init.get_type();
                let len = self.array_len(array);
                self.gen_element_loop(array, len, |this, _, element| {
                    let current = this
                        .builder
                        .build_load(acc_ty, acc, "acc")
                        .expect("failed to load an accumulator");
                    let next = this.call_callee(&callee, &[current, element], args[1])?;
                    this.builder
                        .build_store(acc, next)
                        .expect("failed to store an accumulator");
                    Some(())
                });
                Some(
                    self.builder
                        .build_load(acc_ty, acc, "fold")
                        .expect("failed to load an accumulator"),
                )
            }
            ArrayMethod::Any | ArrayMethod::All => {
                let callee = self.prepare_callee(args[0])?;
                let is_any = method == ArrayMethod::Any;
                let bool_ty = self.context.bool_type();
                let result = self
                    .new_alloca_builder()
                    .build_alloca(bool_ty, "quantifier")
                    .expect("failed to allocate a result");
                self.builder
                    .build_store(result, bool_ty.const_int(u64::from(!is_any), false))
                    .expect("failed to store a result");
                let done = self
                    .context
                    .append_basic_block(self.fn_value, "quantifier.done");
                let len = self.array_len(array);
                self.gen_element_loop(array, len, |this, _, element| {
                    let verdict = this
                        .call_callee(&callee, &[element], args[0])?
                        .into_int_value();
                    // `any` stops at the first accepted element, `all` at the
                    // first rejected one.
                    let decided = if is_any {
                        verdict
                    } else {
                        this.builder
                            .build_not(verdict, "rejected")
                            .expect("failed to negate")
                    };
                    let stop = this
                        .context
                        .append_basic_block(this.fn_value, "quantifier.stop");
                    let next = this
                        .context
                        .append_basic_block(this.fn_value, "quantifier.next");
                    this.builder
                        .build_conditional_branch(decided, stop, next)
                        .expect("failed to branch");
                    this.builder.position_at_end(stop);
                    this.builder
                        .build_store(result, bool_ty.const_int(u64::from(is_any), false))
                        .expect("failed to store a result");
                    this.builder
                        .build_unconditional_branch(done)
                        .expect("failed to branch");
                    this.builder.position_at_end(next);
                    Some(())
                });
                self.builder
                    .build_unconditional_branch(done)
                    .expect("failed to branch");
                self.builder.position_at_end(done);
                Some(
                    self.builder
                        .build_load(bool_ty, result, "quantifier")
                        .expect("failed to load a result"),
                )
            }
            ArrayMethod::Enumerate => {
                unreachable!("inference allows `enumerate()` only as a `for` loop's iterable")
            }
        }
    }

    /// `base[lo..hi]` (or `base[lo..=hi]`): a new array holding a copy of
    /// those elements. Panics unless `lo <= hi <= len` (`hi < len` for
    /// `..=`).
    pub(super) fn gen_array_slice(
        &mut self,
        expr: ExprId,
        base: ExprId,
        start: ExprId,
        end: ExprId,
        inclusive: bool,
    ) -> Option<BasicValueEnum<'ink>> {
        let base_value = self.gen_expr(base)?;
        let array = self.array_value(base, base_value);
        let usize_ty = array.length_ty();
        let start_value = self.gen_expr(start)?.into_int_value();
        let end_value = self.gen_expr(end)?.into_int_value();
        let len = self.array_len(array);
        let end_index = self.gen_checked_index(expr, end, end_value, len, !inclusive);
        let end_index = if inclusive {
            self.builder
                .build_int_add(end_index, usize_ty.const_int(1, false), "end")
                .expect("failed to add to an index")
        } else {
            end_index
        };
        let start_index = self.index_to_length_type(start, start_value, usize_ty);
        let ordered = self
            .builder
            .build_int_compare(IntPredicate::ULE, start_index, end_index, "ordered")
            .expect("failed to compare slice bounds");
        let negative = if self.is_signed_int(start) {
            self.builder
                .build_int_compare(
                    IntPredicate::SLT,
                    start_value,
                    start_value.get_type().const_zero(),
                    "negative",
                )
                .expect("failed to compare a slice bound")
        } else {
            self.context.bool_type().const_zero()
        };
        let narrowed = if start_value.get_type().get_bit_width() > usize_ty.get_bit_width() {
            let wide_end = self
                .builder
                .build_int_z_extend(end_index, start_value.get_type(), "end.wide")
                .expect("failed to widen a slice bound");
            self.builder
                .build_int_compare(IntPredicate::UGT, start_value, wide_end, "too_far")
                .expect("failed to compare a slice bound")
        } else {
            self.context.bool_type().const_zero()
        };
        let bad = self
            .builder
            .build_or(negative, narrowed, "bad")
            .expect("failed to combine slice checks");
        let ordered = self
            .builder
            .build_and(
                ordered,
                self.builder
                    .build_not(bad, "good")
                    .expect("failed to negate"),
                "ordered",
            )
            .expect("failed to combine slice checks");
        let ok_block = self.context.append_basic_block(self.fn_value, "slice.ok");
        let fail_block = self
            .context
            .append_basic_block(self.fn_value, "slice.reversed");
        self.builder
            .build_conditional_branch(ordered, ok_block, fail_block)
            .expect("failed to branch on slice bounds");
        self.builder.position_at_end(fail_block);
        self.gen_panic("slice starts after its end", expr);
        self.builder.position_at_end(ok_block);
        let count = self
            .builder
            .build_int_sub(end_index, start_index, "count")
            .expect("failed to subtract indices");
        let slice = self.alloc_array(expr, count);
        let src = self.element_ptr(array, start_index);
        let dest = self.element_ptr(slice, usize_ty.const_zero());
        self.gen_element_move(array, dest, src, count);
        Some(slice.into())
    }

    /// Loads an array's length.
    pub(super) fn array_len(&self, array: RuntimeArrayValue<'ink>) -> IntValue<'ink> {
        let length = self
            .builder
            .build_load(
                array.length_ty(),
                array.get_length_ptr(&self.builder),
                "len",
            )
            .expect("failed to load an array's length");
        array_tbaa::tag_value(length, ArrayAccess::Length);
        length.into_int_value()
    }

    /// Stores an array's length.
    pub(super) fn set_array_len(&self, array: RuntimeArrayValue<'ink>, length: IntValue<'ink>) {
        let store = self
            .builder
            .build_store(array.get_length_ptr(&self.builder), length)
            .expect("failed to store an array's length");
        array_tbaa::tag(store, ArrayAccess::Length);
    }

    /// Loads an array's capacity (the payload header's second word, in the
    /// same alias class as the length).
    pub(super) fn array_capacity(&self, array: RuntimeArrayValue<'ink>) -> IntValue<'ink> {
        let capacity = array.get_capacity(&self.builder);
        array_tbaa::tag_value(capacity, ArrayAccess::Length);
        capacity
    }

    /// The address of element `index`, from a fresh load of the payload
    /// pointer.
    pub(super) fn element_ptr(
        &self,
        array: RuntimeArrayValue<'ink>,
        index: IntValue<'ink>,
    ) -> PointerValue<'ink> {
        let elements = array.get_elements(&self.builder);
        // Safety: callers keep `index` within the capacity (or at its end,
        // for an address only used with a zero-length move).
        unsafe {
            self.builder
                .build_gep(array.element_ty(), elements, &[index], "element_ptr")
                .expect("failed to build GEP for an element")
        }
    }

    /// Loads element `index`, which the caller has checked is below the
    /// length.
    pub(super) fn load_element(
        &self,
        array: RuntimeArrayValue<'ink>,
        index: IntValue<'ink>,
    ) -> BasicValueEnum<'ink> {
        let ptr = self.element_ptr(array, index);
        let value = self
            .builder
            .build_load(array.element_ty(), ptr, "element")
            .expect("failed to load an element");
        array_tbaa::tag_value(value, ArrayAccess::Element);
        value
    }

    /// Stores element `index`, which the caller has checked is below the
    /// capacity.
    pub(super) fn store_element(
        &self,
        array: RuntimeArrayValue<'ink>,
        index: IntValue<'ink>,
        value: BasicValueEnum<'ink>,
    ) {
        let ptr = self.element_ptr(array, index);
        let store = self
            .builder
            .build_store(ptr, value)
            .expect("failed to store an element");
        array_tbaa::tag(store, ArrayAccess::Element);
    }

    /// The distance between elements in bytes, as a `usize`.
    fn element_stride(&self, array: RuntimeArrayValue<'ink>) -> IntValue<'ink> {
        let size = array
            .element_ty()
            .size_of()
            .expect("an array element is sized");
        self.builder
            .build_int_cast(size, array.length_ty(), "stride")
            .expect("failed to convert an element size")
    }

    /// Moves `count` elements from `src` to `dest` (which may overlap).
    fn gen_element_move(
        &self,
        array: RuntimeArrayValue<'ink>,
        dest: PointerValue<'ink>,
        src: PointerValue<'ink>,
        count: IntValue<'ink>,
    ) {
        let stride = self.element_stride(array);
        let bytes = self
            .builder
            .build_int_nuw_mul(count, stride, "bytes")
            .expect("failed to multiply");
        self.builder
            .build_memmove(dest, 1, src, 1, bytes)
            .expect("failed to move elements");
    }

    /// `a + b`, saturating at the type's maximum (which no capacity reaches,
    /// so growing to it fails as too large instead of wrapping around).
    fn saturating_add(&self, a: IntValue<'ink>, b: IntValue<'ink>) -> IntValue<'ink> {
        let ty = a.get_type();
        let intrinsic = inkwell::intrinsics::Intrinsic::find("llvm.uadd.sat")
            .and_then(|i| i.get_declaration(self.module, &[ty.into()]))
            .expect("llvm.uadd.sat is always available");
        self.builder
            .build_call(intrinsic, &[a.into(), b.into()], "sum.sat")
            .expect("failed to build llvm.uadd.sat")
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value()
    }

    /// Makes sure `array` has room for `needed` elements, growing it through
    /// the runtime on a path the optimizer is told is unlikely.
    pub(super) fn ensure_capacity(
        &mut self,
        array: RuntimeArrayValue<'ink>,
        needed: IntValue<'ink>,
    ) {
        let capacity = self.array_capacity(array);
        let full = self
            .builder
            .build_int_compare(IntPredicate::UGT, needed, capacity, "full")
            .expect("failed to compare with a capacity");
        let bool_ty = self.context.bool_type();
        let expect = inkwell::intrinsics::Intrinsic::find("llvm.expect")
            .and_then(|i| i.get_declaration(self.module, &[bool_ty.into()]))
            .expect("llvm.expect is always available");
        let full = self
            .builder
            .build_call(expect, &[full.into(), bool_ty.const_zero().into()], "full")
            .expect("failed to build llvm.expect")
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let grow = self.context.append_basic_block(self.fn_value, "array.grow");
        let ready = self
            .context
            .append_basic_block(self.fn_value, "array.ready");
        self.builder
            .build_conditional_branch(full, grow, ready)
            .expect("failed to branch on a capacity");
        self.builder.position_at_end(grow);
        let allocator = self.get_allocator_handle_ptr();
        let handle = PointerValue::from(array);
        self.gen_runtime_call(
            &intrinsics::array_grow,
            &[handle.into(), needed.into(), allocator.into()],
        );
        self.builder
            .build_unconditional_branch(ready)
            .expect("failed to branch");
        self.builder.position_at_end(ready);
    }

    /// Checks `index` (the value of `index_expr`) against `bound`: `index <
    /// bound`, or `index <= bound` when `inclusive`. Out of range it panics
    /// like indexing does (`index out of bounds: the length is ..`) at
    /// `expr`. Returns the index in the length's type.
    fn gen_checked_index(
        &mut self,
        expr: ExprId,
        index_expr: ExprId,
        index: IntValue<'ink>,
        bound: IntValue<'ink>,
        inclusive: bool,
    ) -> IntValue<'ink> {
        let length_type = bound.get_type();
        let signed = self.is_signed_int(index_expr);
        let index_bits = index.get_type().get_bit_width();
        let (wide_index, wide_bound) = if index_bits > length_type.get_bit_width() {
            let wide = self
                .builder
                .build_int_z_extend(bound, index.get_type(), "bound.wide")
                .expect("failed to widen a bound");
            (index, wide)
        } else {
            (
                self.index_to_length_type(index_expr, index, length_type),
                bound,
            )
        };
        let predicate = if inclusive {
            IntPredicate::ULE
        } else {
            IntPredicate::ULT
        };
        let in_bounds = self
            .builder
            .build_int_compare(predicate, wide_index, wide_bound, "in_bounds")
            .expect("failed to build a bounds check");
        let ok_block = self.context.append_basic_block(self.fn_value, "index.ok");
        let fail_block = self
            .context
            .append_basic_block(self.fn_value, "index.out_of_bounds");
        self.builder
            .build_conditional_branch(in_bounds, ok_block, fail_block)
            .expect("failed to branch on a bounds check");

        self.builder.position_at_end(fail_block);
        let i64_type = self.context.i64_type();
        let reported_index = if index_bits > 64 {
            self.builder.build_int_truncate(index, i64_type, "index")
        } else if signed {
            self.builder
                .build_int_s_extend_or_bit_cast(index, i64_type, "index")
        } else {
            self.builder
                .build_int_z_extend_or_bit_cast(index, i64_type, "index")
        }
        .expect("failed to widen an index for a panic report");
        let reported_bound = self
            .builder
            .build_int_z_extend_or_bit_cast(bound, i64_type, "len")
            .expect("failed to widen a length for a panic report");
        let (file, line, column) = self.source_location(expr);
        let file_data = self
            .builder
            .build_extract_value(file, 0, "file.data")
            .expect("a str has a data pointer");
        let file_len = self
            .builder
            .build_extract_value(file, 1, "file.len")
            .expect("a str has a length");
        let u32_type = self.context.i32_type();
        self.gen_runtime_call(
            &intrinsics::panic_index_out_of_bounds,
            &[
                reported_index.into(),
                self.context
                    .i8_type()
                    .const_int(u64::from(signed), false)
                    .into(),
                reported_bound.into(),
                file_data.into(),
                file_len.into(),
                u32_type.const_int(u64::from(line), false).into(),
                u32_type.const_int(u64::from(column), false).into(),
            ],
        );
        self.builder
            .build_unreachable()
            .expect("failed to build unreachable after a panic");

        self.builder.position_at_end(ok_block);
        self.index_to_length_type(index_expr, index, length_type)
    }

    /// Calls a runtime function that returns a value.
    fn gen_runtime_call_value(
        &mut self,
        function: &impl intrinsics::Intrinsic,
        args: &[BasicMetadataValueEnum<'ink>],
    ) -> BasicValueEnum<'ink> {
        let (fn_ty, fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            function,
        );
        self.builder
            .build_indirect_call(fn_ty, fn_ptr, args, "")
            .expect("failed to build call to a runtime function")
            .try_as_basic_value()
            .unwrap_basic()
    }

    /// The element kind the runtime's `sort` and `binary_search` take
    /// (`codira_runtime::native_array`).
    fn sort_kind(&self, elem_ty: &Ty) -> IntValue<'ink> {
        let code = match elem_ty.interned() {
            TyKind::Int(int) => {
                let bits = self
                    .hir_types
                    .get_basic_type(elem_ty)
                    .expect("an integer is a basic type")
                    .into_int_type()
                    .get_bit_width();
                let index = match bits {
                    8 => 0,
                    16 => 1,
                    32 => 2,
                    64 => 3,
                    _ => 4,
                };
                if int.signedness == codira_hir::Signedness::Signed {
                    index
                } else {
                    index + 5
                }
            }
            TyKind::Float(float) if float.bitness == codira_hir::FloatBitness::X32 => 10,
            TyKind::Float(_) => 11,
            // `false < true`, stored as one byte holding 0 or 1.
            TyKind::Bool => 5,
            TyKind::Str => 12,
            _ => unreachable!("inference allows ordering only integers, floats, `bool` and `str`"),
        };
        self.context.i32_type().const_int(code, false)
    }

    /// Whether `a == b` for two values of the element type `ty`: integers
    /// and `bool`s bit for bit, floats by IEEE 754 (`NaN` equals nothing),
    /// `str`s byte for byte.
    fn gen_values_equal(
        &mut self,
        ty: &Ty,
        a: BasicValueEnum<'ink>,
        b: BasicValueEnum<'ink>,
    ) -> IntValue<'ink> {
        match ty.interned() {
            TyKind::Float(_) => self
                .builder
                .build_float_compare(
                    FloatPredicate::OEQ,
                    a.into_float_value(),
                    b.into_float_value(),
                    "eq",
                )
                .expect("failed to compare"),
            TyKind::Str => {
                let mut args: Vec<BasicMetadataValueEnum<'ink>> = Vec::with_capacity(4);
                for value in [a, b] {
                    let value = value.into_struct_value();
                    for index in 0..2 {
                        args.push(
                            self.builder
                                .build_extract_value(value, index, "str.part")
                                .expect("a str has a data pointer and a length")
                                .into(),
                        );
                    }
                }
                let equal = self
                    .gen_runtime_call_value(&intrinsics::str_eq, &args)
                    .into_int_value();
                self.builder
                    .build_int_compare(IntPredicate::NE, equal, equal.get_type().const_zero(), "eq")
                    .expect("failed to compare")
            }
            _ => self
                .builder
                .build_int_compare(
                    IntPredicate::EQ,
                    a.into_int_value(),
                    b.into_int_value(),
                    "eq",
                )
                .expect("failed to compare"),
        }
    }

    /// Searches the first `len` elements for one equal to `key`: whether
    /// there is one, and the index of the first (meaningful when found).
    fn gen_find(
        &mut self,
        array: RuntimeArrayValue<'ink>,
        elem_ty: &Ty,
        key: BasicValueEnum<'ink>,
        len: IntValue<'ink>,
    ) -> (IntValue<'ink>, IntValue<'ink>) {
        let usize_ty = len.get_type();
        let entry = self
            .builder
            .get_insert_block()
            .expect("the builder is positioned");
        let cond = self.context.append_basic_block(self.fn_value, "find.cond");
        let body = self.context.append_basic_block(self.fn_value, "find.body");
        let next = self.context.append_basic_block(self.fn_value, "find.next");
        let done = self.context.append_basic_block(self.fn_value, "find.done");
        self.builder
            .build_unconditional_branch(cond)
            .expect("failed to branch");

        self.builder.position_at_end(cond);
        let index = self
            .builder
            .build_phi(usize_ty, "find.index")
            .expect("failed to build a phi");
        let i = index.as_basic_value().into_int_value();
        let in_range = self
            .builder
            .build_int_compare(IntPredicate::ULT, i, len, "in_range")
            .expect("failed to compare");
        self.builder
            .build_conditional_branch(in_range, body, done)
            .expect("failed to branch");

        self.builder.position_at_end(body);
        let element = self.load_element(array, i);
        let equal = self.gen_values_equal(elem_ty, element, key);
        let body_end = self
            .builder
            .get_insert_block()
            .expect("the builder is positioned");
        self.builder
            .build_conditional_branch(equal, done, next)
            .expect("failed to branch");

        self.builder.position_at_end(next);
        let i_next = self
            .builder
            .build_int_nuw_add(i, usize_ty.const_int(1, false), "find.next")
            .expect("failed to add");
        self.builder
            .build_unconditional_branch(cond)
            .expect("failed to branch");
        index.add_incoming(&[(&usize_ty.const_zero(), entry), (&i_next, next)]);

        self.builder.position_at_end(done);
        let bool_ty = self.context.bool_type();
        let found = self
            .builder
            .build_phi(bool_ty, "found")
            .expect("failed to build a phi");
        found.add_incoming(&[
            (&bool_ty.const_zero(), cond),
            (&bool_ty.const_all_ones(), body_end),
        ]);
        (found.as_basic_value().into_int_value(), i)
    }

    /// Folds `combine` over the first `len` elements, starting from
    /// `initial` -- a loop with no user code in it, which LLVM can
    /// vectorize.
    fn gen_reduce(
        &mut self,
        array: RuntimeArrayValue<'ink>,
        len: IntValue<'ink>,
        initial: BasicValueEnum<'ink>,
        combine: impl Fn(&Self, BasicValueEnum<'ink>, BasicValueEnum<'ink>) -> BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let usize_ty = len.get_type();
        let entry = self
            .builder
            .get_insert_block()
            .expect("the builder is positioned");
        let cond = self
            .context
            .append_basic_block(self.fn_value, "reduce.cond");
        let body = self
            .context
            .append_basic_block(self.fn_value, "reduce.body");
        let done = self
            .context
            .append_basic_block(self.fn_value, "reduce.done");
        self.builder
            .build_unconditional_branch(cond)
            .expect("failed to branch");

        self.builder.position_at_end(cond);
        let index = self
            .builder
            .build_phi(usize_ty, "reduce.index")
            .expect("failed to build a phi");
        let acc = self
            .builder
            .build_phi(initial.get_type(), "reduce.acc")
            .expect("failed to build a phi");
        let i = index.as_basic_value().into_int_value();
        let in_range = self
            .builder
            .build_int_compare(IntPredicate::ULT, i, len, "in_range")
            .expect("failed to compare");
        self.builder
            .build_conditional_branch(in_range, body, done)
            .expect("failed to branch");

        self.builder.position_at_end(body);
        let element = self.load_element(array, i);
        let combined = combine(self, acc.as_basic_value(), element);
        let i_next = self
            .builder
            .build_int_nuw_add(i, usize_ty.const_int(1, false), "reduce.next")
            .expect("failed to add");
        let body_end = self
            .builder
            .get_insert_block()
            .expect("the builder is positioned");
        self.builder
            .build_unconditional_branch(cond)
            .expect("failed to branch");
        index.add_incoming(&[(&usize_ty.const_zero(), entry), (&i_next, body_end)]);
        acc.add_incoming(&[(&initial, entry), (&combined, body_end)]);

        self.builder.position_at_end(done);
        acc.as_basic_value()
    }

    /// Emits a loop over the elements of `array` that user code may run in:
    /// `for i in 0..min(len, current length)`, calling `body` with the
    /// index and the element. `len` is the length read before the loop; the
    /// current length is read on every iteration, so the loop ends early
    /// when `body` shortens the array (and LLVM folds the second test away
    /// when it can see that nothing in the loop writes a length). Returns
    /// the number of iterations that ran to completion.
    ///
    /// When `body` diverges (returns `None`, its block terminated), the rest
    /// of the iteration is unreachable.
    fn gen_element_loop(
        &mut self,
        array: RuntimeArrayValue<'ink>,
        len: IntValue<'ink>,
        mut body: impl FnMut(&mut Self, IntValue<'ink>, BasicValueEnum<'ink>) -> Option<()>,
    ) -> IntValue<'ink> {
        let usize_ty = len.get_type();
        let counter = self
            .new_alloca_builder()
            .build_alloca(usize_ty, "loop.index")
            .expect("failed to allocate a loop index");
        self.builder
            .build_store(counter, usize_ty.const_zero())
            .expect("failed to initialize a loop index");
        let cond = self.context.append_basic_block(self.fn_value, "loop.cond");
        let body_block = self.context.append_basic_block(self.fn_value, "loop.body");
        let done = self.context.append_basic_block(self.fn_value, "loop.done");
        self.builder
            .build_unconditional_branch(cond)
            .expect("failed to branch");

        self.builder.position_at_end(cond);
        let i = self
            .builder
            .build_load(usize_ty, counter, "i")
            .expect("failed to load a loop index")
            .into_int_value();
        let in_range = self.gen_live_in_range(array, i, len);
        self.builder
            .build_conditional_branch(in_range, body_block, done)
            .expect("failed to branch");

        self.builder.position_at_end(body_block);
        let element = self.load_element(array, i);
        if body(self, i, element).is_some() {
            let next = self
                .builder
                .build_int_nuw_add(i, usize_ty.const_int(1, false), "next")
                .expect("failed to add");
            self.builder
                .build_store(counter, next)
                .expect("failed to store a loop index");
            self.builder
                .build_unconditional_branch(cond)
                .expect("failed to branch");
        }

        self.builder.position_at_end(done);
        self.builder
            .build_load(usize_ty, counter, "count")
            .expect("failed to load a loop index")
            .into_int_value()
    }

    /// `index < len && index < array.len()`: the condition of every loop
    /// over an array in which user code runs (see the module docs).
    pub(super) fn gen_live_in_range(
        &self,
        array: RuntimeArrayValue<'ink>,
        index: IntValue<'ink>,
        len: IntValue<'ink>,
    ) -> IntValue<'ink> {
        let below_start = self
            .builder
            .build_int_compare(IntPredicate::ULT, index, len, "in_range")
            .expect("failed to compare a loop index");
        let current = self.array_len(array);
        let below_current = self
            .builder
            .build_int_compare(IntPredicate::ULT, index, current, "in_range.now")
            .expect("failed to compare a loop index");
        self.builder
            .build_and(below_start, below_current, "in_range.both")
            .expect("failed to combine loop conditions")
    }

    /// A `T?` of type `optional_ty`: present with what `value` generates when
    /// `present` holds (generated only on that path), else `nil`.
    pub(super) fn gen_optional_if(
        &mut self,
        optional_ty: &Ty,
        present: IntValue<'ink>,
        value: impl FnOnce(&mut Self) -> BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let optional_type = self
            .hir_types
            .get_basic_type(optional_ty)
            .expect("an optional is a basic type");
        let entry = self
            .builder
            .get_insert_block()
            .expect("the builder is positioned");
        let some = self.context.append_basic_block(self.fn_value, "opt.some");
        let merge = self.context.append_basic_block(self.fn_value, "opt.merge");
        self.builder
            .build_conditional_branch(present, some, merge)
            .expect("failed to branch");
        self.builder.position_at_end(some);
        let payload = value(self);
        let wrapped = self.wrap_optional(optional_ty, payload);
        let some_end = self
            .builder
            .get_insert_block()
            .expect("the builder is positioned");
        self.builder
            .build_unconditional_branch(merge)
            .expect("failed to branch");
        self.builder.position_at_end(merge);
        let phi = self
            .builder
            .build_phi(optional_type, "opt")
            .expect("failed to build a phi");
        phi.add_incoming(&[(&optional_type.const_zero(), entry), (&wrapped, some_end)]);
        phi.as_basic_value()
    }

    /// A `T?` of type `optional_ty`: `value` when `present`, else `nil`,
    /// without a branch.
    pub(super) fn gen_optional_select(
        &mut self,
        optional_ty: &Ty,
        present: IntValue<'ink>,
        value: BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let optional_type = self
            .hir_types
            .get_basic_type(optional_ty)
            .expect("an optional is a basic type");
        let wrapped = self.wrap_optional(optional_ty, value);
        self.builder
            .build_select(present, wrapped, optional_type.const_zero(), "opt")
            .expect("failed to select an optional")
    }

    /// Decides how the closure argument `arg` will be called, evaluating it
    /// now when it is a function value.
    pub(super) fn prepare_callee(&mut self, arg: ExprId) -> Option<Callee<'ink>> {
        let body = self.body.clone();
        if codira_hir::is_inlined_closure_literal(&body, arg) {
            return Some(Callee::Inline(arg));
        }
        if let Some(function) = self.infer.function_value(arg) {
            if matches!(body[arg], Expr::Path(_)) {
                return Some(Callee::Direct(function));
            }
        }
        let fn_ty = match self.infer[arg].interned() {
            TyKind::TypeAlias(alias) => alias.target_type(self.db),
            _ => self.infer[arg].clone(),
        };
        let value = self.gen_expr(arg)?;
        Some(Callee::Value(value, fn_ty))
    }

    /// Calls a prepared closure argument (`arg`) with `values`. Returns
    /// `None` when the call does not return.
    pub(super) fn call_callee(
        &mut self,
        callee: &Callee<'ink>,
        values: &[BasicValueEnum<'ink>],
        arg: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        match callee {
            Callee::Inline(closure) => {
                let body = self.body.clone();
                let Expr::Closure {
                    params,
                    body: closure_body,
                    ..
                } = &body[*closure]
                else {
                    unreachable!("an inlined callee is a closure literal");
                };
                for (&(pat, _), &value) in params.iter().zip(values) {
                    self.bind_pattern_to_value(pat, value);
                }
                self.gen_expr(*closure_body)
            }
            Callee::Direct(function) => {
                let args: Vec<BasicMetadataValueEnum<'ink>> = values
                    .iter()
                    .map(|&v| BasicMetadataValueEnum::from(v))
                    .collect();
                let never = self.infer[arg]
                    .as_function()
                    .is_some_and(|(_, ret)| ret.is_never());
                let result = self.gen_call(None, *function, &args);
                if never {
                    return None;
                }
                Some(result.unwrap_or_else(|| self.gen_empty()))
            }
            Callee::Value(value, fn_ty) => {
                let never = fn_ty.as_function().is_some_and(|(_, ret)| ret.is_never());
                self.gen_fn_value_call(*value, fn_ty, values, never)
            }
        }
    }
}
