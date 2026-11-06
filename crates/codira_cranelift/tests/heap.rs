//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Heap programs on the Cranelift JIT: classes, methods, arrays, `for`
//!   loops; a run under a small heap limit, where allocations collect while
//!   JIT frames hold the only references to live objects; and the precise
//!   stack-map hook.

use codira_compiler::{Config, Driver, PathOrInline, RelativePathBuf};
use codira_cranelift::{compile, compile_with, Options, Returned};

fn driver(source: &str) -> Driver {
    let (driver, _) = Driver::with_file(
        Config::default(),
        PathOrInline::Inline {
            rel_path: RelativePathBuf::from("mod.code"),
            contents: source.to_owned(),
        },
    )
    .unwrap();
    driver
}

/// Runs `main` and returns what it returned and printed.
fn run(source: &str) -> (Returned, String) {
    let driver = driver(source);
    let program = compile(driver.db(), "main").unwrap_or_else(|e| panic!("did not compile: {e}"));
    codira_runtime::capture_output(|| program.run())
}

#[test]
fn classes_fields_and_methods() {
    let source = r"
        class Counter { n: i32, total: f64, on: bool }
        extend Counter {
            func bump(self, by: i32) { self.n += by; self.total += 0.5 }
            func get(self) -> i32 { self.n }
            func make(n: i32) -> Counter { Counter { total: 1.0, n: n, on: true } }
        }
        class Pair(i64, u8);
        class Unit;
        public func main() -> i64 {
            let c = Counter.make(3)
            c.bump(4)
            c.on = !c.on
            let p = Pair(40, 2)
            let u = Unit
            println(c.get(), c.total, c.on, p.0, p.1)
            (c.get() as i64) * 100 + p.0 + (p.1 as i64)
        }
    ";
    let (returned, printed) = run(source);
    assert_eq!(printed, "71.5false402\n");
    assert_eq!(returned, Returned::Int(742));
}

#[test]
fn arrays_grow_index_and_loop() {
    let source = r"
        class Item { weight: i64 }
        func sum(xs: [i64]) -> i64 {
            var t: i64 = 0
            for x in xs { t += x }
            t
        }
        public func main() -> i64 {
            var xs: [i64] = [1, 2, 3]
            for i in 0..1000 { xs.push(i) }
            xs[0] = 40
            xs[1] += 2
            let items = [Item { weight: 5 }, Item { weight: 7 }]
            items[1].weight *= 10
            var w: i64 = 0
            for item in items { w += item.weight }
            var bs = [true, false]
            bs[1] = true
            let small: u8 = 4
            println(xs.len(), xs.is_empty(), bs[0] && bs[1], xs[small], xs.unchecked_get(2))
            sum(xs) + w
        }
    ";
    let (returned, printed) = run(source);
    assert_eq!(printed, "1003falsetrue13\n");
    assert_eq!(returned, Returned::Int(40 + 4 + 3 + 499_500 + 75));
}

/// A run with a heap limit far below what the program allocates in total:
/// allocations collect again and again while the only references to the
/// lists being built are in JIT frames (locals, arguments, temporaries).
/// Every list must survive intact.
#[test]
fn collections_do_not_free_what_jit_frames_hold() {
    let source = r"
        class Node { value: i64, pad: [i64] }
        class Holder { node: Node, rest: [Node] }
        func build(n: i64) -> Holder {
            var h = Holder { node: Node { value: 0, pad: [0, 0, 0, 0] }, rest: [] }
            var i: i64 = 1
            while i < n {
                let rest = h.rest
                rest.push(h.node)
                h = Holder { node: Node { value: i, pad: [i, i, i, i] }, rest: rest }
                i += 1
            }
            h
        }
        func total(h: Holder, n: i64) -> i64 {
            let xs: [i64] = []
            var t: i64 = 0
            for k in 0..n { xs.push(k) }
            for x in xs { t += x }
            for node in h.rest { t += node.pad[2] }
            t + h.node.value + h.node.pad[3]
        }
        public func main() -> i64 {
            var grand: i64 = 0
            for round in 0..200 {
                let h = build(100)
                grand += total(h, 50)
            }
            grand
        }
    ";
    let driver = driver(source);
    let options = Options {
        heap_limit: Some(256 * 1024),
        ..Options::default()
    };
    let program = compile_with(driver.db(), "main", options)
        .unwrap_or_else(|e| panic!("did not compile: {e}"));
    // 0..50 sums to 1225; the 99 earlier nodes' pads to 0 + .. + 98 = 4851;
    // the last node adds 99 twice.
    assert_eq!(program.run(), Returned::Int(200 * (1225 + 4851 + 99 + 99)));
    assert!(
        program.emergency_collections() > 0,
        "the heap limit never forced a collection"
    );
}

/// With precise stack maps asked for, Cranelift records the GC handles live
/// across each call; the program runs the same.
#[test]
fn precise_stack_maps_are_recorded_on_request() {
    let source = r"
        class Box { v: i64 }
        func make(v: i64) -> Box { Box { v: v } }
        public func main() -> i64 {
            let a = make(1)
            let b = make(2)
            let xs = [a, b]
            xs.push(make(3))
            a.v + b.v + xs[2].v
        }
    ";
    let driver = driver(source);
    let plain = compile(driver.db(), "main").unwrap();
    assert_eq!(plain.stack_maps(), 0);
    assert_eq!(plain.run(), Returned::Int(6));
    let precise = compile_with(
        driver.db(),
        "main",
        Options {
            precise_stack_maps: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(precise.stack_maps() > 0);
    assert_eq!(precise.run(), Returned::Int(6));
}

/// Value structs, tuples, enums and optionals are still refused, by name.
#[test]
fn value_aggregates_are_refused() {
    for (source, what) in [
        (
            "struct P { x: i64 } public func main() -> i64 { let p = P { x: 1 }\n p.x }",
            "value struct",
        ),
        (
            "public func main() -> i64 { let t = (1, 2)\n t.0 }",
            "tuple",
        ),
    ] {
        let driver = driver(source);
        let error = compile(driver.db(), "main").err().expect("refused");
        assert!(error.what.contains(what), "{error}");
    }
}
