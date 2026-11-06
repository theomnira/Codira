//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{convert::identity, ops::Index, sync::Arc};

use codira_hir_input::ModuleId;
use la_arena::ArenaMap;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    code_model::{Enum, EnumVariant, Struct, StructKind, VariantKind},
    diagnostics::DiagnosticSink,
    expr::{Body, Expr, ExprId, Literal, Pat, PatId, RecordLitField, Statement, UnaryOp},
    name_resolution::Namespace,
    resolve::{Resolver, TypeNs, ValueNs},
    ty::{
        cast::{check_cast, CastCheck},
        infer::{diagnostics::InferenceDiagnostic, type_variable::TypeVariableTable},
        lower::LowerDiagnostic,
        op, Substitution, Ty, TyKind, TypableDef, TypeWalk,
    },
    type_ref::LocalTypeRefId,
    BinaryOp, BuiltinFn, CallableDef, Function, HirDatabase, HirDisplay, Name, Path, StringOp,
    Trait,
};

mod array_methods;
mod closures;
mod effects;
mod exhaustive;
mod format;
mod pipelines;
mod place_expr;
mod propagate;
pub use propagate::{error_conversions, main_result, result_variants, MainResult};
pub use tasks::{group_shape, GroupShape};
mod strings;
mod tasks;
mod type_variable;
mod unify;

use crate::{
    expr::{LiteralFloat, LiteralFloatKind, LiteralInt, LiteralIntKind},
    has_module::HasModule,
    ids::{DefWithBodyId, FunctionId, GenericDefId, TypeParamId},
    method_resolution::{lookup_method, AssociationMode},
    resolve::{resolver_for_expr, HasResolver, ResolveValueResult},
    ty::primitives::{FloatTy, IntTy},
};

mod coerce;

pub use closures::ClosureInfo;

/// A list of interned types
#[derive(Clone, PartialEq, Eq, Debug)]
struct InternedStandardTypes {
    unknown: Ty,
}

impl Default for InternedStandardTypes {
    fn default() -> Self {
        InternedStandardTypes {
            unknown: TyKind::Unknown.intern(),
        }
    }
}

/// The result of type inference: A mapping from expressions and patterns to
/// types.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InferenceResult {
    pub(crate) type_of_expr: ArenaMap<ExprId, Ty>,
    pub(crate) type_of_pat: ArenaMap<PatId, Ty>,
    pub(crate) diagnostics: Vec<diagnostics::InferenceDiagnostic>,

    /// For each method call expression, records the function it resolves to.
    pub(crate) method_resolutions: FxHashMap<ExprId, FunctionId>,

    /// For each call expression whose callee is a compiler builtin
    /// (`print`, `println`), records which one. Such a call has no
    /// `CallableDef`; its callee expression stays untyped.
    pub(crate) builtin_calls: FxHashMap<ExprId, BuiltinFn>,

    /// What every `perform` and handler arm resolved to. See
    /// [`effects::EffectResolutions`].
    pub(crate) effects: effects::EffectResolutions,

    /// Every closure literal, with its signature and captures.
    pub(crate) closures: FxHashMap<ExprId, ClosureInfo>,

    /// Path expressions naming a function that are used as a function
    /// value rather than called.
    pub(crate) fn_values: FxHashMap<ExprId, FunctionId>,

    /// Calls through a value of function type: `f(x)` with `f` a function
    /// value, and `obj.f(x)` with `f` a field of function type.
    pub(crate) indirect_calls: FxHashSet<ExprId>,

    /// The method-call expressions among `indirect_calls`: `obj.f(x)`
    /// calling the function value in the field `f`.
    pub(crate) field_calls: FxHashSet<ExprId>,
    /// Expressions whose value is implicitly wrapped into an optional: a
    /// `T` used where a `T?` is expected. Maps each such expression to the
    /// optional type it becomes; the expression's own type stays `T`.
    pub(crate) optional_wraps: FxHashMap<ExprId, Ty>,
    /// `str` expressions used where a `String` is expected, which become a
    /// `String` copy of the text. The expression's own type stays `str`.
    pub(crate) string_conversions: FxHashSet<ExprId>,

    /// The enum variant each `Shape.Circle(..)`/`Shape.Point` expression
    /// constructs.
    pub(crate) variant_exprs: FxHashMap<ExprId, EnumVariant>,

    /// The enum variant each `Shape.Circle(..)`/`Shape.Point` pattern
    /// matches.
    pub(crate) variant_pats: FxHashMap<PatId, EnumVariant>,
    /// For each `x?`, the type its early return produces: the result type
    /// of the function or closure it returns from.
    pub(crate) propagations: FxHashMap<ExprId, Ty>,
    /// For each `x?` whose error is wrapped on the way out, the variant of
    /// the function's error enum that wraps it (see `infer::propagate`).
    pub(crate) propagation_wraps: FxHashMap<ExprId, EnumVariant>,
    /// For each call of a generic function -- a free function, a method or
    /// an associated function -- the types its generic parameters stand for
    /// at that call, in the order of the callee's
    /// [`Function::generic_environment`].
    ///
    /// This is what monomorphisation reads to find out which instance of
    /// the callee a call reaches. Inside a generic body the arguments may
    /// themselves mention the body's own parameters; substituting the
    /// caller's instance into them (see [`InferenceResult::map_types`])
    /// makes them concrete.
    pub(crate) instantiations: FxHashMap<ExprId, Substitution>,

    /// For each use of an object as an iterator -- `for x in it`, keyed by
    /// the `for` expression, and `it.iter()`, keyed by that call -- the
    /// `next(self) -> T?` method each element is pulled with. Its
    /// instantiation, for a generic class, is in `instantiations` under the
    /// same expression.
    pub(crate) iterator_nexts: FxHashMap<ExprId, FunctionId>,

    /// Interned Unknown to return references to.
    standard_types: InternedStandardTypes,
}

impl Index<ExprId> for InferenceResult {
    type Output = Ty;
    fn index(&self, expr: ExprId) -> &Ty {
        self.type_of_expr
            .get(expr)
            .unwrap_or(&self.standard_types.unknown)
    }
}

impl Index<PatId> for InferenceResult {
    type Output = Ty;
    fn index(&self, pat: PatId) -> &Ty {
        self.type_of_pat
            .get(pat)
            .unwrap_or(&self.standard_types.unknown)
    }
}

impl InferenceResult {
    /// Find the method resolution for the given expression. Returns `None` if
    /// the expression is not a method call.
    pub fn method_resolution(&self, expr: ExprId) -> Option<FunctionId> {
        self.method_resolutions.get(&expr).cloned()
    }

    /// Every type this result records: the type of every expression and of
    /// every pattern.
    pub fn types(&self) -> impl Iterator<Item = &Ty> {
        self.type_of_expr
            .iter()
            .map(|(_, ty)| ty)
            .chain(self.type_of_pat.iter().map(|(_, ty)| ty))
    }

    /// Returns the builtin a call expression invokes, or `None` when `expr`
    /// is not a call to a builtin. See [`BuiltinFn`].
    pub fn builtin_call(&self, expr: ExprId) -> Option<BuiltinFn> {
        self.builtin_calls.get(&expr).copied()
    }

    /// The signature and captures of the closure literal `expr`, or `None`
    /// when `expr` is not a closure.
    pub fn closure(&self, expr: ExprId) -> Option<&ClosureInfo> {
        self.closures.get(&expr)
    }

    /// Every closure literal of the body, in arena order -- the order code
    /// generation numbers them in.
    pub fn closures(&self) -> Vec<(ExprId, &ClosureInfo)> {
        let mut closures: Vec<_> = self.closures.iter().map(|(&e, c)| (e, c)).collect();
        closures.sort_by_key(|(e, _)| e.into_raw());
        closures
    }

    /// The function a path expression names when it is used as a function
    /// value (`let f = square`), or `None` for any other expression.
    /// Creating such a value allocates, like creating a closure does.
    pub fn function_value(&self, expr: ExprId) -> Option<Function> {
        self.fn_values.get(&expr).map(|&id| Function::from(id))
    }

    /// The type of every expression that has one.
    pub fn expr_types(&self) -> impl Iterator<Item = (ExprId, &Ty)> {
        self.type_of_expr.iter()
    }

    /// The type of every pattern that has one.
    pub fn pat_types(&self) -> impl Iterator<Item = (PatId, &Ty)> {
        self.type_of_pat.iter()
    }

    /// Every path expression of the body used as a function value, with the
    /// function it names, in arena order.
    pub fn function_values(&self) -> Vec<(ExprId, Function)> {
        let mut values: Vec<_> = self
            .fn_values
            .iter()
            .map(|(&e, &f)| (e, Function::from(f)))
            .collect();
        values.sort_by_key(|(e, _)| e.into_raw());
        values
    }

    /// Whether `expr` is a call through a function value -- `f(x)` where
    /// `f` has a function type, or `obj.f(x)` calling a field of function
    /// type. The callee is not known statically: an analysis that needs to
    /// know what a call does must treat such a call as unknown.
    pub fn is_indirect_call(&self, expr: ExprId) -> bool {
        self.indirect_calls.contains(&expr)
    }

    /// Whether the method-call expression `expr` calls the function value
    /// stored in a field (`obj.on_event(x)`) rather than a method.
    pub fn is_field_call(&self, expr: ExprId) -> bool {
        self.field_calls.contains(&expr)
    }

    /// Every type this inference assigned to an expression or a pattern.
    ///
    /// For consumers that need every type a body mentions -- code
    /// generation exports the layout of each tuple and optional the runtime
    /// may meet.
    pub fn all_types(&self) -> impl Iterator<Item = &Ty> {
        self.type_of_expr
            .iter()
            .map(|(_, ty)| ty)
            .chain(self.type_of_pat.iter().map(|(_, ty)| ty))
            .chain(self.optional_wraps.values())
            .chain(self.propagations.values())
    }

    /// The type `x?` returns early with (the enclosing result type), if
    /// `expr` is an `x?`.
    pub fn propagation(&self, expr: ExprId) -> Option<&Ty> {
        self.propagations.get(&expr)
    }

    /// The variant of the enclosing function's error enum that `expr`, an
    /// `x?`, wraps `x`'s error in, if it is wrapped.
    pub fn propagation_wrap(&self, expr: ExprId) -> Option<EnumVariant> {
        self.propagation_wraps.get(&expr).copied()
    }

    /// The enum variant `expr` constructs, if it is `Shape.Circle(..)` or
    /// `Shape.Point`.
    pub fn variant_of_expr(&self, expr: ExprId) -> Option<EnumVariant> {
        self.variant_exprs.get(&expr).copied()
    }

    /// The enum variant `pat` matches, if it is a variant pattern.
    pub fn variant_of_pat(&self, pat: PatId) -> Option<EnumVariant> {
        self.variant_pats.get(&pat).copied()
    }

    /// The optional type `expr`'s value is wrapped into, if it is used
    /// where a `T?` is expected (see `optional_wraps`).
    pub fn optional_wrap(&self, expr: ExprId) -> Option<&Ty> {
        self.optional_wraps.get(&expr)
    }

    /// Whether `expr`, a `str`, is converted to a `String` where it is
    /// used (see `string_conversions`).
    pub fn is_string_conversion(&self, expr: ExprId) -> bool {
        self.string_conversions.contains(&expr)
    }

    /// The types a call of a generic function instantiated the callee's
    /// generic parameters with, in the order of the callee's
    /// [`Function::generic_environment`]. `None` when `call` does not call
    /// a generic function.
    pub fn instantiation(&self, call: ExprId) -> Option<&Substitution> {
        self.instantiations.get(&call)
    }

    /// The `next` method an iterator object is pulled with, when `expr` is
    /// a `for` loop over such an object or `it.iter()` on one (see
    /// `ty::infer::pipelines`).
    pub fn iterator_next(&self, expr: ExprId) -> Option<Function> {
        self.iterator_nexts.get(&expr).map(|&id| Function::from(id))
    }

    /// Every use of an object as an iterator, with its `next` method, in
    /// arena order.
    pub fn iterator_nexts(&self) -> Vec<(ExprId, Function)> {
        let mut uses: Vec<_> = self
            .iterator_nexts
            .iter()
            .map(|(&e, &f)| (e, Function::from(f)))
            .collect();
        uses.sort_by_key(|(e, _)| e.into_raw());
        uses
    }

    /// This result with `f` applied to every type it records: the type of
    /// every expression and pattern and the arguments of every
    /// instantiation.
    ///
    /// Monomorphisation uses it to read a generic body at one instance --
    /// `f` replaces the body's own parameters with that instance's
    /// arguments -- so that code generation sees nothing but concrete
    /// types. Diagnostics are dropped: they belong to the generic
    /// declaration, which reports them once.
    pub fn map_types(&self, mut f: impl FnMut(&Ty) -> Ty) -> InferenceResult {
        let mut type_of_expr = ArenaMap::default();
        for (expr, ty) in self.type_of_expr.iter() {
            type_of_expr.insert(expr, f(ty));
        }
        let mut type_of_pat = ArenaMap::default();
        for (pat, ty) in self.type_of_pat.iter() {
            type_of_pat.insert(pat, f(ty));
        }
        let instantiations = self
            .instantiations
            .iter()
            .map(|(expr, args)| (*expr, args.iter().map(&mut f).collect()))
            .collect();
        InferenceResult {
            type_of_expr,
            type_of_pat,
            diagnostics: Vec::new(),
            method_resolutions: self.method_resolutions.clone(),
            builtin_calls: self.builtin_calls.clone(),
            effects: self.effects.clone(),
            instantiations,
            optional_wraps: self
                .optional_wraps
                .iter()
                .map(|(expr, ty)| (*expr, f(ty)))
                .collect(),
            string_conversions: self.string_conversions.clone(),
            variant_exprs: self.variant_exprs.clone(),
            variant_pats: self.variant_pats.clone(),
            propagations: self
                .propagations
                .iter()
                .map(|(expr, ty)| (*expr, f(ty)))
                .collect(),
            propagation_wraps: self.propagation_wraps.clone(),
            standard_types: InternedStandardTypes::default(),
            closures: self
                .closures
                .iter()
                .map(|(expr, closure)| {
                    let closure = ClosureInfo {
                        captures: closure.captures.clone(),
                        params: closure.params.iter().map(&mut f).collect(),
                        ret: f(&closure.ret),
                    };
                    (*expr, closure)
                })
                .collect(),
            fn_values: self.fn_values.clone(),
            indirect_calls: self.indirect_calls.clone(),
            field_calls: self.field_calls.clone(),
            iterator_nexts: self.iterator_nexts.clone(),
        }
    }

    /// Adds all the `InferenceDiagnostic`s of the result to the
    /// `DiagnosticSink`.
    pub(crate) fn add_diagnostics(
        &self,
        db: &dyn HirDatabase,
        owner: Function,
        sink: &mut DiagnosticSink<'_>,
    ) {
        self.diagnostics
            .iter()
            .for_each(|it| it.add_to(db, owner, sink));
    }
}

/// The entry point of type inference. This method takes a body and infers the
/// types of all the expressions and patterns. Diagnostics are also reported and
/// stored in the `InferenceResult`.
pub fn infer_query(db: &dyn HirDatabase, def: DefWithBodyId) -> Arc<InferenceResult> {
    let body = db.body(def);
    let resolver = def.resolver(db);
    let mut ctx = InferenceResultBuilder::new(db, &body, resolver);

    match def {
        DefWithBodyId::FunctionId(_) => ctx.infer_signature(),
    }

    ctx.infer_body();

    Arc::new(ctx.resolve_all())
}

/// Placeholders required during type inferencing. There are seperate values for
/// integer and floating-point types and for generic type variables. The first
/// being used to distinguish literals; e.g `100` can be represented by a lot of
/// different integer types.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum InferTy {
    Type(type_variable::TypeVarId),
    Int(type_variable::TypeVarId),
    Float(type_variable::TypeVarId),
}

impl InferTy {
    fn to_inner(self) -> type_variable::TypeVarId {
        match self {
            InferTy::Type(ty) | InferTy::Int(ty) | InferTy::Float(ty) => ty,
        }
    }

    fn fallback_value(self) -> Ty {
        match self {
            InferTy::Type(..) => TyKind::Unknown,
            InferTy::Int(..) => TyKind::Int(IntTy::i32()),
            InferTy::Float(..) => TyKind::Float(FloatTy::f64()),
        }
        .intern()
    }
}

/// What a call of an associated function is made on.
enum CallReceiver {
    /// `value.method(..)`: the receiver expression and its type.
    Value(ExprId, Ty),
    /// `Type.assoc(..)`: the receiver expression and the type it names.
    Type(ExprId, Ty),
    /// `T.assoc(..)` for a type parameter `T` whose bound declares `assoc`:
    /// the receiver expression and `T`. The callee is the trait's
    /// declaration, written in terms of the trait's `Self`, which is what
    /// `T` instantiates.
    TraitSelf(ExprId, Ty),
}

enum ActiveLoop {
    Loop(Ty, Expectation),
    While,
    For,
}

