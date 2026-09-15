//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::fmt;

use codira_hir_input::WithFixture;

use crate::{mock::MockDatabase, DefDatabase, DiagnosticSink};

fn print_item_tree(text: &str) -> Result<String, fmt::Error> {
    let (db, file_id) = MockDatabase::with_single_file(text);
    let item_tree = db.item_tree(file_id);
    let mut result_str = super::pretty::print_item_tree(&db, &item_tree)?;
    let mut sink = DiagnosticSink::new(|diag| {
        result_str.push_str(&format!(
            "\n{:?}: {}",
            diag.highlight_range(),
            diag.message()
        ));
    });

    item_tree
        .diagnostics
        .iter()
        .for_each(|diag| diag.add_to(&db, &item_tree, &mut sink));

    drop(sink);
    Ok(result_str)
}

#[test]
fn top_level_items() {
    insta::assert_snapshot!(print_item_tree(
        r#"
    func foo(a:i32, b:u8, c:String) -> i32 {}
    public func bar(a:i32, b:u8, c:String) ->  {}
    internal func bar(a:i32, b:u8, c:String) ->  {}
    internal func baz(a:i32, b:, c:String) ->  {}
    extern func eval(a:String) -> bool;

    struct Foo {
        a: i32,
        b: u8,
        c: String,
    }
    struct Foo2 {
        a: i32,
        b: ,
        c: String,
    }
    struct Bar (i32, u32, String)
    struct Baz;

    type FooBar = Foo;
    type FooBar = root.Foo;
    "#
    )
    .unwrap());
}

#[test]
fn test_use() {
    insta::assert_snapshot!(print_item_tree(
        r#"
    public import foo
    import super.bar
    import super.*
    import foo.{bar as _, baz.hello as world}
        "#
    )
    .unwrap());
}

#[test]
fn test_impls() {
    insta::assert_snapshot!(print_item_tree(
        r#"
    extend Bar {
        func foo(a:i32, b:u8, c:String) -> i32 {}
        public func bar(a:i32, b:u8, c:String) ->  {}
    }
    "#
    )
    .unwrap());
}

#[test]
fn test_generic_params() {
    insta::assert_snapshot!(print_item_tree(
        r#"
    struct Box[T] {
        value: T,
    }
    struct SIMD[T, N: usize] {
        data: T,
    }
    func first[T](items: T) -> T {}
    "#
    )
    .unwrap());
}

#[test]
fn test_data_struct() {
    insta::assert_snapshot!(print_item_tree(
        r#"
    data struct Point {
        x: f64,
        y: f64,
    }
    struct Plain {
        x: f64,
    }
    data struct Pair[T] {
        first: T,
        second: T,
    }
    "#
    )
    .unwrap());
}

#[test]
fn test_effects() {
    insta::assert_snapshot!(print_item_tree(
        r#"
    effect Logger {
        func log(message: String)
    }
    func risky() uses Logger -> i32 {
        42
    }
    func pure_fn() -> i32 {
        1
    }
    "#
    )
    .unwrap());
}

#[test]
fn test_duplicate_import() {
    insta::assert_snapshot!(print_item_tree(
        r#"
    import foo.Bar
    import baz.Bar

    struct Bar {}
    "#
    )
    .unwrap());
}

// ---- module-level bindings and cycle detection --------------------------

#[test]
fn module_level_bindings() {
    // The shape the standard library actually uses for its constants.
    insta::assert_snapshot!(print_item_tree(
        r#"
    public let RELAXED: i32 = 0;
    let DERIVED: i32 = RELAXED;
    let COMPUTED: i64 = 1 + 2;
    "#,
    )
    .unwrap());
}

#[test]
fn direct_const_cycle_is_rejected() {
    // `A -> B -> A`. Tarjan finds one strongly-connected component of two
    // vertices; both are reported, because either is a valid place to
    // break the cycle.
    insta::assert_snapshot!(print_item_tree(
        r#"
    let A: i32 = B;
    let B: i32 = A;
    "#,
    )
    .unwrap());
}

#[test]
fn self_referential_const_is_rejected() {
    // A one-vertex component is only a cycle when the vertex has an edge
    // to itself -- the case a naive `component.len() > 1` check misses.
    insta::assert_snapshot!(print_item_tree(
        r#"
    let LOOP: i32 = LOOP;
    "#,
    )
    .unwrap());
}

#[test]
fn longer_const_cycle_is_rejected() {
    // `A -> B -> C -> A`, with an acyclic binding alongside it to confirm
    // only the participants are flagged.
    insta::assert_snapshot!(print_item_tree(
        r#"
    let A: i32 = B;
    let B: i32 = C;
    let C: i32 = A;
    let FINE: i32 = 7;
    "#,
    )
    .unwrap());
}

#[test]
fn acyclic_chain_is_accepted() {
    // A deep chain is not a cycle, however long. This also exercises the
    // iterative DFS: the recursive formulation would grow its stack with
    // the chain length.
    insta::assert_snapshot!(print_item_tree(
        r#"
    let A: i32 = 1;
    let B: i32 = A;
    let C: i32 = B;
    let D: i32 = C;
    let E: i32 = D;
    "#,
    )
    .unwrap());
}

#[test]
fn diamond_dependency_is_not_a_cycle() {
    // `D` depends on `B` and `C`, both of which depend on `A`. A vertex
    // reachable by two paths is revisited during the DFS; a detector that
    // confused "already seen" with "on the current stack" would call this
    // a cycle.
    insta::assert_snapshot!(print_item_tree(
        r#"
    let A: i32 = 1;
    let B: i32 = A;
    let C: i32 = A;
    let D: i32 = B + C;
    "#,
    )
    .unwrap());
}

#[test]
fn reference_to_a_function_is_not_a_dependency_edge() {
    // `collect_path_references` over-approximates: it records every
    // single-segment name. `helper` is a function, not a binding, so it is
    // not a vertex and contributes no edge -- and cannot create a cycle.
    insta::assert_snapshot!(print_item_tree(
        r#"
    func helper() -> i32 { 1 }
    let VALUE: i32 = helper();
    "#,
    )
    .unwrap());
}
