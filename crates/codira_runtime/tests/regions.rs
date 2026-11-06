//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of `@region` functions: the same program with and
//!   without the attribute returns the same result, with frame allocation off
//!   and on; objects kept past their loop iteration, arrays whose length is
//!   known only at run time and long array literals come from the region;
//!   heap objects referred to only from region objects survive the
//!   collections the heap limit forces; every region is closed and its
//!   chunks released when the function returns, also through early
//!   returns, `?` and recursion.
//! - Region checks are on in debug builds (as these tests run): every close
//!   also stops the world and verifies nothing refers into the region.

use codira_test::CompileAndRunTestDriver;

const BLOCK: usize = 64 * 1024;

const PROGRAM: &str = r#"
    class Node { var value: i64, var next: Node? }
    class Box { var item: Node }

    // A heap object: the result escapes.
    func node(v: i64) -> Node { Node { value: v, next: nil } }

    // Allocates `n` heap objects that die at once, so the heap limit forces
    // collections while the caller's region objects hold heap objects.
    func churn(n: i64) -> i64 {
        var total = 0
        var i = 0
        while i < n {
            let g = node(i)
            total += g.value
            i += 1
        }
        total
    }

    ANNOTATION
    public func chain(n: i64, churned: i64) -> i64 {
        // Each iteration's box is kept past the iteration: region objects.
        var last = Box { item: node(0) }
        var total = 0
        for i in 0..n {
            let b = Box { item: node(i) }
            if i % 3 == 0 { last = b }
            total += b.item.value
        }
        let xs = array_filled(2, n as usize)
        let ys = xs.map(func(x) { x * 3 })
        let long = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]
        let c = churn(churned)
        total + last.item.value + ys[0] + ys.len() as i64 + long[17] + c
    }

    ANNOTATION
    public func early(n: i64) -> i64 {
        var keep = Box { item: node(1) }
        for i in 0..n {
            let b = Box { item: node(i) }
            keep = b
            if i == 7 { return keep.item.value * 100 }
        }
        keep.item.value
    }

    ANNOTATION
    public func recursive(depth: i64) -> i64 {
        var keep = Box { item: node(depth) }
        for i in 0..3 {
            let b = Box { item: node(i + depth) }
            keep = b
        }
        if depth == 0 { keep.item.value } else { keep.item.value + recursive(depth - 1) }
    }

    ANNOTATION
    public func repeated(calls: i64) -> i64 {
        var total = 0
        for i in 0..calls {
            let xs = array_filled(i, 64)
            total += xs[63]
        }
        total
    }
"#;

fn program(region: bool) -> String {
    PROGRAM.replace("ANNOTATION", if region { "@region" } else { "" })
}

fn driver(region: bool, frame: bool) -> CompileAndRunTestDriver {
    CompileAndRunTestDriver::with_frame_allocation(&program(region), frame, |b| {
        b.heap_limit(8 * BLOCK)
    })
    .expect("Failed to build test driver")
}

#[test]
fn region_functions_return_what_plain_functions_return() {
    let reference = driver(false, false);
    let expected: Vec<i64> = vec![
        reference
            .runtime
            .invoke("chain", (500i64, 60_000i64))
            .unwrap(),
        reference.runtime.invoke("early", (20i64,)).unwrap(),
        reference.runtime.invoke("recursive", (10i64,)).unwrap(),
        reference.runtime.invoke("repeated", (100i64,)).unwrap(),
    ];
    for frame in [false, true] {
        let regions = driver(true, frame);
        let runtime = &regions.runtime;
        assert!(runtime.gc().region_checks(), "tests run with region checks");
        let got: Vec<i64> = vec![
            runtime.invoke("chain", (500i64, 60_000i64)).unwrap(),
            runtime.invoke("early", (20i64,)).unwrap(),
            runtime.invoke("recursive", (10i64,)).unwrap(),
            runtime.invoke("repeated", (100i64,)).unwrap(),
        ];
        assert_eq!(got, expected, "frame allocation {frame}");
        let stats = runtime.region_stats();
        // chain, early, 11 recursive calls, repeated.
        assert_eq!(stats.opened, 14, "{stats:?}");
        assert_eq!(
            stats.closed, stats.opened,
            "a region was not closed: {stats:?}"
        );
        assert_eq!(stats.live_chunks, 0, "{stats:?}");
        assert!(stats.objects_allocated >= 500, "{stats:?}");
        assert!(
            runtime.stats().collections.total() > 0,
            "nothing collected while regions were open"
        );
    }
}

#[test]
fn region_objects_are_not_heap_allocations() {
    let plain = driver(false, false);
    let regions = driver(true, false);
    let count = |d: &CompileAndRunTestDriver| {
        let before = d.runtime.stats().objects_allocated;
        let _: i64 = d.runtime.invoke("repeated", (1000i64,)).unwrap();
        d.runtime.stats().objects_allocated - before
    };
    assert_eq!(count(&plain), 1000);
    assert_eq!(count(&regions), 0);
    let stats = regions.runtime.region_stats();
    assert_eq!(stats.objects_allocated, 1000);
    // Each call's array was freed when it returned.
    assert_eq!(stats.live_chunk_bytes, 0);
}
