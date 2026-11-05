//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - The effect analysis behind `@noalloc`, `@nopanic`, `@noblock` and
//!   `@realtime`: what every function can do (allocate, block, panic, run
//!   unboundedly), why, and the diagnostics for annotated functions that do
//!   what their attribute rules out.
//!
//! # The analysis
//!
//! Four effects are tracked. [`HirDatabase::effect_sources`] lists what a
//! function's own body does -- each source with its location and the effects
//! it has -- and every call it makes. [`HirDatabase::effect_summary`] closes
//! that over the call graph: it collects every function reachable from the
//! summarised one, marks the recursive ones (Tarjan's strongly connected
//! components), and propagates effects along call edges to a fixpoint, so
//! recursion terminates. For every effect present the summary keeps one
//! *witness*: the chain of calls down to the source that causes it, which is
//! what a diagnostic prints.
//!
//! An attribute forbids effects: `@noalloc` Alloc, `@nopanic` Panic,
//! `@noblock` Block, `@realtime` all three plus Unbounded. Every source in
//! the annotated function's body, and every call whose callee's summary has
//! a forbidden effect, is an error.
//!
//! # Where each effect comes from (today's code generator and runtime)
//!
//! Alloc -- every one of these is a call into the runtime that can also take
//! the GC heap's lock (Block) and aborts the process when the heap limit or
//! the machine's memory is exhausted (Panic):
//!
//! * constructing a `class` (any garbage-collected struct): a record literal, a
//!   tuple-constructor call, or a unit value -- the `new` intrinsic;
//! * an array literal -- `new_array`. Arrays cannot grow today;
//! * `print`/`println`: the runtime formats numbers into a heap string and
//!   writes under standard output's lock;
//! * `channel(n)`, `scope { .. }` (its task bookkeeping; its end waits for
//!   every task) and `spawn` (copies the arguments to the heap and queues the
//!   task); a channel send can grow the channel's queue; `select` (its wait
//!   list, with or without `default`) and `S.open()` (a session's channels);
//! * a call to a function of *another module* with a value-struct parameter or
//!   result: such calls go through the runtime's dispatch table, whose calling
//!   convention boxes those structs on the GC heap.
//!
//! String literals are constants and allocate nothing; value structs,
//! tuples and effect handler records live on the stack.
//!
//! Block: channel send and receive (on a session endpoint too, and
//! `ep.choose(..)` and `offer`), `select`, the end of a `scope`, and the
//! allocations above (locks).
//!
//! Panic (anything that ends the call abnormally):
//!
//! * `a[i]` and `a[i] = v`: the bounds check calls
//!   `codira.panic_index_out_of_bounds`. `a.unchecked_get(i)` has no check and
//!   is the documented escape hatch (an out-of-range index there reads
//!   arbitrary memory instead);
//! * integer `/` and `%` (and `/=`, `%=`): both code generators check the
//!   divisor and call `codira.panic` with the source location on a zero
//!   divisor, and, signed, on `MIN / -1` and `MIN % -1`. A division is safe --
//!   no check is emitted and it is no source here -- when its divisor is a
//!   positive integer literal, or when the function has a contract and the
//!   verifier *proves* both obligations it already emits for that division
//!   (non-zero divisor, no overflow). The decision is made once, here
//!   ([`EffectSources::safe_divisions`], [`division_is_safe`]), and the code
//!   generators follow it;
//! * postfix `!` on an optional, which panics on `nil`;
//! * calling a `@runtime_checked` function with a `requires` clause: the check
//!   traps on entry when it fails;
//! * every allocation (see above).
//!
//! Unbounded (only `@realtime` forbids it):
//!
//! * a `while` or `loop` without a `decreases` measure, or whose measure the
//!   verifier does not prove (non-negative at the head, smaller after the
//!   body). `for` loops are bounded by construction: the iteration count is
//!   fixed before the first iteration;
//! * recursion, direct or mutual.
//!
//! Everything the analysis cannot see counts as all four: an `extern`
//! function (C, runtime, `@asm`), a `perform` (its handler is chosen at run
//! time), and a call through anything that is not a named function. An
//! `extern` declaration may carry `@noalloc`, `@nopanic`, `@noblock` or
//! `@realtime` as a *trusted assertion*: nothing checks it, and the
//! analysis takes it at its word. `extern "codira-intrinsic"` operations are
//! emitted in place and have none of the effects.
//!
//! # What is not covered, honestly
//!
//! * Stack overflow. Every call and every `stack_alloc` uses stack, and without
//!   a stack bound the analysis cannot rule out running out of it, so
//!   `@nopanic` and `@realtime` do not.
//! * A proven division relies on the function's `requires`, which every Codira
//!   caller is proven to satisfy; a host calling through `Runtime::invoke` is
//!   trusted to.
//! * Shifts never panic: the shift amount is taken modulo the operand's width
//!   (`agents/USING_CODIRA.md`, "Integer arithmetic"), so they are no source.
//! * `codira build --fuel` makes every function entry and loop iteration a
//!   point where the call can stop; such a build rejects `@nopanic` and
//!   `@realtime` functions ([`fuel_diagnostics`]).
//! * `@realtime` proves termination, not a time bound: `@deadline(..)` would
//!   need a cycle model of the target CPU and is rejected.

use std::sync::Arc;

use codira_syntax::{
    ast::{self, AstNode, AttributeOwner, NameOwner},
    SyntaxNodePtr,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    code_model::Function,
    diagnostics::{DiagnosticSink, EffectAttributeViolation},
    expr::{ArithOp, BinaryOp},
    in_file::InFile,
    resolve::{resolver_for_expr, ValueNs},
    verify::{exec::Frame, ObligationKind, Verdict, VerificationReport},
    BuiltinFn, CallableDef, Expr, ExprId, HasSource, HirDatabase, Literal, Module, Path,
    StructMemoryKind, TyKind,
};

