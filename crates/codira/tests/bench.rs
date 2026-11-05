//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - End-to-end tests of `codira bench`: `@bench` functions in `src/` and
//!   `benches/` are found and measured (with and without an iteration
//!   parameter), a baseline is saved, a slowed-down function is reported as a
//!   regression and fails the command, and a non-public benchmark or an
//!   unsupported signature is reported, not silently skipped.

use std::{path::Path, process::Command};

const FAST: &str = r#"
public func work(n: i64) -> i64 {
    var a = 0
    var i = 0
    while i < n {
        a = (a * 31 + i) % 1000003
        i += 1
    }
    a
}

@bench
public func call_once() -> i64 { work(10) }
"#;

/// `work` with forty times as many iterations (of a loop the optimiser
/// cannot reduce to a closed form).
const SLOW: &str = r#"
public func work(n: i64) -> i64 {
    var a = 0
    var i = 0
    while i < n * 40 {
        a = (a * 31 + i) % 1000003
        i += 1
    }
    a
}

@bench
public func call_once() -> i64 { work(10) }
"#;

const BENCHES: &str = r#"
import root.work

@bench
public func looped(n: i64) -> i64 {
    var acc = 0
    var i = 0
    while i < n {
        acc += work(20 + (i & 1))
        i += 1
    }
    acc
}

@bench
func not_public() -> i64 { 1 }

@bench
public func wrong_signature(a: f64) -> i64 { 1 }
"#;

fn write(dir: &Path, src: &str) {
    std::fs::write(
        dir.join("codira.toml"),
        "[package]\nname=\"timed\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("benches")).unwrap();
    std::fs::write(dir.join("src/mod.code"), src).unwrap();
    std::fs::write(dir.join("benches/work.code"), BENCHES).unwrap();
}

fn bench(dir: &Path, extra: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args([
            "bench",
            "--warm-up-ms",
            "30",
            "--measurement-ms",
            "200",
            "--samples",
            "20",
            "--rounds",
            "2",
        ])
        .args(extra)
        .current_dir(dir)
        .output()
        .expect("could not run codira");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[test]
fn bench_measures_saves_and_detects_a_regression() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), FAST);

    let (ok, out) = bench(dir.path(), &["--save-baseline", "main"]);
    // The two broken benchmarks fail the run; the others are measured.
    assert!(!ok, "{out}");
    assert!(out.contains("call overhead:"), "{out}");
    assert!(out.contains("bench call_once ... median"), "{out}");
    assert!(
        out.contains("bench benches.work.looped ... median"),
        "{out}"
    );
    assert!(out.contains("95% CI ["), "{out}");
    assert!(out.contains("20 samples of"), "{out}");
    assert!(
        out.contains("not_public ... FAILED: a benchmark must be `public`"),
        "{out}"
    );
    assert!(
        out.contains("wrong_signature ... FAILED: a benchmark takes no parameters, or one `i64`"),
        "{out}"
    );
    assert!(out.contains("bench result: 2 measured, 2 failed"), "{out}");
    let saved = dir.path().join("target/bench/baselines/main.json");
    let json = std::fs::read_to_string(&saved).unwrap();
    assert!(json.contains("codira-bench-baseline/1") && json.contains("per_call_ns"));

    // Forty times the work: a regression far beyond any noise.
    write(dir.path(), SLOW);
    let (ok, out) = bench(dir.path(), &["looped", "--baseline", "main"]);
    assert!(!ok, "a regression fails the command:\n{out}");
    assert!(out.contains("REGRESSED"), "{out}");
    assert!(out.contains("1 regressed"), "{out}");
}

#[test]
fn bench_refuses_a_missing_baseline() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), FAST);
    let (ok, out) = bench(dir.path(), &["--baseline", "nope"]);
    assert!(!ok);
    assert!(out.contains("no baseline 'nope'"), "{out}");
}
