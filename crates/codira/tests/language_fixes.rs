//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests, through the real `codira` binary, of language
//!   constructs the compiler used to accept without compiling them as
//!   written:
//!   - functions returning `never`: built and run, locally, as `extern "C"`
//!     declarations and across modules, on the LLVM backend and on the
//!     Cranelift `--jit` backend.

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
