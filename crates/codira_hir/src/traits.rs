//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - Traits with static dispatch: which traits a generic parameter is bound
//!   by, which implementations exist, whether a type implements a trait, and
//!   which function a trait-method call reaches once its `Self` is known.
//!
//! # The model
//!
//! A trait (`trait Show { func show(self) -> i64; }`) is a set of method
//! signatures, some with a default body. A type implements it in an
//! `extend` block that names it, `extend Point: Show { func show(self) ..
//! }`, which must provide every method without a default, with the
//! signature the trait declares (`Self` read as the extended type).
//!
//! A generic parameter can be bounded, `func f[T: Show](x: T)` or `where T:
//! Show`; the bounds of an `extend Box[T: Show]` block hold in each of its
//! methods. Inside the body, `x.show()` resolves to the trait's declaration
//! of `show`, with `Self` instantiated at `T` like any generic callee
//! (`ty::infer`). At each call of `f`, the type `T` stands for must
//! implement `Show` (`implements`), or the call is an error naming the
//! type, the trait and the parameter.
//!
//! There is no dynamic dispatch. A generic function is compiled once per
//! instantiation (`crate::mono`); reading its body at an instance makes
//! `T` concrete, and [`resolve_method`] then replaces the trait's
//! declaration with the implementation for that type -- or, for a method
//! the implementation does not provide, with the trait's default body,
//! itself instantiated at `Self = that type`. Code generation only ever
//! sees ordinary direct calls.
//!
//! A default method is a function whose container is the trait. Its
//! generic environment is `[Self]`, so it has no code of its own and is
//! instantiated per type like any generic function.
//!
//! # Limits of this version
//!
//! Each reported where it is written: generic traits (`trait
//! Iterable[E]`), generic trait methods, contracts on trait methods, bounds
//! on a struct's own parameters, and a `where` clause on a trait. There are
//! no associated types and no trait objects; operators (`a + b`, `a < b`)
//! on a type parameter are not resolved through traits.
use std::sync::Arc;

use codira_hir_input::{FileId, PackageId};
use codira_syntax::{
    ast::{self, AstNode, GenericParamsOwner, NameOwner},
    AstPtr, SyntaxNodePtr,
};
use rustc_hash::FxHashMap;

use crate::{
    code_model::{AssocItem, Impl},
    diagnostics::TraitError,
    ids::{FunctionId, GenericDefId, ImplId, ItemContainerId, Lookup, TypeParamId},
    method_resolution::AssociationMode,
    mono,
    name::AsName,
    resolve::{HasResolver, Resolver},
    DiagnosticSink, Function, HasSource, HirDatabase, HirDisplay, Name, Package, Path,
    Substitution, Trait, Ty, TyKind,
};

/// How deeply `implements` may recurse through the bounds of generic
/// implementations (`Box[T]: Show` when `T: Show`) before giving up.
const MAX_IMPLEMENTS_DEPTH: usize = 32;

/// The path a bound, supertrait or implemented trait is written as, or
/// `None` when the type reference is not a path (`[T]`, `(A, B)`).
pub(crate) fn trait_path(type_ref: &ast::TypeRef) -> Option<Path> {
    match type_ref.kind() {
        ast::TypeRefKind::PathType(path) => path.path().and_then(Path::from_ast),
        _ => None,
    }
}

/// Whether the type reference carries generic arguments, `Iterable[E]`.
fn has_generic_args(type_ref: &ast::TypeRef) -> bool {
    match type_ref.kind() {
        ast::TypeRefKind::PathType(path) => path.generic_arg_list().is_some(),
        _ => false,
    }
}

/// What a bound written as `type_ref` names.
enum BoundRef {
    /// A trait.
    Trait(Trait),
    /// A type: the `usize` of a const parameter `N: usize`, which is not a
    /// bound and is left to compile-time evaluation.
    Type,
    /// Neither; the message says why.
    Error(String),
}

