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
    code_model::{Struct, StructKind},
    diagnostics::DiagnosticSink,
    expr::{Body, Expr, ExprId, Literal, Pat, PatId, RecordLitField, Statement, UnaryOp},
    name_resolution::Namespace,
    resolve::{Resolver, TypeNs, ValueNs},
    ty::{
        cast::{check_cast, CastCheck},
        infer::{diagnostics::InferenceDiagnostic, type_variable::TypeVariableTable},
        lower::LowerDiagnostic,
        op, Ty, TypableDef,
    },
    type_ref::LocalTypeRefId,
    BinaryOp, CallableDef, Function, HirDatabase, Name, Path,
};

mod place_expr;
mod type_variable;
mod unify;

use crate::{
    expr::{LiteralFloat, LiteralFloatKind, LiteralInt, LiteralIntKind},
    has_module::HasModule,
    ids::{DefWithBodyId, FunctionId},
    method_resolution::{lookup_method, AssociationMode},
    resolve::{resolver_for_expr, HasResolver, ResolveValueResult},
    ty::{
        primitives::{FloatTy, IntTy},
        TyKind,
    },
};

mod coerce;

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
        for (pat, type_ref) in self.body.params().iter() {
            let ty = self.resolve_type(*type_ref);
            self.infer_pat(*pat, ty);
        }

        // Resolve the return type
        self.return_ty = self.resolve_type(self.body.ret_type());
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
            Expr::Missing => error_type(),
            Expr::Path(p) => {
                // FIXME this could be more efficient...
                let resolver = resolver_for_expr(self.db, self.body.owner(), tgt_expr);
                self.infer_path_expr(&resolver, p, tgt_expr, check_params)
                    .unwrap_or_else(error_type)
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
                    if let BinaryOp::Assignment { op: _op } = op {
                        let resolver = resolver_for_expr(self.db, self.body.owner(), tgt_expr);
                        if !self.check_place_expression(&resolver, *lhs) {
                            self.diagnostics.push(InferenceDiagnostic::InvalidLhs {
                                id: tgt_expr,
                                lhs: *lhs,
                            });
                        }
                    };
                    let rhs_expected = op::binary_op_rhs_expectation(*op, lhs_ty.clone());
                    if lhs_ty.is_known() && rhs_expected.is_unknown() {
                        self.diagnostics
                            .push(InferenceDiagnostic::CannotApplyBinaryOp {
                                id: tgt_expr,
                                lhs: lhs_ty,
                                rhs: rhs_expected.clone(),
                            });
                    }
                    let rhs_ty = self.infer_expr(*rhs, &Expectation::has_type(rhs_expected));
                    op::binary_op_return_ty(*op, rhs_ty)
                }
                _ => error_type(),
            },
            Expr::Block { statements, tail } => self.infer_block(statements, *tail, expected),
            Expr::Call { callee: call, args } => self.infer_call(tgt_expr, *call, args, expected),
            Expr::MethodCall {
                receiver,
                args,
                method_name,
            } => self.infer_method_call(tgt_expr, *receiver, args, method_name, expected),
            Expr::Literal(lit) => match lit {
                Literal::String(_) => TyKind::Unknown.intern(),
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
                Literal::Nil => TyKind::Unknown.intern(),
            },
            Expr::Return { expr } => {
                if let Some(expr) = expr {
                    self.infer_expr(*expr, &Expectation::has_type(self.return_ty.clone()));
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
            Expr::RecordLit {
                type_id,
                fields,
                spread,
            } => {
                let ty = self.resolve_type(*type_id);
                let def_id = ty.as_struct();
                self.unify(&ty, &expected.ty);

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
                        .map_or(error_type(), |field| field.ty(self.db));
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
                let receiver_ty = self.infer_expr(*expr, &Expectation::none());
                if let Some((field_ty, is_visible)) = self.lookup_field(receiver_ty.clone(), name) {
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
            Expr::UnaryOp { expr, op } => {
                let inner_ty =
                    self.infer_expr_inner(*expr, &Expectation::none(), &CheckParams::default());
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
            Expr::Index { base, index } => {
                let elem_ty = if expected.ty.is_unknown() {
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

        let source = self.normalize_for_cast(&source_ty);
        let target = self.normalize_for_cast(&target_ty);
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

    /// Resolves instantiated inference variables and type aliases so that the
    /// cast check sees the underlying primitive type of e.g. `type Byte = u8`.
    ///
    /// `replace_if_possible` unwraps a single alias level per call, so this
    /// iterates; the bound keeps a cyclic alias (`type A = B; type B = A;`,
    /// already reported as `cyclic type`) from looping forever. Whatever is
    /// left at that point is still a `TypeAlias`, which the cast check treats
    /// as unknown rather than as an error.
    fn normalize_for_cast(&mut self, ty: &Ty) -> Ty {
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
            TyKind::Struct(s) => {
                let struct_data = self.db.struct_data(s.id);
                let local_field_idx = struct_data.find_field(field_name)?;
                let field_types = self.db.lower_struct(*s);
                let field_visibilities = self.db.field_visibilities(s.id.into());
                let field_data = &struct_data.fields[local_field_idx];
                Some((
                    field_types[field_data.type_ref].clone(),
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
        _expected: &Expectation,
    ) -> Ty {
        // If the method name is missing from the AST we simply return an error type
        // since an error would have already been emitted by the AST generation.
        if method_name.is_missing() {
            return error_type();
        }

        // `Type.assoc_fn(args)` -- static member access, which
        // `spec/LANGUAGE_SPEC.md` section 3 spells out directly
        // (`Box[i32].new(5)`). Syntactically this is indistinguishable from a
        // method call, so it has to be recognised here, *before* the receiver
        // is inferred as a value: a record struct's name is not a value, so
        // inferring it would report a spurious "mismatched struct literal"
        // and then a "method does not exist" on top of it.
        if let Some(ty) = self.infer_static_member_call(tgt_expr, receiver, args, method_name) {
            return ty;
        }

        let receiver_ty = self.infer_expr(receiver, &Expectation::none());

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
                        receiver_ty,
                    });
                resolved
            }
            Err(None) => {
                // Check if there is a field with the same name.
                let field_with_same_name = self
                    .lookup_field(receiver_ty.clone(), method_name)
                    .map(|(field_ty, _is_visible)| field_ty);

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

        let type_for_def = |def| self.db.type_for_def(def, Namespace::Types);
        let self_ty = match type_ns {
            TypeNs::StructId(id) => type_for_def(TypableDef::Struct(id.into())),
            TypeNs::TypeAliasId(id) => type_for_def(TypableDef::TypeAlias(id.into())),
            TypeNs::PrimitiveType(id) => type_for_def(TypableDef::PrimitiveType(id.into())),
            TypeNs::SelfType(id) => self.db.type_for_impl_self(id),
            // As above: `T.assoc(..)` needs `where T: Trait` bounds to
            // resolve against, which are parsed but not yet resolved.
            TypeNs::GenericParam(..) => return None,
        };

        let resolved_function = lookup_method(
            self.db,
            &self_ty,
            self.module(),
            method_name,
            Some(AssociationMode::WithoutSelf),
        )
        .ok()?;

        // Record the receiver's type so later stages never see `Unknown`
        // here, and register the resolution the same way a method call does.
        self.set_expr_type(receiver, self_ty);
        self.method_resolution.insert(tgt_expr, resolved_function);

        Some(self.infer_call_arguments_and_return(
            tgt_expr,
            args,
            Function::from(resolved_function).into(),
        ))
    }

    fn infer_call_arguments_and_return(
        &mut self,
        tgt_expr: ExprId,
        args: &[ExprId],
        callable: CallableDef,
    ) -> Ty {
        // Retrieve the function signature.
        let signature = self.db.callable_sig(callable);

        // Verify that the number of arguments matches
        if signature.params().len() != args.len() {
            self.diagnostics
                .push(InferenceDiagnostic::ParameterCountMismatch {
                    id: tgt_expr,
                    found: args.len(),
                    expected: signature.params().len(),
                });
        }

        // Verify the argument types
        for (&arg, param_ty) in args.iter().zip(signature.params().iter()) {
            self.infer_expr_coerce(arg, &Expectation::has_type(param_ty.clone()));
        }

        signature.ret().clone()
    }

    /// Inferences the type of a call expression.
    fn infer_call(
        &mut self,
        tgt_expr: ExprId,
        callee: ExprId,
        args: &[ExprId],
        _expected: &Expectation,
    ) -> Ty {
        let callee_ty = self.infer_expr_inner(
            callee,
            &Expectation::none(),
            &CheckParams {
                is_unit_struct: false,
            },
        );

        match callee_ty.interned() {
            TyKind::Struct(s) => {
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
                let (param_tys, ret_ty) = (sig.params().to_vec(), sig.ret().clone());
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
                ValueNs::LocalBinding(pat) => Some(self.type_of_pat.get(pat)?.clone()),
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
            let was_unknown = ty.is_unknown();
            let resolved = self.type_variables.resolve_ty_completely(ty.clone());
            if !was_unknown && resolved.is_unknown() {
                self.report_expr_inference_failure(expr);
            }
            *ty = resolved;
        }
        let mut pat_types = std::mem::take(&mut self.type_of_pat);
        for (pat, ty) in pat_types.iter_mut() {
            let was_unknown = ty.is_unknown();
            let resolved = self.type_variables.resolve_ty_completely(ty.clone());
            if !was_unknown && resolved.is_unknown() {
                self.report_pat_inference_failure(pat);
            }
            *ty = resolved;
        }
        InferenceResult {
            //            field_resolutions: self.field_resolutions,
            //            variant_resolutions: self.variant_resolutions,
            //            assoc_resolutions: self.assoc_resolutions,
            type_of_expr: expr_types,
            type_of_pat: pat_types,
            diagnostics: self.diagnostics,
            standard_types: InternedStandardTypes::default(),
            method_resolutions: self.method_resolution,
        }
    }

    fn infer_block(
        &mut self,
        statements: &[Statement],
        tail: Option<ExprId>,
        expected: &Expectation,
    ) -> Ty {
        let mut diverges = false;
        for stmt in statements {
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
                    self.infer_pat(*pat, ty);
                }
                Statement::Expr(expr) => {
                    if self.infer_expr(*expr, &Expectation::none()).is_never() {
                        diverges = true;
                    };
                }
            }
        }
        let ty = if let Some(expr) = tail {
            // Perform coercion of the trailing expression unless the expression has a Never
            // return type because we want the block to get the Never type in
            // that case.
            let ty = self.infer_expr_inner(expr, expected, &CheckParams::default());
            if ty.is_never() {
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

    fn infer_loop_expr(&mut self, _tgt_expr: ExprId, body: ExprId, expected: &Expectation) -> Ty {
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
        _tgt_expr: ExprId,
        condition: ExprId,
        body: ExprId,
        _expected: &Expectation,
    ) -> Ty {
        self.infer_expr(condition, &Expectation::has_type(TyKind::Bool.intern()));
        self.infer_loop_block(body, ActiveLoop::While);
        Ty::unit()
    }

    #[allow(clippy::unused_self)]
    pub fn report_pat_inference_failure(&mut self, _pat: PatId) {
        //        self.diagnostics.push(InferenceDiagnostic::PatInferenceFailed {
        //            pat
        //        });
        // Currently this should never happen because we can only infer integer and
        // floating-point types which always have a fallback value.
        panic!("pattern failed inferencing");
    }

    #[allow(clippy::unused_self)]
    pub fn report_expr_inference_failure(&mut self, _expr: ExprId) {
        //        self.diagnostics.push(InferenceDiagnostic::ExprInferenceFailed {
        //            expr
        //        });
        // Currently this should never happen because we can only infer integer and
        // floating-point types which always have a fallback value.
        panic!("expression failed inferencing");
    }
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
}

impl Default for CheckParams {
    fn default() -> Self {
        Self {
            is_unit_struct: true,
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
            CannotApplyUnaryOp, CyclicType, DiagnosticSink, ExpectedFunction, FieldCountMismatch,
            IncompatibleBranch, InvalidCast, InvalidLhs, InvalidTupleDestructure,
            LiteralOutOfRange, MethodNotFound, MethodNotInScope, MismatchedStructLit,
            MismatchedType, MissingElseBranch, MissingFields, NoFields, NoSuchField,
            ParameterCountMismatch, PrivateAccess, ReturnMissingExpression, UnresolvedType,
            UnresolvedValue,
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
        CyclicType {
            id: LocalTypeRefId,
        },
        ExpectedFunction {
            id: ExprId,
            found: Ty,
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
                InferenceDiagnostic::UnresolvedType { id } => {
                    let type_ref = body.type_ref_syntax(*id).expect("If this is not found, it must be a type ref generated by the library which should never be unresolved.");
                    sink.push(UnresolvedType { file, type_ref });
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
            }
        }
    }
}
