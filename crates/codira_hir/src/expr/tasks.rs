//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Lowers the structured-concurrency expressions to HIR: `scope { .. }`,
//!   `spawn f(args)`, `channel <- value`, `<-channel` and `select { .. }`.
//!
//! # The model
//!
//! A `scope` block owns tasks: `spawn f(a, b)` evaluates `a` and `b` on the
//! spawning thread and runs `f` with them on the runtime's worker pool, and
//! every task spawned in a scope has finished before control leaves the
//! scope -- by its end, `return`, `break`, or an effect handler abandoning
//! it. Nothing outlives the scope that started it, so no task can observe a
//! frame that is gone.
//!
//! Tasks share nothing but channels: a spawned function takes only scalars
//! (integers, floats, `bool`) and `Channel[T]` handles, and a channel carries
//! only scalars. A channel, created by `channel(capacity)`, belongs to the
//! scope it is created in and is freed when that scope ends, after its tasks
//! have been joined. See `expr::validator::tasks` for the rules that make
//! those lifetimes sound and `codira_runtime::tasks` for the runtime.
//!
//! # `select`
//!
//! `select { arm, .. }` multiplexes channel operations, Go-style. Each arm is
//! `name = <-ch -> body` (the received value is bound to `name` in `body`),
//! `<-ch -> body`, `ch <- value -> body`, or `default -> body`. Every
//! channel and sent value is evaluated first, in source order; then the
//! select waits until at least one operation can proceed, performs exactly
//! one -- chosen uniformly at random among the ready ones, so no arm starves
//! -- and runs its body. With `default` it does not wait. The arms' bodies
//! have one type, which is the `select`'s.
//!
//! # `group`
//!
//! `group { .. }` is a `scope` whose tasks return values: its value is the
//! results of the tasks spawned directly in it (not in a nested `scope`,
//! `group`, closure or handler arm), in spawn order, as an array -- or, when
//! they return a `Result[T, E]` whose `Ok` holds a type parameter, a
//! `Result[[T], E]`: `Ok` of every task's `Ok` payload, or the `Err` of the
//! *lowest-index* task that failed. A failure cancels the tasks spawned
//! after it, never the ones before (see `codira_runtime::tasks::group` for
//! why that makes the value deterministic). A group's tasks take only
//! scalars -- no channels, so a task the group never starts cannot leave a
//! sibling waiting -- and may return any type (`ty::infer::tasks` and
//! `expr::validator::tasks` have the rules).
use codira_syntax::{ast, AstNode, AstPtr};

use super::{Body, Expr, ExprCollector, ExprId, SelectArm, SelectOp};

/// The `spawn` expressions whose tasks belong to the `scope` or `group`
/// `owner`: those in its body that are not inside a nested `scope`,
/// `group`, closure or handler arm, in source order.
pub fn direct_spawns(body: &Body, owner: ExprId) -> Vec<ExprId> {
    fn walk(body: &Body, expr: ExprId, found: &mut Vec<ExprId>) {
        match &body[expr] {
            Expr::Spawn { .. } => {
                found.push(expr);
                return;
            }
            Expr::Scope { .. } | Expr::Group { .. } | Expr::Closure { .. } => return,
            Expr::Handle { body: handled, .. } => {
                walk(body, *handled, found);
                return;
            }
            _ => {}
        }
        body[expr].walk_child_exprs(|child| walk(body, child, found));
    }
    let mut found = Vec::new();
    if let Expr::Scope { body: inner } | Expr::Group { body: inner } = &body[owner] {
        walk(body, *inner, &mut found);
    }
    found
}

impl ExprCollector<'_> {
    /// `channel <- value`. The node has no named children: its two
    /// expression children are the channel and the value, in order.
    pub(super) fn collect_channel_send(
        &mut self,
        send: &ast::ChannelSendExpr,
        syntax_ptr: AstPtr<ast::Expr>,
    ) -> ExprId {
        let mut operands = send.syntax().children().filter_map(ast::Expr::cast);
        let channel = operands.next();
        let value = operands.next();
        let channel = self.collect_expr_opt(channel);
        let value = self.collect_expr_opt(value);
        self.alloc_expr(Expr::ChannelSend { channel, value }, syntax_ptr)
    }

    /// `select { arm, .. }`.
    pub(super) fn collect_select(
        &mut self,
        select: &ast::SelectExpr,
        syntax_ptr: AstPtr<ast::Expr>,
    ) -> ExprId {
        let arms = select
            .select_arm_list()
            .into_iter()
            .flat_map(|list| list.arms())
            .map(|arm| {
                let binding = arm.pat().map(|pat| self.collect_pat(pat));
                let op = if arm.is_default() {
                    SelectOp::Default
                } else {
                    match arm.operation().map(|op| (op.kind(), op)) {
                        Some((ast::ExprKind::ChannelRecvExpr(recv), _)) => SelectOp::Receive {
                            channel: self.collect_expr_opt(recv.expr()),
                        },
                        Some((ast::ExprKind::ChannelSendExpr(send), _)) => {
                            let mut operands = send.syntax().children().filter_map(ast::Expr::cast);
                            let channel = operands.next();
                            let value = operands.next();
                            SelectOp::Send {
                                channel: self.collect_expr_opt(channel),
                                value: self.collect_expr_opt(value),
                            }
                        }
                        Some((_, other)) => SelectOp::Invalid {
                            expr: self.collect_expr(other),
                        },
                        None => SelectOp::Invalid {
                            expr: self.collect_expr_opt(None),
                        },
                    }
                };
                let body = self.collect_expr_opt(arm.body());
                SelectArm { binding, op, body }
            })
            .collect();
        self.alloc_expr(Expr::Select { arms }, syntax_ptr)
    }
}
