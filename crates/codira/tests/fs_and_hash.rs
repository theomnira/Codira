//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests of the runtime's file and hash services: a Codira
//!   program built by `codira build` and loaded by the runtime writes a file,
//!   fsyncs it, reads it back, checks its size, renames and removes it and
//!   takes the error paths (missing file, closed handle, wrong mode), and it
//!   hashes the NIST SHA-256/SHA-512 and BLAKE3 known-answer messages and
//!   compares every digest byte itself.
//! - The effect analysis: a function calling the hash services under
//!   `@noalloc @nopanic @noblock` builds (the declarations assert it), and a
//!   `@noblock` function that writes a file is rejected by `codira check`.
//!
//! Programs cannot import `std` yet, so the program declares the services
//! itself, exactly as `std/fs/fs.code` and `std/crypto/hash.code` do.

use std::{
    ffi::OsString,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use codira::run_with_args;
use codira_runtime::Runtime;

const DECLARATIONS: &str = r#"
extern "C" {
    func codira_fs_open(path_addr: usize, path_len: usize, flags: u32) -> i64;
    func codira_fs_read(fd: i64, buf_addr: usize, len: usize) -> i64;
    func codira_fs_write(fd: i64, buf_addr: usize, len: usize) -> i64;
    func codira_fs_seek(fd: i64, offset: i64, whence: u32) -> i64;
    func codira_fs_fsync(fd: i64) -> i32;
    func codira_fs_close(fd: i64) -> i32;
    func codira_fs_size(fd: i64) -> i64;
    func codira_fs_remove(path_addr: usize, path_len: usize) -> i32;
    func codira_fs_rename(from_addr: usize, from_len: usize, to_addr: usize, to_len: usize) -> i32;
    func codira_fs_sync_dir(path_addr: usize, path_len: usize) -> i32;
    func codira_fs_last_os_error() -> i32;

    @noalloc @nopanic @noblock
    func codira_hash_sha256(data_addr: usize, len: usize, out_addr: usize) -> bool;
    @noalloc @nopanic @noblock
    func codira_hash_sha512(data_addr: usize, len: usize, out_addr: usize) -> bool;
    @noalloc @nopanic @noblock
    func codira_hash_blake3(data_addr: usize, len: usize, out_addr: usize) -> bool;
}

extern "codira-intrinsic" {
    func str_data(s: str) -> usize;
    func str_len(s: str) -> usize;
    func stack_alloc(size: usize, align: usize) -> usize;
    func load_u8(addr: usize) -> u8;
}
"#;

/// The file program: `0` when every step behaved, otherwise the number of
/// the first step that did not. `{dir}` is the directory it works in.
const FILE_PROGRAM: &str = r#"
public func file_round_trip() -> i64 {
    let path = "{dir}/data.txt";
    let renamed = "{dir}/renamed.txt";
    let text = "durable bytes from Codira";
    let n = str_len(text);

    // READ | WRITE | CREATE | TRUNCATE
    let fd = codira_fs_open(str_data(path), str_len(path), 1 | 2 | 8 | 16);
    if fd <= 0 { return 1; }
    if codira_fs_write(fd, str_data(text), n) != n as i64 { return 2; }
    if codira_fs_fsync(fd) != 0 { return 3; }
    if codira_fs_size(fd) != n as i64 { return 4; }
    if codira_fs_seek(fd, 0, 0) != 0 { return 5; }

    let buf = stack_alloc(64, 8);
    if codira_fs_read(fd, buf, 64) != n as i64 { return 6; }
    var i: usize = 0;
    while i < n {
        if load_u8(buf + i) != load_u8(str_data(text) + i) { return 7; }
        i += 1;
    }
    // At the end of the file a read returns 0.
    if codira_fs_read(fd, buf, 64) != 0 { return 8; }
    if codira_fs_close(fd) != 0 { return 9; }
    // A closed handle is refused: FS_ERR_BAD_HANDLE.
    if codira_fs_close(fd) != -6 { return 10; }
    if codira_fs_read(fd, buf, 64) != -6 { return 11; }

    if codira_fs_rename(str_data(path), str_len(path), str_data(renamed), str_len(renamed)) != 0 {
        return 12;
    }
    if codira_fs_sync_dir(str_data("{dir}"), str_len("{dir}")) != {sync_dir_result} { return 13; }
    // The old name is gone: FS_ERR_NOT_FOUND, and the OS said ENOENT
    // (ERROR_FILE_NOT_FOUND on Windows; both are 2).
    if codira_fs_open(str_data(path), str_len(path), 1) != -2 { return 14; }
    if codira_fs_last_os_error() != 2 { return 15; }

    // READ only: the size survived the rename, and writing is refused
    // (FS_ERR_WRONG_MODE).
    let again = codira_fs_open(str_data(renamed), str_len(renamed), 1);
    if again <= 0 { return 16; }
    if codira_fs_size(again) != n as i64 { return 17; }
    if codira_fs_write(again, str_data(text), n) != -18 { return 18; }
    if codira_fs_close(again) != 0 { return 19; }

    if codira_fs_remove(str_data(renamed), str_len(renamed)) != 0 { return 20; }
    if codira_fs_remove(str_data(renamed), str_len(renamed)) != -2 { return 21; }

    let missing = "{dir}/never/created.txt";
    if codira_fs_open(str_data(missing), str_len(missing), 1) != -2 { return 22; }
    // Creating needs write access: FS_ERR_INVALID_ARGUMENT.
    if codira_fs_open(str_data(missing), str_len(missing), 1 | 8) != -5 { return 23; }
    0
}
"#;

/// The NIST FIPS 180-4 example digests, and BLAKE3's (checked against the
/// portable C implementation of BLAKE3 as well as the Rust crate).
const SHA256_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const SHA512_ABC: &str = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
                          2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";
const BLAKE3_ABC: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";

/// A `@noalloc @nopanic @noblock` Codira function that hashes `message`
/// with `hash` and returns how many digest bytes differ from `expected`
/// (`1000` if the call was refused).
fn hash_check(name: &str, hash: &str, message: &str, expected: &str) -> String {
    let bytes: Vec<u8> = (0..expected.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&expected[i..i + 2], 16).unwrap())
        .collect();
    let checks: String = bytes
        .iter()
        .enumerate()
        .map(|(i, byte)| format!("    if load_u8(out + {i}) != {byte} {{ bad += 1; }}\n"))
        .collect();
    format!(
        r#"
@noalloc @nopanic @noblock
public func {name}() -> usize {{
    let message = "{message}";
    let out = stack_alloc({len}, 8);
    if !{hash}(str_data(message), str_len(message), out) {{ return 1000; }}
    var bad: usize = 0;
{checks}    bad
}}
"#,
        len = bytes.len()
    )
}

