//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Namespace {
    Types,
    Values,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct PerNs<T> {
    pub types: Option<T>,
    pub values: Option<T>,
}

impl<T> Default for PerNs<T> {
    fn default() -> Self {
        PerNs {
            types: None,
            values: None,
        }
    }
}

impl<T> PerNs<T> {
    pub fn none() -> PerNs<T> {
        PerNs {
            types: None,
            values: None,
        }
    }

    pub fn values(t: T) -> PerNs<T> {
        PerNs {
            types: None,
            values: Some(t),
        }
    }

    pub fn types(t: T) -> PerNs<T> {
        PerNs {
            types: Some(t),
            values: None,
        }
    }

    pub fn both(types: T, values: T) -> PerNs<T> {
        PerNs {
            types: Some(types),
            values: Some(values),
        }
    }

    pub fn is_none(&self) -> bool {
        self.types.is_none() && self.values.is_none()
    }

    pub fn is_both(&self) -> bool {
        self.types.is_some() && self.values.is_some()
    }

    pub fn take(self, namespace: Namespace) -> Option<T> {
        match namespace {
            Namespace::Types => self.types,
            Namespace::Values => self.values,
        }
    }

    pub fn take_types(self) -> Option<T> {
        self.take(Namespace::Types)
    }

    pub fn take_values(self) -> Option<T> {
        self.take(Namespace::Values)
    }

    pub fn get(&self, namespace: Namespace) -> Option<&T> {
        self.as_ref().take(namespace)
    }

    pub fn as_ref(&self) -> PerNs<&T> {
        PerNs {
            types: self.types.as_ref(),
            values: self.values.as_ref(),
        }
    }

    pub fn or(self, other: PerNs<T>) -> PerNs<T> {
        PerNs {
            types: self.types.or(other.types),
            values: self.values.or(other.values),
        }
    }

    /// Like [`PerNs::or`], but only evaluates the fallback if some namespace
    /// is still empty.
    ///
    /// Worth having lazily because the fallback is a prelude lookup on the
    /// unqualified-name path, which is walked for every name in the program;
    /// the common case is a name that already resolved in both namespaces.
    pub fn or_else(self, f: impl FnOnce() -> PerNs<T>) -> PerNs<T> {
        if self.types.is_some() && self.values.is_some() {
            return self;
        }
        self.or(f())
    }

    pub fn and_then<U>(self, f: impl Fn(T) -> Option<U>) -> PerNs<U> {
        PerNs {
            types: self.types.and_then(&f),
            values: self.values.and_then(&f),
        }
    }

    pub fn map<U>(self, f: impl Fn(T) -> U) -> PerNs<U> {
        PerNs {
            types: self.types.map(&f),
            values: self.values.map(&f),
        }
    }
}