fn resolve_bound(db: &dyn HirDatabase, resolver: &Resolver, type_ref: &ast::TypeRef) -> BoundRef {
    let text = type_ref.syntax().text().to_string();
    let Some(path) = trait_path(type_ref) else {
        return BoundRef::Error(format!("`{text}` is not a trait: a bound names a trait"));
    };
    if let Some((id, _)) = resolver.resolve_path_as_trait(db, &path) {
        if has_generic_args(type_ref) {
            return BoundRef::Error(format!(
                "generic traits are not supported yet: `{text}` takes no arguments"
            ));
        }
        return BoundRef::Trait(id.into());
    }
    if resolver.resolve_path_as_type_fully(db, &path).is_some() {
        return BoundRef::Type;
    }
    BoundRef::Error(format!("undefined trait `{text}`"))
}

/// One bound: `param` must implement `trait_`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bound {
    pub param: TypeParamId,
    pub trait_: Trait,
}

/// The bounds that hold in a declaration: of a function, everything its
/// body may assume about its generic parameters and those of its `extend`
/// block; of an `extend` block, the bounds written in its type.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GenericBounds {
    /// The bounds as written (supertraits not expanded), in source order.
    pub bounds: Vec<Bound>,
    diagnostics: Vec<(FileId, SyntaxNodePtr, String)>,
}

impl GenericBounds {
    /// Every trait `param` implements by its bounds, each bound's
    /// supertraits included.
    pub fn traits_of(&self, db: &dyn HirDatabase, param: TypeParamId) -> Vec<Trait> {
        let mut traits = Vec::new();
        for bound in self.bounds.iter().filter(|b| b.param == param) {
            for t in bound.trait_.with_supertraits(db) {
                if !traits.contains(&t) {
                    traits.push(t);
                }
            }
        }
        traits
    }

    /// The traits written as bounds of `param`, without supertraits.
    pub fn written_bounds_of(&self, param: TypeParamId) -> Vec<Trait> {
        self.bounds
            .iter()
            .filter(|b| b.param == param)
            .map(|b| b.trait_)
            .collect()
    }

    pub(crate) fn add_diagnostics(&self, sink: &mut DiagnosticSink<'_>) {
        for (file, node, message) in &self.diagnostics {
            sink.push(TraitError {
                file: *file,
                node: node.clone(),
                message: message.clone(),
            });
        }
    }

    fn push_bounds(
        &mut self,
        db: &dyn HirDatabase,
        resolver: &Resolver,
        file: FileId,
        param: TypeParamId,
        bounds: impl Iterator<Item = ast::TypeRef>,
    ) {
        for type_ref in bounds {
            match resolve_bound(db, resolver, &type_ref) {
                BoundRef::Trait(trait_) => self.bounds.push(Bound { param, trait_ }),
                BoundRef::Type => {}
                BoundRef::Error(message) => {
                    self.diagnostics.push((
                        file,
                        AstPtr::new(&type_ref).syntax_node_ptr(),
                        message,
                    ));
                }
            }
        }
    }
}

/// The bounds that hold in the body of `function`: its own (`[T: A + B]`,
/// `where T: A`), those of the `extend` block it is in, and for a method of
/// a trait, `Self: ThatTrait`.
pub(crate) fn function_bounds_query(db: &dyn HirDatabase, id: FunctionId) -> Arc<GenericBounds> {
    let function = Function::from(id);
    let mut result = GenericBounds::default();
    match id.lookup(db).container {
        ItemContainerId::ImplId(impl_id) => {
            result
                .bounds
                .extend(db.impl_bounds(impl_id).bounds.iter().copied());
        }
        ItemContainerId::TraitId(trait_id) => {
            let trait_ = Trait::from(trait_id);
            result.bounds.push(Bound {
                param: trait_.self_param(),
                trait_,
            });
        }
        ItemContainerId::ModuleId(_) => {}
    }

    let source = function.source(db);
    let file = source.file_id;
    let resolver = id.resolver(db);
    let params: Vec<ast::GenericParam> = source
        .value
        .generic_param_list()
        .map(|list| list.generic_params().collect())
        .unwrap_or_default();
    for (index, param) in params.iter().enumerate() {
        let param_id = TypeParamId {
            owner: GenericDefId::FunctionId(id),
            index: index as u32,
        };
        result.push_bounds(db, &resolver, file, param_id, param.bounds());
    }

    if let Some(clause) = source.value.where_clause() {
        for pred in clause.predicates() {
            let subject = pred.subject();
            let own = subject
                .as_ref()
                .and_then(trait_path)
                .and_then(|path| path.as_ident().cloned())
                .and_then(|name| {
                    params
                        .iter()
                        .position(|p| p.name().map(|n| n.as_name()).as_ref() == Some(&name))
                });
            match own {
                Some(index) => {
                    let param_id = TypeParamId {
                        owner: GenericDefId::FunctionId(id),
                        index: index as u32,
                    };
                    result.push_bounds(db, &resolver, file, param_id, pred.bounds());
                }
                None => result.diagnostics.push((
                    file,
                    SyntaxNodePtr::new(pred.syntax()),
                    format!(
                        "a `where` clause can only bound a generic parameter of `{}` itself; \
                         `{}` is not one",
                        function.name(db),
                        subject.map_or_else(String::new, |s| s.syntax().text().to_string())
                    ),
                )),
            }
        }
    }
    Arc::new(result)
}

