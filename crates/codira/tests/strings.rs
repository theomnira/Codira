//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - End-to-end tests of `String`: programs are built and run by the real
//!   `codira` binary, and their output, panics and exit status are checked.

use std::{path::Path, process::Command};

/// Writes a one-file package whose `src/mod.code` is `source`.
fn package_with(source: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_strings")
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

/// Builds and runs `source`, asserting success, and returns its stdout.
fn run_ok(source: &str) -> String {
    let dir = package_with(source);
    let output = codira_run(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert!(
        output.status.success(),
        "codira run failed:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Builds and runs `source`, asserting a panic (exit status 101), and returns
/// its stderr.
fn run_panics(source: &str) -> String {
    let dir = package_with(source);
    let output = codira_run(dir.path(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    assert_eq!(
        output.status.code(),
        Some(101),
        "expected a panic:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    stderr
}

/// Builds `source` and returns the compiler's diagnostics, asserting the
/// build failed.
fn build_fails(source: &str) -> String {
    let dir = package_with(source);
    let output = codira_run(dir.path(), &[]);
    assert!(!output.status.success(), "the build was expected to fail");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn construction_concatenation_and_printing() {
    let out = run_ok(
        r#"
public func main() {
    let a = "hello".to_string()
    let b = a + ", " + "world"
    println(b)
    println(b.len(), " ", b.is_empty(), " ", "".to_string().is_empty())
    let n = 42
    println("n = " + n.to_string() + ", x = " + 2.5.to_string() + ", " + true.to_string())
    println((-7i8).to_string(), " ", 18446744073709551615u64.to_string())
    println(170141183460469231731687303715884105727i128.to_string())
    let empty: String = "".to_string()
    println("[", empty, "]")
}
"#,
    );
    assert_eq!(
        out,
        "hello, world\n12 false true\nn = 42, x = 2.5, true\n-7 18446744073709551615\n\
         170141183460469231731687303715884105727\n[]\n"
    );
}
