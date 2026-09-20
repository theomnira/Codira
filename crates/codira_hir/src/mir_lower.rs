//! Copyright (c) 2026 Omnira CJSC
//!
//! General HIR `Body` -> `codira_mir::Generator` lowering.
//!
//! Generalizes `comptime_fold`'s single-tail-expression walker to real
//! function bodies: function parameters become `codira_mir::OpKind::Arg`,
//! the function's own generic parameters become `OpKind::ParamRef`, and
//! `let`-bindings/multi-statement blocks are supported (not just one tail
//! expression). This is the prerequisite `spec/KGEN_SUPERSET_STATUS.md`
//! roadmap item 4/session-2-item-#17 -- without it, elaboration
//! (`codira_comptime::elaborate`) has nothing but hand-built `Generator`s
//! to operate on.
//!
//! Scope, precisely (same honesty convention as `comptime_fold`): supports
//! int/bool/float literals, arithmetic/comparison/logical/bitwise/shift
//! operators, unary neg/not, `expr as Type` casts, `if`/`else`, references
//! to the function's own parameters and generic parameters, `let`-bindings
//! with a simple `Pat::Bind` pattern and an initializer, and calls to plain
//! named functions (`f(a, b)` where the callee is a bare single-identifier
//! path -- lowered to `codira_mir::OpKind::Call` with the callee's *name* as
//! the symbol, resolved later against a `GeneratorStore` exactly per
//! `OpKind::Call`'s doc; the name matches `Generator::name` because both
//! come from the same `FunctionData::name`). **Not** supported (lowering
//! returns `None`, honestly, rather than producing a wrong or partial
//! `Generator`): method calls, multi-segment callee paths, calling
//! through a local or parameter, loops (`while`/`loop` bodies mutate
//! outer locals to make progress, and mutation is not representable under
//! this pass's re-lower-by-initializer locals model -- a structured
//! `cf.while` lowering becomes worthwhile only once locals are real SSA
//! values), struct/array construction, uninitialized `let` bindings,
//! non-trivial patterns, and any expression-statement (no side effects
//! are modeled).
//!
//! # Types (RFC-001 §1.8 phase 2)
//!
//! Every op this pass emits carries a `codira_mir::TypeId` derived from the
//! function's `InferenceResult`, so a lowered body is checkable by
//! `codira_mir::check_types` and not just structurally verifiable. Two
//! deliberate rules keep that honest:
//!
//! * A HIR type with no machine representation here (a struct, an array, a
//!   still-unresolved generic parameter, an unresolved type alias, an error
//!   type) maps to *no* `TypeId` -- [`ty_to_mir_type`] returns `None` and the
//!   op stays `TypeId::UNTYPED`, which the verifier skips. Never a guess.
//! * A type is only attached when it satisfies the *category* rule
//!   `codira_mir::verify::check_types` enforces for that op kind (see
//!   [`typed_result`]). The one place this bites today is HIR's `!`, which is
//!   both logical negation on `bool` *and* bitwise complement on integers,
//!   while `codira_mir::OpKind::Not` is checked as the logical one only: an
//!   integer `!x` therefore lowers to an honestly untyped `core.not` rather
//!   than to an op the verifier would reject.

use std::sync::Arc;

use codira_mir::{CastMode, TypeId};
use codira_target::abi::TargetDataLayout;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

use crate::{
    code_model::Function,
    expr::{
        ArithOp, BinaryOp, Body as HirBody, CmpOp, Expr, ExprId, Literal, LiteralInt, LogicOp,
        Ordering, Pat, Statement, UnaryOp,
    },
    ty::cast::{check_cast, CastCheck, CastOp},
    FloatBitness, HirDatabase, InferenceResult, IntBitness, Name, ResolveBitness, Ty, TyKind,
};

/// Salsa query implementation for `HirDatabase::mir_generator` -- see
/// `db.rs`. Memoized per-`Function`: unchanged functions (by salsa's
/// usual dependency tracking, ultimately rooted in source text) are never
/// re-lowered.
pub(crate) fn mir_generator_query(
    db: &dyn HirDatabase,
    func: Function,
) -> Option<Arc<codira_mir::Generator>> {
    lower_function_to_generator(db, func).map(Arc::new)
}

