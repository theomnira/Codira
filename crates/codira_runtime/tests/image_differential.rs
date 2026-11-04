//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 15, 2026
//!
//! Functionality:
//! - Differential test of the direct image writer (`codira_image`) against LLD,
//!   end to end: every program below is built twice, once linked by LLD and
//!   once by the image writer (with the byte-for-byte cross-check against LLD
//!   switched on), loaded through the runtime, and given the same invocations.
//!   The results must be the same, and the counters must show that the image
//!   writer really did write the second library -- the test fails rather than
//!   skips when it declines.
//! - Every other test in the workspace that builds a `.codiralib` is also a
//!   differential test while debug assertions are on, because the cross-check
//!   is then the default (`LINK_VERIFY_ENV`).
//!
//! The environment variables that pick the linker are process-wide, so this
//! file has a single test.
use codira_compiler::{
    image_link_stats, DIRECT_ELF_ENV, DIRECT_LINK_ENV, LINK_MODE_ENV, LINK_VERIFY_ENV,
};
use codira_test::CompileAndRunTestDriver;

type Calls = fn(&CompileAndRunTestDriver) -> Vec<String>;

/// Invokes `$name` with no arguments, or with one `$arg`, and describes the
/// call and its result.
macro_rules! call {
    ($driver:expr, $ty:ty, $name:expr) => {{
        let result: $ty = $driver.runtime.invoke($name, ()).unwrap();
        format!("{}() = {:?}", $name, result)
    }};
    ($driver:expr, $ty:ty, $name:expr, $arg:expr) => {{
        let result: $ty = $driver.runtime.invoke($name, ($arg,)).unwrap();
        format!("{}({}) = {:?}", $name, $arg, result)
    }};
}

const README: &str = r"
func clamp(x: i64, lo: i64, hi: i64) -> i64
    requires lo <= hi
    ensures lo <= result && result <= hi
{
    if x < lo { lo } else if x > hi { hi } else { x }
}

type Speed = f64 unit(m / s);

func speed(d: f64 unit(m), t: f64 unit(s)) -> Speed {
    d / t
}

func square(jobs: Channel[i64], results: Channel[i64]) {
    let x = <-jobs
    results <- x * x
}

public func sum_of_squares(n: i64) -> i64 {
    scope {
        let jobs = channel(64)
        let results = channel(64)
        for _ in 0..n { spawn square(jobs, results) }
        for i in 0..n { jobs <- i }
        var sum = 0
        for _ in 0..n { sum += <-results }
        sum
    }
}

@realtime
func triangle(n: i64) -> i64 {
    var total = 0
    for i in 0..n { total += i }
    total
}

public func main() -> i64 {
    let v = speed(100.0, 8.0)
    clamp(sum_of_squares(10) + triangle(4), 0, 1000)
}
";

const FLOATS: &str = r"
struct P { x: f64, y: f64 }
class C { var n: i64 }
func dist(a: P, b: P) -> f64 { (a.x - b.x) * 2.5 + (a.y - b.y) * 3.14159 }
func inc(c: C) { c.n += 1 }
public func fl(x: f64) -> f64 { x * 1.5 + 2.25 }
public func arr() -> i64 { let a = [1, 2, 3]; a[0] + a[2] }
public func bump() -> i64 { let c = C { n: 41 }; inc(c); c.n }
public func main() -> f64 { dist(P { x: 1.0, y: 2.0 }, P { x: 3.0, y: 4.0 }) }
";

const FIBONACCI: &str = r"
public func fibonacci(n: i64) -> i64 {
    if n < 2 {
        n
    } else {
        fibonacci(n - 1) + fibonacci(n - 2)
    }
}

public func sum_to(n: i64) -> i64 {
    var total = 0;
    var i = 0;
    while i <= n {
        total += i;
        i += 1;
    }
    total
}
";

const EFFECTS: &str = r"
effect Ask {
    func ask(hint: i32) -> i32
}

func compute(x: i32) uses Ask -> i32 {
    let a = perform Ask.ask(x);
    let b = perform Ask.ask(a);
    a + b * 10
}

public func main() -> i32 {
    handle compute(2) {
        Ask.ask(hint) -> resume(hint + 1)
    }
}
";

const TASKS: &str = r"
func produce(dest: Channel[i64], from: i64, count: i64) {
    var i = 0;
    while i < count {
        dest <- from + i;
        i += 1;
    }
}

public func producers() -> i64 {
    scope {
        let ch = channel(8);
        spawn produce(ch, 0, 100);
        spawn produce(ch, 1000, 100);
        var total = 0;
        var n = 0;
        while n < 200 {
            total += <-ch;
            n += 1;
        };
        total
    }
}
";

