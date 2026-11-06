//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Optionals on the Cranelift JIT: a `T` where a `T?` is expected, `nil`,
//!   postfix `!`, `==`/`!=`, and optionals in fields, arrays, parameters
//!   and results (a linked list through `Node?`, under a heap limit).

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

#[test]
fn optionals_wrap_compare_and_unwrap() {
    let source = r#"
        class Node { value: i64, next: Node? }
        func find(xs: [i64], target: i64) -> i64? {
            for x in xs { if x == target { return x } }
            nil
        }
        func half(x: f64?) -> f64? { if x == nil { nil } else { x! / 2.0 } }
        public func main() -> i64 {
            var list: Node? = nil
            for i in 0..1000 { list = Node { value: i, next: list } }
            var total: i64 = 0
            var at = list
            while at != nil {
                total += at!.value
                at = at!.next
            }
            let xs: [i64] = [3, 1, 4]
            let found = find(xs, 4)
            let missing = find(xs, 9)
            let opts: [bool?] = [true, nil]
            opts.push(false)
            println(found == 4, " ", missing == nil, " ", missing != found, " ",
                opts[1] == nil, " ", opts[2] == false, " ", half(3.0)!, " ", half(nil) == nil)
            total + found!
        }
    "#;
    let driver = driver(source);
    let program = compile_with(
        driver.db(),
        "main",
        Options {
            heap_limit: Some(256 * 1024),
            ..Options::default()
        },
    )
    .unwrap_or_else(|e| panic!("did not compile: {e}"));
    let (returned, printed) = codira_runtime::capture_output(|| program.run());
    assert_eq!(printed, "true true true true true 1.5 true\n");
    assert_eq!(returned, Returned::Int(499_500 + 4));
}

/// `if let`, `match` and `?` are not compiled: refused, by name.
#[test]
fn pattern_matching_on_optionals_is_refused() {
    let source = r"
        public func main() -> i64 {
            let x: i64? = 5
            if let v = x { v } else { 0 }
        }
    ";
    let driver = driver(source);
    let error = compile(driver.db(), "main").err().expect("refused");
    assert!(error.what.contains("match"), "{error}");
}
