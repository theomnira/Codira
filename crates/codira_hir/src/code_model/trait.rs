//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - The code model of a trait declaration (`trait Name { .. }`): its
//!   methods, its supertraits, and its one type parameter `Self`.
//!
//! A trait is a set of method signatures, some with a default body. It has
//! no runtime representation: every call of a trait method is resolved to
//! one implementation at compile time, per instantiation of the generic code
//! that makes it (see `crate::traits` and `crate::mono`). Its methods are
//! ordinary [`Function`]s whose container is the trait; inside them `Self`
//! is the type parameter `TypeParamId { owner: TraitId, index: 0 }`.
use std::sync::Arc;

use codira_hir_input::FileId;
use codira_syntax::{
    ast::{self, AstNode, GenericParamsOwner},
    AstPtr, SyntaxNodePtr,
};

use super::Module;
use crate::{
    diagnostics::TraitError,
    has_module::HasModule,
    ids::{FunctionLoc, GenericDefId, Intern, ItemContainerId, Lookup, TraitId, TypeParamId},
    item_tree::{AssociatedItem, ItemTreeId},
    name,
    resolve::HasResolver,
    visibility::RawVisibility,
    DefDatabase, DiagnosticSink, Function, HasSource, HasVisibility, HirDatabase, Name, Ty, TyKind, Visibility,
};

/// A `trait` declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct Trait {
    pub(crate) id: TraitId,
}

impl From<TraitId> for Trait {
    fn from(id: TraitId) -> Self {
        Trait { id }
    }
}

/// The data of a trait declaration, read from its item tree.
#[derive(Debug, PartialEq, Eq)]
pub struct TraitData {
    pub name: Name,
    pub visibility: RawVisibility,
    /// Every method, in declaration order.
    pub items: Vec<crate::ids::FunctionId>,
}

impl TraitData {
    pub(crate) fn trait_data_query(db: &dyn DefDatabase, id: TraitId) -> Arc<TraitData> {
        let loc = id.lookup(db);
        let item_tree = db.item_tree(loc.id.file_id);
        let trait_def = &item_tree[loc.id.value];
        let container = ItemContainerId::TraitId(id);
        let items = trait_def
            .items
            .iter()
            .map(|item| match item {
                AssociatedItem::Function(f) => FunctionLoc {
                    container,
                    id: ItemTreeId::new(loc.id.file_id, *f),
                }
                .intern(db),
            })
            .collect();
        Arc::new(TraitData {
            name: trait_def.name.clone(),
            visibility: item_tree[trait_def.visibility].clone(),
            items,
        })
    }
}

