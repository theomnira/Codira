//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{ops::Index, sync::Arc};

use codira_hir_input::FileId;
use rustc_hash::FxHashMap;

use crate::{CodeGenDatabase, ModuleGroup};

/// A `ModuleGroupId` refers to a single [`ModuleGroup`] in a
/// [`ModulePartition`]
#[derive(Default, PartialEq, Eq, Clone, Debug, Hash, PartialOrd, Ord, Copy)]
pub struct ModuleGroupId(usize);

/// A `ModulePartition` defines how modules are grouped together.
#[derive(Default, PartialEq, Eq, Clone, Debug)]
pub struct ModulePartition {
    groups: Vec<ModuleGroup>,
    module_to_group: FxHashMap<codira_hir::Module, ModuleGroupId>,
    file_to_group: FxHashMap<FileId, ModuleGroupId>,
}

impl ModulePartition {
    /// Adds a new group of modules to the partition. This function panics if a
    /// module is added twice in different groups.
    pub fn add_group(
        &mut self,
        db: &dyn codira_hir::HirDatabase,
        group: ModuleGroup,
    ) -> ModuleGroupId {
        let id = ModuleGroupId(self.groups.len());
        for module in group.iter() {
            assert!(
                self.module_to_group.insert(module, id).is_none(),
                "cannot add a module to multiple groups"
            );
            if let Some(file_id) = module.file_id(db) {
                assert!(
                    self.file_to_group.insert(file_id, id).is_none(),
                    "cannot add a file to multiple groups"
                );
            }
        }

        self.groups.push(group);
        id
    }

    /// Returns the group to which the specified module belongs.
    pub fn group_for_module(&self, module: codira_hir::Module) -> Option<ModuleGroupId> {
        self.module_to_group.get(&module).copied()
    }

    /// Returns the group to which the specified module belongs.
    pub fn group_for_file(&self, file: FileId) -> Option<ModuleGroupId> {
        self.file_to_group.get(&file).copied()
    }

    /// Returns an iterator over all the groups
    pub fn iter(&self) -> impl Iterator<Item = (ModuleGroupId, &ModuleGroup)> + '_ {
        self.groups
            .iter()
            .enumerate()
            .map(|(idx, group)| (ModuleGroupId(idx), group))
    }
}

impl Index<ModuleGroupId> for ModulePartition {
    type Output = ModuleGroup;

    fn index(&self, index: ModuleGroupId) -> &Self::Output {
        &self.groups[index.0]
    }
}

/// Builds a module partition from the contents of the database
pub(crate) fn build_partition(db: &dyn CodeGenDatabase) -> Arc<ModulePartition> {
    let mut partition = ModulePartition::default();
    for module in codira_hir::Package::all(db)
        .into_iter()
        .flat_map(|package| package.modules(db))
    {
        let name = if module.name(db).is_some() {
            module.full_name(db)
        } else {
            String::from("mod")
        };

        partition.add_group(db, ModuleGroup::new(db, name, vec![module]));
    }
    Arc::new(partition)
}
