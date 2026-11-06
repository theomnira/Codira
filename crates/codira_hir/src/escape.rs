//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Escape analysis on typed HIR: for every allocation a function's body
//!   makes (class instances, array literals, `array_filled`, the arrays
//!   `copy`/`map`/`filter`/slicing build, closures), whether the object can
//!   outlive the call that made it, and why.
//! - Per-function escape summaries ([`HirDatabase::escape_summary`]): what a
//!   callee does with each parameter, computed to a fixpoint over recursion,
//!   so a call is analysed without looking into the callee again.
//! - The placement decision code generation follows: [`Placement::Frame`]
//!   for an object that may live in its function's stack frame, with the
//!   reason for every object that stays on the GC heap.
//!
//! # What the analysis promises (the correctness contract)
//!
//! [`HirDatabase::escape_analysis`] marks an allocation [`Placement::Frame`]
//! only if, in every execution of every instance of the function:
//!
//! 1. **No reference to the object survives the call.** It is not returned
//!    (also not inside a value struct, tuple, enum or optional, and not by
//!    `?`), not stored into a field or element of any object that is not
//!    itself a frame object of the same call, not captured by a closure, not
//!    sent to a task or channel, not passed to `perform` or `resume`, and not
//!    passed to any code whose behaviour the analysis does not know (an
//!    `extern` function, a call through a function value, a trait method on
//!    a generic type, a function of another module, a builtin the analysis
//!    does not model).
//! 2. **No object on the GC heap ever holds a reference to it.** The
//!    collector traces heap objects; it must never be handed the address of
//!    a frame. So anything stored into an object that is not a frame object
//!    is on the heap, transitively: an object on the heap makes everything
//!    stored in it heap as well.
//! 3. **It is dead before its allocation runs again.** A frame object gets
//!    one stack slot per allocation site. A site inside a loop (or inside a
//!    closure an array method or pipeline compiles in place, which runs once
//!    per element) is reused every iteration, so its object may not flow
//!    into a binding, or into a frame object, that lives outside the loop.
//! 4. **The runtime never sees it.** Only generated code touches it: field
//!    and element loads and stores, the inline array methods listed in
//!    [`inline_array_method`], comparison by address, and the debug form
//!    (`{x:?}`, which only records the address while it prints).
//!
//! Every property is established by over-approximation: the analysis tracks
//! which abstract objects ([`Obj`]) each local binding, each object's
//! fields, and each expression may hold; everything it cannot see holds
//! [`Obj::Unknown`]; storing into `Unknown` is storing into the heap. When
//! the analysis meets a construct it does not model, every object the
//! construct's operands may hold escapes. Unknown means escapes.
//!
//! # Summaries
//!
//! A parameter's [`ParamEscape`] covers the parameter *and everything
//! reachable from it*: reading a field of a parameter yields the parameter's
//! own abstract object again. `NoEscape` means the callee neither keeps nor
//! returns anything reachable from the argument; `Returned` that it may
//! return it (or something reachable from it), so the call's result may
//! hold everything reachable from the argument; `Escapes` that it may keep
//! it beyond the call. Storing into a field of a parameter counts as storing
//! into the heap: the argument may be a heap object. A callee that writes a
//! heap object into a field of a frame object is fine (the frame is scanned,
//! see below), and a later read of that field holds `Unknown`.
//!
//! Recursion: [`HirDatabase::escape_summary`] finds the strongly connected
//! component of the call graph that holds the function and iterates every
//! member's summary from `NoEscape` upwards until none changes. The
//! transfer functions are monotone and the lattice is finite, so this
//! terminates with the least fixpoint, which is sound for every call.
//!
//! Calls into another module are unknown code: a hot reload replaces one
//! module's code without recompiling its callers, so a callee's summary is
//! only trusted inside the module it is compiled with.
//!
//! # Why a frame object is safe for the collector, hot reload, heap
//! # snapshots and debug printing
//!
//! A frame object has the heap's layout: a 32-byte header whose first word
//! points to the payload, and the payload, both in the function's frame. A
//! reference to it is the address of that header, like a heap handle, so
//! code that receives one cannot tell the two apart.
//!
//! * **Collector.** The collector scans every stack conservatively, from the
//!   parked thread's stack pointer to its base, and the callee-saved
//!   registers. A frame object's payload is in its function's frame, which
//!   is in that range for as long as the object is alive, so every GC
//!   reference stored in its fields is seen as a root. The header's own
//!   `data` word points into the stack, not into the heap, and is ignored
//!   by the scan. By (2) no heap object refers to a frame object, so the
//!   collector never traces one. The collector does not move objects, so
//!   the frame holds handles that stay valid.
//! * **Hot reload** migrates heap objects in place, but only while no thread
//!   is inside Codira code (`Runtime::update` defers otherwise), so no frame,
//!   and no frame object, is alive when it runs.
//! * **Heap snapshots** write the heap's objects. No heap object refers to a
//!   frame object (2), so a snapshot is complete without them; a snapshot
//!   is taken while no other thread runs Codira code, like a collection.
//! * **Debug printing** is generated code reading fields (see
//!   `codira_codegen`'s `ir/body/format.rs`); the runtime only records the
//!   object's address to detect cycles.
//! * **The host API** sees an object only through a return value, a field of
//!   a returned object, or an argument of an `extern` call, all of which are
//!   escapes.
//!
//! Objects of these kinds are never frame objects: closures (their function
//! value object is read by the runtime's call cells), `array_filled` and the
//! arrays `copy`, `map`, `filter`, slicing and pipelines build (their length
//! is only known at run time), array literals of more than
//! [`MAX_FRAME_ARRAY_LEN`] elements, and anything allocated inside a closure
//! body, a `handle` expression or a `spawn`.
//!
//! # Reuse
//!
//! The results are salsa queries, keyed by [`Function`]:
//!
//! * [`HirDatabase::escape_summary`] -- the per-parameter summary, for any
//!   analysis that wants to know whether a call keeps its arguments (for
//!   instance, whether a reference count must be incremented for a call);
//! * [`HirDatabase::escape_analysis`] -- every allocation of the body with
//!   its [`Placement`] and its reason.
//!
//! Both are pure functions of the HIR: they do not depend on the target or
//! on code generation options.
//!
//! # Regions: the design, and why they are not built yet
//!
//! A statically checked region would extend frame allocation to the objects
//! it cannot take: arrays whose length is known only at run time
//! (`array_filled`, `map`, `filter`, `copy`, slices), array literals longer
//! than [`MAX_FRAME_ARRAY_LEN`], and objects allocated in a loop and kept
//! past their iteration. The shape that fits the language as it is:
//!
//! * **Syntax**: an attribute on a function, `@region`, parsed like
//!   `@noalloc` (attributes attach to items only; the grammar has no
//!   attributed blocks, and adding one would change the shared grammar for
//!   one feature). A block-scoped region is a nested function.
//! * **Semantics**: every allocation the function's own body makes, other
//!   than one that goes to the frame, comes from a bump arena created at
//!   entry and freed on every exit (the code generator already undoes state
//!   on each return for `--sanitize=memtag`). Callees allocate as usual.
//! * **Check**: every allocation of the function whose [`Placement`] is an
//!   escape is an error naming the escape, e.g. "`f` is `@region` but class
//!   `Node` (line 12) escapes: returned from `f`", reported through the
//!   same sink as the effect attributes. No new effect is needed: a region
//!   restricts where the function's own objects may go, which is exactly
//!   what this analysis computes; `@noalloc` keeps counting region
//!   allocations as allocations.
//!
//! What blocks it is the collector, not the checker. A frame object is safe
//! because its payload is on the stack, which the collector scans; an arena
//! chunk is not, so a GC reference stored in a region object (a `String`
//! field, a heap object's handle) would not keep its object alive. Making
//! it safe needs one of two things the collector does not have: a way to
//! register extra conservative root ranges for the duration of a call (the
//! arena's chunks), or precise maps of the reference slots of region objects.
//! Both belong to the collector's root-finding work (precise stack maps),
//! not to this analysis. Without them, the only sound region is one whose
//! objects hold no references to the heap at all -- numbers, and other
//! region or frame objects -- which is too narrow to be worth its runtime
//! interface (arena calls in the dispatch table, a growth path that
//! allocates in the arena instead of calling the runtime's `array_grow`).