pub mod envelope;
#[cfg(test)]
mod tests;

pub use envelope::{
    envelope_report, inferred_values, package_envelope_violations, Envelope, FunctionEnvelope,
    PackageViolation,
};

/// Something a function can do that an attribute can rule out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Effect {
    /// Allocates on the GC heap (or the runtime allocates on its behalf).
    Alloc,
    /// Waits for another thread: a channel, a scope's end, a lock.
    Block,
    /// Ends the call abnormally: a panic, a trap, an abort, or undefined
    /// behaviour.
    Panic,
    /// May run without a bound: a loop without a proven measure, or
    /// recursion.
    Unbounded,
    /// Runs code the analysis cannot see: an `extern` function, `@asm`, a
    /// handler chosen at run time, or a call through a function value.
    /// Tracked for `@envelope(foreign: none)`.
    Foreign,
    /// Starts concurrent work: `scope` or `spawn`. Tracked for
    /// `@envelope(concurrency: none)`.
    Spawn,
    /// Performs an algebraic effect (`perform`). Tracked for
    /// `@envelope(effects: none)`.
    Perform,
    /// Handles data the program marks `secret`: a `secret` parameter,
    /// result or `let`, an `oblivious` function, or `declassify`. Tracked
    /// for `@envelope(secrecy: none)`.
    Secret,
}

impl Effect {
    pub const ALL: [Effect; 8] = [
        Effect::Alloc,
        Effect::Block,
        Effect::Panic,
        Effect::Unbounded,
        Effect::Foreign,
        Effect::Spawn,
        Effect::Perform,
        Effect::Secret,
    ];

    fn bit(self) -> u8 {
        1 << (self as u8)
    }

    /// What a function with this effect does, as a verb phrase.
    pub fn verb(self) -> &'static str {
        match self {
            Effect::Alloc => "allocates",
            Effect::Block => "may block",
            Effect::Panic => "may panic",
            Effect::Unbounded => "may not terminate",
            Effect::Foreign => "runs code outside the analysis",
            Effect::Spawn => "starts concurrent tasks",
            Effect::Perform => "performs algebraic effects",
            Effect::Secret => "handles `secret` data",
        }
    }
}

/// A set of [`Effect`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EffectSet(u8);

impl EffectSet {
    pub const NONE: EffectSet = EffectSet(0);
    pub const ALL: EffectSet = EffectSet(0b1111_1111);
    /// The four effects the `@noalloc`/`@nopanic`/`@noblock`/`@realtime`
    /// attributes are about: Alloc, Block, Panic and Unbounded.
    pub const CLASSIC: EffectSet = EffectSet(0b1111);
    /// A GC allocation: it can take the heap lock and aborts when memory
    /// runs out.
    const GC_ALLOC: EffectSet = EffectSet(0b0111);
    /// `scope` and `spawn`: an allocation that starts concurrent work.
    const TASKS: EffectSet = EffectSet(0b0010_0111);
    /// Everything that comes from code the analysis cannot see, except
    /// handling secrets: a secret can only reach such code through a
    /// function that itself mentions `secret`, which is counted there.
    const OPAQUE_PUBLIC: EffectSet = EffectSet(0b0111_1111);

    pub fn of(effects: &[Effect]) -> EffectSet {
        EffectSet(effects.iter().fold(0, |bits, e| bits | e.bit()))
    }

    pub fn contains(self, effect: Effect) -> bool {
        self.0 & effect.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn union(self, other: EffectSet) -> EffectSet {
        EffectSet(self.0 | other.0)
    }

    pub fn intersection(self, other: EffectSet) -> EffectSet {
        EffectSet(self.0 & other.0)
    }

    pub fn without(self, other: EffectSet) -> EffectSet {
        EffectSet(self.0 & !other.0)
    }

    pub fn iter(self) -> impl Iterator<Item = Effect> {
        Effect::ALL.into_iter().filter(move |e| self.contains(*e))
    }

    /// "allocates, may block and may panic".
    pub fn verbs(self) -> String {
        let verbs: Vec<&str> = self.iter().map(Effect::verb).collect();
        match verbs.as_slice() {
            [] => String::new(),
            [only] => (*only).to_string(),
            [init @ .., last] => format!("{} and {last}", init.join(", ")),
        }
    }
}

/// One thing a function's own body does that has effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub effects: EffectSet,
    pub site: InFile<SyntaxNodePtr>,
    /// What happens there, as a noun phrase ("class `Particle`
    /// constructed").
    pub what: String,
}

/// A call to a named function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEdge {
    pub site: InFile<SyntaxNodePtr>,
    pub callee: Function,
}

/// What a function's own body does, before looking into its callees.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectSources {
    pub sources: Vec<Source>,
    pub calls: Vec<CallEdge>,
    /// The integer divisions and remainders (`/`, `%`, `/=`, `%=`) of the
    /// body that cannot fail: the divisor is a positive integer literal, or
    /// the verifier proved it non-zero (and, signed, the division free of
    /// `MIN / -1`). Both code generators omit the run-time check for exactly
    /// these and check every other integer division, so this one decision
    /// is what both the analysis and the generated code follow.
    pub safe_divisions: Vec<ExprId>,
}

