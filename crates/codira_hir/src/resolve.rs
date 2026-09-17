//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::sync::Arc;

use codira_hir_input::{ModuleId, PackageModuleId};

use crate::{
    expr::{scope::LocalScopeId, PatId},
    has_module::HasModule,
    ids::{
        DefWithBodyId, FunctionId, GenericDefId, ImplId, ItemContainerId, ItemDefinitionId, Lookup,
        StructId, TypeAliasId, TypeParamId,
    },
    item_scope::BUILTIN_SCOPE,
    name,
    package_defs::PackageDefs,
    primitive_type::PrimitiveType,
    visibility::RawVisibility,
    DefDatabase, ExprId, ExprScopes, Name, Path, PerNs, Visibility,
};

#[derive(Debug, Clone, Default)]
pub struct Resolver {
    scopes: Vec<Scope>,
}

#[derive(Debug, Clone)]
pub(crate) enum Scope {
    /// All the items and imported names of a module
    Module(ModuleItemMap),
    /// Brings `Self` in `impl` block into scope
    Impl(ImplId),
    /// Brings a declaration's `[T, U]` generic parameters into scope.
    ///
    /// Pushed between the module scope and any expression scope, so a
    /// parameter named `T` shadows a module-level type of the same name --
    /// inside `func map[T](..)`, `T` means the parameter, which is the only
    /// reading that makes the signature mean what it says.
    GenericParams(GenericDefId),
    /// Local bindings
    Expr(ExprScope),
}

#[derive(Debug, Clone)]
pub(crate) struct ModuleItemMap {
    package_defs: Arc<PackageDefs>,
    module_id: PackageModuleId,
}

