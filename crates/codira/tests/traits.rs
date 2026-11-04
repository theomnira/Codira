//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests of traits and bounded generics: the real `codira`
//!   binary builds and runs programs whose generic functions call trait
//!   methods on primitive, struct, class, enum and generic struct types,
//!   with default methods, supertraits, `where` clauses and traits declared
//!   in another module, and checks their output; a program that breaks a
//!   bound, or an effect promise through a bound, does not build.

use std::{path::Path, process::Command};

/// Writes a package whose `src/` holds `files` (`(name, source)` pairs).
fn package_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_traits")
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

/// Runs `codira run` on the package in `dir` with `extra` arguments.
fn codira_run(dir: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("run")
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .args(extra)
        .output()
        .expect("failed to spawn codira")
}

/// Builds and runs the program made of `files` with `extra` arguments, and
/// returns its stdout and stderr.
fn run_ok(files: &[(&str, &str)], extra: &[&str]) -> (String, String) {
    let dir = package_with(files);
    let output = codira_run(dir.path(), extra);
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
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
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    assert!(
        !output.status.success(),
        "codira run succeeded, but should have been rejected:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("unreachable"),
        "a rejected program ran"
    );
    stderr
}

const SHAPES: &str = r#"
trait Shape {
    func area(self) -> i64;
    func describe(self) -> i64 { self.area() * 10 }
}

trait Named: Shape {
    func id(self) -> i64;
}

trait Make {
    func make() -> Self;
}

struct Rect { w: i64, h: i64 }
class Counter { var n: i64 }
struct Box[T] { v: T }

extend Rect: Shape {
    func area(self) -> i64 { self.w * self.h }
}
extend Rect: Named {
    func id(self) -> i64 { 7 }
}
extend Rect: Make {
    func make() -> Rect { Rect { w: 2, h: 5 } }
}
extend Counter: Shape {
    func area(self) -> i64 { self.n }
    // Overrides the default.
    func describe(self) -> i64 { 0 - self.n }
}
extend i64: Shape {
    func area(self) -> i64 { self * self }
}
extend Box[T: Shape]: Shape {
    func area(self) -> i64 { self.v.area() + 1000 }
}

func total[T: Shape](x: T) -> i64 { x.area() + x.describe() }
func named[T](x: T) -> i64 where T: Named { x.id() + total(x) }
func fresh[T: Shape + Make](x: T) -> i64 { T.make().area() + x.area() }
func nested[T: Shape](x: T) -> i64 { total(Box { v: x }) }

public func main() {
    let r = Rect { w: 3, h: 4 }
    let c = Counter { n: 5 }
    let n: i64 = 6
    println(total(r))
    println(total(c))
    println(total(n))
    println(total(Box { v: r }))
    println(named(r))
    println(fresh(r))
    println(nested(n))
    println(r.describe())
    c.n = 9
    println(total(c))
}
"#;

/// A generic function bounded by a trait runs the implementation for each
/// type it is instantiated at: a struct, a class (whose overriding
/// `describe` wins over the default), a primitive, and a generic struct
/// whose own implementation is bounded. Also a supertrait through a
/// `where` clause, `A + B` with an associated function called on the type
/// parameter, a bounded parameter passed on inside a generic struct, and a
/// default method called on a concrete type.
#[test]
fn bounded_generics_dispatch_to_each_implementation() {
    let (stdout, _) = run_ok(&[("mod.code", SHAPES)], &[]);
    assert_eq!(
        stdout,
        "132\n\
         0\n\
         396\n\
         11132\n\
         139\n\
         22\n\
         11396\n\
         120\n\
         0\n"
    );
}

const ORDERING: &str = r#"
public trait Eq {
    func eq(self, other: Self) -> bool;
    func ne(self, other: Self) -> bool { !self.eq(other) }
}

public trait Ord: Eq {
    func lt(self, other: Self) -> bool;
    func le(self, other: Self) -> bool { self.lt(other) || self.eq(other) }
}

public func largest[T: Ord](xs: [T]) -> T {
    var best = xs[0]
    for x in xs {
        if best.lt(x) { best = x }
    }
    best
}

public func count_equal[T: Eq](xs: [T], x: T) -> i64 {
    var n = 0
    for y in xs {
        if y.eq(x) { n += 1 }
    }
    n
}
"#;

const ORDERING_MAIN: &str = r#"
import ordering.{Eq, Ord, largest, count_equal}

enum Color {
    Red
    Green
    Custom(level: i64)
}

extend Color {
    func level(self) -> i64 {
        match self {
            Color.Red -> 1
            Color.Green -> 2
            Color.Custom(l) -> l
        }
    }
}

extend Color: Eq {
    func eq(self, other: Color) -> bool { self.level() == other.level() }
}

class Account { var balance: i64 }

extend Account: Eq {
    func eq(self, other: Account) -> bool { self.balance == other.balance }
}
extend Account: Ord {
    func lt(self, other: Account) -> bool { self.balance < other.balance }
}

extend f64: Eq {
    func eq(self, other: f64) -> bool { self == other }
}

public func main() {
    let cs = [Color.Red, Color.Custom(2), Color.Green, Color.Custom(1)]
    println(count_equal(cs, Color.Green))
    println(Color.Red.ne(Color.Green))
    let a = Account { balance: 10 }
    let b = Account { balance: 30 }
    let c = Account { balance: 20 }
    println(largest([a, b, c]).balance)
    println(a.le(Account { balance: 10 }))
    println(b.le(c))
    println(count_equal([1.5, 2.5, 1.5], 1.5))
}
"#;

