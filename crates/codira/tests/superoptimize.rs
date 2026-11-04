//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests of `codira build --superoptimize` (experimental): the
//!   real `codira` binary builds a package, rewrites what it can prove
//!   cheaper, leaves constant-time functions alone, reports every rewrite,
//!   and the library still computes the same results.

use std::{path::Path, process::Command};

const SOURCE: &str = r#"
public func mix(x: i64, y: i64) -> i64 {
    (x ^ y) + ((x & y) << 1)
}

public func mul9_add(x: i64, y: i64) -> i64 {
    x * 9 + y
}

oblivious func ct_mix(x: u64, y: u64) -> u64 {
    (x ^ y) + ((x & y) << 1)
}

public func main() {
    var acc = 0
    var i = 0
    while i < 1000 {
        acc = acc + mix(i, acc) + mul9_add(i, 3)
        i += 1
    }
    println(acc)
    println(declassify(ct_mix(7u64, 9u64)))
}
"#;

fn package() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_superopt")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), SOURCE).unwrap();
    dir
}

fn codira(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .output()
        .expect("failed to spawn codira")
}

fn start(dir: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("start")
        .arg(dir.join("target/mod.codiralib"))
        .arg("main")
        .output()
        .expect("failed to spawn codira");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn superoptimize_rewrites_proven_cheaper_tails_and_keeps_results() {
    let dir = package();
    // `--check-constant-time` keeps function symbols, as `--superoptimize`
    // does, so the two libraries differ only where code was rewritten.
    let plain = codira(dir.path(), &["build", "-O", "3", "--check-constant-time"]);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let expected = start(dir.path());
    let plain_bytes = std::fs::read(dir.path().join("target/mod.codiralib")).unwrap();

    let out = codira(
        dir.path(),
        &[
            "build",
            "-O",
            "3",
            "--superoptimize",
            "--check-constant-time",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    // LLVM compiles `mix` to four instructions; it is `x + y`.
    let line = stderr
        .lines()
        .find(|l| l.starts_with("superoptimiser: `mix`"))
        .unwrap_or_else(|| panic!("no rewrite of mix reported:\n{stderr}"));
    assert!(
        line.contains("=> lea rax, [rdi + rsi]") && line.contains("proven by Z3"),
        "{line}"
    );
    // `mul9_add` is already `lea` + `add`; the constant-time copy of `mix`
    // must not be touched.
    assert!(!stderr.contains("`mul9_add` at"), "{stderr}");
    assert!(!stderr.contains("`ct_mix` at"), "{stderr}");
    assert!(
        stderr.contains("1 constant-time function(s) left alone"),
        "{stderr}"
    );

    let optimized = std::fs::read(dir.path().join("target/mod.codiralib")).unwrap();
    assert_ne!(optimized, plain_bytes, "the library was not changed");
    assert_eq!(
        optimized.len(),
        plain_bytes.len(),
        "a rewrite changes no size"
    );
    let changed = optimized
        .iter()
        .zip(&plain_bytes)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        changed <= 13,
        "{changed} bytes changed; `mix`'s tail is 13 bytes"
    );
    assert_eq!(start(dir.path()), expected);

    // A second build answers from the cache with the same rewrite.
    let again = codira(
        dir.path(),
        &[
            "build",
            "-O",
            "3",
            "--superoptimize",
            "--check-constant-time",
        ],
    );
    let stderr2 = String::from_utf8_lossy(&again.stderr);
    assert!(again.status.success(), "{stderr2}");
    assert!(stderr2.contains(line), "{stderr2}");
    assert_eq!(
        std::fs::read(dir.path().join("target/mod.codiralib")).unwrap(),
        optimized
    );
}

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn superoptimize_and_the_constant_time_check_compose() {
    // The rewrite runs first and leaves constant-time code alone, so the
    // machine-code check still passes.
    let dir = package();
    let out = codira(
        dir.path(),
        &[
            "build",
            "-O",
            "2",
            "--superoptimize",
            "--check-constant-time",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("superoptimiser: `mix`"), "{stderr}");
    assert!(stderr.contains("no rule broken"), "{stderr}");
}

#[test]
fn other_targets_are_skipped_with_a_note() {
    let dir = package();
    let out = codira(
        dir.path(),
        &[
            "build",
            "--target",
            "aarch64-unknown-linux-gnu",
            "--superoptimize",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("rewrites x86-64 code only"), "{stderr}");
}
