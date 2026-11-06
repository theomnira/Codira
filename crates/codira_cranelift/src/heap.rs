//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The garbage-collected heap in JIT-compiled code: constructing class
//!   instances and reading and writing their fields; array literals,
//!   indexing with the LLVM backend's bounds check and panic, `len()`,
//!   `push(..)` and `unchecked_get(..)`; assignment to elements and fields;
//!   and `for` loops over ranges and arrays.
//!
//! Every operation follows its LLVM counterpart in `codira_codegen`'s
//! `ir::body` (and `ir::body::arrays`) step for step, in the same order, so
//! the two backends evaluate, check and panic alike:
//!
//! * a class is allocated with `new` after its field values are evaluated,
//!   and its handle is what a variable holds; a field is at the offset
//!   `codira_hir::layout::object_layout` gives, in the payload whose address
//!   is the handle's first word;
//! * an array literal allocates with `new_array` (which sets the length)
//!   and then evaluates and stores the elements in order;
//! * `a[i]` evaluates `a`, then `i`, then compares `i` (extended to `usize`
//!   by its signedness) with the length, unsigned, and on failure calls
//!   `codira.panic_index_out_of_bounds` with the index, its signedness, the
//!   length and the expression's location;
//! * `xs.push(x)` evaluates `xs`, then `x`, grows the buffer through
//!   `codira.array_grow` when the new length exceeds the capacity, and then
//!   stores the element and the new length;
//! * an assignment to a number or `bool` evaluates the place's operands,
//!   checks an element's index, reads the old value (compound assignment),
//!   evaluates the right operand, and stores, recomputing the address and
//!   checking the index again; any other assignment evaluates the right
//!   operand first and then the place;
//! * a `for` loop over an array reads the length once and stops at the
//!   smaller of it and the current length, so a body that shortens the
//!   array never reads past its end.
//!
//! The payload address is loaded from the handle again after every call
//! that may allocate or grow an array; only the handle is kept across one
//! (see the crate documentation on safepoints).

use codira_abi::runtime_functions as rt;
use codira_hir::{
    layout::{array_elements, object_layout},
    mono, ArithOp, ArrayMethod, BuiltinFn, Expr, ExprId, Name, Pat, PatId, Path, RecordLitField,
    StructMemoryKind, Ty, TyKind, ValueNs,
};
use cranelift_codegen::ir::{condcodes::IntCC, types, InstBuilder, MemFlagsData, TrapCode, Value};
use cranelift_frontend::Variable;

use crate::{
    translate::{LoopTarget, Lowered, Translator, Vals},
    ScalarTy, Unsupported,
};

/// The place an assignment writes, with its operands evaluated.
enum Place {
    /// A local variable.
    Local(Vec<Variable>),
    /// An element of an array: the expression `a[i]`, the expression `i`,
    /// the array's handle and the index.
    Element {
        expr: ExprId,
        index_expr: ExprId,
        array: Value,
        index: Value,
        element: Ty,
    },
    /// A field of a class instance: the instance's handle and the field's
    /// offset in the payload.
    Field { handle: Value, offset: u64 },
}

impl Translator<'_, '_> {
    /// Whether `ty` is a class (a garbage-collected struct).
    fn is_class(&self, ty: &Ty) -> bool {
        ty.as_struct()
            .is_some_and(|s| s.data(self.state.db).memory_kind == StructMemoryKind::Gc)
    }

    /// A constant pointer-sized value.
    fn pointer_const(&mut self, value: usize) -> Value {
        let pointer = self.state.pointer;
        self.builder
            .ins()
            .iconst(pointer, i64::try_from(value).unwrap_or(i64::MAX))
    }

    /// The type handle of `ty`, as a constant.
    pub(crate) fn type_handle(&mut self, ty: &Ty) -> Result<Value, Unsupported> {
        let handle = self
            .state
            .heap
            .handle(self.state.db, ty)
            .or_else(|what| self.unsupported(what))?;
        Ok(self.pointer_const(handle as usize))
    }