/// Salsa query implementation for `HirDatabase::elaborate_generator` --
/// see `db.rs`. This is the concrete "incremental elaboration cache"
/// architecture doc §4.1 describes: memoized per `(Function, bindings)`,
/// so re-elaborating the same function with the same concrete parameter
/// values under `codira build --watch` is a cache hit, not repeated work --
/// for free, from salsa, rather than a hand-built DAG-of-expansions cache
/// (contrast KGEN's own `DesignOverview.md` "Dynamic Programming /
/// Caching" section, which describes wanting to build exactly this as
/// future work).
pub(crate) fn elaborate_generator_query(
    db: &dyn HirDatabase,
    func: Function,
    bindings: Vec<(SmolStr, codira_comptime::Value)>,
) -> Option<Arc<codira_mir::Body>> {
    let generator = db.mir_generator(func)?;
    Some(Arc::new(codira_comptime::elaborate(&generator, &bindings)))
}

/// Attempts to lower `func`'s body to a `codira_mir::Generator`. `None`
/// means the body uses a construct outside the scope described in this
/// module's doc comment -- not an error, just "not liftable yet."
pub fn lower_function_to_generator(
    db: &dyn HirDatabase,
    func: Function,
) -> Option<codira_mir::Generator> {
    let data = func.data(db);
    let hir_body = func.body(db);

    let mut param_index: FxHashMap<Name, u32> = FxHashMap::default();
    for (i, (pat_id, _ty)) in hir_body.params().iter().enumerate() {
        if let Pat::Bind { name } = &hir_body[*pat_id] {
            param_index.insert(name.clone(), i as u32);
        }
    }

    let generic_names: FxHashMap<Name, SmolStr> = data
        .generic_params()
        .iter()
        .map(|name| (name.clone(), name.to_string().into()))
        .collect();

    // Types for the ops we emit come from the function's own inference
    // result (RFC-001 phase 2); `usize`/`isize` need the target to have a
    // width at all, exactly as `ty::cast::check_cast` does.
    let infer = func.infer(db);
    let layout = db.target_data_layout();

    let mut lowerer = Lowerer {
        hir_body: &hir_body,
        infer: &infer,
        layout: &layout,
        param_index,
        generic_names,
        locals: FxHashMap::default(),
    };

    let mut mir_body = codira_mir::Body::new();
    lowerer.lower_expr(hir_body.body_expr(), &mut mir_body)?;

    let params = data
        .generic_params()
        .iter()
        .map(|name| codira_mir::GeneratorParam {
            name: name.to_string().into(),
        })
        .collect();

    Some(codira_mir::Generator {
        name: data.name().to_string().into(),
        params,
        body: mir_body,
    })
}

struct Lowerer<'a> {
    hir_body: &'a HirBody,
    /// Types for the lowered ops, and the source/target of every `as` cast.
    /// Keyed by the same `ExprId`s as `hir_body` (both come from one
    /// `DefWithBodyId`), so indexing is total.
    infer: &'a InferenceResult,
    /// Needed to give `usize`/`isize` a concrete width.
    layout: &'a TargetDataLayout,
    param_index: FxHashMap<Name, u32>,
    generic_names: FxHashMap<Name, SmolStr>,
    /// `let`-bound locals: name -> the HIR `ExprId` of its initializer.
    ///
    /// Deliberately *not* a `codira_mir::OpId`: ops are indices into one
    /// specific `Body`'s arena, but a local can be referenced from inside
    /// a nested `cf.if` branch, which builds into its own fresh, separate
    /// `Body` (`codira_mir` has no cross-region SSA value referencing --
    /// see architecture doc §2, regions are plain nested arenas, not
    /// dominance-tracked blocks). So instead of storing *where* a local's
    /// value was computed, this stores *how* to compute it again, and
    /// every reference re-lowers the initializer fresh into whichever
    /// `Body` is currently being built. Fine for this pass's side-effect-
    /// free restricted subset (re-evaluating is observably identical to
    /// reusing a value) -- see module doc for what's out of scope.
    locals: FxHashMap<Name, ExprId>,
}