/// The inference context contains all information needed during type inference.
struct InferenceResultBuilder<'a> {
    db: &'a dyn HirDatabase,
    body: &'a Body,
    resolver: Resolver,

    type_of_expr: ArenaMap<ExprId, Ty>,
    type_of_pat: ArenaMap<PatId, Ty>,
    diagnostics: Vec<InferenceDiagnostic>,

    type_variables: TypeVariableTable,

    /// Information on the current loop that we're processing (or None if we're
    /// not in a loop) the entry contains the current type of the loop
    /// statement (initially `never`) and the expected type of the loop
    /// expression. Both these values are updated when a break statement is
    /// encountered.
    active_loop: Option<ActiveLoop>,

    /// The return type of the function being inferred.
    return_ty: Ty,

    /// Stores the resolution of method calls
    method_resolution: FxHashMap<ExprId, FunctionId>,

    /// Stores which call expressions invoke a compiler builtin
    builtin_calls: FxHashMap<ExprId, BuiltinFn>,

    /// What every `perform` and handler arm resolved to.
    effects: effects::EffectResolutions,

    /// The return type of the effect operation whose handler arm is being
    /// inferred: what `resume` takes. `None` outside every arm.
    active_arm_ret: Option<Ty>,

    /// The closures whose bodies are being inferred, innermost last.
    closure_frames: Vec<closures::ClosureFrame>,
    closures: FxHashMap<ExprId, ClosureInfo>,
    fn_values: FxHashMap<ExprId, FunctionId>,
    indirect_calls: FxHashSet<ExprId>,
    field_calls: FxHashSet<ExprId>,
    /// Expressions implicitly wrapped into an optional
    optional_wraps: FxHashMap<ExprId, Ty>,
    string_conversions: FxHashSet<ExprId>,

    /// Variant constructions and variant patterns (see `InferenceResult`).
    variant_exprs: FxHashMap<ExprId, EnumVariant>,
    pat_variants: FxHashMap<PatId, EnumVariant>,
    /// See [`InferenceResult::propagations`].
    propagations: FxHashMap<ExprId, Ty>,
    propagation_wraps: FxHashMap<ExprId, EnumVariant>,
    /// See [`InferenceResult::instantiations`]; the callee is kept until
    /// `resolve_all` so that a parameter no argument determined can be
    /// reported by name.
    instantiations: FxHashMap<ExprId, (Function, Substitution)>,
    /// See [`InferenceResult::iterator_nexts`].
    iterator_nexts: FxHashMap<ExprId, FunctionId>,

    /// The bounds that hold in the body: what its type parameters
    /// implement (see `crate::traits`).
    bounds: Arc<crate::traits::GenericBounds>,
}

impl<'a> InferenceResultBuilder<'a> {
    /// Construct a new `InferenceContext` from a `Body` and a `Resolver` for
    /// that body.
    fn new(db: &'a dyn HirDatabase, body: &'a Body, resolver: Resolver) -> Self {
        InferenceResultBuilder {
            type_of_expr: ArenaMap::default(),
            type_of_pat: ArenaMap::default(),
            diagnostics: Vec::default(),
            active_loop: None,
            type_variables: TypeVariableTable::default(),
            db,
            body,
            resolver,
            return_ty: TyKind::Unknown.intern(), // set in collect_fn_signature
            method_resolution: FxHashMap::default(),
            builtin_calls: FxHashMap::default(),
            effects: effects::EffectResolutions::default(),
            active_arm_ret: None,
            closure_frames: Vec::new(),
            closures: FxHashMap::default(),
            fn_values: FxHashMap::default(),
            indirect_calls: FxHashSet::default(),
            field_calls: FxHashSet::default(),

            optional_wraps: FxHashMap::default(),
            string_conversions: FxHashSet::default(),
            variant_exprs: FxHashMap::default(),
            pat_variants: FxHashMap::default(),
            propagations: FxHashMap::default(),
            propagation_wraps: FxHashMap::default(),
            instantiations: FxHashMap::default(),
            iterator_nexts: FxHashMap::default(),
            bounds: match body.owner() {
                DefWithBodyId::FunctionId(f) => db.function_bounds(f),
            },
        }
    }

    /// Returns the module in which the body is defined.
    pub fn module(&self) -> ModuleId {
        match self.body.owner() {
            DefWithBodyId::FunctionId(func) => func.module(self.db),
        }
    }

    /// Associate the given `ExprId` with the specified `Ty`.
    fn set_expr_type(&mut self, expr: ExprId, ty: Ty) {
        self.type_of_expr.insert(expr, ty);
    }

    /// Associate the given `PatId` with the specified `Ty`.
    fn set_pat_type(&mut self, pat: PatId, ty: Ty) {
        self.type_of_pat.insert(pat, ty);
    }

    /// Given a `LocalTypeRefId`, resolve the reference to an actual `Ty`. If
    /// the the type could not be resolved an error is emitted and
    /// `Ty::Error` is returned.
    fn resolve_type(&mut self, type_ref: LocalTypeRefId) -> Ty {
        // Try to resolve the type from the Hir
        let (ty, diagnostics) = Ty::from_hir(
            self.db,
            // FIXME use right resolver for block
            &self.resolver,
            self.body.type_refs(),
            type_ref,
        );

        // Convert the diagnostics from resolving the type reference
        for diag in diagnostics {
            let diag = match diag {
                LowerDiagnostic::UnresolvedType { id } => {
                    InferenceDiagnostic::UnresolvedType { id }
                }
                LowerDiagnostic::DualOfNonSession { id } => {
                    InferenceDiagnostic::DualOfNonSession { id }
                }
                LowerDiagnostic::TraitAsType { id } => InferenceDiagnostic::TraitAsType { id },
                LowerDiagnostic::TypeIsPrivate { id } => InferenceDiagnostic::TypeIsPrivate { id },
            };
            self.diagnostics.push(diag);
        }

        ty
    }
}

