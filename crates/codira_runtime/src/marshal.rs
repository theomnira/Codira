//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::ptr::NonNull;

use codira_memory::Type;

use crate::Runtime;

/// Used to do value-to-value conversions that require runtime type information
/// while consuming the input value.
///
/// If no `TypeInfo` is provided, the type is `()`.
pub trait Marshal<'t>: Sized {
    /// The type used in the Codira ABI
    type CodiraType;

    /// Marshals from a value (i.e. Codira -> Rust).
    fn marshal_from<'r>(value: Self::CodiraType, runtime: &'r Runtime) -> Self
    where
        Self: 't,
        'r: 't;

    /// Marshals itself into a `Marshalled` value (i.e. Rust -> Codira).
    fn marshal_into(self) -> Self::CodiraType;

    /// Marshals the value at memory location `ptr` into a `Marshalled` value
    /// (i.e. Codira -> Rust).
    fn marshal_from_ptr<'r>(
        ptr: NonNull<Self::CodiraType>,
        runtime: &'r Runtime,
        type_info: &Type,
    ) -> Self
    where
        Self: 't,
        'r: 't;

    /// Marshals `value` to memory location `ptr` (i.e. Rust -> Codira).
    fn marshal_to_ptr(value: Self, ptr: NonNull<Self::CodiraType>, type_info: &Type);
}