/// Traits and generic functions declared in one module, implemented for an
/// enum, a class and `f64` in another: default methods (`ne`, `le`) reach
/// the implementations through `Self`, and a supertrait's method (`eq` of
/// `Eq` for `T: Ord`) is callable through the subtrait's bound.
#[test]
fn traits_cross_modules_with_defaults_and_supertraits() {
    let (stdout, _) = run_ok(
        &[("mod.code", ORDERING_MAIN), ("ordering.code", ORDERING)],
        &[],
    );
    assert_eq!(stdout, "2\ntrue\n30\ntrue\nfalse\n2\n");
}

/// The Cranelift backend does not compile trait-method calls or generic
/// functions: it says so and the program runs on LLVM, with the same
/// output.
#[test]
fn jit_falls_back_to_llvm_for_traits() {
    let source = r#"
trait Double {
    func double(self) -> i64;
}
extend i64: Double {
    func double(self) -> i64 { self * 2 }
}
func quad[T: Double](x: T) -> i64 { x.double() * 2 }
public func main() -> i64 {
    let n: i64 = 5
    quad(n)
}
"#;
    let (stdout, stderr) = run_ok(&[("mod.code", source)], &["--jit"]);
    assert_eq!(stdout, "20\n");
    assert!(
        stderr.contains("running on the LLVM backend"),
        "stderr: {stderr}"
    );
}

/// A call whose argument does not implement the bound is an error naming
/// the type, the trait and the parameter, and the program does not run.
#[test]
fn a_type_that_does_not_implement_the_bound_is_rejected() {
    let source = r#"
trait Shape {
    func area(self) -> i64;
}
struct Rect { w: i64, h: i64 }
struct Circle { r: i64 }
extend Rect: Shape {
    func area(self) -> i64 { self.w * self.h }
}
func total[T: Shape](x: T) -> i64 { x.area() }
public func main() {
    println("unreachable")
    println(total(Circle { r: 1 }))
}
"#;
    let stderr = run_err(&[("mod.code", source)]);
    assert!(
        stderr.contains(
            "the type `Circle` does not implement the trait `Shape`, which `total` requires of \
             its parameter `T`"
        ),
        "stderr: {stderr}"
    );
}

/// An implementation missing a method, or with the wrong signature, is
/// rejected where it is written.
#[test]
fn an_implementation_that_does_not_match_is_rejected() {
    let source = r#"
trait Shape {
    func area(self) -> i64;
    func scale(self, by: i64) -> Self;
}
struct Rect { w: i64, h: i64 }
extend Rect: Shape {
    func area(self) -> f64 { 1.0 }
}
public func main() { println("unreachable") }
"#;
    let stderr = run_err(&[("mod.code", source)]);
    assert!(
        stderr.contains("the implementation of `Shape` for `Rect` is missing the method `scale`"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains(
            "`area` does not match its declaration in trait `Shape` (with `Self` = `Rect`): \
             expected `func(self) -> i64`, found `func(self) -> f64`"
        ),
        "stderr: {stderr}"
    );
}

/// The effect analysis cannot see which implementation a trait-method call
/// on a type parameter reaches, so a `@noalloc` function calling a bounded
/// generic function is rejected, with the reason; calling the
/// implementation directly is fine.
#[test]
fn noalloc_through_a_bound_is_rejected() {
    let source = r#"
trait Shape {
    func area(self) -> i64;
}
struct Rect { w: i64, h: i64 }
extend Rect: Shape {
    func area(self) -> i64 { self.w * self.h }
}
func total[T: Shape](x: T) -> i64 { x.area() }
@noalloc
public func direct(r: Rect) -> i64 { r.area() }
@noalloc
public func through_bound(r: Rect) -> i64 { total(r) }
public func main() { println("unreachable") }
"#;
    let stderr = run_err(&[("mod.code", source)]);
    assert!(
        stderr.contains(
            "a trait method called on a generic type, whose implementation is chosen per \
             instantiation and is not known here"
        ),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("`direct` is `@noalloc`"),
        "stderr: {stderr}"
    );
}

/// `bool`, `str` and `f32` implement a trait, and so does a generic class
/// whose implementation, and an inherent `extend` block, are bounded: the
/// bound holds in their methods and is checked where `Cell[str]` and
/// `Cell[Cell[bool]]` are used.
#[test]
fn primitives_and_a_bounded_generic_class() {
    let source = r#"
trait Weight {
    func weight(self) -> i64;
}
extend bool: Weight {
    func weight(self) -> i64 { if self { 1 } else { 0 } }
}
extend str: Weight {
    func weight(self) -> i64 { 100 }
}
extend f32: Weight {
    func weight(self) -> i64 { 3 }
}
class Cell[T] { var v: T }
extend Cell[T: Weight]: Weight {
    func weight(self) -> i64 { self.v.weight() * 2 }
}
extend Cell[T: Weight] {
    func heavy(self) -> bool { self.v.weight() > 1 }
}
func w[T: Weight](x: T) -> i64 { x.weight() }
public func main() {
    let f: f32 = 1.5
    println(w(true) + w(false) + w("hi") + w(f))
    let c = Cell { v: "x" }
    println(w(c))
    println(c.heavy())
    println(w(Cell { v: Cell { v: true } }))
}
"#;
    let (stdout, _) = run_ok(&[("mod.code", source)], &[]);
    assert_eq!(stdout, "104\n200\ntrue\n4\n");
}
