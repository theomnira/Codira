//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Tests of the effect analysis and of `@noalloc`, `@nopanic`, `@noblock`,
//!   `@realtime` and `@deadline`: source text in, summaries and diagnostics
//!   out.

use codira_hir_input::WithFixture;

use super::{Effect, EffectSet};
use crate::{
    diagnostics::DiagnosticSink, mock::MockDatabase, utils::tests::diagnostics, AstDatabase,
    HirDatabase, ModuleDef, Package,
};

/// Every diagnostic of a multi-file fixture, one per line.
fn diagnostics_of_files(fixture: &str) -> String {
    let db = MockDatabase::with_files(fixture);
    let mut diags = Vec::new();
    for module in Package::all(&db).iter().flat_map(|pkg| pkg.modules(&db)) {
        if let Some(file_id) = module.file_id(&db) {
            for err in db.parse(file_id).errors() {
                diags.push(format!("{:?}: {err}", err.location()));
            }
        }
    }
    let mut sink = DiagnosticSink::new(|diag| {
        diags.push(format!("{:?}: {}", diag.highlight_range(), diag.message()));
    });
    for module in Package::all(&db).iter().flat_map(|pkg| pkg.modules(&db)) {
        module.diagnostics(&db, &mut sink);
    }
    drop(sink);
    diags.join("\n")
}

