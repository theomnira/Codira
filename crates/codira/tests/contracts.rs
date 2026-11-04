//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - End-to-end tests of verified contracts: a package whose functions carry
//!   `requires`/`ensures`/`invariant`/`decreases`/`ghost let` builds through
//!   the whole pipeline, runs with the contracts erased, and a
//!   `@runtime_checked` precondition traps at run time.

use std::{ffi::OsString, path::Path};

use codira::run_with_args;
use codira_runtime::Runtime;

const VERIFIED: &str = r#"
public type Positive = i64 { v | v > 0 }

public func half(x: Positive) -> i64
    ensures result >= 0 && result < x
{
    x / 2
}

public func sum_to(n: i64) -> i64
    requires n >= 0 && n <= 1000
    ensures result >= 0
{
    ghost let limit = n
    var i = 0
    var s = 0
    while i < n
        invariant i >= 0 && i <= limit
        invariant s >= 0 && s <= i * 1000
        decreases n - i
    {
        i += 1
        s += i
    }
    s
}

public func call_half() -> i64 {
    half(9)
}

@runtime_checked
public func checked_inverse(x: i64) -> i64
    requires x != 0 && x != -1
{
    1000 / x
}
"#;

fn write_package(dir: &Path, source: &str) {
    std::fs::write(
        dir.join("codira.toml"),
        "[package]\nname=\"verified\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/mod.code"), source).unwrap();
}

fn build(dir: &Path) -> codira::ExitStatus {
    let args: Vec<OsString> = vec![
        "codira".into(),
        "build".into(),
        "--manifest-path".into(),
        dir.join("codira.toml").into(),
    ];
    run_with_args(args).unwrap()
}

#[test]
fn verified_contracts_build_and_run_with_the_contracts_erased() {
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), VERIFIED);
    assert_eq!(build(project.path()), codira::ExitStatus::Success);

    let library = project.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    let sum: i64 = runtime.invoke("sum_to", (10i64,)).unwrap();
    assert_eq!(sum, 55);
    let half: i64 = runtime.invoke("call_half", ()).unwrap();
    assert_eq!(half, 4);
    let inverse: i64 = runtime.invoke("checked_inverse", (8i64,)).unwrap();
    assert_eq!(inverse, 125);

    // A host call bypasses every static check; the `@runtime_checked`
    // precondition is what stops it, with a trap rather than a division by
    // zero.
    //
    // Catching that trap needs `codira_healing::trap`, whose fault
    // interception exists only on Windows (vectored exception handlers);
    // elsewhere `protected` runs the closure unprotected and the trap ends
    // the test process with SIGILL. So the trap is only provoked where it
    // can be observed. The checks above run on every platform.
    if cfg!(windows) {
        expect_entry_trap(&runtime);
    }
}

fn expect_entry_trap(runtime: &Runtime) {
    codira_healing::trap::install();
    let outcome = codira_healing::trap::protected(|| {
        let _: i64 = runtime.invoke("checked_inverse", (0i64,)).unwrap();
    });
    let fault = outcome.expect_err("x = 0 must trap on entry");
    assert_eq!(
        fault.kind,
        codira_healing::trap::FaultKind::IllegalInstruction,
        "llvm.trap, not the division"
    );
}

#[test]
fn a_refuted_contract_fails_the_build() {
    let project = tempfile::tempdir().unwrap();
    write_package(
        project.path(),
        r#"
public func abs(x: i64) -> i64
    ensures result >= 0
{
    if x < 0 { -x } else { x }
}
"#,
    );
    assert_eq!(build(project.path()), codira::ExitStatus::Error);
    assert!(!project.path().join("target/mod.codiralib").exists());
}

/// Units of measure exist only in the front end: a package that uses them
/// builds and computes exactly what the same code without them would.
#[test]
fn units_of_measure_are_erased_before_code_generation() {
    let project = tempfile::tempdir().unwrap();
    write_package(
        project.path(),
        r#"
public type Speed = f64 unit(m / s);

public func speed(d: f64 unit(m), t: f64 unit(s)) -> Speed {
    d / t
}

public func trip() -> f64 unit(m) {
    let v = speed(100.0, 8.0);
    let t: f64 unit(s) = 2.0;
    v * t
}
"#,
    );
    assert_eq!(build(project.path()), codira::ExitStatus::Success);
    let library = project.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    let distance: f64 = runtime.invoke("trip", ()).unwrap();
    assert!((distance - 25.0).abs() < 1e-9, "{distance}");
}