fn write_package(dir: &Path, source: &str) {
    std::fs::write(
        dir.join("codira.toml"),
        "[package]\nname=\"fs_and_hash\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/mod.code"), source).unwrap();
}

fn build(dir: &Path) -> codira::ExitStatus {
    let args: Vec<OsString> = vec![
        "codira".into(),
        "build".into(),
        "--manifest-path".into(),
        dir.join("codira.toml").into(),
    ];
    run_with_args(args).unwrap()
}

/// Runs the `codira` binary with a hard timeout, so a hang cannot hold the
/// build lock.
fn codira(dir: &Path, args: &[&str]) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
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
fn a_codira_program_writes_reads_renames_and_removes_a_file() {
    let project = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // A Codira string literal: escape what the lexer would read otherwise.
    let dir = work
        .path()
        .to_str()
        .unwrap()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let program = FILE_PROGRAM.replace("{dir}", &dir).replace(
        "{sync_dir_result}",
        // FS_ERR_UNSUPPORTED where directories cannot be synced.
        if cfg!(unix) { "0" } else { "-13" },
    );
    write_package(project.path(), &format!("{DECLARATIONS}{program}"));
    assert_eq!(build(project.path()), codira::ExitStatus::Success);

    let library = project.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    let step: i64 = runtime.invoke("file_round_trip", ()).unwrap();
    assert_eq!(step, 0, "step {step} of `file_round_trip` failed");
    // The program removed everything it created.
    assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
}

#[test]
fn a_codira_program_computes_the_known_answer_digests() {
    let source = [
        DECLARATIONS.to_owned(),
        hash_check("sha256_empty", "codira_hash_sha256", "", SHA256_EMPTY),
        hash_check("sha256_abc", "codira_hash_sha256", "abc", SHA256_ABC),
        hash_check("sha512_abc", "codira_hash_sha512", "abc", SHA512_ABC),
        hash_check("blake3_abc", "codira_hash_blake3", "abc", BLAKE3_ABC),
    ]
    .concat();
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), &source);
    // The functions are `@noalloc @nopanic @noblock`: building them means the
    // analysis accepted the declarations' assertions.
    assert_eq!(build(project.path()), codira::ExitStatus::Success);

    let library = project.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    for name in ["sha256_empty", "sha256_abc", "sha512_abc", "blake3_abc"] {
        let bad: usize = runtime.invoke(name, ()).unwrap();
        assert_eq!(bad, 0, "`{name}`: {bad} digest bytes differ");
    }
}

