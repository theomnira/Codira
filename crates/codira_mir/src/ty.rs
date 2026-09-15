//! Copyright (c) 2026 Omnira CJSC
//!
//! The Eidos type system (RFC-001 §1).
//!
//! # Why [`TypeId`] is packed rather than always-interned
//!
//! RFC-001 specifies interned types so that equality is an integer
//! compare -- the e-graph hashconses constantly and cannot afford
//! structural comparison. A naive interner, however, would mean
//! [`Body::push`](crate::Body::push) needs a `&mut TypeStore` to name even
//! `i64`, which would force a store parameter through every IR
//! construction site in the workspace, including every test.
//!
//! So `TypeId` is a **tagged u32**:
//!
//! ```text
//!  bit 31 == 0   inline primitive, fully encoded in the low bits
//!  bit 31 == 1   index into a TypeStore (composite: Tuple/Struct/Ptr/Param)
//! ```
//!
//! Primitives -- every `Int`, `Float`, `Bool`, `Unit`, `Mem` -- are
//! therefore **store-free**: `TypeId::I64` is a constant, comparable and
//! constructible anywhere, with no allocation and no context. Only
//! composites need a store. Equality stays a `u32` compare in both cases.
//!
//! # `UNTYPED` and the migration
//!
//! RFC-001 §1.8 phase 1 proposed defaulting untyped ops to `Int{64}`.
//! This module uses a distinct [`TypeId::UNTYPED`] sentinel instead,
//! which is strictly better: it lets the verifier distinguish "not yet
//! migrated" from "genuinely i64", lets phase 2 progress be *measured*
//! (count the `UNTYPED` ops), and makes it impossible for a
//! not-yet-migrated op to be silently mistaken for a well-typed 64-bit
//! one by a later pass.
//!
//! `UNTYPED` is accepted by the verifier and skipped by type checking.
//! When phase 2 completes, `UNTYPED` becomes a verifier error.

use rustc_hash::FxHashMap;
use smol_str::SmolStr;

/// A type, either encoded inline in the id or interned in a [`TypeStore`].
///
/// Equality and hashing are on the raw `u32`, which is what makes this
/// usable as an e-graph hashcons key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(u32);

const COMPOSITE_BIT: u32 = 1 << 31;

// Inline primitive layout (bit 31 clear):
//   bits 0..3   kind tag
//   bits 4..7   width code (Int/Float only)
//   bit  8      signedness (Int only)
const TAG_UNTYPED: u32 = 0;
const TAG_BOOL: u32 = 1;
const TAG_UNIT: u32 = 2;
const TAG_MEM: u32 = 3;
const TAG_INT: u32 = 4;
const TAG_FLOAT: u32 = 5;
const TAG_NEVER: u32 = 6;

const WIDTH_SHIFT: u32 = 4;
const SIGNED_BIT: u32 = 1 << 8;

/// Width codes, kept dense so the packed form stays small.
fn width_code(width: u16) -> u32 {
    match width {
        8 => 1,
        16 => 2,
        32 => 3,
        64 => 4,
        128 => 5,
        _ => panic!("unsupported integer/float width: {width}"),
    }
}

fn code_width(code: u32) -> u16 {
    match code {
        1 => 8,
        2 => 16,
        3 => 32,
        4 => 64,
        5 => 128,
        _ => unreachable!("malformed TypeId width code {code}"),
    }
}

impl TypeId {
    /// Not yet assigned a type -- see the module doc. Accepted by the
    /// verifier during the phase-1/2 migration; an error afterwards.
    pub const UNTYPED: TypeId = TypeId(TAG_UNTYPED);
    pub const BOOL: TypeId = TypeId(TAG_BOOL);
    pub const UNIT: TypeId = TypeId(TAG_UNIT);
    /// The memory state token (RFC-002 §2.2). Reserved now so the
    /// Memory-SSA work does not have to renumber the tag space.
    pub const MEM: TypeId = TypeId(TAG_MEM);
    /// The empty type: the result of a diverging computation.
    pub const NEVER: TypeId = TypeId(TAG_NEVER);

