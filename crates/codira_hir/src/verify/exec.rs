//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Symbolic execution of a HIR body into Z3 terms, collecting the proof
//!   obligations `crate::verify` describes, then discharging them.
//!
//! The executor walks the body once, forward, keeping a *state*: the
//! current symbolic value of every local and the path condition under which
//! that point is reached. Branches fork the state and merge it back with
//! `ite` terms, so the formula grows with the code rather than with the
//! number of paths. A `return` checks the postcondition on the spot; a loop
//! is cut at its invariant (see `eval_loop`). Nothing is unrolled and
//! nothing recurses into callees, so the work is linear in the size of the
//! function plus one Z3 query per obligation.

use std::sync::Arc;

use codira_hir_input::FileId;
use codira_smt::{BoolExpr, BvExpr, CheckOutcome, Context, UnknownReason};
use codira_syntax::SyntaxNodePtr;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    refinement::{self, Refinement},
    Counterexample, Obligation, ObligationKind, ParamInfo, ScalarTy, UnknownCause, Value, Verdict,
    VerificationReport,
};
use crate::{
    code_model::{Function, HasSource},
    expr::{ArithOp, BinaryOp, BodySourceMap, CmpOp, LogicOp, Ordering, SelectArm, Statement},
    ids::DefWithBodyId,
    in_file::InFile,
    resolve::{resolver_for_expr, ValueNs},
    ty::lower::CallableDef,
    Body, Expr, ExprId, HirDatabase, InferenceResult, IntTy, Literal, MatchArm, Pat, PatId, Path,
    ResolveBitness, Ty, TyKind, UnaryOp,
};

/// A symbolic value.
#[derive(Clone, Copy)]
pub(crate) enum Sym<'ctx> {
    /// A machine integer: a bit-vector of the type's width, plus the type
    /// (which decides signed versus unsigned operations).
    Int(BvExpr<'ctx>, IntTy),
    Bool(BoolExpr<'ctx>),
    Unit,
    /// A value the verifier does not model.
    Opaque,
}

/// The width of a resolved integer type.
pub(crate) fn int_width(int: IntTy) -> u32 {
    use crate::IntBitness;
    match int.bitness {
        IntBitness::X8 => 8,
        IntBitness::X16 => 16,
        IntBitness::X32 => 32,
        // `Xsize` never reaches here resolved; 64 is the host's answer.
        IntBitness::X64 | IntBitness::Xsize => 64,
        IntBitness::X128 => 128,
    }
}

/// What the verifier knows how to model about `ty`.
enum Kind {
    Int(IntTy),
    Bool,
    Unit,
    Other,
}

fn kind_of(db: &dyn HirDatabase, ty: &Ty) -> Kind {
    match ty.interned() {
        TyKind::Int(int) => Kind::Int(int.resolve(&db.target_data_layout())),
        TyKind::Bool => Kind::Bool,
        TyKind::Tuple(0, _) => Kind::Unit,
        TyKind::TypeAlias(alias) => kind_of(db, &alias.target_type(db)),
        _ => Kind::Other,
    }
}

pub(crate) fn scalar_of(db: &dyn HirDatabase, ty: &Ty) -> Option<ScalarTy> {
    match kind_of(db, ty) {
        Kind::Int(int) => Some(ScalarTy::Int {
            signed: int.signedness.is_signed(),
            width: int_width(int),
        }),
        Kind::Bool => Some(ScalarTy::Bool),
        Kind::Unit | Kind::Other => None,
    }
}

/// Everything about one function body the executor reads.
pub(crate) struct Frame {
    pub owner: DefWithBodyId,
    pub func: Function,
    pub body: Arc<Body>,
    pub source_map: Arc<BodySourceMap>,
    pub infer: Arc<InferenceResult>,
    pub file_id: FileId,
}

impl Frame {
    pub(crate) fn new(db: &dyn HirDatabase, func: Function) -> Frame {
        let owner: DefWithBodyId = func.id.into();
        let (body, source_map) = db.body_with_source_map(owner);
        Frame {
            owner,
            func,
            body,
            source_map,
            infer: db.infer(owner),
            file_id: func.source(db).file_id,
        }
    }

    /// The syntax an expression came from.
    pub(crate) fn site(&self, expr: ExprId) -> Option<InFile<SyntaxNodePtr>> {
        self.source_map.expr_syntax(expr).map(|src| {
            src.map(|ptr| ptr.either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr()))
        })
    }

    /// The source text of an expression, for messages.
    pub(crate) fn text(&self, db: &dyn HirDatabase, expr: ExprId) -> String {
        self.site(expr).map_or_else(
            || "<expression>".to_string(),
            |site| {
                let root = db.parse(site.file_id).syntax_node();
                site.value.to_node(&root).text().to_string()
            },
        )
    }

    /// The local binding a path expression refers to, if any.
    pub(crate) fn resolve_local(
        &self,
        db: &dyn HirDatabase,
        expr: ExprId,
        path: &Path,
    ) -> Option<PatId> {
        let resolver = resolver_for_expr(db, self.owner, expr);
        match resolver.resolve_path_as_value_fully(db, path) {
            Some((ValueNs::LocalBinding(pat), _)) => Some(pat),
            _ => None,
        }
    }

    /// The function a call expression's callee names, if it names one.
    fn callee(&self, callee: ExprId) -> Option<Function> {
        match self.infer[callee].interned() {
            TyKind::FnDef(CallableDef::Function(f), _) => Some(*f),
            _ => None,
        }
    }
}

/// Whether `func` takes part in verification on its own account: it has a
/// contract clause, a loop invariant or measure, a ghost binding, or a
/// refined parameter or return type. Such a function gets its postcondition,
/// loop and division obligations checked; every other function is only
/// checked at the calls it makes.
pub fn is_participating(db: &dyn HirDatabase, func: Function) -> bool {
    !db.body(func.id.into()).contracts().is_empty()
        || refinement::param_refinements(db, func)
            .iter()
            .any(Option::is_some)
        || refinement::return_refinement(db, func).is_some()
}

/// Whether calling `func` creates an obligation at the call site.
fn has_call_contract(db: &dyn HirDatabase, func: Function) -> bool {
    !db.body(func.id.into()).contracts().requires.is_empty()
        || refinement::param_refinements(db, func)
            .iter()
            .any(Option::is_some)
}

/// The executor's state at one program point.
#[derive(Clone)]
struct State<'ctx> {
    /// Under which condition this point is reached.
    pc: BoolExpr<'ctx>,
    vars: FxHashMap<PatId, Sym<'ctx>>,
}

/// The ways out of one enclosing loop, collected while its body is
/// evaluated.
#[derive(Default)]
struct LoopFrame<'ctx> {
    /// The states (and values) `break` leaves the loop with.
    breaks: Vec<(State<'ctx>, Sym<'ctx>)>,
    /// The states `continue` starts the next iteration from: back-edges,
    /// exactly like reaching the end of the body.
    continues: Vec<State<'ctx>>,
}

/// An obligation before it has been handed to Z3.
struct Pending<'ctx> {
    kind: ObligationKind,
    site: InFile<SyntaxNodePtr>,
    clause: String,
    premise: BoolExpr<'ctx>,
    /// `Err` when the goal could not be encoded; the string says why.
    goal: Result<BoolExpr<'ctx>, String>,
    at_site: Vec<(String, Sym<'ctx>)>,
    runtime_checked: bool,
}

