//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Frame allocation (`--frame-alloc`): class instances and short array
//!   literals that `codira_hir::escape` proves never outlive their call are
//!   placed in the function's stack frame instead of the GC heap.
//!
//! # Layout
//!
//! A frame object looks exactly like a heap object to the code that uses
//! it: a reference is the address of a header whose first word points to
//! the payload. Both live in `alloca`s of the function's entry block, one
//! pair per allocation site, so a site inside a loop reuses its slot every
//! iteration; the analysis guarantees the previous iteration's object is
//! unreachable by then.
//!
//! ```text
//! header  (32 bytes, the collector's ObjectHeader shape):
//!   data: ptr   -> payload        (the only word generated code reads)
//!   the rest: zero                (only the runtime reads it, and the
//!                                  runtime never sees a frame object)
//! payload: the class's fields, or { len, capacity, [N x T] } for an array
//! ```
//!
//! # Why the collector stays correct
//!
//! The payload is in the frame, and the collector scans every stack
//! conservatively from the parked thread's stack pointer to its base, so
//! every GC reference a frame object holds is a root for as long as the
//! frame is live. If LLVM promotes the payload to registers, the values are
//! in registers or spill slots instead, which the scan covers the same way.
//! No heap object ever refers to a frame object (the analysis keeps
//! anything stored into a heap object on the heap), so the collector never
//! traces one. See `codira_hir::escape` for the full contract.

use codira_hir::{ExprId, Ty};
use inkwell::{
    types::{BasicType, BasicTypeEnum, StructType},
    values::{BasicValueEnum, PointerValue, StructValue},
    AddressSpace,
};

