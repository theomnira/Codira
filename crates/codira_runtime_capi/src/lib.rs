//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! The Codira Runtime C API
//!
//! The Codira Runtime C API exposes runtime functionality using the C ABI. This
//! can be used to integrate the Codira Runtime into other languages that allow
//! interoperability with C.
#![warn(missing_docs)]

pub mod alloc;
pub mod diagnostics;
pub mod gc;
pub mod runtime;

pub mod function;

#[macro_use]
#[cfg(test)]
mod test_util;
