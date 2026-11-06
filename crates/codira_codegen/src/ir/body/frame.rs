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
use crate::ir::{RuntimeArrayValue, RuntimeReferenceValue};

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// Whether the allocation `expr` goes to the frame.
    pub(super) fn is_frame_allocation(&self, expr: ExprId) -> bool {
        self.escapes
            .as_ref()
            .is_some_and(|escapes| escapes.is_frame(expr))
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