    /// The allocator handle, as a constant.
    pub(crate) fn allocator(&mut self) -> Value {
        let allocator = self.state.heap.allocator();
        self.pointer_const(allocator as usize)
    }

    /// The address of an object's payload: the first word of its header.
    fn payload(&mut self, handle: Value) -> Value {
        let pointer = self.state.pointer;
        self.builder
            .ins()
            .load(pointer, MemFlagsData::trusted(), handle, 0)
    }

    /// The fields of the class `ty`: their types and offsets in the
    /// payload.
    fn class_fields(&self, ty: &Ty) -> Result<(Vec<Ty>, Vec<u64>), Unsupported> {
        let Some((_, offsets)) = object_layout(self.state.db, ty) else {
            return self.unsupported(format!(
                "a class with a field the backend cannot lay out (`{}`)",
                self.state.display(ty)
            ));
        };
        Ok((mono::struct_field_types(self.state.db, ty), offsets))
    }

    /// Allocates an instance of the class `ty` and stores `fields` (in
    /// declaration order) in it.
    fn construct(&mut self, ty: &Ty, fields: Vec<Vals>) -> Lowered {
        if !self.is_class(ty) {
            return self.unsupported(format!("a value struct (`{}`)", self.state.display(ty)));
        }
        let (field_types, offsets) = self.class_fields(ty)?;
        let type_handle = self.type_handle(ty)?;
        let allocator = self.allocator();
        let Some(handle) = self.call_runtime(&rt::NEW, &[type_handle, allocator])? else {
            return self.unsupported("`new` without a result");
        };
        self.gc_value(handle);
        let payload = self.payload(handle);
        for ((field_ty, offset), values) in field_types.iter().zip(offsets).zip(fields) {
            self.store_value(field_ty, &values, payload, offset)?;
        }
        Ok(vec![handle])
    }

    /// `Foo { b: .., a: .. }`: the values evaluated in the order written,
    /// stored in the fields they name.
    pub(crate) fn record_literal(&mut self, id: ExprId, fields: &[RecordLitField]) -> Lowered {
        let ty = self.infer[id].clone();
        let Some(strukt) = ty.as_struct() else {
            return self.unsupported("a record literal of a non-struct type");
        };
        if !self.is_class(&ty) {
            return self.unsupported(format!("a value struct (`{}`)", self.state.display(&ty)));
        }
        let field_types = mono::struct_field_types(self.state.db, &ty);
        let mut values: Vec<Option<Vals>> = vec![None; field_types.len()];
        for field in fields {
            let Some(index) = strukt
                .field(self.state.db, &field.name)
                .map(|f| f.index(self.state.db) as usize)
            else {
                return self.unsupported("a record literal naming a missing field");
            };
            let value = self.value(field.expr, &field_types[index])?;
            values[index] = Some(value);
        }
        let Some(values) = values.into_iter().collect::<Option<Vec<_>>>() else {
            return self.unsupported("a record literal that leaves a field out");
        };
        self.construct(&ty, values)
    }

    /// `Foo(a, b)`: a class constructed from its fields in order.
    pub(crate) fn tuple_struct(&mut self, id: ExprId, args: &[ExprId]) -> Lowered {
        let ty = self.infer[id].clone();
        let field_types = mono::struct_field_types(self.state.db, &ty);
        let mut values = Vec::with_capacity(args.len());
        for (&arg, field_ty) in args.iter().zip(&field_types) {
            values.push(self.value(arg, field_ty)?);
        }
        self.construct(&ty, values)
    }

    /// A path that is not a local: a class without fields (`Foo`) is
    /// constructed.
    pub(crate) fn unit_struct(&mut self, id: ExprId, path: &Path) -> Lowered {
        let resolver = codira_hir::resolver_for_expr(self.state.db, self.body.owner(), id);
        match resolver.resolve_path_as_value_fully(self.state.db, path) {
            Some((ValueNs::StructId(_) | ValueNs::ImplSelf(_), _)) => {
                let ty = self.infer[id].clone();
                self.construct(&ty, Vec::new())
            }
            _ => self.unsupported("a path that is not a local variable"),
        }
    }

