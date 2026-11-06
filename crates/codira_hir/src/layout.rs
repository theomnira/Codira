//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The memory layout of a Codira value, independent of any code generator:
//!   its size, its alignment, and the offset and machine kind of every scalar
//!   it is made of. The LLVM backend lays values out with LLVM struct types
//!   (`codira_codegen::ir::ty`); the Cranelift backend has no such types and
//!   reads this instead. `codira_codegen`'s `layout_matches_llvm` checks that
//!   the two agree, offset by offset, on a battery of types.
//!
//! # The rules
//!
//! A value is laid out the way LLVM lays out the type the LLVM backend gives
//! it (`HirTypeCache::get_basic_type`):
//!
//! * a number or `bool` is one scalar; a `bool` is a byte holding 0 or 1;
//! * a reference -- a class instance, an array, a function value -- is one
//!   pointer to the object's handle; a channel or session endpoint is a
//!   `usize` runtime handle;
//! * a value struct is its fields in declaration order (read through its
//!   generic arguments), a tuple its elements, an optional `{ bool present,
//!   T payload }`, an enum `{ u32 tag, slot0, slot1, .. }` over its
//!   `EnumLayout` slots, a `str` `{ ptr, usize }` and a `String` `{ ptr buf,
//!   usize start, usize len }`;
//! * an aggregate is C-like: each member at the next offset that is a
//!   multiple of its alignment, the aggregate aligned to its most aligned
//!   member (and at least the target's aggregate alignment), its size
//!   rounded up to that alignment.
//!
//! Alignments come from the target's data layout, so an `i128` is aligned as
//! LLVM aligns it on that target.

use codira_target::abi::TargetDataLayout;

use crate::{
    mono, FloatBitness, HirDatabase, IntBitness, ResolveBitness, StructMemoryKind, Ty, TyKind,
};

/// The machine kind of one scalar of a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scalar {
    /// A `bool`: one byte, 0 or 1.
    Bool,
    /// An integer of this many bits (8, 16, 32, 64 or 128).
    Int(u8),
    /// An `f32`.
    F32,
    /// An `f64`.
    F64,
    /// A pointer: the handle of a garbage-collected object (`gc` set), or
    /// the address of bytes outside the GC heap (a `str`'s data).
    Ptr {
        /// Whether the collector must trace it.
        gc: bool,
    },
}

/// Where one scalar of a value lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Leaf {
    /// The offset from the start of the value, in bytes.
    pub offset: u64,
    pub scalar: Scalar,
}

/// The layout of a value: what [`value_layout`] returns.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ValueLayout {
    /// The size in bytes, a multiple of [`ValueLayout::align`].
    pub size: u64,
    /// The alignment in bytes.
    pub align: u64,
    /// Every scalar, in memory order. Bytes between them are padding.
    pub leaves: Vec<Leaf>,
}

impl ValueLayout {
    fn scalar(scalar: Scalar, size: u64, align: u64) -> Self {
        ValueLayout {
            size,
            align,
            leaves: vec![Leaf { offset: 0, scalar }],
        }
    }

    /// The aggregate of `members`, in order, as LLVM lays out a non-packed
    /// struct; also returns the offset of each member.
    pub fn aggregate(
        layout: &TargetDataLayout,
        members: impl IntoIterator<Item = ValueLayout>,
    ) -> (ValueLayout, Vec<u64>) {
        let mut size = 0u64;
        let mut align = layout.aggregate_align.abi.bytes().max(1);
        let mut leaves = Vec::new();
        let mut offsets = Vec::new();
        for member in members {
            size = size.next_multiple_of(member.align);
            offsets.push(size);
            leaves.extend(member.leaves.iter().map(|leaf| Leaf {
                offset: leaf.offset + size,
                scalar: leaf.scalar,
            }));
            size += member.size;
            align = align.max(member.align);
        }
        (
            ValueLayout {
                size: size.next_multiple_of(align),
                align,
                leaves,
            },
            offsets,
        )
    }

    /// The distance between consecutive elements of an array of this value.
    pub fn stride(&self) -> u64 {
        self.size.next_multiple_of(self.align)
    }
}

/// The layout of one scalar kind on `layout`'s target.
pub fn scalar_layout(layout: &TargetDataLayout, scalar: Scalar) -> ValueLayout {
    let (size, align) = match scalar {
        Scalar::Bool => (1, layout.i1_align.abi.bytes()),
        Scalar::Int(8) => (1, layout.i8_align.abi.bytes()),
        Scalar::Int(16) => (2, layout.i16_align.abi.bytes()),
        Scalar::Int(32) => (4, layout.i32_align.abi.bytes()),
        Scalar::Int(64) => (8, layout.i64_align.abi.bytes()),
        Scalar::Int(_) => (16, layout.i128_align.abi.bytes()),
        Scalar::F32 => (4, layout.f32_align.abi.bytes()),
        Scalar::F64 => (8, layout.f64_align.abi.bytes()),
        Scalar::Ptr { .. } => (
            layout.pointer_size.bytes(),
            layout.pointer_align.abi.bytes(),
        ),
    };
    ValueLayout::scalar(scalar, size, align.max(1))
}