use std::{collections::BTreeSet, fmt, sync::Arc};

use codira_syntax::SyntaxNodePtr;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    code_model::Function,
    expr::{BinaryOp, Expr, ExprId, Pat, PatId, Statement},
    in_file::InFile,
    resolve::{resolver_for_expr, ValueNs},
    verify::exec::Frame,
    ArrayMethod, BuiltinFn, CallableDef, HirDatabase, Name, StructMemoryKind, TyKind,
};

#[cfg(test)]
mod tests;

/// The longest array literal that may be placed in a frame.
pub const MAX_FRAME_ARRAY_LEN: usize = 16;

/// What one allocation site allocates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllocKind {
    /// An instance of a `class`: a record literal, a tuple-constructor call
    /// or a unit value.
    Class { name: Name },
    /// An array literal of `len` elements.
    ArrayLiteral { len: usize },
    /// An array whose length is known only at run time, built by the named
    /// builtin (`array_filled`, `copy`, `map`, `filter`, a slice, `collect`).
    BuiltArray { by: &'static str },
    /// A closure's function value object.
    Closure,
}

impl fmt::Display for AllocKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AllocKind::Class { name } => write!(f, "class `{name}`"),
            AllocKind::ArrayLiteral { len: 0 } => write!(f, "empty array literal"),
            AllocKind::ArrayLiteral { len: 1 } => write!(f, "array literal of 1 element"),
            AllocKind::ArrayLiteral { len } => write!(f, "array literal of {len} elements"),
            AllocKind::BuiltArray { by } => write!(f, "array built by `{by}`"),
            AllocKind::Closure => write!(f, "closure"),
        }
    }
}

/// Why an object is on the GC heap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    // ---- escapes: a reference may outlive the call ----
    /// Returned from the function (also inside a value, or by `?`).
    Returned { from: Name },
    /// Stored into a field (or, with `field: None`, an element) of an object
    /// that is, or may be, on the heap.
    StoredInHeap { field: Option<Name> },
    /// Stored into a field (or element) of a parameter, which may be a heap
    /// object.
    StoredInParam { param: String, field: Option<Name> },
    /// Stored into an object that is itself on the heap.
    InsideHeapObject { container: String },
    /// Mentioned inside a closure body, which captures it.
    CapturedByClosure,
    /// An argument of `spawn`, copied to the heap for the task.
    Spawned,
    /// Sent on a channel.
    SentOnChannel,
    /// Mentioned inside a `handle` expression (its arms run as handlers).
    InsideHandle,
    /// Passed to `perform`, or given back by `resume`.
    Performed,
    /// The value of a `loop` (`break value`).
    LoopValue,
    /// Passed to a function whose summary says the parameter escapes.
    PassedTo {
        callee: Name,
        param: String,
        why: Box<Reason>,
    },
    /// Used by code the analysis does not see or does not model.
    Unknown { what: String },
    // ---- kept on the heap without escaping ----
    /// Would be reused by the next iteration of a loop while still
    /// reachable: it flows into something that lives outside the loop.
    OutlivesIteration { into: String },
    /// The array literal is longer than [`MAX_FRAME_ARRAY_LEN`].
    TooLong { len: usize },
    /// Its length is only known at run time.
    RuntimeLength,
    /// A closure's function value object, which the runtime's call cells
    /// read; never placed in a frame.
    ClosureObject,
    /// Allocated inside a closure body, a `handle` or a `spawn`, which the
    /// analysis treats as separate, unknown code.
    InsideOpaque { what: &'static str },
}

impl Reason {
    /// Whether a reference to the object may outlive the call that made it
    /// (as opposed to staying on the heap for another reason).
    pub fn is_escape(&self) -> bool {
        !matches!(
            self,
            Reason::OutlivesIteration { .. }
                | Reason::TooLong { .. }
                | Reason::RuntimeLength
                | Reason::ClosureObject
                | Reason::InsideOpaque { .. }
        )
    }

    /// The reason as a phrase: "returned from `make`".
    pub fn phrase(&self) -> String {
        let field = |field: &Option<Name>| match field {
            Some(name) => format!("field `{name}`"),
            None => "an element".to_string(),
        };
        match self {
            Reason::Returned { from } => format!("returned from `{from}`"),
            Reason::StoredInHeap { field: f } => {
                format!("stored into {} of a heap object", field(f))
            }
            Reason::StoredInParam { param, field: f } => format!(
                "stored into {} of parameter `{param}`, which may be a heap object",
                field(f)
            ),
            Reason::InsideHeapObject { container } => {
                format!("stored into {container}, which is on the heap")
            }
            Reason::CapturedByClosure => "captured by a closure".to_string(),
            Reason::Spawned => "sent to a task by `spawn`".to_string(),
            Reason::SentOnChannel => "sent on a channel".to_string(),
            Reason::InsideHandle => "used inside a `handle` expression".to_string(),
            Reason::Performed => "passed to an effect handler".to_string(),
            Reason::LoopValue => "the value of a `loop`".to_string(),
            Reason::PassedTo { callee, param, why } => format!(
                "passed to `{callee}` as `{param}`, which is {}",
                why.phrase()
            ),
            Reason::Unknown { what } => what.clone(),
            Reason::OutlivesIteration { into } => format!(
                "kept past its loop iteration in {into} (its frame slot is reused each \
                 iteration)"
            ),
            Reason::TooLong { len } => format!(
                "{len} elements; arrays of at most {MAX_FRAME_ARRAY_LEN} elements are moved to \
                 the stack"
            ),
            Reason::RuntimeLength => "its length is known only at run time".to_string(),
            Reason::ClosureObject => "function value objects stay on the heap".to_string(),
            Reason::InsideOpaque { what } => {
                format!("allocated inside {what}, which is analysed as separate code")
            }
        }
    }
}

/// Where an allocation's object lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// In the stack frame of the function that allocates it.
    Frame,
    /// On the GC heap, for this reason.
    Heap(Reason),
}

impl fmt::Display for Placement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Placement::Frame => write!(f, "moved to the stack"),
            Placement::Heap(reason) if reason.is_escape() => {
                write!(f, "escapes: {}", reason.phrase())
            }
            Placement::Heap(reason) => write!(f, "stays on the heap: {}", reason.phrase()),
        }
    }
}

/// One allocation site of a function body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    pub expr: ExprId,
    pub kind: AllocKind,
    pub placement: Placement,
    /// The syntax of the allocating expression, for reports.
    pub site: Option<InFile<SyntaxNodePtr>>,
}

/// What a function does with one parameter (and everything reachable from
/// it). Ordered: a later variant covers the earlier ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamEscape {
    NoEscape,
    Returned,
    Escapes(Reason),
}

impl ParamEscape {
    fn rank(&self) -> u8 {
        match self {
            ParamEscape::NoEscape => 0,
            ParamEscape::Returned => 1,
            ParamEscape::Escapes(_) => 2,
        }
    }
}

/// What a call to a function does with its arguments. Parameters are
/// numbered in call order: `self` first when the function has one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EscapeSummary {
    pub params: Vec<ParamEscape>,
    /// The parameters' names, for messages.
    pub names: Vec<String>,
}

impl EscapeSummary {
    /// The summary of a function the analysis cannot see into: every
    /// parameter escapes.
    fn unknown(arity: usize, why: &str) -> EscapeSummary {
        EscapeSummary {
            params: vec![
                ParamEscape::Escapes(Reason::Unknown {
                    what: why.to_string()
                });
                arity
            ],
            names: (0..arity).map(|i| format!("#{i}")).collect(),
        }
    }

