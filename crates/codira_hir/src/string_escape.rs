//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Where a function's `String` values go: whether a value is only used
//!   while the function runs (read, compared, printed, copied into another
//!   string) or may be *kept* -- returned, stored in an object, an array or
//!   a map, captured by a closure -- and so outlive the call. Code
//!   generation asks this of every format plan (`codira_codegen`'s
//!   `ir::body::fabric`): a plan whose result is kept gets a buffer of its
//!   own, sized for it, while a temporary continues in the function's shared
//!   plan buffer. A `String` is a view of a buffer, so a kept view of a
//!   shared buffer would keep every other result in it alive.
//!
//! # The analysis
//!
//! [`HirDatabase::string_flows`] walks one body and records, for every
//! `String` expression and every local binding, the *sinks* its value flows
//! into: [`Sink::Kept`] (it may outlive the call), another local (`let b =
//! a`, `b = a`, a `match` or `for` binding), or a parameter of a function it
//! is passed to. [`is_kept`] follows those edges -- into the bodies of the
//! functions called, so `map.insert(f"k{i}", v)` is kept because `insert`
//! stores its key, and `check(f"{n:08}")` is not when `check` only reads its
//! argument -- and answers whether any path reaches `Kept`.
//!
//! What counts as kept:
//!
//! - the value of the body (the result) and of `return` and `break`;
//! - a field of a record literal, an element of an array or tuple literal, a
//!   value assigned to a field or an element (`a.f = v`, `xs[i] = v`), a
//!   value sent on a channel or performed as an effect;
//! - the arguments of `push`, `insert`, `append` and every other array
//!   method that stores or calls (only `contains`, `index_of` and
//!   `binary_search` merely read), and of `array_filled`;
//! - a local a closure captures, and the value of a closure's body;
//! - a parameter whose function keeps it, by the same rules; an `extern`
//!   function, a call through a function value, and a call more than
//!   [`MAX_DEPTH`] functions deep keep everything they are passed.
//!
//! A view taken of a string (`slice`, `trim`, `split`, `lines`, a
//! `String`'s `to_string()`) goes where the string goes, and so does a chain
//! `a + b` that starts with an existing `String`, which appends to `a`'s
//! buffer. Everything else reads or copies: comparisons, `len`, `find`,
//! `print`, a hole of a format string, the right side of `+=`, the pieces of
//! a chain that starts with new text.
//!
//! A trait method called on a type parameter (`key.eq(other)` in a generic
//! map) is taken to read its receiver and arguments: the code that runs is
//! chosen per instantiation, and the traits of the standard library compare
//! and hash. Nothing here affects what a program computes: a wrong answer
//! only puts a kept value among temporaries (it keeps their buffer alive)
//! or a temporary among kept values (its bytes stay as long as theirs).
//!
//! # Why not `crate::escape`
//!
//! The escape analysis behind frame allocation answers a stricter question
//! for objects: whether one may live in its function's frame. It must take
//! every string builtin as handing the value to the runtime, and a call into
//! another module as unknown code (a hot reload may replace the callee), so
//! by its summaries `s.len()` and `println(s)` keep `s`. Here reading a
//! string's bytes keeps nothing, and a stale answer after a hot reload only
//! moves bytes, so the callee's body is followed wherever it is.

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    code_model::Function,
    expr::Statement,
    ids::DefWithBodyId,
    resolve::{resolver_for_expr, ValueNs},
    ArithOp, ArrayMethod, BinaryOp, Body, BuiltinFn, CallableDef, Expr, ExprId, HirDatabase,
    InferenceResult, Pat, PatId, StringOp, TyKind,
};

/// How many functions deep [`is_kept`] follows arguments before it takes a
/// value as kept.
pub const MAX_DEPTH: u32 = 8;

/// Where a value goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sink {
    /// It may outlive the call.
    Kept,
    /// Into the local binding (or parameter) bound by this pattern.
    Local(PatId),
    /// Into a parameter of a function: the receiver (`self`) is parameter
    /// 0 of a method, the others follow in order.
    Arg(Function, u32),
}

