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
    pub(super) fn string_call(
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
    pub(super) fn alloc_args(&self, ty: &Ty) -> [BasicMetadataValueEnum<'ink>; 2] {
        [
            self.string_type_handle(ty).into(),
            self.get_allocator_handle_ptr().into(),
        ]
    }

    /// A slot in the entry block for a runtime function to write a `String`,
    /// a `usize`, an `i64` or an `f64` to.
    pub(super) fn string_slot(
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
    pub(super) fn string_making_call(
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
    pub(super) fn string_fields(
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
    pub(super) fn make_string(
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
    pub(super) fn string_data(
        &self,
        buf: PointerValue<'ink>,
        start: IntValue<'ink>,
    ) -> PointerValue<'ink> {
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
    pub(super) fn string_text(
        &self,
        expr: ExprId,
        value: BasicValueEnum<'ink>,
    ) -> (PointerValue<'ink>, IntValue<'ink>) {
        self.string_text_of(&self.infer[expr].clone(), value)
    }

    /// The data pointer and byte length of `value`, a `String` or a `str`
    /// as `ty` says.
    pub(super) fn string_text_of(
        &self,
        ty: &Ty,
        value: BasicValueEnum<'ink>,
    ) -> (PointerValue<'ink>, IntValue<'ink>) {
        let value = value.into_struct_value();
        if matches!(ty.interned(), TyKind::String) {
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
    pub(super) fn location_args(&mut self, expr: ExprId) -> [BasicMetadataValueEnum<'ink>; 4] {
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
    pub(super) fn is_set(&self, byte: BasicValueEnum<'ink>) -> IntValue<'ink> {
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
        if op == StringOp::DebugString {
            let ty = self.infer[receiver].clone();
            return Some(self.gen_debug_string(&ty, receiver_value, false));
        }
        if op == StringOp::TypeName {
            // A constant: the receiver's type is concrete here.
            let name = codira_hir::HirDisplay::display(&self.infer[receiver], self.db).to_string();
            return Some(self.gen_string_literal(&name));
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
            StringOp::FindJsonEscape => {
                let from = self.clamped_offset(args[0], arg_values[0].into_int_value(), len);
                self.gen_find_json_escape(data, from, len).into()
            }
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
            StringOp::ToString
            | StringOp::FromUtf8
            | StringOp::TakeUtf8
            | StringOp::DebugString
            | StringOp::TypeName => {
                unreachable!("lowered separately")
            }
        })
    }

    /// The integer type of `bits` bits (at least 1).
    fn int_type_of_width(&self, bits: u32) -> inkwell::types::IntType<'ink> {
        let bits = std::num::NonZeroU32::new(bits).expect("a width is at least 1 bit");
        self.context
            .custom_width_int_type(bits)
            .expect("LLVM supports integer types of this width")
    }

    /// The integer `value` (the value of `expr`) as a `usize` offset
    /// clamped to `0..=len`: a negative value is 0, one past the end `len`.
    fn clamped_offset(
        &self,
        expr: ExprId,
        value: IntValue<'ink>,
        len: IntValue<'ink>,
    ) -> IntValue<'ink> {
        let len_ty = len.get_type();
        let width = value.get_type().get_bit_width().max(len_ty.get_bit_width());
        let wide_ty = self.int_type_of_width(width);
        let signed = self.is_signed_int(expr);
        let wide = if signed {
            self.builder
                .build_int_s_extend_or_bit_cast(value, wide_ty, "from")
        } else {
            self.builder
                .build_int_z_extend_or_bit_cast(value, wide_ty, "from")
        }
        .expect("failed to widen an offset");
        let wide_len = self
            .builder
            .build_int_z_extend_or_bit_cast(len, wide_ty, "len")
            .expect("failed to widen a length");
        let zero = wide_ty.const_zero();
        let negative = if signed {
            self.builder
                .build_int_compare(IntPredicate::SLT, wide, zero, "negative")
                .expect("failed to compare an offset")
        } else {
            self.context.bool_type().const_zero()
        };
        let past = self
            .builder
            .build_int_compare(IntPredicate::UGT, wide, wide_len, "past")
            .expect("failed to compare an offset");
        let high = self
            .builder
            .build_select(past, wide_len, wide, "from.high")
            .expect("failed to clamp an offset")
            .into_int_value();
        let clamped = self
            .builder
            .build_select(negative, zero, high, "from.clamped")
            .expect("failed to clamp an offset")
            .into_int_value();
        self.builder
            .build_int_truncate_or_bit_cast(clamped, len_ty, "from")
            .expect("failed to narrow an offset")
    }

    /// The first offset in `from..len` of the bytes at `data` that is `"`,
    /// `\\` or below 0x20, or `len`: 16 bytes at a time with one unaligned
    /// vector load and three compares (LLVM lowers them to SSE2 or NEON,
    /// or to scalar code where there is no vector unit), then the last
    /// bytes one by one. Every load is below `len`.
    fn gen_find_json_escape(
        &mut self,
        data: PointerValue<'ink>,
        from: IntValue<'ink>,
        len: IntValue<'ink>,
    ) -> IntValue<'ink> {
        const LANES: u32 = 16;
        let usize_ty = len.get_type();
        let i8_ty = self.context.i8_type();
        let vec_ty = i8_ty.vec_type(LANES);
        let mask_ty = self.int_type_of_width(LANES);
        let splat = |byte: u64| {
            let lanes: Vec<_> = (0..LANES).map(|_| i8_ty.const_int(byte, false)).collect();
            inkwell::types::VectorType::const_vector(&lanes)
        };
        let entry = self.builder.get_insert_block().expect("inside a block");
        let vec_cond = self.context.append_basic_block(self.fn_value, "jesc.vec");
        let vec_body = self
            .context
            .append_basic_block(self.fn_value, "jesc.vec.body");
        let vec_found = self
            .context
            .append_basic_block(self.fn_value, "jesc.vec.found");
        let scalar_cond = self.context.append_basic_block(self.fn_value, "jesc.byte");
        let scalar_body = self
            .context
            .append_basic_block(self.fn_value, "jesc.byte.body");
        let done = self.context.append_basic_block(self.fn_value, "jesc.done");
        let b = &self.builder;
        b.build_unconditional_branch(vec_cond).expect("branch");

        // while i + 16 <= len
        b.position_at_end(vec_cond);
        let i = b.build_phi(usize_ty, "i").expect("phi");
        let lanes = usize_ty.const_int(u64::from(LANES), false);
        let i_val = i.as_basic_value().into_int_value();
        let next = b.build_int_nuw_add(i_val, lanes, "i.next").expect("add");
        // `i <= len` always, so `len - i` does not wrap.
        let left = b.build_int_sub(len, i_val, "left").expect("sub");
        let room = b
            .build_int_compare(IntPredicate::UGE, left, lanes, "room")
            .expect("compare");
        b.build_conditional_branch(room, vec_body, scalar_cond)
            .expect("branch");

        b.position_at_end(vec_body);
        // Safety: `i + 16 <= len`, so the 16 bytes are the string's.
        let at = unsafe { b.build_gep(i8_ty, data, &[i_val], "at").expect("gep") };
        let chunk = b.build_load(vec_ty, at, "chunk").expect("load");
        inkwell::values::BasicValue::as_instruction_value(&chunk)
            .expect("a load is an instruction")
            .set_alignment(1)
            .expect("alignment 1 is valid");
        let chunk = chunk.into_vector_value();
        let control = b
            .build_int_compare(IntPredicate::ULT, chunk, splat(0x20), "control")
            .expect("compare");
        let quote = b
            .build_int_compare(IntPredicate::EQ, chunk, splat(0x22), "quote")
            .expect("compare");
        let backslash = b
            .build_int_compare(IntPredicate::EQ, chunk, splat(0x5c), "backslash")
            .expect("compare");
        let any = b.build_or(control, quote, "any").expect("or");
        let any = b.build_or(any, backslash, "any").expect("or");
        let bits = b
            .build_bit_cast(any, mask_ty, "bits")
            .expect("bitcast")
            .into_int_value();
        let hit = b
            .build_int_compare(IntPredicate::NE, bits, mask_ty.const_zero(), "hit")
            .expect("compare");
        b.build_conditional_branch(hit, vec_found, vec_cond)
            .expect("branch");
        i.add_incoming(&[(&from, entry), (&next, vec_body)]);

        b.position_at_end(vec_found);
        let cttz = inkwell::intrinsics::Intrinsic::find("llvm.cttz")
            .and_then(|f| f.get_declaration(self.module, &[mask_ty.into()]))
            .expect("llvm.cttz is always available");
        let first = b
            .build_call(
                cttz,
                &[
                    bits.into(),
                    self.context.bool_type().const_int(1, false).into(),
                ],
                "first",
            )
            .expect("call")
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let first = b
            .build_int_z_extend_or_bit_cast(first, usize_ty, "first")
            .expect("zext");
        let found_vec = b.build_int_nuw_add(i_val, first, "found").expect("add");
        b.build_unconditional_branch(done).expect("branch");

        // The last bytes, one by one.
        b.position_at_end(scalar_cond);
        let j = b.build_phi(usize_ty, "j").expect("phi");
        let j_val = j.as_basic_value().into_int_value();
        let more = b
            .build_int_compare(IntPredicate::ULT, j_val, len, "more")
            .expect("compare");
        b.build_conditional_branch(more, scalar_body, done)
            .expect("branch");

        b.position_at_end(scalar_body);
        // Safety: `j < len`.
        let at = unsafe { b.build_gep(i8_ty, data, &[j_val], "at").expect("gep") };
        let byte = b
            .build_load(i8_ty, at, "byte")
            .expect("load")
            .into_int_value();
        let control = b
            .build_int_compare(
                IntPredicate::ULT,
                byte,
                i8_ty.const_int(0x20, false),
                "control",
            )
            .expect("compare");
        let quote = b
            .build_int_compare(
                IntPredicate::EQ,
                byte,
                i8_ty.const_int(0x22, false),
                "quote",
            )
            .expect("compare");
        let backslash = b
            .build_int_compare(
                IntPredicate::EQ,
                byte,
                i8_ty.const_int(0x5c, false),
                "backslash",
            )
            .expect("compare");
        let any = b.build_or(control, quote, "any").expect("or");
        let any = b.build_or(any, backslash, "any").expect("or");
        let j_next = b
            .build_int_nuw_add(j_val, usize_ty.const_int(1, false), "j.next")
            .expect("add");
        b.build_conditional_branch(any, done, scalar_cond)
            .expect("branch");
        j.add_incoming(&[(&i_val, vec_cond), (&j_next, scalar_body)]);

        b.position_at_end(done);
        let result = b.build_phi(usize_ty, "jesc").expect("phi");
        result.add_incoming(&[
            (&found_vec, vec_found),
            (&len, scalar_cond),
            (&j_val, scalar_body),
        ]);
        result.as_basic_value().into_int_value()
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
        self.gen_to_string_of(&self.infer[receiver].clone(), value)
    }

    /// `x.to_string()` for an `x` of type `ty`: a `String`, a `str`, an
    /// integer, a float or a `bool`.
    pub(super) fn gen_to_string_of(
        &mut self,
        ty: &Ty,
        value: BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        match ty.interned() {
            // Immutable, so the value itself is its own copy.
            TyKind::String => value,
            TyKind::Str => {
                let (data, len) = self.string_text_of(ty, value);
                self.string_making_call(&intrinsics::string_from_bytes, &[data.into(), len.into()])
            }
            // A plan of one piece: the digits written into the function's
            // plan buffer (see `fabric`), usually with no allocation.
            TyKind::Int(_) | TyKind::Float(_) => self.gen_number_string(ty, value),
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

    /// `string_from_utf8(bytes)` and `string_take_utf8(bytes)`.
    pub(super) fn gen_string_from_utf8(
        &mut self,
        expr: ExprId,
        op: StringOp,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let array = self.gen_expr(*args.first()?)?;
        let string_ty = self.hir_types.get_string_type();
        let slot = self.string_slot(string_ty.into(), "string.out");
        let [ty, alloc] = self.alloc_args(&strings::byte_array());
        let call_args = [array.into(), ty, alloc, slot.into()];
        let ok = if op == StringOp::TakeUtf8 {
            self.string_call(&intrinsics::string_take_utf8, &call_args)
        } else {
            self.string_call(&intrinsics::string_from_utf8, &call_args)
        }
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

    /// `+`, `+=` and the comparisons on `String`s and `str`s. Appending is
    /// String Fabric's (`fabric`): `+=` appends the pieces of its right side
    /// to the place's value, `+` is compiled as a format plan.
    pub(super) fn gen_string_binary_op(
        &mut self,
        expr: ExprId,
        lhs: ExprId,
        rhs: ExprId,
        op: BinaryOp,
    ) -> Option<BasicValueEnum<'ink>> {
        match op {
            BinaryOp::Assignment {
                op: Some(ArithOp::Add),
            } => self.gen_append_assign(lhs, rhs),
            BinaryOp::ArithOp(ArithOp::Add) => self.gen_string_plan(expr),
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
}