    /// The index and offset of the field `name` of the class `receiver_ty`.
    fn class_field(&self, receiver_ty: &Ty, name: &Name) -> Result<(Ty, u64), Unsupported> {
        if !self.is_class(receiver_ty) {
            return self.unsupported(format!(
                "a field of a `{}`",
                self.state.display(receiver_ty)
            ));
        }
        let Some(strukt) = receiver_ty.as_struct() else {
            return self.unsupported("a field of a non-struct");
        };
        let Some(field) = strukt.field(self.state.db, name) else {
            return self.unsupported("a field inference did not resolve");
        };
        let index = field.index(self.state.db) as usize;
        let (field_types, offsets) = self.class_fields(receiver_ty)?;
        Ok((field_types[index].clone(), offsets[index]))
    }

    /// `obj.field`.
    pub(crate) fn field(&mut self, receiver: ExprId, name: &Name) -> Lowered {
        let receiver_ty = self.infer[receiver].clone();
        let (field_ty, offset) = self.class_field(&receiver_ty, name)?;
        let handle = self.single(receiver)?;
        let payload = self.payload(handle);
        self.load_value(&field_ty, payload, offset)
    }

    /// The element type, the offset of the first element and the stride of
    /// the array type `array_ty`.
    fn array_shape(&self, array_ty: &Ty) -> Result<(Ty, u64, u64), Unsupported> {
        let Some(element) = array_ty.as_array() else {
            return self.unsupported("indexing something that is not an array");
        };
        self.state
            .check(element)
            .or_else(|what| self.unsupported(what))?;
        let Some((first, stride)) = array_elements(self.state.db, element) else {
            return self.unsupported("an array of values without a layout");
        };
        Ok((element.clone(), first, stride))
    }

    /// An array's length.
    fn array_len(&mut self, handle: Value) -> Value {
        let payload = self.payload(handle);
        self.builder
            .ins()
            .load(types::I64, MemFlagsData::trusted(), payload, 0)
    }

    /// The address of element `index` (a `usize`) of the array `handle`,
    /// unchecked.
    fn element_address(&mut self, handle: Value, index: Value, first: u64, stride: u64) -> Value {
        let payload = self.payload(handle);
        let scaled = self
            .builder
            .ins()
            .imul_imm_u(index, i64::try_from(stride).unwrap_or(i64::MAX));
        let address = self.builder.ins().iadd(payload, scaled);
        self.builder
            .ins()
            .iadd_imm_s(address, i64::try_from(first).unwrap_or(i64::MAX))
    }

    /// The index value of `index_expr` extended to `usize` by its
    /// signedness, as the LLVM backend's `index_to_length_type` does.
    fn index_to_usize(&mut self, index_expr: ExprId, index: Value) -> Result<Value, Unsupported> {
        match self.scalar_of(&self.infer[index_expr].clone())? {
            Some(ScalarTy::Int { bits: 64, .. }) => Ok(index),
            Some(ScalarTy::Int { signed: true, .. }) => {
                Ok(self.builder.ins().sextend(types::I64, index))
            }
            Some(ScalarTy::Int { signed: false, .. }) => {
                Ok(self.builder.ins().uextend(types::I64, index))
            }
            _ => self.unsupported("an index that is not an integer"),
        }
    }