impl Lowerer<'_> {
    /// The `codira_mir` type of the *value* HIR expression `id` produces, or
    /// `TypeId::UNTYPED` when it has no machine representation in this IR.
    fn ty_of(&self, id: ExprId) -> TypeId {
        ty_to_mir_type(&self.infer[id], self.layout).unwrap_or(TypeId::UNTYPED)
    }

    fn lower_expr(&mut self, id: ExprId, body: &mut codira_mir::Body) -> Option<codira_mir::OpId> {
        use codira_mir::{Attr, OpKind, Region};

        match &self.hir_body[id] {
            Expr::Literal(Literal::Bool(b)) => {
                Some(body.push_typed(OpKind::Const(Attr::Bool(*b)), [], self.ty_of(id)))
            }
            Expr::Literal(Literal::Int(LiteralInt { value, .. })) => {
                let v = i64::try_from(*value).ok()?;
                Some(body.push_typed(OpKind::Const(Attr::Int(v)), [], self.ty_of(id)))
            }
            Expr::Literal(Literal::Float(crate::expr::LiteralFloat { value, .. })) => {
                Some(body.push_typed(OpKind::Const(Attr::float(*value)), [], self.ty_of(id)))
            }

            Expr::Path(path) => {
                let name = path.as_ident()?;
                if let Some(&idx) = self.param_index.get(name) {
                    Some(body.push_typed(OpKind::Arg(idx), [], self.ty_of(id)))
                } else if let Some(param_name) = self.generic_names.get(name) {
                    // A generic parameter is compile-time data with no
                    // settled type until elaboration substitutes it, so
                    // `param.ref` stays untyped on purpose.
                    Some(body.push_typed(OpKind::ParamRef(param_name.clone()), [], self.ty_of(id)))
                } else if let Some(&init) = self.locals.get(name) {
                    // Re-lowering the initializer also re-derives its type,
                    // which is the local's type -- see `locals`' doc.
                    self.lower_expr(init, body)
                } else {
                    None
                }
            }

            Expr::UnaryOp { expr, op } => {
                let inner = self.lower_expr(*expr, body)?;
                let kind = match op {
                    UnaryOp::Neg => OpKind::Neg,
                    UnaryOp::Not => OpKind::Not,
                    UnaryOp::BitNot => OpKind::BitNot,
                };
                let ty = typed_result(&kind, self.ty_of(id));
                Some(body.push_typed(kind, [inner], ty))
            }

            Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(op),
            } => {
                let lhs_id = self.lower_expr(*lhs, body)?;
                let rhs_id = self.lower_expr(*rhs, body)?;
                let kind = binary_op_kind(*op)?;
                let ty = typed_result(&kind, self.ty_of(id));
                Some(body.push_typed(kind, [lhs_id, rhs_id], ty))
            }

            // `expr as Type`. The (source, target) pair -- not the syntax --
            // decides what is emitted, and the decision is made by the same
            // `ty::cast::check_cast` type inference used to reject illegal
            // casts, so the two can never disagree about what `as` means.
            Expr::Cast { expr, .. } => {
                // `infer[id]` *is* the resolved target type: `infer_cast`
                // types the cast expression as its target.
                let source = self.infer[*expr].clone();
                let target = self.infer[id].clone();

                match check_cast(&source, &target, self.layout) {
                    // Same machine representation on both sides (`i32 as
                    // i32`, or `isize as i64` on a 64-bit target): the cast
                    // *is* its operand. No op at all.
                    CastCheck::Legal(CastOp::Identity) => self.lower_expr(*expr, body),

                    CastCheck::Legal(CastOp::Convert(kind, mode)) => {
                        // S6 never produces `Checked`; that is reserved for
                        // the refinement-types integration (M7).
                        debug_assert_eq!(mode, CastMode::Wrapping);
                        let target_ty = ty_to_mir_type(&target, self.layout)?;
                        let source_ty = ty_to_mir_type(&source, self.layout)?;
                        // `bool as Int` is a legal `zext` per the language
                        // spec, but `codira_mir`'s `check_types` admits only
                        // integer operands for `Zext`/`Sext`, so emitting it
                        // would build a body that fails the very checker
                        // this pass is supposed to satisfy. Declined rather
                        // than emitted untyped and hidden from the checker.
                        if source_ty == TypeId::BOOL {
                            return None;
                        }
                        let operand = self.lower_expr(*expr, body)?;
                        Some(body.push_typed(OpKind::Cast(kind, mode), [operand], target_ty))
                    }

                    // Illegal: already reported as a type error by inference,
                    // and there is no operation that means it.
                    // Undetermined: an error/unresolved/aliased type on one
                    // side -- no operation can be *selected*, so guessing one
                    // is exactly the wrong move.
                    CastCheck::Illegal(_) | CastCheck::Undetermined => None,
                }
            }

            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond_id = self.lower_expr(*condition, body)?;

                let mut then_body = codira_mir::Body::new();
                self.lower_expr(*then_branch, &mut then_body)?;

                let mut else_body = codira_mir::Body::new();
                if let Some(else_branch) = else_branch {
                    self.lower_expr(*else_branch, &mut else_body)?;
                }

                Some(body.push_with_regions_typed(
                    OpKind::If,
                    [cond_id],
                    [Region::new(then_body), Region::new(else_body)],
                    self.ty_of(id),
                ))
            }

            Expr::Block { statements, tail } => {
                for stmt in statements {
                    match stmt {
                        Statement::Let {
                            pat,
                            initializer: Some(init),
                            ..
                        } => {
                            // Validate the initializer lowers *now*, into
                            // a throwaway probe body, even though the
                            // local is only actually re-lowered (into
                            // whichever body needs it) on first reference
                            // below. Without this, an unused local whose
                            // initializer contains an unsupported
                            // construct (a call, say) would silently drop
                            // it instead of honestly bailing -- and unlike
                            // a bare expression-statement (already
                            // rejected outright), a `let` initializer
                            // could easily hide a side effect nobody
                            // meant to discard.
                            self.lower_expr(*init, &mut codira_mir::Body::new())?;
                            match &self.hir_body[*pat] {
                                Pat::Bind { name } => {
                                    self.locals.insert(name.clone(), *init);
                                }
                                // Non-trivial patterns (tuple-struct
                                // destructuring, wildcards standing in for
                                // side-effect-only initializers, etc.) are
                                // out of scope -- see module doc.
                                _ => return None,
                            }
                        }
                        // Uninitialized `let` and expression-statements
                        // are out of scope -- see module doc.
                        Statement::Let {
                            initializer: None, ..
                        }
                        | Statement::Expr(_) => return None,
                    }
                }
                match tail {
                    // A block's value *is* its tail's, type included.
                    Some(tail) => self.lower_expr(*tail, body),
                    None => Some(body.push_typed(OpKind::Const(Attr::Unit), [], TypeId::UNIT)),
                }
            }

            Expr::Call { callee, args } => {
                // Only a bare single-identifier callee path that names a
                // *function* (not a parameter, generic parameter, or
                // local -- calling through a value is not a symbol call)
                // lowers; the symbol is the function's plain name, which
                // is exactly what `lower_function_to_generator` uses for
                // `Generator::name`, so `GeneratorStore::generator_by_name`
                // resolution lines up. Per `OpKind::Call`'s doc the name
                // is *not* resolved here -- a call to a function that
                // never lowers (or doesn't exist) surfaces as
                // `UnknownCallee` at interpretation time, never as a
                // wrong answer.
                let Expr::Path(path) = &self.hir_body[*callee] else {
                    return None;
                };
                let name = path.as_ident()?;
                if self.param_index.contains_key(name)
                    || self.generic_names.contains_key(name)
                    || self.locals.contains_key(name)
                {
                    return None;
                }
                let symbol: SmolStr = name.to_string().into();
                let arg_ids = args
                    .iter()
                    .map(|arg| self.lower_expr(*arg, body))
                    .collect::<Option<Vec<_>>>()?;
                Some(body.push_typed(OpKind::Call(symbol), arg_ids, self.ty_of(id)))
            }

            // `MethodCall`, `Index`, `Array`, `RecordLit`, `Field`,
            // `Loop`, `While`, `Return`, `Break`, `Missing`, and string
            // literals are all out of scope for this pass -- see module
            // doc (in particular the note on why loops stay out until
            // locals are real SSA values).
            _ => None,
        }
    }
}

