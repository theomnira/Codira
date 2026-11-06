//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Differential testing of frame allocation (`--frame-alloc`): seeded,
//!   random programs over classes that hold classes, small arrays of class
//!   instances, helper functions that read, return, keep and swap their
//!   arguments, loops that allocate per iteration and sometimes keep the
//!   object past the iteration, field and element stores, and garbage that
//!   makes the heap limit force collections. Each program is compiled with
//!   frame allocation off and on, at `-O 0` and at the default level, and
//!   every build must return the same checksum.
//! - The checksum reads every field of every object at the end, so an
//!   object whose frame slot was reused too early, or a heap object the
//!   collector freed while a frame object held it, changes the result.
//! - `CODIRA_FRAME_DIFF_PROGRAMS` and `CODIRA_FRAME_DIFF_SEED` change how many
//!   programs run and from which seed. On a difference the test fails with
//!   the seed and the program.

use std::fmt::Write as _;

use codira_test::CompileAndRunTestDriver;

const DEFAULT_SEED: u64 = 0xF4A3_E000;
const DEFAULT_PROGRAMS: u64 = 60;
/// Room for a block of every size class the programs use, and small enough
/// that `churn` reaches it.
const HEAP_LIMIT: usize = 1024 * 1024;

/// `SplitMix64`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

const PRELUDE: &str = r#"
class A { var v: i64, var w: i64 }
class B { var a: A, var k: i64 }
class C { var b: B, var xs: [A], var t: i64 }

func churn(n: i64) -> i64 {
    var total = 0
    var i = 0
    while i < n {
        let g = A { v: i, w: i + 1 }
        let keep = make(g.v)
        total += keep.w - g.v
        i += 1
    }
    total
}

func make(n: i64) -> A { A { v: n, w: n * 3 + 1 } }
func read_a(x: A) -> i64 { x.v * 7 + x.w }
func read_b(b: B) -> i64 { b.a.v + b.a.w * 5 + b.k }
func id_a(x: A) -> A { x }
func pick(c: bool, x: A, y: A) -> A { if c { x } else { y } }
func keep(b: B, x: A) { b.a = x }
func swap(b: B, c: B) { let t = b.a; b.a = c.a; c.a = t }
func sum_xs(c: C) -> i64 {
    var s = 0
    for x in c.xs { s += x.v - x.w }
    s
}
func stash(c: C, x: A) { c.xs[0] = x }
func countdown(x: A, n: i64) -> i64 { if n <= 0 { x.v } else { countdown(x, n - 1) + 1 } }
func wrap(x: A, k: i64) -> B { B { a: x, k: k } }
"#;

/// Locals of the generated function, by class.
#[derive(Default)]
struct Scope {
    a: Vec<String>,
    b: Vec<String>,
    c: Vec<String>,
    /// The `var` locals of class `A`, which may be reassigned.
    var_a: Vec<String>,
    next: usize,
}

impl Scope {
    fn fresh(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }
}

fn int(rng: &mut Rng, scope: &Scope) -> String {
    match rng.below(5) {
        0 | 1 => format!("{}", rng.below(100) as i64 - 20),
        2 if !scope.a.is_empty() => format!("{}.v", rng.pick(&scope.a)),
        3 if !scope.b.is_empty() => format!("{}.a.w", rng.pick(&scope.b)),
        _ => format!("(total & 255)"),
    }
}

fn a_expr(rng: &mut Rng, scope: &Scope) -> String {
    let have = !scope.a.is_empty();
    match rng.below(6) {
        0 if have => format!("id_a({})", rng.pick(&scope.a)),
        1 if have && scope.a.len() > 1 => format!(
            "pick(total % 2 == 0, {}, {})",
            rng.pick(&scope.a),
            rng.pick(&scope.a)
        ),
        2 => format!("make({})", int(rng, scope)),
        3 if !scope.b.is_empty() => format!("{}.a", rng.pick(&scope.b)),
        _ => format!("A {{ v: {}, w: {} }}", int(rng, scope), int(rng, scope)),
    }
}

