//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::collections::HashSet;

use super::ExprValidator;
use crate::{
    diagnostics::{DiagnosticSink, PossiblyUninitializedVariable},
    resolve::{resolver_for_expr, ValueNs},
    BinaryOp, Expr, ExprId, PatId, Path, Resolver, Statement,
};

#[derive(Copy, Clone, PartialEq, Eq)]
enum ExprKind {
    Normal,
    Place,
    Both,
}

impl ExprValidator<'_> {
    /// Validates that all binding access has previously been initialized.
    pub(super) fn validate_uninitialized_access(&self, sink: &mut DiagnosticSink<'_>) {
        let mut initialized_patterns = HashSet::new();

        // Add all parameter patterns to the set of initialized patterns (they must have
        // been initialized)
        if let Some((pat, _)) = self.body.self_param {
            self.mark_initialized(&mut initialized_patterns, pat);
        }

        for (pat, _) in self.body.params.iter() {
            self.mark_initialized(&mut initialized_patterns, *pat);
        }

        self.validate_expr_access(
            sink,
            &mut initialized_patterns,
            self.body.body_expr,
            ExprKind::Normal,
        );
    }

    /// Marks `pat` and every binding beneath it as initialized.
    ///
    /// The whole subtree matters, not just the root: a tuple pattern binds
    /// nothing under its own `PatId`, so `let (x, y) = p` followed by `x + y`
    /// reported both `x` and `y` as possibly-uninitialized when only the
    /// enclosing `(x, y)` had been recorded. Recursion also covers nesting
    /// (`let ((a, b), c) = ..`), which a single `walk_child_pats` pass would
    /// miss one level down.
    fn mark_initialized(&self, initialized_patterns: &mut HashSet<PatId>, pat: PatId) {
        initialized_patterns.insert(pat);
        let body = self.body.clone();
        body[pat].walk_child_pats(|child| self.mark_initialized(initialized_patterns, child));
    }

    /// Validates that the specified expr does not access unitialized bindings
    fn validate_expr_access(
        &self,
        sink: &mut DiagnosticSink<'_>,
        initialized_patterns: &mut HashSet<PatId>,
        expr: ExprId,
        expr_side: ExprKind,
    ) {
        let body = self.body.clone();
        match &body[expr] {
            Expr::Call { callee, args } => {
                self.validate_expr_access(sink, initialized_patterns, *callee, expr_side);
                for arg in args.iter() {
                    self.validate_expr_access(sink, initialized_patterns, *arg, expr_side);
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                self.validate_expr_access(sink, initialized_patterns, *receiver, expr_side);
                for arg in args.iter() {
                    self.validate_expr_access(sink, initialized_patterns, *arg, expr_side);
                }
            }
            Expr::Path(p) => {
                let resolver = resolver_for_expr(self.db, self.body.owner(), expr);
                self.validate_path_access(
                    sink,
                    initialized_patterns,
                    &resolver,
                    p,
                    expr,
                    expr_side,
                );
            }
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.validate_expr_access(sink, initialized_patterns, *condition, ExprKind::Normal);
                let mut then_branch_initialized_patterns = initialized_patterns.clone();
                self.validate_expr_access(
                    sink,
                    &mut then_branch_initialized_patterns,
                    *then_branch,
                    ExprKind::Normal,
                );
                if let Some(else_branch) = else_branch {
                    let mut else_branch_initialized_patterns = initialized_patterns.clone();
                    self.validate_expr_access(
                        sink,
                        &mut else_branch_initialized_patterns,
                        *else_branch,
                        ExprKind::Normal,
                    );
                    let then_is_never = self.infer[*then_branch].is_never();
                    let else_is_never = self.infer[*else_branch].is_never();
                    match (then_is_never, else_is_never) {
                        (false, false) => {
                            initialized_patterns.extend(
                                then_branch_initialized_patterns
                                    .intersection(&else_branch_initialized_patterns),
                            );
                        }
                        (true, false) => {
                            initialized_patterns.extend(else_branch_initialized_patterns);
                        }
                        (false, true) => {
                            initialized_patterns.extend(then_branch_initialized_patterns);
                        }
                        (true, true) => {}
                    };
                }
            }
            Expr::UnaryOp { expr, .. }
            | Expr::Field { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::Scope { body: expr }
            | Expr::Spawn { call: expr }
            | Expr::ChannelReceive { channel: expr }
            | Expr::Unwrap { expr }
            | Expr::Propagate { expr } => {
                self.validate_expr_access(sink, initialized_patterns, *expr, ExprKind::Normal);
            }
            Expr::BinaryOp { lhs, rhs, op } => {
                let lhs_expr_kind = match op {
                    Some(BinaryOp::Assignment { op: Some(_) }) => ExprKind::Both,
                    Some(BinaryOp::Assignment { op: None }) => ExprKind::Place,
                    _ => ExprKind::Normal,
                };
                self.validate_expr_access(sink, initialized_patterns, *lhs, lhs_expr_kind);
                self.validate_expr_access(sink, initialized_patterns, *rhs, ExprKind::Normal);
            }
            Expr::Block { statements, tail } => {
                for statement in statements.iter() {
                    match statement {
                        Statement::Let {
                            pat, initializer, ..
                        } => {
                            if let Some(initializer) = initializer {
                                self.validate_expr_access(
                                    sink,
                                    initialized_patterns,
                                    *initializer,
                                    ExprKind::Normal,
                                );
                                self.mark_initialized(initialized_patterns, *pat);
                            }
                        }
                        Statement::Expr(expr) => {
                            self.validate_expr_access(
                                sink,
                                initialized_patterns,
                                *expr,
                                ExprKind::Normal,
                            );
                            if self.infer[*expr].is_never() {
                                return;
                            }
                        }
                    }
                }
                if let Some(tail) = tail {
                    self.validate_expr_access(sink, initialized_patterns, *tail, ExprKind::Normal);
                }
            }
            Expr::Return { expr } | Expr::Break { expr } => {
                if let Some(expr) = expr {
                    self.validate_expr_access(sink, initialized_patterns, *expr, ExprKind::Normal);
                }
            }
            Expr::Loop { body } => {
                self.validate_expr_access(sink, initialized_patterns, *body, ExprKind::Normal);
            }
            Expr::While { condition, body } => {
                self.validate_expr_access(sink, initialized_patterns, *condition, ExprKind::Normal);
                self.validate_expr_access(
                    sink,
                    &mut initialized_patterns.clone(),
                    *body,
                    ExprKind::Normal,
                );
            }
            Expr::For {
                pat,
                iterable,
                body,
            } => {
                self.validate_expr_access(sink, initialized_patterns, *iterable, ExprKind::Normal);
                // The body may run zero times, so nothing it initializes
                // counts afterwards -- the same rule as a `while` body.
                let mut body_patterns = initialized_patterns.clone();
                self.mark_initialized(&mut body_patterns, *pat);
                self.validate_expr_access(sink, &mut body_patterns, *body, ExprKind::Normal);
            }
            Expr::Match { expr, arms, .. } => {
                self.validate_expr_access(sink, initialized_patterns, *expr, ExprKind::Normal);
                // Like `if`: a binding is initialized afterwards only if
                // every arm that completes initialized it.
                let mut after: Option<HashSet<PatId>> = None;
                for arm in arms {
                    let mut arm_patterns = initialized_patterns.clone();
                    self.mark_initialized(&mut arm_patterns, arm.pat);
                    self.validate_expr_access(sink, &mut arm_patterns, arm.expr, ExprKind::Normal);
                    if !self.infer[arm.expr].is_never() {
                        after = Some(match after {
                            None => arm_patterns,
                            Some(prev) => prev.intersection(&arm_patterns).copied().collect(),
                        });
                    }
                }
                if let Some(after) = after {
                    initialized_patterns.extend(after);
                }
            }
            Expr::Range { start, end, .. } => {
                self.validate_expr_access(sink, initialized_patterns, *start, ExprKind::Normal);
                self.validate_expr_access(sink, initialized_patterns, *end, ExprKind::Normal);
            }
            Expr::RecordLit { fields, spread, .. } => {
                for field in fields.iter() {
                    self.validate_expr_access(
                        sink,
                        initialized_patterns,
                        field.expr,
                        ExprKind::Normal,
                    );
                }
                if let Some(expr) = spread {
                    self.validate_expr_access(sink, initialized_patterns, *expr, ExprKind::Normal);
                }
            }
            Expr::Index { base, index } => {
                self.validate_expr_access(sink, initialized_patterns, *base, ExprKind::Normal);
                self.validate_expr_access(sink, initialized_patterns, *index, ExprKind::Normal);
            }
            Expr::Array(exprs) | Expr::Tuple(exprs) => {
                for expr in exprs {
                    self.validate_expr_access(sink, initialized_patterns, *expr, ExprKind::Normal);
                }
            }
            Expr::Perform { args, .. } => {
                for arg in args {
                    self.validate_expr_access(sink, initialized_patterns, *arg, ExprKind::Normal);
                }
            }
            Expr::ChannelSend { channel, value } => {
                self.validate_expr_access(sink, initialized_patterns, *channel, ExprKind::Normal);
                self.validate_expr_access(sink, initialized_patterns, *value, ExprKind::Normal);
            }
            Expr::Resume { value } => {
                if let Some(value) = value {
                    self.validate_expr_access(sink, initialized_patterns, *value, ExprKind::Normal);
                }
            }
            // The body may be abandoned at any `perform` by an aborting arm,
            // so nothing it initializes is known to be initialized after the
            // `handle`. An arm runs only when its operation is performed, and
            // sees its own parameters, which are always bound.
            Expr::Handle { body, arms } => {
                self.validate_expr_access(
                    sink,
                    &mut initialized_patterns.clone(),
                    *body,
                    ExprKind::Normal,
                );
                for arm in arms {
                    let mut arm_initialized = initialized_patterns.clone();
                    for &param in &arm.params {
                        self.mark_initialized(&mut arm_initialized, param);
                    }
                    self.validate_expr_access(
                        sink,
                        &mut arm_initialized,
                        arm.body,
                        ExprKind::Normal,
                    );
                }
            }
            // Every operand is evaluated; then exactly one arm runs, so what
            // is initialized after the `select` is what every arm that can
            // finish initializes.
            Expr::Select { arms } => {
                for arm in arms {
                    for operand in arm.operands() {
                        self.validate_expr_access(
                            sink,
                            initialized_patterns,
                            operand,
                            ExprKind::Normal,
                        );
                    }
                }
                let mut after: Option<HashSet<PatId>> = None;
                for arm in arms {
                    let mut arm_initialized = initialized_patterns.clone();
                    if let Some(binding) = arm.binding {
                        self.mark_initialized(&mut arm_initialized, binding);
                    }
                    self.validate_expr_access(
                        sink,
                        &mut arm_initialized,
                        arm.body,
                        ExprKind::Normal,
                    );
                    if !self.infer[arm.body].is_never() {
                        after = Some(match after {
                            None => arm_initialized,
                            Some(previous) => {
                                previous.intersection(&arm_initialized).copied().collect()
                            }
                        });
                    }
                }
                if let Some(after) = after {
                    initialized_patterns.extend(after);
                }
            }
            // Exactly one branch runs, as for a `select`.
            Expr::Offer { endpoint, branches } => {
                self.validate_expr_access(sink, initialized_patterns, *endpoint, ExprKind::Normal);
                let mut after: Option<HashSet<PatId>> = None;
                for &branch in branches {
                    let mut branch_initialized = initialized_patterns.clone();
                    self.validate_expr_access(
                        sink,
                        &mut branch_initialized,
                        branch,
                        ExprKind::Normal,
                    );
                    if !self.infer[branch].is_never() {
                        after = Some(match after {
                            None => branch_initialized,
                            Some(previous) => previous
                                .intersection(&branch_initialized)
                                .copied()
                                .collect(),
                        });
                    }
                }
                if let Some(after) = after {
                    initialized_patterns.extend(after);
                }
            }
            // A closure reads its captures when it is created, so every
            // variable its body mentions must be initialized at the literal;
            // its own parameters always are. Nothing it assigns (it may
            // assign only its own locals) is visible outside.
            Expr::Closure { params, body, .. } => {
                let mut closure_initialized = initialized_patterns.clone();
                for &(param, _) in params {
                    self.mark_initialized(&mut closure_initialized, param);
                }
                self.validate_expr_access(sink, &mut closure_initialized, *body, ExprKind::Normal);
            }
            Expr::Literal(_) | Expr::Missing | Expr::Continue => {}
        }
    }

    fn validate_path_access(
        &self,
        sink: &mut DiagnosticSink<'_>,
        initialized_patterns: &mut HashSet<PatId>,
        resolver: &Resolver,
        path: &Path,
        expr: ExprId,
        expr_side: ExprKind,
    ) {
        let pat = match resolver.resolve_path_as_value_fully(self.db, path) {
            Some((ValueNs::LocalBinding(pat), _)) => pat,
            _ => return,
        };

        if expr_side == ExprKind::Normal || expr_side == ExprKind::Both {
            // Check if the binding has already been initialized
            if initialized_patterns.get(&pat).is_none() {
                let (_, body_source_map) = self.db.body_with_source_map(self.func.id.into());
                sink.push(PossiblyUninitializedVariable {
                    file: self.func.file_id(self.db),
                    pat: body_source_map
                        .expr_syntax(expr)
                        .unwrap()
                        .value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr()),
                });
            }
        }

        if expr_side == ExprKind::Place {
            // The binding should be initialized
            initialized_patterns.insert(pat);
        }
    }
}
