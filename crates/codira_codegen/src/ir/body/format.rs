//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Lowering of the holes with a format spec (`f"{x:>8.3}"`) and of the
//!   debug form of every type (`f"{x:?}"`, `f"{x:#?}"`, `x.debug_string()`).
//!
//! # Format specs
//!
//! A hole with a spec is a piece of a format plan (see `fabric`): appended
//! to the string being built by a runtime function chosen by the value's
//! type -- `codira.string_append_format_int`, `_float` or `_text` -- with
//! the spec passed as four scalars (a fill character, the flags word of
//! `FormatSpec::abi_flags`, a width and a precision, either a constant or
//! the variable the spec names), or inline when there is nothing to pad.
//! Inference has already parsed the spec and checked that it applies, so
//! nothing is decided at run time but the digits and the padding.
//!
//! # The debug form
//!
//! The debug form of a type is a function generated for it, in the module
//! that needs it, the first time it is needed: `codira.debug:<type>` (and
//! `codira.debug_pretty:<type>` for `#?`), private to the module. It takes
//! a slot holding the `String` being built, the value and an indentation
//! depth, and appends the value's text: a number's `to_string()`, a string
//! quoted, `nil`, and for an aggregate its parts, each by a call to the
//! function of its own type. Everything is decided statically from the
//! type; nothing at run time inspects a type. A type that contains itself
//! (through an array, a class or an optional) has a function that calls
//! itself, which is why the function is declared before its body is
//! generated.
//!
//! The text reads as the Codira that builds the value: `Point { x: 1, y:
//! 2.0 }`, `Pair(1, "a")`, `[1, 2]`, `(1, true)`, `Shape.Circle(2.0)`,
//! `Shape.Dot`, `nil`, `"tab\there"`. Generic types are written by their
//! name alone (`Box { value: 3 }`), as a literal writes them. A function
//! value is `<func>`, a channel `<channel>` and a session endpoint
//! `<session>`. The pretty form puts every field and element on a line of
//! its own, indented four spaces a level, with a trailing comma, like
//! Rust's `{:#?}`.
//!
//! A class instance is a reference, and references can form a cycle. Each
//! one is recorded by the runtime (`codira.debug_enter`) while its fields
//! are being written; meeting it again further down writes `Name { .. }`
//! instead of looping. An instance reached twice without a cycle (shared,
//! not nested) is written both times.

use codira_hir::{
    mono, ExprId, HirDatabase, StructKind, StructMemoryKind, Ty, TyKind, VariantKind,
};
use inkwell::{
    module::Linkage,
    values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue},
    AddressSpace, IntPredicate,
};

use super::BodyIrGenerator;
use crate::{
    intrinsics,
    ir::{strings, RuntimeArrayValue, RuntimeReferenceValue},
};

/// How a type's debug form is laid out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// On one line: `P { x: 1, y: 2 }`.
    Compact,
    /// A line per field or element: Rust's `{:#?}`.
    Pretty,
}

