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
//! - Version 1: a package `[envelope]` in `codira.toml` that a public
//!   function leaves fails `check` and `build` with the call chain, and one
//!   that every public function fits passes; `build --certify` records each
//!   function's envelope and `codira certify` re-derives it, rejecting a
//!   tampered one; `codira api-diff` calls a widened envelope major.

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
    write_package_with(dir, source, "0.1.0", "");
}

/// A package at `version` whose manifest ends with `extra` (e.g. an
/// `[envelope]` table).
fn write_package_with(dir: &Path, source: &str, version: &str, extra: &str) {
    std::fs::write(
        dir.join("codira.toml"),
        format!("[package]\nname=\"envelope\"\nauthors=[]\nversion=\"{version}\"\n{extra}"),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/mod.code"), source).unwrap();
}

/// No `@envelope` anywhere: the package envelope alone constrains it.
const UNANNOTATED_BROKEN: &str = r#"
class Buffer { len: i64 }
func make(n: i64) -> i64 { let b = Buffer { len: n }; b.len }
func log_it() { println("private, and no public function calls it") }

public func run(n: i64) -> i64 { make(n) }
public func twice(x: i64) -> i64 { x * 2 }
"#;

const UNANNOTATED_KEPT: &str = r#"
func step(x: i64) -> i64 { x * 3 + 1 }
func log_it() { println("private, and no public function calls it") }

public func run(n: i64) -> i64 {
    var acc = 0
    for i in 0..n { acc += step(i) }
    acc
}
"#;

const PACKAGE_ENVELOPE: &str =
    "\n[envelope]\nallocation = \"none\"\nblocking = false\nforeign = \"none\"\n";

/// Runs the `codira` binary on the package in `dir` with a hard timeout.
fn codira(dir: &Path, args: &[&str]) -> (bool, String) {
    let manifest = dir.join("codira.toml");
    let manifest = manifest.to_str().unwrap();
    let mut all = args.to_vec();
    all.extend(["--manifest-path", manifest]);
    codira_raw(&all)
}

/// Runs the `codira` binary with exactly `args`, with a hard timeout.
fn codira_raw(args: &[&str]) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
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
    assert!(
        out.contains("2 with a declared envelope, 0 not met"),
        "{out}"
    );

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

#[test]
fn a_package_envelope_a_public_function_leaves_fails_check_and_build() {
    let project = tempfile::tempdir().unwrap();
    write_package_with(
        project.path(),
        UNANNOTATED_BROKEN,
        "0.1.0",
        PACKAGE_ENVELOPE,
    );
    let (ok, out) = codira(project.path(), &["check"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains(
            "public function `run` (mod.code:6) does not fit the package envelope \
             (`[envelope]` in codira.toml: allocation = \"none\", blocking = false): it \
             allocates and may block: calls `make` (line 6); class `Buffer` constructed at line 3"
        ),
        "{out}"
    );
    // `twice` fits, and the private `log_it` is reached by no public
    // function, so neither is reported.
    assert!(
        !out.contains("`twice`") && !out.contains("`log_it`"),
        "{out}"
    );

    let (ok, out) = codira(project.path(), &["build"]);
    assert!(!ok, "{out}");
    assert!(out.contains("public function `run`"), "{out}");
    assert!(out.contains("nothing was built"), "{out}");
    assert!(!project.path().join("target/mod.codiralib").exists());

    let (ok, out) = codira(project.path(), &["envelope", "--check"]);
    assert!(!ok, "{out}");
    assert!(out.contains("package envelope (codira.toml)"), "{out}");
    assert!(out.contains("(NOT MET)"), "{out}");
    let (ok, out) = codira(project.path(), &["envelope", "--json"]);
    assert!(ok, "{out}");
    let json: serde_json::Value =
        serde_json::from_str(out.split("\n--- stderr ---").next().unwrap()).unwrap();
    let package = &json["package_envelope"];
    assert_eq!(package["declared"]["allocation"], "none");
    assert_eq!(package["violations"][0]["name"], "run");
    assert_eq!(package["violations"].as_array().unwrap().len(), 1);
}

#[test]
fn a_met_package_envelope_passes_and_a_bad_key_is_an_error() {
    let project = tempfile::tempdir().unwrap();
    write_package_with(project.path(), UNANNOTATED_KEPT, "0.1.0", PACKAGE_ENVELOPE);
    let (ok, out) = codira(project.path(), &["check"]);
    assert!(ok, "{out}");
    let (ok, out) = codira(project.path(), &["build"]);
    assert!(ok, "{out}");
    let (ok, out) = codira(project.path(), &["envelope", "--check"]);
    assert!(ok, "{out}");
    assert!(out.contains("(met by every public function)"), "{out}");

    // An unchecked key is rejected, never ignored.
    write_package_with(
        project.path(),
        UNANNOTATED_KEPT,
        "0.1.0",
        "\n[envelope]\nallocation = \"none\"\nnetwork = \"none\"\n",
    );
    let (ok, out) = codira(project.path(), &["check"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains(
            "codira.toml: `[envelope]` key `network` is part of the semantic-envelope design \
             but cannot be checked yet"
        ),
        "{out}"
    );
    let (ok, out) = codira(project.path(), &["build"]);
    assert!(!ok, "{out}");
    assert!(out.contains("`[envelope]` key `network`"), "{out}");
}

fn certificate_text(project: &Path) -> String {
    std::fs::read_to_string(project.join("target/mod.codiralib.cert.json")).unwrap()
}

#[test]
fn certified_envelopes_are_rederived_and_a_tampered_one_is_rejected() {
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), KEPT);
    let (ok, out) = codira(project.path(), &["build", "--certify"]);
    assert!(ok, "{out}");
    let library = project.path().join("target/mod.codiralib");
    let library = library.to_str().unwrap();

    let certificate =
        codira_certificate::Certificate::from_json(&certificate_text(project.path())).unwrap();
    assert_eq!(certificate.format, "codira-certificate/2");
    assert!(certificate
        .envelope_basis
        .as_deref()
        .unwrap()
        .contains("not a statement about the machine code"));
    let step = certificate
        .functions
        .iter()
        .find(|f| f.symbol == "step")
        .unwrap();
    let envelope = step.envelope.as_ref().unwrap();
    assert_eq!(envelope.len(), 8, "{envelope:?}");
    assert_eq!(envelope["allocation"], "none");
    assert_eq!(envelope["termination"], "bounded");

    // Round trip: the recorded envelopes are the ones the source gives.
    let (ok, out) = codira(project.path(), &["certify", library]);
    assert!(ok, "{out}");
    assert!(
        out.contains(&format!(
            "{n} semantic envelope(s) recorded, {n} re-derived from the source and equal",
            n = certificate.functions.len()
        )),
        "{out}"
    );

    // Away from its source the certificate still checks, but says the
    // envelopes were not re-derived; `--require-envelopes` makes that fatal.
    let elsewhere = tempfile::tempdir().unwrap();
    let copy = elsewhere.path().join("mod.codiralib");
    std::fs::copy(library, &copy).unwrap();
    std::fs::write(
        elsewhere.path().join("mod.codiralib.cert.json"),
        certificate_text(project.path()),
    )
    .unwrap();
    let copy = copy.to_str().unwrap();
    let (ok, out) = codira_raw(&["certify", copy]);
    assert!(ok, "{out}");
    assert!(out.contains("NOT re-derived"), "{out}");
    let (ok, out) = codira_raw(&["certify", "--require-envelopes", copy]);
    assert!(!ok, "{out}");

    // One recorded value changed: certification fails and names it.
    let mut tampered = certificate.clone();
    let run = tampered
        .functions
        .iter_mut()
        .find(|f| f.symbol == "run")
        .unwrap();
    run.envelope
        .as_mut()
        .unwrap()
        .insert("termination".to_string(), "any".to_string());
    std::fs::write(
        project.path().join("target/mod.codiralib.cert.json"),
        tampered.to_json(),
    )
    .unwrap();
    let (ok, out) = codira(project.path(), &["certify", library]);
    assert!(!ok, "{out}");
    assert!(
        out.contains(
            "`run`: its recorded envelope is not the one the source has: `termination` is \
             recorded as any but the source gives bounded"
        ),
        "{out}"
    );

    // A removed envelope fails too, without the source.
    let mut stripped = certificate.clone();
    stripped.functions[0].envelope = None;
    std::fs::write(
        project.path().join("target/mod.codiralib.cert.json"),
        stripped.to_json(),
    )
    .unwrap();
    let (ok, out) = codira(project.path(), &["certify", library]);
    assert!(!ok, "{out}");
    assert!(out.contains("has none"), "{out}");

    // The source changed after the build: the envelope no longer matches.
    std::fs::write(
        project.path().join("target/mod.codiralib.cert.json"),
        certificate.to_json(),
    )
    .unwrap();
    std::fs::write(
        project.path().join("src/mod.code"),
        KEPT.replace("x * 3 + 1", "{ println(\"hi\"); x * 3 + 1 }"),
    )
    .unwrap();
    let (ok, out) = codira(project.path(), &["certify", library]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("`allocation` is recorded as none but the source gives any"),
        "{out}"
    );
}

#[test]
fn api_diff_calls_a_widened_envelope_major_and_a_narrowed_one_minor() {
    let quiet = r#"
func helper(x: i64) -> i64 { x + 1 }
public func f(x: i64) -> i64 { helper(x) }
"#;
    let noisy = r#"
func helper(x: i64) -> i64 { println("hi"); x + 1 }
public func f(x: i64) -> i64 { helper(x) }
"#;
    let old = tempfile::tempdir().unwrap();
    let new = tempfile::tempdir().unwrap();
    write_package_with(old.path(), quiet, "1.0.0", "");
    write_package_with(new.path(), noisy, "1.1.0", "");
    let (old_dir, new_dir) = (old.path().to_str().unwrap(), new.path().to_str().unwrap());

    let (ok, out) = codira(new.path(), &["api-diff", old_dir, new_dir]);
    assert!(ok, "{out}");
    assert!(
        out.contains("major: widens the semantic envelope of `f()` (allocation: none -> any"),
        "{out}"
    );
    assert!(out.contains("required bump: major"), "{out}");
    // 1.0.0 -> 1.1.0 is not enough for a widened envelope.
    let (ok, out) = codira(new.path(), &["api-diff", "--check", old_dir, new_dir]);
    assert!(!ok, "{out}");

    // The other way round it is a minor change, and a minor bump is enough.
    write_package_with(old.path(), noisy, "1.0.0", "");
    write_package_with(new.path(), quiet, "1.1.0", "");
    let (ok, out) = codira(new.path(), &["api-diff", "--check", old_dir, new_dir]);
    assert!(ok, "{out}");
    assert!(
        out.contains("minor: narrows the semantic envelope of `f()` (allocation: any -> none"),
        "{out}"
    );
    assert!(out.contains("required bump: minor"), "{out}");
}