#[test]
fn file_io_counts_as_blocking() {
    let source = format!(
        "{DECLARATIONS}
@noblock
public func log_byte(fd: i64, addr: usize) -> i64 {{
    codira_fs_write(fd, addr, 1)
}}
"
    );
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), &source);
    let (ok, out) = codira(project.path(), &["check"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("`log_byte` is `@noblock` but calls `codira_fs_write`"),
        "{out}"
    );
}

/// The text of `std/<relative>` without its `import` lines: a package's
/// `root` is the package, not `std`, and nothing the wrappers use comes from
/// the import.
fn std_source(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../std")
        .join(relative);
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("import "))
        .map(|line| format!("{line}\n"))
        .collect()
}

const STD_FS_PROGRAM: &str = r#"
extern "codira-intrinsic" {
    func stack_alloc(size: usize, align: usize) -> usize;
    func load_u8(addr: usize) -> u8;
}

public func through_std_fs() -> i64 {
    let path = "{dir}/std.txt";
    let fd = fs_open(path, FS_WRITE() | FS_CREATE() | FS_CREATE_NEW());
    if !fs_ok(fd) { return 1; }
    if fs_write_str(fd, "hello ") != 6 { return 2; }
    if fs_write_str(fd, "world") != 5 { return 3; }
    if fs_fsync(fd) != 0 { return 4; }
    if fs_close(fd) != 0 { return 5; }
    // CREATE_NEW refuses the existing file.
    if fs_open(path, FS_WRITE() | FS_CREATE_NEW()) != FS_ERR_ALREADY_EXISTS() { return 6; }

    let reader = fs_open(path, FS_READ());
    if !fs_ok(reader) { return 7; }
    if fs_size(reader) != 11 { return 8; }
    let buf = stack_alloc(32, 8);
    if fs_read_full(reader, buf, 32) != 11 { return 9; }
    // 'w'
    if load_u8(buf + 6) != 119 { return 10; }
    if fs_seek(reader, -5, FS_SEEK_END()) != 6 { return 11; }
    if fs_read_full(reader, buf, 1) != 1 { return 12; }
    if load_u8(buf) != 119 { return 13; }
    if fs_close(reader) != 0 { return 14; }
    if fs_remove(path) != 0 { return 15; }
    if fs_open(path, FS_READ()) != FS_ERR_NOT_FOUND() { return 16; }
    if fs_last_os_error() != 2 { return 17; }
    0
}
"#;

/// `std/fs/fs.code` and `std/crypto/hash.code` themselves, compiled into a
/// package (as `std` cannot be imported yet) and driven by a program.
#[test]
fn the_std_wrappers_compile_and_work() {
    let work = tempfile::tempdir().unwrap();
    let dir = work
        .path()
        .to_str()
        .unwrap()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let project = tempfile::tempdir().unwrap();
    write_package(
        project.path(),
        &format!(
            "{}{}",
            std_source("fs/fs.code"),
            STD_FS_PROGRAM.replace("{dir}", &dir)
        ),
    );
    assert_eq!(build(project.path()), codira::ExitStatus::Success);
    let library = project.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    let step: i64 = runtime.invoke("through_std_fs", ()).unwrap();
    assert_eq!(step, 0, "step {step} of `through_std_fs` failed");
    assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);

    let checks: String = (0..SHA256_ABC.len())
        .step_by(2)
        .enumerate()
        .map(|(i, at)| {
            let byte = u8::from_str_radix(&SHA256_ABC[at..at + 2], 16).unwrap();
            format!("    if load_u8(digest + {i}) != {byte} {{ bad += 1; }}\n")
        })
        .collect();
    let hash_program = format!(
        r#"
extern "codira-intrinsic" {{
    func stack_alloc(size: usize, align: usize) -> usize;
    func load_u8(addr: usize) -> u8;
}}

@noalloc @nopanic @noblock
public func through_std_crypto() -> usize {{
    let digest = stack_alloc(32, 8);
    if !sha256_str("abc", digest) {{ return 1000; }}
    var bad: usize = 0;
{checks}    bad
}}
"#
    );
    let project = tempfile::tempdir().unwrap();
    write_package(
        project.path(),
        &format!("{}{hash_program}", std_source("crypto/hash.code")),
    );
    assert_eq!(build(project.path()), codira::ExitStatus::Success);
    let library = project.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    let bad: usize = runtime.invoke("through_std_crypto", ()).unwrap();
    assert_eq!(bad, 0, "{bad} digest bytes differ");
}