fn statement(rng: &mut Rng, scope: &mut Scope, out: &mut String, depth: usize, in_loop: bool) {
    let indent = "    ".repeat(depth + 1);
    match rng.below(19) {
        0 | 1 => {
            let value = a_expr(rng, scope);
            let mutable = rng.chance(30) && !in_loop;
            let name = scope.fresh("a");
            let _ = writeln!(
                out,
                "{indent}{} {name} = {value}",
                if mutable { "var" } else { "let" }
            );
            scope.a.push(name.clone());
            if mutable {
                scope.var_a.push(name);
            }
        }
        2 if !scope.a.is_empty() => {
            let name = scope.fresh("b");
            let value = if rng.chance(30) {
                format!("wrap({}, {})", rng.pick(&scope.a), int(rng, scope))
            } else {
                format!("B {{ a: {}, k: {} }}", rng.pick(&scope.a), int(rng, scope))
            };
            let _ = writeln!(out, "{indent}let {name} = {value}");
            scope.b.push(name);
        }
        3 if !scope.b.is_empty() && !scope.a.is_empty() => {
            let name = scope.fresh("c");
            let len = 1 + rng.below(3);
            let elements: Vec<String> = (0..len).map(|_| rng.pick(&scope.a).clone()).collect();
            let _ = writeln!(
                out,
                "{indent}let {name} = C {{ b: {}, xs: [{}], t: {} }}",
                rng.pick(&scope.b),
                elements.join(", "),
                int(rng, scope)
            );
            scope.c.push(name);
        }
        4 if !scope.a.is_empty() => {
            let target = rng.pick(&scope.a).clone();
            let field = if rng.chance(50) { "v" } else { "w" };
            let _ = writeln!(out, "{indent}{target}.{field} = {}", int(rng, scope));
        }
        5 if !scope.b.is_empty() && !scope.a.is_empty() => {
            let _ = writeln!(
                out,
                "{indent}{}.a = {}",
                rng.pick(&scope.b),
                rng.pick(&scope.a)
            );
        }
        6 if !scope.b.is_empty() && !scope.a.is_empty() => {
            let _ = writeln!(
                out,
                "{indent}keep({}, {})",
                rng.pick(&scope.b),
                rng.pick(&scope.a)
            );
        }
        7 if scope.b.len() > 1 => {
            let _ = writeln!(
                out,
                "{indent}swap({}, {})",
                rng.pick(&scope.b),
                rng.pick(&scope.b)
            );
        }
        8 if !scope.c.is_empty() && !scope.a.is_empty() => {
            let c = rng.pick(&scope.c).clone();
            if rng.chance(50) {
                let _ = writeln!(out, "{indent}stash({c}, {})", rng.pick(&scope.a));
            } else {
                let _ = writeln!(out, "{indent}{c}.xs[0] = {}", rng.pick(&scope.a));
            }
        }
        9 if !scope.c.is_empty() && !scope.a.is_empty() => {
            let _ = writeln!(
                out,
                "{indent}{}.b.a = {}",
                rng.pick(&scope.c),
                rng.pick(&scope.a)
            );
        }
        10 if !scope.var_a.is_empty() && !scope.a.is_empty() => {
            let _ = writeln!(
                out,
                "{indent}{} = {}",
                rng.pick(&scope.var_a),
                rng.pick(&scope.a)
            );
        }
        11 if depth < 2 => {
            // A loop that allocates per iteration and sometimes keeps the
            // object in a variable outside the loop.
            let i = scope.fresh("i");
            let rounds = 1 + rng.below(6);
            let _ = writeln!(out, "{indent}for {i} in 0..{rounds} {{");
            let saved = (
                scope.a.len(),
                scope.b.len(),
                scope.c.len(),
                scope.var_a.len(),
            );
            let local = scope.fresh("t");
            let _ = writeln!(
                out,
                "{indent}    let {local} = A {{ v: {i}, w: {i} * 2 + (total & 7) }}"
            );
            scope.a.push(local.clone());
            let _ = writeln!(out, "{indent}    total += read_a({local})");
            if let Some(outer) = scope.var_a.first().cloned() {
                if rng.chance(50) {
                    let _ = writeln!(out, "{indent}    if {i} % 2 == 0 {{ {outer} = {local} }}");
                }
            }
            for _ in 0..1 + rng.below(3) {
                statement(rng, scope, out, depth + 1, true);
            }
            scope.a.truncate(saved.0);
            scope.b.truncate(saved.1);
            scope.c.truncate(saved.2);
            scope.var_a.truncate(saved.3);
            let _ = writeln!(out, "{indent}}}");
        }
        12 => {
            let _ = writeln!(out, "{indent}total += churn({})", 2000 + rng.below(6000));
        }
        13 if !scope.c.is_empty() => {
            let _ = writeln!(out, "{indent}total += sum_xs({})", rng.pick(&scope.c));
        }
        14 if !scope.a.is_empty() => {
            let _ = writeln!(
                out,
                "{indent}total += countdown({}, {})",
                rng.pick(&scope.a),
                rng.below(5)
            );
        }
        15 if !scope.a.is_empty() => {
            // A fold whose closure allocates the next accumulator.
            let elements: Vec<String> = (0..1 + rng.below(4))
                .map(|_| rng.pick(&scope.a).clone())
                .collect();
            let _ = writeln!(
                out,
                "{indent}total += [{}].fold(A {{ v: {}, w: 1 }}, func(acc, x) {{ let p = A {{ v: 1, \
                 w: x.w }}; p.v = acc.v * 3 + acc.w + x.v; p }}).v",
                elements.join(", "),
                int(rng, scope)
            );
        }
        16 if !scope.a.is_empty() => {
            // Objects built by a pipeline stage, collected.
            let _ = writeln!(
                out,
                "{indent}for p in [{}, {}].iter().map(func(x) {{ A {{ v: x.w, w: x.v + (total & \
                 3) }} }}).collect() {{ total += read_a(p) }}",
                rng.pick(&scope.a),
                rng.pick(&scope.a)
            );
        }
        17 if !scope.a.is_empty() => {
            // `map` building new objects from the elements.
            let _ = writeln!(
                out,
                "{indent}total += [{}].map(func(x) {{ A {{ v: x.v + 1, w: x.w }} }})[0].v",
                rng.pick(&scope.a)
            );
        }
        _ if !scope.b.is_empty() => {
            let _ = writeln!(out, "{indent}total += read_b({})", rng.pick(&scope.b));
        }
        _ => {
            let _ = writeln!(out, "{indent}total += {}", int(rng, scope));
        }
    }
}

