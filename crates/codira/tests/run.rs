//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - End-to-end tests of `codira run`: the real `codira` binary builds a
//!   package and runs it, and its stdout, stderr and exit status are checked.

use std::{path::Path, process::Command};

/// Copies the package at `example` (relative to the repository root) into a
/// fresh temporary directory, so building it writes nothing into the
/// repository.
fn copy_example(example: &str) -> tempfile::TempDir {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(example);
    let dir = tempfile::Builder::new()
        .prefix("codira_run")
        .tempdir()
        .unwrap();
    std::fs::copy(source.join("codira.toml"), dir.path().join("codira.toml")).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    for entry in std::fs::read_dir(source.join("src")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.path().join("src").join(entry.file_name())).unwrap();
    }
    dir
}

/// Writes a one-file package whose `src/mod.code` is `source`.
fn package_with(source: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_run")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), source).unwrap();
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

#[test]
fn hello_example_runs() {
    let dir = copy_example("examples/hello");
    let output = codira_run(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "codira run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout.replace("\r\n", "\n"),
        "Hello, world!\n\
         1 squared is 1\n\
         2 squared is 4\n\
         3 squared is 9\n\
         pi is roughly 3.14159, and that is true\n"
    );
}

/// A non-default entry point, and its primitive return value printed after
/// whatever the function itself printed.
#[test]
fn run_invokes_the_named_entry_point() {
    let dir = package_with(
        r#"
public func main() { println("not me") }
public func other() -> i32 {
    println("other runs")
    41 + 1
}
"#,
    );
    let output = codira_run(dir.path(), &["other"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        "other runs\n42\n"
    );
}

/// A program with an error is reported and not run.
#[test]
fn run_does_not_run_a_program_that_fails_to_compile() {
    let dir = package_with(
        r#"
public func main() {
    println("unreachable")
    let x: i32 = "not a number"
}
"#,
    );
    let output = codira_run(dir.path(), &[]);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("unreachable"));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("expected `i32`"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// An out-of-range index stops the program with a panic naming the index,
/// the length and the source location -- after flushing what was already
/// printed -- and exits with status 101. A negative index reads as negative.
#[test]
fn out_of_bounds_index_panics_with_its_location() {
    let dir = package_with(
        r#"
public func main() {
    let xs = [1, 2, 3]
    println("before")
    var i: i32 = 0
    for _ in 0..5 { i -= 1 }
    println(xs[i + 4])
    println(xs[i])
    println("after")
}
"#,
    );
    let output = codira_run(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(101), "stderr: {stderr}");
    assert_eq!(stdout, "before\n");
    assert!(
        stderr.contains(
            "panic at mod.code:7:13: index out of bounds: the length is 3 but the index is -1"
        ),
        "stderr: {stderr}"
    );
}

/// Unwrapping `nil` with postfix `!` stops the program with the location of
/// the `!` expression.
#[test]
fn unwrapping_nil_panics_with_its_location() {
    let dir = package_with(
        r#"
func lookup(key: i32) -> i32? {
    if key == 1 { 10 } else { nil }
}

public func main() {
    println(lookup(1)!)
    println(lookup(2)!)
}
"#,
    );
    let output = codira_run(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(101), "stderr: {stderr}");
    assert_eq!(stdout, "10\n");
    assert!(
        stderr.contains("panic at mod.code:8:13: unwrapped `nil` with postfix `!`"),
        "stderr: {stderr}"
    );
}

/// Regression: assigning an array (`a = b`, and an element of an array of
/// arrays) used to panic the code generator with "unimplemented operation
/// [i64]op[i64]". The assignment stores the handle, so the element shares
/// the assigned array.
#[test]
fn arrays_can_be_assigned_to_locals_and_elements() {
    let dir = package_with(
        r#"
public func main() {
    var a = [1, 2]
    a = [5, 6, 7]
    println(a[2], " ", a.len())

    var rows = [[1, 2], [3, 4]]
    let row = [7, 8]
    rows[1] = row
    println(rows[1][0], " ", rows[1][1], " ", rows[0][1])
}
"#,
    );
    let output = codira_run(dir.path(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "7 3\n7 8 2\n");
}
