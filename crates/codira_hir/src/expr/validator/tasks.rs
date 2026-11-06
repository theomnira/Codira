//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - The rules that make `scope`, `spawn` and channels memory- and race-safe
//!   (see `crate::expr::tasks` for the model).
//!
//! # Rules
//!
//! * `spawn` appears inside a `scope { .. }` of the same function (a handler
//!   arm is a function of its own), so there is always a scope to join it.
//! * `spawn` takes a direct call `f(args)` to a non-generic function of the
//!   same module that declares no effects (a task has no handler to perform
//!   through), and every parameter of `f` is a scalar -- an integer, a float or
//!   `bool` -- or a `Channel[T]`. Tasks share nothing else, so they cannot race
//!   on memory.
//! * `channel(capacity)` appears only as the initializer of `let name = ..`
//!   inside a `scope`, and carries scalars. The channel belongs to that scope
//!   and is freed when it ends, after every task of the scope has finished.
//! * A channel value is only ever *used*: as the operand of `<-`, or as an
//!   argument passed to a function or to `spawn`. It is never stored, returned,
//!   copied into another binding, or made the value of a block. Bindings are
//!   lexical, so a channel cannot be named after its scope ends, and a function
//!   it is passed to cannot keep it (no return, no field, no array can hold
//!   one); a task it is passed to is joined no later than the scope that owns
//!   the channel. A `select` arm's channel is a use like `<-`.
//! * Session endpoints (`expr::session`) follow the same "only used" rule, with
//!   `.choose(..)` and `offer` as further uses; `S.open()` appears only inside
//!   a `scope`, which owns the pair; a spawned function may take endpoints. The
//!   protocol checker's findings are reported here too.
//! * A `select` has at least one arm -- with none it would wait forever -- and
//!   at most one `default`. Every other arm is a channel operation (`name =
//!   <-ch`, `<-ch` or `ch <- value`), and only a receive binds a name.
//! * `group { .. }` is a `scope` whose tasks return values. A `spawn` belongs
//!   to the innermost `scope` or `group` around it. A group's tasks take
//!   only integers, floats and `bool` -- no channels or endpoints: the group
//!   does not start the tasks after a failed one, and a task that never runs
//!   must not leave a sibling waiting on a channel. A group spawns at least
//!   one task (its value's type comes from them), and its tasks return a
//!   value -- not `()` (that is a `scope`), not a `secret` (the value crosses
//!   to the joining task outside the secrecy checker's sight), not a channel
//!   or endpoint. Any other result type is allowed: a task's arguments are
//!   scalars and there is no module-level state, so everything its result
//!   refers to was made by the task, which has finished when the result is
//!   read -- the result is moved, never shared. Channels and sessions are
//!   still created only inside a `scope` (the one that owns them), which may
//!   enclose the group.
//! * A closure is a function of its own: a `scope` around the literal does not
//!   cover a `spawn` in its body. `spawn` never runs a function *value* (a
//!   closure, or a named function passed around as a value): a closure carries
//!   captured references, and nothing proves those safe to share with another
//!   task.
use rustc_hash::FxHashSet;

use super::ExprValidator;
use crate::{
    diagnostics::{DiagnosticSink, EffectError},
    resolve::{resolver_for_expr, ValueNs},
    BuiltinFn, Expr, ExprId, HasSource, HirDisplay, Pat, SelectArm, SelectOp, Statement, Ty,
    TyKind,
};

/// Whether values of `ty` may cross between tasks.
fn is_scalar(ty: &Ty) -> bool {
    matches!(
        ty.interned(),
        TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool
    )
}

/// The innermost `scope` or `group` around an expression, in the same
/// function.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskOwner {
    None,
    Scope,
    Group,
}

/// Whether `ty` mentions a type that may not be a group task's result.
fn bad_group_result(ty: &Ty) -> Option<&'static str> {
    let mut found = None;
    crate::TypeWalk::walk(ty, &mut |ty: &Ty| {
        if matches!(ty.interned(), TyKind::Channel(_) | TyKind::Session(..)) {
            found = Some("a channel or session endpoint");
        }
    });
    found
}

/// Whether values of `ty` may be passed to a task.
fn is_task_argument(ty: &Ty) -> bool {
    match ty.interned() {
        TyKind::Channel(element) => is_scalar(element),
        TyKind::Session(..) => true,
        _ => is_scalar(ty),
    }
}

