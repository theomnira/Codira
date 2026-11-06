//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - End-to-end tests for optionals (`T?`, `nil`, `if let`, `while let`,
//!   postfix `!`, comparison with `nil`) and `match` over literals, tuples,
//!   strings and optionals.

#[macro_use]
mod util;

use codira_runtime::{capture_output, StructRef};
use codira_test::CompileAndRunTestDriver;

fn driver(source: &str) -> CompileAndRunTestDriver {
    CompileAndRunTestDriver::new(source, |builder| builder).expect("failed to build driver")
}

/// The language spec's own example (section 7): a search that returns `nil`
/// when nothing is found, consumed with `if let` and with `!`.
#[test]
fn optional_search() {
    let driver = driver(
        r#"
    func find(items: [i32], target: i32) -> i32? {
        for item in items {
            if item == target { return item }
        }
        nil
    }

    public func found() -> i32 {
        if let value = find([3, 7, 9], 7) { value } else { -1 }
    }

    public func missing() -> i32 {
        if let value = find([3, 7, 9], 8) { value } else { -1 }
    }

    public func forced() -> i32 {
        find([1, 2], 2)!
    }

    public func is_nil() -> bool {
        find([1], 5) == nil
    }

    public func is_not_nil() -> bool {
        find([1], 1) != nil
    }
    "#,
    );
    assert_invoke_eq!(i32, 7, driver, "found");
    assert_invoke_eq!(i32, -1, driver, "missing");
    assert_invoke_eq!(i32, 2, driver, "forced");
    assert_invoke_eq!(bool, true, driver, "is_nil");
    assert_invoke_eq!(bool, true, driver, "is_not_nil");
}

#[test]
fn optional_values_and_comparison() {
    let driver = driver(
        r#"
    public func wrapped_equal() -> bool {
        let a: i32? = 5
        a == 5
    }

    public func nil_equals_nil() -> bool {
        let a: i32? = nil
        let b: i32? = nil
        a == b
    }

    public func present_differs_from_nil() -> bool {
        let a: f64? = 0.0
        let b: f64? = nil
        a != b
    }

    // `while let` keeps going while the optional is present.
    public func countdown() -> i32 {
        var n: i32? = 4
        var total = 0
        while let v = n {
            total += v
            n = if v > 1 { v - 1 } else { nil }
        }
        total
    }

    public func reassigned() -> i32 {
        var a: i32? = nil
        a = 3
        a!
    }
    "#,
    );
    assert_invoke_eq!(bool, true, driver, "wrapped_equal");
    assert_invoke_eq!(bool, true, driver, "nil_equals_nil");
    assert_invoke_eq!(bool, true, driver, "present_differs_from_nil");
    assert_invoke_eq!(i32, 10, driver, "countdown");
    assert_invoke_eq!(i32, 3, driver, "reassigned");
}

#[test]
fn match_on_literals_tuples_and_strings() {
    let driver = driver(
        r#"
    public func classify(n: i32) -> i32 {
        match n {
            0 -> 100,
            1 -> 200,
            _ -> 300,
        }
    }

    public func bools(b: bool) -> i32 {
        match b {
            true -> 1,
            false -> 0,
        }
    }

    public func tuples(a: i32, b: i32) -> i32 {
        match (a, b) {
            (0, _) -> 0,
            (1, y) -> y * 10,
            (x, y) -> x + y,
        }
    }

    public func strings(n: i32) -> i32 {
        let s = if n == 0 { "zero" } else { "other" }
        match s {
            "zero" -> 1,
            "one" -> 2,
            _ -> 3,
        }
    }

    public func optionals(n: i32) -> i32 {
        let x: i32? = if n > 0 { n } else { nil }
        match x {
            nil -> -1,
            1 -> 100,
            v -> v * 2,
        }
    }
    "#,
    );
    assert_invoke_eq!(i32, 100, driver, "classify", 0i32);
    assert_invoke_eq!(i32, 200, driver, "classify", 1i32);
    assert_invoke_eq!(i32, 300, driver, "classify", 7i32);
    assert_invoke_eq!(i32, 1, driver, "bools", true);
    assert_invoke_eq!(i32, 0, driver, "bools", false);
    assert_invoke_eq!(i32, 0, driver, "tuples", 0i32, 5i32);
    assert_invoke_eq!(i32, 50, driver, "tuples", 1i32, 5i32);
    assert_invoke_eq!(i32, 9, driver, "tuples", 4i32, 5i32);
    assert_invoke_eq!(i32, 1, driver, "strings", 0i32);
    assert_invoke_eq!(i32, 3, driver, "strings", 1i32);
    assert_invoke_eq!(i32, -1, driver, "optionals", 0i32);
    assert_invoke_eq!(i32, 100, driver, "optionals", 1i32);
    assert_invoke_eq!(i32, 10, driver, "optionals", 5i32);
}

/// A `match` arm can print, return early, or loop -- it is an ordinary
/// expression.
#[test]
fn match_arms_with_effects() {
    let driver = driver(
        r#"
    public func main() {
        for i in 0..4 {
            match i % 3 {
                0 -> println(i, " fizz"),
                1 -> { if i > 3 { continue } println(i, " one") },
                _ -> println(i, " other"),
            }
        }
    }
    "#,
    );
    let ((), output) = capture_output(|| driver.runtime.invoke("main", ()).unwrap());
    assert_eq!(output, "0 fizz\n1 one\n2 other\n3 fizz\n");
}

/// An optional reference is a value holding a pointer: a class can link to
/// another through a `T?` field, and the collector traces through it -- a
/// list built in Codira survives a collection while the host holds its head.
#[test]
fn optional_references_are_traced() {
    let driver = driver(
        r#"
    public class Node {
        value: i32,
        next: Node?,
    }

    public func build(n: i32) -> Node {
        var head: Node? = nil
        var i = 1
        while i <= n {
            head = Node { value: i, next: head }
            i += 1
        }
        head!
    }

    public func sum(list: Node) -> i32 {
        var total = 0
        var current: Node? = list
        while let node = current {
            total += node.value
            current = node.next
        }
        total
    }

    public func firsts(list: Node) -> i32 {
        let all: [Node?] = [nil, list, list.next]
        var total = 0
        for item in all {
            if let node = item { total += node.value }
        }
        total
    }
    "#,
    );
    let runtime = &driver.runtime;
    let list: StructRef<'_> = runtime.invoke("build", (4i32,)).unwrap();
    let list = list.root();
    runtime.collect_blocking().unwrap();
    let list = list.as_ref(runtime);
    assert_invoke_eq!(i32, 10, driver, "sum", list.clone());
    assert_invoke_eq!(i32, 7, driver, "firsts", list);
}
