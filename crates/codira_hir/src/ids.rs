//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::hash::{Hash, Hasher};

use codira_hir_input::ModuleId;

use crate::{
    item_tree::{
        Effect, Enum, Function, Impl, ItemTreeId, ItemTreeNode, Session, Struct, Trait, TypeAlias,
    },
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
// Named after the id type each wraps, like the other id enums here (see
// `GenericDefId`).
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemContainerId {
    ModuleId(ModuleId),
    ImplId(ImplId),
    /// A method declared in a `trait`: a required method (no body) or a
    /// default one (a body written in terms of the trait's `Self`).
    TraitId(TraitId),
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

/// An algebraic effect declared with `effect Name { .. }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct EffectId(salsa::InternId);

pub(crate) type EffectLoc = ItemLoc<Effect>;
impl_intern!(EffectId, EffectLoc, intern_effect, lookup_intern_effect);

/// A session type declared with `session Name { .. }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct SessionId(salsa::InternId);

pub(crate) type SessionLoc = ItemLoc<Session>;
impl_intern!(SessionId, SessionLoc, intern_session, lookup_intern_session);

/// A trait declared with `trait Name { .. }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct TraitId(salsa::InternId);

pub(crate) type TraitLoc = ItemLoc<Trait>;
impl_intern!(TraitId, TraitLoc, intern_trait, lookup_intern_trait);

/// An enumeration declared with `enum Name { .. }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct EnumId(salsa::InternId);

pub(crate) type EnumLoc = ItemLoc<Enum>;
impl_intern!(EnumId, EnumLoc, intern_enum, lookup_intern_enum);

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
    /// An effect lives in the type namespace (`uses Logger`,
    /// `perform Logger.log(..)`), but it is not a type: type resolution
    /// rejects it, and only `Resolver::resolve_path_as_effect` accepts it.
    EffectId(EffectId),
    /// A session type: type resolution builds `TyKind::Session` from it
    /// through `Resolver::resolve_path_as_session`.
    SessionId(SessionId),
    /// An enum is a type; its variants are reached through it
    /// (`Shape.Circle`), never by name alone.
    EnumId(EnumId),
    /// A trait lives in the type namespace but is not a type: only
    /// `Resolver::resolve_path_as_trait` accepts it (a bound, or the trait
    /// of an `extend Type: Trait`).
    TraitId(TraitId),
}

impl From<TraitId> for ItemDefinitionId {
    fn from(id: TraitId) -> Self {
        ItemDefinitionId::TraitId(id)
    }
}

impl From<EnumId> for ItemDefinitionId {
    fn from(id: EnumId) -> Self {
        ItemDefinitionId::EnumId(id)
    }
}

impl From<EffectId> for ItemDefinitionId {
    fn from(id: EffectId) -> Self {
        ItemDefinitionId::EffectId(id)
    }
}

impl From<SessionId> for ItemDefinitionId {
    fn from(id: SessionId) -> Self {
        ItemDefinitionId::SessionId(id)
    }
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
    /// A trait. Its one parameter, index 0, is `Self`: the type a method
    /// of the trait is called on, which is known only per implementation.
    TraitId(TraitId),
}

impl From<TraitId> for GenericDefId {
    fn from(id: TraitId) -> Self {
        GenericDefId::TraitId(id)
    }
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

impl TypeParamId {
    /// The name the parameter was declared with, for messages. `None` only
    /// for an index past the owner's parameter list.
    pub fn name(self, db: &dyn DefDatabase) -> Option<crate::Name> {
        let index = self.index as usize;
        match self.owner {
            GenericDefId::FunctionId(id) => db.fn_data(id).generic_params().get(index).cloned(),
            GenericDefId::StructId(id) => db.struct_data(id).generic_params.get(index).cloned(),
            // An `extend` block's parameters are owned by the extended type
            // (see `resolve::impl_generic_param`), so none is ever owned by
            // the block itself.
            GenericDefId::ImplId(_) => None,
            GenericDefId::TraitId(_) => (index == 0).then_some(crate::name![Self]),
        }
    }
}