/// The bounds written in an `extend` block's type: `T: Show` in `extend
/// Box[T: Show]`. The parameter is the extended type's own (see
/// `resolve::impl_generic_param`).
pub(crate) fn impl_bounds_query(db: &dyn HirDatabase, id: ImplId) -> Arc<GenericBounds> {
    let mut result = GenericBounds::default();
    let impl_ = Impl::from(id);
    let source = id.lookup(db).source(db);
    let file = source.file_id;
    let Some(ast::TypeRefKind::PathType(path_type)) = source.value.type_ref().map(|t| t.kind())
    else {
        return Arc::new(result);
    };
    let Some(list) = path_type.generic_arg_list() else {
        return Arc::new(result);
    };
    let resolver = id.resolver(db);
    for (arg, bounds) in list.args_with_bounds() {
        if bounds.is_empty() {
            continue;
        }
        let param = trait_path(&arg)
            .and_then(|path| path.as_ident().cloned())
            .and_then(|name| crate::resolve::impl_generic_param(db, id, &name));
        match param {
            Some(param) => result.push_bounds(db, &resolver, file, param, bounds.into_iter()),
            None => result.diagnostics.push((
                file,
                AstPtr::new(&arg).syntax_node_ptr(),
                format!(
                    "only a parameter of `{}` can be bounded here, and `{}` is a type",
                    impl_.self_ty(db).display(db),
                    arg.syntax().text()
                ),
            )),
        }
    }
    Arc::new(result)
}

/// One implementation of a trait: `extend Point: Show`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraitImpl {
    pub impl_id: ImplId,
    pub trait_: Trait,
    /// The extended type, in terms of the block's parameters (`Box[T]`).
    pub self_ty: Ty,
}

/// Every implementation of a trait in one package.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TraitImpls {
    pub impls: Vec<TraitImpl>,
}

pub(crate) fn trait_impls_in_package_query(
    db: &dyn HirDatabase,
    package: PackageId,
) -> Arc<TraitImpls> {
    let mut impls = Vec::new();
    let package_defs = db.package_defs(package);
    for (_, scope) in package_defs.modules.iter() {
        for impl_id in scope.impls() {
            let traits: Vec<Trait> = impl_traits(db, impl_id)
                .into_iter()
                .filter_map(|(_, t)| t.ok())
                .collect();
            if traits.is_empty() {
                continue;
            }
            let self_ty = db.type_for_impl_self(impl_id);
            if self_ty.is_unknown() {
                continue;
            }
            for trait_ in traits {
                impls.push(TraitImpl {
                    impl_id,
                    trait_,
                    self_ty: self_ty.clone(),
                });
            }
        }
    }
    Arc::new(TraitImpls { impls })
}

/// The traits an `extend` block names after its `:`, each with what it
/// resolves to, or why it does not.
pub(crate) fn impl_traits(
    db: &dyn HirDatabase,
    id: ImplId,
) -> Vec<(ast::TypeRef, Result<Trait, String>)> {
    let source = id.lookup(db).source(db).value;
    let Some(list) = source.inheritance_list() else {
        return Vec::new();
    };
    let resolver = id.resolver(db);
    list.bounds()
        .map(|type_ref| {
            let resolved = match resolve_bound(db, &resolver, &type_ref) {
                BoundRef::Trait(t) => Ok(t),
                BoundRef::Type => Err(format!(
                    "`{}` is a type, not a trait: an `extend` block can only implement traits",
                    type_ref.syntax().text()
                )),
                BoundRef::Error(message) => Err(message),
            };
            (type_ref, resolved)
        })
        .collect()
}