impl InferenceResultBuilder<'_> {
    /// Collect all the parameter patterns from the body. After calling this
    /// method the `return_ty` will have a valid value, also all parameters
    /// are added inferred.
    fn infer_signature(&mut self) {
        if let Some((self_pat, self_type_ref)) = self.body.self_param() {
            let ty = self.resolve_type(*self_type_ref);
            self.infer_pat(*self_pat, ty);
        }

        // Iterate over all the parameters and associated types of the body and infer
        // the types of the parameters.
        // Aliases are seen through exactly as `fn_sig_for_fn` does, so a
        // parameter's binding has the same type callers see in the
        // signature.
        let strip_alias = |db: &dyn HirDatabase, ty: Ty| match ty.interned() {
            TyKind::TypeAlias(alias) => alias.target_type(db),
            _ => ty,
        };
        for (pat, type_ref) in self.body.params().iter() {
            let ty = self.resolve_type(*type_ref);
            let ty = strip_alias(self.db, ty);
            self.check_irrefutable(*pat, "a parameter's pattern");
            self.infer_pat(*pat, ty);
        }

        // Resolve the return type
        let ret = self.resolve_type(self.body.ret_type());
        self.return_ty = strip_alias(self.db, ret);
    }

    /// Record the type of the specified pattern and all sub-patterns.
    fn infer_pat(&mut self, pat: PatId, ty: Ty) {
        match &self.body[pat] {
            Pat::Bind { name: _name } => {
                self.set_pat_type(pat, ty);
            }
            // `let (x, y): (A, B)` -- each sub-pattern takes the type of the
            // element it destructures, so the binding types come out right
            // without the sub-patterns having to be annotated.
            //
            // An arity mismatch is reported once, against the tuple pattern
            // itself, and the sub-patterns are then left un-typed rather than
            // being paired up against whatever happens to line up: a
            // plausible-looking wrong type on `y` would send the error
            // cascading into every later use of it.
            Pat::Tuple(args) => {
                let args = args.clone();
                if let TyKind::Tuple(arity, substs) = ty.interned() {
                    if *arity == args.len() {
                        let substs = substs.clone();
                        self.set_pat_type(pat, ty);
                        for (&arg, elem_ty) in args.iter().zip(substs.iter()) {
                            self.infer_pat(arg, elem_ty.clone());
                        }
                        return;
                    }

                    self.diagnostics
                        .push(InferenceDiagnostic::InvalidTupleDestructure {
                            id: pat,
                            found: ty.clone(),
                            arity: args.len(),
                            expected_arity: Some(*arity),
                        });
                } else if !ty.is_unknown() {
                    self.diagnostics
                        .push(InferenceDiagnostic::InvalidTupleDestructure {
                            id: pat,
                            found: ty.clone(),
                            arity: args.len(),
                            expected_arity: None,
                        });
                }
                self.set_pat_type(pat, ty);
            }
            _ => {}
        }
    }

    /// Infer the types of all the expressions and sub-expressions in the body.
    fn infer_body(&mut self) {
        self.infer_expr_coerce(
            self.body.body_expr(),
            &Expectation::has_type(self.return_ty.clone()),
        );
        self.infer_contracts();
    }

    /// Type-checks the function's `requires`/`ensures` clauses: each must be
    /// a `bool`, and inside `ensures` the binding `result` has the function's
    /// return type. Loop clauses and ghost lets are checked where they
    /// appear (`infer_loop_contract`, `infer_block`).
    fn infer_contracts(&mut self) {
        let contracts = self.body.contracts();
        if let Some(result) = contracts.result_pat {
            self.set_pat_type(result, self.return_ty.clone());
        }
        let bool_ty = Expectation::has_type(TyKind::Bool.intern());
        for &clause in contracts.requires.iter().chain(contracts.ensures.iter()) {
            self.infer_expr(clause, &bool_ty);
        }
    }

    /// Type-checks a loop's `invariant` clauses (each a `bool`) and its
    /// `decreases` measure (any type here; the verifier reports a measure
    /// that is not an integer).
    fn infer_loop_contract(&mut self, loop_expr: ExprId) {
        let Some(contract) = self.body.contracts().loops.get(&loop_expr) else {
            return;
        };
        let bool_ty = Expectation::has_type(TyKind::Bool.intern());
        for &clause in &contract.invariants {
            self.infer_expr(clause, &bool_ty);
        }
        if let Some(measure) = contract.decreases {
            self.infer_expr(measure, &Expectation::none());
        }
    }

    /// Infers the type of the `tgt_expr`
    fn infer_expr(&mut self, tgt_expr: ExprId, expected: &Expectation) -> Ty {
        let ty = self.infer_expr_inner(tgt_expr, expected, &CheckParams::default());
        if !self.unify(&ty, &expected.ty) {
            self.diagnostics.push(InferenceDiagnostic::MismatchedTypes {
                expected: expected.ty.clone(),
                found: ty.clone(),
                id: tgt_expr,
            });
        };

        self.resolve_ty_as_far_as_possible(ty)
    }

    /// Infer type of expression with possibly implicit coerce to the expected
    /// type. Return the type after possible coercion. Adds a diagnostic
    /// message if coercion failed.
    fn infer_expr_coerce(&mut self, expr: ExprId, expected: &Expectation) -> Ty {
        let ty = self.infer_expr_inner(expr, expected, &CheckParams::default());
        self.coerce_expr_ty(expr, ty, expected)
    }

    /// Performs implicit coercion of the specified `Ty` to an expected type.
    /// Returns the type after possible coercion. Adds a diagnostic message
    /// if coercion failed.
    fn coerce_expr_ty(&mut self, expr: ExprId, ty: Ty, expected: &Expectation) -> Ty {
        if let Some(wrapped) = self.try_wrap_optional(expr, &ty, &expected.ty) {
            return wrapped;
        }
        if let Some(converted) = self.try_convert_to_string(expr, &ty, &expected.ty) {
            return converted;
        }
        let ty = if !self.coerce(&ty, &expected.ty) {
            self.diagnostics.push(InferenceDiagnostic::MismatchedTypes {
                expected: expected.ty.clone(),
                found: ty.clone(),
                id: expr,
            });
            ty
        } else if expected.ty.is_unknown() {
            ty
        } else {
            expected.ty.clone()
        };

        self.resolve_ty_as_far_as_possible(ty)
    }

    /// The implicit `T` to `T?` conversion: a value used where an optional
    /// is expected becomes a present optional. Records `expr` for code
    /// generation and returns the optional type, or `None` when no wrapping
    /// applies (the value is already optional, still unknown, or diverges).
    fn try_wrap_optional(&mut self, expr: ExprId, ty: &Ty, expected: &Ty) -> Option<Ty> {
        let expected = self.normalize(expected);
        let TyKind::Optional(inner) = expected.interned() else {
            return None;
        };
        let actual = self.normalize(ty);
        match actual.interned() {
            TyKind::Optional(_)
            | TyKind::InferenceVar(InferTy::Type(_))
            | TyKind::Never
            | TyKind::Unknown => return None,
            _ => {}
        }
        if !self.unify(&actual, inner) {
            return None;
        }
        let expected = self.resolve_ty_as_far_as_possible(expected.clone());
        self.optional_wraps.insert(expr, expected.clone());
        Some(expected)
    }

    /// Infer the type of the given expression. Returns the type of the
    /// expression.
    #[allow(clippy::match_same_arms)]
    fn infer_expr_inner(
        &mut self,
        tgt_expr: ExprId,
        expected: &Expectation,
        check_params: &CheckParams,
    ) -> Ty {
        let ty = match &self.body[tgt_expr] {
            // A synthesis hole takes the type its context expects; a
            // `choose` also gives that type to its binder and checks its
            // predicate.
            Expr::Missing if self.body.contracts().holes.contains(&tgt_expr) => expected.ty.clone(),
            Expr::Missing if self.body.contracts().chooses.contains_key(&tgt_expr) => {
                let choose = self.body.contracts().chooses[&tgt_expr];
                self.set_pat_type(choose.binder, expected.ty.clone());
                self.infer_expr(
                    choose.predicate,
                    &Expectation::has_type(TyKind::Bool.intern()),
                );
                expected.ty.clone()
            }
            Expr::Missing => error_type(),
            Expr::Path(p) => {
                // FIXME this could be more efficient...
                let resolver = resolver_for_expr(self.db, self.body.owner(), tgt_expr);
                let ty = self
                    .infer_path_expr(&resolver, p, tgt_expr, check_params)
                    .unwrap_or_else(error_type);
                if check_params.is_callee {
                    ty
                } else {
                    self.function_item_as_value(tgt_expr, ty)
                }
            }
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => self.infer_if(tgt_expr, expected, *condition, *then_branch, *else_branch),
            Expr::BinaryOp { lhs, rhs, op } => match op {
                Some(op) => {
                    let lhs_expected = match op {
                        BinaryOp::LogicOp(..) => Expectation::has_type(TyKind::Bool.intern()),
                        _ => Expectation::none(),
                    };
                    let lhs_ty = self.infer_expr(*lhs, &lhs_expected);
                    // An operator sees through a type alias to the type it
                    // names: `x / 2` on `x: Positive` (`type Positive = i64
                    // { v | v > 0 }`) is `i64` arithmetic, and its result is
                    // a plain `i64` -- a refinement does not survive
                    // arithmetic, so it is the base type that flows on.
                    let lhs_ty = match lhs_ty.interned() {
                        TyKind::TypeAlias(alias) => alias.target_type(self.db),
                        _ => lhs_ty,
                    };
                    if let BinaryOp::Assignment { op: _op } = op {
                        let resolver = resolver_for_expr(self.db, self.body.owner(), tgt_expr);
                        if !self.check_place_expression(&resolver, *lhs) {
                            self.diagnostics.push(InferenceDiagnostic::InvalidLhs {
                                id: tgt_expr,
                                lhs: *lhs,
                            });
                        }
                        self.check_captured_assignment(tgt_expr, *lhs);
                        self.check_immutable_assignment(tgt_expr, *lhs);
                    };
                    // `+`, `+=` and comparisons on `String` and `str`.
                    if let Some(ty) = self.infer_string_binary_op(tgt_expr, *op, *rhs, &lhs_ty) {
                        let ty = self.resolve_ty_as_far_as_possible(ty);
                        self.set_expr_type(tgt_expr, ty.clone());
                        return ty;
                    }
                    let rhs_expected = op::binary_op_rhs_expectation(*op, lhs_ty.clone());
                    if matches!(lhs_ty.interned(), TyKind::TypeParam(..))
                        && rhs_expected.is_unknown()
                    {
                        self.report_op_on_type_param(
                            tgt_expr,
                            &format!("cannot apply `{}` to", binary_op_symbol(*op)),
                            &lhs_ty,
                        );
                    } else if lhs_ty.is_known() && rhs_expected.is_unknown() {
                        self.diagnostics
                            .push(InferenceDiagnostic::CannotApplyBinaryOp {
                                id: tgt_expr,
                                lhs: lhs_ty.clone(),
                                rhs: rhs_expected.clone(),
                            });
                    }
                    let lhs_is_optional =
                        matches!(self.normalize(&lhs_ty).interned(), TyKind::Optional(_));
                    let rhs_ty = if lhs_is_optional || lhs_ty.is_string() {
                        self.infer_expr_coerce(*rhs, &Expectation::has_type(rhs_expected))
                    } else {
                        self.infer_expr(*rhs, &Expectation::has_type(rhs_expected))
                    };
                    if lhs_is_optional {
                        self.check_optional_comparison(tgt_expr, *op, &lhs_ty, *rhs);
                    }
                    if let TyKind::Enum(e, args) = self.normalize(&lhs_ty).interned().clone() {
                        self.check_enum_comparison(tgt_expr, *op, e, &args);
                    }
                    op::binary_op_return_ty(*op, rhs_ty)
                }
                _ => error_type(),
            },
            Expr::Block { statements, tail } => {
                self.infer_block(tgt_expr, statements, *tail, expected)
            }
            Expr::Call { callee: call, args } => self.infer_call(tgt_expr, *call, args, expected),
            Expr::MethodCall {
                receiver,
                args,
                method_name,
            } => self.infer_method_call(tgt_expr, *receiver, args, method_name, expected),
            Expr::Literal(lit) => match lit {
                Literal::String(_) => TyKind::Str.intern(),
                Literal::Bool(_) => TyKind::Bool.intern(),
                Literal::Int(LiteralInt {
                    kind: LiteralIntKind::Suffixed(suffix),
                    ..
                }) => TyKind::Int(IntTy {
                    bitness: suffix.bitness,
                    signedness: suffix.signedness,
                })
                .intern(),
                Literal::Float(LiteralFloat {
                    kind: LiteralFloatKind::Suffixed(suffix),
                    ..
                }) => TyKind::Float(FloatTy {
                    bitness: suffix.bitness,
                })
                .intern(),
                Literal::Int(LiteralInt {
                    kind: LiteralIntKind::Unsuffixed,
                    ..
                }) => self.type_variables.new_integer_var(),
                Literal::Float(LiteralFloat {
                    kind: LiteralFloatKind::Unsuffixed,
                    ..
                }) => self.type_variables.new_float_var(),
                // `nil` should unify with whatever `Type?` it's checked against; there
                // is no dedicated `TyKind::Optional` yet (see `TypeRef::Optional` in
                // `type_ref.rs`), so for now it is left as `Unknown` rather than
                // pretending to enforce a nil-safety guarantee that isn't wired up.
                // `nil` is the absent value of whatever optional it is used
                // as; with no optional expected, its payload type is left
                // for later uses to determine.
                Literal::Nil => {
                    let expected_ty = self.normalize(&expected.ty);
                    if let TyKind::Optional(_) = expected_ty.interned() {
                        expected_ty
                    } else {
                        TyKind::Optional(self.type_variables.new_type_var()).intern()
                    }
                }
            },
            Expr::Return { expr } => {
                if let Some(expr) = expr {
                    self.infer_expr_coerce(*expr, &Expectation::has_type(self.return_ty.clone()));
                } else if !self.return_ty.is_empty() {
                    self.diagnostics
                        .push(InferenceDiagnostic::ReturnMissingExpression { id: tgt_expr });
                }

                TyKind::Never.intern()
            }
            Expr::Break { expr } => self.infer_break(tgt_expr, *expr),
            Expr::Loop { body } => self.infer_loop_expr(tgt_expr, *body, expected),
            Expr::While { condition, body } => {
                self.infer_while_expr(tgt_expr, *condition, *body, expected)
            }
            Expr::For {
                pat,
                iterable,
                body,
            } => self.infer_for_expr(tgt_expr, *pat, *iterable, *body),
            Expr::Match { expr, arms, source } => {
                self.infer_match(tgt_expr, *expr, arms, *source, expected)
            }
            Expr::Propagate { expr } => self.infer_propagate(tgt_expr, *expr),
            Expr::Format {
                value,
                width,
                precision,
                ..
            } => self.infer_format(*value, *width, *precision),
            Expr::Unwrap { expr } => {
                let inner_ty = self.infer_expr(*expr, &Expectation::none());
                match self.normalize(&inner_ty).interned() {
                    TyKind::Optional(payload) => payload.clone(),
                    TyKind::Unknown => error_type(),
                    _ => {
                        self.diagnostics.push(InferenceDiagnostic::ExprError {
                            id: tgt_expr.into(),
                            message: format!(
                                "postfix `!` unwraps an optional (`T?`), but this is `{}`",
                                inner_ty.display(self.db)
                            ),
                        });
                        error_type()
                    }
                }
            }
            Expr::Continue => {
                if self.active_loop.is_none() {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: "`continue` outside of a loop".to_string(),
                    });
                }
                TyKind::Never.intern()
            }
            Expr::Range { start, end, .. } => {
                self.infer_range_bounds(*start, *end);
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: tgt_expr.into(),
                    message: "a range can only be used as the iterable of a `for` loop or as the \
                              source of a pipeline (`(0..n).map(f).sum()`) for now; there is no \
                              range value type yet"
                        .to_string(),
                });
                error_type()
            }
            Expr::RecordLit {
                type_id,
                fields,
                spread,
            } => {
                // `Box { v: 7 }` names `Box`, which lowers to `Box[T]` with
                // its own parameter standing in. Instantiating it here is
                // what lets the field values decide what `T` is -- written
                // without an argument list, that is the only thing that can.
                let declared = self.resolve_type(*type_id);
                let ty = self.instantiate_type(&declared);
                let def_id = ty.as_struct();
                self.unify(&ty, &expected.ty);

                let substitution = ty
                    .type_parameters()
                    .cloned()
                    .unwrap_or_else(Substitution::empty);

                for (idx, field) in fields.iter().enumerate() {
                    let field_ty = def_id
                        .as_ref()
                        .and_then(|it| {
                            if let Some(field) = it.field(self.db, &field.name) {
                                Some(field)
                            } else {
                                self.diagnostics.push(InferenceDiagnostic::NoSuchField {
                                    id: tgt_expr,
                                    field: idx,
                                });
                                None
                            }
                        })
                        .map_or(error_type(), |field| {
                            // The field's declared type is in terms of the
                            // struct's parameters, so it has to be read
                            // through the same instantiation the literal got.
                            def_id.map_or_else(
                                || field.ty(self.db),
                                |s| {
                                    substitute_type_params(
                                        field.ty(self.db),
                                        &GenericDefId::from(s.id),
                                        &substitution,
                                    )
                                },
                            )
                        });
                    self.infer_expr_coerce(field.expr, &Expectation::has_type(field_ty));
                }
                if let Some(expr) = spread {
                    self.infer_expr(*expr, &Expectation::has_type(ty.clone()));
                }
                if let Some(s) = ty.as_struct() {
                    self.check_record_lit(tgt_expr, &ty, s, fields);
                }
                ty
            }
            Expr::Field { expr, name } => {
                if let Some(e) = self.enum_named_by(tgt_expr, *expr) {
                    self.infer_variant_construction(tgt_expr, *expr, e, name, None, expected)
                } else {
                    let receiver_ty = self.infer_expr(*expr, &Expectation::none());
                    if let Some((field_ty, is_visible)) =
                        self.lookup_field(receiver_ty.clone(), name)
                    {
                        if !is_visible {
                            self.diagnostics
                                .push(InferenceDiagnostic::AccessPrivateField {
                                    id: tgt_expr,
                                    receiver_ty,
                                    name: name.clone(),
                                });
                        }
                        field_ty
                    } else {
                        self.diagnostics
                            .push(InferenceDiagnostic::AccessUnknownField {
                                id: tgt_expr,
                                receiver_ty,
                                name: name.clone(),
                            });
                        error_type()
                    }
                }
            }
            Expr::UnaryOp { expr, op } => {
                let inner_ty =
                    self.infer_expr_inner(*expr, &Expectation::none(), &CheckParams::default());
                if matches!(inner_ty.interned(), TyKind::TypeParam(..)) {
                    let what = match op {
                        UnaryOp::Neg => "cannot negate",
                        UnaryOp::Not | UnaryOp::BitNot => "cannot complement",
                    };
                    self.report_op_on_type_param(tgt_expr, what, &inner_ty);
                    return error_type();
                }
                match op {
                    // `~` is bitwise complement: integers only. `!` keeps
                    // its historical bool-or-integer behaviour; separating
                    // them is what lets `~` be type-checked strictly.
                    UnaryOp::BitNot => match inner_ty.interned() {
                        TyKind::Int(_) | TyKind::InferenceVar(InferTy::Int(_)) => inner_ty,
                        _ => {
                            self.diagnostics
                                .push(InferenceDiagnostic::CannotApplyUnaryOp {
                                    id: *expr,
                                    ty: inner_ty,
                                });
                            error_type()
                        }
                    },
                    UnaryOp::Not => match inner_ty.interned() {
                        TyKind::Bool | TyKind::Int(_) | TyKind::InferenceVar(InferTy::Int(_)) => {
                            inner_ty
                        }
                        _ => {
                            self.diagnostics
                                .push(InferenceDiagnostic::CannotApplyUnaryOp {
                                    id: *expr,
                                    ty: inner_ty,
                                });
                            error_type()
                        }
                    },
                    UnaryOp::Neg => match inner_ty.interned() {
                        TyKind::Float(_)
                        | TyKind::Int(_)
                        | TyKind::InferenceVar(InferTy::Int(_) | InferTy::Float(_)) => inner_ty,
                        _ => {
                            self.diagnostics
                                .push(InferenceDiagnostic::CannotApplyUnaryOp {
                                    id: *expr,
                                    ty: inner_ty,
                                });
                            error_type()
                        }
                    },
                }
            }
            Expr::Array(array) => {
                let elem_ty = match expected.ty.interned() {
                    TyKind::Array(elem_ty) => elem_ty.clone(),
                    _ => self.type_variables.new_type_var(),
                };

                for expr in array.iter() {
                    self.infer_expr_coerce(*expr, &Expectation::has_type(elem_ty.clone()));
                }

                TyKind::Array(elem_ty).intern()
            }
            // `(a, b)`. Unlike an array, each element has its own type, so the
            // expectation is pushed down element-wise: `let t: (i32, f64) =
            // (1, 2)` must see `f64` for the second element, not a fresh
            // variable that later defaults to an integer.
            //
            // An expectation of the wrong arity is simply not propagated; the
            // resulting type still describes what was written, and the
            // mismatch is reported by the caller's own unification rather
            // than being masked by a silently truncated zip here.
            Expr::Tuple(exprs) => {
                let expected_elems = match expected.ty.interned() {
                    TyKind::Tuple(arity, subs) if *arity == exprs.len() => Some(subs.clone()),
                    _ => None,
                };

                let elem_tys: Vec<Ty> = exprs
                    .iter()
                    .enumerate()
                    .map(|(idx, expr)| {
                        let expectation = expected_elems
                            .as_ref()
                            .map_or_else(Expectation::none, |subs| {
                                Expectation::has_type(subs[idx].clone())
                            });
                        self.infer_expr_coerce(*expr, &expectation)
                    })
                    .collect();

                TyKind::Tuple(elem_tys.len(), elem_tys.into_iter().collect()).intern()
            }
            Expr::Index { base, index } if matches!(self.body[*index], Expr::Range { .. }) => {
                let Expr::Range { start, end, .. } = self.body[*index] else {
                    unreachable!("matched above")
                };
                self.infer_array_slice(*base, start, end, expected)
            }
            Expr::Index { base, index } => {
                // An optional expected type is reached by wrapping the
                // element afterwards (`xs[i]` where an `i64?` is expected),
                // not by an array of optionals.
                let expects_optional =
                    matches!(self.normalize(&expected.ty).interned(), TyKind::Optional(_));
                let elem_ty = if expected.ty.is_unknown() || expects_optional {
                    self.type_variables.new_type_var()
                } else {
                    expected.ty.clone()
                };

                let base_ty = self.infer_expr(
                    *base,
                    &Expectation::has_type(TyKind::Array(elem_ty).intern()),
                );

                let inner_ty = self.type_variables.new_integer_var();
                let _index_expr = self.infer_expr(*index, &Expectation::has_type(inner_ty));

                match base_ty.interned() {
                    TyKind::Array(ty) => ty.clone(),
                    _ => error_type(),
                }
            }
            Expr::Cast { expr, type_ref } => self.infer_cast(tgt_expr, *expr, *type_ref),
            Expr::Perform { path, args } => self.infer_perform(tgt_expr, path, args),
            Expr::Handle { body, arms } => self.infer_handle(tgt_expr, *body, arms, expected),
            Expr::Resume { value } => self.infer_resume(tgt_expr, *value),
            Expr::Scope { body } => self.infer_expr_coerce(*body, expected),
            Expr::Group { body } => self.infer_group(tgt_expr, *body),
            Expr::Spawn { call } => self.infer_spawn(*call),
            Expr::ChannelSend { channel, value } => {
                self.infer_channel_send(tgt_expr, *channel, *value)
            }
            Expr::ChannelReceive { channel } => self.infer_channel_receive(tgt_expr, *channel),
            Expr::Select { arms } => self.infer_select(arms, expected),
            Expr::Offer { endpoint, branches } => self.infer_offer(*endpoint, branches, expected),
            Expr::Closure {
                params,
                ret_type,
                body,
            } => self.infer_closure(tgt_expr, params, *ret_type, *body, expected),
        };

        let ty = self.resolve_ty_as_far_as_possible(ty);
        self.set_expr_type(tgt_expr, ty.clone());
        ty
    }

    /// Infers the type of an `expr as Type` cast.
    ///
    /// The type of the whole expression is always the written-out target
    /// type; `as` is an *explicit* conversion, so the target deliberately does
    /// not flow back into the operand as an expectation (`1 as i64` leaves the
    /// literal at its own default type rather than making it an `i64`).
    ///
    /// The conversion itself must be one the language actually allows: the
    /// legality matrix lives in [`crate::ty::cast`] and an illegal cast is a
    /// type error here, never a silent reinterpretation.
    fn infer_cast(&mut self, tgt_expr: ExprId, operand: ExprId, type_ref: LocalTypeRefId) -> Ty {
        let source_ty = self.infer_expr(operand, &Expectation::none());
        let target_ty = self.resolve_type(type_ref);

        let source = self.normalize(&source_ty);
        let target = self.normalize(&target_ty);
        let layout = self.db.target_data_layout();

        if let CastCheck::Illegal(reason) = check_cast(&source, &target, &layout) {
            self.diagnostics.push(InferenceDiagnostic::InvalidCast {
                id: tgt_expr,
                from: source,
                to: target,
                reason,
            });
        }

        target_ty
    }

    /// Whether `ty` is the never type, seeing through aliases.
    ///
    /// `Ty::is_never` matches `TyKind::Never` exactly, so a function declared
    /// `-> Never` (where `Never` is `type Never = never`) did not count as
    /// diverging: its block fell through to `()` and then failed to match its
    /// own return type. An alias is the same type as what it names, and
    /// divergence is the one property where getting that wrong turns working
    /// code into an error.
    fn is_never(&mut self, ty: &Ty) -> bool {
        ty.is_never() || self.normalize(ty).is_never()
    }

    /// Resolves instantiated inference variables and type aliases so that a
    /// check sees the underlying type of e.g. `type Byte = u8`.
    ///
    /// `replace_if_possible` unwraps a single alias level per call, so this
    /// iterates; the bound keeps a cyclic alias (`type A = B; type B = A;`,
    /// already reported as `cyclic type`) from looping forever. Whatever is
    /// left at that point is still a `TypeAlias`, which callers treat as
    /// unknown rather than as an error.
    fn normalize(&mut self, ty: &Ty) -> Ty {
        let mut ty = ty.clone();
        for _ in 0..16 {
            let resolved = self.replace_if_possible(&ty).into_owned();
            if resolved == ty {
                break;
            }
            ty = resolved;
        }
        ty
    }

    /// Inferences the type of an if statement.
    fn infer_if(
        &mut self,
        tgt_expr: ExprId,
        expected: &Expectation,
        condition: ExprId,
        then_branch: ExprId,
        else_branch: Option<ExprId>,
    ) -> Ty {
        self.infer_expr(condition, &Expectation::has_type(TyKind::Bool.intern()));
        let then_ty = self.infer_expr_coerce(then_branch, expected);
        if let Some(else_branch) = else_branch {
            let else_ty = self.infer_expr_coerce(else_branch, expected);
            if let Some(ty) = self.coerce_merge_branch(&then_ty, &else_ty) {
                ty
            } else {
                self.diagnostics
                    .push(InferenceDiagnostic::IncompatibleBranches {
                        id: tgt_expr,
                        then_ty: then_ty.clone(),
                        else_ty: else_ty.clone(),
                    });
                then_ty
            }
        } else {
            if !self.coerce(&then_ty, &Ty::unit()) {
                self.diagnostics
                    .push(InferenceDiagnostic::MissingElseBranch {
                        id: tgt_expr,
                        then_ty,
                    });
            }
            Ty::unit()
        }
    }

    fn lookup_field(&mut self, receiver_ty: Ty, field_name: &Name) -> Option<(Ty, bool)> {
        match receiver_ty.interned() {
            TyKind::Tuple(_, subs) => {
                let idx = field_name.as_tuple_index()?;
                let field_ty = subs.interned().get(idx)?.clone();
                Some((field_ty, true))
            }
            TyKind::Struct(s, substitution) => {
                let struct_data = self.db.struct_data(s.id);
                let local_field_idx = struct_data.find_field(field_name)?;
                let field_types = self.db.lower_struct(*s);
                let field_visibilities = self.db.field_visibilities(s.id.into());
                let field_data = &struct_data.fields[local_field_idx];

                // The declared field type is written in terms of the
                // struct's own parameters -- `value: T` for `Box[T]`. The
                // receiver says what `T` is here, so applying its
                // substitution is what turns `T` into `i32` for a
                // `Box[i32]`. Without it every field of a generic struct
                // reads back as the parameter itself.
                let declared = field_types[field_data.type_ref].clone();
                let field_ty =
                    substitute_type_params(declared, &GenericDefId::from(s.id), substitution);

                Some((
                    field_ty,
                    field_visibilities[local_field_idx].is_visible_from(self.db, self.module()),
                ))
            }
            _ => None,
        }
    }

    fn infer_method_call(
        &mut self,
        tgt_expr: ExprId,
        receiver: ExprId,
        args: &[ExprId],
        method_name: &Name,
        expected: &Expectation,
    ) -> Ty {
        // If the method name is missing from the AST we simply return an error type
        // since an error would have already been emitted by the AST generation.
        if method_name.is_missing() {
            return error_type();
        }

        // `S.open()` on a session's name.
        if let Some(session) = crate::expr::session::opened_session(self.db, self.body, tgt_expr) {
            return self.infer_session_open(tgt_expr, session, args, method_name);
        }

        // `Type.assoc_fn(args)` -- static member access, which
        // `spec/LANGUAGE_SPEC.md` section 3 spells out directly
        // (`Box[i32].new(5)`). Syntactically this is indistinguishable from a
        // method call, so it has to be recognised here, *before* the receiver
        // is inferred as a value: a record struct's name is not a value, so
        // inferring it would report a spurious "mismatched struct literal"
        // and then a "method does not exist" on top of it.
        if let Some(ty) =
            self.infer_static_member_call(tgt_expr, receiver, args, method_name, expected)
        {
            return ty;
        }

        // `(a..b).map(f)`: a range is the source of a pipeline.
        if let (Expr::Range { start, end, .. }, Some(op)) = (
            &self.body[receiver],
            method_name.as_str().and_then(crate::PipelineOp::from_name),
        ) {
            let bounds = (*start, *end);
            return self.infer_range_pipeline_call(tgt_expr, receiver, bounds, op, args);
        }

        let receiver_ty = self.infer_expr(receiver, &Expectation::none());

        // `ep.choose(branch)` on a session endpoint.
        if matches!(self.normalize(&receiver_ty).interned(), TyKind::Session(..)) {
            return self.infer_endpoint_method(args);
        }

        // Resolve the method on the receiver type.
        let resolved_function = match lookup_method(
            self.db,
            &receiver_ty,
            self.module(),
            method_name,
            Some(AssociationMode::WithSelf),
        ) {
            Ok(resolved) => resolved,
            Err(Some(resolved)) => {
                self.diagnostics
                    .push(InferenceDiagnostic::MethodNotInScope {
                        id: tgt_expr,
                        receiver_ty: receiver_ty.clone(),
                    });
                resolved
            }
            // A method of a type parameter is one its bounds declare:
            // `x.show()` for `x: T` and `T: Show`. It resolves to the
            // trait's declaration; monomorphisation replaces that with the
            // implementation for each type `T` is instantiated at.
            Err(None) if matches!(receiver_ty.interned(), TyKind::TypeParam(..)) => {
                let TyKind::TypeParam(param, _) = receiver_ty.interned() else {
                    unreachable!("matched above")
                };
                match crate::traits::bound_method(
                    self.db,
                    &self.bounds,
                    *param,
                    method_name,
                    AssociationMode::WithSelf,
                ) {
                    crate::traits::BoundMethod::Found(method) => method.id,
                    // Every type has a debug form, generated per
                    // instantiation.
                    crate::traits::BoundMethod::NotFound(_)
                        if matches!(method_name.as_str(), Some("debug_string" | "type_name")) =>
                    {
                        let op = if method_name.as_str() == Some("type_name") {
                            crate::StringOp::TypeName
                        } else {
                            crate::StringOp::DebugString
                        };
                        return self.infer_string_method(tgt_expr, op, &receiver_ty, args);
                    }
                    other => {
                        for &arg in args {
                            self.infer_expr(arg, &Expectation::none());
                        }
                        self.report_bound_method_error(tgt_expr, &receiver_ty, method_name, &other);
                        return error_type();
                    }
                }
            }
            // A default method of a trait the type implements, which its
            // implementation does not override: `p.ne(q)`.
            Err(None)
                if !crate::traits::default_methods(
                    self.db,
                    &receiver_ty,
                    method_name,
                    AssociationMode::WithSelf,
                )
                .is_empty() =>
            {
                let defaults = crate::traits::default_methods(
                    self.db,
                    &receiver_ty,
                    method_name,
                    AssociationMode::WithSelf,
                );
                if let [method] = defaults.as_slice() {
                    method.id
                } else {
                    for &arg in args {
                        self.infer_expr(arg, &Expectation::none());
                    }
                    let traits: Vec<Trait> = defaults
                        .iter()
                        .filter_map(|m| m.trait_container(self.db))
                        .collect();
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!(
                            "`{method_name}` is a default method of more than one trait `{}` \
                             implements ({}); this version cannot tell which one is meant",
                            receiver_ty.display(self.db),
                            crate::traits::trait_list(self.db, &traits)
                        ),
                    });
                    return error_type();
                }
            }
            Err(None) => {
                // `s.split(sep).map(f).count()`: the pieces of a string as
                // a pipeline, when the chain needs one.
                if self.starts_text_pipeline(tgt_expr, receiver) {
                    let op = method_name
                        .as_str()
                        .and_then(crate::PipelineOp::from_name)
                        .expect("a pipeline operation");
                    return self.infer_pipeline_method(
                        tgt_expr,
                        op,
                        &TyKind::String.intern(),
                        args,
                    );
                }
                // `len()` and friends on arrays and `str` are builtins.
                if let Some(ty) =
                    self.infer_builtin_method(tgt_expr, &receiver_ty, method_name, args)
                {
                    return ty;
                }

                // Check if there is a field with the same name.
                let field = self.lookup_field(receiver_ty.clone(), method_name);
                let field_with_same_name = field.as_ref().map(|(field_ty, _)| field_ty.clone());

                // A field holding a function value is called like a method:
                // `self.on_event(x)`.
                if let Some((field_ty, is_visible)) = field {
                    let field_ty = self.normalize(&field_ty);
                    if matches!(field_ty.interned(), TyKind::Function(_)) {
                        if !is_visible {
                            self.diagnostics
                                .push(InferenceDiagnostic::AccessPrivateField {
                                    id: tgt_expr,
                                    receiver_ty: receiver_ty.clone(),
                                    name: method_name.clone(),
                                });
                        }
                        self.field_calls.insert(tgt_expr);
                        return self.infer_indirect_call(tgt_expr, &field_ty, args);
                    }
                }

                // Check if there is an associated function with the same name.
                let associated_function_with_same_name = lookup_method(
                    self.db,
                    &receiver_ty,
                    self.module(),
                    method_name,
                    Some(AssociationMode::WithoutSelf),
                )
                .map_or_else(identity, Some);

                // Method could not be resolved, emit an error.
                self.diagnostics.push(InferenceDiagnostic::MethodNotFound {
                    id: tgt_expr,
                    method_name: method_name.clone(),
                    receiver_ty,
                    field_with_same_name,
                    associated_function_with_same_name,
                });
                return error_type();
            }
        };

        // Store the method resolution.
        self.method_resolution.insert(tgt_expr, resolved_function);

        self.infer_call_arguments_and_return(
            tgt_expr,
            args,
            Function::from(resolved_function).into(),
            CallReceiver::Value(receiver, receiver_ty),
        )
    }

    /// Handles `Type.assoc_fn(args)` when `receiver` is a bare path naming a
    /// type, returning the call's type, or `None` when this is an ordinary
    /// method call on a value after all.
    ///
    /// Two conditions must both hold before this claims the call, so that a
    /// genuine method call is never stolen:
    ///
    /// 1. The receiver is a bare `Expr::Path` that resolves in the *type*
    ///    namespace. A local variable, a field access or a call result never
    ///    does, so those fall straight through.
    /// 2. That type actually has an associated function of this name with no
    ///    `self` parameter (`AssociationMode::WithoutSelf`). If it only has a
    ///    method by that name, this is a user error better reported by the
    ///    ordinary path, which already knows how to say "method `f` does not
    ///    exist" and point at a field or associated function with the same
    ///    name.
    ///
    /// The receiver expression is recorded as having the named type, and the
    /// resolved function goes into `method_resolutions` exactly as a method
    /// call's would. Codegen tells the two apart by asking the *callee*
    /// whether it has a receiver, which is the only thing that actually
    /// decides whether an argument has to be passed.
    fn infer_static_member_call(
        &mut self,
        tgt_expr: ExprId,
        receiver: ExprId,
        args: &[ExprId],
        method_name: &Name,
        expected: &Expectation,
    ) -> Option<Ty> {
        let Expr::Path(path) = &self.body[receiver] else {
            return None;
        };
        let path = path.clone();

        let resolver = resolver_for_expr(self.db, self.body.owner(), tgt_expr);
        // `_fully` already rejects a path with unresolved trailing segments.
        // Visibility of the *type* is not checked here on purpose: the
        // `lookup_method` call below gates on the visibility of the
        // *function*, which is the thing actually being reached, and a type
        // that is not visible at all fails to resolve in the first place.
        let (type_ns, _visibility) = resolver.resolve_path_as_type_fully(self.db, &path)?;

        // `Shape.Circle(..)` is a variant; any other name an associated
        // function from an `extend Shape` block, if there is one.
        if let TypeNs::EnumId(id) = type_ns {
            let e = Enum::from(id);
            let is_assoc_fn = e.variant(self.db, method_name).is_none()
                && lookup_method(
                    self.db,
                    &e.ty(self.db),
                    self.module(),
                    method_name,
                    Some(AssociationMode::WithoutSelf),
                )
                .is_ok();
            if !is_assoc_fn {
                return Some(self.infer_variant_construction(
                    tgt_expr,
                    receiver,
                    e,
                    method_name,
                    Some(args),
                    expected,
                ));
            }
        }

        let type_for_def = |def| self.db.type_for_def(def, Namespace::Types);
        let self_ty = match type_ns {
            TypeNs::StructId(id) => type_for_def(TypableDef::Struct(id.into())),
            TypeNs::TypeAliasId(id) => type_for_def(TypableDef::TypeAlias(id.into())),
            TypeNs::PrimitiveType(id) => type_for_def(TypableDef::PrimitiveType(id.into())),
            TypeNs::SelfType(id) => self.db.type_for_impl_self(id),
            TypeNs::EnumId(id) => Enum::from(id).ty(self.db),
            // `T.assoc(..)`: an associated function a bound of `T`
            // declares, such as `T.default()` for `T: Defaultable`.
            TypeNs::GenericParam(param, _) => {
                let param_ty = TyKind::TypeParam(param, path.as_ident()?.clone()).intern();
                match crate::traits::bound_method(
                    self.db,
                    &self.bounds,
                    param,
                    method_name,
                    AssociationMode::WithoutSelf,
                ) {
                    crate::traits::BoundMethod::Found(method) => {
                        self.method_resolution.insert(tgt_expr, method.id);
                        return Some(self.infer_call_arguments_and_return(
                            tgt_expr,
                            args,
                            method.into(),
                            CallReceiver::TraitSelf(receiver, param_ty),
                        ));
                    }
                    other => {
                        self.set_expr_type(receiver, param_ty.clone());
                        for &arg in args {
                            self.infer_expr(arg, &Expectation::none());
                        }
                        self.report_bound_method_error(tgt_expr, &param_ty, method_name, &other);
                        return Some(error_type());
                    }
                }
            }
        };

        let resolved_function = lookup_method(
            self.db,
            &self_ty,
            self.module(),
            method_name,
            Some(AssociationMode::WithoutSelf),
        )
        .ok()?;

        // Register the resolution the same way a method call does. The
        // receiver's type is recorded by the call below, instantiated along
        // with the callee: `Box` names `Box[T]`, and which `T` is meant is
        // only known once the arguments have been checked.
        self.method_resolution.insert(tgt_expr, resolved_function);

        Some(self.infer_call_arguments_and_return(
            tgt_expr,
            args,
            Function::from(resolved_function).into(),
            CallReceiver::Type(receiver, self_ty),
        ))
    }

    /// Replaces each generic parameter in a callee's signature with a fresh
    /// inference variable, so that every call gets its own instantiation.
    ///
    /// `func first[T](list: List[T]) -> T` declares one `T`; two calls in
    /// the same body may pass `List[i32]` and `List[f64]`, and they must not
    /// be forced to agree. Handing the declared signature to the checker
    /// unchanged is what previously made `b.get()` on a `Box[i32]` report
    /// "expected i32, found T": nothing ever connected the parameter to the
    /// argument.
    ///
    /// Every parameter in the signature is instantiated regardless of owner.
    /// A callee's signature is written in its own scope, so the only
    /// parameters that can appear are its own and those of the type it is
    /// associated with -- and for an associated function, the type's
    /// parameters are exactly what the receiver has to determine.
    /// Replaces every generic parameter in a single type with a fresh
    /// inference variable.
    ///
    /// The one-type counterpart of `instantiate_signature`, for a use site
    /// that has no signature to go with it -- a record literal naming a
    /// generic struct.
    fn instantiate_type(&mut self, ty: &Ty) -> Ty {
        let (mut instantiated, _) = self.instantiate_signature(std::slice::from_ref(ty), ty);
        instantiated.pop().unwrap_or_else(|| ty.clone())
    }

    fn instantiate_signature(&mut self, params: &[Ty], ret: &Ty) -> (Vec<Ty>, Ty) {
        let (params, ret, _) = self.instantiate_signature_with_args(params, ret);
        (params, ret)
    }

    /// [`Self::instantiate_signature`], also returning which fresh variable
    /// stands for which parameter, so that the call can record its
    /// instantiation (see [`Self::record_instantiation`]).
    fn instantiate_signature_with_args(
        &mut self,
        params: &[Ty],
        ret: &Ty,
    ) -> (Vec<Ty>, Ty, FxHashMap<TypeParamId, Ty>) {
        let mut fresh: FxHashMap<TypeParamId, Ty> = FxHashMap::default();

        for ty in params.iter().chain(std::iter::once(ret)) {
            ty.walk(&mut |inner| {
                if let TyKind::TypeParam(id, _) = inner.interned() {
                    fresh.entry(*id).or_insert_with(|| {
                        // Deliberately an ordinary type variable and not an
                        // integer/float one: a parameter is unconstrained
                        // until an argument says otherwise, and guessing a
                        // numeric default here would silently pick a type.
                        self.type_variables.new_type_var()
                    });
                }
            });
        }

        if fresh.is_empty() {
            return (params.to_vec(), ret.clone(), fresh);
        }

        let apply = |ty: &Ty| {
            ty.clone().fold(&mut |inner| match inner.interned() {
                TyKind::TypeParam(id, _) => fresh.get(id).cloned().unwrap_or(inner),
                _ => inner,
            })
        };

        let params = params.iter().map(apply).collect();
        let ret = apply(ret);
        (params, ret, fresh)
    }

    /// Reports an operation applied to a value whose type is a generic
    /// parameter.
    ///
    /// A parameter stands for every type, so the only operations valid on
    /// it are the ones every type has -- moving, copying, passing,
    /// returning and storing -- and the methods its bounds declare.
    /// Operators (`a + b`, `a < b`, `-a`) are not resolved through traits
    /// yet. The message says that instead of the generic "cannot apply
    /// binary operator", which reads as if `T` were a type the operator
    /// merely does not support.
    fn report_op_on_type_param(&mut self, id: ExprId, what: &str, ty: &Ty) {
        let ty = ty.display(self.db).to_string();
        self.diagnostics.push(InferenceDiagnostic::ExprError {
            id: id.into(),
            message: format!(
                "{what} a value of generic type `{ty}`: operators on a type parameter are not \
                 supported; declare the operation as a method of a trait, bound `{ty}` by it \
                 (`{ty}: Trait`) and call the method"
            ),
        });
    }

    /// Reports a method call on a type parameter that its bounds do not
    /// resolve: no bound declares the method, or more than one does.
    fn report_bound_method_error(
        &mut self,
        id: ExprId,
        ty: &Ty,
        method_name: &Name,
        found: &crate::traits::BoundMethod,
    ) {
        let ty = ty.display(self.db).to_string();
        let message = match found {
            crate::traits::BoundMethod::NotFound(bounds) if bounds.is_empty() => format!(
                "cannot call `{method_name}` on a value of generic type `{ty}`: `{ty}` has no \
                 bounds; bound it by a trait that declares `{method_name}` (`{ty}: Trait`)"
            ),
            crate::traits::BoundMethod::NotFound(bounds) => format!(
                "no method `{method_name}` in the bounds of `{ty}` ({})",
                crate::traits::trait_list(self.db, bounds)
            ),
            crate::traits::BoundMethod::Ambiguous(traits) => format!(
                "`{method_name}` is declared by more than one bound of `{ty}` ({}); this \
                 version cannot tell which one is meant",
                crate::traits::trait_list(self.db, traits)
            ),
            crate::traits::BoundMethod::Found(_) => return,
        };
        self.diagnostics.push(InferenceDiagnostic::ExprError {
            id: id.into(),
            message,
        });
    }

    /// Records which instance of `callee` the call `call` reaches: the
    /// variable each of the callee's generic parameters was instantiated
    /// with, in the order of [`Function::generic_environment`].
    ///
    /// A parameter that appears in neither the parameters nor the result of
    /// the signature got no variable from the instantiation, so it gets one
    /// here. Nothing can ever determine it, and `resolve_all` reports it as
    /// a parameter the call does not say -- rather than code generation
    /// later finding an instance with a hole in it.
    fn record_instantiation(
        &mut self,
        call: ExprId,
        callee: Function,
        fresh: &FxHashMap<TypeParamId, Ty>,
    ) {
        let environment = callee.generic_environment(self.db);
        if environment.is_empty() {
            return;
        }
        let args = environment
            .iter()
            .map(|param| {
                fresh
                    .get(param)
                    .cloned()
                    .unwrap_or_else(|| self.type_variables.new_type_var())
            })
            .collect();
        self.instantiations.insert(call, (callee, args));
    }

    fn infer_call_arguments_and_return(
        &mut self,
        tgt_expr: ExprId,
        args: &[ExprId],
        callable: CallableDef,
        receiver: CallReceiver,
    ) -> Ty {
        // Retrieve the function signature.
        let signature = self.db.callable_sig(callable);

        // The receiver is instantiated together with the signature, through
        // the same variables, so that it can decide the parameters of the
        // type it belongs to: `b.get()` on a `Box[i32]` makes `get`'s `T`
        // `i32` only because `self: Box[T]` and the `T -> T` of the result
        // share one variable.
        let receiver_declared = match (&receiver, callable) {
            (CallReceiver::Value(..), CallableDef::Function(f)) => f.self_param_ty(self.db),
            (CallReceiver::Type(_, named), _) => Some(named.clone()),
            (CallReceiver::TraitSelf(..), CallableDef::Function(f)) => {
                f.trait_container(self.db).map(Trait::self_ty)
            }
            _ => None,
        };
        let mut instantiated: Vec<Ty> = signature.params().to_vec();
        instantiated.extend(receiver_declared.iter().cloned());
        let (mut param_tys, ret_ty, fresh) =
            self.instantiate_signature_with_args(&instantiated, signature.ret());
        let receiver_instantiated = receiver_declared.and_then(|_| param_tys.pop());

        match (receiver, receiver_instantiated) {
            (CallReceiver::Value(expr, actual), Some(expected)) => {
                if !self.unify(&expected, &actual) {
                    self.diagnostics.push(InferenceDiagnostic::MismatchedTypes {
                        expected,
                        found: actual,
                        id: expr,
                    });
                }
            }
            (CallReceiver::Type(expr, _), Some(instantiated)) => {
                self.set_expr_type(expr, instantiated);
            }
            (CallReceiver::TraitSelf(expr, named), Some(instantiated)) => {
                self.unify(&instantiated, &named);
                self.set_expr_type(expr, named);
            }
            _ => {}
        }

        if let CallableDef::Function(function) = callable {
            self.record_instantiation(tgt_expr, function, &fresh);
        }

        // Verify that the number of arguments matches
        if param_tys.len() != args.len() {
            self.diagnostics
                .push(InferenceDiagnostic::ParameterCountMismatch {
                    id: tgt_expr,
                    found: args.len(),
                    expected: param_tys.len(),
                });
        }

        // Verify the argument types
        for (&arg, param_ty) in args.iter().zip(param_tys.iter()) {
            self.infer_expr_coerce(arg, &Expectation::has_type(param_ty.clone()));
        }

        ret_ty
    }

    /// Inferences the type of a call expression.
    fn infer_call(
        &mut self,
        tgt_expr: ExprId,
        callee: ExprId,
        args: &[ExprId],
        _expected: &Expectation,
    ) -> Ty {
        if let Some(builtin) = self.resolve_builtin_callee(callee) {
            return self.infer_builtin_call(tgt_expr, builtin, args);
        }

        let callee_ty = self.infer_expr_inner(
            callee,
            &Expectation::none(),
            &CheckParams {
                is_unit_struct: false,
                is_callee: true,
            },
        );
        let callee_ty = self.normalize(&callee_ty);

        match callee_ty.interned() {
            TyKind::Function(_) => self.infer_indirect_call(tgt_expr, &callee_ty, args),
            TyKind::Struct(s, _) => {
                // Erroneously found either a unit struct or record struct literal. Record
                // struct literals can never be used as a value so that will
                // have already been reported.
                if s.data(self.db).kind == StructKind::Unit {
                    self.diagnostics
                        .push(InferenceDiagnostic::MismatchedStructLit {
                            id: tgt_expr,
                            expected: StructKind::Unit,
                            found: StructKind::Tuple,
                        });
                }

                // Still derive subtypes
                for arg in args.iter() {
                    self.infer_expr(*arg, &Expectation::none());
                }

                callee_ty
            }
            TyKind::FnDef(def, _substs) => {
                // Found either a tuple struct literal or function
                let sig = callee_ty.callable_sig(self.db).unwrap();
                let (param_tys, ret_ty, fresh) =
                    self.instantiate_signature_with_args(sig.params(), sig.ret());
                if let CallableDef::Function(function) = *def {
                    self.record_instantiation(tgt_expr, function, &fresh);
                }
                self.check_call_argument_count(
                    tgt_expr,
                    def.is_struct(),
                    args.len(),
                    param_tys.len(),
                );
                for (&arg, param_ty) in args.iter().zip(param_tys.iter()) {
                    self.infer_expr_coerce(arg, &Expectation::has_type(param_ty.clone()));
                }

                ret_ty
            }
            TyKind::Unknown => {
                // Error has already been emitted somewhere else
                error_type()
            }
            _ => {
                self.diagnostics
                    .push(InferenceDiagnostic::ExpectedFunction {
                        id: callee,
                        found: callee_ty,
                    });
                error_type()
            }
        }
    }

    /// Returns the builtin `callee` names, if it is a bare identifier that
    /// names one *and* nothing in scope defines that identifier.
    ///
    /// The scope check is what keeps builtins from breaking existing code: a
    /// program that declares its own `print` keeps calling it.
    fn resolve_builtin_callee(&mut self, callee: ExprId) -> Option<BuiltinFn> {
        let Expr::Path(path) = &self.body[callee] else {
            return None;
        };
        let builtin = BuiltinFn::from_name(path.as_ident()?)?;
        let resolver = resolver_for_expr(self.db, self.body.owner(), callee);
        if resolver.resolve_path_as_value(self.db, path).is_some() {
            return None;
        }
        Some(builtin)
    }

    /// Infers a call to a compiler builtin. See [`BuiltinFn`].
    fn infer_builtin_call(&mut self, tgt_expr: ExprId, builtin: BuiltinFn, args: &[ExprId]) -> Ty {
        self.builtin_calls.insert(tgt_expr, builtin);
        match builtin {
            BuiltinFn::Print | BuiltinFn::Println => {
                for &arg in args {
                    let ty = self.infer_expr(arg, &Expectation::none());
                    let ty = self.normalize(&ty);
                    if !is_printable(&ty) {
                        self.diagnostics.push(InferenceDiagnostic::ExprError {
                            id: arg.into(),
                            message: format!(
                                "`{}` cannot print a value of type `{}`: only integers, floats, `bool`, `str` and `String` are printable",
                                builtin.name(),
                                ty.display(self.db)
                            ),
                        });
                    }
                }
                Ty::unit()
            }
            BuiltinFn::Declassify => {
                if args.len() != 1 {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!(
                            "`declassify` takes exactly one argument, found {}",
                            args.len()
                        ),
                    });
                }
                let mut ty = error_type();
                for (idx, &arg) in args.iter().enumerate() {
                    let arg_ty = self.infer_expr(arg, &Expectation::none());
                    if idx == 0 {
                        ty = arg_ty;
                    }
                }
                ty
            }
            BuiltinFn::CtSelect => {
                if args.len() != 3 {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!(
                            "`ct_select` takes a condition and two values, found {} argument(s)",
                            args.len()
                        ),
                    });
                    for &arg in args {
                        self.infer_expr(arg, &Expectation::none());
                    }
                    return error_type();
                }
                self.infer_expr(args[0], &Expectation::has_type(TyKind::Bool.intern()));
                let ty = self.infer_expr(args[1], &Expectation::none());
                let ty = self.infer_expr(args[2], &Expectation::has_type(ty));
                let ty = self.normalize(&ty);
                if !matches!(ty.interned(), TyKind::Int(_) | TyKind::Bool) && !ty.is_unknown() {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!(
                            "`ct_select` chooses between integers or `bool`s, not `{}`",
                            ty.display(self.db)
                        ),
                    });
                }
                ty
            }
            BuiltinFn::Channel => self.infer_channel_new(tgt_expr, args),
            BuiltinFn::Cancelled => {
                if !args.is_empty() {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!("`cancelled` takes no arguments, found {}", args.len()),
                    });
                }
                for &arg in args {
                    self.infer_expr(arg, &Expectation::none());
                }
                TyKind::Bool.intern()
            }
            BuiltinFn::ArrayFilled => {
                if args.len() != 2 {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!(
                            "`array_filled` takes a value and a count, found {} argument(s)",
                            args.len()
                        ),
                    });
                    for &arg in args {
                        self.infer_expr(arg, &Expectation::none());
                    }
                    return error_type();
                }
                // Any element type: the array holds `n` copies of the value
                // (of the reference, for a class), written before its length
                // is, so the collector never sees an unwritten element.
                let elem_ty = self.infer_expr(args[0], &Expectation::none());
                self.infer_expr(
                    args[1],
                    &Expectation::has_type(TyKind::Int(IntTy::usize()).intern()),
                );
                let elem_ty = self.normalize(&elem_ty);
                TyKind::Array(elem_ty).intern()
            }
            BuiltinFn::Select => {
                if args.len() != 3 {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!(
                            "`select` takes a condition and two values, found {} argument(s)",
                            args.len()
                        ),
                    });
                    for &arg in args {
                        self.infer_expr(arg, &Expectation::none());
                    }
                    return error_type();
                }
                self.infer_expr(args[0], &Expectation::has_type(TyKind::Bool.intern()));
                let ty = self.infer_expr(args[1], &Expectation::none());
                let ty = self.infer_expr(args[2], &Expectation::has_type(ty));
                let ty = self.normalize(&ty);
                if !is_plain_scalar(&ty) && !ty.is_unknown() {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!(
                            "`select` chooses between integers, floats or `bool`s, not `{}`",
                            ty.display(self.db)
                        ),
                    });
                }
                ty
            }
            BuiltinFn::Likely | BuiltinFn::Unlikely => {
                if args.len() != 1 {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!(
                            "`{}` takes exactly one `bool`, found {} argument(s)",
                            builtin.name(),
                            args.len()
                        ),
                    });
                    for &arg in args {
                        self.infer_expr(arg, &Expectation::none());
                    }
                    return error_type();
                }
                self.infer_expr(args[0], &Expectation::has_type(TyKind::Bool.intern()));
                TyKind::Bool.intern()
            }
            BuiltinFn::Fma => {
                if args.len() != 3 {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!(
                            "`fma` takes three floats, found {} argument(s)",
                            args.len()
                        ),
                    });
                    for &arg in args {
                        self.infer_expr(arg, &Expectation::none());
                    }
                    return error_type();
                }
                let ty = self.infer_expr(args[0], &Expectation::none());
                self.infer_expr(args[1], &Expectation::has_type(ty.clone()));
                let ty = self.infer_expr(args[2], &Expectation::has_type(ty));
                let ty = self.normalize(&ty);
                if !matches!(
                    ty.interned(),
                    TyKind::Float(_) | TyKind::InferenceVar(InferTy::Float(_))
                ) && !ty.is_unknown()
                {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: tgt_expr.into(),
                        message: format!(
                            "`fma` takes `f32` or `f64` operands, not `{}`",
                            ty.display(self.db)
                        ),
                    });
                }
                ty
            }
            BuiltinFn::String(op @ (StringOp::FromUtf8 | StringOp::TakeUtf8)) => {
                self.infer_string_free_fn(tgt_expr, op, args)
            }
            BuiltinFn::ArrayLen
            | BuiltinFn::ArrayUncheckedGet
            | BuiltinFn::StrLen
            | BuiltinFn::Array(_)
            | BuiltinFn::String(_)
            | BuiltinFn::Pipeline(_) => {
                unreachable!("method builtins are resolved by `infer_builtin_method`")
            }
        }
    }

    /// Infers a builtin method call -- `len()` on an array or `str`,
    /// `unchecked_get(i)` on an array -- or returns `None` when
    /// `method_name` names no builtin method of `receiver_ty`.
    fn infer_builtin_method(
        &mut self,
        tgt_expr: ExprId,
        receiver_ty: &Ty,
        method_name: &Name,
        args: &[ExprId],
    ) -> Option<Ty> {
        let receiver_ty = self.normalize(receiver_ty);
        if let Some(op) = strings::string_method(&receiver_ty, method_name.as_str()?) {
            return Some(self.infer_string_method(tgt_expr, op, &receiver_ty, args));
        }
        let builtin = match (receiver_ty.interned(), method_name.as_str()?) {
            (TyKind::Array(elem), name) if crate::ArrayMethod::from_name(name).is_some() => {
                let method = crate::ArrayMethod::from_name(name).expect("matched above");
                let elem = elem.clone();
                return Some(self.infer_array_method(tgt_expr, method, &elem, args));
            }
            (TyKind::Iter(elem), name) if crate::PipelineOp::from_name(name).is_some() => {
                let op = crate::PipelineOp::from_name(name).expect("matched above");
                let elem = elem.clone();
                return Some(self.infer_pipeline_method(tgt_expr, op, &elem, args));
            }
            (TyKind::Array(_) | TyKind::Struct(..), "iter") => {
                return self.infer_iter_source(tgt_expr, &receiver_ty, args);
            }
            (TyKind::Array(_), "len") => BuiltinFn::ArrayLen,
            (TyKind::Array(_), "unchecked_get") => BuiltinFn::ArrayUncheckedGet,
            (TyKind::Str, "len") => BuiltinFn::StrLen,
            _ => return None,
        };
        self.builtin_calls.insert(tgt_expr, builtin);

        let expected_args = usize::from(builtin == BuiltinFn::ArrayUncheckedGet);
        if args.len() != expected_args {
            self.diagnostics
                .push(InferenceDiagnostic::ParameterCountMismatch {
                    id: tgt_expr,
                    found: args.len(),
                    expected: expected_args,
                });
        }

        Some(match builtin {
            BuiltinFn::ArrayUncheckedGet => {
                for &arg in args {
                    let index_ty = self.type_variables.new_integer_var();
                    self.infer_expr(arg, &Expectation::has_type(index_ty));
                }
                match receiver_ty.interned() {
                    TyKind::Array(elem) => elem.clone(),
                    _ => error_type(),
                }
            }
            _ => TyKind::Int(IntTy::usize()).intern(),
        })
    }

    /// Checks whether the specified struct type is a unit struct.
    fn check_unit_struct_lit(&mut self, tgt_expr: ExprId, expected: Struct) {
        let struct_data = expected.data(self.db);
        if struct_data.kind != StructKind::Unit {
            self.diagnostics
                .push(InferenceDiagnostic::MismatchedStructLit {
                    id: tgt_expr,
                    expected: struct_data.kind,
                    found: StructKind::Unit,
                });
        }
    }

    /// Checks whether the number of passed arguments matches the number of
    /// parameters of a callable definition.
    fn check_call_argument_count(
        &mut self,
        tgt_expr: ExprId,
        is_tuple_lit: bool,
        num_args: usize,
        num_params: usize,
    ) {
        if num_args != num_params {
            self.diagnostics.push(if is_tuple_lit {
                InferenceDiagnostic::FieldCountMismatch {
                    id: tgt_expr,
                    found: num_args,
                    expected: num_params,
                }
            } else {
                InferenceDiagnostic::ParameterCountMismatch {
                    id: tgt_expr,
                    found: num_args,
                    expected: num_params,
                }
            });
        }
    }

    // Checks whether the passed fields match the fields of a struct definition.
    fn check_record_lit(
        &mut self,
        tgt_expr: ExprId,
        ty: &Ty,
        expected: Struct,
        fields: &[RecordLitField],
    ) {
        let struct_data = expected.data(self.db);
        if struct_data.kind != StructKind::Record {
            self.diagnostics
                .push(InferenceDiagnostic::MismatchedStructLit {
                    id: tgt_expr,
                    expected: struct_data.kind,
                    found: StructKind::Record,
                });
            return;
        }

        let lit_fields: FxHashSet<_> = fields.iter().map(|f| &f.name).collect();
        let missed_fields: Vec<Name> = struct_data
            .fields
            .iter()
            .filter_map(|(_f, d)| {
                let name = d.name.clone();
                if lit_fields.contains(&name) {
                    None
                } else {
                    Some(name)
                }
            })
            .collect();

        if !missed_fields.is_empty() {
            self.diagnostics.push(InferenceDiagnostic::MissingFields {
                id: tgt_expr,
                struct_ty: ty.clone(),
                names: missed_fields,
            });
        }
    }

    fn resolve_assoc_item(
        &mut self,
        def: TypeNs,
        path: &Path,
        remaining_index: usize,
        id: ExprId,
    ) -> Option<ValueNs> {
        // We can only resolve the last element of the path.
        let name = if remaining_index == path.segments.len() - 1 {
            &path.segments[remaining_index]
        } else {
            return None;
        };

        // Infer the type of the definitions
        let type_for_def_fn = |def| self.db.type_for_def(def, Namespace::Types);
        let root_ty = match def {
            TypeNs::SelfType(id) => self.db.type_for_impl_self(id),
            TypeNs::StructId(id) => type_for_def_fn(TypableDef::Struct(id.into())),
            TypeNs::TypeAliasId(id) => type_for_def_fn(TypableDef::TypeAlias(id.into())),
            TypeNs::PrimitiveType(id) => type_for_def_fn(TypableDef::PrimitiveType(id.into())),
            // An enum's associated functions come from `extend` blocks; its
            // variants are reached through `Expr::Field`/`Expr::MethodCall`
            // (see `infer_variant_construction`), never through here.
            TypeNs::EnumId(id) => Enum::from(id).ty(self.db),
            // `T.method(..)` on an unconstrained type parameter needs bound
            // resolution (`where T: Trait`) to know what methods exist.
            // Bounds parse but are not yet resolved, so there is nothing to
            // look the method up in.
            TypeNs::GenericParam(..) => return None,
        };

        // Resolve the value.
        let function_id = match lookup_method(
            self.db,
            &root_ty,
            self.module(),
            name,
            Some(AssociationMode::WithoutSelf),
        ) {
            Ok(value) => value,
            Err(Some(value)) => {
                self.diagnostics
                    .push(InferenceDiagnostic::PathIsPrivate { id });
                value
            }
            _ => return None,
        };

        Some(ValueNs::FunctionId(function_id))
    }

    fn resolve_value_path_inner(
        &mut self,
        resolver: &Resolver,
        path: &Path,
        id: ExprId,
    ) -> Option<ValueNs> {
        let value_or_partial = resolver.resolve_path_as_value(self.db, path)?;
        match value_or_partial {
            ResolveValueResult::ValueNs(it, vis) => {
                if !vis.is_visible_from(self.db, self.module()) {
                    self.diagnostics
                        .push(diagnostics::InferenceDiagnostic::PathIsPrivate { id });
                }

                Some(it)
            }
            ResolveValueResult::Partial(def, remaining_index) => {
                self.resolve_assoc_item(def, path, remaining_index, id)
            }
        }
    }

    fn infer_path_expr(
        &mut self,
        resolver: &Resolver,
        path: &Path,
        id: ExprId,
        check_params: &CheckParams,
    ) -> Option<Ty> {
        if let Some(value) = self.resolve_value_path_inner(resolver, path, id) {
            // Match based on what type of value we found
            match value {
                ValueNs::ImplSelf(i) => {
                    let ty = self.db.type_for_impl_self(i);
                    Some(ty)
                }
                ValueNs::LocalBinding(pat) => {
                    self.record_local_use(pat);
                    Some(self.type_of_pat.get(pat)?.clone())
                }
                ValueNs::FunctionId(f) => {
                    let ty = self
                        .db
                        .type_for_def(TypableDef::Function(f.into()), Namespace::Values);
                    Some(ty)
                }
                ValueNs::StructId(s) => {
                    if check_params.is_unit_struct {
                        self.check_unit_struct_lit(id, s.into());
                    }
                    let ty = self
                        .db
                        .type_for_def(TypableDef::Struct(s.into()), Namespace::Values);
                    Some(ty)
                }
            }
        } else {
            // If no value was found, try to resolve the path as a type. This will always
            // result in an error but it does provide much better diagnostics.
            let ty = resolver.resolve_path_as_type_fully(self.db, path);
            if let Some((TypeNs::StructId(struct_id), _)) = ty {
                // We can only really get here if the struct is actually a record. Both other
                // types can be seen as a values because they have a constructor.
                debug_assert_eq!(
                    Struct::from(struct_id).data(self.db).kind,
                    StructKind::Record
                );

                // Should it be a unit struct?
                if check_params.is_unit_struct {
                    self.diagnostics
                        .push(InferenceDiagnostic::MismatchedStructLit {
                            id,
                            expected: StructKind::Record,
                            found: StructKind::Unit,
                        });
                } else {
                    self.diagnostics
                        .push(InferenceDiagnostic::MismatchedStructLit {
                            id,
                            expected: StructKind::Record,
                            found: StructKind::Tuple,
                        });
                }

                let ty = self
                    .db
                    .type_for_def(TypableDef::Struct(struct_id.into()), Namespace::Values);
                return Some(ty);
            }

            // If the path also cannot be resolved as type, it must be considered an invalid
            // value and there is nothing we can make of this path.
            self.diagnostics
                .push(InferenceDiagnostic::UnresolvedValue { id: id.into() });
            None
        }
    }

    fn resolve_all(mut self) -> InferenceResult {
        // FIXME resolve obligations as well (use Guidance if necessary)
        //let mut tv_stack = Vec::new();
        let mut expr_types = std::mem::take(&mut self.type_of_expr);
        for (expr, ty) in expr_types.iter_mut() {
            let was_unknown = ty.is_unknown() || contains_unknown(ty);
            let resolved = self.type_variables.resolve_ty_completely(ty.clone());
            if !was_unknown && is_ambiguous(&resolved) {
                self.report_expr_inference_failure(expr);
            }
            *ty = resolved;
        }
        let mut closures = std::mem::take(&mut self.closures);
        for closure in closures.values_mut() {
            for ty in closure
                .params
                .iter_mut()
                .chain(std::iter::once(&mut closure.ret))
            {
                *ty = self.type_variables.resolve_ty_completely(ty.clone());
            }
        }
        let mut optional_wraps = std::mem::take(&mut self.optional_wraps);
        for ty in optional_wraps.values_mut() {
            *ty = self.type_variables.resolve_ty_completely(ty.clone());
        }
        let mut propagations = std::mem::take(&mut self.propagations);
        for ty in propagations.values_mut() {
            *ty = self.type_variables.resolve_ty_completely(ty.clone());
        }
        let mut pat_types = std::mem::take(&mut self.type_of_pat);
        for (pat, ty) in pat_types.iter_mut() {
            let was_unknown = ty.is_unknown() || contains_unknown(ty);
            let resolved = self.type_variables.resolve_ty_completely(ty.clone());
            if !was_unknown && is_ambiguous(&resolved) {
                self.report_pat_inference_failure(pat);
            }
            *ty = resolved;
        }
        self.check_capture_types(&closures, &pat_types);
        self.check_array_methods(&expr_types);
        self.check_pipelines(&expr_types, &closures);
        self.check_formats(&expr_types);
        let instantiations = self.resolve_instantiations();
        InferenceResult {
            //            field_resolutions: self.field_resolutions,
            //            variant_resolutions: self.variant_resolutions,
            //            assoc_resolutions: self.assoc_resolutions,
            type_of_expr: expr_types,
            type_of_pat: pat_types,
            diagnostics: self.diagnostics,
            standard_types: InternedStandardTypes::default(),
            method_resolutions: self.method_resolution,
            builtin_calls: self.builtin_calls,
            effects: self.effects,
            closures,
            fn_values: self.fn_values,
            indirect_calls: self.indirect_calls,
            field_calls: self.field_calls,

            optional_wraps,
            string_conversions: self.string_conversions,
            variant_exprs: self.variant_exprs,
            variant_pats: self.pat_variants,
            propagations,
            propagation_wraps: self.propagation_wraps,
            instantiations,
            iterator_nexts: self.iterator_nexts,
        }
    }

    /// Resolves the arguments of every recorded instantiation, reporting a
    /// generic parameter that nothing at the call determined.
    ///
    /// Such a parameter is one the callee's signature does not mention in a
    /// way the call constrains -- `func make[T]() -> i32` called as
    /// `make()`. There is no type to generate the instance at, so it is an
    /// error here rather than a hole code generation would find later. The
    /// report is held back when the body already has errors: an argument
    /// whose own type failed to infer leaves the parameter undetermined
    /// too, and saying so again would only repeat the first error.
    fn resolve_instantiations(&mut self) -> FxHashMap<ExprId, Substitution> {
        let mut calls: Vec<_> = std::mem::take(&mut self.instantiations)
            .into_iter()
            .collect();
        calls.sort_by_key(|(call, _)| u32::from(call.into_raw()));

        let report = self.diagnostics.is_empty();
        let mut resolved_calls = FxHashMap::default();
        for (call, (callee, args)) in calls {
            let resolved: Substitution = args
                .iter()
                .map(|ty| self.type_variables.resolve_ty_completely(ty.clone()))
                .collect();
            let undetermined = resolved.iter().position(|ty| {
                let mut unknown = false;
                ty.walk(&mut |inner| unknown |= inner.is_unknown());
                unknown
            });
            if let (true, Some(index)) = (report, undetermined) {
                let name = callee
                    .generic_environment(self.db)
                    .get(index)
                    .and_then(|param| param.name(self.db))
                    .map_or_else(|| "?".to_owned(), |name| name.to_string());
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: call.into(),
                    message: format!(
                        "cannot infer the generic parameter `{name}` of `{}` at this call: \
                         nothing passed to it or expected from it says what `{name}` is",
                        callee.name(self.db)
                    ),
                });
            }
            self.check_bounds(call, callee, &resolved);
            resolved_calls.insert(call, resolved);
        }
        resolved_calls
    }

    /// Reports each argument of the call `call` of the generic `callee`
    /// that does not implement a trait `callee`'s bounds require of the
    /// parameter it stands for. Inside a generic body an argument may be
    /// one of the body's own parameters, which implements what its own
    /// bounds say.
    fn check_bounds(&mut self, call: ExprId, callee: Function, args: &Substitution) {
        let bounds = self.db.function_bounds(callee.id);
        if bounds.bounds.is_empty() {
            return;
        }
        let environment = callee.generic_environment(self.db);
        for bound in &bounds.bounds {
            let Some(arg) = environment
                .iter()
                .position(|param| *param == bound.param)
                .and_then(|index| args.get(index))
            else {
                continue;
            };
            if crate::traits::implements(self.db, arg, bound.trait_, &self.bounds) {
                continue;
            }
            let param = bound
                .param
                .name(self.db)
                .map_or_else(|| "?".to_owned(), |name| name.to_string());
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: call.into(),
                message: format!(
                    "the type `{}` does not implement the trait `{}`, which `{}` requires of \
                     its parameter `{param}`",
                    arg.display(self.db),
                    bound.trait_.name(self.db),
                    callee.name(self.db)
                ),
            });
        }
    }

    fn infer_block(
        &mut self,
        block: ExprId,
        statements: &[Statement],
        tail: Option<ExprId>,
        expected: &Expectation,
    ) -> Ty {
        let mut diverges = false;
        // Ghost lets sit outside `statements` (see `BodyContracts`), tagged
        // with the index of the statement they precede; they are checked in
        // that position so they see the same bindings an ordinary `let`
        // there would.
        let ghosts = self.body.contracts().ghost_lets.get(&block);
        let ghosts_at = |idx: usize| {
            ghosts
                .into_iter()
                .flatten()
                .filter(move |(pos, _)| *pos == idx)
                .map(|(_, stmt)| stmt)
        };
        let ordered: Vec<&Statement> = statements
            .iter()
            .enumerate()
            .flat_map(|(idx, stmt)| ghosts_at(idx).chain(std::iter::once(stmt)))
            .chain(ghosts_at(statements.len()))
            .collect();
        for stmt in ordered {
            match stmt {
                Statement::Let {
                    pat,
                    type_ref,
                    initializer,
                } => {
                    let decl_ty = type_ref
                        .as_ref()
                        .map_or_else(error_type, |tr| self.resolve_type(*tr));
                    //let decl_ty = self.insert_type_vars(decl_ty);
                    let ty = if let Some(expr) = initializer {
                        self.infer_expr_coerce(*expr, &Expectation::has_type(decl_ty))
                    } else {
                        decl_ty
                    };

                    let ty = self.resolve_ty_as_far_as_possible(ty);
                    self.check_irrefutable(*pat, "a `let` pattern");
                    self.infer_pat(*pat, ty);
                }
                Statement::Expr(expr) => {
                    let ty = self.infer_expr(*expr, &Expectation::none());
                    if self.is_never(&ty) {
                        diverges = true;
                    }
                }
            }
        }
        let ty = if let Some(expr) = tail {
            // Perform coercion of the trailing expression unless the expression has a Never
            // return type because we want the block to get the Never type in
            // that case.
            let ty = self.infer_expr_inner(expr, expected, &CheckParams::default());
            if self.is_never(&ty) {
                ty
            } else {
                self.coerce_expr_ty(expr, ty, expected)
            }
        } else {
            Ty::unit()
        };

        if diverges {
            TyKind::Never.intern()
        } else {
            ty
        }
    }

    fn infer_break(&mut self, tgt_expr: ExprId, expr: Option<ExprId>) -> Ty {
        let expected = match &self.active_loop {
            Some(ActiveLoop::Loop(_, info)) => info.clone(),
            Some(_) => {
                if expr.is_some() {
                    self.diagnostics
                        .push(InferenceDiagnostic::BreakWithValueOutsideLoop { id: tgt_expr });
                }
                return TyKind::Never.intern();
            }
            None => {
                self.diagnostics
                    .push(InferenceDiagnostic::BreakOutsideLoop { id: tgt_expr });
                return TyKind::Never.intern();
            }
        };

        // Infer the type of the break expression
        let ty = if let Some(expr) = expr {
            self.infer_expr_inner(expr, &expected, &CheckParams::default())
        } else {
            Ty::unit()
        };

        // Verify that it matches what we expected
        let ty = if self.unify(&ty, &expected.ty) {
            ty
        } else {
            self.diagnostics.push(InferenceDiagnostic::MismatchedTypes {
                expected: expected.ty.clone(),
                found: ty,
                id: tgt_expr,
            });
            expected.ty
        };

        // Update the expected type for the rest of the loop
        self.active_loop = Some(ActiveLoop::Loop(ty.clone(), Expectation::has_type(ty)));

        TyKind::Never.intern()
    }

    fn infer_loop_expr(&mut self, tgt_expr: ExprId, body: ExprId, expected: &Expectation) -> Ty {
        self.infer_loop_contract(tgt_expr);
        if let ActiveLoop::Loop(ty, _) = self.infer_loop_block(
            body,
            ActiveLoop::Loop(TyKind::Never.intern(), expected.clone()),
        ) {
            ty
        } else {
            panic!("returned active loop must be a loop")
        }
    }

    fn infer_loop_block(&mut self, body: ExprId, lp: ActiveLoop) -> ActiveLoop {
        let top_level_loop = self.active_loop.replace(lp);

        // Infer the body of the loop
        self.infer_expr_coerce(body, &Expectation::has_type(Ty::unit()));

        // Take the result of the loop information and replace with top level loop
        std::mem::replace(&mut self.active_loop, top_level_loop).unwrap()
    }

    fn infer_while_expr(
        &mut self,
        tgt_expr: ExprId,
        condition: ExprId,
        body: ExprId,
        _expected: &Expectation,
    ) -> Ty {
        self.infer_expr(condition, &Expectation::has_type(TyKind::Bool.intern()));
        self.infer_loop_contract(tgt_expr);
        self.infer_loop_block(body, ActiveLoop::While);
        Ty::unit()
    }

    /// `==` and `!=` on optionals: equal when both are `nil`, or both are
    /// present with equal payloads. Ordering optionals means nothing, and
    /// comparing payloads is only possible for scalar payloads; comparing
    /// with `nil` works for any payload.
    fn check_optional_comparison(
        &mut self,
        tgt_expr: ExprId,
        op: BinaryOp,
        lhs_ty: &Ty,
        rhs: ExprId,
    ) {
        let BinaryOp::CmpOp(cmp) = op else {
            return;
        };
        let lhs_ty = self.normalize(lhs_ty);
        if let crate::CmpOp::Ord { .. } = cmp {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: tgt_expr.into(),
                message: format!(
                    "optionals (`{}`) are not ordered; compare them with `==`/`!=` or unwrap them first",
                    lhs_ty.display(self.db)
                ),
            });
            return;
        }
        let rhs_is_nil = matches!(self.body[rhs], Expr::Literal(Literal::Nil));
        let TyKind::Optional(payload) = lhs_ty.interned() else {
            return;
        };
        let payload = self.normalize(payload);
        let scalar = matches!(
            payload.interned(),
            TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Unknown
                | TyKind::InferenceVar(InferTy::Int(_) | InferTy::Float(_))
        );
        if !rhs_is_nil && !scalar {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: tgt_expr.into(),
                message: format!(
                    "`{}` values cannot be compared with `==`; compare the optional with `nil` instead",
                    payload.display(self.db)
                ),
            });
        }
    }

    /// Infers a `match` (or an `if let`/`while let`, which lower to one).
    ///
    /// Every arm's pattern is checked against the scrutinee's type and binds
    /// its names; the arms' values merge like `if` branches. A `match`
    /// written as such must also be exhaustive.
    fn infer_match(
        &mut self,
        tgt_expr: ExprId,
        scrutinee: ExprId,
        arms: &[crate::expr::MatchArm],
        source: crate::expr::MatchSource,
        expected: &Expectation,
    ) -> Ty {
        let scrutinee_ty = self.infer_expr(scrutinee, &Expectation::none());
        let scrutinee_ty = self.normalize(&scrutinee_ty);
        for arm in arms {
            self.infer_match_pat(arm.pat, &scrutinee_ty);
        }

        let mut result: Option<Ty> = None;
        for arm in arms {
            let arm_ty = self.infer_expr_coerce(arm.expr, expected);
            if self.is_never(&arm_ty) {
                continue;
            }
            result = Some(match result {
                None => arm_ty,
                Some(previous) => {
                    if let Some(merged) = self.coerce_merge_branch(&previous, &arm_ty) {
                        merged
                    } else {
                        self.diagnostics.push(InferenceDiagnostic::ExprError {
                            id: arm.expr.into(),
                            message: format!(
                                "`match` arms have incompatible types: `{}` and `{}`",
                                previous.display(self.db),
                                arm_ty.display(self.db)
                            ),
                        });
                        previous
                    }
                }
            });
        }

        if source == crate::expr::MatchSource::Match {
            let pats: Vec<PatId> = arms.iter().map(|arm| arm.pat).collect();
            if let Some(missing) = self.missing_pattern(&pats, &scrutinee_ty) {
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: tgt_expr.into(),
                    message: format!(
                        "`match` is not exhaustive: {missing} is not covered; add an arm for it, or a `_ ->` arm"
                    ),
                });
            }
        }

        // No arm completes (or there are none): the match diverges.
        result.unwrap_or_else(|| TyKind::Never.intern())
    }

    /// Checks `pat` against a value of type `ty`, recording the type of the
    /// pattern and of every name it binds.
    ///
    /// Against an optional, `nil` and `_` match the optional itself, and
    /// every other pattern matches the *payload* of a present value -- so
    /// `if let x = maybe` binds the unwrapped value, and `match maybe { nil
    /// -> .., x -> .. }` covers both cases.
    fn infer_match_pat(&mut self, pat: PatId, ty: &Ty) {
        let ty = self.normalize(ty);
        let pattern = self.body[pat].clone();
        if let TyKind::Optional(payload) = ty.interned() {
            match pattern {
                Pat::Wild | Pat::Missing | Pat::Literal(Literal::Nil) => {
                    self.set_pat_type(pat, ty.clone());
                }
                _ => self.infer_match_pat(pat, &payload.clone()),
            }
            return;
        }

        match pattern {
            Pat::Bind { .. } | Pat::Wild | Pat::Missing => self.set_pat_type(pat, ty),
            Pat::Literal(literal) => {
                let literal_ty = match &literal {
                    Literal::Bool(_) => TyKind::Bool.intern(),
                    Literal::String(_) => TyKind::Str.intern(),
                    Literal::Int(LiteralInt {
                        kind: LiteralIntKind::Suffixed(suffix),
                        ..
                    }) => TyKind::Int(IntTy {
                        bitness: suffix.bitness,
                        signedness: suffix.signedness,
                    })
                    .intern(),
                    Literal::Int(_) => self.type_variables.new_integer_var(),
                    Literal::Float(LiteralFloat {
                        kind: LiteralFloatKind::Suffixed(suffix),
                        ..
                    }) => TyKind::Float(FloatTy {
                        bitness: suffix.bitness,
                    })
                    .intern(),
                    Literal::Float(_) => self.type_variables.new_float_var(),
                    Literal::Nil => {
                        self.diagnostics.push(InferenceDiagnostic::ExprError {
                            id: pat.into(),
                            message: format!(
                                "`nil` only matches an optional (`T?`), not `{}`",
                                ty.display(self.db)
                            ),
                        });
                        self.set_pat_type(pat, ty);
                        return;
                    }
                };
                if !self.unify(&literal_ty, &ty) {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: pat.into(),
                        message: format!(
                            "this pattern is a `{}`, but the value it matches is a `{}`",
                            literal_ty.display(self.db),
                            ty.display(self.db)
                        ),
                    });
                }
                self.set_pat_type(pat, ty);
            }
            Pat::Tuple(args) => {
                match ty.interned() {
                    TyKind::Tuple(arity, substs) if *arity == args.len() => {
                        let substs = substs.clone();
                        for (&arg, elem_ty) in args.iter().zip(substs.iter()) {
                            self.infer_match_pat(arg, elem_ty);
                        }
                    }
                    TyKind::Unknown => {
                        for &arg in &args {
                            self.infer_match_pat(arg, &ty);
                        }
                    }
                    _ => self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: pat.into(),
                        message: format!(
                            "a tuple pattern of {} elements cannot match a value of type `{}`",
                            args.len(),
                            ty.display(self.db)
                        ),
                    }),
                }
                self.set_pat_type(pat, ty);
            }
            Pat::Path(path) => self.infer_variant_pat(pat, Some(&path), &[], false, &ty),
            Pat::TupleStruct { path, args } => {
                self.infer_variant_pat(pat, path.as_ref(), &args, true, &ty);
            }
        }
    }

    /// The enum `receiver` names, when it is a bare path that resolves to
    /// an enum *type* -- the `Shape` of `Shape.Circle(..)`/`Shape.Point`.
    /// A local variable never resolves as a type, so `value.field` is never
    /// taken for a variant.
    fn enum_named_by(&self, tgt_expr: ExprId, receiver: ExprId) -> Option<Enum> {
        let Expr::Path(path) = &self.body[receiver] else {
            return None;
        };
        let resolver = resolver_for_expr(self.db, self.body.owner(), tgt_expr);
        if resolver
            .resolve_path_as_value_fully(self.db, path)
            .is_some()
        {
            return None;
        }
        match resolver.resolve_path_as_type_fully(self.db, path)? {
            (TypeNs::EnumId(id), _) => Some(id.into()),
            _ => None,
        }
    }

    /// Infers `Shape.Circle(args)` (`args` is `Some`) or `Shape.Point`
    /// (`None`): the value of the variant called `name` of `e`. The payload
    /// is checked against the variant's field types, and the expression is
    /// recorded for code generation.
    fn infer_variant_construction(
        &mut self,
        tgt_expr: ExprId,
        receiver: ExprId,
        e: Enum,
        name: &Name,
        args: Option<&[ExprId]>,
        expected: &Expectation,
    ) -> Ty {
        // `Result.Ok(1)` names `Result`, which is `Result[T, E]` with the
        // enum's own parameters; each construction gets fresh variables for
        // them, which the payload and the expected type then decide
        // (`Result.Err(e)` returned from a `-> Result[i64, str]` learns its
        // `T` only from the return type).
        let enum_ty = self.instantiate_type(&e.ty(self.db));
        if matches!(self.normalize(&expected.ty).interned(), TyKind::Enum(other, _) if *other == e)
        {
            self.unify(&enum_ty, &expected.ty);
        }
        let enum_args = crate::mono::enum_args(&enum_ty);
        // The receiver names the type; give it that type so no later stage
        // sees `Unknown` there.
        self.set_expr_type(receiver, enum_ty.clone());

        let Some(variant) = e.variant(self.db, name) else {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: tgt_expr.into(),
                message: format!("enum `{}` has no variant `{name}`", e.name(self.db)),
            });
            for &arg in args.unwrap_or_default() {
                self.infer_expr(arg, &Expectation::none());
            }
            return error_type();
        };
        let field_tys = variant.field_types_at(self.db, &enum_args);
        let shown = variant.display(self.db).to_string();
        match (args, variant.kind(self.db)) {
            (None, VariantKind::Unit) | (Some(_), VariantKind::Tuple | VariantKind::Record)
                if args.map_or(0, <[ExprId]>::len) == field_tys.len() => {}
            (None, _) => {
                let blanks = vec!["_"; field_tys.len()].join(", ");
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: tgt_expr.into(),
                    message: format!(
                        "`{shown}` carries a payload: construct it with `{shown}({blanks})` \
                         (variants are not function values)"
                    ),
                });
            }
            (Some(_), VariantKind::Unit) => {
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: tgt_expr.into(),
                    message: format!("`{shown}` has no payload: write it without parentheses"),
                });
            }
            (Some(args), _) => {
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: tgt_expr.into(),
                    message: format!(
                        "`{shown}` takes {} payload value{}, but {} {} given",
                        field_tys.len(),
                        if field_tys.len() == 1 { "" } else { "s" },
                        args.len(),
                        if args.len() == 1 { "was" } else { "were" },
                    ),
                });
            }
        }
        for (idx, &arg) in args.unwrap_or_default().iter().enumerate() {
            match field_tys.get(idx) {
                Some(field_ty) => {
                    self.infer_expr_coerce(arg, &Expectation::has_type(field_ty.clone()));
                }
                None => {
                    self.infer_expr(arg, &Expectation::none());
                }
            }
        }
        self.variant_exprs.insert(tgt_expr, variant);
        enum_ty
    }

    /// Resolves `Shape.Circle` in a pattern to the variant it names: every
    /// segment but the last must name an enum type, and the last one of its
    /// variants.
    fn resolve_variant_path(&self, path: &Path) -> Result<EnumVariant, String> {
        let not_a_variant = || {
            let written: Vec<String> = path.segments.iter().map(ToString::to_string).collect();
            format!(
                "`{}` is not an enum variant: a variant pattern is written `Enum.Variant`",
                written.join(".")
            )
        };
        let Some((last, prefix)) = path.segments.split_last() else {
            return Err(not_a_variant());
        };
        if prefix.is_empty() {
            return Err(not_a_variant());
        }
        let enum_path = Path {
            kind: path.kind.clone(),
            segments: prefix.to_vec(),
        };
        let e: Enum = match self
            .resolver
            .resolve_path_as_type_fully(self.db, &enum_path)
        {
            Some((TypeNs::EnumId(id), _)) => id.into(),
            _ => return Err(not_a_variant()),
        };
        e.variant(self.db, last)
            .ok_or_else(|| format!("enum `{}` has no variant `{last}`", e.name(self.db)))
    }

    /// Checks the variant pattern `pat` -- `Shape.Point` (`has_parens`
    /// false) or `Shape.Circle(args)` -- against a value of type `ty`.
    fn infer_variant_pat(
        &mut self,
        pat: PatId,
        path: Option<&Path>,
        args: &[PatId],
        has_parens: bool,
        ty: &Ty,
    ) {
        self.set_pat_type(pat, ty.clone());
        let resolved = match path {
            Some(path) => self.resolve_variant_path(path),
            // A pattern with no path is a syntax error, already reported.
            None => Err(String::new()),
        };
        let variant = match resolved {
            Ok(variant) => variant,
            Err(message) => {
                if !message.is_empty() {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: pat.into(),
                        message,
                    });
                }
                for &arg in args {
                    self.infer_match_pat(arg, &error_type());
                }
                return;
            }
        };
        // The value's type decides the payload types: in a `Result[i64,
        // str]`, `Result.Ok(n)` binds an `i64`. A value whose type is not
        // known yet is given the enum's type, with fresh arguments.
        let enum_args = match self.normalize(ty).interned().clone() {
            TyKind::Enum(e, args) if e == variant.parent() => args,
            TyKind::Unknown => Substitution::empty(),
            TyKind::InferenceVar(InferTy::Type(_)) => {
                let fresh = self.instantiate_type(&variant.parent().ty(self.db));
                self.unify(&fresh, ty);
                crate::mono::enum_args(&fresh)
            }
            _ => {
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: pat.into(),
                    message: format!(
                        "this pattern is a `{}` variant, but the value it matches is a `{}`",
                        variant.parent().name(self.db),
                        ty.display(self.db)
                    ),
                });
                for &arg in args {
                    self.infer_match_pat(arg, &error_type());
                }
                return;
            }
        };
        let shown = variant.display(self.db).to_string();
        let field_tys = variant.field_types_at(self.db, &enum_args);
        let unit = variant.kind(self.db) == VariantKind::Unit;
        if has_parens && unit {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: pat.into(),
                message: format!("`{shown}` has no payload: match it without parentheses"),
            });
        } else if !has_parens && !unit {
            let blanks = vec!["_"; field_tys.len()].join(", ");
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: pat.into(),
                message: format!("`{shown}` carries a payload: match it with `{shown}({blanks})`"),
            });
        } else if has_parens && args.len() != field_tys.len() {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: pat.into(),
                message: format!(
                    "`{shown}` has {} payload field{}, but this pattern has {}",
                    field_tys.len(),
                    if field_tys.len() == 1 { "" } else { "s" },
                    args.len()
                ),
            });
        }
        for (idx, &arg) in args.iter().enumerate() {
            let field_ty = field_tys.get(idx).cloned().unwrap_or_else(error_type);
            self.infer_match_pat(arg, &field_ty);
        }
        self.pat_variants.insert(pat, variant);
    }

    /// `==` and `!=` on enums: equal when the variants are and their
    /// payloads are. Only enums whose payloads are all numbers or `bool`s
    /// compare (see `Enum::is_comparable`); enums are not ordered.
    fn check_enum_comparison(
        &mut self,
        tgt_expr: ExprId,
        op: BinaryOp,
        e: Enum,
        args: &Substitution,
    ) {
        let args: Substitution = args.iter().map(|arg| self.normalize(arg)).collect();
        let BinaryOp::CmpOp(cmp) = op else {
            return;
        };
        let name = e.name(self.db);
        if let crate::CmpOp::Ord { .. } = cmp {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: tgt_expr.into(),
                message: format!(
                    "enums (`{name}`) are not ordered; compare them with `==`/`!=` or `match` on them"
                ),
            });
        } else if !e.is_comparable(self.db, &args, |ty| {
            matches!(
                ty.interned(),
                TyKind::InferenceVar(InferTy::Int(_) | InferTy::Float(_))
            )
        }) {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: tgt_expr.into(),
                message: format!(
                    "`==` compares only enums whose payloads are all numbers or `bool`s, and `{name}` has others; `match` on it instead"
                ),
            });
        }
    }

    /// Reports a refutable pattern where one must always match: a `let` or
    /// a parameter.
    fn check_irrefutable(&mut self, pat: PatId, place: &str) {
        if !self.is_irrefutable(pat) {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: pat.into(),
                message: format!(
                    "{place} must always match: use a name, `_` or a tuple of those, and `if let` or `match` for a pattern that may not"
                ),
            });
        }
    }

    /// Infers the two bounds of a range, which must be the same integer
    /// type, and returns that type.
    fn infer_range_bounds(&mut self, start: ExprId, end: ExprId) -> Ty {
        let start_ty = self.infer_expr(start, &Expectation::none());
        let end_ty = self.infer_expr_coerce(end, &Expectation::has_type(start_ty.clone()));
        let elem_ty = self.normalize(&end_ty);
        match elem_ty.interned() {
            TyKind::Int(_) | TyKind::InferenceVar(InferTy::Int(_)) | TyKind::Unknown => elem_ty,
            _ => {
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: start.into(),
                    message: format!(
                        "a range's bounds must be integers, found `{}`",
                        elem_ty.display(self.db)
                    ),
                });
                error_type()
            }
        }
    }

    /// Infers `for pat in iterable { body }`.
    ///
    /// The iterable is a range, whose element is its bounds' integer type,
    /// an array, whose element is the array's, a lazy pipeline (`Iter[T]`)
    /// or an iterator object (a class with `next(self) -> T?`; see
    /// `pipelines`). The pattern must be irrefutable -- a binding, `_` or a
    /// tuple of those -- because there is no "skip this element" path for a
    /// pattern that fails to match.
    fn infer_for_expr(
        &mut self,
        tgt_expr: ExprId,
        pat: PatId,
        iterable: ExprId,
        body: ExprId,
    ) -> Ty {
        let elem_ty = if let Expr::Range { start, end, .. } = self.body[iterable] {
            self.infer_range_bounds(start, end)
        } else {
            let iterable_ty = self.infer_expr(iterable, &Expectation::none());
            let iterable_ty = self.normalize(&iterable_ty);
            match iterable_ty.interned() {
                TyKind::Array(elem) | TyKind::Iter(elem) => elem.clone(),
                TyKind::Unknown => error_type(),
                _ => match self.infer_iterator_object(tgt_expr, &iterable_ty) {
                    pipelines::IteratorObject::Yields(elem) => elem,
                    pipelines::IteratorObject::Refused => error_type(),
                    pipelines::IteratorObject::No => {
                        self.diagnostics.push(InferenceDiagnostic::ExprError {
                            id: iterable.into(),
                            message: format!(
                                "cannot iterate over a value of type `{}`: a `for` loop iterates over a range (`a..b`, `a..=b`), an array, a pipeline (`xs.iter().map(f)`) or an object of a class with a method `next(self) -> T?`",
                                iterable_ty.display(self.db)
                            ),
                        });
                        error_type()
                    }
                },
            }
        };

        if !self.is_irrefutable(pat) {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: pat.into(),
                message:
                    "a `for` loop's pattern must always match: use a name, `_` or a tuple of those"
                        .to_string(),
            });
        }
        self.infer_pat(pat, elem_ty);
        self.infer_loop_block(body, ActiveLoop::For);
        Ty::unit()
    }

    /// Whether `pat` matches every value of its type.
    fn is_irrefutable(&self, pat: PatId) -> bool {
        match &self.body[pat] {
            Pat::Bind { .. } | Pat::Wild | Pat::Missing => true,
            Pat::Tuple(args) => args.iter().all(|&arg| self.is_irrefutable(arg)),
            Pat::Path(_) | Pat::Literal(_) | Pat::TupleStruct { .. } => false,
        }
    }

    /// Records that inference could not determine the type of `pat`.
    ///
    /// This used to panic, on the reasoning that only integer and float
    /// variables existed and both have a fallback. Generic instantiation
    /// broke that: a type parameter no call constrains resolves to nothing,
    /// and there is no default that would be right. An ambiguous type is an
    /// ordinary thing for a program to contain and a diagnostic to report --
    /// crashing the compiler over it would take the whole rest of the file's
    /// diagnostics down with it.
    pub fn report_pat_inference_failure(&mut self, pat: PatId) {
        self.diagnostics
            .push(InferenceDiagnostic::TypeAnnotationNeeded {
                id: ExprOrPatId::PatId(pat),
            });
    }

    /// Records that inference could not determine the type of `expr`.
    ///
    /// See `report_pat_inference_failure` for why this reports rather than
    /// panics.
    pub fn report_expr_inference_failure(&mut self, expr: ExprId) {
        self.diagnostics
            .push(InferenceDiagnostic::TypeAnnotationNeeded {
                id: ExprOrPatId::ExprId(expr),
            });
    }
}