/// Whether `expr` of `body` is an integer division or remainder (`/`, `%`,
/// `/=`, `%=`) whose divisor the generated code must check: every one that
/// is not in its function's [`EffectSources::safe_divisions`]. Both code
/// generators ask this for each integer division they emit, and on `true`
/// branch to a panic on a zero divisor and, for a signed type, on
/// `MIN / -1` and `MIN % -1`.
pub fn division_needs_check(
    db: &dyn HirDatabase,
    body: &crate::Body,
    infer: &crate::InferenceResult,
    expr: ExprId,
) -> bool {
    let Expr::BinaryOp {
        lhs,
        op:
            Some(
                BinaryOp::ArithOp(ArithOp::Divide | ArithOp::Remainder)
                | BinaryOp::Assignment {
                    op: Some(ArithOp::Divide | ArithOp::Remainder),
                },
            ),
        ..
    } = &body[expr]
    else {
        return false;
    };
    if !matches!(infer[*lhs].interned(), TyKind::Int(_)) {
        return false;
    }
    let crate::ids::DefWithBodyId::FunctionId(id) = body.owner();
    !db.effect_sources(Function::from(id))
        .safe_divisions
        .contains(&expr)
}

/// Why a function has an effect: the calls from it down to the function
/// whose own body has the source, and the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Witness {
    /// Empty when the source is in the summarised function itself.
    pub calls: Vec<CallEdge>,
    pub source: Source,
}

/// Every effect a call to a function can have, with one witness each.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectSummary {
    pub effects: EffectSet,
    witnesses: Vec<(Effect, Witness)>,
    /// Present when the function is recursive: the call that leads back to
    /// it.
    pub recursion: Option<Source>,
}

impl EffectSummary {
    pub fn witness(&self, effect: Effect) -> Option<&Witness> {
        self.witnesses
            .iter()
            .find_map(|(e, w)| (*e == effect).then_some(w))
    }
}

/// An attribute that rules effects out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectAttr {
    NoAlloc,
    NoPanic,
    NoBlock,
    Realtime,
}

impl EffectAttr {
    fn from_name(name: &str) -> Option<EffectAttr> {
        Some(match name {
            "noalloc" => EffectAttr::NoAlloc,
            "nopanic" => EffectAttr::NoPanic,
            "noblock" => EffectAttr::NoBlock,
            "realtime" => EffectAttr::Realtime,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            EffectAttr::NoAlloc => "@noalloc",
            EffectAttr::NoPanic => "@nopanic",
            EffectAttr::NoBlock => "@noblock",
            EffectAttr::Realtime => "@realtime",
        }
    }

    /// The effects a function carrying this attribute may not have.
    pub fn forbids(self) -> EffectSet {
        match self {
            EffectAttr::NoAlloc => EffectSet::of(&[Effect::Alloc]),
            EffectAttr::NoPanic => EffectSet::of(&[Effect::Panic]),
            EffectAttr::NoBlock => EffectSet::of(&[Effect::Block]),
            EffectAttr::Realtime => EffectSet::CLASSIC,
        }
    }
}

/// The effect attributes on a function, its `@envelope`, and its
/// `@deadline` attributes.
struct Attributes {
    effects: Vec<(EffectAttr, ast::Attribute)>,
    deadlines: Vec<ast::Attribute>,
    /// The first `@envelope`; later ones are in `extra_envelopes`.
    envelope: Option<(Envelope, ast::Attribute)>,
    extra_envelopes: Vec<ast::Attribute>,
}

impl Attributes {
    fn of(db: &dyn HirDatabase, func: Function) -> Attributes {
        let mut attrs = Attributes {
            effects: Vec::new(),
            deadlines: Vec::new(),
            envelope: None,
            extra_envelopes: Vec::new(),
        };
        let Some(list) = func.source(db).value.attribute_list() else {
            return attrs;
        };
        for attr in list.attributes() {
            let name = attr
                .path()
                .and_then(|p| p.segment())
                .and_then(|s| match s.kind() {
                    Some(ast::PathSegmentKind::Name(n)) => Some(n.text().to_string()),
                    _ => None,
                });
            match name.as_deref() {
                Some("deadline") => attrs.deadlines.push(attr),
                Some("envelope") if envelope::is_envelope(&attr) => {
                    if attrs.envelope.is_some() {
                        attrs.extra_envelopes.push(attr);
                    } else {
                        attrs.envelope = Some((Envelope::parse(&attr), attr));
                    }
                }
                Some(name) => {
                    if let Some(effect_attr) = EffectAttr::from_name(name) {
                        attrs.effects.push((effect_attr, attr));
                    }
                }
                None => {}
            }
        }
        attrs
    }

    fn forbidden(&self) -> EffectSet {
        let attrs = self
            .effects
            .iter()
            .fold(EffectSet::NONE, |set, (attr, _)| set.union(attr.forbids()));
        match &self.envelope {
            Some((envelope, _)) => attrs.union(envelope.forbidden()),
            None => attrs,
        }
    }