/// Maps a HIR type to the `codira_mir` machine type representing it, or
/// `None` when this IR has no representation for it.
///
/// `None` is the honest-decline path, not a failure: the caller leaves the
/// op `TypeId::UNTYPED`, which `codira_mir::check_types` skips. Structs,
/// arrays, function types, unresolved generic parameters, unresolved type
/// aliases, inference variables and error types all land here. (`Never` is
/// deliberately included: `TypeId::NEVER` exists, but nothing in this
/// pass's subset diverges, so claiming it would be asserting something
/// unverified.)
///
/// `layout` is required because `usize`/`isize` have no width until the
/// target is known -- the same reason `ty::cast::check_cast` takes one.
fn ty_to_mir_type(ty: &Ty, layout: &TargetDataLayout) -> Option<TypeId> {
    match ty.interned() {
        TyKind::Bool => Some(TypeId::BOOL),
        TyKind::Int(int_ty) => {
            let resolved = int_ty.resolve(layout);
            let width = match resolved.bitness {
                IntBitness::X8 => 8,
                IntBitness::X16 => 16,
                IntBitness::X32 => 32,
                IntBitness::X64 => 64,
                IntBitness::X128 => 128,
                // `ResolveBitness::resolve` maps `Xsize` to a concrete
                // width; reaching this arm would mean it had not been
                // called, so decline rather than pick a width.
                IntBitness::Xsize => return None,
            };
            Some(TypeId::int(width, resolved.signedness.is_signed()))
        }
        TyKind::Float(float_ty) => {
            let width = match float_ty.resolve(layout).bitness {
                FloatBitness::X32 => 32,
                FloatBitness::X64 => 64,
            };
            Some(TypeId::float(width))
        }
        // The unit type is the empty tuple; every other tuple is an
        // aggregate this IR has no scalar type for.
        TyKind::Tuple(0, _) => Some(TypeId::UNIT),
        // A generic parameter joins these: it has no MIR type until the
        // declaration is instantiated.
        TyKind::TypeParam(..)
        | TyKind::Tuple(..)
        | TyKind::Struct(..)
        | TyKind::Array(_)
        | TyKind::FnDef(..)
        | TyKind::Never
        | TyKind::TypeAlias(_)
        | TyKind::InferenceVar(_)
        | TyKind::Unknown => None,
    }
}

