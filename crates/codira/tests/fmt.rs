//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - End-to-end tests of `codira fmt`: `--check` fails on an unformatted
//!   package and names the file, formatting rewrites it to the standard form, a
//!   second run changes nothing, `codira check` reports the same result before
//!   and after, a file with syntax errors is reported and left untouched, and
//!   `--stdin` formats a stream.

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

const UNFORMATTED: &str = "// A small package.\n\n\n\npublic func add( a:i64,b :i64 )->i64{\n  // the sum\n        return a+b\n}\n\nfunc unused(x: i64) -> i64 {\nlet y = - x;   \n  y * 2\n}\n";

const FORMATTED: &str = "// A small package.\n\npublic func add(a: i64, b: i64) -> i64 {\n    // the sum\n    return a + b\n}\n\nfunc unused(x: i64) -> i64 {\n    let y = -x;\n    y * 2\n}\n";

fn codira(dir: &Path, args: &[&str], stdin: Option<&str>) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not run codira");
    {
        let mut pipe = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            pipe.write_all(text.as_bytes()).unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn write_package(dir: &Path, source: &str) {
    std::fs::write(
        dir.join("codira.toml"),
        "[package]\nname=\"fmt_test\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/mod.code"), source).unwrap();
}

#[test]
fn fmt_check_format_and_idempotence() {
    let dir = tempfile::tempdir().unwrap();
    write_package(dir.path(), UNFORMATTED);
    let source = dir.path().join("src/mod.code");

    let (ok, out) = codira(dir.path(), &["fmt", "--check", "--diff"], None);
    assert!(!ok, "--check must fail on an unformatted file:\n{out}");
    assert!(
        out.contains("not formatted:") && out.contains("mod.code"),
        "{out}"
    );
    assert!(
        out.contains("+    return a + b"),
        "the diff shows the change:\n{out}"
    );
    assert_eq!(
        std::fs::read_to_string(&source).unwrap(),
        UNFORMATTED,
        "--check writes nothing"
    );

    let (_, before) = codira(
        dir.path(),
        &["check", "--summary", "--no-counterexample-tests"],
        None,
    );
    assert!(before.contains("checked fmt_test"), "{before}");

    let (ok, out) = codira(dir.path(), &["fmt"], None);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(&source).unwrap(), FORMATTED);

    let (ok, out) = codira(dir.path(), &["fmt", "--check"], None);
    assert!(ok, "formatted output passes --check:\n{out}");
    assert!(out.contains("0 not formatted"), "{out}");

    let (_, after) = codira(
        dir.path(),
        &["check", "--summary", "--no-counterexample-tests"],
        None,
    );
    assert_eq!(
        before, after,
        "formatting changed what `codira check` reports"
    );
}

#[test]
fn fmt_refuses_a_file_with_syntax_errors() {
    let dir = tempfile::tempdir().unwrap();
    let broken = "func f( {\n  return 1\n";
    write_package(dir.path(), broken);
    let (ok, out) = codira(dir.path(), &["fmt"], None);
    assert!(!ok, "{out}");
    assert!(out.contains("syntax errors"), "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/mod.code")).unwrap(),
        broken,
        "a file with syntax errors is never rewritten"
    );
}

#[test]
fn fmt_stdin_and_explicit_paths() {
    let dir = tempfile::tempdir().unwrap();
    let (ok, out) = codira(dir.path(), &["fmt", "--stdin"], Some(UNFORMATTED));
    assert!(ok, "{out}");
    assert_eq!(out, FORMATTED);

    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    std::fs::write(dir.path().join("a/b/x.code"), UNFORMATTED).unwrap();
    std::fs::write(dir.path().join("a/y.code"), FORMATTED).unwrap();
    std::fs::write(dir.path().join("a/notes.txt"), "not code").unwrap();
    let (ok, out) = codira(dir.path(), &["fmt", "--check", "a"], None);
    assert!(!ok);
    assert!(out.contains("2 file(s) checked, 1 not formatted"), "{out}");
}
