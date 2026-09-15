//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#[macro_use]
mod generated;

use std::fmt;

pub use self::generated::SyntaxKind;

impl fmt::Debug for SyntaxKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.info().name;
        f.write_str(name)
    }
}

pub(crate) struct SyntaxInfo {
    pub name: &'static str,
}

impl SyntaxKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, SyntaxKind::WHITESPACE | SyntaxKind::COMMENT)
    }
}