/// Whether a value of type `ty` can be passed to `print`/`println`.
///
/// Every scalar the runtime can format: integers of any width and
/// signedness, both float widths, `bool` and `str`. An integer or float
/// literal whose type is not pinned down yet is printable too -- it defaults
/// to `i32`/`f64` -- and an unknown type is accepted so that an error already
/// reported elsewhere is not reported a second time.
fn is_printable(ty: &Ty) -> bool {
    matches!(
        ty.interned(),
        TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Bool
            | TyKind::Str
            | TyKind::String
            | TyKind::Unknown
            | TyKind::InferenceVar(InferTy::Int(_) | InferTy::Float(_))
    )
}

/// Whether `ty` is an integer, float or `bool` (a literal's not yet defaulted
/// type included): what `array_filled` and `select` handle.
fn is_plain_scalar(ty: &Ty) -> bool {
    matches!(
        ty.interned(),
        TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Bool
            | TyKind::InferenceVar(InferTy::Int(_) | InferTy::Float(_))
    )
}

/// Whether a type that resolved to `ty` was left undetermined: the unknown
/// type itself, or an optional of it -- `let x = nil` with nothing to say
/// what `x` holds. Deliberately not "contains unknown anywhere": a generic
/// instantiation nothing pinned down already reports where it matters.
fn is_ambiguous(ty: &Ty) -> bool {
    match ty.interned() {
        TyKind::Optional(inner) => inner.is_unknown(),
        _ => ty.is_unknown(),
    }
}

