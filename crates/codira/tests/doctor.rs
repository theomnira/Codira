//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - End-to-end tests of `codira doctor`: the linked toolchain passes, a broken
//!   standard-library path and an unsupported target fail with the exact fix,
//!   and every problem reported comes with a fix.

use std::process::Command;

fn doctor(args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codira"));
    cmd.arg("doctor").args(args).current_dir(dir.path());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("could not run codira");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Every `[warn]` or `[FAIL]` line is followed by at least one `fix:` line.
fn every_problem_has_a_fix(out: &str) {
    let lines: Vec<&str> = out.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("[warn]") || line.starts_with("[FAIL]") {
            assert!(
                lines
                    .get(i + 1)
                    .is_some_and(|l| l.trim_start().starts_with("fix:")),
                "no fix for: {line}\n{out}"
            );
        }
    }
}

#[test]
fn doctor_passes_the_linked_toolchain() {
    let (_, out) = doctor(&[], &[]);
    assert!(out.contains("[ok  ] LLVM") && out.contains("22."), "{out}");
    assert!(out.contains("[ok  ] Z3"), "{out}");
    assert!(out.contains("[ok  ] target"), "{out}");
    assert!(out.contains("[ok  ] LLD"), "{out}");
    assert!(out.contains("disk space"), "{out}");
    every_problem_has_a_fix(&out);
}

#[test]
fn doctor_reports_the_fix_for_each_problem() {
    let (ok, out) = doctor(
        &["--target", "sparc-sun-solaris"],
        &[
            ("CODIRA_STD_PATH", "/no/such/std"),
            ("CODIRA_CLANG", "/no/such/clang"),
        ],
    );
    assert!(!ok, "a failed check fails the command:\n{out}");
    assert!(out.contains("[FAIL] std library"), "{out}");
    assert!(out.contains("fix: set CODIRA_STD_PATH="), "{out}");
    assert!(
        out.contains("[FAIL] target") && out.contains("x86_64-unknown-linux-gnu"),
        "{out}"
    );
    // Outside a package with [native] sources a missing C compiler is a
    // warning, not a failure.
    assert!(out.contains("[warn] C compiler"), "{out}");
    assert!(out.contains("CODIRA_CLANG"), "{out}");
    every_problem_has_a_fix(&out);
}
