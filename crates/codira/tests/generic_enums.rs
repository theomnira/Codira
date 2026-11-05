//! Copyright (c) 2026 Omnira CJSC
//! Date: November 5, 2026
//!
//! Functionality:
//! - End-to-end tests of generic enums and error propagation: the real
//!   `codira` binary builds and runs programs that construct and match
//!   instantiated generic enums, give them methods (with bounds), keep them
//!   in fields, arrays and class-linked lists, pass them across modules, use
//!   `?` on `Result`s and optionals (wrapping errors into the caller's error
//!   enum), and use `std.result.result`, `std.fs.file` and `std.text.ascii`;
//!   the `examples/results` package runs; programs that break the rules are
//!   rejected with the rule's message.

use std::{path::Path, process::Command};

/// Writes a package whose `src/` holds `files` (`(name, source)` pairs).
fn package_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_generic_enums")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    for (name, source) in files {
        std::fs::write(dir.path().join("src").join(name), source).unwrap();
    }
    dir
}

/// Runs `codira run` on the package in `dir`, from `dir`, with `extra`
/// arguments.
fn codira_run(dir: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .current_dir(dir)
        .arg("run")
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .args(extra)
        // This source tree's standard library, never another checkout's.
        .env(
            "CODIRA_STD_PATH",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std"),
        )
        .output()
        .expect("failed to spawn codira")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// Builds and runs the program made of `files` with `extra` arguments, and
/// returns its stdout and stderr.
fn run_ok(files: &[(&str, &str)], extra: &[&str]) -> (String, String) {
    let dir = package_with(files);
    let output = codira_run(dir.path(), extra);
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    assert!(
        output.status.success(),
        "codira run failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    (stdout, stderr)
}

/// Builds the program and expects it to be rejected; returns stderr.
fn run_err(files: &[(&str, &str)]) -> String {
    let dir = package_with(files);
    let output = codira_run(dir.path(), &[]);
    let stderr = text(&output.stderr);
    assert!(
        !output.status.success(),
        "codira run succeeded, but should have been rejected:\n{}",
        text(&output.stdout)
    );
    assert!(
        !text(&output.stdout).contains("ran"),
        "a rejected program ran"
    );
    stderr
}

const BASICS: &str = r#"
enum Result[T, E] { Ok(T), Err(E) }
enum Maybe[T] { Just(T), Nothing }

extend Result[T, E] {
    func is_ok(self) -> bool {
        match self { Result.Ok(_) -> true, Result.Err(_) -> false }
    }
    func unwrap_or(self, fallback: T) -> T {
        match self { Result.Ok(v) -> v, Result.Err(_) -> fallback }
    }
}

func half(n: i64) -> Result[i64, str] {
    if n % 2 == 0 { Result.Ok(n / 2) } else { Result.Err("odd") }
}

func count[T](xs: [Maybe[T]]) -> i64 {
    var n = 0
    for x in xs {
        if let Maybe.Just(_) = x { n += 1 }
    }
    n
}

func countdown(m: Maybe[i64]) -> i64 {
    var cur = m
    var steps = 0
    while let Maybe.Just(n) = cur {
        steps += 1
        cur = if n > 0 { Maybe.Just(n - 1) } else { Maybe.Nothing }
    }
    steps
}

public func main() {
    let a = half(10)
    let b = half(3)
    println(a.unwrap_or(0))
    println(b.unwrap_or(-1))
    if let Result.Err(msg) = b { println(msg) }
    println(a.is_ok())
    println(count([Maybe.Just(1.5), Maybe.Nothing, Maybe.Just(2.5)]))
    println(count([Maybe.Just(true)]))
    let r: Result[f64, i64] = Result.Ok(2.5)
    match r {
        Result.Ok(v) -> println(v),
        Result.Err(e) -> println(e),
    }
    let nested: Maybe[Maybe[i64]] = Maybe.Just(Maybe.Nothing)
    match nested {
        Maybe.Just(Maybe.Just(v)) -> println(v),
        Maybe.Just(Maybe.Nothing) -> println("just nothing"),
        Maybe.Nothing -> println("nothing"),
    }
    println(countdown(Maybe.Just(3)))
    println(Result.Ok(3) == Result.Err(3))
    let x: Result[i64, i64] = Result.Ok(3)
    println(x == Result.Ok(3))
}
"#;

/// Construction, `match`, `if let`, `while let`, nested instantiations,
/// methods through `extend Result[T, E]`, generic functions over a generic
/// enum, and `==`, each at several instantiations.
#[test]
fn generic_enums_construct_match_and_have_methods() {
    let (stdout, _) = run_ok(&[("mod.code", BASICS)], &[]);
    assert_eq!(
        stdout,
        "5\n-1\nodd\ntrue\n2\n1\n2.5\njust nothing\n4\nfalse\ntrue\n"
    );
}

const OUTCOME: &str = r#"
public enum Outcome[T, E] { Done(T), Failed(E) }

public trait Show {
    func show(self) -> i64;
}

extend i64: Show { func show(self) -> i64 { self * 10 } }

extend Outcome[T, E] {
    public func is_done(self) -> bool {
        match self { Outcome.Done(_) -> true, Outcome.Failed(_) -> false }
    }
    public func or(self, fallback: T) -> T {
        match self { Outcome.Done(v) -> v, Outcome.Failed(_) -> fallback }
    }
    public func done(value: T) -> Outcome[T, E] { Outcome.Done(value) }
}

// Only for an Outcome whose value can be shown.
extend Outcome[T: Show, E] {
    public func shown(self) -> i64 {
        match self { Outcome.Done(v) -> v.show(), Outcome.Failed(_) -> -1 }
    }
}

public func checked_div(a: i64, b: i64) -> Outcome[i64, str] {
    if b == 0 { Outcome.Failed("division by zero") } else { Outcome.Done(a / b) }
}

// A recursive generic enum, through a class.
public class Node[T] {
    value: T,
    next: Link[T]
}

public enum Link[T] { End, More(Node[T]) }

public func sum_list(l: Link[i64]) -> i64 {
    var total = 0
    var cur = l
    while let Link.More(node) = cur {
        total += node.value
        cur = node.next
    }
    total
}
"#;

const OUTCOME_MAIN: &str = r#"
import outcome.{Outcome, Link, Node, checked_div, sum_list}

struct Holder { o: Outcome[f64, i32], n: i64 }
class Boxed { o: Outcome[i64, str] }

func first_failure(xs: [Outcome[i64, str]]) -> i64 {
    var i = 0
    for x in xs {
        if !x.is_done() { return i }
        i += 1
    }
    -1
}

public func main() {
    let a = checked_div(10, 2)
    let b = checked_div(1, 0)
    println(a.or(0))
    println(b.or(-7))
    if let Outcome.Failed(why) = b { println(why) }
    println(a.shown())
    let h = Holder { o: Outcome.Done(1.5), n: 3 }
    match h.o { Outcome.Done(v) -> println(v), Outcome.Failed(c) -> println(c) }
    let boxed = Boxed { o: checked_div(9, 3) }
    println(boxed.o.or(0))
    println(first_failure([a, a, b, a]))
    let o2: Outcome[i64, bool] = Outcome.done(4)
    println(o2.or(0))
    let list = Link.More(Node { value: 1, next: Link.More(Node { value: 2, next: Link.End }) })
    println(sum_list(list))
}
"#;

/// A generic enum declared in one module and used in another: as a
/// parameter and result, a struct field, a class field and an array
/// element, with methods, an associated function, a bounded `extend`, and a
/// recursive list held through a class.
#[test]
fn generic_enums_cross_modules_and_live_in_fields_arrays_and_classes() {
    let (stdout, _) = run_ok(
        &[("outcome.code", OUTCOME), ("mod.code", OUTCOME_MAIN)],
        &[],
    );
    assert_eq!(stdout, "5\n-7\ndivision by zero\n50\n1.5\n3\n2\n4\n3\n");
}

const PROPAGATE: &str = r#"
enum Result[T, E] { Ok(T), Err(E) }

func parse_digit(c: i64) -> Result[i64, str] {
    if c >= 48 && c <= 57 { Result.Ok(c - 48) } else { Result.Err("not a digit") }
}

func two_digits(a: i64, b: i64) -> Result[i64, str] {
    let hi = parse_digit(a)?
    let lo = parse_digit(b)?
    Result.Ok(hi * 10 + lo)
}

func level3(x: i64) -> Result[i64, i64] {
    if x < 0 { Result.Err(x) } else { Result.Ok(x * 2) }
}
func level2(x: i64) -> Result[i64, i64] { Result.Ok(level3(x)? + 1) }
func level1(x: i64) -> Result[i64, i64] { Result.Ok(level2(x)? * 3) }

func first_even(xs: [i64]) -> i64? {
    for x in xs {
        if x % 2 == 0 { return x }
    }
    nil
}

func doubled_first_even(xs: [i64]) -> i64? {
    let v = first_even(xs)?
    v * 2
}

// `?` in a closure returns from the closure.
func via_closure(c: i64) -> i64 {
    let f = func(x: i64) -> Result[i64, str] { Result.Ok(parse_digit(x)? + 100) }
    match f(c) { Result.Ok(v) -> v, Result.Err(_) -> -1 }
}

// `?` in a generic function, at two instantiations.
func first_ok[T](a: Result[T, str], b: Result[T, str]) -> Result[T, str] {
    let x = a?
    let y = b?
    Result.Ok(x)
}

func show(r: Result[i64, str]) {
    match r {
        Result.Ok(v) -> println(v),
        Result.Err(e) -> println(e),
    }
}

public func main() {
    show(two_digits(52, 55))
    show(two_digits(52, 65))
    show(two_digits(1, 55))
    match level1(5) { Result.Ok(v) -> println(v), Result.Err(e) -> println(e) }
    match level1(-4) { Result.Ok(v) -> println(v), Result.Err(e) -> println(e) }
    if let v = doubled_first_even([1, 3, 6, 8]) { println(v) }
    println(doubled_first_even([1, 3]) == nil)
    println(via_closure(55))
    println(via_closure(5))
    show(first_ok(Result.Ok(1), Result.Err("second")))
    match first_ok(Result.Ok(true), Result.Ok(false)) {
        Result.Ok(v) -> println(v),
        Result.Err(e) -> println(e),
    }
}
"#;

/// `?` on a `Result` returns the `Err` early and is the `Ok` value
/// otherwise, through three call levels, in a closure and in a generic
/// function; on an optional it returns `nil` early.
#[test]
fn propagation_returns_errors_and_nil_early() {
    let (stdout, _) = run_ok(&[("mod.code", PROPAGATE)], &[]);
    assert_eq!(
        stdout,
        "47\nnot a digit\nnot a digit\n33\n-4\n12\ntrue\n107\n-1\nsecond\ntrue\n"
    );
}

const WRAP: &str = r#"
import errors.{Result, IoError, read}

enum Problem { Io(IoError), Parse(i64), Other }

func parse(n: i64) -> Result[i64, i64] {
    if n < 0 { Result.Err(n) } else { Result.Ok(n) }
}

func both(name: i64, n: i64) -> Result[i64, Problem] {
    Result.Ok(read(name)? + parse(n)?)
}

func report(r: Result[i64, Problem]) {
    match r {
        Result.Ok(v) -> println(v),
        Result.Err(Problem.Io(IoError.Missing)) -> println("missing"),
        Result.Err(Problem.Io(IoError.Denied(code))) -> println(code),
        Result.Err(Problem.Parse(at)) -> println(at),
        Result.Err(Problem.Other) -> println("other"),
    }
}

public func main() {
    report(both(1, 2))
    report(both(0, 2))
    report(both(-5, 2))
    report(both(1, -9))
}
"#;

const ERRORS: &str = r#"
public enum Result[T, E] { Ok(T), Err(E) }
public enum IoError { Missing, Denied(i64) }

public func read(name: i64) -> Result[i64, IoError] {
    if name == 0 { Result.Err(IoError.Missing) }
    else if name < 0 { Result.Err(IoError.Denied(name)) }
    else { Result.Ok(name * 100) }
}
"#;

/// `?` wraps an error in the one variant of the caller's error enum that
/// holds its type, across modules.
#[test]
fn propagation_wraps_errors_in_the_callers_error_enum() {
    let (stdout, _) = run_ok(&[("errors.code", ERRORS), ("mod.code", WRAP)], &[]);
    assert_eq!(stdout, "102\nmissing\n-5\n-9\n");
}

const STD_RESULT: &str = r#"
import std.result.result.{Result, from_optional}

func half(n: i64) -> Result[i64, str] {
    if n % 2 == 0 { Result.Ok(n / 2) } else { Result.Err("odd") }
}

func quarter(n: i64) -> Result[i64, str] {
    half(half(n)?)
}

public func main() {
    println(half(8).is_ok())
    println(half(7).is_err())
    println(half(8).unwrap_or(0))
    println(half(7).unwrap_or(-1))
    println(half(8).map(func(v: i64) -> f64 { v as f64 * 1.5 }).unwrap_or(0.0))
    println(half(8).and_then(half).unwrap())
    println(quarter(12).expect("12 is a multiple of 4"))
    println(quarter(6).unwrap_or_else(func(e: str) -> i64 { -100 }))
    if let msg = half(3).err() { println(msg) }
    println(half(3).ok() == nil)
    println(half(3).or(half(10)).unwrap())
    let none: i64? = nil
    println(from_optional(none, "missing").is_err())
    let some: i64? = 4
    println(from_optional(some, "missing").unwrap())
    println(half(3).map_err(func(e: str) -> i64 { 7 }).expect_err("odd"))
    println(half(3).expect("three is even"))
    println("not reached")
}
"#;

/// `std.result.result` imports, its combinators compute what they say, and
/// `expect` on an `Err` panics with its message.
#[test]
fn std_result_combinators_and_expect() {
    let dir = package_with(&[("mod.code", STD_RESULT)]);
    let output = codira_run(dir.path(), &[]);
    let (stdout, stderr) = (text(&output.stdout), text(&output.stderr));
    assert!(
        !output.status.success(),
        "the `expect` should have panicked"
    );
    assert_eq!(
        stdout,
        "true\ntrue\n4\n-1\n6.0\n2\n3\n-100\nodd\ntrue\n5\ntrue\n4\n7\n"
    );
    assert!(stderr.contains("panic: three is even"), "{stderr}");
}

/// The `examples/results` package: sums the numbers of a file, and reports
/// a missing file, a bad word and an overflow as values.
#[test]
fn the_results_example_runs() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/results");
    let dir = tempfile::Builder::new()
        .prefix("codira_results")
        .tempdir()
        .unwrap();
    std::fs::copy(source.join("codira.toml"), dir.path().join("codira.toml")).unwrap();
    std::fs::copy(source.join("numbers.txt"), dir.path().join("numbers.txt")).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::copy(source.join("src/mod.code"), dir.path().join("src/mod.code")).unwrap();

    let run = |numbers: Option<&str>| {
        let path = dir.path().join("numbers.txt");
        match numbers {
            Some(numbers) => std::fs::write(&path, numbers).unwrap(),
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        let output = codira_run(dir.path(), &[]);
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout)
    };
    let shipped = std::fs::read_to_string(source.join("numbers.txt")).unwrap();
    assert_eq!(run(Some(&shipped)), "1000\n");
    assert_eq!(run(Some("1 2x 3")), "numbers.txt: not a number at byte 3\n");
    assert_eq!(
        run(Some("99999999999999999999")),
        "numbers.txt: a number is empty or too large\n"
    );
    assert_eq!(
        run(Some("-9223372036854775808 9223372036854775807 + -")),
        "numbers.txt: a number is empty or too large\n"
    );
    assert_eq!(
        run(Some("-9223372036854775808 9223372036854775807\n\t+5 -6")),
        "-2\n"
    );
    assert_eq!(run(Some("")), "0\n");
    assert_eq!(run(None), "numbers.txt: no such file\n");
}

/// A value enum that holds itself is rejected, generic or not; through an
/// array it is fine.
#[test]
fn a_generic_enum_holding_itself_by_value_is_rejected() {
    let stderr = run_err(&[(
        "mod.code",
        r#"
enum Tree[T] { Leaf, Node(Tree[T], T, Tree[T]) }
public func main() { println("ran") }
"#,
    )]);
    assert!(
        stderr.contains("`Tree` contains itself by value"),
        "{stderr}"
    );

    let (stdout, _) = run_ok(
        &[(
            "mod.code",
            r#"
enum Tree[T] { Leaf(T), Node([Tree[T]]) }
func size[T](t: Tree[T]) -> i64 {
    match t {
        Tree.Leaf(_) -> 1,
        Tree.Node(children) -> {
            var n = 0
            for c in children { n += size(c) }
            n
        }
    }
}
public func main() {
    let t = Tree.Node([Tree.Leaf(1), Tree.Node([Tree.Leaf(2), Tree.Leaf(3)]), Tree.Leaf(4)])
    println(size(t))
}
"#,
        )],
        &[],
    );
    assert_eq!(stdout, "4\n");
}

/// Breaking a rule of `?` or of a bounded `extend` is a compile error that
/// names the rule.
#[test]
fn propagation_and_bound_errors_are_reported() {
    let stderr = run_err(&[(
        "mod.code",
        r#"
enum Result[T, E] { Ok(T), Err(E) }
func get() -> Result[i64, bool] { Result.Ok(1) }
func wrong() -> Result[i64, str] { Result.Ok(get()?) }
public func main() { println("ran") }
"#,
    )]);
    assert!(
        stderr.contains(
            "`?` would return this `Err(bool)` from a function that returns `Result[i64, str]`"
        ),
        "{stderr}"
    );

    let stderr = run_err(&[(
        "mod.code",
        r#"
func opt() -> i64? { 1 }
public func main() { let x = opt()?; println("ran") }
"#,
    )]);
    assert!(
        stderr.contains("`?` on an optional returns `nil` early"),
        "{stderr}"
    );

    let stderr = run_err(&[
        ("outcome.code", OUTCOME),
        (
            "mod.code",
            r#"
import outcome.{Outcome}
public func main() {
    let o: Outcome[f64, str] = Outcome.Done(1.0)
    println(o.shown())
    println("ran")
}
"#,
        ),
    ]);
    assert!(stderr.contains("Show"), "{stderr}");
}

/// `codira run --jit` does not compile generic enums or `?`: it says so,
/// and the program runs on LLVM with the same output.
#[test]
fn jit_refuses_and_falls_back_to_llvm() {
    let (stdout, stderr) = run_ok(&[("mod.code", PROPAGATE)], &["--jit"]);
    assert!(stderr.contains("running on the LLVM backend"), "{stderr}");
    assert!(stdout.starts_with("47\nnot a digit\n"), "{stdout}");
}