/// Whether `ty` is, or contains, the unknown type.
fn contains_unknown(ty: &Ty) -> bool {
    let mut found = false;
    ty.walk(&mut |inner| found |= inner.is_unknown());
    found
}

/// Returns a type used for errors
fn error_type() -> Ty {
    TyKind::Unknown.intern()
}

/// Constructs an integer inference variable of the shape an unsuffixed
/// integer literal produces.
///
/// Test-only: the variable is not registered with any `TypeVariableTable`, so
/// it must never be unified or resolved. It exists so that sibling modules
/// (notably `crate::ty::cast`) can unit-test how they treat a literal whose
/// width is not pinned down yet without standing up a whole inference run.
#[cfg(test)]
pub(super) fn test_integer_var() -> Ty {
    TyKind::InferenceVar(InferTy::Int(type_variable::TypeVarId(0))).intern()
}

/// When inferring an expression, we propagate downward whatever type hint we
/// are able in the form of an `Expectation`.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Expectation {
    ty: Ty,
    // FIXME: In some cases, we need to be aware whether the expectation is that
    // the type match exactly what we passed, or whether it just needs to be
    // coercible to the expected type. See Expectation::rvalue_hint in rustc.
}

impl Expectation {
    /// The expectation that the type of the expression needs to equal the given
    /// type.
    fn has_type(ty: Ty) -> Self {
        Expectation { ty }
    }

