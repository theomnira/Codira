//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! HIR provides high-level, object-oriented access to Codira code. It is
//! constructed by first parsing Codira code with the `codira_syntax` crate and
//! then it is lowered into HIR constructs, names are resolved, and type
//! checking is performed. HIR is the input for both the compiler as well as the
//! language server.

#![allow(dead_code)]

pub use codira_hir_input::ModuleId;
pub use salsa;

pub use self::code_model::{
    Field, Function, FunctionData, HasSource, Module, ModuleDef, Package, PrimitiveType, Struct,
    StructMemoryKind, TypeAlias,
};
pub use crate::{
    db::{
        AstDatabase, AstDatabaseStorage, DefDatabase, DefDatabaseStorage, HirDatabase,
        HirDatabaseStorage, InternDatabase, InternDatabaseStorage,
    },
    diagnostics::{Diagnostic, DiagnosticSink},
    display::HirDisplay,
    expr::{
        ArithOp, BinaryOp, Body, CmpOp, Expr, ExprId, ExprScopes, Literal, LogicOp, Ordering, Pat,
        PatId, RecordLitField, Statement, UnaryOp,
    },
    ids::{AssocItemId, ItemLoc},
    in_file::InFile,
    name::Name,
    name_resolution::{Namespace, PerNs},
    path::{Path, PathKind},
    primitive_type::{FloatBitness, IntBitness, Signedness},
    resolve::{resolver_for_expr, resolver_for_scope, Resolver, TypeNs, ValueNs},
    ty::{
        cast::{check_cast, CastCheck, CastOp, InvalidCastReason},
        lower::CallableDef,
        FloatTy, FnSig, InferenceResult, IntTy, ResolveBitness, Substitution, Ty, TyKind,
        TypableDef,
    },
    visibility::{HasVisibility, Visibility},
};
use crate::{name::AsName, source_id::AstIdMap};

#[macro_use]
mod macros;
mod code_model;
mod comptime_fold;
mod data_derive;
mod db;
pub mod diagnostics;
mod display;
mod expr;
mod ids;
mod in_file;
mod item_tree;
mod mir_lower;
mod name;
mod name_resolution;
mod path;
mod primitive_type;
mod refinement_check;
mod resolve;
mod source_id;
mod supervisor_validator;
mod ty;
mod type_ref;
mod utils;

mod has_module;
mod heal_check;
mod item_scope;
pub mod method_resolution;
#[cfg(test)]
mod mock;
mod package_defs;
mod pretty;
pub mod semantics;
mod source_analyzer;
#[cfg(test)]
mod tests;
mod visibility;
