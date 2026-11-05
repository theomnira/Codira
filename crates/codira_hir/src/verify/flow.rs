//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Information-flow and constant-time checking for `secret` data.
//!
//! # The surface
//!
//! * `secret T` -- a type qualifier on a parameter, a return type or a `let`
//!   ascription. It erases to `T` for type checking and code generation; only
//!   this checker reads it.
//! * `declassify(e)` -- a builtin that returns `e` unchanged and marks it
//!   public. The one sanctioned way out.
//! * `ct_select(c, a, b)` -- a builtin `c ? a : b` that code generation emits
//!   as a single LLVM `select` (marked `!unpredictable`), never a branch. The
//!   sanctioned way to choose on a secret.
//! * `@constant_time` on a function, or `oblivious func` (which is
//!   `@constant_time` with every parameter and the result `secret`), turns on
//!   the timing rules below.
//!
//! # What is checked
//!
//! Taint starts at secret parameters, secret `let`s and calls to functions
//! with a secret result, and flows through every operator, assignment and
//! `ct_select`; assignments under a branch whose condition is secret are
//! tainted too (an implicit flow). `declassify` stops it. Then, everywhere:
//!
//! * a secret value may not reach a non-secret sink: returned from a function
//!   whose result is not `secret`, passed to a non-`secret` parameter
//!   (including `print`/`println`), bound by a `let` declared with a non-secret
//!   type, stored through a field or index, or sent on a channel (plain or in a
//!   `select` arm);
//!
//! and in a `@constant_time`/`oblivious` function, where the time taken must
//! not depend on secrets:
//!
//! * no `if`/`while` condition, no `match` (or `if let`/`while let`) scrutinee,
//!   and no left operand of `&&`/`||` (all branch), may be secret -- this
//!   includes loop bounds;
//! * no index may be secret (the address of a memory access leaks through the
//!   cache);
//! * no operand of `/` or `%` may be secret (integer division takes
//!   data-dependent time on mainstream CPUs);
//! * a secret may only be passed to a function that is itself `@constant_time`
//!   (or to the builtins above).
//!
//! # Function values
//!
//! A closure that captures a secret is itself secret: so is the result of
//! calling a secret function value, and a secret closure cannot be returned
//! from a non-`secret` function or stored in a field, like any other secret.
//! A function value's parameters are never `secret` (a function type cannot
//! say so), so passing a secret to one is reported. A constant-time function
//! neither creates nor calls function values: whatever is called through one
//! is not known to be constant-time. Inside a closure body, `return` leaves
//! the closure, so it is not checked against the enclosing function's
//! result; the closure's own taint carries the value out.
//!
//! # Threat model, honestly
//!
//! What is verified is the *source* (these rules) and, for `ct_select`, the
//! LLVM IR the compiler emits (a `select`, not a `br`). What is not:
//! LLVM's backend may still turn a `select` into a branch (x86's cmov
//! converter, for instance, if it judges the condition predictable --
//! `!unpredictable` asks it not to, it is not a guarantee), and nothing
//! here models micro-architectural effects beyond branches, memory
//! addresses and division: multiplication, shifts and memory loads at
//! public addresses are assumed constant-time, which holds on mainstream
//! x86-64 and `AArch64` cores but is not universal. Speculative execution is
//! out of scope. A checked function is constant-time *at the source level*.
//!
//! `codira build --check-constant-time` (`codira_ctcheck`) checks the
//! machine code of these functions afterwards, for specific rules only
//! (conditional branches, variable-time instructions, unchecked calls and,
//! on x86-64, secret-dependent addresses); see that crate for what it does
//! not check. It does find lowered branches: at `-O0` on x86-64 a
//! `ct_select` on 8-bit integers becomes a jump on the condition.

use std::sync::Arc;

use codira_syntax::ast::{self, AstNode, AttributeOwner, TypeAscriptionOwner};
use rustc_hash::FxHashSet;

use super::exec::Frame;
use crate::{
    code_model::Function,
    diagnostics::{DiagnosticSink, SecretFlow},
    expr::{ArithOp, BinaryOp, LogicOp, Statement},
    BuiltinFn, Expr, ExprId, HasSource, HirDatabase, Pat, PatId,
};

