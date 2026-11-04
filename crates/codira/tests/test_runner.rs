//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - End-to-end tests of `codira test`, of `codira check` writing runnable
//!   counterexample tests, and of `codira query`, through the real `codira`
//!   binary.

use std::{path::Path, process::Command};

fn write_package(dir: &Path, source: &str) {
    std::fs::write(
        dir.join("codira.toml"),
        "[package]\nname=\"tested\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/mod.code"), source).unwrap();
}

/// Runs the `codira` binary in `dir`; returns (success, stdout).
fn codira(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (
        output.status.success(),
        format!("{stdout}\n--- stderr ---\n{stderr}"),
    )
}

#[test]
fn test_functions_pass_fail_and_trap_in_isolation() {
    let project = tempfile::tempdir().unwrap();
    write_package(
        project.path(),
        r#"
public func add(a: i64, b: i64) -> i64 { a + b }

@test
public func addition_works() -> bool {
    add(2, 3) == 5
}

@test
public func addition_is_wrong() -> bool {
    add(2, 2) == 5
}

@test
public func runs_to_completion() {
    add(1, 1);
}

public func divide(a: i64, b: i64) -> i64 { a / b }

@test
public func divides_by_zero() -> bool {
    divide(1, add(0, 0)) == 0
}
"#,
    );
    let (ok, out) = codira(project.path(), &["test"]);
    assert!(!ok, "{out}");
    assert!(out.contains("running 4 test(s)"), "{out}");
    assert!(out.contains("test addition_works ... ok"), "{out}");
    assert!(out.contains("test runs_to_completion ... ok"), "{out}");
    assert!(
        out.contains("test addition_is_wrong ... FAILED: returned false"),
        "{out}"
    );
    // The child catches the fault and names it only where
    // `codira_healing::trap` can intercept faults (Windows). Elsewhere the
    // child dies of the signal, and the runner reports that as a crash; the
    // test still fails either way.
    let expected = if cfg!(windows) {
        "test divides_by_zero ... FAILED: trapped (IntegerDivideByZero)"
    } else {
        "test divides_by_zero ... FAILED: crashed (signal: 8"
    };
    assert!(out.contains(expected), "{out}");
    assert!(
        out.contains("test result: FAILED. 2 passed; 2 failed"),
        "{out}"
    );

    let (ok, out) = codira(project.path(), &["test", "addition_works"]);
    assert!(ok, "{out}");
    assert!(out.contains("running 1 test(s)"), "{out}");
}

#[test]
fn a_refuted_contract_becomes_a_runnable_failing_test_until_it_is_fixed() {
    let project = tempfile::tempdir().unwrap();
    write_package(
        project.path(),
        r#"
func abs(x: i64) -> i64
    ensures result >= 0
{
    if x < 0 { -x } else { x }
}
"#,
    );
    let generated = project
        .path()
        .join("tests/generated/abs_counterexample.code");

    let (ok, out) = codira(project.path(), &["check"]);
    assert!(!ok, "{out}");
    assert!(out.contains("wrote counterexample test tests"), "{out}");
    let test_source = std::fs::read_to_string(&generated).unwrap();
    assert!(
        test_source.contains("let x = (-9223372036854775807 - 1);"),
        "{test_source}"
    );

    // The generated test reproduces the bug at run time: `abs(i64::MIN)` is
    // negative.
    let (ok, out) = codira(project.path(), &["test"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("contract obligation(s) are not proven; running the tests anyway"),
        "{out}"
    );
    assert!(
        out.contains("test abs_counterexample::abs_counterexample ... FAILED: returned false"),
        "{out}"
    );

    // Fix the contract: the obligation is proven and the test disappears.
    write_package(
        project.path(),
        r#"
func abs(x: i64) -> i64
    requires x > -9223372036854775807 - 1
    ensures result >= 0
{
    if x < 0 { -x } else { x }
}
"#,
    );
    let (ok, out) = codira(project.path(), &["check"]);
    assert!(ok, "{out}");
    assert!(out.contains("removed counterexample test"), "{out}");
    assert!(!generated.exists());
}

#[test]
fn query_lists_functions_that_can_return_a_negative_value() {
    let project = tempfile::tempdir().unwrap();
    write_package(
        project.path(),
        r#"
func abs(x: i64) -> i64 { if x < 0 { -x } else { x } }
func clamp_low(x: i64) -> i64 { if x < 0 { 0 } else { x } }
"#,
    );
    let (ok, out) = codira(project.path(), &["query", "result < 0"]);
    assert!(ok, "{out}");
    assert!(out.contains("mod.code:2: func abs(x: i64) -> i64"), "{out}");
    assert!(
        out.contains("witness: abs(x: -9223372036854775808) returns -9223372036854775808"),
        "{out}"
    );
    assert!(!out.contains("clamp_low"), "{out}");
    assert!(out.contains("1 match(es), 0 unknown"), "{out}");
}
