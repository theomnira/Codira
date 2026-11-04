//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
mod effect;
mod r#enum;
mod function;
mod r#impl;
mod module;
mod package;
mod primitive_type;
pub(crate) mod session;
pub(crate) mod src;
pub(crate) mod r#struct;
pub(crate) mod r#trait;
mod type_alias;

use std::sync::Arc;

pub(crate) use self::{
    effect::lower_effect_query,
    r#enum::lower_enum_query,
    session::{lower_session_query, session_protocol_query},
};
pub use self::{
    effect::{Effect, EffectData, EffectOpSig},
    function::{Function, FunctionData},
    module::{Module, ModuleDef},
    package::Package,
    primitive_type::PrimitiveType,
    r#enum::{Enum, EnumData, EnumLayout, EnumSlot, EnumVariant, VariantKind},
    r#impl::{AssocItem, Impl, ImplData},
    r#struct::{Field, Struct, StructData, StructKind, StructMemoryKind},
    r#trait::{Trait, TraitData},
    session::{Protocol, ProtocolStep, Session, SessionData},
    src::HasSource,
    type_alias::{TypeAlias, TypeAliasData},
};
use crate::{expr::BodySourceMap, HirDatabase, Name};

/// The definitions that have a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefWithBody {
    Function(Function),
}
impl_froms!(DefWithBody: Function);

impl DefWithBody {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            DefWithBody::Function(f) => f.module(db),
        }
    }

    pub fn body_source_map(self, db: &dyn HirDatabase) -> Arc<BodySourceMap> {
        match self {
            DefWithBody::Function(f) => f.body_source_map(db),
        }
    }
}

/// Definitions that have a struct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DefWithStruct {
    Struct(Struct),
}
impl_froms!(DefWithStruct: Struct);

impl DefWithStruct {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            DefWithStruct::Struct(s) => s.module(db),
        }
    }

    pub fn fields(self, db: &dyn HirDatabase) -> Vec<Field> {
        match self {
            DefWithStruct::Struct(s) => s.fields(db),
        }
    }

    pub fn field(self, db: &dyn HirDatabase, name: &Name) -> Option<Field> {
        match self {
            DefWithStruct::Struct(s) => s.field(db, name),
        }
    }

    pub fn data(self, db: &dyn HirDatabase) -> Arc<StructData> {
        match self {
            DefWithStruct::Struct(s) => s.data(db),
        }
    }
}