    /// "`@noalloc` and `@nopanic`": the attributes that forbid any of
    /// `effects`.
    fn names_forbidding(&self, effects: EffectSet) -> String {
        let mut names: Vec<&str> = Vec::new();
        for (attr, _) in &self.effects {
            if !attr.forbids().intersection(effects).is_empty() && !names.contains(&attr.name()) {
                names.push(attr.name());
            }
        }
        let mut names: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
        if let Some(text) = self
            .envelope
            .as_ref()
            .and_then(|(envelope, _)| envelope.name_forbidding(effects))
        {
            names.push(format!("`{text}`"));
        }
        names.join(" and ")
    }
}

// ---- local sources --------------------------------------------------------

/// The query behind [`HirDatabase::effect_sources`].
pub(crate) fn effect_sources_query(db: &dyn HirDatabase, func: Function) -> Arc<EffectSources> {
    let mut out = EffectSources::default();
    if func.is_extern(db) {
        if func.is_intrinsic(db) {
            return Arc::new(out);
        }
        let asserted = Attributes::of(db, func).forbidden();
        let effects = EffectSet::OPAQUE_PUBLIC.without(asserted);
        if !effects.is_empty() {
            out.sources.push(Source {
                effects,
                site: name_site(db, func),
                what: format!(
                    "`{}` is an `extern` function, which the analysis cannot see into (its \
                     declaration may assert `@noalloc`, `@nopanic`, `@noblock`, `@realtime` or \
                     an `@envelope`)",
                    func.name(db)
                ),
            });
        }
        push_secret_source(db, func, &mut out);
        return Arc::new(out);
    }

    push_secret_source(db, func, &mut out);
    let frame = Frame::new(db, func);
    let root = frame.body.body_expr();
    if matches!(frame.body[root], Expr::Missing) {
        return Arc::new(out);
    }
    let mut collector = Collector {
        db,
        frame,
        report: None,
        out,
    };

    if crate::verify::is_runtime_checked(db, func) {
        if let Some(&first) = collector.frame.body.contracts().requires.first() {
            if let Some(site) = collector.frame.site(first) {
                collector.out.sources.push(Source {
                    effects: EffectSet::of(&[Effect::Panic]),
                    site,
                    what: format!(
                        "`{}` is `@runtime_checked`: its `requires` clauses are checked on entry \
                         and trap when one fails",
                        func.name(db)
                    ),
                });
            }
        }
    }

    collector.visit(root);
    Arc::new(collector.out)
}

/// A function whose own source mentions `secret` (a parameter, the result
/// or a `let`), or that is `oblivious`, handles secret data.
fn push_secret_source(db: &dyn HirDatabase, func: Function, out: &mut EffectSources) {
    let src = func.source(db).value;
    let mentions_secret = src.is_oblivious()
        || src
            .syntax()
            .descendants()
            .any(|node| node.kind() == codira_syntax::SyntaxKind::SECRET_TYPE);
    if mentions_secret {
        out.sources.push(Source {
            effects: EffectSet::of(&[Effect::Secret]),
            site: name_site(db, func),
            what: format!(
                "`{}` takes, returns or binds a `secret` value (or is `oblivious`)",
                func.name(db)
            ),
        });
    }
}

/// The location of a function's name, for sources that are about the whole
/// function.
fn name_site(db: &dyn HirDatabase, func: Function) -> InFile<SyntaxNodePtr> {
    let src = func.source(db);
    let ptr = src.value.name().map_or_else(
        || SyntaxNodePtr::new(src.value.syntax()),
        |name| SyntaxNodePtr::new(name.syntax()),
    );
    InFile::new(src.file_id, ptr)
}

struct Collector<'a> {
    db: &'a dyn HirDatabase,
    frame: Frame,
    report: Option<Arc<VerificationReport>>,
    out: EffectSources,
}

