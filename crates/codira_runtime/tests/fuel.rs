//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Fuel in ahead-of-time compiled Codira: instrumented code is charged one
//!   unit per function entry and loop iteration, exactly; uninstrumented code
//!   is not metered; running out ends the process with a message (or, under a
//!   HERACLES guard, is caught -- see `codira_healing`'s tests).
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use codira_test::CompileAndRunTestDriver;

const SPIN: &str = r#"
    public func spin(n: i64) -> i64 {
        var i = 0;
        while i < n {
            i += 1;
        }
        i
    }

    public func twice(n: i64) -> i64 {
        spin(n) + spin(n)
    }

    public func forever() -> i64 {
        var i = 0;
        loop {
            i += 1;
        }
    }
"#;

#[test]
fn instrumented_code_is_charged_per_entry_and_iteration() {
    let driver =
        CompileAndRunTestDriver::with_fuel_instrumentation(SPIN, |builder| builder.fuel(1_000_000))
            .expect("Failed to build test driver");
    let runtime = &driver.runtime;
    assert!(runtime.is_fuel_instrumented());

    let result: i64 = runtime.invoke("spin", (1000i64,)).unwrap();
    assert_eq!(result, 1000);
    // One function entry plus 1000 loop back-edges
    assert_eq!(runtime.fuel_remaining(), Some(1_000_000 - 1001));

    let result: i64 = runtime.invoke("twice", (10i64,)).unwrap();
    assert_eq!(result, 20);
    // `twice` and two calls of `spin`, 10 iterations each
    assert_eq!(runtime.fuel_remaining(), Some(1_000_000 - 1001 - 23));

    runtime.set_fuel(None);
    let _: i64 = runtime.invoke("spin", (5000i64,)).unwrap();
    assert_eq!(runtime.fuel_remaining(), None);
}

#[test]
fn uninstrumented_code_is_not_metered() {
    let driver = CompileAndRunTestDriver::new(SPIN, |builder| builder.fuel(10))
        .expect("Failed to build test driver");
    assert!(!driver.runtime.is_fuel_instrumented());
    let result: i64 = driver.runtime.invoke("spin", (1000i64,)).unwrap();
    assert_eq!(result, 1000);
    assert_eq!(driver.runtime.fuel_remaining(), Some(10));
}

#[test]
fn instrumented_code_without_a_budget_runs_unlimited() {
    let driver = CompileAndRunTestDriver::with_fuel_instrumentation(SPIN, |builder| builder)
        .expect("Failed to build test driver");
    let result: i64 = driver.runtime.invoke("spin", (100_000i64,)).unwrap();
    assert_eq!(result, 100_000);
    assert_eq!(driver.runtime.fuel_remaining(), None);
}

/// Environment variable that makes [`exhaustion_child`] do its work.
const CHILD_ENV: &str = "CODIRA_FUEL_EXHAUSTION_CHILD";

/// Runs in a child process: an endless loop with a budget of 10,000.
#[test]
fn exhaustion_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let driver =
        CompileAndRunTestDriver::with_fuel_instrumentation(SPIN, |builder| builder.fuel(10_000))
            .expect("Failed to build test driver");
    let _: i64 = driver.runtime.invoke("forever", ()).unwrap();
    unreachable!("an endless loop cannot return");
}

#[test]
fn an_endless_loop_ends_when_its_fuel_does() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "exhaustion_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run the child test");

    // The child must end on its own; never let a regression hang the suite.
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("the child did not end when its fuel ran out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "child succeeded:\n{stderr}");
    assert!(
        stderr.contains("codira runtime: fuel exhausted"),
        "unexpected child output:\n{stderr}"
    );
    #[cfg(windows)]
    assert_eq!(
        output.status.code().map(|code| code as u32),
        Some(codira_runtime::FUEL_EXHAUSTED_EXCEPTION_CODE),
        "an unhandled fuel exception ends the process with its code"
    );
}