    /// Checks the index `index` (the value of `index_expr`) of the array
    /// `handle` and returns the element's address; out of range it panics
    /// at `expr` as the LLVM backend's `gen_place_index` does.
    fn checked_element(
        &mut self,
        expr: ExprId,
        index_expr: ExprId,
        handle: Value,
        index: Value,
        element: &Ty,
    ) -> Result<Value, Unsupported> {
        let array_ty = TyKind::Array(element.clone()).intern();
        let (_, first, stride) = self.array_shape(&array_ty)?;
        let signed = matches!(
            self.scalar_of(&self.infer[index_expr].clone())?,
            Some(ScalarTy::Int { signed: true, .. })
        );
        let len = self.array_len(handle);
        let wide = self.index_to_usize(index_expr, index)?;
        let in_bounds = self.builder.ins().icmp(IntCC::UnsignedLessThan, wide, len);
        let fail = self.builder.create_block();
        let ok = self.builder.create_block();
        self.builder.set_cold_block(fail);
        self.builder.ins().brif(in_bounds, ok, &[], fail, &[]);
        self.switch_to(fail);
        let (file, line, column) = self.location(expr);
        let (file_data, file_len) = self.string_value(&file)?;
        let signed_flag = self.builder.ins().iconst(types::I8, i64::from(signed));
        let line = self.builder.ins().iconst(types::I32, i64::from(line));
        let column = self.builder.ins().iconst(types::I32, i64::from(column));
        self.call_runtime(
            &rt::PANIC_INDEX_OUT_OF_BOUNDS,
            &[wide, signed_flag, len, file_data, file_len, line, column],
        )?;
        self.builder.ins().trap(TrapCode::unwrap_user(1));
        self.switch_to(ok);
        Ok(self.element_address(handle, wide, first, stride))
    }

    /// `a[i]`.
    pub(crate) fn index(&mut self, id: ExprId, base: ExprId, index: ExprId) -> Lowered {
        let base_ty = self.infer[base].clone();
        let (element, _, _) = self.array_shape(&base_ty)?;
        let handle = self.single(base)?;
        let index_value = self.operand(index)?;
        let address = self.checked_element(id, index, handle, index_value, &element)?;
        self.load_value(&element, address, 0)
    }

    /// `[a, b, c]`.
    pub(crate) fn array_literal(&mut self, id: ExprId, elements: &[ExprId]) -> Lowered {
        let array_ty = self.infer[id].clone();
        let (element, first, stride) = self.array_shape(&array_ty)?;
        let type_handle = self.type_handle(&array_ty)?;
        let length = self.pointer_const(elements.len());
        let allocator = self.allocator();
        let Some(handle) = self.call_runtime(&rt::NEW_ARRAY, &[type_handle, length, allocator])?
        else {
            return self.unsupported("`new_array` without a result");
        };
        self.gc_value(handle);
        for (index, &expr) in elements.iter().enumerate() {
            let value = self.value(expr, &element)?;
            let index = self.pointer_const(index);
            let address = self.element_address(handle, index, first, stride);
            self.store_value(&element, &value, address, 0)?;
        }
        Ok(vec![handle])
    }

