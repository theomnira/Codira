//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests of the Codira Semantic Envelope (experimental): a
//!   package whose `@envelope`s hold checks, builds and reports a met
//!   envelope; `codira check` rejects a broken one with the call chain;
//!   `codira envelope --json` emits the `codira-envelope/0` schema and
//!   `--check` fails on a broken declaration.

use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const KEPT: &str = r#"
@envelope(allocation: none, panic: false, termination: bounded, foreign: none, concurrency: none)
func step(x: i64) -> i64 { x * 3 + 1 }

@envelope(allocation: none, foreign: none)
public func run(n: i64) -> i64 {
    var acc = 0
    for i in 0..n { acc += step(i) }
    acc
}
"#;

const BROKEN: &str = r#"
class Buffer { len: i64 }
func make(n: i64) -> i64 { let b = Buffer { len: n }; b.len }

@envelope(allocation: none)
public func run(n: i64) -> i64 { make(n) }
"#;

fn write_package(dir: &Path, source: &str) {
    std::fs::write(
        dir.join("codira.toml"),
        "[package]\nname=\"envelope\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/mod.code"), source).unwrap();
}

/// Runs the `codira` binary with a hard timeout.
fn codira(dir: &Path, args: &[&str]) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(300);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("`codira {}` did not finish within 300 s", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    let text = format!(
        "{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

#[test]
fn kept_envelopes_check_build_and_report() {
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), KEPT);
    let (ok, out) = codira(project.path(), &["check"]);
    assert!(ok, "{out}");
    let (ok, out) = codira(project.path(), &["build"]);
    assert!(ok, "{out}");
    let (ok, out) = codira(project.path(), &["envelope", "--check"]);
    assert!(ok, "{out}");
    assert!(out.contains("2 with a declared envelope, 0 not met"), "{out}");

    let (ok, out) = codira(project.path(), &["envelope", "--json"]);
    assert!(ok, "{out}");
    let json_text = out.split("\n--- stderr ---").next().unwrap();
    let json: serde_json::Value = serde_json::from_str(json_text).unwrap();
    assert_eq!(json["schema"], "codira-envelope/0");
    let step = json["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "step")
        .unwrap();
    assert_eq!(step["basis"], "static-analysis");
    assert_eq!(step["inferred"]["allocation"]["value"], "none");
    assert_eq!(step["inferred"]["termination"]["value"], "bounded");
    assert_eq!(step["violated"].as_array().unwrap().len(), 0);
}

#[test]
fn a_broken_envelope_fails_check_with_the_call_chain() {
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), BROKEN);
    let (ok, out) = codira(project.path(), &["check"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains(
            "`run` is `@envelope(allocation: none)` but calls `make` (line 6), which allocates: \
             class `Buffer` constructed"
        ),
        "{out}"
    );
    let (ok, out) = codira(project.path(), &["envelope", "--check"]);
    assert!(!ok, "{out}");
    assert!(out.contains("(NOT MET)"), "{out}");
    // Without `--check` the report is informational.
    let (ok, _) = codira(project.path(), &["envelope"]);
    assert!(ok);
}