    /// This expresses no expectation on the type.
    fn none() -> Self {
        Expectation {
            ty: TyKind::Unknown.intern(),
        }
    }

    fn is_none(&self) -> bool {
        self.ty.is_unknown()
    }
}

/// Parameters for toggling validation checks.
struct CheckParams {
    /// Checks whether a `Expr::Path` of type struct, is actually a unit struct
    is_unit_struct: bool,
    /// The expression is the callee of a call, so a path naming a function
    /// keeps its `FnDef` type instead of becoming a function value.
    is_callee: bool,
}

impl Default for CheckParams {
    fn default() -> Self {
        Self {
            is_unit_struct: true,
            is_callee: false,
        }
    }
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub(crate) enum ExprOrPatId {
    ExprId(ExprId),
    PatId(PatId),
}

impl From<ExprId> for ExprOrPatId {
    fn from(e: ExprId) -> Self {
        ExprOrPatId::ExprId(e)
    }
}

impl From<PatId> for ExprOrPatId {
    fn from(p: PatId) -> Self {
        ExprOrPatId::PatId(p)
    }
}

mod diagnostics {
    use crate::{
        code_model::{src::HasSource, StructKind},
        diagnostics::{
            AccessUnknownField, BreakOutsideLoop, BreakWithValueOutsideLoop, CannotApplyBinaryOp,
            CannotApplyUnaryOp, CyclicType, DiagnosticSink, ExpectedFunction, ExprError,
            FieldCountMismatch, IncompatibleBranch, InvalidCast, InvalidLhs,
            InvalidTupleDestructure, LiteralOutOfRange, MethodNotFound, MethodNotInScope,
            MismatchedStructLit, MismatchedType, MissingElseBranch, MissingFields, NoFields,
            NoSuchField, ParameterCountMismatch, PrivateAccess, ReturnMissingExpression,
            TypeAnnotationNeeded, UnresolvedType, UnresolvedValue,
        },
        ids::FunctionId,
        ty::{cast::InvalidCastReason, infer::ExprOrPatId},
        type_ref::LocalTypeRefId,
        ExprId, Function, HirDatabase, IntTy, Name, PatId, Ty,
    };

