//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of the heap policy of the `codira` tool, through the
//!   real binary: a program that allocates far more than the heap limit in
//!   total, with a small live set, completes; one whose live set exceeds the
//!   limit ends with the documented diagnostic and exit status; the limit
//!   comes from `--heap-limit`, then `CODIRA_HEAP_LIMIT`, then
//!   `[runtime] heap-limit` in `codira.toml`, then the default; `unlimited`
//!   is warned about; `--runtime-stats` reports the collections; `codira
//!   test` runs its tests under the same policy.

use std::{
    path::Path,
    process::{Command, Output},
};

/// The exit status of a program that ran out of memory
/// (`codira_runtime::HEAP_EXHAUSTED_EXIT_STATUS`).
const HEAP_EXHAUSTED: i32 = 102;

/// A list whose live data grows without end.
const GROW: &str = r#"
public class Node { value: i64, next: Node? }

public func main() {
    println("growing")
    var head: Node? = nil
    var i = 0
    while i < 100000000 {
        head = Node { value: i, next: head }
        i += 1
    }
    println(head!.value)
}

public func small() -> i64 { 42 }
"#;

/// Writes a one-file package whose `src/mod.code` is `source`, with
/// `runtime` as its `[runtime]` table (if any).
fn package_with(source: &str, runtime: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_heap")
        .tempdir()
        .unwrap();
    let mut manifest = "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n".to_string();
    if let Some(runtime) = runtime {
        manifest.push_str("\n[runtime]\n");
        manifest.push_str(runtime);
        manifest.push('\n');
    }
    std::fs::write(dir.path().join("codira.toml"), manifest).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), source).unwrap();
    dir
}

/// Copies the churn example into a fresh directory.
fn churn_example() -> tempfile::TempDir {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/gc_churn");
    let dir = tempfile::Builder::new()
        .prefix("codira_heap")
        .tempdir()
        .unwrap();
    std::fs::copy(source.join("codira.toml"), dir.path().join("codira.toml")).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::copy(source.join("src/mod.code"), dir.path().join("src/mod.code")).unwrap();
    dir
}

/// Runs `codira <command>` on the package in `dir` with `extra` arguments
/// and `env` set (and the heap variables of the test's own environment
/// removed).
fn codira(command: &str, dir: &Path, extra: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codira"));
    cmd.arg(command)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .args(extra)
        .env_remove("CODIRA_HEAP_LIMIT")
        .env_remove("CODIRA_GC_GROWTH");
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.output().expect("failed to spawn codira")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}