impl Collector<'_> {
    fn source(&mut self, expr: ExprId, effects: EffectSet, what: impl Into<String>) {
        if let Some(site) = self.frame.site(expr) {
            self.out.sources.push(Source {
                effects,
                site,
                what: what.into(),
            });
        }
    }

    fn visit(&mut self, expr: ExprId) {
        let body = Arc::clone(&self.frame.body);
        match &body[expr] {
            // Nothing of their own; their operands are visited below.
            Expr::Missing
            | Expr::Literal(_)
            | Expr::If { .. }
            | Expr::UnaryOp { .. }
            | Expr::Block { .. }
            | Expr::Return { .. }
            // `x?` returns early (a `Result`'s `Err`, or `nil`): an ordinary
            // return, not a panic.
            | Expr::Propagate { .. }
            | Expr::Break { .. }
            | Expr::Continue
            | Expr::Range { .. }
            | Expr::Field { .. }
            | Expr::Tuple(_)
            | Expr::Cast { .. }
            | Expr::Handle { .. }
            | Expr::Resume { .. }
            // Inference proved it exhaustive, so its arms decide everything.
            // (The code generator still ends the arms with a panic for a value
            // no arm matches, which only a hot reload that removed an enum
            // variant can produce.) A string-literal arm compares bytes in
            // the runtime without allocating or blocking.
            | Expr::Match { .. }
            // Bounded by construction: the iteration count is fixed before
            // the first iteration, and the counter is not the program's.
            | Expr::For { .. } => {}
            Expr::Path(_) if self.frame.infer.function_value(expr).is_some() => self.source(
                expr,
                EffectSet::GC_ALLOC,
                "a function used as a value allocates its function value object",
            ),
            Expr::Path(path) => self.unit_struct(expr, path),
            // The body runs only when the closure is called, and a call
            // through a function value is unknown (below); what the literal
            // itself does is allocate. Its body is still visited, which can
            // only over-approximate.
            Expr::Closure { .. } => self.source(
                expr,
                EffectSet::GC_ALLOC,
                "a closure allocates its function value object",
            ),
            Expr::Call { .. } | Expr::MethodCall { .. }
                if self.frame.infer.is_indirect_call(expr) =>
            {
                self.source(
                    expr,
                    EffectSet::ALL,
                    "a call through a function value, whose code the analysis cannot see",
                );
            }
            Expr::RecordLit { .. } => {
                if let Some(s) = self.frame.infer[expr].as_struct() {
                    self.construct(expr, s);
                }
            }
            Expr::Call { callee, .. } => {
                if let Some(builtin) = self.frame.infer.builtin_call(expr) {
                    self.builtin(expr, builtin);
                } else {
                    match self.frame.infer[*callee].as_callable_def() {
                        Some(CallableDef::Function(f)) => self.call(expr, f),
                        Some(CallableDef::Struct(s)) => self.construct(expr, s),
                        None => self.source(
                            expr,
                            EffectSet::ALL,
                            "a call through a value that is not a named function, which the \
                             analysis cannot see",
                        ),
                    }
                }
            }
            // `S.open()`: the runtime allocates the pair's channels.
            Expr::MethodCall { .. }
                if crate::expr::opened_session(self.db, &self.frame.body, expr).is_some() =>
            {
                self.source(
                    expr,
                    EffectSet::GC_ALLOC,
                    "opening a session allocates its two channels",
                );
            }
            // `ep.choose(k)`: a send on the session's channel.
            Expr::MethodCall { receiver, .. }
                if matches!(self.frame.infer[*receiver].interned(), TyKind::Session(..)) =>
            {
                self.source(
                    expr,
                    EffectSet::of(&[Effect::Block]),
                    "`choose` waits while the other endpoint has not taken the previous message",
                );
            }
            Expr::MethodCall { .. } => {
                if let Some(builtin) = self.frame.infer.builtin_call(expr) {
                    self.builtin(expr, builtin);
                } else if self.frame.infer.variant_of_expr(expr).is_some() {
                    // `Shape.Circle(..)` builds a value: no call, no allocation.
                } else {
                    match self.frame.infer.method_resolution(expr) {
                        // A trait method called on a type parameter (`x.show()`
                        // for `x: T`, `T: Show`) reaches a different
                        // implementation per instantiation, and this analysis
                        // runs once per generic body, before any is chosen:
                        // what resolution recorded is only the trait's
                        // declaration (or default body), not the code that
                        // runs. So the call is treated as unknown -- every
                        // effect -- which is sound for every instantiation.
                        Some(f)
                            if Function::from(f).trait_container(self.db).is_some()
                                && self
                                    .frame
                                    .infer
                                    .instantiation(expr)
                                    .and_then(|args| args.first())
                                    .is_none_or(crate::mono::contains_type_param) =>
                        {
                            self.source(
                                expr,
                                EffectSet::ALL,
                                "a trait method called on a generic type, whose implementation \
                                 is chosen per instantiation and is not known here",
                            );
                        }
                        Some(f) => self.call(expr, Function::from(f)),
                        None => self.source(
                            expr,
                            EffectSet::ALL,
                            "a method call the analysis cannot resolve",
                        ),
                    }
                }
            }
            Expr::BinaryOp { lhs, rhs, op } => {
                if let Some(
                    BinaryOp::ArithOp(op @ (ArithOp::Divide | ArithOp::Remainder))
                    | BinaryOp::Assignment {
                        op: Some(op @ (ArithOp::Divide | ArithOp::Remainder)),
                    },
                ) = op
                {
                    self.division(expr, *lhs, *rhs, *op);
                }
            }
            Expr::Unwrap { .. } => self.source(
                expr,
                EffectSet::of(&[Effect::Panic]),
                "postfix `!` panics when the optional is `nil`",
            ),
            Expr::Index { .. } => self.source(
                expr,
                EffectSet::of(&[Effect::Panic]),
                "indexing panics when the index is out of range (`unchecked_get` skips the check)",
            ),
            Expr::Array(_) => self.source(expr, EffectSet::GC_ALLOC, "array literal allocated"),
            Expr::Loop { .. } | Expr::While { .. } => self.loop_bound(expr),
            Expr::Perform { .. } => self.source(
                expr,
                EffectSet::ALL,
                "`perform` runs a handler chosen at run time, which the analysis cannot see",
            ),
            Expr::Scope { .. } => self.source(
                expr,
                EffectSet::TASKS,
                "`scope` allocates its task bookkeeping and waits for its tasks at the end",
            ),
            Expr::Spawn { .. } => self.source(
                expr,
                EffectSet::TASKS,
                "`spawn` copies the task's arguments to the heap and queues it",
            ),
            Expr::ChannelSend { .. } => self.source(
                expr,
                EffectSet::GC_ALLOC,
                "channel send waits while the channel is full (and may grow its queue)",
            ),
            Expr::ChannelReceive { .. } => self.source(
                expr,
                EffectSet::of(&[Effect::Block]),
                "channel receive waits while the channel is empty",
            ),
            // Even with `default` a select takes its channels' locks; it
            // also builds its wait list on the heap.
            Expr::Select { arms } => self.source(
                expr,
                EffectSet::GC_ALLOC,
                if arms
                    .iter()
                    .any(|arm| matches!(arm.op, crate::SelectOp::Default))
                {
                    "`select` locks its channels and allocates its wait list"
                } else {
                    "`select` waits until one of its channels is ready, and allocates its wait \
                     list"
                },
            ),
            Expr::Offer { .. } => self.source(
                expr,
                EffectSet::of(&[Effect::Block]),
                "`offer` waits for the other endpoint's choice",
            ),
        }

        let mut children = Vec::new();
        body[expr].walk_child_exprs(|child| children.push(child));
        for child in children {
            self.visit(child);
        }
    }

    fn builtin(&mut self, expr: ExprId, builtin: BuiltinFn) {
        match builtin {
            BuiltinFn::Print | BuiltinFn::Println => self.source(
                expr,
                EffectSet::GC_ALLOC,
                format!(
                    "`{}` formats its arguments on the heap and writes them under standard \
                     output's lock",
                    builtin.name()
                ),
            ),
            BuiltinFn::Channel => {
                self.source(
                    expr,
                    EffectSet::GC_ALLOC,
                    "`channel(..)` allocates a channel",
                );
            }
            BuiltinFn::ArrayFilled => {
                self.source(
                    expr,
                    EffectSet::GC_ALLOC,
                    "`array_filled(..)` allocates an array",
                );
            }
            BuiltinFn::Declassify => self.source(
                expr,
                EffectSet::of(&[Effect::Secret]),
                "`declassify` releases a `secret` value",
            ),
            BuiltinFn::CtSelect
            | BuiltinFn::Select
            | BuiltinFn::Likely
            | BuiltinFn::Unlikely
            | BuiltinFn::Fma
            | BuiltinFn::ArrayLen
            | BuiltinFn::ArrayUncheckedGet
            | BuiltinFn::StrLen => {}
        }
    }

    /// A unit value of a struct type (`Foo`), which constructs it.
    fn unit_struct(&mut self, expr: ExprId, path: &Path) {
        let Some(s) = self.frame.infer[expr].as_struct() else {
            return;
        };
        if s.data(self.db).memory_kind != StructMemoryKind::Gc {
            return;
        }
        let resolver = resolver_for_expr(self.db, self.frame.owner, expr);
        if matches!(
            resolver.resolve_path_as_value_fully(self.db, path),
            Some((ValueNs::StructId(_) | ValueNs::ImplSelf(_), _))
        ) {
            self.construct(expr, s);
        }
    }

    fn construct(&mut self, expr: ExprId, s: crate::Struct) {
        if s.data(self.db).memory_kind == StructMemoryKind::Gc {
            let what = format!("class `{}` constructed", s.name(self.db));
            self.source(expr, EffectSet::GC_ALLOC, what);
        }
    }

    fn call(&mut self, expr: ExprId, callee: Function) {
        let Some(site) = self.frame.site(expr) else {
            return;
        };
        self.out.calls.push(CallEdge {
            site: site.clone(),
            callee,
        });
        // A call into another module goes through the dispatch table, whose
        // calling convention boxes value structs on the GC heap.
        if callee.is_extern(self.db) || callee.module(self.db) == self.frame.func.module(self.db) {
            return;
        }
        let Some(sig) = callee.ty(self.db).callable_sig(self.db) else {
            return;
        };
        let boxed = sig
            .params()
            .iter()
            .chain(std::iter::once(sig.ret()))
            .filter_map(crate::Ty::as_struct)
            .find(|s| s.data(self.db).memory_kind == StructMemoryKind::Value);
        if let Some(s) = boxed {
            let what = format!(
                "the call to `{}` in another module goes through the runtime's dispatch table, \
                 which boxes the struct `{}` on the GC heap",
                callee.name(self.db),
                s.name(self.db)
            );
            self.out.sources.push(Source {
                effects: EffectSet::GC_ALLOC,
                site,
                what,
            });
        }
    }

    fn report(&mut self) -> Option<Arc<VerificationReport>> {
        if !crate::verify::is_participating(self.db, self.frame.func) {
            return None;
        }
        Some(Arc::clone(self.report.get_or_insert_with(|| {
            self.db.verify_function(self.frame.func)
        })))
    }

    /// Whether the verifier proved every obligation of each of `kinds` at
    /// `expr`, with at least one obligation of each kind.
    fn proven(&mut self, expr: ExprId, kinds: &[ObligationKind]) -> bool {
        let Some(report) = self.report() else {
            return false;
        };
        let Some(site) = self.frame.site(expr) else {
            return false;
        };
        kinds.iter().all(|kind| {
            let mut found = false;
            for o in report
                .obligations
                .iter()
                .filter(|o| &o.kind == kind && o.site == site)
            {
                if !matches!(o.verdict, Verdict::Proven) {
                    return false;
                }
                found = true;
            }
            found
        })
    }

    fn division(&mut self, expr: ExprId, lhs: ExprId, rhs: ExprId, op: ArithOp) {
        let crate::TyKind::Int(int_ty) = self.frame.infer[lhs].interned() else {
            // Float division has no failure mode.
            return;
        };
        let signed = int_ty.signedness.is_signed();
        // A positive literal divisor is never zero, and `MIN / k` for k >= 1
        // never overflows.
        if let Expr::Literal(Literal::Int(lit)) = &self.frame.body[rhs] {
            if lit.value > 0 {
                self.out.safe_divisions.push(expr);
                return;
            }
        }
        let mut kinds = vec![ObligationKind::DivisionByZero];
        if signed {
            kinds.push(ObligationKind::DivisionOverflow);
        }
        if self.proven(expr, &kinds) {
            self.out.safe_divisions.push(expr);
            return;
        }
        let symbol = if op == ArithOp::Divide { "/" } else { "%" };
        let what = format!(
            "integer `{symbol}` whose divisor is not proven {}: the generated code checks it and \
             panics when it fails (give the function a contract that proves it)",
            if signed {
                "non-zero and free of `MIN / -1` overflow"
            } else {
                "non-zero"
            }
        );
        self.source(expr, EffectSet::of(&[Effect::Panic]), what);
    }

    fn loop_bound(&mut self, expr: ExprId) {
        let measure = self
            .frame
            .body
            .contracts()
            .loops
            .get(&expr)
            .and_then(|c| c.decreases);
        let unbounded = EffectSet::of(&[Effect::Unbounded]);
        let Some(measure) = measure else {
            self.source(expr, unbounded, "loop without a `decreases` measure");
            return;
        };
        if !self.proven(
            measure,
            &[
                ObligationKind::MeasureNonNegative,
                ObligationKind::MeasureDecreases,
            ],
        ) && !self.proven_without_back_edge(measure)
        {
            self.source(
                expr,
                unbounded,
                "loop whose `decreases` measure is not proven",
            );
        }
    }

    /// A loop whose body never reaches its end (every path breaks or
    /// returns) has no `MeasureDecreases` obligation: it runs at most once.
    /// Its measure counts as proven when the head obligation is.
    fn proven_without_back_edge(&mut self, measure: ExprId) -> bool {
        let Some(report) = self.report() else {
            return false;
        };
        let Some(site) = self.frame.site(measure) else {
            return false;
        };
        let at_site: Vec<_> = report
            .obligations
            .iter()
            .filter(|o| o.site == site)
            .collect();
        !at_site
            .iter()
            .any(|o| o.kind == ObligationKind::MeasureDecreases)
            && at_site
                .iter()
                .any(|o| o.kind == ObligationKind::MeasureNonNegative)
            && at_site.iter().all(|o| matches!(o.verdict, Verdict::Proven))
    }
}