    pub fn param(&self, index: usize) -> &ParamEscape {
        self.params.get(index).unwrap_or(&ParamEscape::NoEscape)
    }
}

/// The escape analysis of one function body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FunctionEscapes {
    pub allocations: Vec<Allocation>,
    pub summary: EscapeSummary,
}

impl FunctionEscapes {
    /// The placement of the allocation `expr`, if `expr` allocates.
    pub fn placement(&self, expr: ExprId) -> Option<&Placement> {
        self.allocations
            .iter()
            .find(|a| a.expr == expr)
            .map(|a| &a.placement)
    }

    /// Whether `expr` allocates an object that may live in the frame.
    pub fn is_frame(&self, expr: ExprId) -> bool {
        self.placement(expr) == Some(&Placement::Frame)
    }
}

// ---- queries ---------------------------------------------------------------

/// The query behind [`HirDatabase::escape_callees`]: the functions whose
/// summaries the analysis of `func` reads.
pub(crate) fn escape_callees_query(db: &dyn HirDatabase, func: Function) -> Arc<Vec<Function>> {
    let mut callees = Vec::new();
    let mut seen = FxHashSet::default();
    let _ = analyze(db, func, &mut |callee| {
        if seen.insert(callee) {
            callees.push(callee);
        }
        None
    });
    Arc::new(callees)
}