struct Exec<'a, 'ctx> {
    db: &'a dyn HirDatabase,
    ctx: &'ctx Context,
    frame: Frame,
    /// Whether this function's own postconditions, loops and divisions are
    /// checked (see [`is_participating`]).
    check_own: bool,
    /// Parameter values on entry, which is what `ensures` refers to.
    entry_vars: FxHashMap<PatId, Sym<'ctx>>,
    inputs: Vec<(String, Sym<'ctx>)>,
    ret_refinement: Option<Refinement>,
    pending: Vec<Pending<'ctx>>,
    /// One entry per enclosing loop (`while`, `loop` and `for`), innermost
    /// last: where its `break`s and `continue`s go.
    loops: Vec<LoopFrame<'ctx>>,
    fresh: u32,
    /// Whether this function's own `requires` clauses are enforced at run
    /// time (`@runtime_checked`), so one the verifier cannot encode is not
    /// an error.
    own_requires_checked: bool,
    /// Every return point reached: its path condition and returned value.
    /// Read by [`return_paths`] (semantic search); verification itself
    /// checks postconditions at each return and does not need the list.
    returns: Vec<(BoolExpr<'ctx>, Sym<'ctx>)>,
    /// Values for synthesis holes (`??` and `choose`), keyed by their
    /// `Expr::Missing`. Empty outside synthesis: a hole is then unknown.
    hole_values: FxHashMap<ExprId, Sym<'ctx>>,
    /// Concrete parameter values, for the synthesiser's "does the candidate
    /// work on this counterexample" step. `None` means symbolic inputs.
    fixed_inputs: Option<Vec<Option<Value>>>,
}

/// Verifies one function. See the module docs of `crate::verify`.
pub(crate) fn verify(db: &dyn HirDatabase, func: Function) -> VerificationReport {
    verify_recording(db, func, false).0
}

/// [`verify`], also returning, for each obligation (in the report's order),
/// the SMT-LIB2 text of every query Z3 was asked for it when `record` is
/// set: one per place the obligation arises (a postcondition is checked at
/// every return point), in the order they were checked. An obligation that
/// could not be encoded has none.
pub(crate) fn verify_recording(
    db: &dyn HirDatabase,
    func: Function,
    record: bool,
) -> (VerificationReport, Vec<Vec<String>>) {
    let frame = Frame::new(db, func);
    let params: Vec<ParamInfo> = frame
        .body
        .params()
        .iter()
        .enumerate()
        .map(|(idx, (pat, _))| ParamInfo {
            name: pat_name(&frame.body, *pat, idx),
            ty: scalar_of(db, &frame.infer[*pat]),
        })
        .collect();
    let empty = || {
        (
            VerificationReport {
                params: params.clone(),
                obligations: Vec::new(),
            },
            Vec::new(),
        )
    };

    if func.is_extern(db) || matches!(frame.body[frame.body.body_expr()], Expr::Missing) {
        return empty();
    }
    let check_own = is_participating(db, func);
    if !check_own && !calls_contracted_function(db, &frame) {
        return empty();
    }

    let ctx = Context::new();
    let mut exec = Exec {
        db,
        ctx: &ctx,
        ret_refinement: refinement::return_refinement(db, func),
        frame,
        check_own,
        entry_vars: FxHashMap::default(),
        inputs: Vec::new(),
        pending: Vec::new(),
        loops: Vec::new(),
        fresh: 0,
        own_requires_checked: super::is_runtime_checked(db, func),
        returns: Vec::new(),
        hole_values: FxHashMap::default(),
        fixed_inputs: None,
    };
    let mut state = exec.entry_state();
    let body_expr = exec.frame.body.body_expr();
    if let Some(value) = exec.eval(&mut state, body_expr) {
        exec.check_return(&state, value);
    }
    let (obligations, queries) = exec.solve(record);
    (
        VerificationReport {
            params,
            obligations,
        },
        queries,
    )
}

/// The proof obligations of `func` with its synthesis holes given values:
/// `holes` receives the parameters (as symbolic values, or the constants of
/// `fixed_inputs`) and returns a value for every hole. Returns each
/// obligation as `(premise, goal)`, unsolved, plus the parameters; an
/// obligation that cannot be encoded is an `Err` naming why.
#[allow(clippy::type_complexity)]
pub(crate) fn synthesis_obligations<'ctx>(
    db: &dyn HirDatabase,
    func: Function,
    ctx: &'ctx Context,
    fixed_inputs: Option<Vec<Option<Value>>>,
    holes: impl FnOnce(&[(String, Sym<'ctx>)]) -> FxHashMap<ExprId, Sym<'ctx>>,
) -> Result<
    (
        Vec<(String, Sym<'ctx>)>,
        Vec<(BoolExpr<'ctx>, BoolExpr<'ctx>)>,
    ),
    String,
> {
    let frame = Frame::new(db, func);
    let mut exec = Exec {
        db,
        ctx,
        ret_refinement: refinement::return_refinement(db, func),
        frame,
        check_own: true,
        entry_vars: FxHashMap::default(),
        inputs: Vec::new(),
        pending: Vec::new(),
        loops: Vec::new(),
        fresh: 0,
        own_requires_checked: false,
        returns: Vec::new(),
        hole_values: FxHashMap::default(),
        fixed_inputs,
    };
    let mut state = exec.entry_state();
    exec.hole_values = holes(&exec.inputs);
    let body_expr = exec.frame.body.body_expr();
    if let Some(value) = exec.eval(&mut state, body_expr) {
        exec.check_return(&state, value);
    }
    let mut obligations = Vec::with_capacity(exec.pending.len());
    for pending in exec.pending {
        match pending.goal {
            Ok(goal) => obligations.push((pending.premise, goal)),
            Err(reason) => return Err(format!("`{}`: {reason}", pending.clause)),
        }
    }
    Ok((exec.inputs, obligations))
}

/// A contract clause encoded over a function's *interface* rather than its
/// body: parameter `i` is the constant `p<i>` and the return value is
/// `result`, each a bit-vector of its type's width (or a boolean). Two
/// versions of a function with the same signature are encoded over the same
/// constants, so their clauses can be compared by implication in one
/// context.
pub(crate) struct ContractTerms<'ctx> {
    /// `requires` clauses, then the parameters' refinements (`x: T { v |
    /// .. }`), each with its source text. `Err` says why one cannot be
    /// encoded.
    pub requires: Vec<(String, Result<BoolExpr<'ctx>, String>)>,
    /// `ensures` clauses, then the return type's refinement.
    pub ensures: Vec<(String, Result<BoolExpr<'ctx>, String>)>,
}

/// The constant named `name` standing for a value of type `ty`: a
/// bit-vector of its width, a boolean, or `Opaque` for a type the verifier
/// does not model. The same name and type give the same constant in `ctx`.
pub(crate) fn interface_value<'ctx>(
    db: &dyn HirDatabase,
    ctx: &'ctx Context,
    ty: &Ty,
    name: &str,
) -> Sym<'ctx> {
    match kind_of(db, ty) {
        Kind::Int(int) => Sym::Int(ctx.bv_var(name, int_width(int)), int),
        Kind::Bool => Sym::Bool(ctx.bool_var(name)),
        Kind::Unit => Sym::Unit,
        Kind::Other => Sym::Opaque,
    }
}

/// Encodes `func`'s contract over its interface; see [`ContractTerms`].
pub(crate) fn contract_terms<'ctx>(
    db: &dyn HirDatabase,
    func: Function,
    ctx: &'ctx Context,
) -> ContractTerms<'ctx> {
    let frame = Frame::new(db, func);
    let body = Arc::clone(&frame.body);
    let exec = Exec {
        db,
        ctx,
        ret_refinement: None,
        frame,
        check_own: false,
        entry_vars: FxHashMap::default(),
        inputs: Vec::new(),
        pending: Vec::new(),
        loops: Vec::new(),
        fresh: 0,
        own_requires_checked: false,
        returns: Vec::new(),
        hole_values: FxHashMap::default(),
        fixed_inputs: None,
    };
    let interface = |ty: &Ty, name: &str| interface_value(db, ctx, ty, name);
    let encode = |vars: &FxHashMap<PatId, Sym<'ctx>>, clause: ExprId| match exec.pure(
        &exec.frame,
        vars,
        clause,
    ) {
        Ok(Sym::Bool(b)) => Ok(b),
        Ok(_) => Err("the clause is not a boolean the verifier can model".to_string()),
        Err(reason) => Err(reason),
    };

    let mut vars = FxHashMap::default();
    let mut params = Vec::new();
    for (idx, (pat, _)) in body.params().iter().enumerate() {
        let value = interface(&exec.frame.infer[*pat], &format!("p{idx}"));
        vars.insert(*pat, value);
        params.push(value);
    }
    let mut requires: Vec<_> = body
        .contracts()
        .requires
        .iter()
        .map(|&clause| (exec.frame.text(db, clause), encode(&vars, clause)))
        .collect();
    for (refinement, value) in refinement::param_refinements(db, func)
        .into_iter()
        .zip(&params)
    {
        if let Some(refinement) = refinement {
            let encoded = refinement::encode(ctx, &refinement, *value);
            requires.push((refinement.text, encoded));
        }
    }

    let result = interface(&func.ret_type(db), "result");
    if let Some(result_pat) = body.contracts().result_pat {
        vars.insert(result_pat, result);
    }
    let mut ensures: Vec<_> = body
        .contracts()
        .ensures
        .iter()
        .map(|&clause| (exec.frame.text(db, clause), encode(&vars, clause)))
        .collect();
    if let Some(refinement) = refinement::return_refinement(db, func) {
        let encoded = refinement::encode(ctx, &refinement, result);
        ensures.push((refinement.text, encoded));
    }
    ContractTerms { requires, ensures }
}

/// What a function can return, as symbolic values over its parameters.
pub(crate) struct ReturnPaths<'ctx> {
    /// The parameters, by name.
    pub inputs: Vec<(String, Sym<'ctx>)>,
    /// Each return point: the condition (including the function's
    /// `requires` and parameter refinements) under which it is reached, and
    /// the value returned there.
    pub returns: Vec<(BoolExpr<'ctx>, Sym<'ctx>)>,
}

