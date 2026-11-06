//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - GC stress tests of frame allocation (`--frame-alloc`): objects in a
//!   stack frame whose fields are the only references to heap objects, while
//!   the heap limit forces collections on this thread and on task threads.
//!   If the collector missed a reference held by a frame object, the heap
//!   object's cell would be swept and reused, and the values read back would
//!   change.
//! - Every program runs with frame allocation off and on and must give the
//!   same result.

use codira_test::CompileAndRunTestDriver;

const BLOCK: usize = 64 * 1024;

const STRESS: &str = r#"
    class Node { value: i64, twice: i64 }
    class Pair { var left: Node, var right: Node }
    class Outer { var pair: Pair, var extra: Node }

    // Escapes: the result is a heap object.
    func node(v: i64) -> Node { Node { value: v, twice: v * 2 } }

    // Allocates `n` heap objects that die at once, so the heap limit forces
    // collections.
    func churn(n: i64) -> i64 {
        var total = 0
        var i = 0
        while i < n {
            let garbage = node(i)
            total += garbage.twice - garbage.value
            i += 1
        }
        total
    }

    public func frame_holds_heap(n: i64) -> i64 {
        // `pair` and `outer` do not escape and go to the frame; the nodes are
        // heap objects (`node` returns them) referenced only from the
        // frame objects' fields.
        let pair = Pair { left: node(11), right: node(22) }
        let outer = Outer { pair: pair, extra: node(33) }
        let refs = [node(44), node(55), node(66)]
        let churned = churn(n)
        // Replace a field mid-way: the old node becomes garbage, the new one
        // is held only by the frame object.
        pair.left = node(77)
        let churned_again = churn(n)
        churned + churned_again
            + outer.pair.left.value + outer.pair.right.twice + outer.extra.value
            + refs[0].value + refs[1].twice + refs[2].value
    }

    public func in_a_loop(n: i64, rounds: i64) -> i64 {
        var total = 0
        for r in 0..rounds {
            // A fresh frame object per iteration, in one reused slot.
            let pair = Pair { left: node(r), right: node(r + 1) }
            total += churn(n)
            total += pair.left.value * 10 + pair.right.value
        }
        total
    }

    func work(n: i64) -> i64 { churn(n) }

    public func with_tasks(n: i64) -> i64 {
        let pair = Pair { left: node(5), right: node(6) }
        var a = 0
        var b = 0
        scope {
            spawn work(n)
            spawn work(n)
            a = churn(n)
        }
        b = churn(n)
        a + b + pair.left.value + pair.right.twice
    }
"#;

fn churn(n: i64) -> i64 {
    n * (n - 1) / 2
}

fn run_both<F: Fn(&CompileAndRunTestDriver) -> i64>(heap_limit: usize, f: F) -> i64 {
    let mut results = Vec::new();
    for frame in [false, true] {
        let driver = CompileAndRunTestDriver::with_frame_allocation(STRESS, frame, |builder| {
            builder.heap_limit(heap_limit)
        })
        .expect("Failed to build test driver");
        results.push(f(&driver));
        assert!(
            driver.runtime.gc().emergency_collections() > 0,
            "the heap limit was never reached (frame allocation {frame})"
        );
    }
    assert_eq!(
        results[0], results[1],
        "frame allocation changed the result"
    );
    results[0]
}

#[test]
fn frame_objects_keep_heap_objects_alive_across_collections() {
    let n = 100_000;
    let total = run_both(4 * BLOCK, |driver| {
        driver.runtime.invoke("frame_holds_heap", (n,)).unwrap()
    });
    assert_eq!(total, 2 * churn(n) + 77 + 44 + 33 + 44 + 110 + 66);
}

#[test]
fn a_reused_frame_slot_keeps_each_iterations_objects_alive() {
    let (n, rounds) = (20_000, 10);
    let total = run_both(4 * BLOCK, |driver| {
        driver.runtime.invoke("in_a_loop", (n, rounds)).unwrap()
    });
    let expected: i64 = (0..rounds).map(|r| churn(n) + r * 10 + r + 1).sum();
    assert_eq!(total, expected);
}

#[test]
fn collections_on_task_threads_see_frame_objects() {
    let n = 50_000;
    let total = run_both(4 * BLOCK, |driver| {
        driver.runtime.invoke("with_tasks", (n,)).unwrap()
    });
    assert_eq!(total, 2 * churn(n) + 5 + 12);
}
