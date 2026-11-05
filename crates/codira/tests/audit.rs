//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - End-to-end tests of `codira audit`: the licence fields, each native source
//!   with its SHA-256 and SPDX identifier, the extern declarations with their
//!   unchecked trusted assertions, and the standard-library modules imported; a
//!   missing native source fails the audit, and `--strict` also fails on a
//!   missing SPDX identifier.

use std::{path::Path, process::Command};

const MANIFEST: &str = r#"[package]
name = "audited"
version = "0.3.0"
authors = []
license = "Apache-2.0"
license-file = "LICENSE"

[native]
sources = ["c/vec.c", "c/fast.S"]
include = ["c/include"]
defines = ["FAST=1"]
libs = ["m"]
"#;

const SOURCE: &str = r#"import std.math.math

extern "C" {
    @noalloc
    func vec_sum(a: i64, b: i64) -> i64;
    func cos(x: f64) -> f64;
}

@asm(arch: "x86_64", "lea ($1,$2), $0", constraints: "=r,r,r", pure: true)
@asm(arch: "aarch64", "add $0, $1, $2", constraints: "=r,r,r", pure: true)
extern func add(a: i64, b: i64) -> i64;

public func total(a: i64, b: i64) -> i64 { vec_sum(a, b) + add(a, b) }
"#;

fn package(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("c/include")).unwrap();
    std::fs::write(dir.join("codira.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("LICENSE"), "licence text\n").unwrap();
    std::fs::write(
        dir.join("c/vec.c"),
        "// SPDX-License-Identifier: Apache-2.0\nlong vec_sum(long a, long b) { return a + b; }\n",
    )
    .unwrap();
    std::fs::write(dir.join("c/fast.S"), ".globl nothing\nnothing:\n  ret\n").unwrap();
    std::fs::write(dir.join("src/mod.code"), SOURCE).unwrap();
}

fn audit(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("audit")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("could not run codira");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[test]
fn audit_reports_native_sources_externs_and_trusted_assertions() {
    let dir = tempfile::tempdir().unwrap();
    package(dir.path());
    let (ok, out) = audit(dir.path(), &[]);
    assert!(ok, "{out}");
    assert!(out.contains("license: Apache-2.0"), "{out}");
    // sha256("licence text\n")
    assert!(out.contains("license-file: LICENSE (sha256 "), "{out}");
    assert!(out.contains("c/vec.c [C] 86 bytes, sha256 "), "{out}");
    assert!(out.contains("SPDX: Apache-2.0"), "{out}");
    assert!(
        out.contains("c/fast.S [assembly]") && out.contains("SPDX: none"),
        "{out}"
    );
    assert!(
        out.contains("system libraries (linked by name, not checked): m"),
        "{out}"
    );
    assert!(out.contains("mod.code:5: vec_sum (abi \"C\")"), "{out}");
    assert!(out.contains("trusted, unchecked: @noalloc"), "{out}");
    assert!(out.contains("mod.code:6: cos (abi \"C\")"), "{out}");
    // One assertion per @asm variant.
    assert_eq!(out.matches("trusted, unchecked: @asm(").count(), 2, "{out}");
    assert!(out.contains("3 unchecked trusted assertion(s)"), "{out}");
    assert!(out.contains("std.math.math"), "{out}");
    assert!(
        out.contains("note: native source 'c/fast.S' declares no SPDX"),
        "{out}"
    );

    let (ok, json) = audit(dir.path(), &["--json"]);
    assert!(ok);
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["schema"], "codira-audit/1");
    assert_eq!(json["trusted_assertions"], 3);
    assert_eq!(json["native"]["sources"][0]["spdx"], "Apache-2.0");
    assert_eq!(
        json["native"]["sources"][0]["sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
}

#[test]
fn audit_fails_on_what_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    package(dir.path());
    let (ok, out) = audit(dir.path(), &["--strict"]);
    assert!(!ok, "--strict fails on a missing SPDX identifier:\n{out}");
    assert!(
        out.contains("error: native source 'c/fast.S' declares no SPDX"),
        "{out}"
    );

    std::fs::remove_file(dir.path().join("c/vec.c")).unwrap();
    std::fs::remove_file(dir.path().join("LICENSE")).unwrap();
    let (ok, out) = audit(dir.path(), &[]);
    assert!(!ok, "{out}");
    assert!(out.contains("c/vec.c [C] MISSING"), "{out}");
    assert!(
        out.contains("error: native source 'c/vec.c' does not exist"),
        "{out}"
    );
    assert!(
        out.contains("error: license-file 'LICENSE' does not exist"),
        "{out}"
    );
}
