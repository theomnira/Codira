//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::ops::Index;

use rustc_hash::FxHashMap;

use crate::SourceRootId;

/// Represents the id of a single package, all packages have a unique id, the
/// main package and all dependent packages.
///
/// The [`PackageId`] is used to uniquely identify a package in the project. To
/// gain access to the data of a package, the [`PackageSet`] is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageId(pub u32);

/// Top-level information for a single package in the project.
///
/// Packages are usually represented by a [`PackageId`]. The [`PackageSet`]
/// stores a mapping from [`PackageId`] to [`PackageData`].
#[derive(Debug, Clone)]
pub struct PackageData {
    /// The source root which groups together all the source files of a package.
    pub source_root: SourceRootId,

    /// The name other packages reach this package by (`std`), or `None` for
    /// the package being compiled, which no other package refers to.
    ///
    /// A named package's root module is in scope in every *other* package
    /// under this name, after the importing module's own items, the
    /// built-in types and the importing package's prelude, so a package can
    /// still define a module of the same name for itself.
    pub name: Option<String>,
}

/// Contains information about all the packages in the project.
#[derive(Debug, Clone, Default)]
pub struct PackageSet {
    arena: FxHashMap<PackageId, PackageData>,
}

impl PackageSet {
    /// Adds a new package to the package set with the source files located add
    /// the specified root. Returns the `PackageId` associated with the package.
    pub fn add_package(&mut self, source_root: SourceRootId) -> PackageId {
        self.insert(PackageData {
            source_root,
            name: None,
        })
    }

    /// Adds a package that other packages refer to by `name` (see
    /// [`PackageData::name`]). Returns the `PackageId` associated with it.
    pub fn add_named_package(
        &mut self,
        source_root: SourceRootId,
        name: impl Into<String>,
    ) -> PackageId {
        self.insert(PackageData {
            source_root,
            name: Some(name.into()),
        })
    }

    fn insert(&mut self, data: PackageData) -> PackageId {
        let package_id = PackageId(self.arena.len() as u32);
        self.arena.insert(package_id, data);
        package_id
    }

    /// Iterates over all packages, in the order they were added.
    ///
    /// The order is fixed (not the hash map's), so that everything walking
    /// the packages -- diagnostics, the libraries a build writes -- does so
    /// in the same order in every run.
    pub fn iter(&self) -> impl Iterator<Item = PackageId> + '_ {
        (0..self.arena.len() as u32).map(PackageId)
    }

    /// The package other packages refer to by `name`, if there is one.
    pub fn by_name(&self, name: &str) -> Option<PackageId> {
        self.iter()
            .find(|id| self.arena[id].name.as_deref() == Some(name))
    }
}

impl Index<PackageId> for PackageSet {
    type Output = PackageData;

    fn index(&self, index: PackageId) -> &Self::Output {
        &self.arena[&index]
    }
}