// ---- summaries ------------------------------------------------------------

/// The query behind [`HirDatabase::effect_summary`].
pub(crate) fn effect_summary_query(db: &dyn HirDatabase, func: Function) -> Arc<EffectSummary> {
    // Every function reachable from `func`, in breadth-first order.
    let mut nodes = vec![func];
    let mut index: FxHashMap<Function, usize> = FxHashMap::default();
    index.insert(func, 0);
    let mut sources = Vec::new();
    let mut edges: Vec<Vec<(CallEdge, usize)>> = Vec::new();
    let mut next = 0;
    while next < nodes.len() {
        let local = db.effect_sources(nodes[next]);
        let mut out = Vec::new();
        for call in &local.calls {
            let target = *index.entry(call.callee).or_insert_with(|| {
                nodes.push(call.callee);
                nodes.len() - 1
            });
            out.push((call.clone(), target));
        }
        edges.push(out);
        sources.push(local);
        next += 1;
    }

    let n = nodes.len();
    let component = strongly_connected_components(&edges);

    let mut effects = vec![EffectSet::NONE; n];
    let mut witnesses: Vec<Vec<(Effect, Witness)>> = vec![Vec::new(); n];
    let mut recursion: Vec<Option<Source>> = vec![None; n];

    // Recursion: the first call that stays inside the caller's own
    // component. Another function in the same component lies on a cycle
    // through the caller; a self-call is a cycle by itself.
    for v in 0..n {
        let recursive_call = edges[v].iter().find(|(_, w)| component[*w] == component[v]);
        if let Some((call, w)) = recursive_call {
            let name = nodes[v].name(db);
            let what = if *w == v {
                format!("`{name}` is recursive: it calls itself")
            } else {
                format!(
                    "`{name}` is recursive: it calls `{}`, which can call `{name}` again",
                    nodes[*w].name(db)
                )
            };
            recursion[v] = Some(Source {
                effects: EffectSet::of(&[Effect::Unbounded]),
                site: call.site.clone(),
                what,
            });
        }
    }

    // Own sources first.
    for v in 0..n {
        let own = sources[v].sources.iter().chain(recursion[v].iter());
        for source in own {
            for effect in source.effects.iter() {
                if !effects[v].contains(effect) {
                    effects[v] = effects[v].union(EffectSet::of(&[effect]));
                    witnesses[v].push((
                        effect,
                        Witness {
                            calls: Vec::new(),
                            source: source.clone(),
                        },
                    ));
                }
            }
        }
    }

    // Then along call edges, to a fixpoint. A witness is set once, from a
    // callee that already had the effect, so every chain ends at a source.
    let mut changed = true;
    while changed {
        changed = false;
        for v in 0..n {
            for (call, w) in &edges[v] {
                let new = effects[*w].without(effects[v]);
                for effect in new.iter() {
                    let Some(callee_witness) = witnesses[*w]
                        .iter()
                        .find_map(|(e, wit)| (*e == effect).then_some(wit))
                    else {
                        continue;
                    };
                    let mut calls = vec![call.clone()];
                    calls.extend(callee_witness.calls.iter().cloned());
                    let witness = Witness {
                        calls,
                        source: callee_witness.source.clone(),
                    };
                    effects[v] = effects[v].union(EffectSet::of(&[effect]));
                    witnesses[v].push((effect, witness));
                    changed = true;
                }
            }
        }
    }

    Arc::new(EffectSummary {
        effects: effects[0],
        witnesses: std::mem::take(&mut witnesses[0]),
        recursion: recursion[0].take(),
    })
}