#[derive(Debug, Clone)]
pub(crate) struct ExprScope {
    owner: DefWithBodyId,
    expr_scopes: Arc<ExprScopes>,
    scope_id: LocalScopeId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResolveValueResult {
    ValueNs(ValueNs, Visibility),
    Partial(TypeNs, usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValueNs {
    ImplSelf(ImplId),
    LocalBinding(PatId),
    FunctionId(FunctionId),
    StructId(StructId),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeNs {
    SelfType(ImplId),
    StructId(StructId),
    TypeAliasId(TypeAliasId),
    PrimitiveType(PrimitiveType),
    /// A generic parameter of the enclosing declaration. The `Name` is
    /// carried for display; identity is the `TypeParamId`.
    GenericParam(TypeParamId, Name),
}

/// An item definition visible from a certain scope.
pub enum ScopeDef {
    ImplSelfType(ImplId),
    PerNs(PerNs<(ItemDefinitionId, Visibility)>),
    Local(PatId),
}

impl Resolver {
    /// Adds another scope to the resolver from which it can resolve names
    pub(crate) fn push_scope(mut self, scope: Scope) -> Resolver {
        self.scopes.push(scope);
        self
    }

    /// Adds an `impl` block scope to the resolver from which it can resolve
    /// names
    fn push_impl_scope(self, impl_id: ImplId) -> Resolver {
        self.push_scope(Scope::Impl(impl_id))
    }

    /// Adds the scope of `owner`'s own generic parameters.
    ///
    /// Pushed unconditionally, even for a declaration with no parameters:
    /// the lookup inside is a short list scan that finds nothing, and
    /// making the scope conditional would mean every caller had to know
    /// whether the declaration is generic before asking.
    fn push_generic_param_scope(self, owner: GenericDefId) -> Resolver {
        self.push_scope(Scope::GenericParams(owner))
    }

    /// Adds a module scope to the resolver from which it can resolve names
    pub(crate) fn push_module_scope(
        self,
        package_defs: Arc<PackageDefs>,
        module_id: PackageModuleId,
    ) -> Resolver {
        self.push_scope(Scope::Module(ModuleItemMap {
            package_defs,
            module_id,
        }))
    }

    /// Adds an expression scope from which it can resolve names
    pub(crate) fn push_expr_scope(
        self,
        owner: DefWithBodyId,
        expr_scopes: Arc<ExprScopes>,
        scope_id: LocalScopeId,
    ) -> Resolver {
        self.push_scope(Scope::Expr(ExprScope {
            owner,
            expr_scopes,
            scope_id,
        }))
    }
}

impl Resolver {
    // TODO: This function is useful when we need to resolve paths between modules
    // /// Resolves a path
    // fn resolve_module_path(
    //     &self,
    //     db: &dyn DefDatabase,
    //     path: &Path,
    // ) -> PerNs<(ItemDefinitionId, Visibility)> {
    //     let (defs, module) = match self.module_scope() {
    //         None => return PerNs::none(),
    //         Some(it) => it,
    //     };
    //
    //     let (module_res, segment_index) = defs.resolve_path_in_module(db, module,
    // &path);
    //
    //     // If the `segment_index` contains a value it means the path didn't
    // resolve completely yet     if segment_index.is_some() {
    //         return PerNs::none();
    //     }
    //
    //     module_res
    // }

    /// Returns the `Module` scope of the resolver
    fn module_scope(&self) -> Option<(&PackageDefs, PackageModuleId)> {
        self.scopes.iter().rev().find_map(|scope| {
            if let Scope::Module(m) = scope {
                Some((&*m.package_defs, m.module_id))
            } else {
                None
            }
        })
    }

    /// Resolves the visibility of the the `RawVisibility`
    pub fn resolve_visibility(
        &self,
        db: &dyn DefDatabase,
        visibility: &RawVisibility,
    ) -> Option<Visibility> {
        self.module_scope().map(|(package_defs, module)| {
            Visibility::resolve(db, &package_defs.module_tree, module, visibility)
        })
    }

    /// Resolves the specified `path` as a value. Returns a result that can also
    /// indicate that the path was only partially resolved.
    pub fn resolve_path_as_value(
        &self,
        db: &dyn DefDatabase,
        path: &Path,
    ) -> Option<ResolveValueResult> {
        fn to_value_ns(
            per_ns: PerNs<(ItemDefinitionId, Visibility)>,
        ) -> Option<(ValueNs, Visibility)> {
            let (res, vis) = match per_ns.take_values()? {
                (ItemDefinitionId::FunctionId(id), vis) => (ValueNs::FunctionId(id), vis),
                (ItemDefinitionId::StructId(id), vis) => (ValueNs::StructId(id), vis),
                (
                    ItemDefinitionId::ModuleId(_)
                    | ItemDefinitionId::TypeAliasId(_)
                    | ItemDefinitionId::PrimitiveType(_),
                    _,
                ) => return None,
            };
            Some((res, vis))
        }

        let num_segments = path.segments.len();

        let tmp = name![self];
        let first_name = if path.is_self() {
            &tmp
        } else {
            path.segments.first()?
        };

        for scope in self.scopes.iter().rev() {
            match scope {
                // A generic parameter lives in the type namespace only:
                // `T` names a type, never a value, so value resolution
                // passes straight through it.
                Scope::GenericParams(_) => {}
                Scope::Expr(scope) if num_segments <= 1 => {
                    let entry = scope
                        .expr_scopes
                        .entries(scope.scope_id)
                        .iter()
                        .find(|entry| entry.name() == first_name);

                    if let Some(e) = entry {
                        return Some(ResolveValueResult::ValueNs(
                            ValueNs::LocalBinding(e.pat()),
                            Visibility::Public,
                        ));
                    }
                }
                Scope::Expr(_) => (),

                Scope::Impl(i) => {
                    if first_name == &name![Self] {
                        return Some(if num_segments <= 1 {
                            ResolveValueResult::ValueNs(ValueNs::ImplSelf(*i), Visibility::Public)
                        } else {
                            ResolveValueResult::Partial(TypeNs::SelfType(*i), 1)
                        });
                    }
                }

                Scope::Module(m) => {
                    let (module_def, idx) =
                        m.package_defs.resolve_path_in_module(db, m.module_id, path);
                    return match idx {
                        None => {
                            let (value, vis) = to_value_ns(module_def)?;
                            Some(ResolveValueResult::ValueNs(value, vis))
                        }
                        Some(idx) => {
                            let ty = match module_def.take_types()? {
                                (ItemDefinitionId::StructId(id), _) => TypeNs::StructId(id),
                                (ItemDefinitionId::TypeAliasId(id), _) => TypeNs::TypeAliasId(id),
                                (ItemDefinitionId::PrimitiveType(id), _) => {
                                    TypeNs::PrimitiveType(id)
                                }
                                (
                                    ItemDefinitionId::ModuleId(_) | ItemDefinitionId::FunctionId(_),
                                    _,
                                ) => return None,
                            };
                            Some(ResolveValueResult::Partial(ty, idx))
                        }
                    };
                }
            };
        }

        None
    }

    /// Resolves the specified `path` as a value. Returns either `None` or the
    /// resolved path value.
    pub fn resolve_path_as_value_fully(
        &self,
        db: &dyn DefDatabase,
        path: &Path,
    ) -> Option<(ValueNs, Visibility)> {
        match self.resolve_path_as_value(db, path)? {
            ResolveValueResult::ValueNs(val, vis) => Some((val, vis)),
            ResolveValueResult::Partial(..) => None,
        }
    }

    /// Resolves the specified `path` as a type. Returns a result that can also
    /// indicate that the path was only partially resolved.
    pub fn resolve_path_as_type(
        &self,
        db: &dyn DefDatabase,
        path: &Path,
    ) -> Option<(TypeNs, Visibility, Option<usize>)> {
        fn to_type_ns(
            per_ns: PerNs<(ItemDefinitionId, Visibility)>,
        ) -> Option<(TypeNs, Visibility)> {
            let (res, vis) = match per_ns.take_types()? {
                (ItemDefinitionId::StructId(id), vis) => (TypeNs::StructId(id), vis),
                (ItemDefinitionId::TypeAliasId(id), vis) => (TypeNs::TypeAliasId(id), vis),
                (ItemDefinitionId::PrimitiveType(id), vis) => (TypeNs::PrimitiveType(id), vis),
                (ItemDefinitionId::ModuleId(_) | ItemDefinitionId::FunctionId(_), _) => {
                    return None;
                }
            };
            Some((res, vis))
        }

        let first_name = path.first_segment()?;

        let remaining_idx = || {
            if path.segments.len() == 1 {
                None
            } else {
                Some(1)
            }
        };

        for scope in self.scopes.iter().rev() {
            match scope {
                Scope::Expr(_) => {}
                Scope::Impl(i) => {
                    if first_name == &name![Self] {
                        return Some((TypeNs::SelfType(*i), Visibility::Public, remaining_idx()));
                    }
                }
                // A generic parameter is only ever named by a single bare
                // segment: `T` is a parameter, `a.T` is a path into a
                // module and cannot be one.
                Scope::GenericParams(owner) => {
                    if path.segments.len() == 1 {
                        if let Some(param) = generic_param(db, *owner, first_name) {
                            return Some((
                                TypeNs::GenericParam(param, first_name.clone()),
                                Visibility::Public,
                                None,
                            ));
                        }
                    }
                }
                Scope::Module(m) => {
                    let (module_def, idx) =
                        m.package_defs.resolve_path_in_module(db, m.module_id, path);

                    let (res, vis) = to_type_ns(module_def)?;
                    return Some((res, vis, idx));
                }
            }
        }

        None
    }

    /// Resolves the specified `path` as a type. Returns either `None` or the
    /// resolved path type.
    pub fn resolve_path_as_type_fully(
        &self,
        db: &dyn DefDatabase,
        path: &Path,
    ) -> Option<(TypeNs, Visibility)> {
        let (res, visibility, unresolved) = self.resolve_path_as_type(db, path)?;
        if unresolved.is_some() {
            return None;
        }
        Some((res, visibility))
    }

    /// Returns the module from which this instance resolves names
    pub fn module(&self) -> Option<ModuleId> {
        let (package_defs, local_id) = self.module_scope()?;
        Some(ModuleId {
            package: package_defs.module_tree.package,
            local_id,
        })
    }

    /// If the resolver holds a scope from a body, returns that body.
    pub fn body_owner(&self) -> Option<DefWithBodyId> {
        self.scopes.iter().rev().find_map(|scope| {
            if let Scope::Expr(it) = scope {
                Some(it.owner)
            } else {
                None
            }
        })
    }

    /// Calls the `visitor` for each entry in scope.
    pub fn visit_all_names(&self, db: &dyn DefDatabase, visitor: &mut dyn FnMut(Name, ScopeDef)) {
        for scope in self.scopes.iter().rev() {
            scope.visit_names(db, visitor);
        }
    }
}

impl Scope {
    /// Calls the `visitor` for each entry in scope.
    fn visit_names(&self, _db: &dyn DefDatabase, visitor: &mut dyn FnMut(Name, ScopeDef)) {
        match self {
            Scope::Module(m) => {
                m.package_defs[m.module_id]
                    .entries()
                    .for_each(|(name, def)| visitor(name.clone(), ScopeDef::PerNs(def)));
                BUILTIN_SCOPE.iter().for_each(|(name, &def)| {
                    visitor(name.clone(), ScopeDef::PerNs(def));
                });
            }
            Scope::Impl(i) => {
                visitor(name![Self], ScopeDef::ImplSelfType(*i));
            }
            // Generic parameters are intentionally not offered here.
            // `visit_names` feeds name *completion*, which works in the
            // value namespace; a type parameter is not a value, and
            // `ScopeDef` has no way to describe one.
            Scope::GenericParams(_) => {}
            Scope::Expr(scope) => scope
                .expr_scopes
                .entries(scope.scope_id)
                .iter()
                .for_each(|entry| visitor(entry.name().clone(), ScopeDef::Local(entry.pat()))),
        }
    }
}

/// Returns a resolver applicable to the specified expression
pub fn resolver_for_expr(db: &dyn DefDatabase, owner: DefWithBodyId, expr_id: ExprId) -> Resolver {
    let scopes = db.expr_scopes(owner);
    resolver_for_scope(db, owner, scopes.scope_for(expr_id))
}

pub fn resolver_for_scope(
    db: &dyn DefDatabase,
    owner: DefWithBodyId,
    scope_id: Option<LocalScopeId>,
) -> Resolver {
    let mut r = owner.resolver(db);
    let scopes = db.expr_scopes(owner);
    let scope_chain = scopes.scope_chain(scope_id).collect::<Vec<_>>();
    r.scopes.reserve(scope_chain.len());

    for scope in scope_chain.into_iter().rev() {
        r = r.push_expr_scope(owner, Arc::clone(&scopes), scope);
    }
    r
}

pub trait HasResolver: Copy {
    /// Builds a resolver for type or value references inside this instance.
    fn resolver(self, db: &dyn DefDatabase) -> Resolver;
}

impl HasResolver for ModuleId {
    fn resolver(self, db: &dyn DefDatabase) -> Resolver {
        let defs = db.package_defs(self.package);
        Resolver::default().push_module_scope(defs, self.local_id)
    }
}

impl HasResolver for FunctionId {
    fn resolver(self, db: &dyn DefDatabase) -> Resolver {
        self.lookup(db)
            .container
            .resolver(db)
            .push_generic_param_scope(self.into())
    }
}

impl HasResolver for StructId {
    fn resolver(self, db: &dyn DefDatabase) -> Resolver {
        self.module(db)
            .resolver(db)
            .push_generic_param_scope(self.into())
    }
}

impl HasResolver for TypeAliasId {
    fn resolver(self, db: &dyn DefDatabase) -> Resolver {
        self.module(db).resolver(db)
    }
}

impl HasResolver for DefWithBodyId {
    fn resolver(self, db: &dyn DefDatabase) -> Resolver {
        match self {
            DefWithBodyId::FunctionId(f) => f.resolver(db),
        }
    }
}

impl HasResolver for ItemContainerId {
    fn resolver(self, db: &dyn DefDatabase) -> Resolver {
        match self {
            ItemContainerId::ModuleId(it) => it.resolver(db),
            ItemContainerId::ImplId(it) => it.resolver(db),
        }
    }
}

impl HasResolver for ImplId {
    fn resolver(self, db: &dyn DefDatabase) -> Resolver {
        self.lookup(db)
            .module
            .resolver(db)
            .push_impl_scope(self)
            .push_generic_param_scope(self.into())
    }
}

/// The position of `name` in `owner`'s generic parameter list, if it is one.
///
/// Read from the item tree rather than from `FunctionData`/`StructData`,
/// because this runs *during* name resolution: those queries resolve types,
/// and resolving a type is what asks this question. Going through them would
/// close the cycle.
fn generic_param(db: &dyn DefDatabase, owner: GenericDefId, name: &Name) -> Option<TypeParamId> {
    let params: Box<[crate::item_tree::GenericParamData]> = match owner {
        GenericDefId::FunctionId(id) => {
            let loc = id.lookup(db);
            let item_tree = db.item_tree(loc.id.file_id);
            item_tree[loc.id.value].generic_params.clone()
        }
        GenericDefId::StructId(id) => {
            let loc = id.lookup(db);
            let item_tree = db.item_tree(loc.id.file_id);
            item_tree[loc.id.value].generic_params.clone()
        }
        GenericDefId::ImplId(id) => return impl_generic_param(db, id, name),
    };

    params
        .iter()
        .position(|param| &param.name == name)
        .map(|index| TypeParamId {
            owner,
            index: index as u32,
        })
}

/// Resolves `name` against the binding occurrences in an `extend`'s
/// extended type.
///
/// `extend Box[T] { .. }` has no explicit parameter list -- the `[T]` is
/// syntactically a generic *argument* on the extended type. A bare name
/// there is a binding occurrence exactly when it does not already name a
/// type: `T` in `extend Box[T]` introduces one, `i32` in `extend Box[i32]`
/// does not.
///
/// Crucially, the resulting `TypeParamId` is owned by the **extended type**,
/// not by the `extend` block. `extend Box[T]` means "for every `T`, extend
/// `Box[T]`", so its `T` *is* `Box`'s first parameter -- giving the block
/// its own fresh parameter instead would make a field of type `T` and a
/// return type of `T` two unrelated types, and
/// `func get(self) -> T { self.v }` would fail to type-check with the
/// memorable message "expected `T`, found `T`".
///
/// The name check runs against the enclosing *module* resolver, which has no
/// generic scope of its own, so asking it cannot re-enter this function.
///
/// Read from the AST rather than from the item tree because `TypeRef::Path`
/// does not carry generic arguments -- they are parsed and then dropped
/// during type-ref lowering, so the item tree's `self_ty` for
/// `extend Box[T]` is indistinguishable from `extend Box`.
fn impl_generic_param(db: &dyn DefDatabase, id: ImplId, name: &Name) -> Option<TypeParamId> {
    use codira_syntax::ast::AstNode;

    use crate::code_model::src::HasSource;

    let loc = id.lookup(db);
    let source = loc.source(db);
    let self_ty = source.value.type_ref()?;
    let path_type = codira_syntax::ast::PathType::cast(self_ty.syntax().clone())?;

    let module_resolver = loc.module.resolver(db);

    // The extended type, which will own any parameters bound here.
    let extended = path_type
        .path()
        .and_then(Path::from_ast)
        .and_then(|path| module_resolver.resolve_path_as_type_fully(db, &path))
        .and_then(|(type_ns, _)| match type_ns {
            TypeNs::StructId(struct_id) => Some(GenericDefId::StructId(struct_id)),
            _ => None,
        })?;

    let mut index = 0u32;
    for arg in path_type.generic_arg_list()?.generic_args() {
        let Some(arg_path) = codira_syntax::ast::PathType::cast(arg.syntax().clone())
            .and_then(|p| p.path())
            .and_then(Path::from_ast)
        else {
            continue;
        };
        if arg_path.segments.len() != 1 {
            continue;
        }
        // Already a type? Then it is an argument, not a parameter.
        if module_resolver
            .resolve_path_as_type_fully(db, &arg_path)
            .is_some()
        {
            continue;
        }
        if arg_path.segments.first() == Some(name) {
            return Some(TypeParamId {
                owner: extended,
                index,
            });
        }
        index += 1;
    }
    None
}