/// Whether the `extend` block names any trait (resolved or not).
pub(crate) fn is_trait_impl(db: &dyn HirDatabase, id: ImplId) -> bool {
    id.lookup(db)
        .source(db)
        .value
        .inheritance_list()
        .is_some_and(|list| list.bounds().next().is_some())
}

fn all_trait_impls(db: &dyn HirDatabase) -> Vec<Arc<TraitImpls>> {
    Package::all(db)
        .into_iter()
        .map(|p| db.trait_impls_in_package(p.id))
        .collect()
}

/// Matches `pattern` (a type in terms of an `extend` block's parameters)
/// against `ty`, recording what each parameter stands for. `Box[T]` matches
/// `Box[i32]` with `T = i32`.
pub(crate) fn match_ty(pattern: &Ty, ty: &Ty, map: &mut FxHashMap<TypeParamId, Ty>) -> bool {
    match (pattern.interned(), ty.interned()) {
        (TyKind::TypeParam(p, _), _) => {
            if let Some(bound) = map.get(p) {
                return bound == ty;
            }
            map.insert(*p, ty.clone());
            true
        }
        (TyKind::Struct(a, xs), TyKind::Struct(b, ys)) if a == b => match_all(xs, ys, map),
        (TyKind::Enum(a, xs), TyKind::Enum(b, ys)) if a == b => match_all(xs, ys, map),
        (TyKind::Tuple(n, xs), TyKind::Tuple(m, ys)) if n == m => match_all(xs, ys, map),
        (TyKind::Function(xs), TyKind::Function(ys)) => match_all(xs, ys, map),
        (TyKind::Array(x), TyKind::Array(y))
        | (TyKind::Channel(x), TyKind::Channel(y))
        | (TyKind::Optional(x), TyKind::Optional(y)) => match_ty(x, y, map),
        _ => pattern == ty,
    }
}

fn match_all(xs: &Substitution, ys: &Substitution, map: &mut FxHashMap<TypeParamId, Ty>) -> bool {
    xs.len() == ys.len() && xs.iter().zip(ys.iter()).all(|(x, y)| match_ty(x, y, map))
}

/// Whether two implementations' types can describe the same type: each
/// block's parameters match anything.
fn overlaps(a: &Ty, b: &Ty) -> bool {
    let mut map = FxHashMap::default();
    if match_ty(a, b, &mut map) {
        return true;
    }
    let mut map = FxHashMap::default();
    match_ty(b, a, &mut map)
}

/// The implementation of `trait_` for `ty`, with what its block's
/// parameters stand for, if there is one whose bounds `ty` meets. `env`
/// says what the type parameters in `ty` (if any) implement.
pub fn find_impl(
    db: &dyn HirDatabase,
    trait_: Trait,
    ty: &Ty,
    env: &GenericBounds,
) -> Option<(ImplId, FxHashMap<TypeParamId, Ty>)> {
    find_impl_at(db, trait_, ty, env, 0)
}

fn find_impl_at(
    db: &dyn HirDatabase,
    trait_: Trait,
    ty: &Ty,
    env: &GenericBounds,
    depth: usize,
) -> Option<(ImplId, FxHashMap<TypeParamId, Ty>)> {
    if depth > MAX_IMPLEMENTS_DEPTH {
        return None;
    }
    for impls in all_trait_impls(db) {
        for candidate in impls.impls.iter().filter(|i| i.trait_ == trait_) {
            let mut map = FxHashMap::default();
            if !match_ty(&candidate.self_ty, ty, &mut map) {
                continue;
            }
            let bounds = db.impl_bounds(candidate.impl_id);
            let meets = bounds.bounds.iter().all(|bound| {
                map.get(&bound.param)
                    .is_none_or(|arg| implements_at(db, arg, bound.trait_, env, depth + 1))
            });
            if meets {
                return Some((candidate.impl_id, map));
            }
        }
    }
    None
}