    /// A builtin method: `xs.len()`, `xs.is_empty()`, `xs.push(x)`,
    /// `xs.unchecked_get(i)`.
    pub(crate) fn builtin_method(
        &mut self,
        _id: ExprId,
        builtin: BuiltinFn,
        receiver: ExprId,
        args: &[ExprId],
    ) -> Lowered {
        let receiver_ty = self.infer[receiver].clone();
        match builtin {
            BuiltinFn::ArrayLen => {
                let handle = self.single(receiver)?;
                Ok(vec![self.array_len(handle)])
            }
            BuiltinFn::ArrayUncheckedGet => {
                let (element, first, stride) = self.array_shape(&receiver_ty)?;
                let handle = self.single(receiver)?;
                let index = self.operand(args[0])?;
                let index = self.index_to_usize(args[0], index)?;
                let address = self.element_address(handle, index, first, stride);
                self.load_value(&element, address, 0)
            }
            BuiltinFn::Array(ArrayMethod::IsEmpty) => {
                let handle = self.single(receiver)?;
                let len = self.array_len(handle);
                Ok(vec![self.builder.ins().icmp_imm_u(IntCC::Equal, len, 0)])
            }
            BuiltinFn::Array(ArrayMethod::Push) => {
                let (element, first, stride) = self.array_shape(&receiver_ty)?;
                let handle = self.single(receiver)?;
                let value = self.value(args[0], &element)?;
                let len = self.array_len(handle);
                let new_len = self.builder.ins().iadd_imm_s(len, 1);
                let payload = self.payload(handle);
                let capacity = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::trusted(),
                    payload,
                    i32::try_from(std::mem::size_of::<usize>()).unwrap_or(8),
                );
                let full = self
                    .builder
                    .ins()
                    .icmp(IntCC::UnsignedGreaterThan, new_len, capacity);
                let grow = self.builder.create_block();
                let ready = self.builder.create_block();
                self.builder.set_cold_block(grow);
                self.builder.ins().brif(full, grow, &[], ready, &[]);
                self.switch_to(grow);
                let allocator = self.allocator();
                self.call_runtime(&rt::ARRAY_GROW, &[handle, new_len, allocator])?;
                self.jump(ready, &[]);
                self.switch_to(ready);
                // The elements may have moved: the payload is read again.
                let address = self.element_address(handle, len, first, stride);
                self.store_value(&element, &value, address, 0)?;
                let payload = self.payload(handle);
                self.builder
                    .ins()
                    .store(MemFlagsData::trusted(), new_len, payload, 0);
                Ok(Vec::new())
            }
            BuiltinFn::Array(method) => {
                self.unsupported(format!("the array method `{}`", method.name()))
            }
            BuiltinFn::Pipeline(_) => {
                self.unsupported("a lazy pipeline (`iter()`, `map`, `sum`, ...)")
            }
            BuiltinFn::String(op) => self.string_method(op, receiver, args),
            BuiltinFn::StrLen => match self.operand_vals(receiver)?.as_slice() {
                [_, len] => Ok(vec![*len]),
                _ => self.unsupported("a `str` of an unexpected shape"),
            },
            other => self.unsupported(format!("the builtin `{}`", other.name())),
        }
    }

    /// Evaluates the operands of the place `expr`.
    fn place(&mut self, expr: ExprId) -> Result<Place, Unsupported> {
        if let Some(vars) = self.local(expr) {
            return Ok(Place::Local(vars));
        }
        let body = self.body.clone();
        match &body[expr] {
            Expr::Index { base, index } => {
                let base_ty = self.infer[*base].clone();
                let (element, _, _) = self.array_shape(&base_ty)?;
                let array = self.single(*base)?;
                let index_value = self.operand(*index)?;
                Ok(Place::Element {
                    expr,
                    index_expr: *index,
                    array,
                    index: index_value,
                    element,
                })
            }
            Expr::Field {
                expr: receiver,
                name,
            } => {
                let receiver_ty = self.infer[*receiver].clone();
                let (_, offset) = self.class_field(&receiver_ty, name)?;
                let handle = self.single(*receiver)?;
                Ok(Place::Field { handle, offset })
            }
            _ => self.unsupported("assignment to this kind of place"),
        }
    }

    /// The address of a place with evaluated operands; an element's index
    /// is checked against the array's current length.
    fn place_address(&mut self, place: &Place) -> Result<Value, Unsupported> {
        match place {
            Place::Local(_) => self.unsupported("the address of a local"),
            Place::Element {
                expr,
                index_expr,
                array,
                index,
                element,
            } => {
                let element = element.clone();
                self.checked_element(*expr, *index_expr, *array, *index, &element)
            }
            Place::Field { handle, offset } => {
                let payload = self.payload(*handle);
                Ok(self
                    .builder
                    .ins()
                    .iadd_imm_s(payload, i64::try_from(*offset).unwrap_or(i64::MAX)))
            }
        }
    }

    /// `place = rhs` and `place op= rhs` on a number or `bool`, in the order
    /// the LLVM backend's `gen_scalar_assignment` uses (see the module
    /// documentation).
    pub(crate) fn scalar_assignment(
        &mut self,
        id: ExprId,
        lhs: ExprId,
        rhs: ExprId,
        op: Option<ArithOp>,
    ) -> Lowered {
        let lhs_ty = self.infer[lhs].clone();
        let Some(ty) = self.scalar_of(&lhs_ty)? else {
            return self.unsupported("assignment of unit");
        };
        let place = self.place(lhs)?;
        let old = match (&place, op) {
            (_, None) => {
                if !matches!(place, Place::Local(_)) {
                    // The index check comes before the right operand.
                    self.place_address(&place)?;
                }
                None
            }
            (Place::Local(vars), Some(_)) => Some(self.builder.use_var(vars[0])),
            (_, Some(_)) => {
                let address = self.place_address(&place)?;
                Some(
                    self.builder
                        .ins()
                        .load(ty.clif(), MemFlagsData::trusted(), address, 0),
                )
            }
        };
        let right = self.operand(rhs)?;
        let value = match (op, old) {
            (Some(arith), Some(old)) => {
                self.division_check(id, arith, ty, old, right)?;
                self.arith(arith, ty, old, right)?
            }
            _ => right,
        };
        match &place {
            Place::Local(vars) => self.builder.def_var(vars[0], value),
            _ => {
                let address = self.place_address(&place)?;
                self.builder
                    .ins()
                    .store(MemFlagsData::trusted(), value, address, 0);
            }
        }
        Ok(Vec::new())
    }

    /// `place = rhs` on anything but a number or `bool`: the right operand,
    /// then the place.
    pub(crate) fn plain_assignment(&mut self, lhs: ExprId, rhs: ExprId) -> Lowered {
        let ty = self.infer[lhs].clone();
        let value = self.value(rhs, &ty)?;
        let place = self.place(lhs)?;
        match &place {
            Place::Local(vars) => {
                for (var, value) in vars.iter().zip(&value) {
                    self.builder.def_var(*var, *value);
                }
            }
            _ => {
                let address = self.place_address(&place)?;
                self.store_value(&ty, &value, address, 0)?;
            }
        }
        Ok(Vec::new())
    }

    /// `place op= ..` on a value that is not a number or `bool` (`s += t`
    /// on a `String`), the right operand already evaluated: the place's
    /// operands are evaluated once, its value read, `update` makes the new
    /// value, and it is stored where it was read.
    pub(crate) fn update_place(
        &mut self,
        lhs: ExprId,
        update: impl FnOnce(&mut Self, Vals) -> Lowered,
    ) -> Lowered {
        let ty = self.infer[lhs].clone();
        let place = self.place(lhs)?;
        match &place {
            Place::Local(vars) => {
                let current: Vals = vars.iter().map(|v| self.builder.use_var(*v)).collect();
                let new = update(self, current)?;
                for (var, value) in vars.iter().zip(&new) {
                    self.builder.def_var(*var, *value);
                }
            }
            _ => {
                let address = self.place_address(&place)?;
                let current = self.load_value(&ty, address, 0)?;
                let new = update(self, current)?;
                // The payload address again, rather than one held across
                // the call.
                let address = self.place_address(&place)?;
                self.store_value(&ty, &new, address, 0)?;
            }
        }
        Ok(Vec::new())
    }

    /// Binds a `for` loop's pattern to an element.
    fn bind_loop_pattern(
        &mut self,
        pat: PatId,
        ty: &Ty,
        values: &[Value],
    ) -> Result<(), Unsupported> {
        match self.body[pat] {
            Pat::Bind { .. } => {
                let types = self.types(ty)?;
                self.bind(pat, &types, values);
                Ok(())
            }
            Pat::Wild => Ok(()),
            _ => self.unsupported("a `for` loop with a pattern"),
        }
    }

    /// `for x in a..b`, `for x in a..=b` and `for x in xs`, as the LLVM
    /// backend's `gen_counted_loop` runs them: the bounds (or the array and
    /// its length) evaluated once, a counter the body cannot change, an
    /// inclusive range that ends at its last value without overflowing, and
    /// an array loop that also stops at the array's current length.
    pub(crate) fn for_loop(
        &mut self,
        id: ExprId,
        pat: PatId,
        iterable: ExprId,
        loop_body: ExprId,
    ) -> Lowered {
        let body = self.body.clone();
        if self.infer.iterator_next(id).is_some()
            || matches!(self.infer[iterable].interned(), TyKind::Iter(_))
        {
            return self.unsupported("a `for` loop over an iterator");
        }
        let cond = self.builder.create_block();
        let inside = self.builder.create_block();
        let step = self.builder.create_block();
        let exit = self.builder.create_block();

        // What the step needs: an inclusive range's last value.
        let mut last = None;
        let counter = self.builder.declare_var(types::I64);
        let (counter, element_ty, array) = if let Expr::Range {
            start,
            end,
            inclusive,
        } = body[iterable]
        {
            let ty = self.infer[start].clone();
            let Some(ScalarTy::Int { bits, signed }) = self.scalar_of(&ty)? else {
                return self.unsupported("a range that is not of integers");
            };
            let start_value = self.operand(start)?;
            let end_value = self.operand(end)?;
            let counter = self
                .builder
                .declare_var(ScalarTy::Int { bits, signed }.clif());
            self.builder.def_var(counter, start_value);
            self.jump(cond, &[]);
            self.switch_to(cond);
            let index = self.builder.use_var(counter);
            let cc = match (inclusive, signed) {
                (false, true) => IntCC::SignedLessThan,
                (false, false) => IntCC::UnsignedLessThan,
                (true, true) => IntCC::SignedLessThanOrEqual,
                (true, false) => IntCC::UnsignedLessThanOrEqual,
            };
            let in_range = self.builder.ins().icmp(cc, index, end_value);
            self.builder.ins().brif(in_range, inside, &[], exit, &[]);
            if inclusive {
                last = Some(end_value);
            }
            (counter, ty, None)
        } else if self.infer.builtin_call(iterable)
            == Some(BuiltinFn::Array(ArrayMethod::Enumerate))
        {
            return self.unsupported("`enumerate()`");
        } else {
            let array_ty = self.infer[iterable].clone();
            let (element, first, stride) = self.array_shape(&array_ty)?;
            let handle = self.single(iterable)?;
            let length = self.array_len(handle);
            let zero = self.builder.ins().iconst(types::I64, 0);
            self.builder.def_var(counter, zero);
            self.jump(cond, &[]);
            self.switch_to(cond);
            let index = self.builder.use_var(counter);
            let below_start = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedLessThan, index, length);
            let current = self.array_len(handle);
            let below_current = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedLessThan, index, current);
            let in_range = self.builder.ins().band(below_start, below_current);
            self.builder.ins().brif(in_range, inside, &[], exit, &[]);
            (counter, element, Some((handle, first, stride)))
        };

        self.switch_to(inside);
        let index = self.builder.use_var(counter);
        let element = match array {
            Some((handle, first, stride)) => {
                let address = self.element_address(handle, index, first, stride);
                self.load_value(&element_ty, address, 0)?
            }
            None => vec![index],
        };
        self.bind_loop_pattern(pat, &element_ty, &element)?;
        self.loops.push(LoopTarget {
            exit,
            value: Vec::new(),
            continue_to: step,
        });
        self.expr(loop_body)?;
        self.loops.pop();
        self.jump(step, &[]);

        self.switch_to(step);
        let index = self.builder.use_var(counter);
        if let Some(last) = last {
            let is_last = self.builder.ins().icmp(IntCC::Equal, index, last);
            let increment = self.builder.create_block();
            self.builder.ins().brif(is_last, exit, &[], increment, &[]);
            self.switch_to(increment);
        }
        let next = self.builder.ins().iadd_imm_s(index, 1);
        self.builder.def_var(counter, next);
        self.jump(cond, &[]);
        self.switch_to(exit);
        Ok(Vec::new())
    }
}
