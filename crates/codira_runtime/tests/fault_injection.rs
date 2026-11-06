//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The allocation fault-injection gate for generated code: each program
//!   below is run in a child process with its `N`th GC allocation made to
//!   fail (`CODIRA_TEST_FAIL_NTH_ALLOCATION=N`), for `N = 1, 2, ...` until a
//!   run completes. Every run must either end through the runtime's
//!   out-of-memory exit -- the message naming the failed allocation, then
//!   exit status 102 -- or, once `N` is past the program's last
//!   allocation, complete with the right result. A crash by any other signal
//!   (a segfault from a null handle, say), a Rust panic, a wrong result or a
//!   hang fails the gate.
//!
//! What this checks: every GC allocation point a program reaches (objects,
//! arrays, array growth, strings, closures, allocations on worker threads)
//! fails in a controlled way. What it cannot check: allocations outside
//! the GC heap (arenas, `codira_alloc`, the runtime's own Rust allocations),
//! and recovery -- generated code cannot observe a failed allocation, so the
//! process ends.
//!
//! `CODIRA_FAULT_BOUND` raises the cap on `N` per program (default 3000).
use std::{
    path::Path,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

use codira_runtime::{Runtime, FAIL_NTH_ALLOCATION_ENV, HEAP_EXHAUSTED_EXIT_STATUS};
use codira_test::CompileTestDriver;

/// Objects, arrays of references, growth by `push`, and a cycle.
const OBJECTS: &str = r"
    public class Node { id: i64, kids: [Node] }

    public func run(n: i64) -> i64 {
        let all: [Node] = []
        var i = 0
        while i < n {
            let node = Node { id: i, kids: [] }
            if i > 0 {
                node.kids.push(all[(i * 7 % i) as usize])
                all[(i / 2) as usize].kids.push(node)
            }
            all.push(node)
            i += 1
        }
        var sum = 0
        for node in all {
            sum += node.id * 3
            for kid in node.kids { sum += kid.id }
        }
        sum
    }
";

/// Strings: building, formatting, splitting and comparing.
const STRINGS: &str = r#"
    public func run(n: i64) -> i64 {
        var built = "".to_string()
        var total = 0
        var i = 0
        while i < n {
            // Longer than an inline string, so they live in the GC heap.
            let s = "k" + i.to_string() + "-" + (i * i).to_string() + "-" + "long".repeat(12)
            built += s
            let parts = s.split("-")
            total += parts.len() as i64 + s.len() as i64
            if parts[0] == "k" + i.to_string() { total += 1 }
            i += 1
        }
        total + built.len() as i64 + "ab".repeat(n).len() as i64
    }
"#;

/// Function values and closure environments, and the eager array methods.
const CLOSURES: &str = r"
    func adder(a: i64) -> func(i64) -> i64 {
        func(b: i64) -> i64 { a + b }
    }

    public func run(n: i64) -> i64 {
        let xs: [i64] = []
        var i = 0
        while i < n { xs.push(i); i += 1 }
        var total = 0
        i = 0
        while i < n {
            let f = adder(i)
            total += f(1)
            i += 1
        }
        let doubled = xs.map(func(x) { x * 2 })
        let big = doubled.filter(func(x) { x > n })
        total + big.len() as i64 + xs[1..(n as usize)].len() as i64
    }
";

/// Allocation on worker threads: tasks allocate objects and arrays while
/// the spawning thread does too.
const TASKS: &str = r"
    public class Cell { v: i64, next: [Cell] }

    func build(seed: i64, n: i64) -> i64 {
        let cells: [Cell] = []
        var i = 0
        while i < n {
            cells.push(Cell { v: seed * 100 + i, next: [] })
            i += 1
        }
        var sum = 0
        for c in cells { sum += c.v }
        sum
    }

    func worker(results: Channel[i64], seed: i64, n: i64) {
        results <- build(seed, n)
    }

    public func run(n: i64) -> i64 {
        scope {
            let results = channel(8)
            spawn worker(results, 1, n)
            spawn worker(results, 2, n)
            var sum = build(3, n)
            sum += <-results
            sum += <-results
            sum
        }
    }
";

const CHILD_ENV: &str = "CODIRA_FAULT_CHILD_LIB";
const ARG_ENV: &str = "CODIRA_FAULT_CHILD_ARG";

/// Runs in a child process: loads the library, runs `run(arg)` and prints
/// the result. Fault injection is armed by the runtime from
/// [`FAIL_NTH_ALLOCATION_ENV`], exactly as for `codira run`.
#[test]
fn fault_child() {
    let Some(lib) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let arg: i64 = std::env::var(ARG_ENV).unwrap().parse().unwrap();
    // Safety: the parent compiled this library.
    let runtime = unsafe { Runtime::builder(lib).finish() }.expect("runtime");
    let result: i64 = runtime.invoke("run", (arg,)).unwrap();
    println!("fault-child-result={result}");
    assert_eq!(runtime.gc().injected_failures(), 0);
}

/// How one child run ended.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The program finished with this result.
    Completed(i64),
    /// The runtime's controlled out-of-memory exit.
    OutOfMemory,
}

