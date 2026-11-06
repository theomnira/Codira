//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests, through the real `codira` binary, of language
//!   constructs the compiler used to accept without compiling them as
//!   written:
//!   - functions returning `never`: built and run, locally, as `extern "C"`
//!     declarations and across modules, on the LLVM backend and on the
//!     Cranelift `--jit` backend;
//!   - in-place parameters (`inout`, `mut`, `out`, `mut T`, `&mut T`),
//!     which compiled to a copy and are rejected;
//!   - module-level `let` and `var`, which were dropped and are rejected;
//!   - assignment to a `let` binding, which is rejected, while what a `let`
//!     refers to but does not hold (a class field, an array element) stays
//!     assignable;
//!   - a call of a method or an associated function of another module's
//!     type, which stopped code generation with "unknown function": method
//!     calls never reached the dispatch table, and the receiver was left out
//!     of the boxing on both sides of a call through it;
//!   - a `()` field or enum payload (`Result[(), E]`), whose type the
//!     runtime could not load: it was given a structural type id that no
//!     module exports, instead of the runtime's own `()`.
//!   - an `if` chain whose last `else if` leaves on both branches, which
//!     crashed the code generator.

use std::{
    path::Path,
    process::{Command, Output},
};

/// Writes a package whose `src/` holds `files` (`(name, source)` pairs).
fn package_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_language_fixes")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    for (name, source) in files {
        let path = dir.path().join("src").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    dir
}