const SECRET_ON_CHANNEL: &str =
    "secret value sent on a channel: what a channel delivers is public (declassify it first if \
     that is intended)";

/// What a function's signature says about secrets.
#[derive(Debug, Clone, Default)]
pub(crate) struct SecretSignature {
    /// Per ordinary parameter: is it `secret`?
    pub params: Vec<bool>,
    pub result: bool,
    /// `@constant_time` or `oblivious`.
    pub constant_time: bool,
}

fn is_secret_type(type_ref: Option<ast::TypeRef>) -> bool {
    matches!(
        type_ref.map(|t| t.kind()),
        Some(ast::TypeRefKind::SecretType(_))
    )
}

pub(crate) fn secret_signature(db: &dyn HirDatabase, func: Function) -> SecretSignature {
    let src = func.source(db).value;
    let oblivious = src.is_oblivious();
    let constant_time = oblivious
        || src.attribute_list().is_some_and(|attrs| {
            attrs.attributes().any(|attr| {
                attr.path().and_then(|p| p.segment()).is_some_and(|s| {
                    matches!(
                        s.kind(),
                        Some(ast::PathSegmentKind::Name(n)) if n.text() == "constant_time"
                    )
                })
            })
        });
    let params = src
        .param_list()
        .map(|list| {
            list.params()
                .map(|p| oblivious || is_secret_type(p.ascribed_type()))
                .collect()
        })
        .unwrap_or_default();
    let result = oblivious || is_secret_type(src.ret_type().and_then(|r| r.type_ref()));
    SecretSignature {
        params,
        result,
        constant_time,
    }
}

struct Checker<'a> {
    db: &'a dyn HirDatabase,
    frame: Frame,
    sig: SecretSignature,
    tainted: FxHashSet<PatId>,
    /// Locals bound by a `let` whose ascribed type is not `secret`.
    declared_public: FxHashSet<PatId>,
    /// Every expression inside a closure body.
    in_closure: FxHashSet<ExprId>,
    errors: Vec<(ExprId, String)>,
}

pub(super) fn flow_diagnostics(
    db: &dyn HirDatabase,
    func: Function,
    sink: &mut DiagnosticSink<'_>,
) {
    if func.is_extern(db) {
        return;
    }
    let frame = Frame::new(db, func);
    let sig = secret_signature(db, func);
    let mut in_closure = FxHashSet::default();
    for (closure, _) in frame.infer.closures() {
        let Expr::Closure { body, .. } = &frame.body[closure] else {
            continue;
        };
        collect_subtree(&frame, *body, &mut in_closure);
    }
    let mut checker = Checker {
        db,
        frame,
        sig,
        tainted: FxHashSet::default(),
        declared_public: FxHashSet::default(),
        in_closure,
        errors: Vec::new(),
    };
    if !checker.seed() {
        return;
    }
    checker.propagate();
    checker.check();

    let mut seen = FxHashSet::default();
    for (expr, message) in checker.errors {
        if !seen.insert((expr, message.clone())) {
            continue;
        }
        if let Some(site) = checker.frame.site(expr) {
            sink.push(SecretFlow {
                file: site.file_id,
                site: site.value,
                message,
            });
        }
    }
}

/// Adds `expr` and every expression below it to `out`.
fn collect_subtree(frame: &Frame, expr: ExprId, out: &mut FxHashSet<ExprId>) {
    out.insert(expr);
    frame.body[expr].walk_child_exprs(|child| collect_subtree(frame, child, out));
}