/// Tarjan's algorithm, iteratively: the component index of every node.
fn strongly_connected_components(edges: &[Vec<(CallEdge, usize)>]) -> Vec<usize> {
    const UNVISITED: usize = usize::MAX;
    let n = edges.len();
    let mut index = vec![UNVISITED; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut component = vec![UNVISITED; n];
    let mut next_index = 0;
    let mut next_component = 0;

    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        // (node, next edge to look at)
        let mut work = vec![(root, 0usize)];
        index[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut edge)) = work.last_mut() {
            if let Some((_, w)) = edges[v].get(*edge) {
                let w = *w;
                *edge += 1;
                if index[w] == UNVISITED {
                    index[w] = next_index;
                    low[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    component[w] = next_component;
                    if w == v {
                        break;
                    }
                }
                next_component += 1;
            }
        }
    }
    component
}

// ---- diagnostics ----------------------------------------------------------

/// "line 12", or "other.code:12" when `site` is in another file than
/// `home`.
fn location(
    db: &dyn HirDatabase,
    home: codira_hir_input::FileId,
    site: &InFile<SyntaxNodePtr>,
) -> String {
    let line = db
        .line_index(site.file_id)
        .line_col(site.value.range().start())
        .line
        + 1;
    if site.file_id == home {
        format!("line {line}")
    } else {
        format!("{}:{line}", db.file_relative_path(site.file_id))
    }
}

/// ", which calls `h` (line 20), which allocates: class `P` constructed at
/// line 30" -- the part of a message after the first call.
fn render_witness(
    db: &dyn HirDatabase,
    home: codira_hir_input::FileId,
    effects: EffectSet,
    witness: &Witness,
) -> String {
    let mut text = String::new();
    for call in &witness.calls {
        text.push_str(&format!(
            ", which calls `{}` ({})",
            call.callee.name(db),
            location(db, home, &call.site)
        ));
    }
    text.push_str(&format!(
        ", which {}: {} at {}",
        effects.verbs(),
        witness.source.what,
        location(db, home, &witness.source.site)
    ));
    text
}