/// Symbolically executes `func`'s body in `ctx` and collects its return
/// points. A function without a body to execute (an `extern` declaration)
/// returns a fresh value constrained by its `ensures` and return refinement.
/// No obligation is solved here.
pub(crate) fn return_paths<'ctx>(
    db: &dyn HirDatabase,
    func: Function,
    ctx: &'ctx Context,
) -> Option<ReturnPaths<'ctx>> {
    let frame = Frame::new(db, func);
    if frame.body.self_param().is_some() {
        return None;
    }
    let has_body =
        !func.is_extern(db) && !matches!(frame.body[frame.body.body_expr()], Expr::Missing);
    let mut exec = Exec {
        db,
        ctx,
        ret_refinement: refinement::return_refinement(db, func),
        frame,
        check_own: false,
        entry_vars: FxHashMap::default(),
        inputs: Vec::new(),
        pending: Vec::new(),
        loops: Vec::new(),
        fresh: 0,
        own_requires_checked: false,
        returns: Vec::new(),
        hole_values: FxHashMap::default(),
        fixed_inputs: None,
    };
    let mut state = exec.entry_state();
    if has_body {
        let body_expr = exec.frame.body.body_expr();
        if let Some(value) = exec.eval(&mut state, body_expr) {
            exec.check_return(&state, value);
        }
    } else {
        // Summarise the declaration by its contract, exactly as a caller
        // would see it.
        let ret_ty = func.ret_type(db);
        let result = exec.fresh_for_ty(&ret_ty, "result");
        let body = Arc::clone(&exec.frame.body);
        let mut vars = exec.entry_vars.clone();
        if let Some(result_pat) = body.contracts().result_pat {
            vars.insert(result_pat, result);
        }
        for &clause in &body.contracts().ensures {
            if let Ok(Sym::Bool(b)) = exec.pure(&exec.frame, &vars, clause) {
                state.pc = state.pc.and(b);
            }
        }
        if let Some(refinement) = &exec.ret_refinement {
            if let Ok(b) = refinement::encode(ctx, refinement, result) {
                state.pc = state.pc.and(b);
            }
        }
        exec.returns.push((state.pc, result));
    }
    Some(ReturnPaths {
        inputs: exec.inputs,
        returns: exec.returns,
    })
}

fn pat_name(body: &Body, pat: PatId, idx: usize) -> String {
    match &body[pat] {
        Pat::Bind { name } => name.to_string(),
        _ => format!("arg{idx}"),
    }
}

/// Whether anything below `expr` calls a function with a contract.
fn subtree_calls_contracted(db: &dyn HirDatabase, frame: &Frame, expr: ExprId) -> bool {
    let callee = match &frame.body[expr] {
        Expr::Call { callee, .. } => frame.callee(*callee),
        Expr::MethodCall { .. } => frame.infer.method_resolution(expr).map(Function::from),
        _ => None,
    };
    if callee.is_some_and(|f| has_call_contract(db, f)) {
        return true;
    }
    let mut any = false;
    frame.body[expr].walk_child_exprs(|child| any |= subtree_calls_contracted(db, frame, child));
    any
}

fn calls_contracted_function(db: &dyn HirDatabase, frame: &Frame) -> bool {
    frame.body.exprs().any(|(id, expr)| {
        let callee = match expr {
            Expr::Call { callee, .. } => frame.callee(*callee),
            Expr::MethodCall { .. } => frame.infer.method_resolution(id).map(Function::from),
            _ => None,
        };
        callee.is_some_and(|f| has_call_contract(db, f))
    })
}

/// Merges two values under a selector: `sel ? a : b`.
fn merge_sym<'ctx>(sel: BoolExpr<'ctx>, a: Sym<'ctx>, b: Sym<'ctx>) -> Sym<'ctx> {
    match (a, b) {
        (Sym::Int(x, ty), Sym::Int(y, _)) if x.is_identical(y) => Sym::Int(x, ty),
        (Sym::Int(x, ty), Sym::Int(y, _)) if x.width() == y.width() => {
            Sym::Int(sel.ite_bv(x, y), ty)
        }
        (Sym::Bool(x), Sym::Bool(y)) if x.is_identical(y) => Sym::Bool(x),
        (Sym::Bool(x), Sym::Bool(y)) => Sym::Bool(sel.ite_bool(x, y)),
        (Sym::Unit, Sym::Unit) => Sym::Unit,
        _ => Sym::Opaque,
    }
}

/// Merges two states reached on disjoint paths; `sel` holds on `a`'s.
fn merge_states<'ctx>(a: State<'ctx>, b: State<'ctx>, sel: BoolExpr<'ctx>) -> State<'ctx> {
    let mut vars = a.vars;
    for (pat, vb) in b.vars {
        let merged = match vars.get(&pat) {
            Some(&va) => merge_sym(sel, va, vb),
            None => vb,
        };
        vars.insert(pat, merged);
    }
    State {
        pc: a.pc.or(b.pc),
        vars,
    }
}

