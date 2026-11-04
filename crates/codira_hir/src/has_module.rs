//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_hir_input::ModuleId;

use crate::{
    ids::{
        AssocItemId, AssocItemLoc, EffectId, EnumId, FunctionId, ImplId, ItemContainerId, Lookup,
        SessionId, StructId, TraitId, TypeAliasId, VariantId,
    },
    item_tree::ItemTreeNode,
    DefDatabase,
};

/// A trait to lookup the module associated with an item.
pub trait HasModule {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId;
}

impl HasModule for ItemContainerId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        match self {
            ItemContainerId::ModuleId(it) => *it,
            ItemContainerId::ImplId(it) => it.lookup(db).module,
            ItemContainerId::TraitId(it) => it.lookup(db).module,
        }
    }
}

impl<N: ItemTreeNode> HasModule for AssocItemLoc<N> {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        self.container.module(db)
    }
}

impl HasModule for StructId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        self.lookup(db).module
    }
}

impl HasModule for FunctionId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        self.lookup(db).container.module(db)
    }
}

impl HasModule for ImplId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        self.lookup(db).module
    }
}

impl HasModule for TypeAliasId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        self.lookup(db).module
    }
}

impl HasModule for EnumId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        self.lookup(db).module
    }
}

impl HasModule for EffectId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        self.lookup(db).module
    }
}

impl HasModule for TraitId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        self.lookup(db).module
    }
}

impl HasModule for SessionId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        self.lookup(db).module
    }
}

impl HasModule for AssocItemId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        match self {
            AssocItemId::FunctionId(it) => it.module(db),
        }
    }
}

impl HasModule for VariantId {
    fn module(&self, db: &dyn DefDatabase) -> ModuleId {
        match self {
            VariantId::StructId(it) => it.module(db),
        }
    }
}