/// The query behind [`HirDatabase::escape_summary`].
pub(crate) fn escape_summary_query(db: &dyn HirDatabase, func: Function) -> Arc<EscapeSummary> {
    // Every function reachable from `func`, and the call edges between them.
    let mut nodes = vec![func];
    let mut index: FxHashMap<Function, usize> = FxHashMap::default();
    index.insert(func, 0);
    let mut edges: Vec<Vec<usize>> = Vec::new();
    let mut next = 0;
    while next < nodes.len() {
        let mut out = Vec::new();
        for &callee in db.escape_callees(nodes[next]).iter() {
            let target = *index.entry(callee).or_insert_with(|| {
                nodes.push(callee);
                nodes.len() - 1
            });
            out.push(target);
        }
        edges.push(out);
        next += 1;
    }
    // The component of `func`: every node reachable from it (all of them)
    // that can reach it back.
    let mut reverse: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (from, out) in edges.iter().enumerate() {
        for &to in out {
            reverse[to].push(from);
        }
    }
    let mut reaches_func = vec![false; nodes.len()];
    let mut work = vec![0];
    reaches_func[0] = true;
    while let Some(v) = work.pop() {
        for &u in &reverse[v] {
            if !reaches_func[u] {
                reaches_func[u] = true;
                work.push(u);
            }
        }
    }
    let members: Vec<Function> = (0..nodes.len())
        .filter(|&v| reaches_func[v])
        .map(|v| nodes[v])
        .collect();

    // The members' summaries, from `NoEscape` up to the fixpoint. Callees
    // outside the component cannot reach `func`, so their own queries do
    // not cycle back here.
    let mut current: FxHashMap<Function, EscapeSummary> = FxHashMap::default();
    loop {
        let mut changed = false;
        for &member in &members {
            let snapshot = current.clone();
            let result = analyze(db, member, &mut |callee| {
                if members.contains(&callee) {
                    Some(
                        snapshot
                            .get(&callee)
                            .cloned()
                            .unwrap_or_else(|| EscapeSummary {
                                params: vec![ParamEscape::NoEscape; arity_of(db, callee)],
                                names: Vec::new(),
                            }),
                    )
                } else {
                    Some((*db.escape_summary(callee)).clone())
                }
            });
            if current.get(&member) != Some(&result.summary) {
                current.insert(member, result.summary);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Arc::new(current.remove(&func).unwrap_or_default())
}

/// The query behind [`HirDatabase::escape_analysis`].
pub(crate) fn escape_analysis_query(db: &dyn HirDatabase, func: Function) -> Arc<FunctionEscapes> {
    Arc::new(analyze(db, func, &mut |callee| {
        Some((*db.escape_summary(callee)).clone())
    }))
}

// ---- the analysis ----------------------------------------------------------

/// An abstract object: everything a value may refer to is one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Obj {
    /// The objects allocated by the `n`th allocation site of the body.
    Site(u32),
    /// The `n`th parameter's argument and everything reachable from it.
    Param(u32),
    /// Anything else: a heap object the analysis did not see allocated.
    Unknown,
}

type Objs = BTreeSet<Obj>;

/// Where a binding or an allocation site is: the loops around it (outermost
/// first; a closure compiled in place counts as a loop) and the innermost
/// region of separate code (closure body, `handle`, `spawn`) it is in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Context {
    loops: Vec<ExprId>,
    region: Option<ExprId>,
}

impl Context {
    /// Whether something at `inner` may hold an object allocated at
    /// `self` without the object outliving its loop iteration.
    fn may_hold(&self, inner: &Context) -> bool {
        inner.loops.starts_with(&self.loops)
    }
}

struct SiteInfo {
    expr: ExprId,
    kind: AllocKind,
    context: Context,
    /// Always on the heap, for this reason, whatever flows where.
    fixed: Option<Reason>,
}

/// The oracle for a callee's summary. `None` asks for the optimistic answer
/// (nothing escapes); it is used only to list callees.
type Oracle<'o> = dyn FnMut(Function) -> Option<EscapeSummary> + 'o;

fn analyze(db: &dyn HirDatabase, func: Function, oracle: &mut Oracle<'_>) -> FunctionEscapes {
    let frame = Frame::new(db, func);
    let mut names = Vec::new();
    let mut params = Vec::new();
    if let Some((pat, _)) = frame.body.self_param() {
        params.push(*pat);
        names.push("self".to_string());
    }
    for (pat, _) in frame.body.params() {
        names.push(binding_name(&frame.body, *pat));
        params.push(*pat);
    }
    let arity = params.len();
    if func.is_extern(db) {
        return FunctionEscapes {
            allocations: Vec::new(),
            summary: if func.is_intrinsic(db) {
                // Emitted in place. A scalar argument refers to nothing; any
                // other one is handed to code the analysis does not model
                // (an intrinsic may turn a reference into an address).
                let mut params: Vec<ParamEscape> = Vec::new();
                if let Some(sig) = func.ty(db).callable_sig(db) {
                    for ty in sig.params() {
                        params.push(match ty.interned() {
                            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Str => {
                                ParamEscape::NoEscape
                            }
                            _ => ParamEscape::Escapes(Reason::Unknown {
                                what: format!(
                                    "passed to the intrinsic `{}`, which the analysis does not \
                                     model",
                                    func.name(db)
                                ),
                            }),
                        });
                    }
                }
                params.resize(
                    arity,
                    ParamEscape::Escapes(Reason::Unknown {
                        what: format!("passed to the intrinsic `{}`", func.name(db)),
                    }),
                );
                EscapeSummary { params, names }
            } else {
                EscapeSummary {
                    names,
                    ..EscapeSummary::unknown(
                        arity,
                        &format!(
                            "passed to `{}`, an `extern` function, which the analysis cannot \
                             see into",
                            func.name(db)
                        ),
                    )
                }
            },
        };
    }

    let root = frame.body.body_expr();
    let mut inlined = FxHashSet::default();
    for (expr, e) in frame.body.exprs() {
        let Expr::MethodCall { args, .. } = e else {
            continue;
        };
        let index = match frame.infer.builtin_call(expr) {
            Some(BuiltinFn::Array(method)) => method.closure_arg(),
            Some(BuiltinFn::Pipeline(op)) => op.closure_arg(),
            _ => None,
        };
        if let Some(&arg) = index.and_then(|i| args.get(i)) {
            if crate::is_inlined_closure_literal(&frame.body, arg) {
                inlined.insert(arg);
            }
        }
    }

    let mut a = Analysis {
        db,
        func,
        frame,
        oracle,
        sites: Vec::new(),
        site_of: FxHashMap::default(),
        contents: Vec::new(),
        heap: Vec::new(),
        env: FxHashMap::default(),
        decl: FxHashMap::default(),
        params: vec![ParamEscape::NoEscape; arity],
        param_names: names.clone(),
        context: Context::default(),
        inlined,
        changed: false,
    };
    for (i, pat) in params.iter().enumerate() {
        a.env.insert(*pat, Objs::from([Obj::Param(i as u32)]));
        a.decl.insert(*pat, Context::default());
    }

    if matches!(a.frame.body[root], Expr::Missing) {
        // No body to analyse (a trait method without a default, or code
        // with errors): nothing is known about what a call does.
        let what = format!(
            "passed to `{}`, which has no body to analyse",
            func.name(db)
        );
        a.params = vec![ParamEscape::Escapes(Reason::Unknown { what }); arity];
    } else {
        loop {
            a.changed = false;
            a.context = Context::default();
            let value = a.eval(root);
            let from = func.name(db);
            a.returned(&value, Reason::Returned { from });
            a.propagate_heap();
            if !a.changed {
                break;
            }
        }
    }

    let allocations = a
        .sites
        .iter()
        .enumerate()
        .map(|(i, info)| Allocation {
            expr: info.expr,
            kind: info.kind.clone(),
            placement: match &a.heap[i] {
                Some(reason) => Placement::Heap(reason.clone()),
                None => Placement::Frame,
            },
            site: a.frame.site(info.expr),
        })
        .collect();
    FunctionEscapes {
        allocations,
        summary: EscapeSummary {
            params: a.params,
            names,
        },
    }
}

/// The number of parameters of `func`, `self` included.
fn arity_of(db: &dyn HirDatabase, func: Function) -> usize {
    let body = func.body(db);
    usize::from(body.self_param().is_some()) + body.params().len()
}

fn binding_name(body: &crate::Body, pat: PatId) -> String {
    match &body[pat] {
        Pat::Bind { name } => name.to_string(),
        _ => "_".to_string(),
    }
}

struct Analysis<'a, 'o> {
    db: &'a dyn HirDatabase,
    func: Function,
    frame: Frame,
    oracle: &'a mut Oracle<'o>,
    sites: Vec<SiteInfo>,
    site_of: FxHashMap<ExprId, u32>,
    /// What the fields and elements of each site's objects may hold.
    contents: Vec<Objs>,
    /// Why each site's objects are on the heap, once they are.
    heap: Vec<Option<Reason>>,
    /// What each local binding may hold.
    env: FxHashMap<PatId, Objs>,
    /// Where each local binding is declared.
    decl: FxHashMap<PatId, Context>,
    params: Vec<ParamEscape>,
    param_names: Vec<String>,
    context: Context,
    /// The closure literals that array methods and pipelines compile in
    /// place.
    inlined: FxHashSet<ExprId>,
    changed: bool,
}

/// The place an assignment writes.
enum Place {
    /// A local binding (or a field of a value struct or tuple held in it).
    Local(PatId),
    /// A field or element of the objects `Objs`, named `field` (an element
    /// for `None`).
    Fields(Objs, Option<Name>),
    /// Somewhere the analysis does not know.
    Unknown,
}

impl Analysis<'_, '_> {
    // ---- state updates ----

    fn site(&mut self, expr: ExprId, kind: AllocKind, fixed: Option<Reason>) -> Obj {
        if let Some(&index) = self.site_of.get(&expr) {
            return Obj::Site(index);
        }
        let index = self.sites.len() as u32;
        let fixed = fixed.or_else(|| {
            self.context.region.map(|region| Reason::InsideOpaque {
                what: self.region_what(region),
            })
        });
        self.sites.push(SiteInfo {
            expr,
            kind,
            context: self.context.clone(),
            fixed: fixed.clone(),
        });
        self.site_of.insert(expr, index);
        self.contents.push(Objs::new());
        self.heap.push(None);
        self.changed = true;
        if let Some(reason) = fixed {
            self.heap[index as usize] = Some(reason);
        }
        Obj::Site(index)
    }

    fn region_what(&self, region: ExprId) -> &'static str {
        match self.frame.body[region] {
            Expr::Handle { .. } => "a `handle` expression",
            Expr::Spawn { .. } => "a `spawn`",
            _ => "a closure body",
        }
    }

    /// Marks every object of `objs` (and, later, what they hold) as
    /// escaping.
    fn escape(&mut self, objs: &Objs, reason: Reason) {
        for obj in objs {
            match *obj {
                Obj::Site(s) => {
                    let slot = &mut self.heap[s as usize];
                    let replace = match slot {
                        None => true,
                        // An escape is the more useful explanation than a
                        // placement limit.
                        Some(old) => !old.is_escape() && reason.is_escape(),
                    };
                    if replace {
                        *slot = Some(reason.clone());
                        self.changed = true;
                    }
                }
                Obj::Param(p) => {
                    let slot = &mut self.params[p as usize];
                    if slot.rank() < 2 {
                        *slot = ParamEscape::Escapes(reason.clone());
                        self.changed = true;
                    }
                }
                Obj::Unknown => {}
            }
        }
    }

    /// Marks `objs` as returned from the function.
    fn returned(&mut self, objs: &Objs, reason: Reason) {
        for obj in objs {
            if let Obj::Param(p) = *obj {
                let slot = &mut self.params[p as usize];
                if slot.rank() < 1 {
                    *slot = ParamEscape::Returned;
                    self.changed = true;
                }
            }
        }
        let sites: Objs = objs
            .iter()
            .filter(|o| matches!(o, Obj::Site(_)))
            .copied()
            .collect();
        self.escape(&sites, reason);
    }

    /// Everything a heap site holds is on the heap too.
    fn propagate_heap(&mut self) {
        loop {
            let outer = std::mem::replace(&mut self.changed, false);
            for s in 0..self.sites.len() {
                if self.heap[s].is_none() || self.contents[s].is_empty() {
                    continue;
                }
                let held = self.contents[s].clone();
                let container = format!("an object of {}", self.sites[s].kind);
                self.escape(&held, Reason::InsideHeapObject { container });
            }
            let progressed = self.changed;
            self.changed |= outer;
            if !progressed {
                break;
            }
        }
    }

    /// Adds `objs` to the binding `pat`, checking that nothing allocated in
    /// a loop flows to a binding outside it.
    fn bind(&mut self, pat: PatId, objs: &Objs) {
        let decl = self.decl.get(&pat).cloned().unwrap_or_default();
        let name = format!("`{}`", binding_name(&self.frame.body, pat));
        self.outlives_check(objs, &decl, || name.clone());
        let entry = self.env.entry(pat).or_default();
        let before = entry.len();
        entry.extend(objs.iter().copied());
        if entry.len() != before {
            self.changed = true;
        }
    }

    fn outlives_check(&mut self, objs: &Objs, holder: &Context, into: impl Fn() -> String) {
        for obj in objs {
            if let Obj::Site(s) = *obj {
                if !self.sites[s as usize].context.may_hold(holder) {
                    let reason = Reason::OutlivesIteration { into: into() };
                    self.escape(&Objs::from([*obj]), reason);
                }
            }
        }
    }

    /// Stores `value` into a field or element of each object of `targets`.
    fn store(&mut self, targets: &Objs, value: &Objs, field: Option<Name>) {
        for target in targets {
            match *target {
                Obj::Site(t) => {
                    let holder = self.sites[t as usize].context.clone();
                    let kind = self.sites[t as usize].kind.clone();
                    self.outlives_check(value, &holder, || format!("an object of {kind}"));
                    let entry = &mut self.contents[t as usize];
                    let before = entry.len();
                    entry.extend(value.iter().copied());
                    if entry.len() != before {
                        self.changed = true;
                    }
                }
                Obj::Param(p) => {
                    let param = self.param_names[p as usize].clone();
                    self.escape(
                        value,
                        Reason::StoredInParam {
                            param,
                            field: field.clone(),
                        },
                    );
                }
                Obj::Unknown => self.escape(
                    value,
                    Reason::StoredInHeap {
                        field: field.clone(),
                    },
                ),
            }
        }
    }

    /// What the fields and elements of `objs` may hold.
    fn deref(&self, objs: &Objs) -> Objs {
        let mut out = Objs::from([Obj::Unknown]);
        for obj in objs {
            match *obj {
                Obj::Site(s) => out.extend(self.contents[s as usize].iter().copied()),
                Obj::Param(_) => {
                    out.insert(*obj);
                }
                Obj::Unknown => {}
            }
        }
        out
    }

    /// Everything reachable from `objs`, `objs` included.
    fn reach(&self, objs: &Objs) -> Objs {
        let mut out = objs.clone();
        let mut work: Vec<Obj> = objs.iter().copied().collect();
        while let Some(obj) = work.pop() {
            if let Obj::Site(s) = obj {
                for held in &self.contents[s as usize] {
                    if out.insert(*held) {
                        work.push(*held);
                    }
                }
            }
        }
        out
    }

    // ---- evaluation ----

    fn is_class(&self, expr: ExprId) -> bool {
        self.frame.infer[expr]
            .as_struct()
            .is_some_and(|s| s.data(self.db).memory_kind == StructMemoryKind::Gc)
    }

    fn local(&self, expr: ExprId) -> Option<PatId> {
        let Expr::Path(path) = &self.frame.body[expr] else {
            return None;
        };
        self.frame.resolve_local(self.db, expr, path)
    }

    /// Evaluates every child of `expr`, and lets everything they may hold
    /// escape for `reason`. The result holds only `Unknown`.
    fn opaque_children(&mut self, expr: ExprId, reason: Reason) -> Objs {
        let mut children = Vec::new();
        self.frame.body[expr].walk_child_exprs(|child| children.push(child));
        for child in children {
            let value = self.eval(child);
            let reachable = self.reach(&value);
            self.escape(&reachable, reason.clone());
        }
        Objs::from([Obj::Unknown])
    }

    fn eval_all(&mut self, exprs: &[ExprId]) -> Vec<Objs> {
        exprs.iter().map(|&e| self.eval(e)).collect()
    }

    fn union(values: impl IntoIterator<Item = Objs>) -> Objs {
        let mut out = Objs::new();
        for v in values {
            out.extend(v);
        }
        out
    }

    /// Runs `f` with `loop_expr` pushed as the innermost loop.
    fn in_loop<T>(&mut self, loop_expr: ExprId, f: impl FnOnce(&mut Self) -> T) -> T {
        self.context.loops.push(loop_expr);
        let out = f(self);
        self.context.loops.pop();
        out
    }

    /// Runs `f` inside the separate-code region `region`.
    fn in_region<T>(&mut self, region: ExprId, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = self.context.region.replace(region);
        let out = f(self);
        self.context.region = outer;
        out
    }

    fn declare(&mut self, pat: PatId, value: &Objs) {
        // Every binding of the pattern, with what it may hold: a
        // destructured class gives its fields.
        let mut work = vec![(pat, value.clone())];
        while let Some((pat, value)) = work.pop() {
            match &self.frame.body[pat] {
                Pat::Bind { .. } => {
                    let context = self.context.clone();
                    self.decl.entry(pat).or_insert(context);
                    self.bind(pat, &value);
                }
                other => {
                    let mut children = Vec::new();
                    other.walk_child_pats(|child| children.push(child));
                    if !children.is_empty() {
                        let mut inner = value.clone();
                        inner.extend(self.deref(&value));
                        for child in children {
                            work.push((child, inner.clone()));
                        }
                    }
                }
            }
        }
    }

    fn eval(&mut self, expr: ExprId) -> Objs {
        let value = self.eval_inner(expr);
        // A number, a `bool` or `()` refers to nothing, whatever it was
        // computed from.
        match self.frame.infer[expr].interned() {
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Never => Objs::new(),
            TyKind::Tuple(0, _) => Objs::new(),
            _ => value,
        }
    }

    fn eval_inner(&mut self, expr: ExprId) -> Objs {
        let body = Arc::clone(&self.frame.body);
        match &body[expr] {
            Expr::Missing | Expr::Literal(_) | Expr::Continue => Objs::new(),
            Expr::Path(path) => self.eval_path(expr, path),
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.eval(*condition);
                let mut out = self.eval(*then_branch);
                if let Some(e) = else_branch {
                    out.extend(self.eval(*e));
                }
                out
            }
            Expr::UnaryOp { expr: inner, .. } => {
                self.eval(*inner);
                Objs::new()
            }
            Expr::BinaryOp { lhs, rhs, op } => self.eval_binary(*lhs, *rhs, *op),
            Expr::Index { base, index } => {
                let base_value = self.eval(*base);
                if matches!(body[*index], Expr::Range { .. }) {
                    self.eval(*index);
                    // Slicing copies the elements into a new heap array.
                    let held = self.deref(&base_value);
                    self.escape(
                        &self.reach(&held),
                        Reason::InsideHeapObject {
                            container: "the array a slice builds".to_string(),
                        },
                    );
                    let site = self.site(
                        expr,
                        AllocKind::BuiltArray { by: "slice" },
                        Some(Reason::RuntimeLength),
                    );
                    return Objs::from([site, Obj::Unknown]);
                }
                self.eval(*index);
                self.deref(&base_value)
            }
            Expr::Block { statements, tail } => {
                for statement in statements {
                    match statement {
                        Statement::Let {
                            pat, initializer, ..
                        } => {
                            let value = initializer.map(|e| self.eval(e)).unwrap_or_default();
                            self.declare(*pat, &value);
                        }
                        Statement::Expr(e) => {
                            self.eval(*e);
                        }
                    }
                }
                tail.map(|t| self.eval(t)).unwrap_or_default()
            }
            Expr::Return { expr: value } => {
                if let Some(value) = value {
                    let objs = self.eval(*value);
                    if self.context.region.is_none() {
                        let from = self.func.name(self.db);
                        self.returned(&objs, Reason::Returned { from });
                    } else {
                        // Leaves the closure, whose results are unknown.
                        let reachable = self.reach(&objs);
                        self.escape(&reachable, Reason::CapturedByClosure);
                    }
                }
                Objs::new()
            }
            Expr::Break { expr: value } => {
                if let Some(value) = value {
                    let objs = self.eval(*value);
                    let reachable = self.reach(&objs);
                    self.escape(&reachable, Reason::LoopValue);
                }
                Objs::new()
            }
            Expr::Loop { body: inner } => {
                self.in_loop(expr, |this| this.eval(*inner));
                Objs::from([Obj::Unknown])
            }
            Expr::While {
                condition,
                body: inner,
            } => {
                self.in_loop(expr, |this| {
                    this.eval(*condition);
                    this.eval(*inner);
                });
                Objs::new()
            }
            Expr::For {
                pat,
                iterable,
                body: inner,
            } => {
                let source = self.eval(*iterable);
                let element = if let Some(next) = self.frame.infer.iterator_next(expr) {
                    self.call(expr, next, vec![source])
                } else {
                    self.deref(&source)
                };
                self.in_loop(expr, |this| {
                    this.declare(*pat, &element);
                    this.eval(*inner);
                });
                Objs::new()
            }
            Expr::Match {
                expr: scrutinee,
                arms,
                ..
            } => {
                let value = self.eval(*scrutinee);
                let mut out = Objs::new();
                for arm in arms {
                    self.declare(arm.pat, &value);
                    out.extend(self.eval(arm.expr));
                }
                out
            }
            Expr::Unwrap { expr: inner } => self.eval(*inner),
            Expr::Propagate { expr: inner } => {
                let value = self.eval(*inner);
                // `?` returns the error (or `nil`) early: as a return.
                if self.context.region.is_none() {
                    let from = self.func.name(self.db);
                    self.returned(&self.reach(&value), Reason::Returned { from });
                } else {
                    let reachable = self.reach(&value);
                    self.escape(&reachable, Reason::CapturedByClosure);
                }
                if let Some(conversion) = self.frame.infer.method_resolution(expr) {
                    self.call(expr, Function::from(conversion), vec![value.clone()]);
                }
                value
            }
            // Generated code formats the value by reading its fields; the
            // runtime only records a class instance's address while it is
            // being written (cycle detection).
            Expr::Format {
                value,
                width,
                precision,
                ..
            } => {
                self.eval(*value);
                for e in width.iter().chain(precision.iter()) {
                    self.eval(*e);
                }
                Objs::from([Obj::Unknown])
            }
            Expr::Range { start, end, .. } => {
                self.eval(*start);
                self.eval(*end);
                Objs::new()
            }
            Expr::RecordLit { fields, spread, .. } => {
                let mut values = Objs::new();
                let mut stores = Vec::new();
                for field in fields {
                    let value = self.eval(field.expr);
                    stores.push((field.name.clone(), value.clone()));
                    values.extend(value);
                }
                if let Some(spread) = spread {
                    let base = self.eval(*spread);
                    let held = if self.is_class(*spread) {
                        self.deref(&base)
                    } else {
                        base
                    };
                    stores.push((Name::missing(), held.clone()));
                    values.extend(held);
                }
                if self.is_class(expr) {
                    let name = self.frame.infer[expr]
                        .as_struct()
                        .map(|s| s.name(self.db))
                        .unwrap_or_else(Name::missing);
                    let site = self.site(expr, AllocKind::Class { name }, None);
                    let target = Objs::from([site]);
                    for (field, value) in stores {
                        self.store(&target, &value, Some(field));
                    }
                    target
                } else {
                    values
                }
            }
            Expr::Field { expr: base, .. } => {
                let value = self.eval(*base);
                if self.is_class(*base) {
                    self.deref(&value)
                } else {
                    value
                }
            }
            Expr::Array(elements) => {
                let values = self.eval_all(elements);
                let len = elements.len();
                let fixed = (len > MAX_FRAME_ARRAY_LEN).then_some(Reason::TooLong { len });
                let site = self.site(expr, AllocKind::ArrayLiteral { len }, fixed);
                let target = Objs::from([site]);
                for value in values {
                    self.store(&target, &value, None);
                }
                target
            }
            Expr::Tuple(elements) => Self::union(self.eval_all(elements)),
            Expr::Cast { expr: inner, .. } => self.eval(*inner),
            Expr::Closure {
                body: inner,
                params,
                ..
            } => {
                if self.inlined.contains(&expr) {
                    // Compiled in place by the array method or pipeline
                    // stage around it, once per element: a loop. Its
                    // parameters are bound by `inline_closure`.
                    self.in_loop(expr, |this| this.eval(*inner))
                } else {
                    let site = self.site(expr, AllocKind::Closure, Some(Reason::ClosureObject));
                    self.in_region(expr, |this| {
                        for (pat, _) in params {
                            this.declare(*pat, &Objs::from([Obj::Unknown]));
                        }
                        let value = this.eval(*inner);
                        let reachable = this.reach(&value);
                        this.escape(&reachable, Reason::CapturedByClosure);
                    });
                    Objs::from([site])
                }
            }
            Expr::Call { callee, args } => self.eval_call(expr, *callee, args),
            Expr::MethodCall { receiver, args, .. } => self.eval_method_call(expr, *receiver, args),
            Expr::Perform { args, .. } => {
                for &arg in args {
                    let value = self.eval(arg);
                    let reachable = self.reach(&value);
                    self.escape(&reachable, Reason::Performed);
                }
                Objs::from([Obj::Unknown])
            }
            Expr::Resume { value } => {
                if let Some(value) = value {
                    let objs = self.eval(*value);
                    let reachable = self.reach(&objs);
                    self.escape(&reachable, Reason::Performed);
                }
                Objs::new()
            }
            Expr::Handle { .. } => self.in_region(expr, |this| {
                this.opaque_children(expr, Reason::InsideHandle)
            }),
            Expr::Spawn { .. } => {
                self.in_region(expr, |this| this.opaque_children(expr, Reason::Spawned))
            }
            Expr::Scope { body: inner } => {
                self.eval(*inner);
                Objs::new()
            }
            Expr::ChannelSend { channel, value } => {
                self.eval(*channel);
                let objs = self.eval(*value);
                let reachable = self.reach(&objs);
                self.escape(&reachable, Reason::SentOnChannel);
                Objs::new()
            }
            Expr::ChannelReceive { channel } => {
                self.eval(*channel);
                Objs::from([Obj::Unknown])
            }
            Expr::Select { arms } => {
                for arm in arms {
                    let operands: Vec<ExprId> = arm.operands().collect();
                    for operand in operands {
                        let objs = self.eval(operand);
                        let reachable = self.reach(&objs);
                        self.escape(&reachable, Reason::SentOnChannel);
                    }
                    if let Some(binding) = arm.binding {
                        self.declare(binding, &Objs::from([Obj::Unknown]));
                    }
                    self.eval(arm.body);
                }
                Objs::from([Obj::Unknown])
            }
            Expr::Offer { .. } => self.opaque_children(
                expr,
                Reason::Unknown {
                    what: "used by `offer`, which the analysis does not model".to_string(),
                },
            ),
        }
    }

    fn eval_path(&mut self, expr: ExprId, path: &crate::Path) -> Objs {
        let resolver = resolver_for_expr(self.db, self.frame.owner, expr);
        match resolver.resolve_path_as_value_fully(self.db, path) {
            Some((ValueNs::LocalBinding(pat), _)) => {
                let value = self.env.get(&pat).cloned().unwrap_or_default();
                // Mentioned inside separate code it is not declared in: the
                // separate code keeps it.
                if let Some(region) = self.context.region {
                    let declared_in = self.decl.get(&pat).and_then(|c| c.region);
                    if declared_in != Some(region) {
                        let reason = match self.frame.body[region] {
                            Expr::Handle { .. } => Reason::InsideHandle,
                            Expr::Spawn { .. } => Reason::Spawned,
                            _ => Reason::CapturedByClosure,
                        };
                        let reachable = self.reach(&value);
                        self.escape(&reachable, reason);
                    }
                }
                value
            }
            Some((ValueNs::StructId(_) | ValueNs::ImplSelf(_), _)) if self.is_class(expr) => {
                let name = self.frame.infer[expr]
                    .as_struct()
                    .map(|s| s.name(self.db))
                    .unwrap_or_else(Name::missing);
                Objs::from([self.site(expr, AllocKind::Class { name }, None)])
            }
            _ => Objs::from([Obj::Unknown]),
        }
    }

    fn eval_binary(&mut self, lhs: ExprId, rhs: ExprId, op: Option<BinaryOp>) -> Objs {
        match op {
            Some(BinaryOp::Assignment { op: None }) => {
                let value = self.eval(rhs);
                let place = self.place(lhs);
                match place {
                    Place::Local(pat) => self.bind(pat, &value),
                    Place::Fields(targets, field) => self.store(&targets, &value, field),
                    Place::Unknown => self.escape(&value, Reason::StoredInHeap { field: None }),
                }
                Objs::new()
            }
            _ => {
                // Arithmetic, comparison (of class references: by
                // address) and compound assignment of numbers and strings.
                self.eval(lhs);
                self.eval(rhs);
                Objs::new()
            }
        }
    }

    /// The place an assignment's left-hand side names, evaluating what it
    /// reads on the way.
    fn place(&mut self, lhs: ExprId) -> Place {
        let body = Arc::clone(&self.frame.body);
        match &body[lhs] {
            Expr::Path(_) => match self.local(lhs) {
                Some(pat) => {
                    if let Some(region) = self.context.region {
                        if self.decl.get(&pat).and_then(|c| c.region) != Some(region) {
                            return Place::Unknown;
                        }
                    }
                    Place::Local(pat)
                }
                None => Place::Unknown,
            },
            Expr::Field { expr: base, name } => {
                if self.is_class(*base) {
                    let objs = self.eval(*base);
                    Place::Fields(objs, Some(name.clone()))
                } else {
                    // A field of a value struct or tuple: storage of
                    // whatever holds the aggregate.
                    match self.place(*base) {
                        Place::Fields(objs, _) => Place::Fields(objs, Some(name.clone())),
                        other => other,
                    }
                }
            }
            Expr::Index { base, index } => {
                let objs = self.eval(*base);
                self.eval(*index);
                Place::Fields(objs, None)
            }
            _ => {
                self.eval(lhs);
                Place::Unknown
            }
        }
    }

    fn eval_call(&mut self, expr: ExprId, callee: ExprId, args: &[ExprId]) -> Objs {
        if let Some(builtin) = self.frame.infer.builtin_call(expr) {
            return self.builtin(expr, builtin, None, args);
        }
        if self.frame.infer.is_indirect_call(expr) {
            return self.unknown_call(
                expr,
                "passed to a call through a function value, which the analysis cannot see",
            );
        }
        if self.frame.infer.variant_of_expr(expr).is_some() {
            // `Shape.Circle(..)`: an enum value holding its arguments.
            return Self::union(self.eval_all(args));
        }
        match self.frame.infer[callee].as_callable_def() {
            Some(CallableDef::Function(f)) => {
                let values = self.eval_all(args);
                self.call(expr, f, values)
            }
            Some(CallableDef::Struct(s)) => {
                let values = self.eval_all(args);
                if s.data(self.db).memory_kind == StructMemoryKind::Gc {
                    let site = self.site(
                        expr,
                        AllocKind::Class {
                            name: s.name(self.db),
                        },
                        None,
                    );
                    let target = Objs::from([site]);
                    for value in values {
                        self.store(&target, &value, None);
                    }
                    target
                } else {
                    Self::union(values)
                }
            }
            None => self.unknown_call(expr, "passed to a call the analysis cannot resolve"),
        }
    }

    fn eval_method_call(&mut self, expr: ExprId, receiver: ExprId, args: &[ExprId]) -> Objs {
        if let Some(builtin) = self.frame.infer.builtin_call(expr) {
            return self.builtin(expr, builtin, Some(receiver), args);
        }
        if self.frame.infer.is_indirect_call(expr) {
            return self.unknown_call(
                expr,
                "passed to a call through a function value, which the analysis cannot see",
            );
        }
        if self.frame.infer.variant_of_expr(expr).is_some() {
            return Self::union(self.eval_all(args));
        }
        if crate::expr::opened_session(self.db, &self.frame.body, expr).is_some()
            || matches!(self.frame.infer[receiver].interned(), TyKind::Session(..))
        {
            return self.unknown_call(
                expr,
                "used by a session operation, which the analysis does not model",
            );
        }
        match self.frame.infer.method_resolution(expr) {
            Some(f) => {
                let f = Function::from(f);
                // A trait method on a type parameter runs a different
                // implementation per instantiation; the one resolution
                // recorded is only the declaration.
                let generic_trait = f.trait_container(self.db).is_some()
                    && self
                        .frame
                        .infer
                        .instantiation(expr)
                        .and_then(|args| args.first())
                        .is_none_or(crate::mono::contains_type_param);
                if generic_trait {
                    return self.unknown_call(
                        expr,
                        "passed to a trait method on a generic type, whose implementation is \
                         chosen per instantiation",
                    );
                }
                let mut values = vec![self.eval(receiver)];
                values.extend(self.eval_all(args));
                self.call(expr, f, values)
            }
            None => self.unknown_call(expr, "passed to a method call the analysis cannot resolve"),
        }
    }

    /// Every operand of `expr` escapes; the result is unknown.
    fn unknown_call(&mut self, expr: ExprId, what: &str) -> Objs {
        self.opaque_children(
            expr,
            Reason::Unknown {
                what: what.to_string(),
            },
        )
    }

    /// A call to the named function `callee` with the arguments' values.
    fn call(&mut self, _expr: ExprId, callee: Function, args: Vec<Objs>) -> Objs {
        let summary = if callee.module(self.db) != self.func.module(self.db)
            && !callee.is_intrinsic(self.db)
        {
            // A hot reload can replace the callee's module without
            // recompiling this one.
            Some(EscapeSummary::unknown(
                args.len(),
                &format!(
                    "passed to `{}` in another module, whose code a hot reload can replace",
                    callee.name(self.db)
                ),
            ))
        } else {
            (self.oracle)(callee)
        };
        let Some(summary) = summary else {
            // Listing callees only.
            return Self::union(args)
                .into_iter()
                .chain([Obj::Unknown])
                .collect();
        };
        let mut result = Objs::from([Obj::Unknown]);
        let arity_matches = summary.params.len() == args.len();
        for (i, value) in args.into_iter().enumerate() {
            let reachable = self.reach(&value);
            let effect = if arity_matches {
                summary.param(i).clone()
            } else {
                ParamEscape::Escapes(Reason::Unknown {
                    what: format!(
                        "passed to `{}`, whose parameters the analysis cannot match",
                        callee.name(self.db)
                    ),
                })
            };
            match effect {
                ParamEscape::NoEscape => {}
                ParamEscape::Returned => result.extend(reachable),
                ParamEscape::Escapes(why) => {
                    let why = match why {
                        // Already says where it went.
                        unknown @ Reason::Unknown { .. } => unknown,
                        why => Reason::PassedTo {
                            callee: callee.name(self.db),
                            param: summary
                                .names
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| format!("#{i}")),
                            why: Box::new(why),
                        },
                    };
                    self.escape(&reachable, why);
                }
            }
        }
        result
    }

    fn builtin(
        &mut self,
        expr: ExprId,
        builtin: BuiltinFn,
        receiver: Option<ExprId>,
        args: &[ExprId],
    ) -> Objs {
        match builtin {
            // Values in, a value out, no storage.
            BuiltinFn::CtSelect | BuiltinFn::Select | BuiltinFn::Likely | BuiltinFn::Unlikely => {
                let mut values = receiver.map(|r| self.eval(r)).unwrap_or_default();
                for v in self.eval_all(args) {
                    values.extend(v);
                }
                values
            }
            BuiltinFn::Fma | BuiltinFn::ArrayLen | BuiltinFn::StrLen | BuiltinFn::Declassify => {
                let mut values = receiver.map(|r| self.eval(r)).unwrap_or_default();
                for v in self.eval_all(args) {
                    values.extend(v);
                }
                if builtin == BuiltinFn::Declassify {
                    values
                } else {
                    Objs::new()
                }
            }
            BuiltinFn::ArrayUncheckedGet => {
                let base = receiver.map(|r| self.eval(r)).unwrap_or_default();
                self.eval_all(args);
                self.deref(&base)
            }
            BuiltinFn::ArrayFilled => {
                self.eval_all(args);
                let site = self.site(
                    expr,
                    AllocKind::BuiltArray { by: "array_filled" },
                    Some(Reason::RuntimeLength),
                );
                Objs::from([site])
            }
            BuiltinFn::Array(method) => self.array_method(expr, method, receiver, args),
            BuiltinFn::Pipeline(op) => {
                // A pipeline's elements pass through generated loops, are
                // carried between iterations (`fold`) and may be stored into
                // a new heap array (`collect`): every element of the sources
                // and everything a stage's closure produces escapes. The
                // pipeline value itself refers to its sources.
                let mut sources = receiver.map(|r| self.eval(r)).unwrap_or_default();
                let closure = op.closure_arg().and_then(|i| args.get(i)).copied();
                let mut produced = Objs::new();
                for &arg in args {
                    if Some(arg) == closure && self.inlined.contains(&arg) {
                        produced.extend(self.inline_closure(arg, &Objs::from([Obj::Unknown])));
                    } else {
                        let value = self.eval(arg);
                        sources.extend(value);
                    }
                }
                let mut escaping = self.reach(&self.deref(&sources));
                escaping.extend(self.reach(&produced));
                self.escape(
                    &escaping,
                    Reason::Unknown {
                        what: format!(
                            "passed through the pipeline stage `{}`, which the analysis \
                             does not follow",
                            op.name()
                        ),
                    },
                );
                let mut values = sources;
                if op.allocates() {
                    let site = self.site(
                        expr,
                        AllocKind::BuiltArray { by: "collect" },
                        Some(Reason::RuntimeLength),
                    );
                    values.insert(site);
                }
                values.insert(Obj::Unknown);
                values
            }
            BuiltinFn::Print | BuiltinFn::Println | BuiltinFn::Channel | BuiltinFn::String(_) => {
                self.unknown_call(
                    expr,
                    &format!(
                        "passed to the runtime by `{}`",
                        match builtin {
                            BuiltinFn::String(op) => op.name(),
                            other => other.name(),
                        }
                    ),
                )
            }
        }
    }

    /// Binds an inlined closure's parameters to `element` and evaluates its
    /// body as a loop. Its result is what the method does with it.
    fn inline_closure(&mut self, closure: ExprId, element: &Objs) -> Objs {
        let body = Arc::clone(&self.frame.body);
        let Expr::Closure {
            params,
            body: inner,
            ..
        } = &body[closure]
        else {
            return self.eval(closure);
        };
        self.in_loop(closure, |this| {
            for (pat, _) in params {
                this.declare(*pat, element);
            }
            this.eval(*inner)
        })
    }

    fn array_method(
        &mut self,
        expr: ExprId,
        method: ArrayMethod,
        receiver: Option<ExprId>,
        args: &[ExprId],
    ) -> Objs {
        let base = receiver.map(|r| self.eval(r)).unwrap_or_default();
        let elements = self.deref(&base);
        let closure = method.closure_arg().and_then(|i| args.get(i)).copied();
        let mut arg_values = Vec::new();
        for &arg in args {
            let value = if Some(arg) == closure && self.inlined.contains(&arg) {
                // `fold`'s closure takes the accumulator too.
                let mut bound = elements.clone();
                for v in &arg_values {
                    bound.extend(Objs::clone(v));
                }
                let result = self.inline_closure(arg, &bound);
                let reachable = self.reach(&result);
                if method == ArrayMethod::Fold {
                    // The result is the next iteration's accumulator: it
                    // lives across iterations of the closure, whose frame
                    // slots are reused each time.
                    self.escape(
                        &reachable,
                        Reason::OutlivesIteration {
                            into: "`fold`'s accumulator".to_string(),
                        },
                    );
                    result.union(&reachable).copied().collect()
                } else {
                    // `map` stores it into the new array; the predicates
                    // return `bool`.
                    self.escape(
                        &reachable,
                        Reason::InsideHeapObject {
                            container: format!("the array `{}` builds", method.name()),
                        },
                    );
                    Objs::new()
                }
            } else {
                self.eval(arg)
            };
            arg_values.push(value);
        }
        if closure.is_some_and(|c| !self.inlined.contains(&c)) {
            // A function value passed instead of a literal: called through.
            let all = Self::union(arg_values.iter().cloned())
                .union(&self.reach(&base))
                .copied()
                .collect();
            self.escape(
                &all,
                Reason::Unknown {
                    what: format!("passed to a function value that `{}` calls", method.name()),
                },
            );
            return Objs::from([Obj::Unknown]);
        }
        match inline_array_method(method) {
            Some(Inline::Read) => {
                // Reads elements, or writes the length or an element in
                // place; may return an element.
                let mut out = elements;
                for v in arg_values {
                    out.extend(v);
                }
                out
            }
            Some(Inline::Builds) => {
                // `copy`, `map`, `filter`, `enumerate`: a new heap array
                // holding the elements.
                let reachable = self.reach(&elements);
                self.escape(
                    &reachable,
                    Reason::InsideHeapObject {
                        container: format!("the array `{}` builds", method.name()),
                    },
                );
                let site = self.site(
                    expr,
                    AllocKind::BuiltArray { by: method.name() },
                    Some(Reason::RuntimeLength),
                );
                Objs::from([site, Obj::Unknown])
            }
            None => {
                // Grows the buffer in the runtime: the array must be a heap
                // object, and what it stores goes into it.
                let reason = Reason::Unknown {
                    what: format!(
                        "grown by `{}`, which the runtime does on the heap",
                        method.name()
                    ),
                };
                self.escape(&base, reason);
                for v in &arg_values {
                    self.store(&base, v, None);
                }
                Objs::from([Obj::Unknown])
            }
        }
    }
}