/// The scalar kind of a pointer-sized unsigned integer.
pub fn usize_scalar(layout: &TargetDataLayout) -> Scalar {
    Scalar::Int(u8::try_from(layout.pointer_size.bits()).unwrap_or(64))
}

/// The layout of a value of type `ty`, or `None` for a type that has no
/// value representation (a function definition, a type parameter, `never`).
/// Unit is the empty aggregate.
pub fn value_layout(db: &dyn HirDatabase, ty: &Ty) -> Option<ValueLayout> {
    let target = db.target_data_layout();
    let layout = &*target;
    let scalar = |s| Some(scalar_layout(layout, s));
    let gc_ptr = Scalar::Ptr { gc: true };
    let usize = usize_scalar(layout);
    match ty.interned() {
        TyKind::Bool => scalar(Scalar::Bool),
        TyKind::Int(int_ty) => scalar(Scalar::Int(match int_ty.resolve(layout).bitness {
            IntBitness::X8 => 8,
            IntBitness::X16 => 16,
            IntBitness::X32 => 32,
            IntBitness::X64 => 64,
            IntBitness::X128 => 128,
            IntBitness::Xsize => match usize {
                Scalar::Int(bits) => bits,
                _ => 64,
            },
        })),
        TyKind::Float(float_ty) => scalar(match float_ty.resolve(layout).bitness {
            FloatBitness::X32 => Scalar::F32,
            FloatBitness::X64 => Scalar::F64,
        }),
        TyKind::Array(_) | TyKind::Function(_) => scalar(gc_ptr),
        TyKind::Channel(_) | TyKind::Session(..) => scalar(usize),
        TyKind::Struct(s, _) => match s.data(db).memory_kind {
            StructMemoryKind::Gc => scalar(gc_ptr),
            StructMemoryKind::Value => object_layout(db, ty).map(|(layout, _)| layout),
        },
        TyKind::Str => Some(
            ValueLayout::aggregate(
                layout,
                [
                    scalar_layout(layout, Scalar::Ptr { gc: false }),
                    scalar_layout(layout, usize),
                ],
            )
            .0,
        ),
        TyKind::String => Some(string_layout(layout)),
        TyKind::Optional(payload) => {
            let payload = value_layout(db, payload)?;
            Some(ValueLayout::aggregate(layout, [scalar_layout(layout, Scalar::Bool), payload]).0)
        }
        TyKind::Tuple(_, elements) => {
            let members = elements
                .iter()
                .map(|element| value_layout(db, element))
                .collect::<Option<Vec<_>>>()?;
            Some(ValueLayout::aggregate(layout, members).0)
        }
        TyKind::Enum(e, args) => {
            let enum_layout = e.layout_at(db, args);
            let members = std::iter::once(Some(scalar_layout(layout, Scalar::Int(32))))
                .chain(
                    enum_layout
                        .slots
                        .iter()
                        .map(|slot| value_layout(db, &slot.ty)),
                )
                .collect::<Option<Vec<_>>>()?;
            Some(ValueLayout::aggregate(layout, members).0)
        }
        _ => None,
    }
}

/// A `String`: `{ ptr buf, usize start, usize len }`, `buf` the handle of a
/// `[u8]` array object.
pub fn string_layout(layout: &TargetDataLayout) -> ValueLayout {
    let usize = scalar_layout(layout, usize_scalar(layout));
    ValueLayout::aggregate(
        layout,
        [
            scalar_layout(layout, Scalar::Ptr { gc: true }),
            usize.clone(),
            usize,
        ],
    )
    .0
}

/// The layout of the fields of the struct type `ty` -- the payload of a
/// class instance, or a value struct itself -- and the offset of each field,
/// in declaration order.
pub fn object_layout(db: &dyn HirDatabase, ty: &Ty) -> Option<(ValueLayout, Vec<u64>)> {
    let target = db.target_data_layout();
    let fields = mono::struct_field_types(db, ty)
        .iter()
        .map(|field| value_layout(db, field))
        .collect::<Option<Vec<_>>>()?;
    Some(ValueLayout::aggregate(&target, fields))
}

/// The payload of an array object: `{ usize len, usize capacity, T
/// elements[] }`. Returns the offset of the first element and the distance
/// between elements.
pub fn array_elements(db: &dyn HirDatabase, element: &Ty) -> Option<(u64, u64)> {
    let target = db.target_data_layout();
    let element = value_layout(db, element)?;
    let usize = scalar_layout(&target, usize_scalar(&target));
    let (_, offsets) = ValueLayout::aggregate(&target, [usize.clone(), usize, element.clone()]);
    Some((offsets[2], element.stride()))
}
