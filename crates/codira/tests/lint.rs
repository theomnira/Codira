//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - End-to-end tests of `codira lint`: each lint reports exactly the sites the
//!   compiler establishes, and none of the near misses beside them (an
//!   allocation after a loop, a `for` loop's iterable, a function used as a
//!   value, a proven or literal divisor, a small struct, a silenced item);
//!   `--allow`, `--deny` and `--warn` behave as documented.

use std::{path::Path, process::Command};

const SOURCE: &str = r#"class Particle { x: f64 }

struct Big {
    a: f64, b: f64, c: f64, d: f64, e: f64, f: f64, g: f64, h: f64,
    i: f64, j: f64, k: f64, l: f64, m: f64, n: f64, o: f64, p: f64,
}

struct Small { a: f64, b: f64 }

public func spawn_all(n: i64) -> i64 {
    var i = 0
    var total = 0
    while i < n {
        let p = Particle { x: 1.0 }
        total += make(i)
        i += 1
    }
    let q = Particle { x: 2.0 }
    for k in 0..n {
        total += k
    }
    total
}

func make(i: i64) -> i64 {
    let p = Particle { x: 0.0 }
    i
}

public func weigh(b: Big) -> f64 { b.a }

public func light(s: Small) -> f64 { s.a }

func dead() -> i64 { 1 }

func by_value() -> i64 { 2 }

public func caller() -> i64 {
    let f = by_value
    f()
}

public func divide(a: i64, b: i64) -> i64 { a / b }

public func divide_checked(a: i64, b: i64) -> i64
    requires b > 0
{
    a / b
}

public func half(a: i64) -> i64 { a / 2 }

public func add(a: i64, b: i64) -> i64 { a + b }

extern "C" {
    @noalloc
    func malloc(n: u64) -> u64;
    func keep(p: Particle);
}

public func use_c() -> u64 {
    keep(Particle { x: 3.0 })
    malloc(8)
}

@asm(arch: "x86_64", "nop", constraints: "", pure: true)
extern func nothing();

public func call_nothing() { nothing() }

// codira-lint: allow(unused)
func silenced() -> i64 { 3 }
"#;

fn package() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname=\"lints\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), SOURCE).unwrap();
    dir
}

fn lint(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("lint")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("could not run codira");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// `(lint, line)` of every finding.
fn findings(out: &str) -> Vec<(String, u32)> {
    out.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("warning[")?;
            let (lint, rest) = rest.split_once("]: mod.code:")?;
            let line = rest.split(':').next()?.parse().ok()?;
            Some((lint.to_string(), line))
        })
        .collect()
}

fn expected(pairs: &[(&str, u32)]) -> Vec<(String, u32)> {
    pairs.iter().map(|(l, n)| (l.to_string(), *n)).collect()
}

#[test]
fn lint_reports_exactly_what_the_compiler_establishes() {
    let dir = package();
    let (ok, out) = lint(dir.path(), &[]);
    assert!(ok, "lints are warnings unless denied:\n{out}");
    assert_eq!(
        findings(&out),
        expected(&[
            ("alloc-in-loop", 14),
            ("alloc-in-loop", 15),
            ("large-copy", 30),
            ("unused", 34),
            ("unproven-division", 43),
            ("extern-assertion", 57),
            ("extern-gc-reference", 58),
            ("asm", 67),
        ]),
        "{out}"
    );
    assert!(out.contains("class `Particle` constructed"), "{out}");
    assert!(out.contains("at least 128 bytes"), "{out}");
    assert!(out.contains("add `requires b != 0`"), "{out}");
}

#[test]
fn lint_noalloc_candidates_are_opt_in() {
    let dir = package();
    let (_, out) = lint(
        dir.path(),
        &["--warn", "noalloc-candidate", "--allow", "all"],
    );
    // `--allow all` wins over everything, including what --warn enabled.
    assert!(findings(&out).is_empty(), "{out}");
    let (_, out) = lint(dir.path(), &["--warn", "noalloc-candidate"]);
    let candidates: Vec<u32> = findings(&out)
        .into_iter()
        .filter(|(l, _)| l == "noalloc-candidate")
        .map(|(_, n)| n)
        .collect();
    // Public, with a body, never allocating; not `spawn_all`, `caller` or
    // `use_c`, which allocate.
    assert_eq!(candidates, [30, 32, 43, 45, 51, 53], "{out}");
}

#[test]
fn lint_allow_and_deny() {
    let dir = package();
    let (ok, out) = lint(dir.path(), &["--deny", "unused"]);
    assert!(!ok, "a denied lint that reports fails the command:\n{out}");
    let (ok, out) = lint(dir.path(), &["--deny", "unused", "--allow", "unused"]);
    assert!(ok, "{out}");
    assert!(!findings(&out).iter().any(|(l, _)| l == "unused"));
    let (ok, out) = lint(dir.path(), &["--allow", "nonsense"]);
    assert!(!ok);
    assert!(out.is_empty());
    let (ok, out) = lint(dir.path(), &["--list"]);
    assert!(
        ok && out.contains("alloc-in-loop") && out.contains("noalloc-candidate"),
        "{out}"
    );
}