/// How an array method is compiled.
pub enum Inline {
    /// In place, on the array's own payload (the runtime gets at most the
    /// element pointer and the length, as for `sort` and `reverse`).
    Read,
    /// In place, building a new heap array from the elements.
    Builds,
}

/// Which array methods generated code performs without handing the array
/// itself to the runtime. `None` for the ones that may grow it.
pub fn inline_array_method(method: ArrayMethod) -> Option<Inline> {
    use ArrayMethod::*;
    match method {
        Push | Insert | Reserve | Append => None,
        Copy | Map | Filter | Enumerate => Some(Inline::Builds),
        _ => Some(Inline::Read),
    }
}

// ---- the allocation report (`codira build --explain-alloc`) -----------------

/// One line of the allocation report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportEntry {
    pub file_id: codira_hir_input::FileId,
    /// Where the allocating expression starts.
    pub offset: codira_syntax::TextSize,
    pub function: String,
    pub kind: AllocKind,
    pub placement: Placement,
}

/// Every allocation of `functions`, in source order.
pub fn allocation_report(db: &dyn HirDatabase, functions: &[Function]) -> Vec<ReportEntry> {
    let mut entries = Vec::new();
    for &func in functions {
        if func.is_extern(db) {
            continue;
        }
        let analysis = db.escape_analysis(func);
        for allocation in &analysis.allocations {
            let Some(site) = &allocation.site else {
                continue;
            };
            entries.push(ReportEntry {
                file_id: site.file_id,
                offset: site.value.range().start(),
                function: func.name(db).to_string(),
                kind: allocation.kind.clone(),
                placement: allocation.placement.clone(),
            });
        }
    }
    entries.sort_by_key(|e| (e.file_id, e.offset));
    entries
}

