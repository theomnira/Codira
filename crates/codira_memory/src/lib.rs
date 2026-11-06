//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
// AddressSanitizer builds exempt the conservative stack scan (see
// `gc::stack_scan::read_stack_word`).
#![cfg_attr(codira_asan, feature(sanitize))]
pub use r#type::{
    ArrayType, Field, FieldData, HasStaticType, PointerType, StructType, StructTypeBuilder, Type,
    TypeCollectionStats, TypeKind,
};

pub mod ffi {
    pub use super::r#type::ffi::*;
}

pub mod arena;
mod cast;
pub mod diff;
pub mod gc;
pub mod mapping;
pub mod pages;
mod r#type;
pub mod type_table;
use codira_abi as abi;

pub mod prelude {
    pub use crate::{
        diff::{compute_struct_diff, FieldDiff, FieldEditKind, StructDiff},
        mapping::{Action, FieldMapping},
        r#type::{Field, PointerType, StructType, Type, TypeKind},
    };
}

/// An error that can occur when trying to convert from an abi type to an
/// internal type.
#[derive(Debug, thiserror::Error)]
pub enum TryFromAbiError<'a> {
    #[error("unknown TypeId '{0}'")]
    UnknownTypeId(abi::TypeId<'a>),
}