    pub const I8: TypeId = TypeId::int_const(1, true);
    pub const I16: TypeId = TypeId::int_const(2, true);
    pub const I32: TypeId = TypeId::int_const(3, true);
    pub const I64: TypeId = TypeId::int_const(4, true);
    pub const I128: TypeId = TypeId::int_const(5, true);
    pub const U8: TypeId = TypeId::int_const(1, false);
    pub const U16: TypeId = TypeId::int_const(2, false);
    pub const U32: TypeId = TypeId::int_const(3, false);
    pub const U64: TypeId = TypeId::int_const(4, false);
    pub const U128: TypeId = TypeId::int_const(5, false);
    pub const F32: TypeId = TypeId(TAG_FLOAT | (3 << WIDTH_SHIFT));
    pub const F64: TypeId = TypeId(TAG_FLOAT | (4 << WIDTH_SHIFT));

    const fn int_const(code: u32, signed: bool) -> TypeId {
        TypeId(TAG_INT | (code << WIDTH_SHIFT) | if signed { SIGNED_BIT } else { 0 })
    }

    /// An integer type of the given width and signedness.
    pub fn int(width: u16, signed: bool) -> TypeId {
        TypeId(TAG_INT | (width_code(width) << WIDTH_SHIFT) | if signed { SIGNED_BIT } else { 0 })
    }

    /// A float type of the given width (32 or 64).
    pub fn float(width: u16) -> TypeId {
        assert!(matches!(width, 32 | 64), "unsupported float width: {width}");
        TypeId(TAG_FLOAT | (width_code(width) << WIDTH_SHIFT))
    }

    pub fn is_composite(self) -> bool {
        self.0 & COMPOSITE_BIT != 0
    }

    pub fn is_untyped(self) -> bool {
        self == TypeId::UNTYPED
    }

    /// The integer width and signedness, if this is an integer type.
    pub fn as_int(self) -> Option<(u16, bool)> {
        if self.is_composite() || self.0 & 0xF != TAG_INT {
            return None;
        }
        Some((
            code_width((self.0 >> WIDTH_SHIFT) & 0xF),
            self.0 & SIGNED_BIT != 0,
        ))
    }

    /// The float width, if this is a float type.
    pub fn as_float(self) -> Option<u16> {
        if self.is_composite() || self.0 & 0xF != TAG_FLOAT {
            return None;
        }
        Some(code_width((self.0 >> WIDTH_SHIFT) & 0xF))
    }

    pub fn is_int(self) -> bool {
        self.as_int().is_some()
    }

    pub fn is_float(self) -> bool {
        self.as_float().is_some()
    }

    /// Bit width of a scalar type, for cast legality checks.
    pub fn scalar_width(self) -> Option<u16> {
        if let Some((width, _)) = self.as_int() {
            return Some(width);
        }
        if let Some(width) = self.as_float() {
            return Some(width);
        }
        // `bool` is one bit: relevant to bitcast legality, where an
        // i1 and a wider integer must not be interchangeable.
        (self == TypeId::BOOL).then_some(1)
    }

    /// Whether arithmetic (`add`/`sub`/`mul`/`div`/`rem`/`neg`) is
    /// defined on this type.
    pub fn is_arithmetic(self) -> bool {
        self.is_int() || self.is_float()
    }

    fn composite_index(self) -> Option<usize> {
        self.is_composite()
            .then_some((self.0 & !COMPOSITE_BIT) as usize)
    }
}

/// A structural view of a [`TypeId`]. Obtained via
/// [`TypeStore::get`], which resolves composites and decodes primitives.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Untyped,
    Bool,
    Unit,
    Never,
    /// The memory state token (RFC-002 §2.2).
    Mem,
    Int {
        width: u16,
        signed: bool,
    },
    Float {
        width: u16,
    },
    Tuple(Vec<TypeId>),
    /// A pointer. `space` is the LLVM address space.
    Ptr {
        pointee: TypeId,
        space: u16,
    },
    /// A nominal struct; layout lives in the frontend's side table.
    Struct {
        name: SmolStr,
    },
    /// An unresolved generic parameter, substituted by elaboration.
    Param {
        name: SmolStr,
    },
}