    #[derive(Debug, PartialEq, Eq, Clone)]
    pub(crate) enum InferenceDiagnostic {
        UnresolvedValue {
            id: ExprOrPatId,
        },
        UnresolvedType {
            id: LocalTypeRefId,
        },
        /// `dual T` where `T` is not a session type.
        DualOfNonSession {
            id: LocalTypeRefId,
        },
        /// A trait named where a type is expected.
        TraitAsType {
            id: LocalTypeRefId,
        },
        CyclicType {
            id: LocalTypeRefId,
        },
        ExpectedFunction {
            id: ExprId,
            found: Ty,
        },
        TypeAnnotationNeeded {
            id: ExprOrPatId,
        },
        ParameterCountMismatch {
            id: ExprId,
            found: usize,
            expected: usize,
        },
        MismatchedTypes {
            id: ExprId,
            expected: Ty,
            found: Ty,
        },
        IncompatibleBranches {
            id: ExprId,
            then_ty: Ty,
            else_ty: Ty,
        },
        MissingElseBranch {
            id: ExprId,
            then_ty: Ty,
        },
        CannotApplyBinaryOp {
            id: ExprId,
            lhs: Ty,
            rhs: Ty,
        },
        CannotApplyUnaryOp {
            id: ExprId,
            ty: Ty,
        },
        InvalidCast {
            id: ExprId,
            from: Ty,
            to: Ty,
            reason: InvalidCastReason,
        },
        InvalidLhs {
            id: ExprId,
            lhs: ExprId,
        },
        ReturnMissingExpression {
            id: ExprId,
        },
        BreakOutsideLoop {
            id: ExprId,
        },
        BreakWithValueOutsideLoop {
            id: ExprId,
        },
        AccessUnknownField {
            id: ExprId,
            receiver_ty: Ty,
            name: Name,
        },
        AccessPrivateField {
            id: ExprId,
            receiver_ty: Ty,
            name: Name,
        },
        FieldCountMismatch {
            id: ExprId,
            found: usize,
            expected: usize,
        },
        MissingFields {
            id: ExprId,
            struct_ty: Ty,
            names: Vec<Name>,
        },
        MismatchedStructLit {
            id: ExprId,
            expected: StructKind,
            found: StructKind,
        },
        InvalidTupleDestructure {
            id: PatId,
            found: Ty,
            arity: usize,
            expected_arity: Option<usize>,
        },
        NoFields {
            id: ExprId,
            found: Ty,
        },
        NoSuchField {
            id: ExprId,
            field: usize,
        },
        LiteralOutOfRange {
            id: ExprId,
            literal_ty: IntTy,
        },
        TypeIsPrivate {
            id: LocalTypeRefId,
        },
        PathIsPrivate {
            id: ExprId,
        },
        MethodNotInScope {
            id: ExprId,
            receiver_ty: Ty,
        },
        MethodNotFound {
            id: ExprId,
            method_name: Name,
            receiver_ty: Ty,
            field_with_same_name: Option<Ty>,
            associated_function_with_same_name: Option<FunctionId>,
        },
        /// A free-form error attached to an expression or pattern; see
        /// `crate::diagnostics::ExprError`.
        ExprError {
            id: ExprOrPatId,
            message: String,
        },
    }