const GENERICS: &str = r"
struct Point { x: i64, y: i64 }
class Counter { n: i64 }

func id[T](x: T) -> T { x }

func pick[T](a: T, b: T, first: bool) -> T {
    if first { a } else { b }
}

public func ints() -> i64 { id(40) + pick(1, 2, false) }

public func floats() -> f64 { pick(1.5, id(2.25), false) }

public func points() -> i64 {
    let p = pick(Point { x: 1, y: 2 }, Point { x: 3, y: 4 }, true);
    id(p).x * 10 + p.y
}

public func counters() -> i64 {
    let a = Counter { n: 5 };
    let b = pick(a, Counter { n: 9 }, true);
    b.n = 7;
    a.n
}
";

const MEMORY: &str = r#"
extern "codira-intrinsic" {
    func stack_alloc(size: usize, align: usize) -> usize;
    func load_u64(addr: usize) -> u64;
    func store_u64(addr: usize, value: u64);
}

public func stack_sum(n: u64) -> u64 {
    let buffer = stack_alloc(512, 64)
    var i = 0
    while i < 64 {
        store_u64(buffer + (i as usize) * 8, 0)
        i += 1
    }
    var k: u64 = 0
    while k < n {
        let slot = buffer + ((k % 64) as usize) * 8
        store_u64(slot, load_u64(slot) + k)
        k += 1
    }
    var total: u64 = 0
    var j = 0
    while j < 64 {
        total += load_u64(buffer + (j as usize) * 8)
        j += 1
    }
    total
}
"#;

const PROGRAMS: &[(&str, &str, Calls)] = &[
    ("README example", README, |d| {
        vec![call!(d, i64, "main"), call!(d, i64, "sum_of_squares", 6i64)]
    }),
    ("floats, structs, classes, arrays", FLOATS, |d| {
        vec![
            call!(d, f64, "main"),
            call!(d, i64, "arr"),
            call!(d, i64, "bump"),
            call!(d, f64, "fl", 2.0f64),
        ]
    }),
    ("recursion and loops", FIBONACCI, |d| {
        vec![
            call!(d, i64, "fibonacci", 15i64),
            call!(d, i64, "sum_to", 1000i64),
        ]
    }),
    ("algebraic effects", EFFECTS, |d| {
        vec![call!(d, i32, "main")]
    }),
    ("structured concurrency", TASKS, |d| {
        vec![call!(d, i64, "producers")]
    }),
    ("generics", GENERICS, |d| {
        vec![
            call!(d, i64, "ints"),
            call!(d, f64, "floats"),
            call!(d, i64, "points"),
            call!(d, i64, "counters"),
        ]
    }),
    ("machine-level memory intrinsics", MEMORY, |d| {
        vec![call!(d, u64, "stack_sum", 1000u64)]
    }),
];

#[test]
fn programs_behave_the_same_with_the_image_writer_and_with_lld() {
    std::env::set_var(LINK_VERIFY_ENV, "1");
    // The image writer is tried in direct link mode, the default only on
    // Windows hosts, and writes ELF only when asked to: ask for both, so the
    // test compares the same two linkers on every host.
    std::env::set_var(LINK_MODE_ENV, "direct");
    std::env::set_var(DIRECT_ELF_ENV, "1");
    for (name, source, calls) in PROGRAMS {
        // Linked by LLD alone.
        std::env::set_var(DIRECT_LINK_ENV, "0");
        let before = image_link_stats();
        let with_lld = {
            let driver = CompileAndRunTestDriver::new(source, |b| b)
                .unwrap_or_else(|e| panic!("{name}: building with LLD failed: {e}"));
            calls(&driver)
        };
        let after_lld = image_link_stats();
        assert_eq!(
            before.written, after_lld.written,
            "{name}: the image writer was used although it was switched off"
        );

        // Linked by the image writer, and checked against LLD's image.
        std::env::set_var(DIRECT_LINK_ENV, "1");
        let with_image = {
            let driver = CompileAndRunTestDriver::new(source, |b| b)
                .unwrap_or_else(|e| panic!("{name}: building with the image writer failed: {e}"));
            calls(&driver)
        };
        let after = image_link_stats();
        assert!(
            after.written > after_lld.written,
            "{name}: the image writer did not write the library (declined: {:?}, last: {:?})",
            after.declined,
            after.last_decline
        );
        assert!(
            after.verified > after_lld.verified,
            "{name}: the image was not compared with LLD's"
        );
        assert_eq!(
            after.declined, after_lld.declined,
            "{name}: the image writer declined"
        );
        assert_eq!(with_lld, with_image, "{name}: results differ");
        assert!(!with_image.is_empty());
    }
}