/// Interner for composite types. Primitives never reach it (see the
/// module doc), so a store is only needed once tuples, pointers, structs
/// or generic parameters appear.
#[derive(Debug, Default, Clone)]
pub struct TypeStore {
    types: Vec<Type>,
    interned: FxHashMap<Type, TypeId>,
}

impl TypeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Interns `ty`, returning its id. Primitive types are encoded inline
    /// and never stored.
    pub fn intern(&mut self, ty: Type) -> TypeId {
        // Primitives round-trip through the packed encoding.
        match &ty {
            Type::Untyped => return TypeId::UNTYPED,
            Type::Bool => return TypeId::BOOL,
            Type::Unit => return TypeId::UNIT,
            Type::Never => return TypeId::NEVER,
            Type::Mem => return TypeId::MEM,
            Type::Int { width, signed } => return TypeId::int(*width, *signed),
            Type::Float { width } => return TypeId::float(*width),
            _ => {}
        }
        if let Some(&id) = self.interned.get(&ty) {
            return id;
        }
        let index = self.types.len() as u32;
        assert!(
            index & COMPOSITE_BIT == 0,
            "TypeStore exhausted: more than 2^31 composite types"
        );
        let id = TypeId(index | COMPOSITE_BIT);
        self.types.push(ty.clone());
        self.interned.insert(ty, id);
        id
    }

    pub fn tuple(&mut self, elements: impl IntoIterator<Item = TypeId>) -> TypeId {
        self.intern(Type::Tuple(elements.into_iter().collect()))
    }

    pub fn ptr(&mut self, pointee: TypeId) -> TypeId {
        self.intern(Type::Ptr { pointee, space: 0 })
    }

    pub fn struct_ty(&mut self, name: impl Into<SmolStr>) -> TypeId {
        self.intern(Type::Struct { name: name.into() })
    }

    pub fn param(&mut self, name: impl Into<SmolStr>) -> TypeId {
        self.intern(Type::Param { name: name.into() })
    }

    /// Resolves a [`TypeId`] to its structural form. Total: primitives
    /// decode without consulting the store.
    pub fn get(&self, id: TypeId) -> Type {
        if let Some(index) = id.composite_index() {
            return self.types[index].clone();
        }
        match id.0 & 0xF {
            TAG_UNTYPED => Type::Untyped,
            TAG_BOOL => Type::Bool,
            TAG_UNIT => Type::Unit,
            TAG_MEM => Type::Mem,
            TAG_NEVER => Type::Never,
            TAG_INT => {
                let (width, signed) = id.as_int().expect("tag says int");
                Type::Int { width, signed }
            }
            TAG_FLOAT => Type::Float {
                width: id.as_float().expect("tag says float"),
            },
            other => unreachable!("malformed TypeId tag {other}"),
        }
    }

    /// Element types of a tuple, or `None` if `id` is not a tuple.
    pub fn tuple_elements(&self, id: TypeId) -> Option<Vec<TypeId>> {
        match self.get(id) {
            Type::Tuple(elements) => Some(elements),
            _ => None,
        }
    }

    /// Renders a type for diagnostics and the textual IR form.
    pub fn display(&self, id: TypeId) -> String {
        match self.get(id) {
            Type::Untyped => "?".to_string(),
            Type::Bool => "bool".to_string(),
            Type::Unit => "unit".to_string(),
            Type::Never => "!".to_string(),
            Type::Mem => "mem".to_string(),
            Type::Int { width, signed } => {
                format!("{}{width}", if signed { 'i' } else { 'u' })
            }
            Type::Float { width } => format!("f{width}"),
            Type::Tuple(elements) => {
                let inner: Vec<String> = elements.iter().map(|&e| self.display(e)).collect();
                format!("({})", inner.join(", "))
            }
            Type::Ptr { pointee, space } => {
                let base = format!("*{}", self.display(pointee));
                if space == 0 {
                    base
                } else {
                    format!("{base} addrspace({space})")
                }
            }
            Type::Struct { name } => name.to_string(),
            Type::Param { name } => format!("@{name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_are_store_free_and_roundtrip() {
        let store = TypeStore::new();
        assert_eq!(
            store.get(TypeId::I32),
            Type::Int {
                width: 32,
                signed: true
            }
        );
        assert_eq!(
            store.get(TypeId::U8),
            Type::Int {
                width: 8,
                signed: false
            }
        );
        assert_eq!(store.get(TypeId::F64), Type::Float { width: 64 });
        assert_eq!(store.get(TypeId::BOOL), Type::Bool);
        assert_eq!(store.get(TypeId::MEM), Type::Mem);
        assert_eq!(store.get(TypeId::UNTYPED), Type::Untyped);
    }

    #[test]
    fn int_accessors_decode_correctly() {
        assert_eq!(TypeId::I64.as_int(), Some((64, true)));
        assert_eq!(TypeId::U16.as_int(), Some((16, false)));
        assert_eq!(TypeId::F32.as_int(), None);
        assert_eq!(TypeId::F32.as_float(), Some(32));
        assert_eq!(TypeId::I8.as_float(), None);
        assert_eq!(TypeId::I128.as_int(), Some((128, true)));
    }

    #[test]
    fn signedness_and_width_are_independent() {
        // The bug this guards: packing signedness into the width code
        // would make u64 and i64 collide, silently selecting bvudiv for
        // signed division.
        assert_ne!(TypeId::I64, TypeId::U64);
        assert_ne!(TypeId::I32, TypeId::I64);
        assert_eq!(TypeId::int(64, true), TypeId::I64);
        assert_eq!(TypeId::int(8, false), TypeId::U8);
    }

    #[test]
    fn composites_intern_and_dedupe() {
        let mut store = TypeStore::new();
        let a = store.tuple([TypeId::I32, TypeId::BOOL]);
        let b = store.tuple([TypeId::I32, TypeId::BOOL]);
        let c = store.tuple([TypeId::I32, TypeId::I32]);
        assert_eq!(a, b, "identical tuples must intern to one id");
        assert_ne!(a, c);
        assert!(a.is_composite());
        assert!(!TypeId::I32.is_composite());
        assert_eq!(
            store.tuple_elements(a),
            Some(vec![TypeId::I32, TypeId::BOOL])
        );
    }

    #[test]
    fn interning_primitives_does_not_grow_the_store() {
        let mut store = TypeStore::new();
        for _ in 0..100 {
            store.intern(Type::Int {
                width: 64,
                signed: true,
            });
            store.intern(Type::Bool);
        }
        assert_eq!(store.types.len(), 0, "primitives must never be stored");
    }

    #[test]
    fn display_is_readable() {
        let mut store = TypeStore::new();
        assert_eq!(store.display(TypeId::I32), "i32");
        assert_eq!(store.display(TypeId::U8), "u8");
        assert_eq!(store.display(TypeId::F64), "f64");
        assert_eq!(store.display(TypeId::UNTYPED), "?");
        let t = store.tuple([TypeId::I32, TypeId::BOOL]);
        assert_eq!(store.display(t), "(i32, bool)");
        let p = store.ptr(TypeId::U8);
        assert_eq!(store.display(p), "*u8");
    }

    #[test]
    fn scalar_width_covers_bool() {
        assert_eq!(TypeId::BOOL.scalar_width(), Some(1));
        assert_eq!(TypeId::I32.scalar_width(), Some(32));
        assert_eq!(TypeId::F64.scalar_width(), Some(64));
        assert_eq!(TypeId::UNIT.scalar_width(), None);
    }

    #[test]
    fn type_ids_are_four_bytes() {
        // The whole point of the packed representation: adding `ty` to
        // every Op must cost 4 bytes, not a pointer plus indirection.
        assert_eq!(std::mem::size_of::<TypeId>(), 4);
    }
}
