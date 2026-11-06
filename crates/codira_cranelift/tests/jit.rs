//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Compiles Codira programs through the real front end, runs them on the
//!   Cranelift JIT, and checks what they return.

use std::time::Instant;

use codira_compiler::{Config, Driver, PathOrInline, RelativePathBuf};
use codira_cranelift::{compile, Returned};

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

fn run(source: &str) -> Returned {
    let driver = driver(source);
    compile(driver.db(), "main")
        .unwrap_or_else(|e| panic!("did not compile: {e}"))
        .run()
}

#[test]
fn arithmetic_loops_and_recursion() {
    let source = r"
        func fib(n: i64) -> i64 {
            if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
        }
        func sum_to(n: i64) -> i64 {
            var total: i64 = 0
            var i: i64 = 1
            while i <= n {
                total += i
                i += 1
            }
            total
        }
        public func main() -> i64 {
            fib(20) * 1000000 + sum_to(100)
        }
    ";
    assert_eq!(run(source), Returned::Int(6765 * 1_000_000 + 5050));
}

#[test]
fn loop_with_break_value_and_early_return() {
    let source = r"
        func first_square_above(limit: i32) -> i32 {
            var i: i32 = 0
            loop {
                i += 1
                if i * i > limit {
                    break i * i
                }
            }
        }
        func clamp(x: i32) -> i32 {
            if x > 100 {
                return 100
            }
            x
        }
        public func main() -> i32 {
            first_square_above(50) + clamp(250) + clamp(7)
        }
    ";
    assert_eq!(run(source), Returned::Int(64 + 100 + 7));
}

#[test]
fn floats_casts_and_logic() {
    let source = r"
        func area(r: f64) -> f64 { 3.0 * r * r }
        public func main() -> i32 {
            let a = area(2.0)
            let wide = 300 as i64
            let narrow = wide as u8
            let big = 1.0e20 as i32
            let ok = a > 11.0 && a < 13.0 || false
            if ok && narrow == 44 && big == 2147483647 && !(wide < 0) {
                1
            } else {
                0
            }
        }
    ";
    assert_eq!(run(source), Returned::Int(1));
}

#[test]
fn unsigned_arithmetic_is_unsigned() {
    let source = r"
        public func main() -> u32 {
            let x: u32 = 4000000000
            x / 2 + (x % 7)
        }
    ";
    assert_eq!(
        run(source),
        Returned::Int(2_000_000_000 + 4_000_000_000 % 7)
    );
}

#[test]
fn unsupported_constructs_are_named_not_miscompiled() {
    let source = r"
        struct Point { x: i64, y: i64 }
        public func main() -> i64 {
            let p = Point { x: 1, y: 2 }
            p.x
        }
    ";
    let driver = driver(source);
    let error = compile(driver.db(), "main")
        .err()
        .expect("structs are not supported");
    assert_eq!(error.function, "main");
}

/// Enums have no representation in this backend, and matching on an
/// optional is not compiled: a function that does either is refused,
/// naming what it met.
#[test]
fn enums_and_optionals_are_refused() {
    let enums = r"
        enum Shape { Circle(f64) Point }
        public func main() -> i64 {
            match Shape.Point {
                Shape.Circle(_) -> 1,
                Shape.Point -> 2,
            }
        }
    ";
    let optionals = r"
        public func main() -> i64 {
            let x: i64? = 5
            if let v = x { v } else { 0 }
        }
    ";
    for source in [enums, optionals] {
        let driver = driver(source);
        let error = compile(driver.db(), "main")
            .err()
            .expect("enums and `if let` are not supported");
        assert_eq!(error.function, "main");
    }
}

/// A builtin other than `print`/`println` is refused, never compiled as a
/// print. `ct_select` used to be, which produced a program that printed its
/// operands instead of choosing between them.
#[test]
fn builtins_other_than_print_are_refused() {
    let source = r"
        public func main() -> i64 {
            ct_select(true, 1, 2)
        }
    ";
    let driver = driver(source);
    let error = compile(driver.db(), "main")
        .err()
        .expect("ct_select is not compiled by this backend");
    assert!(error.what.contains("ct_select"), "{error}");
}

/// Not an assertion: how long compiling a small program takes on this
/// backend, minimum and median of 25 runs, for comparison with the LLVM
/// path. The machine is shared and loaded, so only the minimum means much.
#[test]
fn compile_time_is_reported() {
    let source = r"
        func fib(n: i64) -> i64 {
            if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
        }
        func collatz(n: i64) -> i64 {
            var x = n
            var steps: i64 = 0
            while x != 1 {
                if x % 2 == 0 { x = x / 2 } else { x = 3 * x + 1 }
                steps += 1
            }
            steps
        }
        public func main() -> i64 { fib(10) + collatz(27) }
    ";
    let driver = driver(source);
    // Front end first, so the measurement is the backend alone.
    let _ = compile(driver.db(), "main").unwrap();
    let mut samples: Vec<f64> = (0..25)
        .map(|_| {
            let start = Instant::now();
            let program = compile(driver.db(), "main").unwrap();
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(program.run(), Returned::Int(55 + 111));
            elapsed
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    eprintln!(
        "cranelift backend, 3 functions: min {:.3} ms, median {:.3} ms",
        samples[0], samples[12]
    );
}

/// A `@region` function, a task `group` and a byte-buffer method have no
/// lowering in this backend: each is refused, naming what it met, so the
/// program runs on LLVM with its regions, groups and buffers intact.
#[test]
fn regions_groups_and_byte_buffers_are_refused() {
    for (source, what) in [
        (
            r"
            class Box { v: i64 }
            @region
            public func main() -> i64 {
                var last = Box { v: 0 }
                for i in 0..10 {
                    let b = Box { v: i }
                    if i % 3 == 0 { last = b }
                }
                last.v
            }
            ",
            "`@region`",
        ),
        (
            r"
            func square(i: i64) -> i64 { i * i }
            public func main() -> i64 {
                let xs = group { spawn square(3) }
                xs[0]
            }
            ",
            "`group`",
        ),
        (
            r"
            public func main() -> i64 {
                let buf: [u8] = []
                buf.push_json_float(1.5)
                buf.len() as i64
            }
            ",
            "array method",
        ),
    ] {
        let driver = driver(source);
        let error = compile(driver.db(), "main")
            .err()
            .unwrap_or_else(|| panic!("compiled what should be refused: {what}"));
        assert!(error.what.contains(what), "{what}: {error}");
    }
}