/// Filters a candidate result type through the *category* rule
/// `codira_mir::verify::check_types` enforces for `kind`, downgrading to
/// `TypeId::UNTYPED` when they disagree.
///
/// This is not defensiveness for its own sake: HIR and this IR genuinely
/// disagree in one place. HIR's `!` is both logical negation on `bool` and
/// bitwise complement on integers (see `ty::infer`'s `UnaryOp::Not` arm),
/// and HIR's `&`/`|`/`^` accept `bool` operands, while `codira_mir` checks
/// `core.not`/`core.and`/`core.or` as strictly logical and
/// `core.bitand`/`core.bitor`/`core.bitxor`/shifts as strictly integral.
/// Rather than emit a type the verifier would reject -- or, worse, silently
/// pick a different op -- the op is left honestly untyped and the verifier
/// skips it.
fn typed_result(kind: &codira_mir::OpKind, ty: TypeId) -> TypeId {
    use codira_mir::OpKind as K;

    let ok = match kind {
        K::Add | K::Sub | K::Mul | K::Div | K::Rem | K::Neg => ty.is_arithmetic(),
        K::BitAnd | K::BitOr | K::BitXor | K::Shl | K::Shr => ty.is_int(),
        K::Eq | K::Ne | K::Lt | K::Le | K::Gt | K::Ge | K::And | K::Or | K::Not => {
            ty == TypeId::BOOL
        }
        _ => true,
    };
    if ok {
        ty
    } else {
        TypeId::UNTYPED
    }
}

fn binary_op_kind(op: BinaryOp) -> Option<codira_mir::OpKind> {
    use codira_mir::OpKind;
    Some(match op {
        BinaryOp::ArithOp(ArithOp::Add) => OpKind::Add,
        BinaryOp::ArithOp(ArithOp::Subtract) => OpKind::Sub,
        BinaryOp::ArithOp(ArithOp::Multiply) => OpKind::Mul,
        BinaryOp::ArithOp(ArithOp::Divide) => OpKind::Div,
        BinaryOp::ArithOp(ArithOp::Remainder) => OpKind::Rem,
        BinaryOp::ArithOp(ArithOp::LeftShift) => OpKind::Shl,
        BinaryOp::ArithOp(ArithOp::RightShift) => OpKind::Shr,
        BinaryOp::ArithOp(ArithOp::BitAnd) => OpKind::BitAnd,
        BinaryOp::ArithOp(ArithOp::BitOr) => OpKind::BitOr,
        BinaryOp::ArithOp(ArithOp::BitXor) => OpKind::BitXor,
        BinaryOp::LogicOp(LogicOp::And) => OpKind::And,
        BinaryOp::LogicOp(LogicOp::Or) => OpKind::Or,
        BinaryOp::CmpOp(CmpOp::Eq { negated: false }) => OpKind::Eq,
        BinaryOp::CmpOp(CmpOp::Eq { negated: true }) => OpKind::Ne,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Less,
            strict: true,
        }) => OpKind::Lt,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Less,
            strict: false,
        }) => OpKind::Le,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Greater,
            strict: true,
        }) => OpKind::Gt,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Greater,
            strict: false,
        }) => OpKind::Ge,
        BinaryOp::Assignment { .. } => return None,
    })
}

#[cfg(test)]
mod tests;
