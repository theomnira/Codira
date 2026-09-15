//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use crate::{name::AsName, ty::lower::type_for_primitive, Name, Ty};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PrimitiveType {
    pub(crate) inner: crate::primitive_type::PrimitiveType,
}

impl PrimitiveType {
    /// Returns the type of the primitive
    pub fn ty(self, _db: &dyn crate::HirDatabase) -> Ty {
        type_for_primitive(self)
    }

    /// Returns the name of the primitive
    pub fn name(self) -> Name {
        self.inner.as_name()
    }
}

impl From<crate::primitive_type::PrimitiveType> for PrimitiveType {
    fn from(inner: crate::primitive_type::PrimitiveType) -> Self {
        PrimitiveType { inner }
    }
}
