//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::hash::{Hash, Hasher};

use codira_hir_input::ModuleId;

use crate::{
    item_tree::{Function, Impl, ItemTreeId, ItemTreeNode, Struct, TypeAlias},
    primitive_type::PrimitiveType,
    DefDatabase,
};

#[derive(Clone, Debug)]
pub struct ItemLoc<N: ItemTreeNode> {
    pub module: ModuleId,
    pub id: ItemTreeId<N>,
}

impl<N: ItemTreeNode> PartialEq for ItemLoc<N> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.module == other.module
    }
}

impl<N: ItemTreeNode> Eq for ItemLoc<N> {}

impl<N: ItemTreeNode> Hash for ItemLoc<N> {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.module.hash(hasher);
        self.id.hash(hasher);
    }
}

impl<N: ItemTreeNode> Copy for ItemLoc<N> {}

#[derive(Clone, Debug)]
pub struct AssocItemLoc<N: ItemTreeNode> {
    pub container: ItemContainerId,
    pub id: ItemTreeId<N>,
}

impl<N: ItemTreeNode> Copy for AssocItemLoc<N> {}

impl<N: ItemTreeNode> PartialEq for AssocItemLoc<N> {
    fn eq(&self, other: &Self) -> bool {
        self.container == other.container && self.id == other.id
    }
}

impl<N: ItemTreeNode> Eq for AssocItemLoc<N> {}

impl<N: ItemTreeNode> Hash for AssocItemLoc<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.container.hash(state);
        self.id.hash(state);
    }
}

macro_rules! impl_intern_key {
    ($name:ident) => {
        impl salsa::InternKey for $name {
            fn from_intern_id(v: salsa::InternId) -> Self {
                $name(v)
            }
            fn as_intern_id(&self) -> salsa::InternId {
                self.0
            }
        }
    };
}

macro_rules! impl_intern {
    ($id:ident, $loc:ident, $intern:ident, $lookup:ident) => {
        impl_intern_key!($id);

        impl Intern for $loc {
            type ID = $id;
            fn intern(self, db: &dyn DefDatabase) -> $id {
                db.$intern(self)
            }
        }

        impl Lookup for $id {
            type Data = $loc;
            fn lookup(&self, db: &dyn DefDatabase) -> $loc {
                db.$lookup(*self)
            }
        }
    };
}

/// Represents an id of an item inside a item container such as a module or a
/// `impl` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemContainerId {
    ModuleId(ModuleId),
    ImplId(ImplId),
}
impl From<ModuleId> for ItemContainerId {
    fn from(value: ModuleId) -> Self {
        ItemContainerId::ModuleId(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct ImplId(salsa::InternId);

pub(crate) type ImplLoc = ItemLoc<Impl>;
impl_intern!(ImplId, ImplLoc, intern_impl, lookup_intern_impl);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct FunctionId(salsa::InternId);

pub(crate) type FunctionLoc = AssocItemLoc<Function>;
impl_intern!(
    FunctionId,
    FunctionLoc,
    intern_function,
    lookup_intern_function
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StructId(salsa::InternId);

pub(crate) type StructLoc = ItemLoc<Struct>;
impl_intern!(StructId, StructLoc, intern_struct, lookup_intern_struct);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeAliasId(salsa::InternId);

pub(crate) type TypeAliasLoc = ItemLoc<TypeAlias>;
impl_intern!(
    TypeAliasId,
    TypeAliasLoc,
    intern_type_alias,
    lookup_intern_type_alias
);

pub trait Intern {
    type ID;
    fn intern(self, db: &dyn DefDatabase) -> Self::ID;
}

pub trait Lookup {
    type Data;
    fn lookup(&self, db: &dyn DefDatabase) -> Self::Data;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemDefinitionId {
    ModuleId(ModuleId),
    FunctionId(FunctionId),
    StructId(StructId),
    TypeAliasId(TypeAliasId),
    PrimitiveType(PrimitiveType),
}

impl From<ModuleId> for ItemDefinitionId {
    fn from(id: ModuleId) -> Self {
        ItemDefinitionId::ModuleId(id)
    }
}

impl From<FunctionId> for ItemDefinitionId {
    fn from(id: FunctionId) -> Self {
        ItemDefinitionId::FunctionId(id)
    }
}

impl From<StructId> for ItemDefinitionId {
    fn from(id: StructId) -> Self {
        ItemDefinitionId::StructId(id)
    }
}

impl From<TypeAliasId> for ItemDefinitionId {
    fn from(id: TypeAliasId) -> Self {
        ItemDefinitionId::TypeAliasId(id)
    }
}

impl From<PrimitiveType> for ItemDefinitionId {
    fn from(id: PrimitiveType) -> Self {
        ItemDefinitionId::PrimitiveType(id)
    }
}

/// Items that are associated with an `impl`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum AssocItemId {
    FunctionId(FunctionId),
}

/// Definitions which have a body
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefWithBodyId {
    FunctionId(FunctionId),
}

impl From<FunctionId> for DefWithBodyId {
    fn from(id: FunctionId) -> Self {
        DefWithBodyId::FunctionId(id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VariantId {
    StructId(StructId),
}
impl From<StructId> for VariantId {
    fn from(value: StructId) -> Self {
        VariantId::StructId(value)
    }
}

/// A declaration that may carry `[T, U]`-style generic parameters.
///
/// Generic parameters are scoped to their declaring item, so a type
/// parameter's identity is (owner, index) rather than a bare name -- the `T`
/// of `func map[T](..)` is a different type from the `T` of
/// `struct Box[T]`, and confusing them would let one unify with the other.
// Variants are named after the id type each wraps, as every other id enum
// in this module is (`ItemDefinitionId::FunctionId`,
// `ItemContainerId::ImplId`). Renaming them to `Function`/`Struct`/`Impl`
// to satisfy the lint would make this the one enum here that reads
// differently from its neighbours.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenericDefId {
    FunctionId(FunctionId),
    StructId(StructId),
    /// An `extend Box[T] { .. }` block. Its parameters are the
    /// binding occurrences among the extended type's generic arguments --
    /// `T` in `extend Box[T]`, but not `i32` in `extend Box[i32]`.
    ImplId(ImplId),
}

impl From<FunctionId> for GenericDefId {
    fn from(id: FunctionId) -> Self {
        GenericDefId::FunctionId(id)
    }
}

impl From<StructId> for GenericDefId {
    fn from(id: StructId) -> Self {
        GenericDefId::StructId(id)
    }
}

impl From<ImplId> for GenericDefId {
    fn from(id: ImplId) -> Self {
        GenericDefId::ImplId(id)
    }
}

/// A single generic type parameter, identified by its owner and its position
/// in that owner's parameter list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeParamId {
    pub owner: GenericDefId,
    pub index: u32,
}