/// What [`HirDatabase::string_flows`] records for one body.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StringFlows {
    /// The sinks of every `String` expression whose value goes somewhere.
    values: FxHashMap<ExprId, Vec<Sink>>,
    /// The sinks of every local binding and parameter whose value goes
    /// somewhere.
    locals: FxHashMap<PatId, Vec<Sink>>,
    /// The parameters' patterns, `self` first.
    params: Vec<PatId>,
    /// An `extern` function: whatever it is passed, it keeps.
    opaque: bool,
}

pub(crate) fn string_flows_query(db: &dyn HirDatabase, func: Function) -> Arc<StringFlows> {
    if func.is_extern(db) {
        return Arc::new(StringFlows {
            opaque: true,
            ..StringFlows::default()
        });
    }
    let owner: DefWithBodyId = func.id.into();
    let body = db.body(owner);
    let infer = db.infer(owner);
    let mut collector = Collector {
        db,
        owner,
        body: &body,
        infer: &infer,
        out: StringFlows::default(),
    };
    collector.out.params = body
        .self_param()
        .into_iter()
        .chain(body.params())
        .map(|(pat, _)| *pat)
        .collect();
    collector.flow(body.body_expr(), &[Sink::Kept]);
    Arc::new(collector.out)
}

/// Whether the value of `expr`, a `String` expression of `func`'s body, may
/// be kept past the call (see the module docs).
pub fn is_kept(db: &dyn HirDatabase, func: Function, expr: ExprId) -> bool {
    let flows = db.string_flows(func);
    let Some(sinks) = flows.values.get(&expr) else {
        return false;
    };
    Walk {
        db,
        seen: FxHashSet::default(),
    }
    .reaches_kept(func, &flows, sinks, 0)
}

/// Whether `func` may keep its parameter number `index` (`self` is 0 of a
/// method).
pub fn param_is_kept(db: &dyn HirDatabase, func: Function, index: u32) -> bool {
    Walk {
        db,
        seen: FxHashSet::default(),
    }
    .reaches_kept(func, &StringFlows::default(), &[Sink::Arg(func, index)], 0)
}

/// A search of the sink graph from one value.
struct Walk<'a> {
    db: &'a dyn HirDatabase,
    /// The bindings already followed: each is followed once, which ends
    /// recursion.
    seen: FxHashSet<(Function, PatId)>,
}

impl Walk<'_> {
    fn reaches_kept(
        &mut self,
        func: Function,
        flows: &StringFlows,
        sinks: &[Sink],
        depth: u32,
    ) -> bool {
        for &sink in sinks {
            let kept = match sink {
                Sink::Kept => true,
                Sink::Local(pat) => self.binding_is_kept(func, flows, pat, depth),
                Sink::Arg(callee, index) => {
                    if depth >= MAX_DEPTH {
                        return true;
                    }
                    let callee_flows = self.db.string_flows(callee);
                    match callee_flows.params.get(index as usize) {
                        _ if callee_flows.opaque => true,
                        None => true,
                        Some(&pat) => self.binding_is_kept(callee, &callee_flows, pat, depth + 1),
                    }
                }
            };
            if kept {
                return true;
            }
        }
        false
    }

    fn binding_is_kept(
        &mut self,
        func: Function,
        flows: &StringFlows,
        pat: PatId,
        depth: u32,
    ) -> bool {
        if !self.seen.insert((func, pat)) {
            return false;
        }
        match flows.locals.get(&pat) {
            Some(next) => self.reaches_kept(func, flows, next, depth),
            None => false,
        }
    }
}

/// The walk of one body that records [`StringFlows`].
struct Collector<'a> {
    db: &'a dyn HirDatabase,
    owner: DefWithBodyId,
    body: &'a Body,
    infer: &'a InferenceResult,
    out: StringFlows,
}

/// Where nothing is kept: a value read, compared or copied.
const NOWHERE: &[Sink] = &[];
/// Where a value is kept.
const KEPT: &[Sink] = &[Sink::Kept];