/// The effects of the function named `name`, through everything it calls.
fn effects_of(source: &str, name: &str) -> EffectSet {
    let (db, _) = MockDatabase::with_single_file(source);
    let func = Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.declarations(&db))
        .find_map(|def| match def {
            ModuleDef::Function(f) if f.name(&db).to_string() == name => Some(f),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}` in fixture"));
    db.effect_summary(func).effects
}

fn set(effects: &[Effect]) -> EffectSet {
    EffectSet::of(effects)
}

#[test]
fn noalloc_rejects_every_allocation_source() {
    let diags = diagnostics(
        r#"
    class Particle {
        x: f64
    }
    struct Point {
        x: f64,
        y: f64,
    }
    @noalloc
    func record() -> Particle { Particle { x: 1.0 } }
    @noalloc
    func array() -> usize { let xs = [1, 2, 3]; xs.len() }
    @noalloc
    func printing() { println("hi") }
    @noalloc
    func tasks() {
        scope {
            let ch = channel(4)
        }
    }
    @noalloc
    func fine(p: Point, xs: [i64]) -> f64 {
        let q = Point { x: p.y, y: p.x }
        let t = (q.x, 2)
        let s = "a constant"
        t.0 + xs.unchecked_get(0) as f64 + s.len() as f64
    }
    "#,
    );
    let count = |needle: &str| diags.matches(needle).count();
    assert_eq!(
        count("`record` is `@noalloc` but allocates here: class `Particle` constructed"),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`array` is `@noalloc` but allocates here: array literal allocated"),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`printing` is `@noalloc` but allocates here: `println` formats"),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`tasks` is `@noalloc` but allocates here: `scope`"),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`tasks` is `@noalloc` but allocates here: `channel(..)`"),
        1,
        "{diags}"
    );
    assert_eq!(count("`fine`"), 0, "{diags}");
    assert_eq!(diags.lines().count(), 5, "{diags}");
}

#[test]
fn nopanic_rejects_indexing_unproven_division_and_runtime_checks() {
    let diags = diagnostics(
        r#"
    @nopanic
    func index(xs: [i64], i: usize) -> i64 { xs[i] }
    @nopanic
    func store(xs: [i64]) { xs[0] = 1 }
    @nopanic
    func unchecked(xs: [i64], i: usize) -> i64 { xs.unchecked_get(i) }
    @nopanic
    func divide(a: i64, b: i64) -> i64 { a / b }
    @nopanic
    func compound(a: u32, b: u32) -> u32 { var x = a; x %= b; x }
    @nopanic
    func by_literal(a: i64) -> i64 { a / 4 + a % 7 }
    @nopanic
    func floats(a: f64, b: f64) -> f64 { a / b }

    @runtime_checked
    func checked(x: i64) -> i64
        requires x > 0
    { x }
    @nopanic
    func calls_checked() -> i64 { checked(3) }
    "#,
    );
    let count = |needle: &str| diags.matches(needle).count();
    assert_eq!(
        count("`index` is `@nopanic` but may panic here: indexing panics"),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`store` is `@nopanic` but may panic here: indexing panics"),
        1,
        "{diags}"
    );
    assert_eq!(
        count(
            "`divide` is `@nopanic` but may panic here: integer `/` whose divisor is not proven \
             non-zero and free of `MIN / -1` overflow"
        ),
        1,
        "{diags}"
    );
    assert_eq!(
        count(
            "`compound` is `@nopanic` but may panic here: integer `%` whose divisor is not \
             proven non-zero:"
        ),
        1,
        "{diags}"
    );
    assert_eq!(
        count(
            "`calls_checked` is `@nopanic` but calls `checked` (line 21), which may panic: \
             `checked` is `@runtime_checked`"
        ),
        1,
        "{diags}"
    );
    for fine in ["`unchecked`", "`by_literal`", "`floats`"] {
        assert_eq!(count(fine), 0, "{diags}");
    }
    assert_eq!(diags.lines().count(), 5, "{diags}");
}

#[test]
fn a_division_the_verifier_proves_safe_does_not_panic() {
    let diags = diagnostics(
        r#"
    @nopanic
    func proven(a: i64, b: i64) -> i64
        requires b > 0
    {
        a / b
    }
    @nopanic
    func refuted(a: i64, b: i64) -> i64
        requires b >= 0
    {
        a / b
    }
    "#,
    );
    // The verifier reports the refuted division itself, and the attribute
    // check reports that the function may panic because of it.
    assert!(diags.contains("divisor `b` might be zero"), "{diags}");
    assert!(
        diags.contains("`refuted` is `@nopanic` but may panic here: integer `/`"),
        "{diags}"
    );
    assert!(!diags.contains("`proven`"), "{diags}");
}

#[test]
fn noblock_rejects_channels_scopes_and_printing() {
    let diags = diagnostics(
        r#"
    func worker(jobs: Channel[i32]) {
        let x = <-jobs
    }
    @noblock
    func sends(ch: Channel[i32]) { ch <- 1 }
    @noblock
    func receives(ch: Channel[i32]) -> i32 { <-ch }
    @noblock
    func waits() { scope { } }
    @noblock
    func indirect(ch: Channel[i32]) { worker(ch) }
    @noblock
    func fine(a: i64) -> i64 { a * 2 }
    "#,
    );
    let count = |needle: &str| diags.matches(needle).count();
    assert_eq!(
        count("`sends` is `@noblock` but may block here: channel send"),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`receives` is `@noblock` but may block here: channel receive"),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`waits` is `@noblock` but may block here: `scope`"),
        1,
        "{diags}"
    );
    assert_eq!(
        count(
            "`indirect` is `@noblock` but calls `worker` (line 11), which may block: channel \
             receive waits while the channel is empty at line 2"
        ),
        1,
        "{diags}"
    );
    assert_eq!(count("`fine`"), 0, "{diags}");
}

/// `select` (even with `default`, which still locks its channels) and every
/// session operation may block; `select` and `open` also allocate.
#[test]
fn select_and_sessions_block() {
    let diags = diagnostics(
        r#"
    session Ping { send i64; recv i64 }
    @noblock
    func polls(ch: Channel[i32]) -> i32 { select { v = <-ch -> v, default -> 0 } }
    @noalloc
    func waits(ch: Channel[i32]) -> i32 { select { v = <-ch -> v } }
    @noblock
    func pings(ep: Ping) { ep <- 1; let x = <-ep; }
    "#,
    );
    let count = |needle: &str| diags.matches(needle).count();
    assert_eq!(
        count("`polls` is `@noblock` but may block here: `select` locks its channels"),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`waits` is `@noalloc` but allocates here: `select` waits"),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`pings` is `@noblock` but may block here"),
        2,
        "{diags}"
    );
    let opened = effects_of(
        "session P { send i64 }\n\
         func peer(ep: dual P) { let x = <-ep; }\n\
         func f() { scope { let (a, b) = P.open(); spawn peer(b); a <- 1; } }",
        "f",
    );
    let wanted = set(&[Effect::Alloc, Effect::Block]);
    assert_eq!(
        opened.intersection(wanted),
        wanted,
        "opening a session allocates and blocks"
    );
}

#[test]
fn the_diagnostic_shows_the_chain_of_calls() {
    let diags = diagnostics(
        r#"
    class Particle {
        x: f64
    }

    @noalloc
    func tick(n: i64) -> f64 {
        step(n)
    }

    func step(n: i64) -> f64 {
        let p = make(n)
        p.x
    }

    func make(n: i64) -> Particle {
        Particle { x: n as f64 }
    }
    "#,
    );
    assert_eq!(
        diags.lines().count(),
        1,
        "only the annotated function is checked: {diags}"
    );
    assert!(
        diags.contains(
            "`tick` is `@noalloc` but calls `step` (line 7), which calls `make` (line 11), \
             which allocates: class `Particle` constructed at line 16"
        ),
        "{diags}"
    );
}

#[test]
fn chains_cross_modules_and_cross_module_calls_box_value_structs() {
    let diags = diagnostics_of_files(
        r#"
    //- /mod.code
    import physics.{ advance, Vec2, scale }

    @noalloc
    public func tick(n: i64) -> i64 {
        advance(n)
    }

    @noalloc
    public func boxes(v: Vec2) -> f64 {
        scale(v)
    }
    //- /physics.code
    public class Body {
        x: i64
    }
    public struct Vec2 {
        x: f64,
        y: f64,
    }
    public func advance(n: i64) -> i64 {
        let b = Body { x: n }
        b.x
    }
    public func scale(v: Vec2) -> f64 {
        v.x * 2.0
    }
    "#,
    );
    assert!(
        diags.contains(
            "`tick` is `@noalloc` but calls `advance` (line 5), which allocates: class `Body` \
             constructed at physics.code:9"
        ),
        "{diags}"
    );
    assert!(
        diags.contains(
            "`boxes` is `@noalloc` but allocates here: the call to `scale` in another module \
             goes through the runtime's dispatch table, which boxes the struct `Vec2` on the \
             GC heap"
        ),
        "{diags}"
    );
    assert_eq!(diags.lines().count(), 2, "{diags}");
}

#[test]
fn externs_are_unknown_unless_their_declaration_asserts() {
    let diags = diagnostics(
        r#"
    extern func c_unknown(x: i64) -> i64;

    @noalloc @nopanic
    extern func c_pure(x: i64) -> i64;

    extern "C" {
        @realtime
        func c_rt(x: i64) -> i64;
    }

    @noalloc
    func calls_unknown(x: i64) -> i64 { c_unknown(x) }
    @noalloc @nopanic
    func calls_asserted(x: i64) -> i64 { c_pure(x) }
    @noblock
    func calls_asserted_but_blocking(x: i64) -> i64 { c_pure(x) }
    @realtime
    func calls_realtime(x: i64) -> i64 { c_rt(x) }
    "#,
    );
    let count = |needle: &str| diags.matches(needle).count();
    assert_eq!(
        count(
            "`calls_unknown` is `@noalloc` but calls `c_unknown` (line 12), which allocates: \
             `c_unknown` is an `extern` function, which the analysis cannot see into"
        ),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`calls_asserted_but_blocking` is `@noblock` but calls `c_pure` (line 16), which may block"),
        1,
        "{diags}"
    );
    assert_eq!(count("`calls_asserted`"), 0, "{diags}");
    assert_eq!(count("`calls_realtime`"), 0, "{diags}");
    assert_eq!(diags.lines().count(), 2, "{diags}");
}

#[test]
fn realtime_needs_proven_measures_and_no_recursion() {
    let diags = diagnostics(
        r#"
    @realtime
    func bounded(n: i64) -> i64
        requires n >= 0
    {
        var i = 0
        var s = 0
        while i < n
            invariant i >= 0 && i <= n
            decreases n - i
        {
            i += 1
            s += 2
        }
        s
    }
    @realtime
    func counted(n: i64) -> i64 {
        var s = 0
        for i in 0..n { s += i }
        s
    }
    @realtime
    func no_measure(n: i64) -> i64 {
        var i = 0
        while i < n { i += 1 }
        i
    }
    @realtime
    func wrong_measure(n: i64) -> i64
        requires n >= 0
    {
        var i = 0
        while i < n
            invariant i >= 0
            decreases i
        {
            i += 1
        }
        i
    }
    @realtime
    func fact(n: i64) -> i64 {
        if n <= 1 { 1 } else { n * fact(n - 1) }
    }
    @realtime
    func ping(n: i64) -> i64 { if n <= 0 { 0 } else { pong(n - 1) } }
    func pong(n: i64) -> i64 { ping(n) }
    @noalloc
    func recursion_allowed(n: i64) -> i64 {
        if n <= 1 { 1 } else { n * recursion_allowed(n - 1) }
    }
    "#,
    );
    let count = |needle: &str| diags.matches(needle).count();
    assert_eq!(count("`bounded`"), 0, "{diags}");
    assert_eq!(count("`counted`"), 0, "{diags}");
    assert_eq!(
        count("`no_measure` is `@realtime` but may not terminate here: loop without a `decreases` measure"),
        1,
        "{diags}"
    );
    assert_eq!(
        count(
            "`wrong_measure` is `@realtime` but may not terminate here: loop whose `decreases` \
             measure is not proven"
        ),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`fact` is `@realtime` but may not terminate here: `fact` is recursive: it calls itself"),
        1,
        "{diags}"
    );
    assert_eq!(
        count(
            "`ping` is `@realtime` but may not terminate here: `ping` is recursive: it calls \
             `pong`, which can call `ping` again"
        ),
        1,
        "{diags}"
    );
    assert_eq!(count("`recursion_allowed`"), 0, "{diags}");
}

#[test]
fn realtime_uses_proofs_through_for_and_continue() {
    let diags = diagnostics(
        r#"
    @realtime
    func skips(n: i64) -> i64
        requires n >= 0
    {
        var i = 0
        while i < n
            invariant i >= 0
            decreases n - i
        {
            if i == 3 { continue }
            i += 1
        }
        i
    }
    @realtime
    func ranged(n: i64) -> i64
        requires n > 0
    {
        var s = 0
        for i in 0..n { s += 100 / (i + 1) }
        s
    }
    "#,
    );
    // `continue` at `i == 3` skips the decrement, so the measure is refuted;
    // the division by `i + 1` inside the `for` is proven from the range.
    assert!(
        diags.contains(
            "`skips` is `@realtime` but may not terminate here: loop whose `decreases` measure is not proven"
        ),
        "{diags}"
    );
    assert!(!diags.contains("`ranged`"), "{diags}");
}

#[test]
fn realtime_reports_every_effect_through_a_chain() {
    let diags = diagnostics(
        r#"
    class Node {
        v: i64
    }
    func helper(xs: [i64]) -> i64 {
        let n = Node { v: 1 }
        xs[0] + n.v
    }
    @realtime
    func tick(xs: [i64]) -> i64 { helper(xs) }
    "#,
    );
    assert!(
        diags.contains(
            "`tick` is `@realtime` but calls `helper` (line 9), which allocates, may block and \
             may panic: class `Node` constructed at line 5"
        ),
        "{diags}"
    );
    assert_eq!(diags.lines().count(), 1, "{diags}");
}

#[test]
fn perform_and_unknown_calls_count_as_every_effect() {
    let diags = diagnostics(
        r#"
    effect Ask {
        func ask(x: i64) -> i64
    }
    @noalloc
    func asks(x: i64) uses Ask -> i64 { perform Ask.ask(x) }
    "#,
    );
    assert!(
        diags.contains(
            "`asks` is `@noalloc` but allocates here: `perform` runs a handler chosen at run time"
        ),
        "{diags}"
    );
}

#[test]
fn deadline_is_rejected_with_the_reason() {
    let diags = diagnostics(
        r#"
    @deadline(100)
    func f() {}
    @noalloc(1)
    func g() {}
    "#,
    );
    assert!(
        diags.contains("`@deadline` is not supported: a worst-case execution time"),
        "{diags}"
    );
    assert!(diags.contains("`@noalloc` takes no arguments"), "{diags}");
    assert_eq!(diags.lines().count(), 2, "{diags}");
}

#[test]
fn summaries_collect_effects_through_calls_and_recursion() {
    let source = r#"
    class C { x: i64 }
    func leaf(xs: [i64]) -> i64 { xs[0] }
    func mid(xs: [i64]) -> i64 { leaf(xs) }
    func alloc() -> i64 { let c = C { x: 1 }; c.x }
    func spin() { loop { } }
    func pure(a: i64) -> i64 { a + 1 }
    func rec(n: i64) -> i64 { if n == 0 { 0 } else { rec(n - 1) } }
    "#;
    assert_eq!(effects_of(source, "mid"), set(&[Effect::Panic]));
    assert_eq!(
        effects_of(source, "alloc"),
        set(&[Effect::Alloc, Effect::Block, Effect::Panic])
    );
    assert_eq!(effects_of(source, "spin"), set(&[Effect::Unbounded]));
    assert_eq!(effects_of(source, "pure"), EffectSet::NONE);
    assert_eq!(effects_of(source, "rec"), set(&[Effect::Unbounded]));
}

#[test]
fn array_filled_allocates_and_the_scalar_builtins_do_not() {
    let diags = diagnostics(
        r#"
    @noalloc
    func filled(n: usize) -> [f64] { array_filled(0.0, n) }
    @realtime
    func scalar(c: bool, a: f64, b: f64, d: f64) -> f64 {
        if likely(c) { fma(a, b, d) } else { select(unlikely(c), a, b) }
    }
    "#,
    );
    assert_eq!(
        diags
            .matches("`array_filled(..)` allocates an array")
            .count(),
        1,
        "{diags}"
    );
    assert!(!diags.contains("scalar"), "{diags}");
}

// ---- @envelope (Codira Semantic Envelope, version 0) ----------------------

#[test]
fn envelope_clauses_are_checked_through_calls() {
    let diags = diagnostics(
        r#"
    class C { x: i64 }
    extern func c_io(x: i64) -> i64;
    func make() -> i64 { let c = C { x: 1 }; c.x }
    func io(x: i64) -> i64 { c_io(x) }
    func tasks() { scope { } }

    @envelope(allocation: none)
    func a() -> i64 { make() }
    @envelope(foreign: none, allocation: any)
    func b(x: i64) -> i64 { io(x) }
    @envelope(concurrency: none)
    func c() { tasks() }
    @envelope(allocation: none, blocking: false, panic: false, termination: bounded, foreign: none, concurrency: none, effects: none, secrecy: none)
    func pure(x: i64) -> i64 { x + 1 }
    "#,
    );
    let count = |needle: &str| diags.matches(needle).count();
    assert_eq!(
        count(
            "`a` is `@envelope(allocation: none)` but calls `make` (line 8), which allocates: \
             class `C` constructed"
        ),
        1,
        "{diags}"
    );
    assert_eq!(
        count(
            "`b` is `@envelope(foreign: none)` but calls `io` (line 10), which calls `c_io` \
             (line 4), which runs code outside the analysis"
        ),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`c` is `@envelope(concurrency: none)` but calls `tasks` (line 12), which starts concurrent tasks"),
        1,
        "{diags}"
    );
    assert_eq!(count("`pure`"), 0, "{diags}");
    assert_eq!(diags.lines().count(), 3, "{diags}");
}

#[test]
fn envelope_and_attributes_combine_in_one_message() {
    let diags = diagnostics(
        r#"
    @noalloc @envelope(allocation: none)
    func both() { println("hi") }
    "#,
    );
    assert!(
        diags.contains(
            "`both` is `@noalloc` and `@envelope(allocation: none)` but allocates here: `println`"
        ),
        "{diags}"
    );
    assert_eq!(diags.lines().count(), 1, "{diags}");
}

#[test]
fn envelope_tracks_effects_and_secrets() {
    let diags = diagnostics(
        r#"
    effect Ask {
        func ask(x: i64) -> i64
    }
    func asks(x: i64) uses Ask -> i64 { perform Ask.ask(x) }
    @envelope(effects: none)
    func caller(x: i64) uses Ask -> i64 { asks(x) }

    func mix(key: secret u64) -> secret u64 { key + 1 }
    @envelope(secrecy: none)
    func public_only(x: u64) -> u64 { declassify(mix(x)) }
    @envelope(secrecy: none)
    func plain(x: u64) -> u64 { x * 2 }
    "#,
    );
    assert!(
        diags.contains("`caller` is `@envelope(effects: none)` but calls `asks`"),
        "{diags}"
    );
    assert!(
        diags.contains(
            "`public_only` is `@envelope(secrecy: none)` but handles `secret` data here: \
             `declassify` releases a `secret` value"
        ),
        "{diags}"
    );
    assert!(
        diags.contains(
            "`public_only` is `@envelope(secrecy: none)` but calls `mix` (line 10), which \
             handles `secret` data: `mix` takes, returns or binds a `secret` value"
        ),
        "{diags}"
    );
    assert!(!diags.contains("`plain`"), "{diags}");
}

#[test]
fn envelope_rejects_what_it_cannot_check() {
    let diags = diagnostics(
        r#"
    @envelope(reads: [Account], allocation: none)
    func r() {}
    @envelope(allocation: maybe)
    func v() {}
    @envelope(colour: blue)
    func u() {}
    @envelope(allocation: none, allocation: any)
    func d() {}
    @envelope(none)
    func p() {}
    @envelope
    func e() {}
    @envelope(allocation: none) @envelope(blocking: false)
    func twice() {}
    "#,
    );
    let count = |needle: &str| diags.matches(needle).count();
    assert_eq!(
        count("`@envelope` key `reads` is part of the semantic-envelope design but cannot be checked yet"),
        1,
        "{diags}"
    );
    assert_eq!(
        count("`@envelope` key `allocation` takes `none` or `any`, not `maybe`"),
        1,
        "{diags}"
    );
    assert_eq!(count("unknown `@envelope` key `colour`"), 1, "{diags}");
    assert_eq!(
        count("`@envelope` key `allocation` is given more than once"),
        1,
        "{diags}"
    );
    assert_eq!(count("`none` has no key"), 1, "{diags}");
    assert_eq!(count("`@envelope` needs clauses"), 1, "{diags}");
    assert_eq!(
        count("a function has at most one `@envelope`"),
        1,
        "{diags}"
    );
    assert_eq!(diags.lines().count(), 7, "{diags}");
}

#[test]
fn envelope_on_an_extern_is_a_trusted_assertion() {
    let diags = diagnostics(
        r#"
    @envelope(foreign: none, allocation: none)
    extern func c_pure(x: i64) -> i64;
    extern func c_other(x: i64) -> i64;
    @envelope(foreign: none, allocation: none)
    func ok(x: i64) -> i64 { c_pure(x) }
    @envelope(foreign: none)
    func bad(x: i64) -> i64 { c_other(x) }
    "#,
    );
    assert!(!diags.contains("`ok`"), "{diags}");
    assert!(
        diags.contains("`bad` is `@envelope(foreign: none)` but calls `c_other`"),
        "{diags}"
    );
    assert_eq!(diags.lines().count(), 1, "{diags}");
}

#[test]
fn scope_spawn_and_externs_have_the_new_effects() {
    let source = r#"
    extern func c_x(x: i64) -> i64;
    func par() { scope { } }
    func ext(x: i64) -> i64 { c_x(x) }
    func s(k: secret u64) -> secret u64 { k }
    "#;
    assert!(effects_of(source, "par").contains(Effect::Spawn));
    let ext = effects_of(source, "ext");
    assert!(ext.contains(Effect::Foreign) && !ext.contains(Effect::Secret));
    assert_eq!(effects_of(source, "s"), set(&[Effect::Secret]));
}

#[test]
fn envelope_report_infers_every_function() {
    let (db, _) = MockDatabase::with_single_file(
        r#"
    func pure(x: i64) -> i64 { x + 1 }
    @envelope(allocation: none)
    func noisy() { println("hi") }
    "#,
    );
    let report = super::envelope_report(&db);
    assert_eq!(report.len(), 2);
    let pure = &report[0];
    assert_eq!(pure.name, "pure");
    assert!(pure.declared.is_none() && pure.violated.is_empty());
    let value = |f: &super::FunctionEnvelope, key: &str| {
        f.inferred
            .iter()
            .find(|d| d.key == key)
            .map(|d| d.value)
            .unwrap()
    };
    assert_eq!(value(pure, "allocation"), "none");
    assert_eq!(value(pure, "termination"), "bounded");
    assert_eq!(value(pure, "secrecy"), "none");
    let noisy = &report[1];
    assert_eq!(value(noisy, "allocation"), "any");
    assert_eq!(noisy.violated, vec!["allocation"]);
    let because = noisy
        .inferred
        .iter()
        .find(|d| d.key == "allocation")
        .and_then(|d| d.because.clone())
        .unwrap();
    assert!(because.contains("`println` formats"), "{because}");
}

// ---- package envelopes, values and comparisons (version 1) ---------------

#[test]
fn envelope_from_manifest_pairs_takes_the_attribute_keys_and_rejects_the_rest() {
    let envelope = super::Envelope::from_pairs([
        ("allocation", "none"),
        ("blocking", "false"),
        ("foreign", "any"),
    ]);
    assert!(envelope.errors.is_empty(), "{:?}", envelope.errors);
    assert_eq!(envelope.forbidden(), set(&[Effect::Alloc, Effect::Block]));

    let envelope = super::Envelope::from_pairs([
        ("reads", "Account"),
        ("colour", "blue"),
        ("allocation", "maybe"),
        ("panic", "false"),
        ("panic", "true"),
    ]);
    let errors = envelope.errors.join("\n");
    assert!(
        errors.contains(
            "`[envelope]` key `reads` is part of the semantic-envelope design but cannot be \
             checked yet"
        ),
        "{errors}"
    );
    assert!(
        errors.contains("unknown `[envelope]` key `colour`"),
        "{errors}"
    );
    assert!(
        errors.contains("`[envelope]` key `allocation` takes `none` or `any`, not `maybe`"),
        "{errors}"
    );
    assert!(
        errors.contains("`[envelope]` key `panic` is given more than once"),
        "{errors}"
    );
    assert_eq!(envelope.errors.len(), 4, "{errors}");

    let empty = super::Envelope::from_pairs([]);
    assert!(
        empty.errors[0].contains("has no keys"),
        "{:?}",
        empty.errors
    );
}

#[test]
fn envelope_package_checks_public_functions_through_private_callees() {
    let (db, _) = MockDatabase::with_single_file(
        r#"
    class Buffer { len: i64 }
    func make(n: i64) -> i64 { let b = Buffer { len: n }; b.len }
    func unreached() { println("never called from the public API") }
    public func run(n: i64) -> i64 { make(n) }
    public func pure(x: i64) -> i64 { x * 2 }
    "#,
    );
    let envelope = super::Envelope::from_pairs([("allocation", "none"), ("foreign", "none")]);
    let violations = super::package_envelope_violations(&db, &envelope);
    assert_eq!(violations.len(), 1, "{violations:#?}");
    let run = &violations[0];
    assert_eq!(run.name, "run");
    assert_eq!(run.keys, vec!["allocation"]);
    assert!(
        run.message.contains(
            "public function `run` (mod.code:4) does not fit the package envelope \
             (`[envelope]` in codira.toml: allocation = \"none\"): it allocates"
        ),
        "{}",
        run.message
    );
    assert!(
        run.message
            .contains("calls `make` (line 4); class `Buffer` constructed at line 2"),
        "{}",
        run.message
    );

    // An envelope that forbids nothing constrains nothing.
    let permissive = super::Envelope::from_pairs([("allocation", "any")]);
    assert!(super::package_envelope_violations(&db, &permissive).is_empty());
}

#[test]
fn envelope_values_and_their_direction_of_change() {
    use super::envelope::{compare_values, inferred_values, Movement, KEY_NAMES};
    let (db, _) = MockDatabase::with_single_file(
        r#"
    func pure(x: i64) -> i64 { x + 1 }
    func noisy() { println("hi") }
    "#,
    );
    let functions: Vec<_> = Package::all(&db)
        .iter()
        .flat_map(|p| p.modules(&db))
        .flat_map(|m| m.all_functions(&db))
        .collect();
    let by_name = |name: &str| {
        *functions
            .iter()
            .find(|f| f.name(&db).to_string() == name)
            .unwrap()
    };
    let pure = inferred_values(&db, by_name("pure"));
    let noisy = inferred_values(&db, by_name("noisy"));
    assert_eq!(pure.iter().map(|(k, _)| *k).collect::<Vec<_>>(), KEY_NAMES);
    assert!(pure.contains(&("allocation", "none")));
    assert!(noisy.contains(&("allocation", "any")));
    assert!(noisy.contains(&("blocking", "true")));

    let widened = compare_values(&pure, &noisy);
    assert!(
        widened.contains(&("allocation", Movement::Widened)),
        "{widened:?}"
    );
    assert!(widened.iter().all(|(_, m)| *m == Movement::Widened));
    let narrowed = compare_values(&noisy, &pure);
    assert!(narrowed.contains(&("allocation", Movement::Narrowed)));
    assert!(narrowed.iter().all(|(_, m)| *m == Movement::Narrowed));
    assert!(compare_values(&pure, &pure).is_empty());
    // A value that is not one of its key's two cannot be compared, so it
    // counts as widened.
    assert_eq!(
        compare_values(&[("allocation", "any")], &[("allocation", "some")]),
        vec![("allocation", Movement::Widened)]
    );
}