impl ExprValidator<'_> {
    /// Checks the rules described in the module documentation.
    pub(super) fn validate_tasks(&self, sink: &mut DiagnosticSink<'_>) {
        let uses_tasks = self.body.exprs().any(|(id, e)| {
            matches!(
                e,
                Expr::Scope { .. }
                    | Expr::Group { .. }
                    | Expr::Spawn { .. }
                    | Expr::ChannelSend { .. }
                    | Expr::ChannelReceive { .. }
                    | Expr::Select { .. }
                    | Expr::Offer { .. }
            ) || self.infer.builtin_call(id) == Some(BuiltinFn::Channel)
                || matches!(
                    self.infer[id].interned(),
                    TyKind::Channel(_) | TyKind::Session(..)
                )
                || crate::expr::opened_session(self.db, &self.body, id).is_some()
        });
        // A function value would let an endpoint out of the checker's sight.
        for (expr, _) in self.body.exprs() {
            let holds_endpoint = matches!(
                self.infer[expr].interned(),
                TyKind::Function(substs)
                    if substs.iter().any(|ty| matches!(ty.interned(), TyKind::Session(..)))
            );
            if holds_endpoint {
                self.push_task_error(
                    expr,
                    "a function value cannot take or return a session endpoint: pass the \
                     endpoint to a named function of its session type instead"
                        .to_string(),
                    sink,
                );
            }
        }
        // An endpoint parameter that is never used still has a protocol to
        // finish, so the checker's findings are reported either way.
        for (expr, message) in &self.db.session_flow(self.body.owner()).errors {
            self.push_task_error(*expr, message.clone(), sink);
        }
        if !uses_tasks {
            return;
        }
        let mut allowed = FxHashSet::default();
        self.validate_task_exprs(
            self.body.body_expr(),
            false,
            TaskOwner::None,
            &mut allowed,
            sink,
        );

        // Every channel-typed expression must be one of the uses collected
        // above.
        for (expr, _) in self.body.exprs() {
            if self.body.contracts().ghost_exprs.contains(&expr) {
                continue;
            }
            if matches!(self.infer[expr].interned(), TyKind::Session(..))
                && !allowed.contains(&expr)
            {
                self.push_task_error(
                    expr,
                    "a session endpoint can only be used with `<-`, `.choose(..)` or `offer`, or \
                     passed to a function or to `spawn`: it is linear, so it cannot be copied, \
                     stored or returned"
                        .to_string(),
                    sink,
                );
            }
            if matches!(self.infer[expr].interned(), TyKind::Channel(_)) && !allowed.contains(&expr)
            {
                self.push_task_error(
                    expr,
                    "a channel can only be used with `<-`, passed to a function or to `spawn`, \
                     or created by `let name = channel(capacity)`: it lives only as long as its \
                     `scope`, so it cannot be stored, returned or copied"
                        .to_string(),
                    sink,
                );
            }
        }
    }

    fn push_task_error(&self, expr: ExprId, message: String, sink: &mut DiagnosticSink<'_>) {
        if let Some(node) = self.expr_node(expr) {
            sink.push(EffectError {
                file: self.func.source(self.db).file_id,
                node,
                message,
            });
        }
    }

    /// Walks the executable tree. `in_scope` tells whether a `scope` of this
    /// function encloses `expr`, and `owner` what the innermost `scope` or
    /// `group` around it is; `allowed` collects the channel-typed
    /// expressions that are legitimate uses.
    fn validate_task_exprs(
        &self,
        expr: ExprId,
        in_scope: bool,
        owner: TaskOwner,
        allowed: &mut FxHashSet<ExprId>,
        sink: &mut DiagnosticSink<'_>,
    ) {
        let mut in_scope = in_scope;
        let mut owner = owner;
        match &self.body[expr] {
            Expr::Scope { .. } => {
                in_scope = true;
                owner = TaskOwner::Scope;
            }
            Expr::Group { .. } => {
                owner = TaskOwner::Group;
                self.validate_group(expr, sink);
            }
            Expr::Handle { body, arms } => {
                // An arm is a function of its own: a scope around the
                // `handle` cannot join a task the arm spawns.
                self.validate_task_exprs(*body, in_scope, owner, allowed, sink);
                for arm in arms {
                    self.validate_task_exprs(arm.body, false, TaskOwner::None, allowed, sink);
                }
                return;
            }
            Expr::Closure { body, .. } => {
                self.validate_task_exprs(*body, false, TaskOwner::None, allowed, sink);
                return;
            }
            Expr::Spawn { call } => {
                if owner == TaskOwner::None {
                    self.push_task_error(
                        expr,
                        "`spawn` must be inside a `scope { .. }` or `group { .. }` block: the \
                         block is what waits for the task to finish"
                            .to_string(),
                        sink,
                    );
                }
                self.validate_spawned_call(*call, owner == TaskOwner::Group, sink);
            }
            Expr::ChannelSend { channel, .. } | Expr::ChannelReceive { channel } => {
                if matches!(self.body[*channel], Expr::Path(_)) {
                    allowed.insert(*channel);
                }
            }
            Expr::Select { arms } => self.validate_select(expr, arms, allowed, sink),
            Expr::Offer { endpoint, .. } => {
                if matches!(self.body[*endpoint], Expr::Path(_)) {
                    allowed.insert(*endpoint);
                }
            }
            Expr::Call { args, .. } | Expr::MethodCall { args, .. } => {
                for &arg in args {
                    if matches!(self.body[arg], Expr::Path(_)) {
                        allowed.insert(arg);
                    }
                }
                // `ep.choose(k)`.
                if let Expr::MethodCall { receiver, .. } = &self.body[expr] {
                    if matches!(self.body[*receiver], Expr::Path(_))
                        && matches!(self.infer[*receiver].interned(), TyKind::Session(..))
                    {
                        allowed.insert(*receiver);
                    }
                }
                if crate::expr::opened_session(self.db, &self.body, expr).is_some() && !in_scope {
                    self.push_task_error(
                        expr,
                        "a session must be opened inside a `scope { .. }` block, which owns its \
                         endpoints and frees them when it ends"
                            .to_string(),
                        sink,
                    );
                }
            }
            Expr::Block { statements, .. } => {
                for statement in statements {
                    let Statement::Let {
                        pat,
                        initializer: Some(init),
                        ..
                    } = statement
                    else {
                        continue;
                    };
                    if self.infer.builtin_call(*init) != Some(BuiltinFn::Channel) {
                        continue;
                    }
                    if !matches!(self.body[*pat], Pat::Bind { .. }) {
                        continue;
                    }
                    allowed.insert(*init);
                    if !in_scope {
                        self.push_task_error(
                            *init,
                            "a channel must be created inside a `scope { .. }` block, which owns \
                             it and frees it when it ends"
                                .to_string(),
                            sink,
                        );
                    }
                    if let TyKind::Channel(element) = self.infer[*init].interned() {
                        if !is_scalar(element) && !element.is_unknown() {
                            self.push_task_error(
                                *init,
                                format!(
                                    "a channel carries integers, floats or `bool`, not `{}`: \
                                     tasks share nothing else",
                                    element.display(self.db)
                                ),
                                sink,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
        self.body[expr].walk_child_exprs(|child| {
            self.validate_task_exprs(child, in_scope, owner, allowed, sink);
        });
    }

    /// The function `spawn` runs, when `call` names one directly.
    fn spawned_function(&self, call: ExprId) -> Option<crate::Function> {
        match &self.body[call] {
            Expr::Call { callee, .. } => match &self.body[*callee] {
                Expr::Path(path) => resolver_for_expr(self.db, self.body.owner(), *callee)
                    .resolve_path_as_value_fully(self.db, path)
                    .and_then(|(ns, _)| match ns {
                        ValueNs::FunctionId(id) => Some(crate::Function::from(id)),
                        _ => None,
                    }),
                _ => None,
            },
            _ => None,
        }
    }

    /// A `group` spawns at least one task, and what its tasks return can be
    /// handed to the joining task (see the module documentation).
    fn validate_group(&self, group: ExprId, sink: &mut DiagnosticSink<'_>) {
        let spawns = crate::expr::direct_spawns(&self.body, group);
        if spawns.is_empty() {
            self.push_task_error(
                group,
                "a `group` spawns at least one task directly in its block: its value is their \
                 results"
                    .to_string(),
                sink,
            );
            return;
        }
        for spawn in spawns {
            let Expr::Spawn { call } = self.body[spawn] else {
                continue;
            };
            let result = &self.infer[call];
            if result.is_unknown() {
                continue;
            }
            let secret = self
                .spawned_function(call)
                .is_some_and(|f| crate::verify::flow::secret_signature(self.db, f).result);
            let problem = if secret {
                Some(
                    "a `secret`, which would reach the joining task as a public value"
                        .to_string(),
                )
            } else if matches!(result.interned(), TyKind::Tuple(0, _)) {
                Some("nothing (`()`): spawn it in a `scope` instead".to_string())
            } else {
                bad_group_result(result).map(|what| {
                    format!(
                        "{what} (`{}`), which cannot be handed to the task that joins the group",
                        result.display(self.db)
                    )
                })
            };
            if let Some(problem) = problem {
                self.push_task_error(
                    call,
                    format!("a task in a `group` returns its result, and this one returns {problem}"),
                    sink,
                );
            }
        }
    }

    /// The `select`-specific rules; collects each arm's channel as a use.
    fn validate_select(
        &self,
        select: ExprId,
        arms: &[SelectArm],
        allowed: &mut FxHashSet<ExprId>,
        sink: &mut DiagnosticSink<'_>,
    ) {
        if arms.is_empty() {
            self.push_task_error(
                select,
                "a `select` with no arms waits forever: give it a channel operation or a \
                 `default` arm"
                    .to_string(),
                sink,
            );
        }
        let mut seen_default = false;
        for arm in arms {
            match arm.op {
                SelectOp::Receive { channel } | SelectOp::Send { channel, .. } => {
                    if matches!(self.body[channel], Expr::Path(_)) {
                        allowed.insert(channel);
                    }
                }
                SelectOp::Default => {
                    if seen_default {
                        self.push_task_error(
                            arm.body,
                            "a `select` has at most one `default` arm".to_string(),
                            sink,
                        );
                    }
                    seen_default = true;
                }
                SelectOp::Invalid { expr } => self.push_task_error(
                    expr,
                    "a `select` arm waits on a channel operation: `name = <-ch -> ..`, \
                     `<-ch -> ..`, `ch <- value -> ..`, or `default -> ..`"
                        .to_string(),
                    sink,
                ),
            }
            if arm.binding.is_some() && !matches!(arm.op, SelectOp::Receive { .. }) {
                self.push_task_error(
                    arm.body,
                    "only a receive arm binds a name: `name = <-ch -> ..`".to_string(),
                    sink,
                );
            }
        }
    }

    /// `spawn f(args)`: a direct call to a module-local, non-generic,
    /// effect-free function whose parameters can cross between tasks (only
    /// scalars for a group's task).
    fn validate_spawned_call(&self, call: ExprId, in_group: bool, sink: &mut DiagnosticSink<'_>) {
        let Some(function) = self.spawned_function(call) else {
            if let Expr::Call { callee, .. } = &self.body[call] {
                if matches!(self.infer[*callee].interned(), TyKind::Function(_)) {
                    self.push_task_error(
                        call,
                        "`spawn` cannot run a function value: a closure may carry references \
                         to shared objects, and nothing proves them safe to use from another \
                         task; spawn a named function of this module instead"
                            .to_string(),
                        sink,
                    );
                    return;
                }
            }
            self.push_task_error(
                call,
                "`spawn` takes a call to a function, `spawn f(args)`".to_string(),
                sink,
            );
            return;
        };
        let name = function.name(self.db);
        if function.module(self.db) != self.func.module(self.db)
            || function.is_extern(self.db)
            || function.data(self.db).self_param().is_some()
        {
            self.push_task_error(
                call,
                format!(
                    "`spawn` runs a function of this module, and `{name}` is not one (or is \
                     `extern` or a method)"
                ),
                sink,
            );
            return;
        }
        if function.is_generic(self.db) {
            // A task starts through a trampoline made per function, and a
            // generic function has one body per instantiation.
            self.push_task_error(
                call,
                format!("`spawn` cannot run the generic function `{name}` yet"),
                sink,
            );
            return;
        }
        if !function.effects(self.db).is_empty() {
            self.push_task_error(
                call,
                format!("`{name}` uses effects, and a task has no handler to perform them through"),
                sink,
            );
        }
        let Some(sig) = function.ty(self.db).callable_sig(self.db) else {
            return;
        };
        for param in sig.params() {
            if in_group && !is_scalar(param) && !param.is_unknown() {
                self.push_task_error(
                    call,
                    format!(
                        "a task in a `group` can only be given integers, floats and `bool`, but \
                         `{name}` takes a `{}`: the group does not start the tasks after a \
                         failed one, and one that never runs must not leave another waiting on \
                         a channel (spawn it in a `scope` instead)",
                        param.display(self.db)
                    ),
                    sink,
                );
                break;
            }
            if !is_task_argument(param) {
                self.push_task_error(
                    call,
                    format!(
                        "a task can only be given integers, floats, `bool` and channels, but \
                         `{name}` takes a `{}`: tasks share nothing else, so they cannot race",
                        param.display(self.db)
                    ),
                    sink,
                );
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::tests::diagnostics;

    fn assert_clean(source: &str) {
        let diags = diagnostics(source);
        assert!(diags.is_empty(), "expected no diagnostics, got:\n{diags}");
    }

    fn assert_reports(source: &str, needle: &str) {
        let diags = diagnostics(source);
        assert!(
            diags.contains(needle),
            "expected a diagnostic containing `{needle}`, got:\n{diags}"
        );
    }

    #[test]
    fn well_formed_tasks_are_clean() {
        assert_clean(
            r"
    func produce(dest: Channel[i64], n: i64) {
        var i = 0;
        while i < n {
            dest <- i;
            i += 1;
        }
    }
    func main() -> i64 {
        scope {
            let ch = channel(4);
            spawn produce(ch, 3);
            let a = <-ch;
            let b = <-ch;
            let c = <-ch;
            a + b + c
        }
    }
    ",
        );
    }

    #[test]
    fn spawn_outside_a_scope() {
        assert_reports(
            "func work(x: i32) {}\nfunc main() { spawn work(1) }",
            "`spawn` must be inside a `scope { .. }` or `group { .. }` block",
        );
    }

    #[test]
    fn well_formed_groups_are_clean() {
        assert_clean(
            r"
    enum Result[T, E] { Ok(T) Err(E) }
    func square(x: i64) -> i64 { x * x }
    func check(x: i64) -> Result[i64, i64] {
        if x < 0 { Result.Err(x) } else { Result.Ok(x) }
    }
    func label(x: i64) -> String { x.to_string() }
    func squares(n: i64) -> [i64] {
        group {
            var i = 0;
            while i < n { spawn square(i); i += 1; }
        }
    }
    func checked(n: i64) -> Result[[i64], i64] {
        group {
            var i = 0;
            while i < n { spawn check(i); i += 1; }
        }
    }
    func labels() -> [String] {
        group { spawn label(1); spawn label(2); }
    }
    func both() -> i64 {
        // A group inside a scope: the channel belongs to the scope.
        scope {
            let ch = channel(1);
            let xs = group { spawn square(2); };
            ch <- xs[0];
            <-ch
        }
    }
    ",
        );
    }

    #[test]
    fn group_tasks_take_only_scalars() {
        assert_reports(
            r"
    func relay(ch: Channel[i64]) -> i64 { <-ch }
    func main() -> i64 {
        scope {
            let ch = channel(1);
            let xs = group { spawn relay(ch); };
            xs[0]
        }
    }
    ",
            "a task in a `group` can only be given integers, floats and `bool`",
        );
    }

    #[test]
    fn group_tasks_return_a_value() {
        assert_reports(
            "func work(x: i64) {}\nfunc main() { let xs = group { spawn work(1); }; }",
            "a task in a `group` returns its result, and this one returns nothing",
        );
    }

    #[test]
    fn group_task_results_are_not_secret() {
        assert_reports(
            "func work(x: i64) -> secret i64 { x }\nfunc main() { let xs = group { spawn work(1); }; }",
            "this one returns a `secret`",
        );
    }

    #[test]
    fn a_group_spawns_something() {
        assert_reports(
            "func main() { let xs = group { 1 }; }",
            "a `group` spawns at least one task",
        );
    }

    #[test]
    fn group_tasks_return_one_type() {
        assert_reports(
            r"
    func a(x: i64) -> i64 { x }
    func b(x: i64) -> bool { x > 0 }
    func main() { let xs = group { spawn a(1); spawn b(2); }; }
    ",
            "mismatched type",
        );
    }

    #[test]
    fn a_spawn_in_a_closure_in_a_group_is_outside_it() {
        assert_reports(
            r"
    func a(x: i64) -> i64 { x }
    func main() {
        let xs = group {
            spawn a(1);
            let f = func() { spawn a(2); };
        };
    }
    ",
            "`spawn` must be inside a `scope { .. }` or `group { .. }` block",
        );
    }

    #[test]
    fn spawn_needs_a_function_call() {
        assert_reports(
            "func main() { scope { spawn 1 } }",
            "`spawn` takes a call to a function",
        );
    }

    #[test]
    fn spawned_function_takes_only_scalars_and_channels() {
        assert_reports(
            "func work(xs: [i32]) {}\nfunc main() { scope { spawn work([1]) } }",
            "a task can only be given integers, floats, `bool` and channels",
        );
    }

    #[test]
    fn spawned_function_cannot_use_effects() {
        assert_reports(
            r"
    effect Ask { func ask() -> i32 }
    func work() uses Ask { perform Ask.ask(); }
    func main() { scope { spawn work() } }
    ",
            "uses effects, and a task has no handler",
        );
    }

    #[test]
    fn channel_outside_a_scope() {
        assert_reports(
            "func main() { let ch = channel(1); ch <- 1 }",
            "a channel must be created inside a `scope { .. }` block",
        );
    }

    #[test]
    fn channel_of_non_scalars() {
        assert_reports(
            "func main() { scope { let ch = channel(1); ch <- [1] } }",
            "a channel carries integers, floats or `bool`",
        );
    }

    #[test]
    fn channel_cannot_escape() {
        assert_reports(
            "func main() { scope { let ch = channel(1); let copy = ch; copy <- 1 } }",
            "a channel can only be used with `<-`",
        );
        assert_reports(
            "func leak(ch: Channel[i32]) -> Channel[i32] { ch }",
            "a channel can only be used with `<-`",
        );
    }

    #[test]
    fn send_to_a_non_channel() {
        assert_reports("func main() { let x = 1; x <- 2 }", "`<-` needs a channel");
    }

    #[test]
    fn well_formed_selects_are_clean() {
        assert_clean(
            r"
    func pump(a: Channel[i64], b: Channel[i64], flags: Channel[bool]) -> i64 {
        var total = 0;
        let x = 3;
        select {
            v = <-a -> total += v,
            <-b -> {},
            flags <- x > 2 -> total += 1,
            default -> total -= 1,
        };
        let picked = select {
            v = <-a -> v,
            w = <-b -> w * 2,
        };
        total + picked
    }
    ",
        );
    }

    #[test]
    fn select_without_arms_waits_forever() {
        assert_reports(
            "func main() { select {} }",
            "a `select` with no arms waits forever",
        );
    }

    #[test]
    fn select_with_two_defaults() {
        assert_reports(
            "func f(a: Channel[i32]) { select { <-a -> {}, default -> {}, default -> {} } }",
            "at most one `default` arm",
        );
    }

    #[test]
    fn select_arm_must_be_a_channel_operation() {
        assert_reports(
            "func g() -> i32 { 1 }\nfunc f() { select { g() -> {} } }",
            "a `select` arm waits on a channel operation",
        );
    }

    #[test]
    fn only_a_receive_arm_binds() {
        assert_reports(
            "func f(a: Channel[i32]) { select { v = a <- 1 -> {} } }",
            "only a receive arm binds a name",
        );
    }

    #[test]
    fn select_arm_types_must_agree() {
        assert_reports(
            "func f(a: Channel[i32]) -> i32 { select { v = <-a -> v, default -> true } }",
            "every `select` arm must have the same type",
        );
    }

    #[test]
    fn select_binding_is_scoped_to_its_arm() {
        let diags = diagnostics(
            "func f(a: Channel[i32]) -> i32 { select { v = <-a -> {}, default -> {} }; v }",
        );
        assert!(!diags.is_empty(), "`v` is not visible after the select");
    }

    #[test]
    fn select_channel_operands_follow_the_channel_rules() {
        assert_reports(
            "func f(a: Channel[i32]) { select { <-a -> {}, default -> {} }; let b = a; }",
            "a channel can only be used with `<-`",
        );
        let diags = diagnostics("func f(a: Channel[i32]) { select { a <- true -> {} } }");
        assert!(!diags.is_empty(), "a `bool` sent on a `Channel[i32]`");
    }
}