/// The report as text: one line per allocation, `path:line:column: in `f`:
/// kind: placement`, then a total. `frame_allocation` says whether the
/// build places objects in frames; without it an object that could be is
/// reported as such. `path` names a file.
pub fn format_allocation_report(
    db: &dyn HirDatabase,
    entries: &[ReportEntry],
    frame_allocation: bool,
    path: impl Fn(codira_hir_input::FileId) -> String,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let (mut frame, mut escapes, mut kept) = (0, 0, 0);
    for entry in entries {
        let position = db.line_index(entry.file_id).line_col(entry.offset);
        let verdict = match &entry.placement {
            Placement::Frame => {
                frame += 1;
                if frame_allocation {
                    entry.placement.to_string()
                } else {
                    "does not escape (moved to the stack with `--frame-alloc`)".to_string()
                }
            }
            Placement::Heap(reason) => {
                if reason.is_escape() {
                    escapes += 1;
                } else {
                    kept += 1;
                }
                entry.placement.to_string()
            }
        };
        let _ = writeln!(
            out,
            "{}:{}:{}: in `{}`: {}: {verdict}",
            path(entry.file_id),
            position.line + 1,
            position.col_utf16 + 1,
            entry.function,
            entry.kind,
        );
    }
    let _ = writeln!(
        out,
        "{} allocation sites: {frame} {}, {escapes} escape, {kept} stay on the heap for another \
         reason",
        entries.len(),
        if frame_allocation {
            "moved to the stack"
        } else {
            "could move to the stack"
        },
    );
    out
}
