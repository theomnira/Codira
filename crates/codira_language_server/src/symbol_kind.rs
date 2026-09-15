//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
/// Defines a set of symbols that can live in a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    Field,
    Function,
    Method,
    Local,
    Module,
    Extend,
    SelfParam,
    SelfType,
    Struct,
    TypeAlias,
}
