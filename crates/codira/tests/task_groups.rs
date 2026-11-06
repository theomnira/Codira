//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of `group { .. }` with the standard library's `Result`
//!   and `std.error.error`'s `AnyError`: tasks that fail through `?` and an
//!   error conversion, a group whose value is passed on by `?`, the
//!   lowest-index error winning, and `main` reporting it. Programs are built
//!   and run by the real `codira` binary against this source tree's
//!   standard library.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// The standard library of this source tree, named explicitly so that the
/// tests never pick up another checkout's.
fn std_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../std")
        .canonicalize()
        .unwrap()
}

/// Writes a package whose `src/` holds `files` (`(name, source)` pairs).
fn package_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_task_groups")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    for (name, source) in files {
        std::fs::write(dir.path().join("src").join(name), source).unwrap();
    }
    dir
}

/// `codira run` of a one-file program: its exit code, stdout and stderr.
fn run(source: &str) -> (Option<i32>, String, String) {
    let dir = package_with(&[("mod.code", source)]);
    let output: Output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("run")
        .arg("--manifest-path")
        .arg(dir.path().join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .env("CODIRA_STD_PATH", std_dir())
        .output()
        .expect("failed to spawn codira");
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    (
        output.status.code(),
        text(&output.stdout),
        text(&output.stderr),
    )
}

const PROGRAM: &str = r#"
import std.error.error.{AnyError, Error}
import std.result.result.{Result}

struct TooBig { value: i64 }
extend TooBig: Error {
    func message(self) -> String { f"{self.value} is too big" }
}

// A task: fails, through `?` and `AnyError`'s conversion, above `limit`.
func check(i: i64, limit: i64) -> Result[i64, AnyError] {
    if i > limit {
        let e: Result[i64, TooBig] = Result.Err(TooBig { value: i })
        return Result.Ok(e?)
    }
    Result.Ok(i * i)
}

func checked(n: i64, limit: i64) -> Result[i64, AnyError] {
    let squares = group {
        var i = 0
        while i < n {
            spawn check(i, limit)
            i += 1
        }
    }?
    var total = 0
    for s in squares { total += s }
    Result.Ok(total)
}

func describe(r: Result[i64, AnyError]) {
    match r {
        Result.Ok(n) -> println(n),
        Result.Err(e) -> println(e.error_type()!, ": ", e.to_string()),
    }
}

public func main() -> Result[i64, AnyError] {
    describe(checked(10, 100))
    // Tasks 41 to 999 fail; 41 has the lowest index, every time.
    var i = 0
    while i < 20 {
        describe(checked(1000, 40))
        i += 1
    }
    let n = checked(5, 2)?
    Result.Ok(n)
}
"#;

/// A group's lowest-index `AnyError` is its value, `?` passes it on, and
/// `main` reports it.
#[test]
fn group_errors_flow_through_question_mark_and_any_error() {
    let (code, stdout, stderr) = run(PROGRAM);
    let mut expected = String::from("285\n");
    for _ in 0..20 {
        expected.push_str("TooBig: 41 is too big\n");
    }
    assert_eq!(stdout, expected, "stderr:\n{stderr}");
    assert_eq!(code, Some(1), "stderr:\n{stderr}");
    assert!(stderr.contains("Error: 3 is too big"), "stderr:\n{stderr}");
}