impl<'ctx> Exec<'_, 'ctx> {
    fn fresh_name(&mut self, hint: &str) -> String {
        self.fresh += 1;
        format!("{hint}!{}", self.fresh)
    }

    /// A fresh, unconstrained value of type `ty`.
    fn fresh_for_ty(&mut self, ty: &Ty, hint: &str) -> Sym<'ctx> {
        match kind_of(self.db, ty) {
            Kind::Int(int) => {
                let name = self.fresh_name(hint);
                Sym::Int(self.ctx.bv_var(&name, int_width(int)), int)
            }
            Kind::Bool => {
                let name = self.fresh_name(hint);
                Sym::Bool(self.ctx.bool_var(&name))
            }
            Kind::Unit => Sym::Unit,
            Kind::Other => Sym::Opaque,
        }
    }

    /// A fresh value of the same sort as `old`.
    fn havoc(&mut self, old: Sym<'ctx>, hint: &str) -> Sym<'ctx> {
        match old {
            Sym::Int(bv, ty) => {
                let name = self.fresh_name(hint);
                Sym::Int(self.ctx.bv_var(&name, bv.width()), ty)
            }
            Sym::Bool(_) => {
                let name = self.fresh_name(hint);
                Sym::Bool(self.ctx.bool_var(&name))
            }
            other => other,
        }
    }

    fn fresh_bool(&mut self, hint: &str) -> BoolExpr<'ctx> {
        let name = self.fresh_name(hint);
        self.ctx.bool_var(&name)
    }

    fn bool_of(&mut self, value: Sym<'ctx>, hint: &str) -> BoolExpr<'ctx> {
        match value {
            Sym::Bool(b) => b,
            _ => self.fresh_bool(hint),
        }
    }

    /// Parameters become fresh values; refinements and `requires` become
    /// the path condition every obligation is proven under.
    fn entry_state(&mut self) -> State<'ctx> {
        let body = Arc::clone(&self.frame.body);
        let mut vars = FxHashMap::default();
        let mut pc = self.ctx.bool_true();
        if let Some((self_pat, _)) = body.self_param() {
            vars.insert(*self_pat, Sym::Opaque);
        }
        let refinements = refinement::param_refinements(self.db, self.frame.func);
        for (idx, (pat, _)) in body.params().iter().enumerate() {
            let name = pat_name(&body, *pat, idx);
            let ty = self.frame.infer[*pat].clone();
            let fixed = self
                .fixed_inputs
                .as_ref()
                .and_then(|values| values.get(idx).copied().flatten());
            let value = match (fixed, kind_of(self.db, &ty)) {
                (Some(Value::Int { bits, width, .. }), Kind::Int(int)) => {
                    Sym::Int(self.ctx.bv_lit(bits, width), int)
                }
                (Some(Value::Bool(b)), Kind::Bool) => Sym::Bool(if b {
                    self.ctx.bool_true()
                } else {
                    self.ctx.bool_false()
                }),
                _ => self.fresh_for_ty(&ty, &name),
            };
            vars.insert(*pat, value);
            self.inputs.push((name, value));
            if let Some(Some(refinement)) = refinements.get(idx) {
                match refinement::encode(self.ctx, refinement, value) {
                    Ok(b) => pc = pc.and(b),
                    Err(reason) => self.pending.push(Pending {
                        kind: ObligationKind::UncheckableClause,
                        site: refinement.site.clone(),
                        clause: refinement.text.clone(),
                        premise: pc,
                        goal: Err(reason),
                        at_site: Vec::new(),
                        runtime_checked: false,
                    }),
                }
            }
        }
        self.entry_vars.clone_from(&vars);
        for &clause in &body.contracts().requires {
            match self.pure(&self.frame, &vars, clause) {
                Ok(Sym::Bool(b)) => pc = pc.and(b),
                other => {
                    let reason = match other {
                        Err(reason) => reason,
                        Ok(_) => "the clause is not a boolean the verifier can model".to_string(),
                    };
                    self.uncheckable(clause, reason, pc);
                }
            }
        }
        State { pc, vars }
    }

    fn uncheckable(&mut self, clause: ExprId, reason: String, premise: BoolExpr<'ctx>) {
        let Some(site) = self.frame.site(clause) else {
            return;
        };
        self.pending.push(Pending {
            kind: ObligationKind::UncheckableClause,
            site,
            clause: self.frame.text(self.db, clause),
            premise,
            goal: Err(reason),
            at_site: Vec::new(),
            runtime_checked: self.own_requires_checked,
        });
    }

    fn oblige(
        &mut self,
        state: &State<'ctx>,
        kind: ObligationKind,
        site: Option<InFile<SyntaxNodePtr>>,
        clause: String,
        goal: Result<Sym<'ctx>, String>,
        at_site: Vec<(String, Sym<'ctx>)>,
    ) {
        let Some(site) = site else {
            return;
        };
        let goal = match goal {
            Ok(Sym::Bool(b)) => Ok(b),
            Ok(_) => Err("the clause is not a boolean the verifier can model".to_string()),
            Err(reason) => Err(reason),
        };
        self.pending.push(Pending {
            kind,
            site,
            clause,
            premise: state.pc,
            goal,
            at_site,
            runtime_checked: false,
        });
    }

    // ---- pure evaluation (contract clauses, any frame) ----------------------

    /// Evaluates a side-effect-free expression -- a contract clause or a
    /// ghost initializer -- of `frame`'s body over `vars`.
    fn pure(
        &self,
        frame: &Frame,
        vars: &FxHashMap<PatId, Sym<'ctx>>,
        expr: ExprId,
    ) -> Result<Sym<'ctx>, String> {
        let unsupported = |what: &str| {
            format!(
                "`{}` uses {what}, which contract clauses do not support yet",
                frame.text(self.db, expr)
            )
        };
        match &frame.body[expr] {
            Expr::Literal(lit) => self
                .literal(frame, expr, lit)
                .ok_or_else(|| unsupported("a literal of an unmodelled type")),
            Expr::Path(path) => match frame.resolve_local(self.db, expr, path) {
                Some(pat) => match vars.get(&pat) {
                    Some(Sym::Opaque) | None => Err(format!(
                        "`{}` has a type the verifier does not model",
                        frame.text(self.db, expr)
                    )),
                    Some(value) => Ok(*value),
                },
                None => Err(unsupported("a name that is not a local or parameter")),
            },
            Expr::UnaryOp { expr: inner, op } => {
                let value = self.pure(frame, vars, *inner)?;
                match unary(*op, value) {
                    Sym::Opaque => Err(unsupported("an operator on an unmodelled type")),
                    v => Ok(v),
                }
            }
            Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(op),
            } => {
                if matches!(op, BinaryOp::Assignment { .. }) {
                    return Err(unsupported("an assignment"));
                }
                let l = self.pure(frame, vars, *lhs)?;
                let r = self.pure(frame, vars, *rhs)?;
                let value = match op {
                    BinaryOp::LogicOp(LogicOp::And) => match (l, r) {
                        (Sym::Bool(a), Sym::Bool(b)) => Sym::Bool(a.and(b)),
                        _ => Sym::Opaque,
                    },
                    BinaryOp::LogicOp(LogicOp::Or) => match (l, r) {
                        (Sym::Bool(a), Sym::Bool(b)) => Sym::Bool(a.or(b)),
                        _ => Sym::Opaque,
                    },
                    BinaryOp::ArithOp(op) => arith(*op, l, r),
                    BinaryOp::CmpOp(op) => compare(*op, l, r),
                    BinaryOp::Assignment { .. } => Sym::Opaque,
                };
                match value {
                    Sym::Opaque => Err(unsupported("an operator on an unmodelled type")),
                    v => Ok(v),
                }
            }
            Expr::If {
                condition,
                then_branch,
                else_branch: Some(else_branch),
            } => {
                let c = match self.pure(frame, vars, *condition)? {
                    Sym::Bool(b) => b,
                    _ => return Err(unsupported("a condition of an unmodelled type")),
                };
                let a = self.pure(frame, vars, *then_branch)?;
                let b = self.pure(frame, vars, *else_branch)?;
                match merge_sym(c, a, b) {
                    Sym::Opaque => Err(unsupported("branches of an unmodelled type")),
                    v => Ok(v),
                }
            }
            Expr::Block {
                statements,
                tail: Some(tail),
            } if statements.is_empty() => self.pure(frame, vars, *tail),
            Expr::Cast { expr: inner, .. } => {
                let value = self.pure(frame, vars, *inner)?;
                match cast(self.db, self.ctx, value, &frame.infer[expr]) {
                    Sym::Opaque => Err(unsupported("a conversion to an unmodelled type")),
                    v => Ok(v),
                }
            }
            Expr::Call { .. } | Expr::MethodCall { .. } => Err(unsupported("a call")),
            _ => Err(unsupported("an expression")),
        }
    }

    fn literal(&self, frame: &Frame, expr: ExprId, lit: &Literal) -> Option<Sym<'ctx>> {
        match lit {
            Literal::Bool(true) => Some(Sym::Bool(self.ctx.bool_true())),
            Literal::Bool(false) => Some(Sym::Bool(self.ctx.bool_false())),
            Literal::Int(int) => match kind_of(self.db, &frame.infer[expr]) {
                Kind::Int(ty) => Some(Sym::Int(self.ctx.bv_lit(int.value, int_width(ty)), ty)),
                _ => None,
            },
            Literal::String(_) | Literal::Float(_) | Literal::Nil => None,
        }
    }

    // ---- executable code ----------------------------------------------------

    /// Evaluates `expr`, updating `state`. `None` means control does not
    /// continue past it (`return`, `break`, a loop with no exit).
    fn eval(&mut self, state: &mut State<'ctx>, expr: ExprId) -> Option<Sym<'ctx>> {
        let body = Arc::clone(&self.frame.body);
        match &body[expr] {
            Expr::Missing => {
                let Some(&value) = self.hole_values.get(&expr) else {
                    return Some(Sym::Opaque);
                };
                // A `choose`'s value must satisfy its predicate wherever the
                // expression is reached.
                if let Some(choose) = self.frame.body.contracts().chooses.get(&expr).copied() {
                    let mut vars = state.vars.clone();
                    vars.insert(choose.binder, value);
                    let goal = self.pure(&self.frame, &vars, choose.predicate);
                    let text = self.frame.text(self.db, choose.predicate);
                    self.oblige(
                        state,
                        ObligationKind::ChoosePredicate,
                        self.frame.site(expr),
                        text,
                        goal,
                        vec![],
                    );
                }
                Some(value)
            }
            Expr::Literal(lit) => {
                let frame = &self.frame;
                Some(self.literal(frame, expr, lit).unwrap_or(Sym::Opaque))
            }
            Expr::Path(path) => Some(
                self.frame
                    .resolve_local(self.db, expr, path)
                    .and_then(|pat| state.vars.get(&pat).copied())
                    .unwrap_or(Sym::Opaque),
            ),
            Expr::UnaryOp { expr: inner, op } => {
                let value = self.eval(state, *inner)?;
                Some(unary(*op, value))
            }
            Expr::BinaryOp { lhs, rhs, op } => self.eval_binary(state, expr, *lhs, *rhs, *op),
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => self.eval_if(state, *condition, *then_branch, *else_branch),
            Expr::Block { statements, tail } => self.eval_block(state, expr, statements, *tail),
            Expr::Return { expr: value } => {
                let value = match value {
                    Some(v) => self.eval(state, *v)?,
                    None => Sym::Unit,
                };
                self.check_return(state, value);
                None
            }
            Expr::Break { expr: value } => {
                let value = match value {
                    Some(v) => self.eval(state, *v)?,
                    None => Sym::Unit,
                };
                if let Some(frame) = self.loops.last_mut() {
                    frame.breaks.push((state.clone(), value));
                }
                None
            }
            // The next iteration starts from here: `eval_loop`/`eval_for`
            // check the back-edge obligations on this state, as at the end
            // of the body.
            Expr::Continue => {
                if let Some(frame) = self.loops.last_mut() {
                    frame.continues.push(state.clone());
                }
                None
            }
            Expr::For {
                pat,
                iterable,
                body: loop_body,
            } => self.eval_for(state, expr, *pat, *iterable, *loop_body),
            Expr::Loop { body: loop_body } => self.eval_loop(state, expr, None, *loop_body),
            Expr::Match {
                expr: scrutinee,
                arms,
                ..
            } => {
                let value = self.eval(state, *scrutinee)?;
                let optional =
                    matches!(self.frame.infer[*scrutinee].interned(), TyKind::Optional(_));
                self.eval_arms(state, value, optional, arms)
            }
            Expr::While {
                condition,
                body: loop_body,
            } => self.eval_loop(state, expr, Some(*condition), *loop_body),
            Expr::Call { callee, args } => {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.eval(state, *arg)?);
                }
                // Builtins with a meaning the solver can see: `declassify`
                // is the identity, `ct_select` is an `ite`.
                match (self.frame.infer.builtin_call(expr), values.as_slice()) {
                    (Some(crate::BuiltinFn::Declassify), [value]) => return Some(*value),
                    (Some(crate::BuiltinFn::CtSelect), [cond, a, b]) => {
                        let c = self.bool_of(*cond, "ct_select");
                        return Some(merge_sym(c, *a, *b));
                    }
                    (Some(crate::BuiltinFn::Select), [cond, a, b]) => {
                        let c = self.bool_of(*cond, "select");
                        return Some(merge_sym(c, *a, *b));
                    }
                    (Some(crate::BuiltinFn::Likely | crate::BuiltinFn::Unlikely), [value]) => {
                        return Some(*value)
                    }
                    (
                        Some(
                            crate::BuiltinFn::ArrayFilled
                            | crate::BuiltinFn::Fma
                            | crate::BuiltinFn::Select
                            | crate::BuiltinFn::Likely
                            | crate::BuiltinFn::Unlikely,
                        ),
                        _,
                    ) => {
                        let ty = self.frame.infer[expr].clone();
                        return Some(self.fresh_for_ty(&ty, "builtin"));
                    }
                    (Some(_), _) => return Some(Sym::Unit),
                    (None, _) => {}
                }
                Some(match self.frame.callee(*callee) {
                    Some(f) => self.apply_callee(state, expr, f, None, &values),
                    None => self.fresh_for_ty(&self.frame.infer[expr].clone(), "call"),
                })
            }
            Expr::MethodCall { receiver, args, .. } => {
                let receiver_value = self.eval(state, *receiver)?;
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.eval(state, *arg)?);
                }
                Some(match self.frame.infer.method_resolution(expr) {
                    Some(f) => self.apply_callee(
                        state,
                        expr,
                        Function::from(f),
                        Some(receiver_value),
                        &values,
                    ),
                    None => self.fresh_for_ty(&self.frame.infer[expr].clone(), "call"),
                })
            }
            Expr::Cast { expr: inner, .. } => {
                let value = self.eval(state, *inner)?;
                Some(cast(self.db, self.ctx, value, &self.frame.infer[expr]))
            }
            Expr::Select { arms } => {
                for arm in arms {
                    for operand in arm.operands() {
                        self.eval(state, operand)?;
                    }
                }
                self.eval_select_arms(state, arms)
            }
            // The other endpoint chooses the branch: unknown, like a
            // `select`'s arm.
            Expr::Offer { endpoint, branches } => {
                self.eval(state, *endpoint)?;
                self.eval_offer_branches(state, branches)
            }
            // A closure's body runs later, if ever, with arguments nobody
            // knows here, so it is not evaluated at the literal. Its
            // obligations are therefore not checked: where there are any (a
            // verified function's own divisions, or a call to a function
            // with a contract), that is reported rather than passed. A call
            // through a function value is unknown and yields a fresh value
            // (see `Expr::Call` above).
            Expr::Closure {
                body: closure_body, ..
            } => {
                if self.check_own || subtree_calls_contracted(self.db, &self.frame, *closure_body) {
                    let text = self.frame.text(self.db, expr);
                    self.oblige(
                        state,
                        ObligationKind::UnverifiedClosure,
                        self.frame.site(expr),
                        text,
                        Err("the obligations in it (divisions, calls to functions with \
                             contracts) would go unchecked; move that code into a named function"
                            .to_string()),
                        vec![],
                    );
                }
                Some(self.fresh_for_ty(&self.frame.infer[expr].clone(), "closure"))
            }
            other => {
                // Fields, indexing, literals of composite types: evaluate
                // the operands for the obligations they carry, then forget
                // the value.
                let mut children = Vec::new();
                other.walk_child_exprs(|child| children.push(child));
                for child in children {
                    self.eval(state, child)?;
                }
                Some(self.fresh_for_ty(&self.frame.infer[expr].clone(), "value"))
            }
        }
    }

    fn eval_binary(
        &mut self,
        state: &mut State<'ctx>,
        expr: ExprId,
        lhs: ExprId,
        rhs: ExprId,
        op: Option<BinaryOp>,
    ) -> Option<Sym<'ctx>> {
        let Some(op) = op else {
            self.eval(state, lhs)?;
            self.eval(state, rhs)?;
            return Some(Sym::Opaque);
        };
        match op {
            BinaryOp::Assignment { op: compound } => {
                let value = self.eval(state, rhs)?;
                let body = Arc::clone(&self.frame.body);
                match &body[lhs] {
                    Expr::Path(path) => {
                        if let Some(pat) = self.frame.resolve_local(self.db, lhs, path) {
                            let new = match compound {
                                None => value,
                                Some(arith_op) => {
                                    let current =
                                        state.vars.get(&pat).copied().unwrap_or(Sym::Opaque);
                                    self.arith_checked(state, expr, rhs, arith_op, current, value)
                                }
                            };
                            state.vars.insert(pat, new);
                        }
                    }
                    place => {
                        // `a.b = v`, `a[i] = v`: the heap is not modelled,
                        // but the place's own operands may carry
                        // obligations.
                        let mut children = Vec::new();
                        place.walk_child_exprs(|child| children.push(child));
                        for child in children {
                            self.eval(state, child)?;
                        }
                    }
                }
                Some(Sym::Unit)
            }
            BinaryOp::LogicOp(logic) => {
                // Short-circuit: the right operand runs only when the left
                // one does not decide the result, so its obligations are
                // proven under that condition.
                let l = self.eval(state, lhs)?;
                let lb = self.bool_of(l, "lhs");
                let (runs_rhs, skips) = match logic {
                    LogicOp::And => (lb, lb.not()),
                    LogicOp::Or => (lb.not(), lb),
                };
                let mut rhs_state = state.clone();
                rhs_state.pc = state.pc.and(runs_rhs);
                let mut skip_state = state.clone();
                skip_state.pc = state.pc.and(skips);
                match self.eval(&mut rhs_state, rhs) {
                    None => {
                        *state = skip_state;
                        Some(Sym::Bool(match logic {
                            LogicOp::And => self.ctx.bool_false(),
                            LogicOp::Or => self.ctx.bool_true(),
                        }))
                    }
                    Some(r) => {
                        let rb = self.bool_of(r, "rhs");
                        *state = merge_states(rhs_state, skip_state, runs_rhs);
                        Some(Sym::Bool(match logic {
                            LogicOp::And => lb.and(rb),
                            LogicOp::Or => lb.or(rb),
                        }))
                    }
                }
            }
            BinaryOp::ArithOp(arith_op) => {
                let l = self.eval(state, lhs)?;
                let r = self.eval(state, rhs)?;
                Some(self.arith_checked(state, expr, rhs, arith_op, l, r))
            }
            BinaryOp::CmpOp(cmp) => {
                let l = self.eval(state, lhs)?;
                let r = self.eval(state, rhs)?;
                Some(compare(cmp, l, r))
            }
        }
    }

    /// Integer arithmetic, plus the division obligations of a verified
    /// function.
    fn arith_checked(
        &mut self,
        state: &State<'ctx>,
        expr: ExprId,
        divisor_expr: ExprId,
        op: ArithOp,
        l: Sym<'ctx>,
        r: Sym<'ctx>,
    ) -> Sym<'ctx> {
        if self.check_own && matches!(op, ArithOp::Divide | ArithOp::Remainder) {
            if let (Sym::Int(a, ty), Sym::Int(b, _)) = (l, r) {
                let width = a.width();
                let zero = self.ctx.bv_lit(0, width);
                let divisor = self.frame.text(self.db, divisor_expr);
                self.oblige(
                    state,
                    ObligationKind::DivisionByZero,
                    self.frame.site(expr),
                    divisor,
                    Ok(Sym::Bool(b.eq(zero).not())),
                    vec![],
                );
                if ty.signedness.is_signed() {
                    let min = self.ctx.bv_lit(1u128 << (width - 1), width);
                    let minus_one = self.ctx.bv_lit(u128::MAX, width);
                    let text = self.frame.text(self.db, expr);
                    self.oblige(
                        state,
                        ObligationKind::DivisionOverflow,
                        self.frame.site(expr),
                        text,
                        Ok(Sym::Bool(a.eq(min).and(b.eq(minus_one)).not())),
                        vec![],
                    );
                }
            }
        }
        arith(op, l, r)
    }

    fn eval_if(
        &mut self,
        state: &mut State<'ctx>,
        condition: ExprId,
        then_branch: ExprId,
        else_branch: Option<ExprId>,
    ) -> Option<Sym<'ctx>> {
        let c = self.eval(state, condition)?;
        let cb = self.bool_of(c, "cond");
        let mut then_state = state.clone();
        then_state.pc = state.pc.and(cb);
        let mut else_state = state.clone();
        else_state.pc = state.pc.and(cb.not());
        let then_value = self.eval(&mut then_state, then_branch);
        let else_value = match else_branch {
            Some(e) => self.eval(&mut else_state, e),
            None => Some(Sym::Unit),
        };
        match (then_value, else_value) {
            (None, None) => None,
            (Some(v), None) => {
                *state = then_state;
                Some(v)
            }
            (None, Some(v)) => {
                *state = else_state;
                Some(v)
            }
            (Some(a), Some(b)) => {
                *state = merge_states(then_state, else_state, cb);
                Some(merge_sym(cb, a, b))
            }
        }
    }

    /// The arms of a `match`, from the first. Patterns are not modelled:
    /// each arm but the last is entered under a fresh condition, so what is
    /// proven inside it holds whichever values reach it, and the last arm
    /// takes the remaining paths (inference has checked the match covers
    /// every value). A name a pattern binds is the scrutinee itself when the
    /// whole pattern is that name, and a fresh value otherwise.
    fn eval_arms(
        &mut self,
        state: &mut State<'ctx>,
        value: Sym<'ctx>,
        optional: bool,
        arms: &[MatchArm],
    ) -> Option<Sym<'ctx>> {
        let (first, rest) = arms.split_first()?;
        let mut arm_state = state.clone();
        let bound = if optional {
            self.fresh_for_ty(&self.frame.infer[first.pat].clone(), "bound")
        } else {
            value
        };
        self.bind(&mut arm_state, first.pat, bound);
        if rest.is_empty() {
            let arm_value = self.eval(&mut arm_state, first.expr);
            *state = arm_state;
            return arm_value;
        }
        let cb = self.fresh_bool("arm");
        arm_state.pc = state.pc.and(cb);
        let mut rest_state = state.clone();
        rest_state.pc = state.pc.and(cb.not());
        let arm_value = self.eval(&mut arm_state, first.expr);
        let rest_value = self.eval_arms(&mut rest_state, value, optional, rest);
        match (arm_value, rest_value) {
            (None, None) => None,
            (Some(v), None) => {
                *state = arm_state;
                Some(v)
            }
            (None, Some(v)) => {
                *state = rest_state;
                Some(v)
            }
            (Some(a), Some(b)) => {
                *state = merge_states(arm_state, rest_state, cb);
                Some(merge_sym(cb, a, b))
            }
        }
    }

    /// The arms of a `select`, after its operands. Which arm runs depends
    /// on other tasks, so each is a nondeterministic choice, and a received
    /// value is unconstrained. No arms: the `select` waits forever.
    fn eval_select_arms(
        &mut self,
        state: &mut State<'ctx>,
        arms: &[SelectArm],
    ) -> Option<Sym<'ctx>> {
        let (arm, rest) = arms.split_first()?;
        let run = |this: &mut Self, state: &mut State<'ctx>| {
            if let Some(binding) = arm.binding {
                let ty = this.frame.infer[binding].clone();
                let value = this.fresh_for_ty(&ty, "received");
                this.bind(state, binding, value);
            }
            this.eval(state, arm.body)
        };
        if rest.is_empty() {
            return run(self, state);
        }
        let choice = self.fresh_bool("select");
        let mut this_state = state.clone();
        this_state.pc = state.pc.and(choice);
        let mut rest_state = state.clone();
        rest_state.pc = state.pc.and(choice.not());
        let this_value = run(self, &mut this_state);
        let rest_value = self.eval_select_arms(&mut rest_state, rest);
        match (this_value, rest_value) {
            (None, None) => None,
            (Some(v), None) => {
                *state = this_state;
                Some(v)
            }
            (None, Some(v)) => {
                *state = rest_state;
                Some(v)
            }
            (Some(a), Some(b)) => {
                *state = merge_states(this_state, rest_state, choice);
                Some(merge_sym(choice, a, b))
            }
        }
    }

    fn eval_offer_branches(
        &mut self,
        state: &mut State<'ctx>,
        branches: &[ExprId],
    ) -> Option<Sym<'ctx>> {
        let (&branch, rest) = branches.split_first()?;
        if rest.is_empty() {
            return self.eval(state, branch);
        }
        let choice = self.fresh_bool("offer");
        let mut this_state = state.clone();
        this_state.pc = state.pc.and(choice);
        let mut rest_state = state.clone();
        rest_state.pc = state.pc.and(choice.not());
        let this_value = self.eval(&mut this_state, branch);
        let rest_value = self.eval_offer_branches(&mut rest_state, rest);
        match (this_value, rest_value) {
            (None, None) => None,
            (Some(v), None) => {
                *state = this_state;
                Some(v)
            }
            (None, Some(v)) => {
                *state = rest_state;
                Some(v)
            }
            (Some(a), Some(b)) => {
                *state = merge_states(this_state, rest_state, choice);
                Some(merge_sym(choice, a, b))
            }
        }
    }

    fn eval_block(
        &mut self,
        state: &mut State<'ctx>,
        block: ExprId,
        statements: &[Statement],
        tail: Option<ExprId>,
    ) -> Option<Sym<'ctx>> {
        let body = Arc::clone(&self.frame.body);
        let ghosts = body.contracts().ghost_lets.get(&block);
        let ghosts_at = |idx: usize| {
            ghosts
                .into_iter()
                .flatten()
                .filter(move |(pos, _)| *pos == idx)
                .map(|(_, stmt)| stmt)
        };
        for idx in 0..=statements.len() {
            for ghost in ghosts_at(idx) {
                if let Statement::Let {
                    pat, initializer, ..
                } = ghost
                {
                    let value = match initializer {
                        Some(init) => self
                            .pure(&self.frame, &state.vars, *init)
                            .unwrap_or(Sym::Opaque),
                        None => self.fresh_for_ty(&self.frame.infer[*pat].clone(), "ghost"),
                    };
                    self.bind(state, *pat, value);
                }
            }
            let Some(stmt) = statements.get(idx) else {
                break;
            };
            match stmt {
                Statement::Let {
                    pat, initializer, ..
                } => {
                    let value = match initializer {
                        Some(init) => self.eval(state, *init)?,
                        None => self.fresh_for_ty(&self.frame.infer[*pat].clone(), "uninit"),
                    };
                    self.bind(state, *pat, value);
                }
                Statement::Expr(e) => {
                    self.eval(state, *e)?;
                }
            }
        }
        match tail {
            Some(t) => self.eval(state, t),
            None => Some(Sym::Unit),
        }
    }

    fn bind(&mut self, state: &mut State<'ctx>, pat: PatId, value: Sym<'ctx>) {
        let body = Arc::clone(&self.frame.body);
        match &body[pat] {
            Pat::Bind { .. } => {
                state.vars.insert(pat, value);
            }
            other => {
                // Destructuring is not modelled: each binding gets a fresh
                // value of its own type.
                let mut children = Vec::new();
                other.walk_child_pats(|child| children.push(child));
                for child in children {
                    let fresh = self.fresh_for_ty(&self.frame.infer[child].clone(), "part");
                    self.bind(state, child, fresh);
                }
            }
        }
    }

    /// Every local assigned anywhere inside `root`.
    fn assigned_locals(&self, root: ExprId) -> FxHashSet<PatId> {
        let mut out = FxHashSet::default();
        let mut stack = vec![root];
        while let Some(expr) = stack.pop() {
            let node = &self.frame.body[expr];
            if let Expr::BinaryOp {
                lhs,
                op: Some(BinaryOp::Assignment { .. }),
                ..
            } = node
            {
                if let Expr::Path(path) = &self.frame.body[*lhs] {
                    if let Some(pat) = self.frame.resolve_local(self.db, *lhs, path) {
                        out.insert(pat);
                    }
                }
            }
            node.walk_child_exprs(|child| stack.push(child));
        }
        out
    }

    /// A loop, cut at its invariant:
    ///
    /// 1. the invariants must hold on entry;
    /// 2. every local the loop assigns is replaced by a fresh value, and the
    ///    invariants are assumed -- this state stands for "the head of an
    ///    arbitrary iteration";
    /// 3. from there, with the condition true, one pass of the body must
    ///    re-establish the invariants and strictly decrease the measure, which
    ///    must not be negative at the head -- at the end of the body and at
    ///    every `continue`, both of which start the next iteration;
    /// 4. after the loop: the head state with the condition false, merged with
    ///    every `break`.
    fn eval_loop(
        &mut self,
        state: &mut State<'ctx>,
        loop_expr: ExprId,
        condition: Option<ExprId>,
        loop_body: ExprId,
    ) -> Option<Sym<'ctx>> {
        let contract = self
            .frame
            .body
            .contracts()
            .loops
            .get(&loop_expr)
            .cloned()
            .unwrap_or_default();

        if self.check_own {
            for &inv in &contract.invariants {
                let goal = self.pure(&self.frame, &state.vars, inv);
                let text = self.frame.text(self.db, inv);
                self.oblige(
                    state,
                    ObligationKind::InvariantOnEntry,
                    self.frame.site(inv),
                    text,
                    goal,
                    vec![],
                );
            }
        }

        let assigned = self.assigned_locals(loop_expr);
        let mut assigned: Vec<PatId> = assigned.into_iter().collect();
        assigned.sort_by_key(|pat| u32::from(pat.into_raw()));
        for pat in assigned {
            if let Some(old) = state.vars.get(&pat).copied() {
                let name = pat_name(&self.frame.body, pat, 0);
                let fresh = self.havoc(old, &name);
                state.vars.insert(pat, fresh);
            }
        }
        for &inv in &contract.invariants {
            if let Ok(Sym::Bool(b)) = self.pure(&self.frame, &state.vars, inv) {
                state.pc = state.pc.and(b);
            }
        }

        let c = match condition {
            Some(cond) => {
                let value = self.eval(state, cond)?;
                self.bool_of(value, "loop_cond")
            }
            None => self.ctx.bool_true(),
        };
        let mut body_state = state.clone();
        body_state.pc = state.pc.and(c);

        let measure_at_head = contract.decreases.map(|measure| {
            let value = self.pure(&self.frame, &body_state.vars, measure);
            (measure, value)
        });
        if let Some((measure, value)) = &measure_at_head {
            let text = self.frame.text(self.db, *measure);
            let goal = match value {
                Ok(Sym::Int(m, ty)) if ty.signedness.is_signed() => {
                    Ok(Sym::Bool(m.sge(self.ctx.bv_lit(0, m.width()))))
                }
                Ok(Sym::Int(..)) => Ok(Sym::Bool(self.ctx.bool_true())),
                Ok(_) => Err("a termination measure must be an integer".to_string()),
                Err(reason) => Err(reason.clone()),
            };
            self.oblige(
                &body_state,
                ObligationKind::MeasureNonNegative,
                self.frame.site(*measure),
                text,
                goal,
                vec![],
            );
        }

        self.loops.push(LoopFrame::default());
        let end = self.eval(&mut body_state, loop_body);
        let LoopFrame { breaks, continues } = self.loops.pop().unwrap_or_default();

        // Every way into the next iteration: the end of the body and each
        // `continue`. Each must re-establish the invariants and decrease the
        // measure.
        let back_edges = end.map(|_| body_state).into_iter().chain(continues);
        for body_state in back_edges {
            if self.check_own {
                for &inv in &contract.invariants {
                    let goal = self.pure(&self.frame, &body_state.vars, inv);
                    let text = self.frame.text(self.db, inv);
                    self.oblige(
                        &body_state,
                        ObligationKind::InvariantPreserved,
                        self.frame.site(inv),
                        text,
                        goal,
                        vec![],
                    );
                }
            }
            if let Some((measure, Ok(Sym::Int(before, ty)))) = &measure_at_head {
                let (measure, before, ty) = (*measure, *before, *ty);
                let after = self.pure(&self.frame, &body_state.vars, measure);
                let goal = match after {
                    Ok(Sym::Int(after, _)) if ty.signedness.is_signed() => {
                        Ok(Sym::Bool(after.slt(before)))
                    }
                    Ok(Sym::Int(after, _)) => Ok(Sym::Bool(after.ult(before))),
                    Ok(_) => Err("a termination measure must be an integer".to_string()),
                    Err(reason) => Err(reason),
                };
                let text = self.frame.text(self.db, measure);
                self.oblige(
                    &body_state,
                    ObligationKind::MeasureDecreases,
                    self.frame.site(measure),
                    text,
                    goal,
                    vec![],
                );
            }
        }

        let mut exits = Vec::new();
        if condition.is_some() {
            let mut exit = state.clone();
            exit.pc = state.pc.and(c.not());
            exits.push((exit, Sym::Unit));
        }
        exits.extend(breaks);
        let mut exits = exits.into_iter();
        let (mut merged, mut value) = exits.next()?;
        for (next_state, next_value) in exits {
            let sel = merged.pc;
            value = merge_sym(sel, value, next_value);
            merged = merge_states(merged, next_state, sel);
        }
        *state = merged;
        Some(if condition.is_some() {
            Sym::Unit
        } else {
            value
        })
    }

    /// A `for` loop. The grammar gives it no `invariant` or `decreases`, so
    /// it is cut like a `while` without them:
    ///
    /// 1. the iterable is evaluated once, before the first iteration (a range's
    ///    bounds, or the array);
    /// 2. every local the body assigns is replaced by a fresh value: this state
    ///    stands for the head of an arbitrary iteration, and also for the state
    ///    after the last one (including after none);
    /// 3. the loop variable is a fresh value -- for a range, one assumed to lie
    ///    in it (`start <= i < end`, or `<= end` for `..=`), which is all a
    ///    counted loop guarantees; for an array, an unknown element -- and one
    ///    pass of the body is evaluated from there, for the obligations inside
    ///    it;
    /// 4. after the loop: the havocked head state. It already covers every
    ///    `break` (each leaves with values the fresh ones can take), and the
    ///    body's `break`s and `continue`s bind to this loop, not an outer one.
    ///
    /// A `for` loop always terminates (its iteration count is fixed before
    /// the first iteration), so it has no measure to check.
    fn eval_for(
        &mut self,
        state: &mut State<'ctx>,
        for_expr: ExprId,
        pat: PatId,
        iterable: ExprId,
        loop_body: ExprId,
    ) -> Option<Sym<'ctx>> {
        let body = Arc::clone(&self.frame.body);
        let range = if let Expr::Range {
            start,
            end,
            inclusive,
        } = &body[iterable]
        {
            let start = self.eval(state, *start)?;
            let end = self.eval(state, *end)?;
            Some((start, end, *inclusive))
        } else {
            self.eval(state, iterable)?;
            None
        };

        let mut assigned: Vec<PatId> = self.assigned_locals(for_expr).into_iter().collect();
        assigned.sort_by_key(|pat| u32::from(pat.into_raw()));
        for local in assigned {
            if let Some(old) = state.vars.get(&local).copied() {
                let name = pat_name(&self.frame.body, local, 0);
                let fresh = self.havoc(old, &name);
                state.vars.insert(local, fresh);
            }
        }

        let mut body_state = state.clone();
        let element = self.fresh_for_ty(&self.frame.infer[pat].clone(), "for");
        if let (Some((Sym::Int(start, ty), Sym::Int(end, _), inclusive)), Sym::Int(i, _)) =
            (range, element)
        {
            if start.width() == i.width() && end.width() == i.width() {
                let signed = ty.signedness.is_signed();
                let from = if signed { start.sle(i) } else { start.ule(i) };
                let to = match (inclusive, signed) {
                    (false, true) => i.slt(end),
                    (false, false) => i.ult(end),
                    (true, true) => i.sle(end),
                    (true, false) => i.ule(end),
                };
                body_state.pc = body_state.pc.and(from).and(to);
            }
        }
        self.bind(&mut body_state, pat, element);

        // The end of the body and every `continue` lead to the next
        // iteration, whose head the havocked state already stands for:
        // nothing is checked there.
        self.loops.push(LoopFrame::default());
        self.eval(&mut body_state, loop_body);
        self.loops.pop();
        Some(Sym::Unit)
    }

    /// A call: the callee's `requires` and parameter refinements are
    /// obligations here; its result is a fresh value about which its
    /// `ensures` and return refinement are assumed.
    fn apply_callee(
        &mut self,
        state: &mut State<'ctx>,
        call: ExprId,
        callee: Function,
        receiver: Option<Sym<'ctx>>,
        args: &[Sym<'ctx>],
    ) -> Sym<'ctx> {
        let callee_frame = Frame::new(self.db, callee);
        let callee_body = Arc::clone(&callee_frame.body);
        let callee_name = callee.name(self.db).to_string();
        let mut vars = FxHashMap::default();
        if let (Some((self_pat, _)), Some(receiver)) = (callee_body.self_param(), receiver) {
            vars.insert(*self_pat, receiver);
        }
        let mut named_args = Vec::new();
        for (idx, ((pat, _), value)) in callee_body.params().iter().zip(args).enumerate() {
            vars.insert(*pat, *value);
            named_args.push((pat_name(&callee_body, *pat, idx), *value));
        }

        let site = self.frame.site(call);
        for (idx, refinement) in refinement::param_refinements(self.db, callee)
            .into_iter()
            .enumerate()
        {
            let (Some(refinement), Some(arg)) = (refinement, args.get(idx)) else {
                continue;
            };
            let param = named_args
                .get(idx)
                .map_or_else(|| format!("#{idx}"), |(n, _)| n.clone());
            let goal = refinement::encode(self.ctx, &refinement, *arg).map(Sym::Bool);
            self.oblige(
                state,
                ObligationKind::RefinedArgument {
                    callee: callee_name.clone(),
                    param: param.clone(),
                },
                site.clone(),
                refinement.text.clone(),
                goal,
                vec![(param, *arg)],
            );
        }

        let callee_checked = super::is_runtime_checked(self.db, callee);
        for &clause in &callee_body.contracts().requires {
            // A clause the callee's own diagnostics already reject as
            // uncheckable is not reported again at every call.
            if let Ok(Sym::Bool(b)) = self.pure(&callee_frame, &vars, clause) {
                self.oblige(
                    state,
                    ObligationKind::Precondition {
                        callee: callee_name.clone(),
                    },
                    site.clone(),
                    callee_frame.text(self.db, clause),
                    Ok(Sym::Bool(b)),
                    named_args.clone(),
                );
                if callee_checked {
                    if let Some(last) = self.pending.last_mut() {
                        last.runtime_checked = true;
                    }
                }
            }
        }

        let ret_ty = callee.ret_type(self.db);
        let result = self.fresh_for_ty(&ret_ty, &format!("{callee_name}_result"));
        if let Some(result_pat) = callee_body.contracts().result_pat {
            vars.insert(result_pat, result);
        }
        for &clause in &callee_body.contracts().ensures {
            if let Ok(Sym::Bool(b)) = self.pure(&callee_frame, &vars, clause) {
                state.pc = state.pc.and(b);
            }
        }
        if let Some(refinement) = refinement::return_refinement(self.db, callee) {
            if let Ok(b) = refinement::encode(self.ctx, &refinement, result) {
                state.pc = state.pc.and(b);
            }
        }
        result
    }

    /// A return point: the postconditions and the return refinement.
    fn check_return(&mut self, state: &State<'ctx>, value: Sym<'ctx>) {
        self.returns.push((state.pc, value));
        if !self.check_own {
            return;
        }
        let body = Arc::clone(&self.frame.body);
        let mut vars = self.entry_vars.clone();
        if let Some(result_pat) = body.contracts().result_pat {
            vars.insert(result_pat, value);
        }
        for &clause in &body.contracts().ensures {
            let goal = self.pure(&self.frame, &vars, clause);
            let text = self.frame.text(self.db, clause);
            self.oblige(
                state,
                ObligationKind::Postcondition,
                self.frame.site(clause),
                text,
                goal,
                vec![("result".to_string(), value)],
            );
        }
        if let Some(refinement) = self.ret_refinement.clone() {
            let goal = refinement::encode(self.ctx, &refinement, value).map(Sym::Bool);
            self.oblige(
                state,
                ObligationKind::RefinedReturn,
                Some(refinement.site.clone()),
                refinement.text.clone(),
                goal,
                vec![("result".to_string(), value)],
            );
        }
    }

    /// Hands every pending obligation to Z3, one push/pop scope each, on a
    /// single solver. Obligations that share a kind, a site and a clause
    /// (one postcondition checked at several returns) are reported once,
    /// with the first failure found.
    ///
    /// With `record`, also returns the SMT-LIB2 text of each query, grouped
    /// per reported obligation (see [`verify_recording`]); otherwise the
    /// second vector is empty.
    fn solve(self, record: bool) -> (Vec<Obligation>, Vec<Vec<String>>) {
        let solver = self.ctx.solver();
        let mut results: Vec<Obligation> = Vec::new();
        let mut queries: Vec<Vec<String>> = Vec::new();
        let mut index: FxHashMap<(ObligationKind, InFile<SyntaxNodePtr>, String), usize> =
            FxHashMap::default();

        for pending in self.pending {
            let key = (
                pending.kind.clone(),
                pending.site.clone(),
                pending.clause.clone(),
            );
            if let Some(&existing) = index.get(&key) {
                if !matches!(results[existing].verdict, Verdict::Proven) {
                    continue;
                }
            }
            let mut query = None;
            let verdict = match pending.goal {
                Err(reason) => Verdict::Unknown(UnknownCause::Unsupported(reason)),
                Ok(goal) => {
                    if record {
                        // Exactly what is asserted below: the premise, then
                        // the negated goal. `unsat` is the proof.
                        query = Some(self.ctx.to_smtlib2(&[pending.premise], goal.not()));
                    }
                    solver.push();
                    solver.assert(pending.premise);
                    solver.assert(goal.not());
                    let verdict = match solver.check_detailed() {
                        CheckOutcome::Unsat => Verdict::Proven,
                        CheckOutcome::Sat => match solver.model() {
                            Some(model) => {
                                let read = |values: &[(String, Sym<'ctx>)]| {
                                    values
                                        .iter()
                                        .filter_map(|(name, sym)| {
                                            let value = match *sym {
                                                Sym::Int(bv, ty) => Value::Int {
                                                    bits: model.eval_bv_bits(bv)?,
                                                    signed: ty.signedness.is_signed(),
                                                    width: bv.width(),
                                                },
                                                Sym::Bool(b) => Value::Bool(model.eval_bool(b)?),
                                                Sym::Unit | Sym::Opaque => return None,
                                            };
                                            Some((name.clone(), value))
                                        })
                                        .collect()
                                };
                                Verdict::Refuted(Counterexample {
                                    inputs: read(&self.inputs),
                                    at_site: read(&pending.at_site),
                                })
                            }
                            None => Verdict::Unknown(UnknownCause::Solver(
                                "no model for a satisfiable query".to_string(),
                            )),
                        },
                        CheckOutcome::Unknown(UnknownReason::ResourceLimit) => {
                            Verdict::Unknown(UnknownCause::ResourceLimit)
                        }
                        CheckOutcome::Unknown(UnknownReason::Other(reason)) => {
                            Verdict::Unknown(UnknownCause::Solver(reason))
                        }
                    };
                    solver.pop(1);
                    verdict
                }
            };
            if let Some(&existing) = index.get(&key) {
                results[existing].verdict = verdict;
                if record {
                    queries[existing].extend(query);
                }
            } else {
                index.insert(key, results.len());
                results.push(Obligation {
                    kind: pending.kind,
                    site: pending.site,
                    clause: pending.clause,
                    verdict,
                    runtime_checked: pending.runtime_checked,
                });
                if record {
                    queries.push(query.into_iter().collect());
                }
            }
        }
        (results, queries)
    }
}