/// Checks `func`'s `@noalloc`/`@nopanic`/`@noblock`/`@realtime` attributes
/// against what it does, and rejects `@deadline`.
pub(crate) fn realtime_diagnostics(
    db: &dyn HirDatabase,
    func: Function,
    sink: &mut DiagnosticSink<'_>,
) {
    let attrs = Attributes::of(db, func);
    let file = func.source(db).file_id;
    for attr in &attrs.deadlines {
        sink.push(EffectAttributeViolation {
            file,
            site: SyntaxNodePtr::new(attr.syntax()),
            message: "`@deadline` is not supported: a worst-case execution time in time units \
                      needs a cycle model of the target CPU (pipeline, caches, clock), which \
                      the compiler does not have. `@realtime` proves the function terminates \
                      without allocating, blocking or panicking, but not how long it takes"
                .to_string(),
        });
    }
    for (attr, syntax) in &attrs.effects {
        if syntax.arg_list().is_some() {
            sink.push(EffectAttributeViolation {
                file,
                site: SyntaxNodePtr::new(syntax.syntax()),
                message: format!("`{}` takes no arguments", attr.name()),
            });
        }
    }
    for attr in &attrs.extra_envelopes {
        sink.push(EffectAttributeViolation {
            file,
            site: SyntaxNodePtr::new(attr.syntax()),
            message: "a function has at most one `@envelope`; merge the clauses into the first"
                .to_string(),
        });
    }
    if let Some((envelope, syntax)) = &attrs.envelope {
        for error in &envelope.errors {
            sink.push(EffectAttributeViolation {
                file,
                site: SyntaxNodePtr::new(syntax.syntax()),
                message: error.clone(),
            });
        }
    }
    // On an `extern` declaration the attributes are trusted assertions.
    let forbidden = attrs.forbidden();
    if forbidden.is_empty() || func.is_extern(db) {
        return;
    }

    let name = func.name(db);
    let mut seen: FxHashSet<(InFile<SyntaxNodePtr>, String)> = FxHashSet::default();
    let mut push = |site: InFile<SyntaxNodePtr>, message: String| {
        if seen.insert((site.clone(), message.clone())) {
            sink.push(EffectAttributeViolation {
                file: site.file_id,
                site: site.value,
                message,
            });
        }
    };

    let local = db.effect_sources(func);
    for source in &local.sources {
        let hit = source.effects.intersection(forbidden);
        if !hit.is_empty() {
            push(
                source.site.clone(),
                format!(
                    "`{name}` is {} but {} here: {}",
                    attrs.names_forbidding(hit),
                    hit.verbs(),
                    source.what
                ),
            );
        }
    }

    if forbidden.contains(Effect::Unbounded) {
        if let Some(recursion) = &db.effect_summary(func).recursion {
            push(
                recursion.site.clone(),
                format!(
                    "`{name}` is {} but may not terminate here: {}",
                    attrs.names_forbidding(recursion.effects),
                    recursion.what
                ),
            );
        }
    }

    for call in &local.calls {
        // Its own sources are reported where they are, and recursion above.
        if call.callee == func {
            continue;
        }
        let summary = db.effect_summary(call.callee);
        let hit = summary.effects.intersection(forbidden);
        // One message per distinct explanation, naming every effect it
        // explains.
        let mut groups: Vec<(EffectSet, &Witness)> = Vec::new();
        for effect in hit.iter() {
            let Some(witness) = summary.witness(effect) else {
                continue;
            };
            match groups.iter_mut().find(|(_, w)| *w == witness) {
                Some((set, _)) => *set = set.union(EffectSet::of(&[effect])),
                None => groups.push((EffectSet::of(&[effect]), witness)),
            }
        }
        for (effects, witness) in groups {
            push(
                call.site.clone(),
                format!(
                    "`{name}` is {} but calls `{}` ({}){}",
                    attrs.names_forbidding(effects),
                    call.callee.name(db),
                    location(db, file, &call.site),
                    render_witness(db, file, effects, witness)
                ),
            );
        }
    }
}

/// The diagnostics a fuel-instrumented build (`codira build --fuel`) adds:
/// such a build can stop any instrumented function at its entry or at a loop
/// iteration, so no function in it can be `@nopanic` or `@realtime`.
pub fn fuel_diagnostics(db: &dyn HirDatabase, module: Module, sink: &mut DiagnosticSink<'_>) {
    let mut functions: Vec<Function> = module
        .declarations(db)
        .into_iter()
        .filter_map(|def| match def {
            crate::ModuleDef::Function(f) => Some(f),
            _ => None,
        })
        .collect();
    for item in module.impls(db) {
        for associated_item in item.items(db) {
            let crate::code_model::AssocItem::Function(f) = associated_item;
            functions.push(f);
        }
    }
    for func in functions {
        if func.is_extern(db) {
            continue;
        }
        let attrs = Attributes::of(db, func);
        if let Some((envelope, syntax)) = &attrs.envelope {
            if let Some(text) = envelope.name_forbidding(EffectSet::of(&[Effect::Panic])) {
                sink.push(EffectAttributeViolation {
                    file: func.source(db).file_id,
                    site: SyntaxNodePtr::new(syntax.syntax()),
                    message: format!(
                        "`{}` is `{text}`, but this build is instrumented with fuel (`--fuel`): \
                         every call and loop iteration can stop it when the fuel runs out",
                        func.name(db),
                    ),
                });
            }
        }
        for (attr, syntax) in &attrs.effects {
            if attr.forbids().contains(Effect::Panic) {
                sink.push(EffectAttributeViolation {
                    file: func.source(db).file_id,
                    site: SyntaxNodePtr::new(syntax.syntax()),
                    message: format!(
                        "`{}` is `{}`, but this build is instrumented with fuel (`--fuel`): \
                         every call and loop iteration can stop it when the fuel runs out",
                        func.name(db),
                        attr.name()
                    ),
                });
            }
        }
    }
}
