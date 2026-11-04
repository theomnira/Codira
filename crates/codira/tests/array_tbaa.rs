//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests of the experimental array alias classes
//!   (`CODIRA_ARRAY_TBAA=1`, `crates/codira_codegen/src/ir/array_tbaa.rs`):
//!   programs that read and write arrays print the same output with and
//!   without them, out-of-range indices still stop the program, and the
//!   metadata appears in the IR only when asked for.

use std::{path::Path, process::Command};

fn package_with(source: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_array_tbaa")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), source).unwrap();
    dir
}

/// Runs `codira <command>` on the package in `dir`, with or without the
/// alias classes.
fn codira(dir: &Path, command: &str, extra: &[&str], tbaa: bool) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codira"));
    cmd.arg(command)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .args(extra);
    if tbaa {
        cmd.env("CODIRA_ARRAY_TBAA", "1");
    } else {
        cmd.env_remove("CODIRA_ARRAY_TBAA");
    }
    cmd.output().expect("failed to spawn codira")
}

const ARRAYS: &str = r#"
struct P {
    x: i64,
    y: i64,
}

func matmul(n: i64) -> f64 {
    let size = (n * n) as usize
    var a = array_filled(0.0, size)
    var b = array_filled(0.0, size)
    var c = array_filled(0.0, size)
    var i = 0
    while i < n * n {
        a[i as usize] = (i % 7) as f64
        b[i as usize] = (i % 5) as f64 - 2.0
        i += 1
    }
    i = 0
    while i < n {
        var k = 0
        while k < n {
            let aik = a[(i * n + k) as usize]
            var j = 0
            while j < n {
                c[(i * n + j) as usize] = c[(i * n + j) as usize] + aik * b[(k * n + j) as usize]
                j += 1
            }
            k += 1
        }
        i += 1
    }
    var s = 0.0
    for v in c {
        s = s + v
    }
    s
}

// Stores to one array between reads of another's length and elements.
func interleave(n: i64) -> i64 {
    var xs = array_filled(1, n as usize)
    let ys = [3, 1, 4, 1, 5, 9, 2, 6]
    var total = 0
    var i = 0
    while i < n {
        xs[i as usize] = xs[i as usize] + ys[(i % 8) as usize] * (ys.len() as i64)
        total += xs[i as usize]
        i += 1
    }
    total + (xs.len() as i64)
}

// Arrays of arrays: the inner handles are elements of the outer array.
func nested() -> i64 {
    var rows = [[1, 2, 3], [4, 5, 6], [7, 8, 9]]
    var r = 0
    while r < 3 {
        rows[r as usize][1] = rows[r as usize][0] + rows[r as usize][2]
        r += 1
    }
    rows[0][1] * 100 + rows[1][1] * 10 + rows[2][1]
}

// Value structs stored inline in elements.
func points() -> i64 {
    var ps = [P { x: 1, y: 2 }, P { x: 3, y: 4 }]
    ps[1].x = ps[0].y * 10
    ps[0] = P { x: ps[1].x + 1, y: 0 }
    ps[0].x * 1000 + ps[1].x * 10 + ps[1].y
}

public func main() {
    println(matmul(24))
    println(interleave(1000))
    println(nested())
    println(points())
}
"#;

#[test]
fn array_programs_print_the_same_with_and_without_alias_classes() {
    let dir = package_with(ARRAYS);
    for level in ["0", "2", "3"] {
        let off = codira(dir.path(), "run", &["-O", level], false);
        let on = codira(dir.path(), "run", &["-O", level], true);
        let text =
            |o: &std::process::Output| String::from_utf8_lossy(&o.stdout).replace("\r\n", "\n");
        assert!(
            off.status.success(),
            "-O {level}: {}",
            String::from_utf8_lossy(&off.stderr)
        );
        assert!(
            on.status.success(),
            "-O {level}: {}",
            String::from_utf8_lossy(&on.stderr)
        );
        assert_eq!(text(&off), text(&on), "-O {level}");
        assert_eq!(text(&on).lines().count(), 4, "-O {level}: {}", text(&on));
    }
}

#[test]
fn out_of_range_stores_still_stop_the_program() {
    // The store loop runs one past the end of `c` while reading `a`; with the
    // alias classes the length loads may move, the check may not.
    let dir = package_with(
        r#"
public func main() {
    let a = array_filled(2, 8)
    var c = array_filled(0, 8)
    var j = 0
    while j <= 8 {
        c[j as usize] = a[(j % 8) as usize] + j
        j += 1
    }
    println(c[0])
}
"#,
    );
    for level in ["0", "3"] {
        let out = codira(dir.path(), "run", &["-O", level], true);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(101), "-O {level}: {stderr}");
        assert!(
            stderr.contains("index out of bounds: the length is 8 but the index is 8"),
            "-O {level}: {stderr}"
        );
    }
}

#[test]
fn alias_metadata_appears_only_when_asked_for() {
    let dir = package_with(ARRAYS);
    let ir = |tbaa: bool| {
        let out = codira(dir.path(), "build", &["-O", "0", "--emit-ir"], tbaa);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::read_to_string(dir.path().join("target/mod.ll")).unwrap()
    };
    let off = ir(false);
    let on = ir(true);
    assert!(
        !off.contains("codira array"),
        "tags emitted without the variable"
    );
    for class in [
        "codira array length",
        "codira array element",
        "codira array handle data pointer",
    ] {
        assert!(on.contains(class), "no `{class}` tag in the IR");
    }
}