/// Runs `codira <args..>` on the package in `dir`.
fn codira(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .env("CODIRA_JIT_TIMINGS", "1")
        .output()
        .expect("failed to spawn codira")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// `codira run` of `files`: its exit code, stdout and stderr.
fn run(files: &[(&str, &str)], args: &[&str]) -> (Option<i32>, String, String) {
    let dir = package_with(files);
    let mut all = vec!["run"];
    all.extend_from_slice(args);
    let output = codira(dir.path(), &all);
    (
        output.status.code(),
        text(&output.stdout),
        text(&output.stderr),
    )
}

/// `codira check` of `files`: whether it succeeded, and its stderr.
fn check(files: &[(&str, &str)]) -> (bool, String) {
    let dir = package_with(files);
    let output = codira(dir.path(), &["check"]);
    (output.status.success(), text(&output.stderr))
}

// ---- `never` ---------------------------------------------------------------

/// The program that crashed the code generator ("could not convert return
/// value"): a function returning `never`, called on a path not taken.
#[test]
fn a_function_returning_never_builds_and_runs() {
    let program = r"
func stop() -> never { loop {} }
public func main() { if false { stop() } println(1) }
";
    let (code, stdout, stderr) = run(&[("mod.code", program)], &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "1\n");
}

/// `never` in one branch of an `if` that has a value, and as the operand
/// of `return`.
#[test]
fn never_where_a_value_is_expected() {
    let program = r"
func stop() -> never { loop {} }
func pick(c: bool) -> i64 { if c { 5 } else { stop() } }
func early(c: bool) -> i64 {
    if !c { return stop() }
    7
}
public func main() {
    println(pick(true))
    println(early(true))
}
";
    let (code, stdout, stderr) = run(&[("mod.code", program)], &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "5\n7\n");
}

/// An `extern "C"` function returning `never` (the runtime's `codira_exit`,
/// registered with a unit result) is called through the dispatch table,
/// and nothing after it runs.
#[test]
fn an_extern_function_returning_never_does_not_come_back() {
    let program = r#"
extern "C" {
    func codira_exit(code: i32) -> never;
}
public func main() {
    println(1)
    codira_exit(3)
    println(2)
}
"#;
    let (code, stdout, stderr) = run(&[("mod.code", program)], &[]);
    assert_eq!(code, Some(3), "{stderr}");
    assert_eq!(stdout, "1\n");
}

/// A `public` function returning `never` called from another module: its
/// reflected signature agrees with the caller's dispatch-table slot.
#[test]
fn a_function_returning_never_called_from_another_module() {
    let main = r"
import other.{fail_with}
public func main() {
    let x: i64 = if true { 3 } else { fail_with(9) }
    println(x)
    fail_with(5)
}
";
    let other = r#"
extern "C" {
    func codira_exit(code: i32) -> never;
}
public func fail_with(code: i32) -> never {
    codira_exit(code)
}
"#;
    let (code, stdout, stderr) = run(&[("mod.code", main), ("other.code", other)], &[]);
    assert_eq!(code, Some(5), "{stderr}");
    assert_eq!(stdout, "3\n");
}

/// The Cranelift backend compiles a call to a function returning `never`
/// (a trap after it) rather than falling back to LLVM.
#[test]
fn the_jit_compiles_never() {
    let program = r"
func stop() -> never { loop {} }
func pick(c: bool) -> i64 { if c { 5 } else { stop() } }
public func main() {
    if false { stop() }
    println(pick(true))
}
";
    let (code, stdout, stderr) = run(&[("mod.code", program)], &["--jit"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stderr.contains("cranelift: compiled in"),
        "fell back to LLVM:\n{stderr}"
    );
    assert_eq!(stdout, "5\n");
}

// ---- in-place parameters -------------------------------------------------

/// `func bump(s: mut S)` used to compile and change only a copy. It is
/// rejected, as are the `inout`, `mut` and `out` conventions, wherever the
/// parameter is declared.
#[test]
fn in_place_parameters_are_rejected() {
    for (program, what) in [
        (
            "struct S { v: i64 }\nfunc bump(s: mut S) { s.v = s.v + 1 }\npublic func main() {}\n",
            "mutable references (`mut T`, `&mut T`) are not supported yet",
        ),
        (
            "struct S { v: i64 }\nfunc bump(s: &mut S) {}\npublic func main() {}\n",
            "mutable references (`mut T`, `&mut T`) are not supported yet",
        ),
        (
            "struct S { v: i64 }\nfunc bump(inout s: S) {}\npublic func main() {}\n",
            "`inout` parameters are not supported yet",
        ),
        (
            "func set(out x: i64) {}\npublic func main() {}\n",
            "`out` parameters are not supported yet",
        ),
        (
            "extern \"C\" { func fill(mut x: i64); }\npublic func main() {}\n",
            "`mut` parameters are not supported yet",
        ),
    ] {
        let (ok, stderr) = check(&[("mod.code", program)]);
        assert!(!ok, "accepted:\n{program}");
        assert!(stderr.contains(what), "{program}\n{stderr}");
    }
}

/// What a copy implements is still accepted: a mutable receiver is a
/// mutable local copy, and returning the new value is how to update one.
#[test]
fn by_copy_updates_still_work() {
    let program = r"
struct S { v: i64 }
extend S {
    func bumped(var self) -> S { self.v = self.v + 1; self }
}
func bump(s: S) -> S { S { v: s.v + 1 } }
public func main() {
    let s = S { v: 1 }
    let t = bump(s)
    println(s.v)
    println(t.v)
    println(t.bumped().v)
    println(t.v)
}
";
    let (code, stdout, stderr) = run(&[("mod.code", program)], &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "1\n2\n3\n2\n");
}

// ---- module-level bindings -----------------------------------------------

/// `public let LIMIT: i64 = 10;` used to be dropped without a word, and a
/// use of it was an undefined name. The binding itself is rejected now,
/// and the replacement the message suggests works across modules.
#[test]
fn module_level_bindings_are_rejected() {
    let (ok, stderr) = check(&[(
        "mod.code",
        "public let LIMIT: i64 = 10;\npublic func main() {}\n",
    )]);
    assert!(!ok);
    assert!(
        stderr.contains("module-level `let` is not supported yet: `LIMIT`"),
        "{stderr}"
    );

    let (ok, stderr) = check(&[("mod.code", "var COUNT: i64 = 0;\npublic func main() {}\n")]);
    assert!(!ok);
    assert!(
        stderr.contains("module-level `var` is not supported yet"),
        "{stderr}"
    );

    let main = "import limits.{LIMIT}\npublic func main() { println(LIMIT() * 2) }\n";
    let limits = "public func LIMIT() -> i64 { 10 }\n";
    let (code, stdout, stderr) = run(&[("mod.code", main), ("limits.code", limits)], &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "20\n");
}

// ---- `let` is immutable -------------------------------------------------

#[test]
fn assigning_to_a_let_binding_is_rejected() {
    for (body, name) in [
        ("let x = 1\n    x = 2", "x"),
        ("let x = 1\n    x += 2", "x"),
        ("let p = P { a: 1 }\n    p.a = 2", "p"),
        ("let x: i64\n    x = 1", "x"),
    ] {
        let program = format!("struct P {{ a: i64 }}\npublic func main() {{\n    {body}\n}}\n");
        let (ok, stderr) = check(&[("mod.code", &program)]);
        assert!(!ok, "accepted:\n{program}");
        assert!(
            stderr.contains(&format!(
                "cannot assign to `{name}`: it is bound with `let`"
            )),
            "{program}\n{stderr}"
        );
    }
}

/// `var`, `let mut`, a class field and an array element reached through a
/// `let` stay assignable, and run as before.
#[test]
fn what_a_let_does_not_hold_stays_assignable() {
    let program = r"
class C { n: i64 }
public func main() {
    var a = 1
    a += 1
    let mut b = 10
    b = b + 1
    let c = C { n: 5 }
    c.n = 6
    let xs = [1, 2, 3]
    xs[0] = 7
    println(a)
    println(b)
    println(c.n)
    println(xs[0])
}
";
    let (code, stdout, stderr) = run(&[("mod.code", program)], &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "2\n11\n6\n7\n");
}

// ---- methods of another module's types ------------------------------------

/// Methods and associated functions of types declared in another module:
/// a value-struct receiver, `String` arguments and results, a value-struct
/// result, a class receiver and an associated function, all called through
/// the dispatch table.
#[test]
fn methods_of_another_modules_types_are_called() {
    let shapes = r#"
public struct Size { public w: i64, public h: i64 }
public class Counter { public n: i64 }

extend Size {
    public func area(self) -> i64 { self.w * self.h }
    public func describe(self, unit: String) -> String { f"{self.w}x{self.h} {unit}" }
    public func doubled(self) -> Size { Size { w: self.w * 2, h: self.h * 2 } }
    public func square(side: i64) -> Size { Size { w: side, h: side } }
}

extend Counter {
    public func bump(self, by: i64) -> i64 {
        self.n += by
        self.n
    }
}
"#;
    let main = r#"
import shapes.{Size, Counter}
public func main() {
    let s = Size { w: 2, h: 3 }
    println(s.area())
    println(s.describe("cm"))
    let d = s.doubled()
    println(d.w, " ", d.h, " ", d.area())
    let q = Size.square(4)
    println(q.area())
    let c = Counter { n: 1 }
    println(c.bump(2), " ", c.bump(3), " ", c.n)
}
"#;
    let (code, stdout, stderr) = run(&[("mod.code", main), ("shapes.code", shapes)], &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "6\n2x3 cm\n4 6 24\n16\n3 6 6\n");
}

// ---- the unit type in a layout ----------------------------------------------

/// `()` as an enum payload, in a generic instantiation and as a struct field.
#[test]
fn a_unit_payload_or_field_loads_and_runs() {
    let program = r#"
enum R[T, E] { Ok(T), Err(E) }
enum U { A(()), B }
struct S { unit: (), n: i64 }

func f(fail: bool) -> R[(), i64] {
    if fail { R.Err(4) } else { R.Ok(()) }
}

public func main() {
    let s = S { unit: (), n: 2 }
    match f(false) { R.Ok(_) -> println("ok ", s.n), R.Err(e) -> println(e) }
    match f(true) { R.Ok(_) -> println("ok"), R.Err(e) -> println("err ", e) }
    match U.A(()) { U.A(_) -> println("a"), U.B -> println("b") }
}
"#;
    let (code, stdout, stderr) = run(&[("mod.code", program)], &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, "ok 2\nerr 4\na\n");
}

// ---- `if` chains whose last two branches diverge ---------------------------

/// An `else if` whose own branches both leave (`continue` and `return`)
/// ends an `if` chain that the enclosing `if` merges after: it used to
/// leave the code generator in a block it had removed from the function,
/// and the build panicked ("called `Result::unwrap()` on an `Err` value").
#[test]
fn an_if_chain_ending_in_two_diverging_branches_compiles() {
    let program = r#"
func step(x: i64) -> i64 {
    var i = 0
    var total = 0
    while i < 10 {
        if x == 1 {
            total += 1
        } else if x == 2 {
            i += 2
            continue
        } else {
            return -1
        }
        i += 1
    }
    total
}

func word(x: i64) -> String {
    var out = "".to_string()
    var i = 0
    while i < 3 {
        if x == 1 {
            out += "a"
        } else if x == 2 {
            if i == 0 { out += "b" } else { out += "c" }
            i += 1
            continue
        } else {
            return "none".to_string()
        }
        i += 1
    }
    out
}

public func main() {
    println(step(1), " ", step(2), " ", step(3))
    println(word(1), " ", word(2), " ", word(3))
}
"#;
    for level in ["0", "2"] {
        let (code, stdout, stderr) = run(&[("mod.code", program)], &["-O", level]);
        assert_eq!(code, Some(0), "-O {level}: {stderr}");
        assert_eq!(stdout, "10 0 -1\naaa bcc none\n", "-O {level}");
    }
}