fn generate(seed: u64) -> String {
    let mut rng = Rng(seed);
    let mut scope = Scope::default();
    let mut body = String::new();
    for _ in 0..8 + rng.below(16) {
        statement(&mut rng, &mut scope, &mut body, 0, false);
    }
    // Collect while every object is alive, then read every field of every
    // object that is still in scope.
    let _ = writeln!(body, "    total += churn(40000)");
    let mut checksum = String::new();
    for a in &scope.a {
        let _ = write!(checksum, " + read_a({a})");
    }
    for b in &scope.b {
        let _ = write!(checksum, " + read_b({b})");
    }
    for c in &scope.c {
        let _ = write!(checksum, " + sum_xs({c}) + {c}.t + read_b({c}.b)");
    }
    format!(
        "{PRELUDE}\npublic func run() -> i64 {{\n    var total = 0\n{body}    total{checksum}\n}}\n"
    )
}

/// What one build of a program did.
struct Run {
    result: i64,
    /// Allocation sites moved to the stack.
    moved: usize,
    /// Collections the heap limit forced.
    collections: usize,
}

/// Runs `source` with frame allocation `frame` at `-O 0` or the default
/// level, under the heap limit.
fn run(source: &str, frame: bool, opt0: bool) -> Run {
    let driver = if opt0 {
        let text = source.to_string();
        let config = codira_compiler::Config {
            optimization_lvl: codira_compiler::OptimizationLevel::None,
            ..codira_compiler::Config::default()
        };
        let driver = codira_test::CompileTestDriver::from_file_with_config(&text, config, |d| {
            d.set_frame_allocation(frame);
        });
        let runtime = unsafe {
            codira_runtime::Runtime::builder(driver.lib_path())
                .heap_limit(HEAP_LIMIT)
                .finish()
        }
        .expect("the runtime starts");
        let result: i64 = runtime.invoke("run", ()).unwrap();
        return Run {
            result,
            moved: driver
                .allocation_report()
                .matches("moved to the stack")
                .count(),
            collections: runtime.gc().emergency_collections(),
        };
    } else {
        CompileAndRunTestDriver::with_frame_allocation(source, frame, |b| b.heap_limit(HEAP_LIMIT))
            .expect("Failed to build test driver")
    };
    let result: i64 = driver.runtime.invoke("run", ()).unwrap();
    Run {
        result,
        moved: driver
            .allocation_report()
            .matches("moved to the stack")
            .count(),
        collections: driver.runtime.gc().emergency_collections(),
    }
}

#[test]
fn frame_allocation_does_not_change_results() {
    let seed = std::env::var("CODIRA_FRAME_DIFF_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SEED);
    let programs = std::env::var("CODIRA_FRAME_DIFF_PROGRAMS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PROGRAMS);
    let mut moved_total = 0;
    let mut collected = 0;
    for index in 0..programs {
        let program_seed = seed.wrapping_add(index);
        let source = generate(program_seed);
        if std::env::var_os("CODIRA_FRAME_DIFF_VERBOSE").is_some() {
            eprintln!("seed {program_seed:#x}\n{source}");
        }
        let reference = run(&source, false, false).result;
        let mut results = vec![("heap, -O 2", reference)];
        let on = run(&source, true, false);
        results.push(("frame, -O 2", on.result));
        if index % 3 == 0 {
            results.push(("heap, -O 0", run(&source, false, true).result));
            let o0 = run(&source, true, true);
            results.push(("frame, -O 0", o0.result));
            collected += usize::from(o0.collections > 0);
        }
        moved_total += on.moved;
        collected += usize::from(on.collections > 0);
        if results.iter().any(|(_, r)| *r != reference) {
            panic!("seed {program_seed:#x}: results differ: {results:?}\n{source}");
        }
    }
    // The programs must exercise the optimisation, or they test nothing.
    assert!(
        moved_total >= programs as usize,
        "only {moved_total} sites moved to the stack in {programs} programs"
    );
    // And collections must run while frame objects are alive.
    assert!(
        collected >= programs as usize,
        "only {collected} frame-allocated runs collected"
    );
    eprintln!(
        "{programs} programs: {moved_total} sites moved to the stack, {collected} \
         frame-allocated runs collected"
    );
}