impl Collector<'_> {
    /// Visits `expr`, whose value goes to `sinks`.
    fn flow(&mut self, expr: ExprId, sinks: &[Sink]) {
        if !sinks.is_empty() && self.infer[expr].is_string() {
            add(self.out.values.entry(expr).or_default(), sinks);
        }
        let body = self.body;
        match &body[expr] {
            Expr::Missing | Expr::Literal(_) | Expr::Continue => {}
            Expr::Path(path) => {
                if sinks.is_empty() {
                    return;
                }
                let resolver = resolver_for_expr(self.db, self.owner, expr);
                if let Some((ValueNs::LocalBinding(pat), _)) =
                    resolver.resolve_path_as_value_fully(self.db, path)
                {
                    add(self.out.locals.entry(pat).or_default(), sinks);
                }
            }
            Expr::Call { callee, args } => {
                self.flow(*callee, NOWHERE);
                if let Some(builtin) = self.infer.builtin_call(expr) {
                    self.builtin(builtin, None, args, sinks);
                } else if self.infer.is_indirect_call(expr)
                    || self.infer.variant_of_expr(expr).is_some()
                {
                    self.all(args, KEPT);
                } else if let Some(CallableDef::Function(f)) =
                    self.infer[*callee].as_callable_def()
                {
                    for (index, &arg) in args.iter().enumerate() {
                        self.flow(arg, &[Sink::Arg(f, index as u32)]);
                    }
                } else {
                    self.all(args, KEPT);
                }
            }
            Expr::MethodCall { receiver, args, .. } => self.method_call(expr, *receiver, args, sinks),
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.flow(*condition, NOWHERE);
                self.flow(*then_branch, sinks);
                if let Some(branch) = else_branch {
                    self.flow(*branch, sinks);
                }
            }
            Expr::UnaryOp { expr: operand, .. } | Expr::Cast { expr: operand, .. } => {
                self.flow(*operand, NOWHERE);
            }
            Expr::BinaryOp { lhs, rhs, op } => self.binary(expr, *lhs, *rhs, *op, sinks),
            Expr::Index { base, index } => {
                self.flow(*base, NOWHERE);
                self.flow(*index, NOWHERE);
            }
            Expr::Block { statements, tail } => {
                for statement in statements {
                    match statement {
                        Statement::Let {
                            pat,
                            initializer: Some(init),
                            ..
                        } => {
                            let bound = self.bindings(*pat);
                            self.flow(*init, &bound);
                        }
                        Statement::Let { .. } => {}
                        Statement::Expr(e) => self.flow(*e, NOWHERE),
                    }
                }
                if let Some(tail) = tail {
                    self.flow(*tail, sinks);
                }
            }
            Expr::Return { expr: value } | Expr::Break { expr: value } => {
                if let Some(value) = value {
                    self.flow(*value, KEPT);
                }
            }
            Expr::Loop { body: inner } => self.flow(*inner, NOWHERE),
            Expr::While {
                condition,
                body: inner,
            } => {
                self.flow(*condition, NOWHERE);
                self.flow(*inner, NOWHERE);
            }
            Expr::For {
                pat,
                iterable,
                body: inner,
            } => {
                let bound = self.bindings(*pat);
                self.flow(*iterable, &bound);
                self.flow(*inner, NOWHERE);
            }
            Expr::Match {
                expr: scrutinee,
                arms,
                ..
            } => {
                let mut bound = Vec::new();
                for arm in arms {
                    add(&mut bound, &self.bindings(arm.pat));
                }
                self.flow(*scrutinee, &bound);
                for arm in arms {
                    self.flow(arm.expr, sinks);
                }
            }
            // `x!`, `x?`: the value inside is the same string.
            Expr::Unwrap { expr: inner } | Expr::Propagate { expr: inner } => {
                self.flow(*inner, sinks);
            }
            Expr::Format {
                value,
                width,
                precision,
                ..
            } => {
                self.flow(*value, NOWHERE);
                for count in width.iter().chain(precision) {
                    self.flow(*count, NOWHERE);
                }
            }
            Expr::Range { start, end, .. } => {
                self.flow(*start, NOWHERE);
                self.flow(*end, NOWHERE);
            }
            Expr::RecordLit { fields, spread, .. } => {
                for field in fields {
                    self.flow(field.expr, KEPT);
                }
                if let Some(spread) = spread {
                    self.flow(*spread, KEPT);
                }
            }
            Expr::Field { expr: base, .. } => self.flow(*base, NOWHERE),
            Expr::Array(elements) | Expr::Tuple(elements) => self.all(elements, KEPT),
            Expr::Scope { body: inner } => self.flow(*inner, sinks),
            Expr::Spawn { call } => self.flow(*call, NOWHERE),
            Expr::ChannelReceive { channel } => self.flow(*channel, NOWHERE),
            Expr::Offer { endpoint, branches } => {
                self.flow(*endpoint, NOWHERE);
                for branch in branches {
                    self.flow(*branch, sinks);
                }
            }
            Expr::Select { arms } => {
                for arm in arms {
                    for operand in arm.operands() {
                        self.flow(operand, KEPT);
                    }
                    self.flow(arm.body, sinks);
                }
            }
            Expr::Closure { body: inner, .. } => {
                if let Some(info) = self.infer.closure(expr) {
                    for &capture in &info.captures {
                        add(self.out.locals.entry(capture).or_default(), KEPT);
                    }
                }
                self.flow(*inner, KEPT);
            }
            // Effects, channels and anything else: everything is kept.
            other => other.walk_child_exprs(|child| self.flow(child, KEPT)),
        }
    }

    fn all(&mut self, exprs: &[ExprId], sinks: &[Sink]) {
        for &e in exprs {
            self.flow(e, sinks);
        }
    }

    /// The local bindings a pattern binds, as sinks.
    fn bindings(&self, pat: PatId) -> Vec<Sink> {
        let mut bound = Vec::new();
        let mut stack = vec![pat];
        while let Some(pat) = stack.pop() {
            if matches!(self.body[pat], Pat::Bind { .. }) {
                bound.push(Sink::Local(pat));
            }
            self.body[pat].walk_child_pats(|child| stack.push(child));
        }
        bound
    }

    /// A call of a builtin: `receiver` is the receiver of a method.
    fn builtin(
        &mut self,
        builtin: BuiltinFn,
        receiver: Option<ExprId>,
        args: &[ExprId],
        sinks: &[Sink],
    ) {
        let (receiver_sinks, arg_sinks): (&[Sink], &[Sink]) = match builtin {
            BuiltinFn::Print
            | BuiltinFn::Println
            | BuiltinFn::Likely
            | BuiltinFn::Unlikely
            | BuiltinFn::Fma
            | BuiltinFn::Channel
            | BuiltinFn::Cancelled
            | BuiltinFn::ArrayLen
            | BuiltinFn::ArrayUncheckedGet
            | BuiltinFn::StrLen => (NOWHERE, NOWHERE),
            // These return an operand.
            BuiltinFn::Declassify | BuiltinFn::CtSelect | BuiltinFn::Select => (NOWHERE, sinks),
            BuiltinFn::ArrayFilled | BuiltinFn::Pipeline(_) => (KEPT, KEPT),
            BuiltinFn::Array(
                ArrayMethod::Contains | ArrayMethod::IndexOf | ArrayMethod::BinarySearch,
            ) => (NOWHERE, NOWHERE),
            BuiltinFn::Array(_) => (NOWHERE, KEPT),
            // A view of the receiver goes where the result goes.
            BuiltinFn::String(
                StringOp::Split
                | StringOp::Lines
                | StringOp::Trim
                | StringOp::TrimStart
                | StringOp::TrimEnd
                | StringOp::Slice
                | StringOp::ToString,
            ) => (sinks, NOWHERE),
            BuiltinFn::String(_) => (NOWHERE, NOWHERE),
        };
        if let Some(receiver) = receiver {
            self.flow(receiver, receiver_sinks);
        }
        self.all(args, arg_sinks);
    }

    fn method_call(&mut self, expr: ExprId, receiver: ExprId, args: &[ExprId], sinks: &[Sink]) {
        if let Some(builtin) = self.infer.builtin_call(expr) {
            return self.builtin(builtin, Some(receiver), args, sinks);
        }
        if self.infer.is_indirect_call(expr)
            || self.infer.is_field_call(expr)
            || self.infer.variant_of_expr(expr).is_some()
        {
            self.flow(receiver, KEPT);
            return self.all(args, KEPT);
        }
        let Some(method) = self.infer.method_resolution(expr) else {
            self.flow(receiver, KEPT);
            return self.all(args, KEPT);
        };
        let method = Function::from(method);
        let on_type_param = method.trait_container(self.db).is_some()
            && self
                .infer
                .instantiation(expr)
                .and_then(|args| args.first())
                .is_none_or(crate::mono::contains_type_param);
        if on_type_param {
            self.flow(receiver, NOWHERE);
            return self.all(args, NOWHERE);
        }
        let first = if self.db.fn_data(method.id).has_self_param() {
            self.flow(receiver, &[Sink::Arg(method, 0)]);
            1
        } else {
            self.flow(receiver, NOWHERE);
            0
        };
        for (index, &arg) in args.iter().enumerate() {
            self.flow(arg, &[Sink::Arg(method, first + index as u32)]);
        }
    }

    fn binary(
        &mut self,
        expr: ExprId,
        lhs: ExprId,
        rhs: ExprId,
        op: Option<BinaryOp>,
        sinks: &[Sink],
    ) {
        match op {
            Some(BinaryOp::Assignment { op: None }) => {
                let target = match &self.body[lhs] {
                    Expr::Path(path) => {
                        let resolver = resolver_for_expr(self.db, self.owner, lhs);
                        match resolver.resolve_path_as_value_fully(self.db, path) {
                            Some((ValueNs::LocalBinding(pat), _)) => vec![Sink::Local(pat)],
                            _ => KEPT.to_vec(),
                        }
                    }
                    _ => KEPT.to_vec(),
                };
                self.flow(lhs, NOWHERE);
                self.flow(rhs, &target);
            }
            Some(BinaryOp::ArithOp(ArithOp::Add)) if self.infer[expr].is_string() => {
                // The pieces of the chain, left to right. A chain that starts
                // with new text copies every piece into a new buffer; one that
                // starts with an existing `String` appends to it, so its
                // first piece goes where the chain goes.
                let mut parts = vec![rhs];
                let mut current = lhs;
                while let Expr::BinaryOp {
                    lhs,
                    rhs,
                    op: Some(BinaryOp::ArithOp(ArithOp::Add)),
                } = &self.body[current]
                {
                    if !self.infer[current].is_string() {
                        break;
                    }
                    parts.push(*rhs);
                    current = *lhs;
                }
                let first_sinks = if self.is_fresh(current) {
                    NOWHERE
                } else {
                    sinks
                };
                self.flow(current, first_sinks);
                self.all(&parts, NOWHERE);
            }
            _ => {
                self.flow(lhs, NOWHERE);
                self.flow(rhs, NOWHERE);
            }
        }
    }

    /// Whether the first piece of a chain makes new text (as code
    /// generation decides it): a `str`, a hole, a non-`String`'s
    /// `to_string()`.
    fn is_fresh(&self, expr: ExprId) -> bool {
        if matches!(self.infer[expr].interned(), TyKind::Str) {
            return true;
        }
        match &self.body[expr] {
            Expr::Format { .. } => true,
            Expr::MethodCall { receiver, .. } => {
                self.infer.builtin_call(expr) == Some(BuiltinFn::String(StringOp::ToString))
                    && !self.infer[*receiver].is_string()
            }
            _ => false,
        }
    }
}

/// Adds `new` to `sinks`, once each.
fn add(sinks: &mut Vec<Sink>, new: &[Sink]) {
    for &sink in new {
        if !sinks.contains(&sink) {
            sinks.push(sink);
        }
    }
}

#[cfg(test)]
mod tests;