/// Whether `ty` implements `trait_`: a type parameter by the bounds in
/// `env` (supertraits included), any other type by an implementation.
///
/// A type inference could not determine is taken to implement everything:
/// that failure is reported on its own, and a second error about traits
/// would only repeat it.
pub fn implements(db: &dyn HirDatabase, ty: &Ty, trait_: Trait, env: &GenericBounds) -> bool {
    implements_at(db, ty, trait_, env, 0)
}

fn implements_at(
    db: &dyn HirDatabase,
    ty: &Ty,
    trait_: Trait,
    env: &GenericBounds,
    depth: usize,
) -> bool {
    match ty.interned() {
        TyKind::Unknown | TyKind::InferenceVar(_) | TyKind::Never => true,
        TyKind::TypeParam(param, _) => env.traits_of(db, *param).contains(&trait_),
        _ => find_impl_at(db, trait_, ty, env, depth).is_some(),
    }
}

/// The function a call of the trait method `method` reaches when `Self` is
/// `self_ty`, with the arguments of that function's generic environment:
/// the implementation's method (`Box[i32]`'s, at `[i32]`), or the trait's
/// default body at `[self_ty]`. `None` when `method` is not a trait
/// method, `self_ty` still contains a type parameter, or nothing
/// implements it -- which type checking rules out for a program that
/// passed it.
pub fn resolve_method(
    db: &dyn HirDatabase,
    method: Function,
    self_ty: &Ty,
) -> Option<(Function, Substitution)> {
    let trait_ = method.trait_container(db)?;
    if mono::contains_type_param(self_ty) {
        return None;
    }
    let name = method.name(db);
    if let Some((impl_id, map)) = find_impl(db, trait_, self_ty, &GenericBounds::default()) {
        let implementation = Impl::from(impl_id).items(db).into_iter().find_map(|item| {
            let AssocItem::Function(f) = item;
            (f.name(db) == name).then_some(f)
        });
        if let Some(f) = implementation {
            let args = f
                .generic_environment(db)
                .iter()
                .map(|p| {
                    map.get(p)
                        .cloned()
                        .unwrap_or_else(|| TyKind::Unknown.intern())
                })
                .collect();
            return Some((f, args));
        }
    }
    method
        .data(db)
        .has_body()
        .then(|| (method, Substitution::single(self_ty.clone())))
}

/// The default methods named `name` that traits implemented by the
/// concrete type `ty` provide and the implementations do not override:
/// what `p.ne(q)` reaches when `Point` implements a trait with a default
/// `ne`. More than one is an ambiguity the caller reports.
pub(crate) fn default_methods(
    db: &dyn HirDatabase,
    ty: &Ty,
    name: &Name,
    mode: AssociationMode,
) -> Vec<Function> {
    let mut found = Vec::new();
    if mono::contains_type_param(ty) {
        return found;
    }
    for impls in all_trait_impls(db) {
        for candidate in &impls.impls {
            let mut map = FxHashMap::default();
            if !match_ty(&candidate.self_ty, ty, &mut map) {
                continue;
            }
            if let Some(method) = candidate.trait_.method(db, name) {
                let data = method.data(db);
                let fits = data.has_self_param() == (mode == AssociationMode::WithSelf);
                if fits && data.has_body() && !found.contains(&method) {
                    found.push(method);
                }
            }
        }
    }
    found
}

/// What a method call on a value (or a type) whose type is the parameter
/// `param` resolves to through the bounds in `env`.
pub(crate) enum BoundMethod {
    /// Exactly one bound trait declares it.
    Found(Function),
    /// None does; the bounds of `param` as written, for the message.
    NotFound(Vec<Trait>),
    /// Several do.
    Ambiguous(Vec<Trait>),
}

pub(crate) fn bound_method(
    db: &dyn HirDatabase,
    env: &GenericBounds,
    param: TypeParamId,
    name: &Name,
    mode: AssociationMode,
) -> BoundMethod {
    let mut found: Vec<(Trait, Function)> = Vec::new();
    for trait_ in env.traits_of(db, param) {
        if let Some(method) = trait_.method(db, name) {
            if method.data(db).has_self_param() == (mode == AssociationMode::WithSelf) {
                found.push((trait_, method));
            }
        }
    }
    match found.as_slice() {
        [] => BoundMethod::NotFound(env.written_bounds_of(param)),
        [(_, method)] => BoundMethod::Found(*method),
        _ => BoundMethod::Ambiguous(found.iter().map(|(t, _)| *t).collect()),
    }
}

