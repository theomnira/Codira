//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - Lowering of `String`: its builtin methods, `to_string()`,
//!   `string_from_utf8`, the operators and printing.
//!
//! A `String` value is `{ ptr buf, usize start, usize len }` (see
//! `codira_runtime`'s `string` module). What only reads the value -- `len`,
//! `is_empty`, `byte_at`, the views `slice` and `trim` once checked or
//! measured -- is inline IR; everything else is one call to a
//! `codira.string_*` runtime function through the dispatch table. A text
//! argument (a `String` or a `str`) is passed to the runtime as its data
//! pointer and length, so a literal argument costs nothing.
//!
//! A runtime function that makes a `String` writes it to a slot in the
//! caller's frame (`out`), which keeps the new buffer's handle on the stack,
//! where the conservative collector sees it.

use codira_hir::{ArithOp, BinaryOp, CmpOp, ExprId, Ordering as CmpOrdering, StringOp, Ty, TyKind};
use inkwell::{
    values::{BasicMetadataValueEnum, BasicValueEnum, IntValue, PointerValue, StructValue},
    IntPredicate,
};

use super::BodyIrGenerator;
use crate::{
    intrinsics::{self, Intrinsic},
    ir::{strings, RuntimeArrayValue},
};

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// Calls a runtime string function and returns its result, if it has one.
    fn string_call(
        &mut self,
        function: &impl Intrinsic,
        args: &[BasicMetadataValueEnum<'ink>],
    ) -> Option<BasicValueEnum<'ink>> {
        let (fn_ty, fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            function,
        );
        self.builder
            .build_indirect_call(fn_ty, fn_ptr, args, "")
            .expect("failed to build a call to a runtime string function")
            .try_as_basic_value()
            .basic()
    }

    /// The type handle of `ty`, from the module's type table.
    fn string_type_handle(&self, ty: &Ty) -> PointerValue<'ink> {
        self.type_table.gen_type_info_lookup(
            self.context,
            &self.builder,
            &self.hir_types.type_id(ty),
            self.external_globals.type_table,
        )
    }

    /// The two handles every allocating string function takes: the type of
    /// what it allocates and the allocator.
    fn alloc_args(&self, ty: &Ty) -> [BasicMetadataValueEnum<'ink>; 2] {
        [
            self.string_type_handle(ty).into(),
            self.get_allocator_handle_ptr().into(),
        ]
    }

    /// A slot in the entry block for a runtime function to write a `String`,
    /// a `usize`, an `i64` or an `f64` to.
    fn string_slot(
        &self,
        ty: inkwell::types::BasicTypeEnum<'ink>,
        name: &str,
    ) -> PointerValue<'ink> {
        self.new_alloca_builder()
            .build_alloca(ty, name)
            .expect("failed to allocate a slot for a string result")
    }

    /// Calls `function` with `args` followed by the `[u8]` allocation
    /// handles and an out-slot, and returns the `String` it wrote.
    fn string_making_call(
        &mut self,
        function: &impl Intrinsic,
        args: &[BasicMetadataValueEnum<'ink>],
    ) -> BasicValueEnum<'ink> {
        let string_ty = self.hir_types.get_string_type();
        let slot = self.string_slot(string_ty.into(), "string.out");
        let mut all = args.to_vec();
        all.extend(self.alloc_args(&strings::byte_array()));
        all.push(slot.into());
        self.string_call(function, &all);
        self.builder
            .build_load(string_ty, slot, "string")
            .expect("failed to load a string result")
    }

    /// The three words of a `String` value.
    fn string_fields(
        &self,
        value: StructValue<'ink>,
    ) -> (PointerValue<'ink>, IntValue<'ink>, IntValue<'ink>) {
        let field = |index, name| {
            self.builder
                .build_extract_value(value, index, name)
                .expect("a String has three fields")
        };
        (
            field(0, "string.buf").into_pointer_value(),
            field(1, "string.start").into_int_value(),
            field(2, "string.len").into_int_value(),
        )
    }

    /// A `String` value from its three words.
    fn make_string(
        &self,
        buf: PointerValue<'ink>,
        start: IntValue<'ink>,
        len: IntValue<'ink>,
    ) -> BasicValueEnum<'ink> {
        let mut value = self.hir_types.get_string_type().get_undef();
        for (index, field) in [buf.into(), start.into(), len.into()]
            .into_iter()
            .enumerate()
        {
            let field: BasicValueEnum<'ink> = field;
            value = self
                .builder
                .build_insert_value(value, field, index as u32, "string")
                .expect("failed to build a String value")
                .into_struct_value();
        }
        value.into()
    }

    /// The address of the first byte of a `String`.
    fn string_data(&self, buf: PointerValue<'ink>, start: IntValue<'ink>) -> PointerValue<'ink> {
        let array_ty = self
            .hir_types
            .get_array_type(&TyKind::Int(codira_hir::IntTy::u8()).intern());
        // Safety: a `String`'s buffer is a `[u8]` array object.
        let array = unsafe { RuntimeArrayValue::from_ptr_unchecked(buf, array_ty) };
        let elements = array.get_elements(&self.builder);
        // Safety: `start` is within the buffer, by construction of every
        // `String` value.
        unsafe {
            self.builder
                .build_gep(self.context.i8_type(), elements, &[start], "string.data")
                .expect("failed to build the address of a string's bytes")
        }
    }

    /// The data pointer and byte length of `value`, the result of `expr`,
    /// which is a `String` or a `str`.
    fn string_text(
        &self,
        expr: ExprId,
        value: BasicValueEnum<'ink>,
    ) -> (PointerValue<'ink>, IntValue<'ink>) {
        let value = value.into_struct_value();
        if matches!(self.infer[expr].interned(), TyKind::String) {
            let (buf, start, len) = self.string_fields(value);
            (self.string_data(buf, start), len)
        } else {
            let data = self
                .builder
                .build_extract_value(value, 0, "str.data")
                .expect("a str has a data pointer")
                .into_pointer_value();
            let len = self
                .builder
                .build_extract_value(value, 1, "str.len")
                .expect("a str has a length")
                .into_int_value();
            (data, len)
        }
    }

    /// Boxes a `String` value on the GC heap, for the host and for a call
    /// through the dispatch table: one pointer to an object of the
    /// `String` value-struct type holding the three words.
    pub(super) fn box_string(&mut self, value: BasicValueEnum<'ink>) -> PointerValue<'ink> {
        let (new_fn_ty, new_fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            &intrinsics::new,
        );
        let type_info = self.string_type_handle(&TyKind::String.intern());
        let allocator = self.get_allocator_handle_ptr();
        let handle = self
            .builder
            .build_indirect_call(
                new_fn_ty,
                new_fn_ptr,
                &[type_info.into(), allocator.into()],
                "string.box",
            )
            .expect("failed to build call to `new`")
            .try_as_basic_value()
            .basic()
            .expect("`new` returns a handle")
            .into_pointer_value();
        let data = self
            .builder
            .build_load(
                self.context.ptr_type(inkwell::AddressSpace::default()),
                handle,
                "string.box.data",
            )
            .expect("failed to load a box's payload")
            .into_pointer_value();
        self.builder
            .build_store(data, value)
            .expect("failed to store a boxed string");
        handle
    }

    /// The `String` value in a box made by [`Self::box_string`].
    pub(super) fn unbox_string(&self, handle: BasicValueEnum<'ink>) -> BasicValueEnum<'ink> {
        let data = self
            .builder
            .build_load(
                self.context.ptr_type(inkwell::AddressSpace::default()),
                handle.into_pointer_value(),
                "string.box.data",
            )
            .expect("failed to load a box's payload")
            .into_pointer_value();
        self.builder
            .build_load(self.hir_types.get_string_type(), data, "string")
            .expect("failed to load a boxed string")
    }

    /// Prints a `String`.
    pub(super) fn gen_print_string(&mut self, arg: ExprId, value: BasicValueEnum<'ink>) {
        let (data, len) = self.string_text(arg, value);
        self.gen_runtime_call(&intrinsics::print_str, &[data.into(), len.into()]);
    }

    /// An integer argument as 64 bits and its signedness, the way the runtime
    /// takes an index or a count.
    fn index_arg(&self, expr: ExprId, value: IntValue<'ink>) -> (IntValue<'ink>, IntValue<'ink>) {
        let i64_type = self.context.i64_type();
        let signed = self.is_signed_int(expr);
        let wide = if value.get_type().get_bit_width() > 64 {
            self.builder.build_int_truncate(value, i64_type, "index")
        } else if signed {
            self.builder
                .build_int_s_extend_or_bit_cast(value, i64_type, "index")
        } else {
            self.builder
                .build_int_z_extend_or_bit_cast(value, i64_type, "index")
        }
        .expect("failed to widen an index");
        (
            wide,
            self.context.i8_type().const_int(u64::from(signed), false),
        )
    }

    /// The location arguments of a runtime function that may panic.
    fn location_args(&mut self, expr: ExprId) -> [BasicMetadataValueEnum<'ink>; 4] {
        let (file, line, column) = self.source_location(expr);
        let u32_type = self.context.i32_type();
        [
            self.builder
                .build_extract_value(file, 0, "file.data")
                .expect("a str has a data pointer")
                .into(),
            self.builder
                .build_extract_value(file, 1, "file.len")
                .expect("a str has a length")
                .into(),
            u32_type.const_int(u64::from(line), false).into(),
            u32_type.const_int(u64::from(column), false).into(),
        ]
    }

    /// An optional of `payload`, present when `present` is set and `nil`
    /// (all zero) otherwise.
    fn string_optional(
        &self,
        optional_ty: &Ty,
        present: IntValue<'ink>,
        payload: BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let optional_type = self
            .hir_types
            .get_basic_type(optional_ty)
            .expect("an optional is a basic type")
            .into_struct_type();
        let zero = optional_type
            .get_field_type_at_index(1)
            .expect("an optional has a payload")
            .const_zero();
        let payload = self
            .builder
            .build_select(present, payload, zero, "payload")
            .expect("failed to select an optional's payload");
        let with_flag = self
            .builder
            .build_insert_value(optional_type.const_zero(), present, 0, "flag")
            .expect("failed to set an optional's flag");
        self.builder
            .build_insert_value(with_flag, payload, 1, "optional")
            .expect("failed to set an optional's payload")
            .into_struct_value()
            .into()
    }

    /// Whether a runtime result byte is non-zero, as an `i1`.
    fn is_set(&self, byte: BasicValueEnum<'ink>) -> IntValue<'ink> {
        let byte = byte.into_int_value();
        self.builder
            .build_int_compare(IntPredicate::NE, byte, byte.get_type().const_zero(), "set")
            .expect("failed to test a runtime flag")
    }

    /// Generates a builtin method of `String` (or `to_string()` on a
    /// primitive): see [`StringOp`].
    pub(super) fn gen_string_method(
        &mut self,
        op: StringOp,
        expr: ExprId,
        receiver: ExprId,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let receiver_value = self.gen_expr(receiver)?;
        let mut arg_values = Vec::with_capacity(args.len());
        for &arg in args {
            arg_values.push(self.gen_expr(arg)?);
        }
        if op == StringOp::ToString {
            return Some(self.gen_to_string(receiver, receiver_value));
        }
        // A method called on a `str` works on a `String` copy of it.
        let receiver_value = if matches!(self.infer[receiver].interned(), TyKind::Str) {
            self.gen_to_string(receiver, receiver_value)
        } else {
            receiver_value
        };
        let usize_type = self.hir_types.get_usize_type();
        let (buf, start, len) = self.string_fields(receiver_value.into_struct_value());
        let data = self.string_data(buf, start);
        let text = |this: &Self, index: usize| this.string_text(args[index], arg_values[index]);
        Some(match op {
            StringOp::Len => len.into(),
            StringOp::IsEmpty => self
                .builder
                .build_int_compare(IntPredicate::EQ, len, usize_type.const_zero(), "is_empty")
                .expect("failed to compare a length")
                .into(),
            StringOp::CharCount => self
                .string_call(&intrinsics::string_char_count, &[data.into(), len.into()])
                .expect("returns a count"),
            StringOp::Hash => self
                .string_call(&intrinsics::string_hash, &[data.into(), len.into()])
                .expect("returns a hash"),
            StringOp::Chars => {
                let [ty, alloc] = self.alloc_args(&strings::code_point_array());
                self.string_call(
                    &intrinsics::string_chars,
                    &[data.into(), len.into(), ty, alloc],
                )
                .expect("returns an array")
            }
            StringOp::Bytes => {
                let [ty, alloc] = self.alloc_args(&strings::byte_array());
                self.string_call(
                    &intrinsics::string_to_bytes,
                    &[data.into(), len.into(), ty, alloc],
                )
                .expect("returns an array")
            }
            StringOp::ByteAt => self.gen_byte_at(expr, args[0], arg_values[0], data, len),
            StringOp::Find | StringOp::RFind => {
                let (needle, needle_len) = text(self, 0);
                let function_args = [data.into(), len.into(), needle.into(), needle_len.into()];
                let found = if op == StringOp::Find {
                    self.string_call(&intrinsics::string_find, &function_args)
                } else {
                    self.string_call(&intrinsics::string_rfind, &function_args)
                }
                .expect("returns an offset")
                .into_int_value();
                let present = self
                    .builder
                    .build_int_compare(
                        IntPredicate::SGE,
                        found,
                        found.get_type().const_zero(),
                        "found",
                    )
                    .expect("failed to test a search result");
                let offset = self
                    .builder
                    .build_int_truncate_or_bit_cast(found, usize_type, "offset")
                    .expect("failed to convert an offset");
                self.string_optional(&self.infer[expr].clone(), present, offset.into())
            }
            StringOp::Contains | StringOp::StartsWith | StringOp::EndsWith => {
                let (other, other_len) = text(self, 0);
                let function_args = [data.into(), len.into(), other.into(), other_len.into()];
                let byte = match op {
                    StringOp::Contains => {
                        self.string_call(&intrinsics::string_contains, &function_args)
                    }
                    StringOp::StartsWith => {
                        self.string_call(&intrinsics::string_starts_with, &function_args)
                    }
                    _ => self.string_call(&intrinsics::string_ends_with, &function_args),
                }
                .expect("returns a flag");
                self.is_set(byte).into()
            }
            StringOp::Split => {
                let (sep, sep_len) = text(self, 0);
                let [ty, alloc] = self.alloc_args(&strings::string_array());
                let [file, file_len, line, column] = self.location_args(expr);
                self.string_call(
                    &intrinsics::string_split,
                    &[
                        buf.into(),
                        start.into(),
                        len.into(),
                        sep.into(),
                        sep_len.into(),
                        ty,
                        alloc,
                        file,
                        file_len,
                        line,
                        column,
                    ],
                )
                .expect("returns an array")
            }
            StringOp::Lines => {
                let [ty, alloc] = self.alloc_args(&strings::string_array());
                self.string_call(
                    &intrinsics::string_lines,
                    &[buf.into(), start.into(), len.into(), ty, alloc],
                )
                .expect("returns an array")
            }
            StringOp::Trim | StringOp::TrimStart | StringOp::TrimEnd => {
                let mode = match op {
                    StringOp::TrimStart => 1,
                    StringOp::TrimEnd => 2,
                    _ => 0,
                };
                let slot = self.string_slot(usize_type.into(), "trim.offset");
                let new_len = self
                    .string_call(
                        &intrinsics::string_trim,
                        &[
                            data.into(),
                            len.into(),
                            self.context.i8_type().const_int(mode, false).into(),
                            slot.into(),
                        ],
                    )
                    .expect("returns a length")
                    .into_int_value();
                let offset = self
                    .builder
                    .build_load(usize_type, slot, "trim.offset")
                    .expect("failed to load a trim offset")
                    .into_int_value();
                let new_start = self
                    .builder
                    .build_int_add(start, offset, "trim.start")
                    .expect("failed to add an offset");
                self.make_string(buf, new_start, new_len)
            }
            StringOp::ToUpper | StringOp::ToLower => {
                let upper = u64::from(op == StringOp::ToUpper);
                self.string_making_call(
                    &intrinsics::string_change_case,
                    &[
                        data.into(),
                        len.into(),
                        self.context.i8_type().const_int(upper, false).into(),
                    ],
                )
            }
            StringOp::Replace => {
                let (from, from_len) = text(self, 0);
                let (to, to_len) = text(self, 1);
                self.string_making_call(
                    &intrinsics::string_replace,
                    &[
                        data.into(),
                        len.into(),
                        from.into(),
                        from_len.into(),
                        to.into(),
                        to_len.into(),
                    ],
                )
            }
            StringOp::Repeat => {
                let (count, signed) = self.index_arg(args[0], arg_values[0].into_int_value());
                self.string_making_call(
                    &intrinsics::string_repeat,
                    &[data.into(), len.into(), count.into(), signed.into()],
                )
            }
            StringOp::Slice => {
                let (from, from_signed) = self.index_arg(args[0], arg_values[0].into_int_value());
                let (to, to_signed) = self.index_arg(args[1], arg_values[1].into_int_value());
                let [file, file_len, line, column] = self.location_args(expr);
                self.string_call(
                    &intrinsics::string_check_slice,
                    &[
                        data.into(),
                        len.into(),
                        from.into(),
                        from_signed.into(),
                        to.into(),
                        to_signed.into(),
                        file,
                        file_len,
                        line,
                        column,
                    ],
                );
                // Checked: `from <= to <= len`, both non-negative.
                let from = self
                    .builder
                    .build_int_truncate_or_bit_cast(from, usize_type, "slice.from")
                    .expect("failed to convert an index");
                let to = self
                    .builder
                    .build_int_truncate_or_bit_cast(to, usize_type, "slice.to")
                    .expect("failed to convert an index");
                let new_start = self
                    .builder
                    .build_int_add(start, from, "slice.start")
                    .expect("failed to add an offset");
                let new_len = self
                    .builder
                    .build_int_sub(to, from, "slice.len")
                    .expect("failed to subtract indices");
                self.make_string(buf, new_start, new_len)
            }
            StringOp::ParseI64 | StringOp::ParseF64 => {
                let payload_ty = if op == StringOp::ParseI64 {
                    self.context.i64_type().into()
                } else {
                    self.context.f64_type().into()
                };
                let slot = self.string_slot(payload_ty, "parsed");
                let function_args = [data.into(), len.into(), slot.into()];
                let ok = if op == StringOp::ParseI64 {
                    self.string_call(&intrinsics::string_parse_i64, &function_args)
                } else {
                    self.string_call(&intrinsics::string_parse_f64, &function_args)
                }
                .expect("returns a flag");
                let present = self.is_set(ok);
                let value = self
                    .builder
                    .build_load(payload_ty, slot, "parsed")
                    .expect("failed to load a parsed number");
                self.string_optional(&self.infer[expr].clone(), present, value)
            }
            StringOp::ToString | StringOp::FromUtf8 => {
                unreachable!("lowered separately")
            }
        })
    }

    /// `s.byte_at(i)`: the byte, after checking `i` against the length the
    /// way array indexing does (a negative index is out of range and is
    /// reported as written).
    fn gen_byte_at(
        &mut self,
        expr: ExprId,
        index_expr: ExprId,
        index: BasicValueEnum<'ink>,
        data: PointerValue<'ink>,
        len: IntValue<'ink>,
    ) -> BasicValueEnum<'ink> {
        let (wide, signed) = self.index_arg(index_expr, index.into_int_value());
        let i64_type = self.context.i64_type();
        let wide_len = self
            .builder
            .build_int_z_extend_or_bit_cast(len, i64_type, "len")
            .expect("failed to widen a length");
        let in_bounds = self
            .builder
            .build_int_compare(IntPredicate::ULT, wide, wide_len, "in_bounds")
            .expect("failed to build a bounds check");
        let ok_block = self.context.append_basic_block(self.fn_value, "byte_at.ok");
        let fail_block = self
            .context
            .append_basic_block(self.fn_value, "byte_at.out_of_bounds");
        self.builder
            .build_conditional_branch(in_bounds, ok_block, fail_block)
            .expect("failed to branch on a bounds check");

        self.builder.position_at_end(fail_block);
        let [file, file_len, line, column] = self.location_args(expr);
        self.gen_runtime_call(
            &intrinsics::panic_index_out_of_bounds,
            &[
                wide.into(),
                signed.into(),
                wide_len.into(),
                file,
                file_len,
                line,
                column,
            ],
        );
        self.builder
            .build_unreachable()
            .expect("failed to build unreachable after a panic");

        self.builder.position_at_end(ok_block);
        let offset = self
            .builder
            .build_int_truncate_or_bit_cast(wide, len.get_type(), "index")
            .expect("failed to convert an index");
        // Safety: `index < len`, checked above.
        let address = unsafe {
            self.builder
                .build_gep(self.context.i8_type(), data, &[offset], "byte")
                .expect("failed to build the address of a byte")
        };
        self.builder
            .build_load(self.context.i8_type(), address, "byte")
            .expect("failed to load a byte")
    }

    /// `x.to_string()`, by the type of `x`.
    fn gen_to_string(
        &mut self,
        receiver: ExprId,
        value: BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let i64_type = self.context.i64_type();
        match self.infer[receiver].clone().interned() {
            // Immutable, so the value itself is its own copy.
            TyKind::String => value,
            TyKind::Str => {
                let (data, len) = self.string_text(receiver, value);
                self.string_making_call(&intrinsics::string_from_bytes, &[data.into(), len.into()])
            }
            TyKind::Int(int_ty) => {
                let value = value.into_int_value();
                let signed = int_ty.signedness == codira_hir::Signedness::Signed;
                if value.get_type().get_bit_width() > 64 {
                    let low = self
                        .builder
                        .build_int_truncate(value, i64_type, "low")
                        .expect("failed to truncate to the low half");
                    let shift = value.get_type().const_int(64, false);
                    let high = self
                        .builder
                        .build_right_shift(value, shift, false, "high_bits")
                        .and_then(|high| self.builder.build_int_truncate(high, i64_type, "high"))
                        .expect("failed to extract the high half");
                    let signed = self.context.i8_type().const_int(u64::from(signed), false);
                    self.string_making_call(
                        &intrinsics::string_from_i128,
                        &[low.into(), high.into(), signed.into()],
                    )
                } else if signed {
                    let wide = self
                        .builder
                        .build_int_s_extend_or_bit_cast(value, i64_type, "wide")
                        .expect("failed to widen an integer");
                    self.string_making_call(&intrinsics::string_from_i64, &[wide.into()])
                } else {
                    let wide = self
                        .builder
                        .build_int_z_extend_or_bit_cast(value, i64_type, "wide")
                        .expect("failed to widen an integer");
                    self.string_making_call(&intrinsics::string_from_u64, &[wide.into()])
                }
            }
            TyKind::Float(_) => {
                let value = value.into_float_value();
                if value.get_type() == self.context.f32_type() {
                    self.string_making_call(&intrinsics::string_from_f32, &[value.into()])
                } else {
                    self.string_making_call(&intrinsics::string_from_f64, &[value.into()])
                }
            }
            TyKind::Bool => {
                let byte = self
                    .builder
                    .build_int_z_extend(value.into_int_value(), self.context.i8_type(), "byte")
                    .expect("failed to widen a bool");
                self.string_making_call(&intrinsics::string_from_bool, &[byte.into()])
            }
            other => unreachable!("inference admits no `to_string` on {other:?}"),
        }
    }

    /// A `str` used where a `String` is expected becomes a `String` copy.
    pub(super) fn gen_string_conversion(
        &mut self,
        expr: ExprId,
        value: BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        self.gen_to_string(expr, value)
    }

    /// `string_from_utf8(bytes)`.
    pub(super) fn gen_string_from_utf8(
        &mut self,
        expr: ExprId,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let array = self.gen_expr(*args.first()?)?;
        let string_ty = self.hir_types.get_string_type();
        let slot = self.string_slot(string_ty.into(), "string.out");
        let [ty, alloc] = self.alloc_args(&strings::byte_array());
        let ok = self
            .string_call(
                &intrinsics::string_from_utf8,
                &[array.into(), ty, alloc, slot.into()],
            )
            .expect("returns a flag");
        let present = self.is_set(ok);
        // On failure the slot was never written: select keeps the payload
        // of a `nil` all zero rather than reading undefined memory.
        let value = self
            .builder
            .build_load(string_ty, slot, "string")
            .expect("failed to load a string result");
        let value = self
            .builder
            .build_select(present, value, string_ty.const_zero().into(), "string")
            .expect("failed to select a string");
        Some(self.string_optional(&self.infer[expr].clone(), present, value))
    }

    /// `+`, `+=` and the comparisons on `String`s and `str`s.
    pub(super) fn gen_string_binary_op(
        &mut self,
        lhs: ExprId,
        rhs: ExprId,
        op: BinaryOp,
    ) -> Option<BasicValueEnum<'ink>> {
        match op {
            BinaryOp::Assignment {
                op: Some(ArithOp::Add),
            } => {
                let rhs_value = self.gen_expr(rhs)?;
                let place = self.gen_place_expr(lhs)?;
                let string_ty = self.hir_types.get_string_type();
                let current = self
                    .builder
                    .build_load(string_ty, place, "string")
                    .expect("failed to load a string place");
                let appended = self.gen_append(current, rhs, rhs_value);
                self.builder
                    .build_store(place, appended)
                    .expect("failed to store a string");
                Some(self.gen_empty())
            }
            BinaryOp::ArithOp(ArithOp::Add) => {
                let lhs_value = self.gen_expr(lhs)?;
                let rhs_value = self.gen_expr(rhs)?;
                if matches!(self.infer[lhs].interned(), TyKind::String) {
                    Some(self.gen_append(lhs_value, rhs, rhs_value))
                } else {
                    let (a, a_len) = self.string_text(lhs, lhs_value);
                    let (b, b_len) = self.string_text(rhs, rhs_value);
                    Some(self.string_making_call(
                        &intrinsics::string_concat,
                        &[a.into(), a_len.into(), b.into(), b_len.into()],
                    ))
                }
            }
            BinaryOp::CmpOp(cmp) => {
                let lhs_value = self.gen_expr(lhs)?;
                let rhs_value = self.gen_expr(rhs)?;
                let (a, a_len) = self.string_text(lhs, lhs_value);
                let (b, b_len) = self.string_text(rhs, rhs_value);
                let args = [a.into(), a_len.into(), b.into(), b_len.into()];
                Some(match cmp {
                    CmpOp::Eq { negated } => {
                        let equal = self
                            .string_call(&intrinsics::string_eq, &args)
                            .expect("returns a flag")
                            .into_int_value();
                        let predicate = if negated {
                            IntPredicate::EQ
                        } else {
                            IntPredicate::NE
                        };
                        self.builder
                            .build_int_compare(
                                predicate,
                                equal,
                                equal.get_type().const_zero(),
                                "eq",
                            )
                            .expect("failed to compare strings")
                            .into()
                    }
                    CmpOp::Ord { ordering, strict } => {
                        let order = self
                            .string_call(&intrinsics::string_cmp, &args)
                            .expect("returns an ordering")
                            .into_int_value();
                        let predicate = match (ordering, strict) {
                            (CmpOrdering::Less, true) => IntPredicate::SLT,
                            (CmpOrdering::Less, false) => IntPredicate::SLE,
                            (CmpOrdering::Greater, true) => IntPredicate::SGT,
                            (CmpOrdering::Greater, false) => IntPredicate::SGE,
                        };
                        self.builder
                            .build_int_compare(
                                predicate,
                                order,
                                order.get_type().const_zero(),
                                "cmp",
                            )
                            .expect("failed to compare strings")
                            .into()
                    }
                })
            }
            _ => unreachable!("inference admits no other string operator"),
        }
    }

    /// `current + text`, for a `String` `current`: see the runtime's
    /// `string_append`.
    fn gen_append(
        &mut self,
        current: BasicValueEnum<'ink>,
        rhs: ExprId,
        rhs_value: BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let (buf, start, len) = self.string_fields(current.into_struct_value());
        let (data, data_len) = self.string_text(rhs, rhs_value);
        self.string_making_call(
            &intrinsics::string_append,
            &[
                buf.into(),
                start.into(),
                len.into(),
                data.into(),
                data_len.into(),
            ],
        )
    }
}
