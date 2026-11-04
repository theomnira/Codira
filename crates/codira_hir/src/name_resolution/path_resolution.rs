//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::iter::successors;

use codira_hir_input::{ModuleId, PackageId, PackageModuleId};

use crate::{
    ids::ItemDefinitionId, item_scope::BUILTIN_SCOPE, package_defs::PackageDefs, DefDatabase, Name,
    Path, PathKind, PerNs, Visibility,
};

/// Indicates whether or not any newly resolved import statements will actually
/// change the outcome of an operation. This is useful to know if more
/// iterations of an algorithm might be required, or whether its hopeless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachedFixedPoint {
    Yes,
    No,
}

/// Contains the result of resolving a path. It contains how far the path was
/// able to be resolved as well as the resolved values or types so far.
#[derive(Debug, Clone)]
pub(crate) struct ResolvePathResult {
    pub(crate) resolved_def: PerNs<(ItemDefinitionId, Visibility)>,
    pub(crate) segment_index: Option<usize>,
    pub(crate) reached_fixedpoint: ReachedFixedPoint,
    pub(crate) package: Option<PackageId>,
}

impl ResolvePathResult {
    /// Constructs an empty `ResolvePathResult`
    fn empty(reached_fixedpoint: ReachedFixedPoint) -> ResolvePathResult {
        ResolvePathResult::with(PerNs::none(), reached_fixedpoint, None, None)
    }

    /// Constructs a new instance of `ResolvePathResult`
    fn with(
        resolved_def: PerNs<(ItemDefinitionId, Visibility)>,
        reached_fixedpoint: ReachedFixedPoint,
        segment_index: Option<usize>,
        package: Option<PackageId>,
    ) -> ResolvePathResult {
        ResolvePathResult {
            resolved_def,
            segment_index,
            reached_fixedpoint,
            package,
        }
    }
}

impl PackageDefs {
    /// Resolves the specified `path` from within the specified `module`. Also
    /// optionally returns which part of the path was resolved, if this is
    /// not `None` it means the path didn't resolve completely yet.
    pub(crate) fn resolve_path_in_module(
        &self,
        db: &dyn DefDatabase,
        module: PackageModuleId,
        path: &Path,
    ) -> (PerNs<(ItemDefinitionId, Visibility)>, Option<usize>) {
        let res = self.resolve_path_with_fixedpoint(db, module, path);
        (res.resolved_def, res.segment_index)
    }

    /// Resolves the specified `name` from within the specified `module`
    ///
    /// Four scopes, in order: the module's own items, the built-in
    /// primitives, the package's prelude, and the other packages by name
    /// (`std`). The prelude comes after the module so that a module can
    /// define a name the prelude also exports and get its own -- shadowing a
    /// prelude name is a local decision, and a prelude that could not be
    /// shadowed would make adding a name to it a breaking change for every
    /// module in the package. Package names come last for the same reason:
    /// a program with a module of its own called `std` keeps it.
    fn resolve_name_in_module(
        &self,
        db: &dyn DefDatabase,
        module: PackageModuleId,
        name: &Name,
    ) -> PerNs<(ItemDefinitionId, Visibility)> {
        self[module]
            .get(name)
            .or(BUILTIN_SCOPE.get(name).copied().unwrap_or_else(PerNs::none))
            .or_else(|| self.resolve_name_in_prelude(module, name))
            .or_else(|| self.resolve_package_name(db, name))
    }

    /// The root module of the package other packages call `name`, if there
    /// is one other than this package (see
    /// `codira_hir_input::PackageData::name`).
    fn resolve_package_name(
        &self,
        db: &dyn DefDatabase,
        name: &Name,
    ) -> PerNs<(ItemDefinitionId, Visibility)> {
        let packages = db.packages();
        match packages.by_name(&name.to_string()) {
            Some(package) if package != self.id => PerNs::types((
                ModuleId {
                    package,
                    local_id: db.module_tree(package).root,
                }
                .into(),
                Visibility::Public,
            )),
            _ => PerNs::none(),
        }
    }

    /// Resolves `name` against the package's prelude, if it has one.
    ///
    /// Only public items count. The prelude is in scope everywhere, so
    /// letting a private item through would make "private" mean nothing at
    /// all for the one module whose contents every other module can see.
    fn resolve_name_in_prelude(
        &self,
        module: PackageModuleId,
        name: &Name,
    ) -> PerNs<(ItemDefinitionId, Visibility)> {
        let Some(prelude) = self.prelude else {
            return PerNs::none();
        };

        // A module inside the prelude reaches its own items through ordinary
        // lookup; going through the prelude here as well would let the
        // prelude's private items leak back into it under public rules.
        if prelude == module {
            return PerNs::none();
        }

        self[prelude]
            .get(name)
            .and_then(|(item, vis)| matches!(vis, Visibility::Public).then_some((item, vis)))
    }

    /// Resolves the specified `path` from within the specified `module`. Also
    /// returns whether or not additions to the `PackageDef` would change
    /// the result or whether a fixed point has been reached. This is useful
    /// when resolving all imports.
    pub(crate) fn resolve_path_with_fixedpoint(
        &self,
        db: &dyn DefDatabase,
        original_module: PackageModuleId,
        path: &Path,
    ) -> ResolvePathResult {
        let mut segments = path.segments.iter().enumerate();
        let mut curr_per_ns: PerNs<(ItemDefinitionId, Visibility)> = match path.kind {
            PathKind::Plain => {
                let (_, segment) = match segments.next() {
                    Some((idx, segment)) => (idx, segment),
                    None => return ResolvePathResult::empty(ReachedFixedPoint::Yes),
                };
                self.resolve_name_in_module(db, original_module, segment)
            }
            PathKind::Super(lvl) => {
                let m = successors(Some(original_module), |m| self.module_tree[*m].parent)
                    .nth(lvl as usize);
                if let Some(local_id) = m {
                    PerNs::types((
                        ModuleId {
                            package: self.module_tree.package,
                            local_id,
                        }
                        .into(),
                        Visibility::Public,
                    ))
                } else {
                    return ResolvePathResult::empty(ReachedFixedPoint::Yes);
                }
            }
            PathKind::Package => PerNs::types((
                ModuleId {
                    package: self.module_tree.package,
                    local_id: self.module_tree.root,
                }
                .into(),
                Visibility::Public,
            )),
        };

        // The package the last segment was looked up in: a path can leave
        // this package through another package's name (`std.fs.fs_open`).
        let mut curr_package = self.module_tree.package;
        for (i, segment) in segments {
            let (curr, vis) = match curr_per_ns.take_types() {
                Some(r) => r,
                None => {
                    return ResolvePathResult::empty(ReachedFixedPoint::No);
                }
            };

            curr_per_ns = match curr {
                ItemDefinitionId::ModuleId(module) if module.package == self.id => {
                    curr_package = module.package;
                    self[module.local_id].get(segment)
                }
                // Another package is complete by the time this one resolves
                // its imports (packages do not import each other in a
                // cycle), so its scopes are final.
                ItemDefinitionId::ModuleId(module) => {
                    curr_package = module.package;
                    db.package_defs(module.package)[module.local_id].get(segment)
                }
                // TODO: Enum variants
                s => {
                    return ResolvePathResult::with(
                        PerNs::types((s, vis)),
                        ReachedFixedPoint::Yes,
                        Some(i),
                        Some(curr_package),
                    );
                }
            };
        }

        ResolvePathResult::with(
            curr_per_ns,
            ReachedFixedPoint::Yes,
            None,
            Some(curr_package),
        )
    }
}