impl Trait {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.module(db).into()
    }

    pub fn file_id(self, db: &dyn HirDatabase) -> FileId {
        self.id.lookup(db).id.file_id
    }

    pub fn data(self, db: &dyn DefDatabase) -> Arc<TraitData> {
        db.trait_data(self.id)
    }

    pub fn name(self, db: &dyn DefDatabase) -> Name {
        self.data(db).name.clone()
    }

    /// The trait's `Self`: the type parameter its methods are written in
    /// terms of.
    pub fn self_param(self) -> TypeParamId {
        TypeParamId {
            owner: GenericDefId::TraitId(self.id),
            index: 0,
        }
    }

    /// `Self` as a type.
    pub fn self_ty(self) -> Ty {
        TyKind::TypeParam(self.self_param(), name![Self]).intern()
    }

    /// Every method the trait declares, in declaration order.
    pub fn methods(self, db: &dyn HirDatabase) -> Vec<Function> {
        self.data(db).items.iter().map(|&f| f.into()).collect()
    }

    /// The method called `name`, if the trait declares one.
    pub fn method(self, db: &dyn HirDatabase, name: &Name) -> Option<Function> {
        self.methods(db).into_iter().find(|f| &f.name(db) == name)
    }

    /// The traits named after the `:` of the declaration that resolve,
    /// in order. An entry that does not resolve is reported by
    /// [`Trait::diagnostics`].
    pub fn direct_supertraits(self, db: &dyn HirDatabase) -> Vec<Trait> {
        self.supertrait_refs(db)
            .into_iter()
            .filter_map(|(_, resolved)| resolved)
            .collect()
    }

    /// This trait and every trait it requires, directly or through other
    /// supertraits, each once, this trait first. A cycle (reported by
    /// [`Trait::diagnostics`]) does not make this loop.
    pub fn with_supertraits(self, db: &dyn HirDatabase) -> Vec<Trait> {
        let mut all = vec![self];
        let mut next = 0;
        while next < all.len() {
            for sup in all[next].direct_supertraits(db) {
                if !all.contains(&sup) {
                    all.push(sup);
                }
            }
            next += 1;
        }
        all
    }

    /// Each entry of the supertrait list with what it resolves to.
    fn supertrait_refs(self, db: &dyn HirDatabase) -> Vec<(ast::TypeRef, Option<Trait>)> {
        let source = self.source(db).value;
        let resolver = self.id.resolver(db);
        source
            .inheritance_list()
            .into_iter()
            .flat_map(|list| list.bounds())
            .map(|type_ref| {
                let resolved = crate::traits::trait_path(&type_ref)
                    .and_then(|path| resolver.resolve_path_as_trait(db, &path))
                    .map(|(id, _)| Trait::from(id));
                (type_ref, resolved)
            })
            .collect()
    }

    /// Reports what the first version of traits does not support, an
    /// unresolved or cyclic supertrait, and everything wrong with the
    /// methods (including their bodies).
    pub fn diagnostics(self, db: &dyn HirDatabase, sink: &mut DiagnosticSink<'_>) {
        let file = self.file_id(db);
        let source = self.source(db).value;
        let name = self.name(db);
        let error = |sink: &mut DiagnosticSink<'_>, node: SyntaxNodePtr, message: String| {
            sink.push(TraitError {
                file,
                node,
                message,
            });
        };
        let whole = SyntaxNodePtr::new(source.syntax());

        if source.generic_param_list().is_some() {
            error(
                sink,
                source
                    .generic_param_list()
                    .map_or(whole.clone(), |l| SyntaxNodePtr::new(l.syntax())),
                format!(
                    "generic traits are not supported yet: `{name}` cannot take type \
                     parameters; only methods on `Self` can be declared"
                ),
            );
        }
        if let Some(clause) = source.where_clause() {
            error(
                sink,
                SyntaxNodePtr::new(clause.syntax()),
                format!("a `where` clause on a trait is not supported yet (on `{name}`)"),
            );
        }

        for (type_ref, resolved) in self.supertrait_refs(db) {
            if resolved.is_none() {
                error(
                    sink,
                    AstPtr::new(&type_ref).syntax_node_ptr(),
                    format!(
                        "`{}` is not a trait, so `{name}` cannot require it",
                        type_ref.syntax().text()
                    ),
                );
            }
        }
        let supertraits = self.with_supertraits(db);
        if supertraits[1..]
            .iter()
            .any(|sup| sup.direct_supertraits(db).contains(&self))
        {
            error(
                sink,
                whole.clone(),
                format!("trait `{name}` requires itself through its supertraits"),
            );
        }

        let mut seen: Vec<Name> = Vec::new();
        for method in self.methods(db) {
            let method_name = method.name(db);
            let method_src = method.source(db).value;
            let method_ptr = SyntaxNodePtr::new(method_src.syntax());
            if seen.contains(&method_name) {
                error(
                    sink,
                    method_ptr.clone(),
                    format!("the method `{method_name}` is declared twice in trait `{name}`"),
                );
            }
            seen.push(method_name.clone());
            if method_src.generic_param_list().is_some() {
                error(
                    sink,
                    method_ptr.clone(),
                    format!(
                        "generic trait methods are not supported yet: `{method_name}` of \
                         `{name}` cannot take type parameters"
                    ),
                );
            }
            // A call through a bound reaches the implementation, which the
            // verifier never checks against the trait's clauses; assuming
            // them at the call would be unsound.
            let contracts = method.body(db);
            let contracts = contracts.contracts();
            if !contracts.requires.is_empty() || !contracts.ensures.is_empty() {
                error(
                    sink,
                    method_ptr.clone(),
                    format!(
                        "contracts on trait methods are not supported yet: put them on each \
                         implementation of `{method_name}` instead"
                    ),
                );
            }
            method.diagnostics(db, sink);
        }
    }
}

impl HasVisibility for Trait {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        self.data(db).visibility.resolve(db, &self.id.resolver(db))
    }
}

impl crate::HasSource for Trait {
    type Ast = ast::TraitDef;
    fn source(&self, db: &dyn DefDatabase) -> crate::InFile<Self::Ast> {
        self.id.lookup(db).source(db)
    }
}

impl Function {
    /// The trait this function is a method of, if it is declared in one.
    pub fn trait_container(self, db: &dyn DefDatabase) -> Option<Trait> {
        match self.id.lookup(db).container {
            ItemContainerId::TraitId(id) => Some(id.into()),
            _ => None,
        }
    }
}