/// Whether a child ended through the runtime's out-of-memory exit: the
/// diagnostic naming the type and the injected failure, then exit status
/// [`HEAP_EXHAUSTED_EXIT_STATUS`] (102).
fn is_out_of_memory_exit(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains("fatal: out of memory: cannot allocate `")
        && stderr.contains("injected allocation failure")
        && output.status.code() == Some(HEAP_EXHAUSTED_EXIT_STATUS)
}

fn classify(output: &Output, n: u64) -> Outcome {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        // The test harness prints the test's name on the same line.
        if let Some((_, rest)) = stdout.split_once("fault-child-result=") {
            let value = rest.split_whitespace().next().unwrap_or_default();
            return Outcome::Completed(value.parse().unwrap());
        }
    }
    if is_out_of_memory_exit(output) {
        return Outcome::OutOfMemory;
    }
    panic!(
        "allocation {n} failed without the controlled exit: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
}

fn run_child(lib: &Path, arg: i64, n: u64) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fault_child", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, lib)
        .env(ARG_ENV, arg.to_string())
        .env(FAIL_NTH_ALLOCATION_ENV, n.to_string())
        .env("RUST_BACKTRACE", "0")
        .output()
        .expect("failed to run the child test")
}

fn bound() -> u64 {
    std::env::var("CODIRA_FAULT_BOUND")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3_000)
}

/// Fails every allocation of `run(arg)` in turn, four children at a time.
/// Returns the number of allocation points checked.
fn fail_every_allocation(source: &str, arg: i64) -> u64 {
    let compiled = CompileTestDriver::from_file(source);
    let lib = compiled.lib_path();
    // The expected result, with nothing injected.
    let expected = {
        // Safety: we compiled the library ourselves.
        let runtime = unsafe { Runtime::builder(lib).finish() }.expect("runtime");
        runtime.invoke::<i64, _>("run", (arg,)).unwrap()
    };

    let bound = bound();
    let next = AtomicU64::new(1);
    let completed_at = AtomicU64::new(u64::MAX);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| loop {
                let n = next.fetch_add(1, Ordering::SeqCst);
                if n > bound || n > completed_at.load(Ordering::SeqCst) {
                    return;
                }
                match classify(&run_child(lib, arg, n), n) {
                    Outcome::Completed(value) => {
                        assert_eq!(value, expected, "allocation {n}: wrong result");
                        completed_at.fetch_min(n, Ordering::SeqCst);
                    }
                    Outcome::OutOfMemory => assert!(
                        n < completed_at.load(Ordering::SeqCst),
                        "allocation {n} failed after a run with fewer allocations completed"
                    ),
                }
            });
        }
    });
    let completed_at = completed_at.into_inner();
    assert!(
        completed_at <= bound,
        "no run completed within {bound} allocations; raise CODIRA_FAULT_BOUND"
    );
    assert!(completed_at > 1, "the program allocated nothing");
    eprintln!(
        "fault injection: {} allocation points failed cleanly",
        completed_at - 1
    );
    completed_at - 1
}

#[test]
fn every_object_and_array_allocation_fails_cleanly() {
    let points = fail_every_allocation(OBJECTS, 40);
    assert!(points >= 80, "only {points} allocation points");
}

#[test]
fn every_string_allocation_fails_cleanly() {
    let points = fail_every_allocation(STRINGS, 25);
    assert!(points >= 80, "only {points} allocation points");
}

#[test]
fn every_closure_allocation_fails_cleanly() {
    let points = fail_every_allocation(CLOSURES, 30);
    assert!(points >= 30, "only {points} allocation points");
}

#[test]
fn every_allocation_on_worker_threads_fails_cleanly() {
    // The order of allocations across threads varies between runs, but
    // their number does not: every N up to it fails one of them.
    let points = fail_every_allocation(TASKS, 30);
    assert!(points >= 90, "only {points} allocation points");
}