    impl InferenceDiagnostic {
        pub(crate) fn add_to(
            &self,
            db: &dyn HirDatabase,
            owner: Function,
            sink: &mut DiagnosticSink<'_>,
        ) {
            let file = owner.source(db).file_id;
            let body = owner.body_source_map(db);
            match self {
                InferenceDiagnostic::UnresolvedValue { id } => {
                    let expr = match id {
                        ExprOrPatId::ExprId(id) => body.expr_syntax(*id).map(|ptr| {
                            ptr.value
                                .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
                        }),
                        ExprOrPatId::PatId(id) => body.pat_syntax(*id).map(|ptr| {
                            ptr.value
                                .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
                        }),
                    }
                    .unwrap();

                    sink.push(UnresolvedValue { file, expr });
                }
                InferenceDiagnostic::TypeAnnotationNeeded { id } => {
                    // Unlike most diagnostics this one can point at a node
                    // the source map has no entry for -- a compiler-generated
                    // expression has no syntax to blame -- so a missing entry
                    // is skipped rather than unwrapped.
                    let expr = match id {
                        ExprOrPatId::ExprId(id) => body.expr_syntax(*id).map(|ptr| {
                            ptr.value
                                .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
                        }),
                        ExprOrPatId::PatId(id) => body.pat_syntax(*id).map(|ptr| {
                            ptr.value
                                .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
                        }),
                    };

                    if let Some(expr) = expr {
                        sink.push(TypeAnnotationNeeded { file, expr });
                    }
                }
                InferenceDiagnostic::UnresolvedType { id } => {
                    let type_ref = body.type_ref_syntax(*id).expect("If this is not found, it must be a type ref generated by the library which should never be unresolved.");
                    sink.push(UnresolvedType { file, type_ref });
                }
                InferenceDiagnostic::TraitAsType { id } => {
                    let type_ref = body.type_ref_syntax(*id).expect("If this is not found, it must be a type ref generated by the library which should never be unresolved.");
                    sink.push(crate::diagnostics::TraitError {
                        file,
                        node: type_ref.syntax_node_ptr(),
                        message: "a trait is not a type: values of a trait type (dynamic dispatch) are not \
                          supported; take a generic parameter bounded by the trait instead \
                          (`[T: Trait]`)"
                            .to_string(),
                    });
                }
                InferenceDiagnostic::DualOfNonSession { id } => {
                    let type_ref = body.type_ref_syntax(*id).expect("If this is not found, it must be a type ref generated by the library which should never be unresolved.");
                    sink.push(crate::diagnostics::SessionError {
                        file,
                        node: type_ref.syntax_node_ptr(),
                        message: "`dual` applies to a session type: `dual S` is the other \
                                  endpoint's view of session `S`"
                            .to_string(),
                    });
                }
                InferenceDiagnostic::CyclicType { id } => {
                    let type_ref = body.type_ref_syntax(*id).expect("If this is not found, it must be a type ref generated by the library which should never be unresolved.");
                    sink.push(CyclicType { file, type_ref });
                }
                InferenceDiagnostic::TypeIsPrivate { id } => {
                    let type_ref = body.type_ref_syntax(*id).expect("If this is not found, it must be a type ref generated by the library which should never be unresolved.");
                    sink.push(PrivateAccess {
                        file,
                        expr: type_ref.syntax_node_ptr(),
                    });
                }
                InferenceDiagnostic::PathIsPrivate { id } => {
                    let expr_syntax = body
                        .expr_syntax(*id)
                        .map(|ptr| {
                            ptr.value
                                .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
                        })
                        .expect("could not resolve expression to syntax node");
                    sink.push(PrivateAccess {
                        file,
                        expr: expr_syntax,
                    });
                }
                InferenceDiagnostic::ParameterCountMismatch {
                    id,
                    expected,
                    found,
                } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(ParameterCountMismatch {
                        file,
                        expr,
                        expected: *expected,
                        found: *found,
                    });
                }
                InferenceDiagnostic::ExpectedFunction { id, found } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(ExpectedFunction {
                        file,
                        expr,
                        found: found.clone(),
                    });
                }
                InferenceDiagnostic::MismatchedTypes {
                    id,
                    found,
                    expected,
                } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(MismatchedType {
                        file,
                        expr,
                        found: found.clone(),
                        expected: expected.clone(),
                    });
                }
                InferenceDiagnostic::IncompatibleBranches {
                    id,
                    then_ty,
                    else_ty,
                } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(IncompatibleBranch {
                        file,
                        if_expr: expr,
                        expected: then_ty.clone(),
                        found: else_ty.clone(),
                    });
                }
                InferenceDiagnostic::MissingElseBranch { id, then_ty } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(MissingElseBranch {
                        file,
                        if_expr: expr,
                        found: then_ty.clone(),
                    });
                }
                InferenceDiagnostic::CannotApplyBinaryOp { id, lhs, rhs } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(CannotApplyBinaryOp {
                        file,
                        expr,
                        lhs: lhs.clone(),
                        rhs: rhs.clone(),
                    });
                }
                InferenceDiagnostic::CannotApplyUnaryOp { id, ty } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(CannotApplyUnaryOp {
                        file,
                        expr,
                        ty: ty.clone(),
                    });
                }
                InferenceDiagnostic::InvalidCast {
                    id,
                    from,
                    to,
                    reason,
                } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(InvalidCast {
                        file,
                        expr,
                        from: from.clone(),
                        to: to.clone(),
                        reason: *reason,
                    });
                }
                InferenceDiagnostic::InvalidLhs { id, lhs } => {
                    let id = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    let lhs = body
                        .expr_syntax(*lhs)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(InvalidLhs {
                        file,
                        expr: id,
                        lhs,
                    });
                }
                InferenceDiagnostic::ReturnMissingExpression { id } => {
                    let id = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(ReturnMissingExpression {
                        file,
                        return_expr: id,
                    });
                }
                InferenceDiagnostic::BreakOutsideLoop { id } => {
                    let id = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(BreakOutsideLoop {
                        file,
                        break_expr: id,
                    });
                }
                InferenceDiagnostic::BreakWithValueOutsideLoop { id } => {
                    let id = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(BreakWithValueOutsideLoop {
                        file,
                        break_expr: id,
                    });
                }
                InferenceDiagnostic::AccessUnknownField {
                    id,
                    receiver_ty,
                    name,
                } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(AccessUnknownField {
                        file,
                        expr,
                        receiver_ty: receiver_ty.clone(),
                        name: name.clone(),
                    });
                }
                InferenceDiagnostic::FieldCountMismatch {
                    id,
                    expected,
                    found,
                } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(FieldCountMismatch {
                        file,
                        expr,
                        expected: *expected,
                        found: *found,
                    });
                }
                InferenceDiagnostic::MissingFields {
                    id,
                    struct_ty,
                    names,
                } => {
                    let fields = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());

                    sink.push(MissingFields {
                        file,
                        struct_ty: struct_ty.clone(),
                        fields,
                        field_names: names.clone(),
                    });
                }
                InferenceDiagnostic::MismatchedStructLit {
                    id,
                    expected,
                    found,
                } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(MismatchedStructLit {
                        file,
                        expr,
                        expected: *expected,
                        found: *found,
                    });
                }
                InferenceDiagnostic::InvalidTupleDestructure {
                    id,
                    found,
                    arity,
                    expected_arity,
                } => {
                    let pat = body
                        .pat_syntax(*id)
                        .expect("a tuple pattern always has a syntax node")
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(InvalidTupleDestructure {
                        file,
                        pat,
                        found: found.clone(),
                        arity: *arity,
                        expected_arity: *expected_arity,
                    });
                }
                InferenceDiagnostic::NoFields { id, found } => {
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(NoFields {
                        file,
                        receiver_expr: expr,
                        found: found.clone(),
                    });
                }
                InferenceDiagnostic::NoSuchField { id, field } => {
                    let field = owner.body_source_map(db).field_syntax(*id, *field).into();
                    sink.push(NoSuchField { file, field });
                }
                InferenceDiagnostic::LiteralOutOfRange { id, literal_ty } => {
                    let literal = body
                        .expr_syntax(*id)
                        .expect("could not retrieve expr from source map")
                        .map(|expr_src| {
                            expr_src
                                .left()
                                .expect("could not retrieve expr from ExprSource")
                                .cast()
                                .expect("could not cast expression to literal")
                        });
                    sink.push(LiteralOutOfRange {
                        literal,
                        int_ty: *literal_ty,
                    });
                }
                InferenceDiagnostic::MethodNotInScope { id, receiver_ty } => {
                    let method_call = body
                        .expr_syntax(*id)
                        .expect("expression missing from source map")
                        .map(|expr_src| {
                            expr_src
                                .left()
                                .expect("could not retrieve expression from ExprSource")
                                .cast()
                                .expect("could not cast expression to method call")
                        });
                    sink.push(MethodNotInScope {
                        method_call,
                        receiver_ty: receiver_ty.clone(),
                    });
                }
                InferenceDiagnostic::MethodNotFound {
                    id,
                    receiver_ty,
                    method_name,
                    field_with_same_name,
                    associated_function_with_same_name,
                } => {
                    let source = body
                        .expr_syntax(*id)
                        .expect("expression missing from source map");
                    // A call the compiler wrote -- the `to_string()` of a hole
                    // in an interpolated literal -- points at the literal,
                    // which is not a method call: blame the literal.
                    let cast = source.value.clone().left().and_then(|ptr| ptr.cast());
                    let Some(cast) = cast else {
                        sink.push(ExprError {
                            file,
                            node: source
                                .value
                                .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr()),
                            message: format!(
                                "method `{method_name}` does not exist on `{}`",
                                crate::HirDisplay::display(receiver_ty, db)
                            ),
                        });
                        return;
                    };
                    let method_call = source.map(|_| cast);
                    sink.push(MethodNotFound {
                        method_call,
                        receiver_ty: receiver_ty.clone(),
                        method_name: method_name.clone(),
                        field_with_same_name: field_with_same_name.clone(),
                        associated_function_with_same_name: *associated_function_with_same_name,
                    });
                }
                InferenceDiagnostic::AccessPrivateField { id, .. } => {
                    // TODO: Add dedicated diagnostic for this
                    let expr = body
                        .expr_syntax(*id)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr());
                    sink.push(PrivateAccess { file, expr });
                }
                InferenceDiagnostic::ExprError { id, message } => {
                    // A compiler-generated node has no syntax to blame; skip
                    // it rather than unwrap, as `TypeAnnotationNeeded` does.
                    let node = match id {
                        ExprOrPatId::ExprId(id) => body.expr_syntax(*id).map(|ptr| {
                            ptr.value
                                .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
                        }),
                        ExprOrPatId::PatId(id) => body.pat_syntax(*id).map(|ptr| {
                            ptr.value
                                .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
                        }),
                    };
                    if let Some(node) = node {
                        sink.push(ExprError {
                            file,
                            node,
                            message: message.clone(),
                        });
                    }
                }
            }
        }
    }
}

/// Replaces every type parameter belonging to `owner` with the corresponding
/// entry of `substitution`.
///
/// This is instantiation: a declaration's types are written in terms of its
/// own parameters, and a use site says what those parameters are. Applying
/// the substitution is what turns the declared `T` into the `i32` the caller
/// actually wrote.
///
/// Parameters belonging to some *other* owner are left alone. That matters
/// inside a generic function that calls another generic function: the
/// caller's own `T` is a fixed type from the body's point of view, and
/// rewriting it with the callee's arguments would confuse two unrelated
/// parameters that happen to share a name.
///
/// An index past the end of `substitution` is also left alone, which happens
/// when a type is written with fewer arguments than it has parameters. There
/// is no substitute that would be more correct than the parameter itself,
/// and silently supplying `Unknown` would turn an arity mistake into a
/// mismatch reported somewhere else entirely.
pub(crate) fn substitute_type_params(
    ty: Ty,
    owner: &GenericDefId,
    substitution: &Substitution,
) -> Ty {
    if substitution.is_empty() {
        return ty;
    }

    ty.fold(&mut |inner| match inner.interned() {
        TyKind::TypeParam(id, _) if &id.owner == owner => substitution
            .interned()
            .get(id.index as usize)
            .cloned()
            .unwrap_or(inner),
        _ => inner,
    })
}

/// How `op` is written, for messages.
fn binary_op_symbol(op: BinaryOp) -> &'static str {
    use crate::{ArithOp, CmpOp, LogicOp, Ordering};
    let arith = |op: ArithOp| match op {
        ArithOp::Add => "+",
        ArithOp::Multiply => "*",
        ArithOp::Subtract => "-",
        ArithOp::Divide => "/",
        ArithOp::Remainder => "%",
        ArithOp::LeftShift => "<<",
        ArithOp::RightShift => ">>",
        ArithOp::BitAnd => "&",
        ArithOp::BitOr => "|",
        ArithOp::BitXor => "^",
    };
    match op {
        BinaryOp::LogicOp(LogicOp::And) => "&&",
        BinaryOp::LogicOp(LogicOp::Or) => "||",
        BinaryOp::ArithOp(op) => arith(op),
        BinaryOp::CmpOp(CmpOp::Eq { negated: false }) => "==",
        BinaryOp::CmpOp(CmpOp::Eq { negated: true }) => "!=",
        BinaryOp::CmpOp(CmpOp::Ord { ordering, strict }) => match (ordering, strict) {
            (Ordering::Less, true) => "<",
            (Ordering::Less, false) => "<=",
            (Ordering::Greater, true) => ">",
            (Ordering::Greater, false) => ">=",
        },
        BinaryOp::Assignment { op: None } => "=",
        BinaryOp::Assignment { op: Some(op) } => match op {
            ArithOp::Add => "+=",
            ArithOp::Multiply => "*=",
            ArithOp::Subtract => "-=",
            ArithOp::Divide => "/=",
            ArithOp::Remainder => "%=",
            ArithOp::LeftShift => "<<=",
            ArithOp::RightShift => ">>=",
            ArithOp::BitAnd => "&=",
            ArithOp::BitOr => "|=",
            ArithOp::BitXor => "^=",
        },
    }
}
