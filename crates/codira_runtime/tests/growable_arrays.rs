//! Copyright (c) 2026 Omnira CJSC
//! Date: November 5, 2026
//!
//! Functionality:
//! - Growable arrays and the runtime: growth under a heap limit that forces
//!   collections in the middle of it keeps every element traced, the host
//!   reads a grown array through `ArrayRef`, and a grown array survives a heap
//!   snapshot and a hot reload.
use codira_runtime::{ArrayRef, StructRef};
use codira_test::CompileAndRunTestDriver;

const BLOCK: usize = 64 * 1024;

const PROGRAM: &str = r#"
    public class Node {
        value: i64,
        twice: i64,
    }

    public class Bag {
        nodes: [Node],
        numbers: [i64],
    }

    public func fill(n: i64) -> [Node] {
        let nodes: [Node] = []
        for i in 0..n {
            // Garbage between the pushes, so the heap limit forces
            // collections while `nodes` is being grown.
            var junk = Node { value: -1, twice: -2 }
            for _ in 0..4 { junk = Node { value: -1, twice: junk.twice } }
            nodes.push(Node { value: i, twice: 2 * i + junk.value + 1 })
        }
        nodes
    }

    public func check(nodes: [Node]) -> i64 {
        var bad = 0
        for (i, node) in nodes.enumerate() {
            if node.value != i as i64 || node.twice != 2 * node.value { bad += 1 }
        }
        bad
    }

    public func numbers(n: i64) -> [i64] {
        let xs: [i64] = []
        for i in 0..n { xs.push(i * 3) }
        xs
    }

    public func bag(n: i64) -> Bag {
        Bag { nodes: fill(n), numbers: numbers(n) }
    }

    public func bag_total(b: Bag) -> i64 {
        b.numbers.sum() + b.nodes.fold(0, func(acc, node) { acc + node.value })
    }
"#;

#[test]
fn growth_under_a_heap_limit_keeps_every_element() {
    let driver = CompileAndRunTestDriver::new(PROGRAM, |builder| builder.heap_limit(4 * BLOCK))
        .expect("Failed to build test driver");
    let n = 2_000i64;
    let nodes: ArrayRef<'_, StructRef<'_>> = driver.runtime.invoke("fill", (n,)).unwrap();
    assert!(
        driver.runtime.gc().emergency_collections() > 0,
        "the cap was never reached"
    );
    assert_eq!(nodes.len(), n as usize);
    let nodes = nodes.root();
    driver.runtime.collect_blocking().unwrap();
    let nodes = nodes.as_ref(&driver.runtime);
    let bad: i64 = driver.runtime.invoke("check", (nodes.clone(),)).unwrap();
    assert_eq!(bad, 0);
    let values: Vec<i64> = nodes
        .iter()
        .map(|n| n.get::<i64>("value").unwrap())
        .collect();
    assert_eq!(values, (0..n).collect::<Vec<_>>());
}

#[test]
fn the_host_reads_a_grown_array() {
    let driver = CompileAndRunTestDriver::new(PROGRAM, |builder| builder)
        .expect("Failed to build test driver");
    let xs: ArrayRef<'_, i64> = driver.runtime.invoke("numbers", (1000i64,)).unwrap();
    assert_eq!(xs.len(), 1000);
    assert!(xs.capacity() >= 1000);
    assert_eq!(
        xs.iter().collect::<Vec<_>>(),
        (0..1000).map(|i| i * 3).collect::<Vec<_>>()
    );
}

#[test]
fn a_grown_array_survives_a_heap_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bag.heap");
    let n = 300i64;
    let expected = 3 * n * (n - 1) / 2 + n * (n - 1) / 2;
    {
        let driver = CompileAndRunTestDriver::new(PROGRAM, |builder| builder)
            .expect("Failed to build test driver");
        let bag: StructRef<'_> = driver.runtime.invoke("bag", (n,)).unwrap();
        let total: i64 = driver.runtime.invoke("bag_total", (bag.clone(),)).unwrap();
        assert_eq!(total, expected);
        let bag = bag.root();
        driver.runtime.save_heap(&path).unwrap();
        drop(bag);
    }
    let driver = CompileAndRunTestDriver::new(PROGRAM, |builder| builder)
        .expect("Failed to build test driver");
    let roots = driver.runtime.restore_heap(&path).unwrap();
    let bag = roots[0].as_ref(&driver.runtime);
    let total: i64 = driver.runtime.invoke("bag_total", (bag.clone(),)).unwrap();
    assert_eq!(total, expected);
    let numbers = bag.get::<ArrayRef<'_, i64>>("numbers").unwrap();
    assert_eq!(numbers.len(), n as usize);
}

#[test]
fn a_grown_array_survives_a_hot_reload() {
    let mut driver = CompileAndRunTestDriver::new(
        r#"
    public class Item { id: i64 }
    public class Holder { items: [Item] }
    public func make(n: i64) -> Holder {
        let items: [Item] = []
        for i in 0..n { items.push(Item { id: i }) }
        Holder { items: items }
    }
    public func total(h: Holder) -> i64 { h.items.fold(0, func(a, it) { a + it.id }) }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");
    let holder: StructRef<'_> = driver.runtime.invoke("make", (100i64,)).unwrap();
    let holder = holder.root();

    // `Item` gains a field: every element of the grown array is migrated.
    driver.update_file(
        "mod.code",
        r#"
    public class Item { id: i64, weight: i64 }
    public class Holder { items: [Item] }
    public func make(n: i64) -> Holder {
        let items: [Item] = []
        for i in 0..n { items.push(Item { id: i, weight: 1 }) }
        Holder { items: items }
    }
    public func total(h: Holder) -> i64 {
        h.items.fold(0, func(a, it) { a + it.id + it.weight })
    }
    public func grow(h: Holder) -> i64 {
        h.items.push(Item { id: 1000, weight: 5 })
        h.items.len() as i64
    }
    "#,
    );
    let holder = holder.as_ref(&driver.runtime);
    let items = holder.get::<ArrayRef<'_, StructRef<'_>>>("items").unwrap();
    assert_eq!(items.len(), 100);
    let total: i64 = driver.runtime.invoke("total", (holder.clone(),)).unwrap();
    // The new field starts at zero in migrated objects.
    assert_eq!(total, 99 * 100 / 2);
    let len: i64 = driver.runtime.invoke("grow", (holder.clone(),)).unwrap();
    assert_eq!(len, 101);
    let total: i64 = driver.runtime.invoke("total", (holder,)).unwrap();
    assert_eq!(total, 99 * 100 / 2 + 1005);
}