/// The churn example allocates more than five times the limit in total, with
/// well under a megabyte live; under an 8 MiB limit it completes, its check
/// value says no live object was lost, and the statistics show the
/// collections that made it possible.
#[test]
fn a_program_allocating_far_more_than_the_limit_completes() {
    let dir = churn_example();
    let output = codira(
        "run",
        dir.path(),
        &["quick", "--heap-limit", "8MiB", "--runtime-stats"],
        &[],
    );
    let err = stderr(&output);
    assert!(output.status.success(), "codira run failed:\n{err}");
    assert!(
        stdout(&output).contains("churn 2000 live, 200000 steps: check 8957513,"),
        "stdout: {}",
        stdout(&output)
    );
    assert!(
        err.contains("heap limit   8.00 MiB, from --heap-limit"),
        "{err}"
    );
    // Far more allocated in total than the limit allows at once.
    let allocated_mib: f64 = err
        .lines()
        .find_map(|line| line.trim().strip_prefix("allocated"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no allocated line\n{err}"));
    assert!(allocated_mib > 5.0 * 8.0, "{err}");
    let collections: u64 = err
        .lines()
        .find_map(|line| line.trim().strip_prefix("collections"))
        .and_then(|rest| rest.trim().split(':').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no collection count in:\n{err}"));
    assert!(collections >= 10, "{collections} collections:\n{err}");
    let peak_mib: f64 = err
        .lines()
        .find_map(|line| line.trim().strip_prefix("heap         "))
        .and_then(|rest| rest.split(" now, ").nth(1))
        .and_then(|rest| rest.split(" MiB peak").next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no peak in:\n{err}"));
    assert!(peak_mib <= 8.0, "peak {peak_mib} MiB:\n{err}");
}

/// A live set beyond the limit: the documented diagnostic (what was asked
/// for, the limit, what is live, where the limit came from) and exit
/// status, after the output printed before.
#[test]
fn a_live_set_beyond_the_limit_fails_with_the_documented_message() {
    let dir = package_with(GROW, None);
    let output = codira("run", dir.path(), &["--heap-limit", "1MiB"], &[]);
    let err = stderr(&output);
    assert_eq!(output.status.code(), Some(HEAP_EXHAUSTED), "stderr:\n{err}");
    assert_eq!(stdout(&output), "growing\n");
    for needle in [
        "fatal: out of memory: cannot allocate `Node` (24 bytes)",
        "  heap limit: 1048576 bytes (1.00 MiB)",
        "  committed:  1048576 bytes; the allocation needed 65536 more",
        "bytes after a full collection",
        "  the limit is 1.00 MiB, from --heap-limit",
        "  raise it with --heap-limit <SIZE>, the CODIRA_HEAP_LIMIT environment variable, or \
         `heap-limit` in the [runtime] table of codira.toml",
    ] {
        assert!(err.contains(needle), "{needle:?} not in:\n{err}");
    }
    let live: usize = err
        .lines()
        .find_map(|line| line.trim().strip_prefix("live:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no live bytes in:\n{err}"));
    assert!(live > 250_000, "live {live}:\n{err}");
}

/// The limit comes from the flag, then the environment, then the manifest,
/// then the default, and the report says which.
#[test]
fn the_flag_then_the_environment_then_the_manifest_set_the_limit() {
    let dir = package_with(GROW, Some("heap-limit = \"3MiB\""));
    let limit_line = |extra: &[&str], env: &[(&str, &str)]| {
        let mut args = vec!["small", "--runtime-stats"];
        args.extend_from_slice(extra);
        let output = codira("run", dir.path(), &args, env);
        let err = stderr(&output);
        assert!(output.status.success(), "codira run failed:\n{err}");
        err.lines()
            .find(|line| line.trim_start().starts_with("heap limit"))
            .map(|line| line.trim().to_string())
            .unwrap_or_else(|| panic!("no heap limit line in:\n{err}"))
    };
    let env = [("CODIRA_HEAP_LIMIT", "2MiB")];
    assert_eq!(
        limit_line(&["--heap-limit", "1MiB"], &env),
        "heap limit   1.00 MiB, from --heap-limit"
    );
    assert_eq!(
        limit_line(&[], &env),
        "heap limit   2.00 MiB, from CODIRA_HEAP_LIMIT"
    );
    let manifest = dir.path().join("codira.toml");
    assert_eq!(
        limit_line(&[], &[]),
        format!(
            "heap limit   3.00 MiB, from `heap-limit` in {}",
            manifest.display()
        )
    );

    let plain = package_with(GROW, None);
    let output = codira("run", plain.path(), &["small", "--runtime-stats"], &[]);
    let err = stderr(&output);
    assert!(output.status.success(), "codira run failed:\n{err}");
    assert!(
        err.contains("(50% of ") && err.contains("), from the default"),
        "{err}"
    );

    // The limit applies: the growing list fails at the manifest's 3 MiB.
    let output = codira("run", dir.path(), &[], &[]);
    let err = stderr(&output);
    assert_eq!(output.status.code(), Some(HEAP_EXHAUSTED), "stderr:\n{err}");
    assert!(err.contains("heap limit: 3145728 bytes"), "{err}");
    assert!(err.contains("from `heap-limit` in"), "{err}");
}

/// `unlimited` must be asked for, and is warned about; the collector still
/// paces.
#[test]
fn unlimited_is_explicit_and_warned_about() {
    let dir = package_with(GROW, None);
    let output = codira(
        "run",
        dir.path(),
        &["small", "--runtime-stats"],
        &[("CODIRA_HEAP_LIMIT", "unlimited")],
    );
    let err = stderr(&output);
    assert!(output.status.success(), "codira run failed:\n{err}");
    assert!(
        err.contains(
            "warning: the garbage-collected heap has no limit (CODIRA_HEAP_LIMIT): a program \
             whose live data keeps growing will grow until the system runs out of memory"
        ),
        "{err}"
    );
    assert!(err.contains("heap limit   unlimited"), "{err}");
    assert!(err.contains("gc growth    100%"), "{err}");
}

/// A value that is not a size is an error that says where it came from,
/// before anything is built.
#[test]
fn a_bad_limit_is_an_error() {
    let dir = package_with(GROW, Some("heap-limit = \"lots\""));
    let output = codira("run", dir.path(), &[], &[]);
    let err = stderr(&output);
    assert!(!output.status.success());
    assert!(
        err.contains("invalid heap limit from `heap-limit` in"),
        "{err}"
    );
    assert!(!stdout(&output).contains("growing"));

    let output = codira("run", dir.path(), &["--heap-limit", "12KiB"], &[]);
    let err = stderr(&output);
    assert!(!output.status.success());
    assert!(
        err.contains("invalid heap limit from --heap-limit"),
        "{err}"
    );
    assert!(err.contains("the least is `64KiB`"), "{err}");
}

/// Under a 64 MiB limit the churn example's collections are paced (the heap
/// never gets near the limit); with `--gc-growth off` (here from the
/// environment) the collector runs only at the limit, and the program still
/// completes.
#[test]
fn pacing_collects_before_the_limit_and_off_collects_at_it() {
    let dir = churn_example();
    let run = |env: &[(&str, &str)]| {
        let output = codira(
            "run",
            dir.path(),
            &["quick", "--heap-limit", "64MiB", "--runtime-stats"],
            env,
        );
        let err = stderr(&output);
        assert!(output.status.success(), "codira run failed:\n{err}");
        assert!(
            stdout(&output).contains("check 8957513,"),
            "{}",
            stdout(&output)
        );
        err
    };
    let paced = run(&[]);
    assert!(
        paced.contains("gc growth    100%, from the default"),
        "{paced}"
    );
    assert!(paced.contains(" 0 at the heap limit"), "{paced}");
    assert!(!paced.contains(": 0 paced, "), "{paced}");

    let off = run(&[("CODIRA_GC_GROWTH", "off")]);
    assert!(
        off.contains("gc growth    off, from CODIRA_GC_GROWTH"),
        "{off}"
    );
    assert!(off.contains(": 0 paced, "), "{off}");
    assert!(!off.contains(" 0 at the heap limit"), "{off}");
}

/// `codira test` runs every test under the package's policy: a test whose
/// live data exceeds the manifest's limit fails with the diagnostic, the
/// others pass.
#[test]
fn codira_test_applies_the_limit_to_each_test() {
    let dir = package_with(
        r#"
public class Node { value: i64, next: Node? }

@test
public func grows_forever() -> bool {
    var head: Node? = nil
    var i = 0
    while i < 100000000 {
        head = Node { value: i, next: head }
        i += 1
    }
    head!.value > 0
}

@test
public func small() -> bool { true }
"#,
        Some("heap-limit = \"2MiB\""),
    );
    let output = codira("test", dir.path(), &[], &[]);
    let out = stdout(&output);
    assert!(!output.status.success(), "{out}");
    assert!(
        out.contains(
            "grows_forever ... FAILED: crashed (exit status: 102): fatal: out of memory: cannot \
             allocate `Node` (24 bytes)"
        ),
        "{out}\n{}",
        stderr(&output)
    );
    assert!(out.contains("small ... ok"), "{out}");
    assert!(out.contains("1 passed; 1 failed"), "{out}");
}