use super::BodyIrGenerator;
use crate::{
    intrinsics,
    ir::{RuntimeArrayValue, RuntimeReferenceValue},
};

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// Whether the allocation `expr` goes to the frame.
    pub(super) fn is_frame_allocation(&self, expr: ExprId) -> bool {
        self.frame_enabled
            && self
                .escapes
                .as_ref()
                .is_some_and(|escapes| escapes.is_frame(expr))
    }

    /// Whether the allocation `expr` goes to the function's region: it is
    /// placed there, or the function is `@region` and the object would be in
    /// the frame but frame allocation is off. (A heap object never refers to
    /// a region object, so in a `@region` function nothing that does not
    /// escape may be on the heap.)
    pub(super) fn is_region_allocation(&self, expr: ExprId) -> bool {
        if !self.region_function {
            return false;
        }
        match self
            .escapes
            .as_ref()
            .and_then(|escapes| escapes.placement(expr))
        {
            Some(codira_hir::escape::Placement::Region(_)) => true,
            Some(codira_hir::escape::Placement::Frame) => !self.frame_enabled,
            _ => false,
        }
    }

    /// The allocator handle, loaded with `builder`.
    fn allocator_handle_with(
        &self,
        builder: &inkwell::builder::Builder<'ink>,
    ) -> PointerValue<'ink> {
        let global = self
            .external_globals
            .alloc_handle
            .expect("a `@region` function has an allocator handle");
        let value_type = global.get_value_type().into_pointer_type();
        builder
            .build_load(value_type, global.as_pointer_value(), "allocator_handle")
            .expect("failed to load the allocator handle")
            .into_pointer_value()
    }

    /// Opens the region of a `@region` function, at its entry, into an
    /// entry-block slot that every allocation and every return reads.
    pub(super) fn gen_region_open(&mut self) {
        if !self.region_function {
            return;
        }
        let ptr = self.context.ptr_type(AddressSpace::default());
        let slot = self
            .new_alloca_builder()
            .build_alloca(ptr, "region.slot")
            .expect("failed to reserve the region slot");
        let alloc = self.allocator_handle_with(&self.builder);
        let (fn_ty, fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            &intrinsics::region_open,
        );
        let region = self
            .builder
            .build_indirect_call(fn_ty, fn_ptr, &[alloc.into()], "region")
            .expect("failed to open the region")
            .try_as_basic_value()
            .unwrap_basic();
        self.builder
            .build_store(slot, region)
            .expect("failed to store the region");
        self.region_slot = Some(slot);
    }

    /// Closes the region before every `ret` of the function.
    pub(super) fn finish_region(&mut self) {
        let Some(slot) = self.region_slot else {
            return;
        };
        let returns: Vec<_> = self
            .fn_value
            .get_basic_block_iter()
            .filter_map(inkwell::basic_block::BasicBlock::get_terminator)
            .filter(|terminator| {
                terminator.get_opcode() == inkwell::values::InstructionOpcode::Return
            })
            .collect();
        let ptr = self.context.ptr_type(AddressSpace::default());
        let builder = self.context.create_builder();
        for ret in returns {
            builder.position_before(&ret);
            let region = builder
                .build_load(ptr, slot, "region")
                .expect("failed to load the region");
            let alloc = self.allocator_handle_with(&builder);
            let (fn_ty, fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
                self.external_globals.dispatch_table,
                &builder,
                &intrinsics::region_close,
            );
            builder
                .build_indirect_call(fn_ty, fn_ptr, &[region.into(), alloc.into()], "")
                .expect("failed to close the region");
        }
    }

    /// Allocates an object with a payload of `size` bytes aligned to
    /// `align` in the function's region; returns the reference and the
    /// payload.
    fn gen_region_object(
        &mut self,
        size: inkwell::values::IntValue<'ink>,
        align: inkwell::values::IntValue<'ink>,
    ) -> (PointerValue<'ink>, PointerValue<'ink>) {
        let slot = self
            .region_slot
            .expect("a region allocation in a `@region` function");
        let ptr = self.context.ptr_type(AddressSpace::default());
        let region = self
            .builder
            .build_load(ptr, slot, "region")
            .expect("failed to load the region");
        let alloc = self.allocator_handle_with(&self.builder);
        let usize_ty = self.hir_types.get_usize_type();
        let size = self
            .builder
            .build_int_z_extend_or_bit_cast(size, usize_ty, "size")
            .expect("failed to widen a size");
        let align = self
            .builder
            .build_int_z_extend_or_bit_cast(align, usize_ty, "align")
            .expect("failed to widen an alignment");
        let (fn_ty, fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            &intrinsics::region_alloc,
        );
        let header = self
            .builder
            .build_indirect_call(
                fn_ty,
                fn_ptr,
                &[region.into(), size.into(), align.into(), alloc.into()],
                "region.ref",
            )
            .expect("failed to allocate in the region")
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        let payload = self
            .builder
            .build_load(ptr, header, "region.data")
            .expect("failed to load a region object's payload")
            .into_pointer_value();
        (header, payload)
    }

    /// A class instance in the function's region, holding `struct_lit`.
    pub(super) fn gen_struct_alloc_in_region(
        &mut self,
        hir_ty: &Ty,
        struct_lit: StructValue<'ink>,
    ) -> BasicValueEnum<'ink> {
        let struct_ir_ty = self.hir_types.get_struct_type(hir_ty);
        let size = struct_ir_ty.size_of().expect("a class has a size");
        let align = struct_ir_ty.get_alignment();
        let (header, payload) = self.gen_region_object(size, align);
        self.builder
            .build_store(payload, struct_lit)
            .expect("failed to store a region object's fields");
        RuntimeReferenceValue::from_ptr(header, struct_ir_ty)
            .expect("unable to construct a reference")
            .into()
    }

    /// An array with room for `length` elements in the function's region,
    /// with its capacity set and its length left for the caller.
    pub(super) fn alloc_array_in_region(
        &mut self,
        expr: ExprId,
        length: inkwell::values::IntValue<'ink>,
    ) -> RuntimeArrayValue<'ink> {
        let element_ty = self.infer[expr]
            .as_array()
            .expect("the type of an array expression is an array")
            .clone();
        let element_ir_ty = self
            .hir_types
            .get_basic_type(&element_ty)
            .expect("an array element has a basic type");
        let usize_ty = self.hir_types.get_usize_type();
        // { len, capacity, [length x T] }: the header, then the elements.
        let header_ty = self
            .context
            .struct_type(&[usize_ty.into(), usize_ty.into(), element_ir_ty], false);
        let first = self
            .builder
            .build_int_z_extend_or_bit_cast(
                header_ty.size_of().expect("an array header has a size"),
                usize_ty,
                "array.header",
            )
            .expect("failed to widen a size");
        let stride = self
            .builder
            .build_int_z_extend_or_bit_cast(
                element_ir_ty.size_of().expect("an element has a size"),
                usize_ty,
                "array.stride",
            )
            .expect("failed to widen a size");
        let length = self
            .builder
            .build_int_z_extend_or_bit_cast(length, usize_ty, "array.len")
            .expect("failed to widen a length");
        // The header type holds one element already.
        let rest = self
            .builder
            .build_int_mul(
                self.builder
                    .build_int_sub(
                        self.builder
                            .build_select(
                                self.builder
                                    .build_int_compare(
                                        inkwell::IntPredicate::EQ,
                                        length,
                                        usize_ty.const_zero(),
                                        "array.empty",
                                    )
                                    .expect("failed to compare a length"),
                                usize_ty.const_int(1, false),
                                length,
                                "array.at_least_one",
                            )
                            .expect("failed to select a length")
                            .into_int_value(),
                        usize_ty.const_int(1, false),
                        "array.more",
                    )
                    .expect("failed to subtract"),
                stride,
                "array.rest",
            )
            .expect("failed to multiply");
        let size = self
            .builder
            .build_int_add(first, rest, "array.size")
            .expect("failed to add");
        let align = header_ty.get_alignment();
        let (header, payload) = self.gen_region_object(size, align);
        let capacity = self
            .builder
            .build_struct_gep(header_ty, payload, 1, "region.capacity")
            .expect("failed to address a region array's capacity");
        self.builder
            .build_store(capacity, length)
            .expect("failed to store a region array's capacity");
        let array_ty = self.hir_types.get_array_type(&element_ty);
        RuntimeArrayValue::from_ptr(header, array_ty).expect("unable to construct an array")
    }

    /// The type of a frame object's header: the 32 bytes of the collector's
    /// `ObjectHeader` (`data`, `ty`, `roots`, `state`, `color`, `flags`,
    /// `class`, `inline_capacity`).
    fn frame_header_type(&self) -> StructType<'ink> {
        let ptr = self.context.ptr_type(AddressSpace::default());
        let usize_ty = self.hir_types.get_usize_type();
        let i8 = self.context.i8_type();
        self.context.struct_type(
            &[
                ptr.into(),
                ptr.into(),
                self.context.i32_type().into(),
                i8.into(),
                i8.into(),
                i8.into(),
                i8.into(),
                usize_ty.into(),
            ],
            false,
        )
    }

    /// Reserves a header and a payload of `payload_ty` in the entry block,
    /// and points the header at the payload here. Returns the header (the
    /// reference) and the payload.
    fn gen_frame_object(
        &mut self,
        expr: ExprId,
        payload_ty: BasicTypeEnum<'ink>,
    ) -> (PointerValue<'ink>, PointerValue<'ink>) {
        let header_ty = self.frame_header_type();
        let entry = self.new_alloca_builder();
        let name = format!("frame.{}", u32::from(expr.into_raw()));
        let header = entry
            .build_alloca(header_ty, &format!("{name}.header"))
            .expect("failed to reserve a frame object header");
        let payload = entry
            .build_alloca(payload_ty, &format!("{name}.data"))
            .expect("failed to reserve a frame object payload");
        let mut value = header_ty.const_zero();
        value = self
            .builder
            .build_insert_value(value, payload, 0, "frame.init")
            .expect("failed to build a frame object header")
            .into_struct_value();
        self.builder
            .build_store(header, value)
            .expect("failed to initialize a frame object header");
        (header, payload)
    }

    /// A class instance in the frame, holding `struct_lit`.
    pub(super) fn gen_struct_alloc_in_frame(
        &mut self,
        expr: ExprId,
        hir_ty: &Ty,
        struct_lit: StructValue<'ink>,
    ) -> BasicValueEnum<'ink> {
        let struct_ir_ty = self.hir_types.get_struct_type(hir_ty);
        let (header, payload) = self.gen_frame_object(expr, struct_ir_ty.as_basic_type_enum());
        self.builder
            .build_store(payload, struct_lit)
            .expect("failed to store a frame object's fields");
        RuntimeReferenceValue::from_ptr(header, struct_ir_ty)
            .expect("unable to construct a reference")
            .into()
    }

    /// An array of `len` elements in the frame, with its capacity set and
    /// its length left for the caller, like `alloc_array`.
    pub(super) fn alloc_array_in_frame(
        &mut self,
        expr: ExprId,
        len: usize,
    ) -> RuntimeArrayValue<'ink> {
        let element_ty = self.infer[expr]
            .as_array()
            .expect("the type of an array literal expression must be an Array")
            .clone();
        let element_ir_ty = self
            .hir_types
            .get_basic_type(&element_ty)
            .expect("an array element has a basic type");
        let usize_ty = self.hir_types.get_usize_type();
        let payload_ty = self.context.struct_type(
            &[
                usize_ty.into(),
                usize_ty.into(),
                element_ir_ty.array_type(len as u32).into(),
            ],
            false,
        );
        let (header, payload) = self.gen_frame_object(expr, payload_ty.into());
        let capacity = self
            .builder
            .build_struct_gep(payload_ty, payload, 1, "frame.capacity")
            .expect("failed to address a frame array's capacity");
        self.builder
            .build_store(capacity, usize_ty.const_int(len as u64, false))
            .expect("failed to store a frame array's capacity");
        let array_ty = self.hir_types.get_array_type(&element_ty);
        RuntimeArrayValue::from_ptr(header, array_ty).expect("unable to construct an array")
    }
}
