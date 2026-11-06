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
    Effect, EffectData, EffectOpSig, Enum, EnumLayout, EnumSlot, EnumVariant, Field, Function,
    FunctionData, HasSource, Module, ModuleDef, Package, PrimitiveType, Protocol, ProtocolStep,
    Session, Struct, StructKind, StructMemoryKind, Trait, TypeAlias, VariantKind,
};
pub use crate::{
    builtin::{
        is_inlined_closure_arg, is_inlined_closure_literal, ArrayMethod, BuiltinFn,
        ElementRequirement, FormatAlign, FormatCount, FormatKind, FormatSpec, PipelineOp,
        StringArg, StringOp, ValueClass,
    },
    db::{
        AstDatabase, AstDatabaseStorage, DefDatabase, DefDatabaseStorage, HirDatabase,
        HirDatabaseStorage, InternDatabase, InternDatabaseStorage,
    },
    diagnostics::{Diagnostic, DiagnosticSink},
    display::HirDisplay,
    expr::{
        direct_spawns, opened_session, ArithOp, BinaryOp, Body, BodyContracts, CmpOp, Expr, ExprId,
        ExprScopes, HandlerArm, Literal, LogicOp, LoopContract, MatchArm, MatchSource, Ordering,
        Pat, PatId, RecordLitField, SelectArm, SelectOp, SessionFlow, Statement, UnaryOp,
    },
    ids::{AssocItemId, ItemLoc},
    in_file::InFile,
    name::Name,
    name_resolution::{Namespace, PerNs},
    path::{import_paths_in_source, Path, PathKind},
    primitive_type::{FloatBitness, IntBitness, Signedness},
    resolve::{resolver_for_expr, resolver_for_scope, Resolver, TypeNs, ValueNs},
    ty::{
        cast::{check_cast, CastCheck, CastOp, InvalidCastReason},
        error_conversions, group_shape,
        lower::CallableDef,
        main_result, result_variants, ClosureInfo, FloatTy, FnSig, GroupShape, InferenceResult,
        IntTy, MainResult, ResolveBitness, Substitution, Ty, TyKind, TypableDef, TypeWalk,
    },
    visibility::{HasVisibility, Visibility},
};
use crate::{name::AsName, source_id::AstIdMap};

#[macro_use]
mod macros;
mod builtin;
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
pub mod mono;
mod name;
mod name_resolution;
mod path;
mod primitive_type;
pub mod realtime;
mod refinement_check;
mod resolve;
mod source_id;
mod supervisor_validator;
pub mod traits;
mod ty;
mod type_ref;
mod unsupported_syntax;
mod utils;
pub mod verify;

mod has_module;
mod heal_check;
pub mod heal_contract;
pub mod inline_asm;
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