/// The name of the generated debug function for `ty`: distinct types
/// (two structs of one name in different modules included) get distinct
/// names, see `mono::type_name`.
fn debug_symbol(db: &dyn HirDatabase, ty: &Ty, layout: Layout) -> String {
    let kind = match layout {
        Layout::Compact => "debug",
        Layout::Pretty => "debug_pretty",
    };
    format!("codira.{kind}:{}", mono::type_name(db, ty))
}

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// Generates `{value:spec}` (an `Expr::Format`) on its own: a plan of
    /// one piece (see `fabric`).
    pub(super) fn gen_format(&mut self, expr: ExprId) -> Option<BasicValueEnum<'ink>> {
        self.gen_string_plan(expr)
    }

    /// A width or precision as the `i64` the runtime takes: the variable's
    /// value, the literal, or 0 when there is none (its flag says so).
    pub(super) fn gen_count(
        &mut self,
        variable: Option<ExprId>,
        count: &codira_hir::FormatCount,
    ) -> Option<IntValue<'ink>> {
        let i64_type = self.context.i64_type();
        Some(match (variable, count) {
            (Some(expr), _) => {
                let value = self.gen_expr(expr)?.into_int_value();
                let bits = value.get_type().get_bit_width();
                if bits > 64 {
                    self.builder.build_int_truncate(value, i64_type, "count")
                } else if self.is_signed_int(expr) {
                    self.builder
                        .build_int_s_extend_or_bit_cast(value, i64_type, "count")
                } else {
                    self.builder
                        .build_int_z_extend_or_bit_cast(value, i64_type, "count")
                }
                .expect("failed to widen a count")
            }
            (None, codira_hir::FormatCount::Literal(n)) => i64_type.const_int(*n, false),
            (None, _) => i64_type.const_zero(),
        })
    }

    /// Whether `ty` is a signed integer type.
    pub(super) fn is_signed_ty(&self, ty: &Ty) -> bool {
        matches!(
            ty.interned(),
            TyKind::Int(int_ty) if int_ty.signedness == codira_hir::Signedness::Signed
        )
    }

    /// An integer as two 64-bit halves of its value extended to 128 bits.
    pub(super) fn int_halves(
        &self,
        value: IntValue<'ink>,
        signed: bool,
    ) -> (IntValue<'ink>, IntValue<'ink>) {
        let i128_type = self.context.i128_type();
        let i64_type = self.context.i64_type();
        let wide = if value.get_type().get_bit_width() == 128 {
            value
        } else if signed {
            self.builder
                .build_int_s_extend(value, i128_type, "wide")
                .expect("failed to widen an integer")
        } else {
            self.builder
                .build_int_z_extend(value, i128_type, "wide")
                .expect("failed to widen an integer")
        };
        let low = self
            .builder
            .build_int_truncate(wide, i64_type, "low")
            .expect("failed to truncate to the low half");
        let high = self
            .builder
            .build_right_shift(wide, i128_type.const_int(64, false), false, "high")
            .and_then(|high| self.builder.build_int_truncate(high, i64_type, "high"))
            .expect("failed to extract the high half");
        (low, high)
    }

    /// `"true"` or `"false"`, as a `str`.
    pub(super) fn gen_bool_text(&mut self, value: IntValue<'ink>) -> BasicValueEnum<'ink> {
        let yes = self.gen_string_literal("true");
        let no = self.gen_string_literal("false");
        self.builder
            .build_select(value, yes, no, "bool.text")
            .expect("failed to select a bool's text")
    }

    /// The debug form of `value`, of type `ty`, as a new `String`.
    pub(super) fn gen_debug_string(
        &mut self,
        ty: &Ty,
        value: BasicValueEnum<'ink>,
        pretty: bool,
    ) -> BasicValueEnum<'ink> {
        let string_ty = self.hir_types.get_string_type();
        let slot = self.string_slot(string_ty.into(), "debug.out");
        // A buffer of the size the text usually has, which it grows past.
        let start = self.gen_string_with_capacity(super::fabric::debug_estimate(self.db, ty, 0));
        self.builder
            .build_store(slot, start)
            .expect("failed to start a debug string");
        self.gen_debug_into(ty, value, pretty, slot);
        self.builder
            .build_load(string_ty, slot, "debug")
            .expect("failed to load a debug string")
    }

    /// Appends the debug form of `value` (of type `ty`) to the `String` in
    /// `slot`.
    pub(super) fn gen_debug_into(
        &mut self,
        ty: &Ty,
        value: BasicValueEnum<'ink>,
        pretty: bool,
        slot: PointerValue<'ink>,
    ) {
        let layout = if pretty {
            Layout::Pretty
        } else {
            Layout::Compact
        };
        self.gen_debug_call(
            ty,
            layout,
            slot,
            value,
            self.context.i64_type().const_zero(),
        );
    }

    /// Calls the debug function of `ty`, appending `value`'s text at `depth`
    /// to the `String` in `slot`.
    fn gen_debug_call(
        &mut self,
        ty: &Ty,
        layout: Layout,
        slot: PointerValue<'ink>,
        value: BasicValueEnum<'ink>,
        depth: IntValue<'ink>,
    ) {
        let function = self.debug_function(ty, layout);
        self.builder
            .build_call(function, &[slot.into(), value.into(), depth.into()], "")
            .expect("failed to call a debug function");
    }

    /// The debug function of `ty` in this module, generated on first use.
    fn debug_function(&mut self, ty: &Ty, layout: Layout) -> FunctionValue<'ink> {
        let name = debug_symbol(self.db, ty, layout);
        if let Some(function) = self.module.get_function(&name) {
            return function;
        }
        let value_type = self
            .hir_types
            .get_basic_type(ty)
            .expect("every value has a basic type");
        let fn_type = self.context.void_type().fn_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                value_type.into(),
                self.context.i64_type().into(),
            ],
            false,
        );
        // Declared first: a type that contains itself calls this function
        // from its own body.
        let function = self
            .module
            .add_function(&name, fn_type, Some(Linkage::Private));

        // Generated here and now, with the builder and the function being
        // generated set aside and restored after.
        let resume_at = self.builder.get_insert_block();
        let outer = std::mem::replace(&mut self.fn_value, function);
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);
        let param = |index| {
            function
                .get_nth_param(index)
                .expect("a debug function has three parameters")
        };
        let slot = param(0).into_pointer_value();
        let value = param(1);
        let depth = param(2).into_int_value();
        self.gen_debug_body(ty, layout, slot, value, depth);
        self.builder
            .build_return(None)
            .expect("failed to return from a debug function");
        self.fn_value = outer;
        if let Some(block) = resume_at {
            self.builder.position_at_end(block);
        }
        function
    }

    /// Appends the `str` constant `text` to the `String` in `slot`.
    fn debug_text(&mut self, slot: PointerValue<'ink>, text: &str) {
        let literal = self.gen_string_literal(text);
        self.debug_append(slot, &TyKind::Str.intern(), literal);
    }

    /// Appends `text` (a `String` or `str`, as `ty` says) to the `String` in
    /// `slot`.
    fn debug_append(&mut self, slot: PointerValue<'ink>, ty: &Ty, text: BasicValueEnum<'ink>) {
        let (data, len) = self.string_text_of(ty, text);
        let current = self.debug_so_far(slot);
        let appended = self.append_bytes(current, data, len);
        self.builder
            .build_store(slot, appended)
            .expect("failed to store a debug string");
    }

    /// The `String` in `slot`, being built.
    fn debug_so_far(&self, slot: PointerValue<'ink>) -> BasicValueEnum<'ink> {
        self.builder
            .build_load(self.hir_types.get_string_type(), slot, "debug.so_far")
            .expect("failed to load a debug string")
    }

    /// Calls a runtime function that takes the `String` in `slot` (as its
    /// three words) first, then `args`, then the `[u8]` allocation handles,
    /// and writes the resulting `String` back to `slot`.
    fn debug_slot_call(
        &mut self,
        function: &impl intrinsics::Intrinsic,
        slot: PointerValue<'ink>,
        args: &[BasicMetadataValueEnum<'ink>],
    ) {
        let string_ty = self.hir_types.get_string_type();
        let current = self
            .builder
            .build_load(string_ty, slot, "debug.so_far")
            .expect("failed to load a debug string")
            .into_struct_value();
        let (buf, start, len) = self.string_fields(current);
        let mut all: Vec<BasicMetadataValueEnum<'ink>> = vec![buf.into(), start.into(), len.into()];
        all.extend_from_slice(args);
        all.extend(self.alloc_args(&strings::byte_array()));
        all.push(slot.into());
        self.string_call(function, &all);
    }

    /// Starts a line at `depth` (the pretty form).
    fn debug_newline(&mut self, slot: PointerValue<'ink>, depth: IntValue<'ink>) {
        self.debug_slot_call(&intrinsics::debug_newline, slot, &[depth.into()]);
    }

    /// `depth + 1`.
    fn deeper(&self, depth: IntValue<'ink>) -> IntValue<'ink> {
        self.builder
            .build_int_add(depth, depth.get_type().const_int(1, false), "deeper")
            .expect("failed to add to a depth")
    }

    /// Appends the parts of an aggregate: `open`, then each part, separated
    /// by `, ` (or on lines of their own, each followed by `,`, in the
    /// pretty form), then `close`. A part is a label (`x: `, or none) and a
    /// value of a type. `braces` puts a space inside `{ .. }` on one line,
    /// and `lone_comma` writes the `,` of a one-element tuple, `(1,)`.
    #[allow(clippy::too_many_arguments)]
    fn debug_parts(
        &mut self,
        layout: Layout,
        slot: PointerValue<'ink>,
        depth: IntValue<'ink>,
        open: &str,
        parts: Vec<(Option<String>, Ty, BasicValueEnum<'ink>)>,
        close: &str,
        (braces, lone_comma): (bool, bool),
    ) {
        let compact = layout == Layout::Compact;
        let pad = if braces && compact { " " } else { "" };
        self.debug_text(slot, &format!("{open}{pad}"));
        let inner = self.deeper(depth);
        let count = parts.len();
        for (index, (label, ty, value)) in parts.into_iter().enumerate() {
            match layout {
                Layout::Compact if index > 0 => self.debug_text(slot, ", "),
                Layout::Compact => {}
                Layout::Pretty => self.debug_newline(slot, inner),
            }
            if let Some(label) = label {
                self.debug_text(slot, &format!("{label}: "));
            }
            self.gen_debug_call(&ty, layout, slot, value, inner);
            if !compact || (lone_comma && count == 1) {
                self.debug_text(slot, ",");
            }
        }
        if !compact && count > 0 {
            self.debug_newline(slot, depth);
        }
        self.debug_text(slot, &format!("{pad}{close}"));
    }

    /// Generates the body of the debug function of `ty`.
    fn gen_debug_body(
        &mut self,
        ty: &Ty,
        layout: Layout,
        slot: PointerValue<'ink>,
        value: BasicValueEnum<'ink>,
        depth: IntValue<'ink>,
    ) {
        match ty.interned() {
            TyKind::Int(_) | TyKind::Float(_) => {
                // The digits go straight into the string being built.
                let current = self.debug_so_far(slot);
                let appended = self.append_number(current, ty, value);
                self.builder
                    .build_store(slot, appended)
                    .expect("failed to store a debug string");
            }
            TyKind::Bool => {
                let text = self.gen_bool_text(value.into_int_value());
                self.debug_append(slot, &TyKind::Str.intern(), text);
            }
            TyKind::String | TyKind::Str => {
                let (data, len) = self.string_text_of(ty, value);
                self.debug_slot_call(&intrinsics::debug_quote, slot, &[data.into(), len.into()]);
            }
            TyKind::Tuple(_, elements) => {
                let tuple = value.into_struct_value();
                let parts = elements
                    .iter()
                    .enumerate()
                    .map(|(index, element)| {
                        let part = self
                            .builder
                            .build_extract_value(tuple, index as u32, "tuple.part")
                            .expect("a tuple has its elements");
                        (None, element.clone(), part)
                    })
                    .collect();
                self.debug_parts(layout, slot, depth, "(", parts, ")", (false, true));
            }
            TyKind::Optional(payload) => {
                self.gen_debug_optional(payload, layout, slot, value, depth)
            }
            TyKind::Array(element) => self.gen_debug_array(element, layout, slot, value, depth),
            TyKind::Struct(strukt, _) => {
                let data = strukt.data(self.db);
                let struct_type = self.hir_types.get_struct_type(ty);
                let is_class = data.memory_kind == StructMemoryKind::Gc;
                let name = strukt.name(self.db).to_string();
                let done = if is_class {
                    // A class instance already being written further up
                    // is a cycle: a marker instead.
                    let handle = value.into_pointer_value();
                    let fresh = self
                        .string_call(&intrinsics::debug_enter, &[handle.into()])
                        .expect("returns a flag");
                    let fresh = self.is_set(fresh);
                    let fields_block = self.context.append_basic_block(self.fn_value, "fields");
                    let cycle_block = self.context.append_basic_block(self.fn_value, "cycle");
                    let done = self.context.append_basic_block(self.fn_value, "done");
                    self.builder
                        .build_conditional_branch(fresh, fields_block, cycle_block)
                        .expect("failed to branch on a cycle check");
                    self.builder.position_at_end(cycle_block);
                    self.debug_text(slot, &format!("{name} {{ .. }}"));
                    self.builder
                        .build_unconditional_branch(done)
                        .expect("failed to branch");
                    self.builder.position_at_end(fields_block);
                    Some(done)
                } else {
                    None
                };
                let fields = if is_class {
                    // Safety: a value of a class type is a reference to an
                    // object of the class's struct type.
                    let reference = unsafe {
                        RuntimeReferenceValue::from_ptr_unchecked(
                            value.into_pointer_value(),
                            struct_type,
                        )
                    };
                    let data_ptr = reference.get_data_ptr(&self.builder);
                    self.builder
                        .build_load(struct_type, data_ptr, "object")
                        .expect("failed to load an object")
                        .into_struct_value()
                } else {
                    value.into_struct_value()
                };
                let field_types = mono::struct_field_types(self.db, ty);
                let parts: Vec<(Option<String>, Ty, BasicValueEnum<'ink>)> = strukt
                    .fields(self.db)
                    .into_iter()
                    .zip(field_types)
                    .enumerate()
                    .map(|(index, (field, field_ty))| {
                        let part = self
                            .builder
                            .build_extract_value(fields, index as u32, "field")
                            .expect("a struct has its fields");
                        let label = (data.kind == StructKind::Record)
                            .then(|| field.name(self.db).to_string());
                        (label, field_ty, part)
                    })
                    .collect();
                match data.kind {
                    StructKind::Unit => self.debug_text(slot, &name),
                    // `Rust`'s form: a record without fields is its name.
                    StructKind::Record if parts.is_empty() => self.debug_text(slot, &name),
                    StructKind::Record => {
                        let open = format!("{name} {{");
                        self.debug_parts(layout, slot, depth, &open, parts, "}", (true, false));
                    }
                    StructKind::Tuple => {
                        let open = format!("{name}(");
                        self.debug_parts(layout, slot, depth, &open, parts, ")", (false, false));
                    }
                }
                if let Some(done) = done {
                    self.string_call(&intrinsics::debug_leave, &[]);
                    self.builder
                        .build_unconditional_branch(done)
                        .expect("failed to branch");
                    self.builder.position_at_end(done);
                }
            }
            TyKind::Enum(hir_enum, args) => {
                let value = value.into_struct_value();
                let tag = self
                    .builder
                    .build_extract_value(value, 0, "tag")
                    .expect("an enum has a tag")
                    .into_int_value();
                let done = self.context.append_basic_block(self.fn_value, "done");
                let unknown = self.context.append_basic_block(self.fn_value, "unknown");
                let variants = hir_enum.variants(self.db);
                let blocks: Vec<_> = variants
                    .iter()
                    .map(|_| self.context.append_basic_block(self.fn_value, "variant"))
                    .collect();
                let cases: Vec<_> = variants
                    .iter()
                    .zip(&blocks)
                    .map(|(variant, block)| {
                        (
                            tag.get_type().const_int(u64::from(variant.index()), false),
                            *block,
                        )
                    })
                    .collect();
                self.builder
                    .build_switch(tag, unknown, &cases)
                    .expect("failed to switch on an enum tag");
                let enum_name = hir_enum.name(self.db).to_string();
                for (variant, block) in variants.iter().zip(blocks) {
                    self.builder.position_at_end(block);
                    let name = format!("{enum_name}.{}", variant.name(self.db));
                    let payload_types = variant.field_types_at(self.db, args);
                    if variant.kind(self.db) == VariantKind::Unit || payload_types.is_empty() {
                        self.debug_text(slot, &name);
                    } else {
                        let slots = variant.slots_at(self.db, args);
                        let parts = payload_types
                            .into_iter()
                            .zip(slots)
                            .map(|(payload_ty, slot_index)| {
                                let part = self
                                    .builder
                                    .build_extract_value(value, slot_index as u32 + 1, "payload")
                                    .expect("the payload slot exists in the layout");
                                (None, payload_ty, part)
                            })
                            .collect();
                        let open = format!("{name}(");
                        self.debug_parts(layout, slot, depth, &open, parts, ")", (false, false));
                    }
                    self.builder
                        .build_unconditional_branch(done)
                        .expect("failed to branch");
                }
                // A tag no variant has: only a hot reload that removed a
                // variant can make one.
                self.builder.position_at_end(unknown);
                self.debug_text(slot, &format!("{enum_name}.<unknown variant>"));
                self.builder
                    .build_unconditional_branch(done)
                    .expect("failed to branch");
                self.builder.position_at_end(done);
            }
            TyKind::Function(_) => self.debug_text(slot, "<func>"),
            TyKind::Channel(_) => self.debug_text(slot, "<channel>"),
            TyKind::Session(..) => self.debug_text(slot, "<session>"),
            _ => self.debug_text(slot, "<?>"),
        }
    }

    /// `nil`, or the payload's debug form.
    fn gen_debug_optional(
        &mut self,
        payload: &Ty,
        layout: Layout,
        slot: PointerValue<'ink>,
        value: BasicValueEnum<'ink>,
        depth: IntValue<'ink>,
    ) {
        let value = value.into_struct_value();
        let present = self
            .builder
            .build_extract_value(value, 0, "is_some")
            .expect("an optional has a present flag")
            .into_int_value();
        let some = self.context.append_basic_block(self.fn_value, "some");
        let none = self.context.append_basic_block(self.fn_value, "none");
        let done = self.context.append_basic_block(self.fn_value, "done");
        self.builder
            .build_conditional_branch(present, some, none)
            .expect("failed to branch on an optional");
        self.builder.position_at_end(some);
        let inner = self
            .builder
            .build_extract_value(value, 1, "payload")
            .expect("an optional has a payload");
        self.gen_debug_call(payload, layout, slot, inner, depth);
        self.builder
            .build_unconditional_branch(done)
            .expect("failed to branch");
        self.builder.position_at_end(none);
        self.debug_text(slot, "nil");
        self.builder
            .build_unconditional_branch(done)
            .expect("failed to branch");
        self.builder.position_at_end(done);
    }

    /// `[a, b, ..]`: a loop over the elements, which reads the array's
    /// length and elements afresh each time round (appending allocates, and
    /// the array is read through its handle).
    fn gen_debug_array(
        &mut self,
        element: &Ty,
        layout: Layout,
        slot: PointerValue<'ink>,
        value: BasicValueEnum<'ink>,
        depth: IntValue<'ink>,
    ) {
        let array_type = self.hir_types.get_array_type(element);
        // Safety: a value of an array type is a reference to an array.
        let array = unsafe {
            RuntimeArrayValue::from_ptr_unchecked(value.into_pointer_value(), array_type)
        };
        let length_type = array.length_ty();
        let index_slot = self
            .new_alloca_builder()
            .build_alloca(length_type, "debug.index")
            .expect("failed to allocate a loop index");
        self.builder
            .build_store(index_slot, length_type.const_zero())
            .expect("failed to start a loop index");
        self.debug_text(slot, "[");
        let inner = self.deeper(depth);

        let head = self.context.append_basic_block(self.fn_value, "elements");
        let body = self.context.append_basic_block(self.fn_value, "element");
        let tail = self
            .context
            .append_basic_block(self.fn_value, "elements.end");
        self.builder
            .build_unconditional_branch(head)
            .expect("failed to branch");

        self.builder.position_at_end(head);
        let index = self
            .builder
            .build_load(length_type, index_slot, "index")
            .expect("failed to load a loop index")
            .into_int_value();
        let length = self
            .builder
            .build_load(length_type, array.get_length_ptr(&self.builder), "len")
            .expect("failed to load an array's length")
            .into_int_value();
        let more = self
            .builder
            .build_int_compare(IntPredicate::ULT, index, length, "more")
            .expect("failed to compare an index");
        self.builder
            .build_conditional_branch(more, body, tail)
            .expect("failed to branch");

        self.builder.position_at_end(body);
        match layout {
            Layout::Compact => {
                let separate = self.context.append_basic_block(self.fn_value, "separate");
                let item = self.context.append_basic_block(self.fn_value, "item");
                let first = self
                    .builder
                    .build_int_compare(IntPredicate::EQ, index, length_type.const_zero(), "first")
                    .expect("failed to compare an index");
                self.builder
                    .build_conditional_branch(first, item, separate)
                    .expect("failed to branch");
                self.builder.position_at_end(separate);
                self.debug_text(slot, ", ");
                self.builder
                    .build_unconditional_branch(item)
                    .expect("failed to branch");
                self.builder.position_at_end(item);
            }
            Layout::Pretty => self.debug_newline(slot, inner),
        }
        let elements = array.get_elements(&self.builder);
        // Safety: `index < len`, checked above.
        let element_ptr = unsafe {
            self.builder
                .build_gep(array.element_ty(), elements, &[index], "element_ptr")
                .expect("failed to address an element")
        };
        let element_value = self
            .builder
            .build_load(array.element_ty(), element_ptr, "element")
            .expect("failed to load an element");
        self.gen_debug_call(element, layout, slot, element_value, inner);
        if layout == Layout::Pretty {
            self.debug_text(slot, ",");
        }
        let next = self
            .builder
            .build_int_add(index, length_type.const_int(1, false), "next")
            .expect("failed to add to an index");
        self.builder
            .build_store(index_slot, next)
            .expect("failed to store a loop index");
        self.builder
            .build_unconditional_branch(head)
            .expect("failed to branch");

        self.builder.position_at_end(tail);
        if layout == Layout::Pretty {
            // A line for the bracket only after elements: `[]` stays `[]`.
            let index = self
                .builder
                .build_load(length_type, index_slot, "count")
                .expect("failed to load a loop index")
                .into_int_value();
            let any = self
                .builder
                .build_int_compare(IntPredicate::NE, index, length_type.const_zero(), "any")
                .expect("failed to compare an index");
            let close_line = self.context.append_basic_block(self.fn_value, "close.line");
            let close = self.context.append_basic_block(self.fn_value, "close");
            self.builder
                .build_conditional_branch(any, close_line, close)
                .expect("failed to branch");
            self.builder.position_at_end(close_line);
            self.debug_newline(slot, depth);
            self.builder
                .build_unconditional_branch(close)
                .expect("failed to branch");
            self.builder.position_at_end(close);
        }
        self.debug_text(slot, "]");
    }
}