/// `A + B` for a list of traits, for messages.
pub(crate) fn trait_list(db: &dyn HirDatabase, traits: &[Trait]) -> String {
    traits
        .iter()
        .map(|t| format!("`{}`", t.name(db)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reports everything wrong with an `extend` block as an implementation of
/// the traits it names, and with the bounds in its type.
pub(crate) fn impl_diagnostics(db: &dyn HirDatabase, id: ImplId, sink: &mut DiagnosticSink<'_>) {
    db.impl_bounds(id).add_diagnostics(sink);

    let traits = impl_traits(db, id);
    if traits.is_empty() {
        return;
    }
    let impl_ = Impl::from(id);
    let source = id.lookup(db).source(db);
    let file = source.file_id;
    let error = |sink: &mut DiagnosticSink<'_>, node: SyntaxNodePtr, message: String| {
        sink.push(TraitError {
            file,
            node,
            message,
        });
    };
    let self_ty = db.type_for_impl_self(id);
    if self_ty.is_unknown() {
        return;
    }
    let self_name = self_ty.display(db).to_string();
    let env = db.impl_bounds(id);
    let methods: Vec<Function> = impl_
        .items(db)
        .into_iter()
        .map(|item| {
            let AssocItem::Function(f) = item;
            f
        })
        .collect();
    let resolved: Vec<Trait> = traits.iter().filter_map(|(_, t)| t.clone().ok()).collect();

    for (type_ref, trait_) in &traits {
        let node = AstPtr::new(type_ref).syntax_node_ptr();
        let trait_ = match trait_ {
            Ok(t) => *t,
            Err(message) => {
                error(sink, node, message.clone());
                continue;
            }
        };
        let trait_name = trait_.name(db);

        // A trait can be implemented for a primitive type only by the
        // package that declares the trait: anything else would let two
        // packages give `i64` two implementations. (A struct or enum must be
        // extended in its own package, which `method_resolution` checks.)
        if is_primitive(&self_ty) && trait_.module(db).package() != impl_.module(db).package() {
            error(
                sink,
                node.clone(),
                format!(
                    "`{trait_name}` can be implemented for `{self_name}` only in the package that \
                     declares `{trait_name}`"
                ),
            );
        }

        for method in trait_.methods(db) {
            let name = method.name(db);
            let provided = methods.iter().any(|f| f.name(db) == name);
            if !provided && !method.data(db).has_body() {
                error(
                    sink,
                    node.clone(),
                    format!(
                        "the implementation of `{trait_name}` for `{self_name}` is missing the \
                         method `{name}`"
                    ),
                );
            }
        }

        for sup in trait_.direct_supertraits(db) {
            if !implements(db, &self_ty, sup, &env) {
                error(
                    sink,
                    node.clone(),
                    format!(
                        "`{trait_name}` requires `{}`, which `{self_name}` does not implement",
                        sup.name(db)
                    ),
                );
            }
        }

        for other in all_trait_impls(db)
            .iter()
            .flat_map(|impls| impls.impls.clone())
            .filter(|other| other.trait_ == trait_ && other.impl_id < id)
        {
            if overlaps(&other.self_ty, &self_ty) {
                error(
                    sink,
                    node.clone(),
                    format!(
                        "`{trait_name}` is already implemented for `{}`; a type can implement a \
                         trait only once",
                        other.self_ty.display(db)
                    ),
                );
                break;
            }
        }
    }

    for f in &methods {
        let name = f.name(db);
        let node = SyntaxNodePtr::new(f.source(db).value.syntax());
        let declared: Vec<(Trait, Function)> = resolved
            .iter()
            .filter_map(|t| t.method(db, &name).map(|m| (*t, m)))
            .collect();
        let Some(&(trait_, declaration)) = declared.first() else {
            if !resolved.is_empty() {
                error(
                    sink,
                    node,
                    format!(
                        "`{name}` is not a method of {}; an `extend` block that implements a \
                         trait holds only that trait's methods",
                        trait_list(db, &resolved)
                    ),
                );
            }
            continue;
        };
        if f.data(db).generic_params().is_empty() {
            if let Some(message) =
                signature_mismatch(db, trait_, declaration, *f, &self_ty, &self_name)
            {
                error(sink, node, message);
            }
        }
    }
}

/// Why `implementation` does not match `declaration` of `trait_` with
/// `Self` read as `self_ty`, if it does not.
fn signature_mismatch(
    db: &dyn HirDatabase,
    trait_: Trait,
    declaration: Function,
    implementation: Function,
    self_ty: &Ty,
    self_name: &str,
) -> Option<String> {
    let name = implementation.name(db);
    let trait_name = trait_.name(db);
    let expected_self = declaration.data(db).has_self_param();
    let found_self = implementation.data(db).has_self_param();
    if expected_self != found_self {
        return Some(if expected_self {
            format!("`{name}` of trait `{trait_name}` takes `self`, and this one does not")
        } else {
            format!("`{name}` of trait `{trait_name}` does not take `self`, and this one does")
        });
    }
    let environment = [trait_.self_param()];
    let args = Substitution::single(self_ty.clone());
    let expected = db.callable_sig(declaration.into());
    let expected_params: Vec<Ty> = expected
        .params()
        .iter()
        .map(|ty| mono::substitute(ty, &environment, &args))
        .collect();
    let expected_ret = mono::substitute(expected.ret(), &environment, &args);
    let found = db.callable_sig(implementation.into());
    if expected_params.as_slice() == found.params() && &expected_ret == found.ret() {
        return None;
    }
    let show = |params: &[Ty], ret: &Ty| {
        let mut list: Vec<String> = Vec::new();
        if expected_self {
            list.push("self".to_owned());
        }
        list.extend(params.iter().map(|ty| ty.display(db).to_string()));
        format!("func({}) -> {}", list.join(", "), ret.display(db))
    };
    Some(format!(
        "`{name}` does not match its declaration in trait `{trait_name}` (with `Self` = \
         `{self_name}`): expected `{}`, found `{}`",
        show(&expected_params, &expected_ret),
        show(found.params(), found.ret()),
    ))
}

fn is_primitive(ty: &Ty) -> bool {
    matches!(
        ty.interned(),
        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Str
    )
}

/// Reports a trait named on a struct's declaration (`struct P: Show`),
/// which does not implement it, and bounds on a struct's own parameters
/// (`struct Box[T: Show]`), which this version does not check where the
/// struct is used; the bound belongs on the `extend` block whose methods
/// need it.
pub(crate) fn struct_bound_diagnostics(
    db: &dyn HirDatabase,
    strukt: crate::Struct,
    sink: &mut DiagnosticSink<'_>,
) {
    let source = strukt.source(db);
    let resolver = strukt.id.resolver(db);

    // `struct Point: Show` reads as if it implemented `Show`, and nothing
    // would hold it to that: the implementation is an `extend` block.
    for entry in source
        .value
        .inheritance_list()
        .into_iter()
        .flat_map(|list| list.bounds())
    {
        if let BoundRef::Trait(trait_) = resolve_bound(db, &resolver, &entry) {
            let name = trait_.name(db);
            sink.push(TraitError {
                file: source.file_id,
                node: AstPtr::new(&entry).syntax_node_ptr(),
                message: format!(
                    "a trait is implemented in an `extend` block, not named on the declaration: \
                     write `extend {}: {name} {{ .. }}`",
                    strukt.name(db)
                ),
            });
        }
    }

    let Some(list) = source.value.generic_param_list() else {
        return;
    };
    for param in list.generic_params() {
        // A const parameter's type (`N: usize`) is not a bound.
        for bound in param.bounds() {
            if matches!(resolve_bound(db, &resolver, &bound), BoundRef::Type) {
                continue;
            }
            sink.push(TraitError {
                file: source.file_id,
                node: AstPtr::new(&bound).syntax_node_ptr(),
                message: format!(
                    "bounds on the parameters of a struct are not supported yet: bound `{}` \
                     on the `extend` block that needs it instead (`extend {}[{}: {}]`)",
                    param
                        .name()
                        .map_or_else(String::new, |n| n.text().to_string()),
                    strukt.name(db),
                    param
                        .name()
                        .map_or_else(String::new, |n| n.text().to_string()),
                    bound.syntax().text()
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests;
