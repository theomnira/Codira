//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Runs `examples/hello` with `codira run --jit` and checks that it ran on
//!   the Cranelift backend, not by falling back to LLVM, and printed what the
//!   LLVM backend prints for it.

use std::{path::Path, process::Command};

#[test]
fn hello_runs_on_the_cranelift_backend() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello/codira.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(["run", "--jit", "--manifest-path"])
        .arg(&manifest)
        .env("CODIRA_JIT_TIMINGS", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("cranelift: compiled in"),
        "fell back to LLVM:\n{stderr}"
    );
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        vec![
            "Hello, world!",
            "1 squared is 1",
            "2 squared is 4",
            "3 squared is 9",
            "pi is roughly 3.14159, and that is true",
        ]
    );
}

/// `codira run --jit` gets the heap limit `codira run` gives a runtime, and
/// running out of it ends the program with the same report and status.
#[test]
fn out_of_memory_under_the_jit_matches_llvm() {
    let dir = tempfile::Builder::new()
        .prefix("codira_jit_oom")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/mod.code"),
        "class Node { v: i64, pad: [i64] }\n\
         public func main() {\n\
             let keep: [Node] = []\n\
             for i in 0..1000000 { keep.push(Node { v: i, pad: [i, i, i, i, i, i, i, i] }) }\n\
             println(keep.len())\n\
         }\n",
    )
    .unwrap();
    let run = |jit: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codira"));
        command
            .args([
                "run",
                "--color",
                "disable",
                "--heap-limit",
                "4MiB",
                "--manifest-path",
            ])
            .arg(dir.path().join("codira.toml"))
            .env("CODIRA_JIT_TIMINGS", "1");
        if jit {
            command.arg("--jit");
        }
        command.output().unwrap()
    };
    let (jit, llvm) = (run(true), run(false));
    let jit_stderr = String::from_utf8_lossy(&jit.stderr);
    assert!(
        jit_stderr.contains("cranelift: compiled in"),
        "{jit_stderr}"
    );
    let report = |stderr: &str| -> String {
        stderr
            .lines()
            .filter(|l| !l.starts_with("cranelift:"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(jit.status.code(), llvm.status.code());
    assert_eq!(
        report(&jit_stderr),
        report(&String::from_utf8_lossy(&llvm.stderr))
    );
    assert!(jit_stderr.contains("fatal: out of memory"), "{jit_stderr}");
}