fn unary(op: UnaryOp, value: Sym<'_>) -> Sym<'_> {
    match (op, value) {
        (UnaryOp::Neg, Sym::Int(bv, ty)) => Sym::Int(bv.neg(), ty),
        (UnaryOp::BitNot, Sym::Int(bv, ty)) => Sym::Int(bv.bitnot(), ty),
        (UnaryOp::Not, Sym::Bool(b)) => Sym::Bool(b.not()),
        _ => Sym::Opaque,
    }
}

/// A shift amount as the language defines it: modulo the operand's width,
/// which is a power of two, so its low `log2(width)` bits read as unsigned
/// (`agents/USING_CODIRA.md`, "Integer arithmetic"). Z3's shifts give 0 (or
/// all sign bits) for an amount of the width or more, which is not what the
/// generated code computes.
fn shift_amount(amount: BvExpr<'_>) -> BvExpr<'_> {
    let width = amount.width();
    let bits = width.trailing_zeros();
    if bits == 0 || !width.is_power_of_two() {
        return amount;
    }
    amount.extract(bits - 1, 0).zero_ext(width - bits)
}

fn arith<'ctx>(op: ArithOp, l: Sym<'ctx>, r: Sym<'ctx>) -> Sym<'ctx> {
    match (l, r) {
        (Sym::Int(a, ty), Sym::Int(b, rty)) => {
            let b = b.resize(a.width(), rty.signedness.is_signed());
            let signed = ty.signedness.is_signed();
            Sym::Int(
                match op {
                    ArithOp::Add => a.add(b),
                    ArithOp::Subtract => a.sub(b),
                    ArithOp::Multiply => a.mul(b),
                    ArithOp::Divide if signed => a.sdiv(b),
                    ArithOp::Divide => a.udiv(b),
                    ArithOp::Remainder if signed => a.srem(b),
                    ArithOp::Remainder => a.urem(b),
                    ArithOp::LeftShift => a.shl(shift_amount(b)),
                    ArithOp::RightShift if signed => a.ashr(shift_amount(b)),
                    ArithOp::RightShift => a.lshr(shift_amount(b)),
                    ArithOp::BitAnd => a.bitand(b),
                    ArithOp::BitOr => a.bitor(b),
                    ArithOp::BitXor => a.bitxor(b),
                },
                ty,
            )
        }
        (Sym::Bool(a), Sym::Bool(b)) => match op {
            ArithOp::BitAnd => Sym::Bool(a.and(b)),
            ArithOp::BitOr => Sym::Bool(a.or(b)),
            ArithOp::BitXor => Sym::Bool(a.xor(b)),
            _ => Sym::Opaque,
        },
        _ => Sym::Opaque,
    }
}

fn compare<'ctx>(op: CmpOp, l: Sym<'ctx>, r: Sym<'ctx>) -> Sym<'ctx> {
    match (l, r) {
        (Sym::Int(a, ty), Sym::Int(b, _)) if a.width() == b.width() => {
            let signed = ty.signedness.is_signed();
            Sym::Bool(match op {
                CmpOp::Eq { negated: false } => a.eq(b),
                CmpOp::Eq { negated: true } => a.eq(b).not(),
                CmpOp::Ord { ordering, strict } => match (ordering, strict, signed) {
                    (Ordering::Less, true, true) => a.slt(b),
                    (Ordering::Less, true, false) => a.ult(b),
                    (Ordering::Less, false, true) => a.sle(b),
                    (Ordering::Less, false, false) => a.ule(b),
                    (Ordering::Greater, true, true) => a.sgt(b),
                    (Ordering::Greater, true, false) => a.ugt(b),
                    (Ordering::Greater, false, true) => a.sge(b),
                    (Ordering::Greater, false, false) => a.uge(b),
                },
            })
        }
        (Sym::Bool(a), Sym::Bool(b)) => match op {
            CmpOp::Eq { negated: false } => Sym::Bool(a.iff(b)),
            CmpOp::Eq { negated: true } => Sym::Bool(a.xor(b)),
            CmpOp::Ord { .. } => Sym::Opaque,
        },
        _ => Sym::Opaque,
    }
}

/// `value as ty`, for the integer conversions (spec section 18.2: truncate
/// or extend by the source signedness) and `bool as int`.
fn cast<'ctx>(
    db: &dyn HirDatabase,
    ctx: &'ctx Context,
    value: Sym<'ctx>,
    target: &Ty,
) -> Sym<'ctx> {
    match (value, kind_of(db, target)) {
        (Sym::Int(bv, from), Kind::Int(to)) => {
            Sym::Int(bv.resize(int_width(to), from.signedness.is_signed()), to)
        }
        (Sym::Bool(b), Kind::Int(to)) => {
            let width = int_width(to);
            Sym::Int(b.ite_bv(ctx.bv_lit(1, width), ctx.bv_lit(0, width)), to)
        }
        (Sym::Bool(b), Kind::Bool) => Sym::Bool(b),
        _ => Sym::Opaque,
    }
}