impl Checker<'_> {
    /// Taints the declared sources. Returns whether there is anything to
    /// check at all.
    fn seed(&mut self) -> bool {
        let body = Arc::clone(&self.frame.body);
        for (idx, (pat, _)) in body.params().iter().enumerate() {
            if self.sig.params.get(idx).copied().unwrap_or(false) {
                self.taint_pat(*pat);
            }
        }
        // `let` ascriptions are read back from the syntax tree: `secret T`
        // lowers to `T` everywhere else.
        for (pat_id, pat) in body.pats() {
            if !matches!(pat, Pat::Bind { .. }) {
                continue;
            }
            let Some(let_stmt) = self.let_stmt_of(pat_id) else {
                continue;
            };
            match let_stmt.ascribed_type() {
                Some(t) if matches!(t.kind(), ast::TypeRefKind::SecretType(_)) => {
                    self.tainted.insert(pat_id);
                }
                Some(_) => {
                    self.declared_public.insert(pat_id);
                }
                None => {}
            }
        }
        let calls_secret = body.exprs().any(|(id, expr)| match expr {
            Expr::Call { .. } | Expr::MethodCall { .. } => self
                .callee(id)
                .is_some_and(|f| secret_signature(self.db, f).result),
            _ => false,
        });
        self.sig.constant_time || !self.tainted.is_empty() || calls_secret || self.sig.result
    }

    fn let_stmt_of(&self, pat: PatId) -> Option<ast::LetStmt> {
        let src = self.frame.source_map.pat_syntax(pat)?;
        let root = self.db.parse(src.file_id).syntax_node();
        let node = src.value.either(
            |p| p.to_node(&root).syntax().clone(),
            |p| p.to_node(&root).syntax().clone(),
        );
        node.ancestors().find_map(ast::LetStmt::cast)
    }

    fn taint_pat(&mut self, pat: PatId) -> bool {
        let mut changed = self.tainted.insert(pat);
        let body = Arc::clone(&self.frame.body);
        let mut children = Vec::new();
        body[pat].walk_child_pats(|c| children.push(c));
        for child in children {
            changed |= self.taint_pat(child);
        }
        changed
    }

    fn callee(&self, call: ExprId) -> Option<Function> {
        match &self.frame.body[call] {
            Expr::Call { callee, .. } => match self.frame.infer[*callee].interned() {
                crate::TyKind::FnDef(crate::CallableDef::Function(f), _) => Some(*f),
                _ => None,
            },
            Expr::MethodCall { .. } => self.frame.infer.method_resolution(call).map(Function::from),
            _ => None,
        }
    }

    /// Whether `expr`'s value depends on a secret.
    fn tainted(&self, expr: ExprId) -> bool {
        let body = &self.frame.body;
        match &body[expr] {
            // Literals carry nothing; control transfers and assignments
            // have no value to carry anything in.
            Expr::Literal(_)
            | Expr::Missing
            | Expr::Return { .. }
            | Expr::Break { .. }
            | Expr::Loop { .. }
            | Expr::While { .. }
            | Expr::BinaryOp {
                op: Some(BinaryOp::Assignment { .. }),
                ..
            } => false,
            Expr::Path(path) => self
                .frame
                .resolve_local(self.db, expr, path)
                .is_some_and(|pat| self.tainted.contains(&pat)),
            Expr::Call { args, .. } => match self.frame.infer.builtin_call(expr) {
                Some(BuiltinFn::Declassify | BuiltinFn::Print | BuiltinFn::Println) => false,
                Some(_) => args.iter().any(|a| self.tainted(*a)),
                // A resolved callee's result is secret exactly when its
                // signature says so; a secret argument to a public parameter
                // is reported at the argument, not propagated through the
                // result as well. A call through a function value is secret
                // when the value is (it captured a secret) or an argument is.
                None => self.callee(expr).map_or_else(
                    || {
                        let callee_tainted = match &body[expr] {
                            Expr::Call { callee, .. } => self.tainted(*callee),
                            _ => false,
                        };
                        callee_tainted || args.iter().any(|a| self.tainted(*a))
                    },
                    |f| secret_signature(self.db, f).result,
                ),
            },
            Expr::MethodCall { receiver, args, .. } => self.callee(expr).map_or_else(
                || self.tainted(*receiver) || args.iter().any(|a| self.tainted(*a)),
                |f| secret_signature(self.db, f).result,
            ),
            Expr::Block { tail, .. } => tail.is_some_and(|t| self.tainted(t)),
            // A closure is secret when it captured a secret, or when what it
            // returns (its tail or a `return`) is secret.
            Expr::Closure {
                body: closure_body, ..
            } => {
                let captured = self
                    .frame
                    .infer
                    .closure(expr)
                    .is_some_and(|c| c.captures.iter().any(|p| self.tainted.contains(p)));
                captured || self.tainted(*closure_body) || self.returns_secret(*closure_body)
            }
            other => {
                let mut any = false;
                other.walk_child_exprs(|c| any |= self.tainted(c));
                any
            }
        }
    }

    /// Whether a `return` somewhere below `expr` returns a secret.
    fn returns_secret(&self, expr: ExprId) -> bool {
        // `x?` returns a piece of `x` (its `Err`, or `nil`).
        if let Expr::Return { expr: Some(value) } | Expr::Propagate { expr: value } =
            &self.frame.body[expr]
        {
            if self.tainted(*value) {
                return true;
            }
        }
        let mut any = false;
        self.frame.body[expr].walk_child_exprs(|c| any |= self.returns_secret(c));
        any
    }

    /// Runs the taint to a fixpoint: assignments and `let`s carry it to the
    /// locals they bind, and anything assigned under a secret condition is
    /// secret.
    fn propagate(&mut self) {
        let root = self.frame.body.body_expr();
        loop {
            let mut changed = false;
            self.walk_propagate(root, false, &mut changed);
            if !changed {
                break;
            }
        }
    }

    fn walk_propagate(&mut self, expr: ExprId, under_secret: bool, changed: &mut bool) {
        let body = Arc::clone(&self.frame.body);
        match &body[expr] {
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.walk_propagate(*condition, under_secret, changed);
                let inner = under_secret || self.tainted(*condition);
                self.walk_propagate(*then_branch, inner, changed);
                if let Some(e) = else_branch {
                    self.walk_propagate(*e, inner, changed);
                }
            }
            Expr::While { condition, body: b } => {
                self.walk_propagate(*condition, under_secret, changed);
                let inner = under_secret || self.tainted(*condition);
                self.walk_propagate(*b, inner, changed);
            }
            // Which arm runs depends on the scrutinee, and every name a
            // pattern binds is a piece of it.
            Expr::Match {
                expr: scrutinee,
                arms,
                ..
            } => {
                self.walk_propagate(*scrutinee, under_secret, changed);
                let secret = self.tainted(*scrutinee);
                let inner = under_secret || secret;
                for arm in arms {
                    if secret && self.taint_pat(arm.pat) {
                        *changed = true;
                    }
                    self.walk_propagate(arm.expr, inner, changed);
                }
            }
            Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(BinaryOp::Assignment { .. }),
            } => {
                self.walk_propagate(*rhs, under_secret, changed);
                if let Expr::Path(path) = &body[*lhs] {
                    if let Some(pat) = self.frame.resolve_local(self.db, *lhs, path) {
                        if (under_secret || self.tainted(*rhs)) && self.tainted.insert(pat) {
                            *changed = true;
                        }
                    }
                }
            }
            Expr::Block { statements, tail } => {
                for stmt in statements {
                    match stmt {
                        Statement::Let {
                            pat, initializer, ..
                        } => {
                            if let Some(init) = initializer {
                                self.walk_propagate(*init, under_secret, changed);
                                if (under_secret || self.tainted(*init)) && self.taint_pat(*pat) {
                                    *changed = true;
                                }
                            }
                        }
                        Statement::Expr(e) => self.walk_propagate(*e, under_secret, changed),
                    }
                }
                if let Some(t) = tail {
                    self.walk_propagate(*t, under_secret, changed);
                }
            }
            other => {
                let mut children = Vec::new();
                other.walk_child_exprs(|c| children.push(c));
                for c in children {
                    self.walk_propagate(c, under_secret, changed);
                }
            }
        }
    }

    fn error(&mut self, expr: ExprId, message: impl Into<String>) {
        self.errors.push((expr, message.into()));
    }

    fn check(&mut self) {
        let root = self.frame.body.body_expr();
        // The function's own result.
        let result_ty = &self.frame.infer[root];
        if !self.sig.result && !result_ty.is_never() && !result_ty.is_empty() && self.tainted(root)
        {
            let site = self.tail_of(root);
            self.error(
                site,
                "secret value returned from a function whose result is not `secret` \
                 (wrap it in `declassify(..)` if revealing it is intended)",
            );
        }
        let ids: Vec<ExprId> = self.frame.body.exprs().map(|(id, _)| id).collect();
        for id in ids {
            if self.frame.body.contracts().ghost_exprs.contains(&id) {
                continue;
            }
            self.check_expr(id);
        }
    }

    fn tail_of(&self, mut expr: ExprId) -> ExprId {
        while let Expr::Block {
            tail: Some(tail), ..
        } = &self.frame.body[expr]
        {
            expr = *tail;
        }
        expr
    }

    fn check_expr(&mut self, id: ExprId) {
        let body = Arc::clone(&self.frame.body);
        let ct = self.sig.constant_time;
        if self.frame.infer.is_indirect_call(id) {
            self.check_indirect_call(id);
        }
        match &body[id] {
            Expr::Closure { .. } if ct => {
                self.error(
                    id,
                    "a constant-time function cannot create a closure: whatever calls it is not \
                     known to be constant-time",
                );
            }
            Expr::Return { expr: Some(value) }
                if !self.sig.result && !self.in_closure.contains(&id) && self.tainted(*value) =>
            {
                self.error(
                    *value,
                    "secret value returned from a function whose result is not `secret` \
                     (wrap it in `declassify(..)` if revealing it is intended)",
                );
            }
            // `x?` tests `x` and may return a piece of it: a branch, and a
            // return.
            Expr::Propagate { expr: value } if ct && self.tainted(*value) => {
                self.error(
                    *value,
                    "`?` on a secret in a constant-time function branches on it (use \
                     `ct_select(..)` to choose without branching)",
                );
            }
            Expr::Propagate { expr: value }
                if !self.sig.result && !self.in_closure.contains(&id) && self.tainted(*value) =>
            {
                self.error(
                    *value,
                    "`?` on a secret may return it from a function whose result is not `secret` \
                     (wrap it in `declassify(..)` if revealing it is intended)",
                );
            }
            Expr::Call { args, .. } => {
                match self.frame.infer.builtin_call(id) {
                    Some(BuiltinFn::Print | BuiltinFn::Println) => {
                        for &arg in args {
                            if self.tainted(arg) {
                                self.error(arg, "secret value printed (declassify it first if that is intended)");
                            }
                        }
                    }
                    // `select` and the branch hints are not constant-time: LLVM
                    // may turn them into a branch. `ct_select` is the form for
                    // a secret condition.
                    Some(BuiltinFn::Select | BuiltinFn::Likely | BuiltinFn::Unlikely) => {
                        if let Some(&cond) = args.first() {
                            if self.tainted(cond) {
                                self.error(
                                    cond,
                                    "secret value used as the condition of a non-constant-time builtin (use `ct_select`)",
                                );
                            }
                        }
                    }
                    // `len`/`unchecked_get` are methods, never a `Call`.
                    Some(
                        BuiltinFn::Declassify
                        | BuiltinFn::CtSelect
                        | BuiltinFn::ArrayFilled
                        | BuiltinFn::Fma
                        | BuiltinFn::Channel
                        | BuiltinFn::ArrayLen
                        | BuiltinFn::ArrayUncheckedGet
                        | BuiltinFn::StrLen,
                    ) => {}
                    None => self.check_call(id, args, None),
                }
            }
            Expr::MethodCall { receiver, args, .. } => self.check_call(id, args, Some(*receiver)),
            // A channel is a public sink: whatever is received from it is
            // public, so a secret sent on one would come out declassified.
            Expr::ChannelSend { value, .. } if self.tainted(*value) => {
                self.error(*value, SECRET_ON_CHANNEL);
            }
            Expr::Select { arms } => {
                for arm in arms {
                    if let crate::SelectOp::Send { value, .. } = arm.op {
                        if self.tainted(value) {
                            self.error(value, SECRET_ON_CHANNEL);
                        }
                    }
                }
            }
            Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(BinaryOp::Assignment { .. }),
            } => match &body[*lhs] {
                Expr::Path(path) => {
                    if let Some(pat) = self.frame.resolve_local(self.db, *lhs, path) {
                        if self.declared_public.contains(&pat) && self.tainted(*rhs) {
                            self.error(
                                *rhs,
                                "secret value assigned to a local declared without `secret`",
                            );
                        }
                    }
                }
                _ => {
                    if self.tainted(*rhs) {
                        self.error(
                            *rhs,
                            "secret value stored through a field or index: only locals, parameters \
                             and results can be `secret`",
                        );
                    }
                }
            },
            Expr::Block { statements, .. } => {
                for stmt in statements {
                    if let Statement::Let {
                        pat,
                        initializer: Some(init),
                        ..
                    } = stmt
                    {
                        if self.declared_public.contains(pat) && self.tainted(*init) {
                            self.error(
                                *init,
                                "secret value bound by a `let` declared without `secret`",
                            );
                        }
                    }
                }
            }
            Expr::If { condition, .. } | Expr::While { condition, .. }
                if ct && self.tainted(*condition) =>
            {
                self.error(
                    *condition,
                    "branch on a secret in a constant-time function (use `ct_select(..)` to choose \
                     without branching)",
                );
            }
            Expr::Match {
                expr: scrutinee, ..
            } if ct && self.tainted(*scrutinee) => {
                self.error(
                    *scrutinee,
                    "`match` (or `if let`/`while let`) on a secret in a constant-time function \
                     branches on it (use `ct_select(..)` to choose without branching)",
                );
            }
            Expr::BinaryOp {
                lhs,
                op: Some(BinaryOp::LogicOp(LogicOp::And | LogicOp::Or)),
                ..
            } if ct && self.tainted(*lhs) => {
                self.error(
                    *lhs,
                    "`&&`/`||` on a secret in a constant-time function short-circuits, which is a \
                     branch (use `&`/`|` on booleans instead)",
                );
            }
            Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(BinaryOp::ArithOp(ArithOp::Divide | ArithOp::Remainder)),
            } if ct && (self.tainted(*lhs) || self.tainted(*rhs)) => {
                self.error(
                    id,
                    "division by or of a secret in a constant-time function: integer division \
                     takes data-dependent time",
                );
            }
            Expr::Index { index, .. } if ct && self.tainted(*index) => {
                self.error(
                    *index,
                    "secret index in a constant-time function: the address of a memory access \
                     leaks through the cache",
                );
            }
            _ => {}
        }
    }

    /// A call through a function value: its parameters are never `secret`,
    /// and in a constant-time function it is not allowed at all.
    fn check_indirect_call(&mut self, call: ExprId) {
        let args = match &self.frame.body[call] {
            Expr::Call { args, .. } | Expr::MethodCall { args, .. } => args.clone(),
            _ => return,
        };
        if self.sig.constant_time {
            self.error(
                call,
                "a constant-time function cannot call a function value: the callee is not known \
                 to be constant-time",
            );
        }
        for arg in args {
            if self.tainted(arg) {
                self.error(
                    arg,
                    "secret value passed to a function value, whose parameters are never `secret`",
                );
            }
        }
    }

    fn check_call(&mut self, call: ExprId, args: &[ExprId], receiver: Option<ExprId>) {
        let Some(callee) = self.callee(call) else {
            return;
        };
        let sig = secret_signature(self.db, callee);
        let name = callee.name(self.db).to_string();
        let any_secret = args
            .iter()
            .copied()
            .chain(receiver)
            .any(|a| self.tainted(a));
        for (idx, &arg) in args.iter().enumerate() {
            if self.tainted(arg) && !sig.params.get(idx).copied().unwrap_or(false) {
                self.error(
                    arg,
                    format!("secret value passed to a non-`secret` parameter of `{name}`"),
                );
            }
        }
        if self.sig.constant_time && any_secret && !sig.constant_time {
            self.error(
                call,
                format!(
                    "secret passed to `{name}`, which is not `@constant_time`, from a \
                     constant-time function"
                ),
            );
        }
    }
}
