//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests of importing the standard library from a program
//!   the real `codira` binary builds and
//!   runs programs that `import std.<module>`, and the standard-library
//!   modules they import are compiled into libraries of their own in the
//!   program's `target/` directory, which the runtime loads with the
//!   program.
//! - A program computes a SHA-256 through `std.crypto.hash` and prints it;
//!   one writes and reads back a file through `std.fs.fs`; one uses
//!   `std.memory.region`, whose functions take and return a value struct.
//! - A program importing a standard-library module with errors does not
//!   build, and the errors are reported against the standard-library file.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use codira_runtime::Runtime;

/// The standard library of this source tree. Tests name it explicitly
/// (`CODIRA_STD_PATH`), so they never pick up another checkout's.
fn std_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../std")
        .canonicalize()
        .unwrap()
}

/// Writes a package whose `src/` holds `files` (`(name, source)` pairs).
fn package_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_std_imports")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    for (name, source) in files {
        let path = dir.path().join("src").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    dir
}

/// Runs `codira <command>` on the package in `dir`, with the standard
/// library at `std`.
fn codira(dir: &Path, command: &str, std: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg(command)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .env("CODIRA_STD_PATH", std)
        .output()
        .expect("failed to spawn codira")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// Builds and runs the program made of `files`; returns its stdout.
fn run_ok(files: &[(&str, &str)]) -> (tempfile::TempDir, String) {
    let dir = package_with(files);
    let output = codira(dir.path(), "run", &std_dir());
    let stdout = text(&output.stdout);
    assert!(
        output.status.success(),
        "codira run failed\nstdout:\n{stdout}\nstderr:\n{}",
        text(&output.stderr)
    );
    (dir, stdout)
}

/// Prints the bytes at `addr` as lower-case hexadecimal.
const HEX: &str = r#"
extern "codira-intrinsic" {
    func stack_alloc(size: usize, align: usize) -> usize;
    func load_u8(addr: usize) -> u8;
}

func print_nibble(n: u8) {
    if n < 10 { print(n) }
    else if n == 10 { print("a") }
    else if n == 11 { print("b") }
    else if n == 12 { print("c") }
    else if n == 13 { print("d") }
    else if n == 14 { print("e") }
    else { print("f") }
}

func print_hex(addr: usize, len: usize) {
    var i: usize = 0
    while i < len {
        let b = load_u8(addr + i)
        print_nibble(b / 16)
        print_nibble(b % 16)
        i += 1
    }
    println("")
}
"#;

const SHA256_PROGRAM: &str = r#"
import std.crypto.hash.{sha256_str, SHA256_LEN}

public func main() {
    let digest = stack_alloc(SHA256_LEN(), 8)
    if !sha256_str("abc", digest) {
        println("refused")
        return
    }
    print_hex(digest, SHA256_LEN())
}

public func hash_byte(n: usize) -> u8 {
    let digest = stack_alloc(32, 8)
    if !sha256_str("abc", digest) { return 0; }
    load_u8(digest + n)
}
"#;

/// FIPS 180-4's example: SHA-256("abc").
const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

#[test]
fn a_program_computes_sha256_through_std_crypto_hash() {
    let (dir, stdout) = run_ok(&[("mod.code", &format!("{HEX}{SHA256_PROGRAM}"))]);
    assert_eq!(stdout, format!("{SHA256_ABC}\n"));

    // The standard-library module is a library of its own next to the
    // program's, and the program's library depends on it: the runtime
    // loads it with the program, as it loads the program's own modules.
    let target = dir.path().join("target");
    let std_library = target.join("hash$crypto$std.codiralib");
    assert!(std_library.is_file(), "{}", std_library.display());
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(target.join("mod.codiralib")).finish() }.unwrap();
    let first: u8 = runtime.invoke("hash_byte", (0usize,)).unwrap();
    assert_eq!(first, 0xba);
}

#[test]
fn aliased_and_glob_imports_of_std_work() {
    let program = format!(
        "{HEX}
import std.crypto.hash.sha256_str as digest_of
import std.crypto.hash.*

public func main() {{
    let digest = stack_alloc(32, 8)
    if digest_of(\"abc\", digest) && blake3_str(\"abc\", digest + 0) {{
        print_hex(digest, BLAKE3_LEN())
    }}
}}
"
    );
    let (_dir, stdout) = run_ok(&[("mod.code", &program)]);
    // BLAKE3("abc") overwrote the SHA-256.
    assert_eq!(
        stdout,
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85\n"
    );
}

const FS_PROGRAM: &str = r#"
import std.fs.fs.{fs_open, fs_ok, fs_write_str, fs_fsync, fs_close, fs_size, fs_read_full,
                   FS_READ, FS_WRITE, FS_CREATE, FS_TRUNCATE, FS_ERR_NOT_FOUND}

public func main() {
    let path = "{path}"
    let fd = fs_open(path, FS_WRITE() | FS_CREATE() | FS_TRUNCATE())
    if !fs_ok(fd) { println("open failed"); return }
    println(fs_write_str(fd, "written through std.fs"))
    println(fs_fsync(fd))
    println(fs_close(fd))

    let reader = fs_open(path, FS_READ())
    println(fs_size(reader))
    let buf = stack_alloc(64, 8)
    let n = fs_read_full(reader, buf, 64)
    println(n)
    // 'w', the first byte
    println(load_u8(buf))
    println(fs_close(reader))
    println(fs_open("{path}.missing", FS_READ()) == FS_ERR_NOT_FOUND())
}
"#;

#[test]
fn a_program_writes_and_reads_a_file_through_std_fs() {
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("data.txt");
    let path = file
        .to_str()
        .unwrap()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let program = format!("{HEX}{}", FS_PROGRAM.replace("{path}", &path));
    let (_dir, stdout) = run_ok(&[("mod.code", &program)]);
    assert_eq!(stdout, "22\n0\n0\n22\n22\n119\n0\ntrue\n");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "written through std.fs"
    );
}

/// `std.memory.region`'s functions take and return the value struct
/// `Region`: the shape that used to crash `codira build`.
#[test]
fn a_program_uses_std_memory_region_and_its_value_struct() {
    let program = r#"
import std.memory.region.{Region, region_with_quota, region_alloc, region_used, region_free}

func fill(r: Region) -> usize {
    let a = region_alloc(r, 24, 8)
    let b = region_alloc(r, 100, 8)
    if a == 0 { return 1000; }
    // `b` is over the quota.
    if b != 0 { return 2000; }
    region_used(r)
}

public func main() {
    let r = region_with_quota(64)
    println(fill(r))
    println(region_free(r))
    println(region_free(r))
}
"#;
    let (_dir, stdout) = run_ok(&[("mod.code", program)]);
    assert_eq!(stdout, "24\ntrue\nfalse\n");
}

/// The program's own modules and the standard library's do not collide,
/// even with the same path: `fs` here is the program's.
#[test]
fn program_modules_and_std_modules_with_the_same_path_are_distinct() {
    let program = r#"
import fs.{fs_ok}
import std.fs.fs.{FS_READ}

public func main() {
    println(fs_ok())
    println(FS_READ())
}
"#;
    let own_fs = r#"
public func fs_ok() -> i64 { 7 }
"#;
    let (_dir, stdout) = run_ok(&[("mod.code", program), ("fs.code", own_fs)]);
    assert_eq!(stdout, "7\n1\n");
}

#[test]
fn importing_a_std_module_with_errors_reports_them_against_the_std_file() {
    let program = r#"
import std.math.fast.{fast_sqrt}

public func main() {
    println(fast_sqrt(4.0))
}
"#;
    let dir = package_with(&[("mod.code", program)]);
    let output = codira(dir.path(), "build", &std_dir());
    let stderr = text(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("--> std/math/fast.code:"), "{stderr}");
    assert!(
        stderr.contains(
            "error: the standard-library module `std.math.fast` (std/math/fast.code) has"
        ),
        "{stderr}"
    );
    // The program itself is fine; nothing is blamed on it.
    assert!(!stderr.contains("--> mod.code"), "{stderr}");
    assert!(!dir.path().join("target/mod.codiralib").exists());
}

#[test]
fn errors_of_a_module_a_std_module_imports_are_reported_too() {
    // `std/ai/algorithm/functional.code` checks clean, but imports
    // `std/ai/tensor/raw.code`, which does not.
    let program = r#"
import std.ai.algorithm.functional.*

public func main() {}
"#;
    let dir = package_with(&[("mod.code", program)]);
    let output = codira(dir.path(), "check", &std_dir());
    let stderr = text(&output.stderr) + &text(&output.stdout);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("the standard-library module `std.ai.tensor.raw`"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("the standard-library module `std.ai.algorithm.functional`"),
        "{stderr}"
    );
}

#[test]
fn an_import_of_a_module_std_does_not_have_is_unresolved() {
    let program = r#"
import std.no_such_module.thing

public func main() {}
"#;
    let dir = package_with(&[("mod.code", program)]);
    let output = codira(dir.path(), "build", &std_dir());
    let stderr = text(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("unresolved import"), "{stderr}");
}

#[test]
fn a_missing_standard_library_is_reported() {
    let program = r#"
import std.crypto.hash.sha256_str

public func main() {}
"#;
    let dir = package_with(&[("mod.code", program)]);
    let empty = tempfile::tempdir().unwrap();
    let output = codira(dir.path(), "build", empty.path());
    let stderr = text(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("the program imports the standard library, but CODIRA_STD_PATH"),
        "{stderr}"
    );
}

/// A program that imports nothing from `std` builds exactly as before: no
/// standard-library file is loaded, and no library is written for one --
/// even when the standard library cannot be found.
#[test]
fn a_program_without_std_imports_does_not_load_it() {
    let program = r#"
public func main() { println(1) }
"#;
    let dir = package_with(&[("mod.code", program)]);
    let empty = tempfile::tempdir().unwrap();
    let output = codira(dir.path(), "run", empty.path());
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "1\n");
    let libraries: Vec<String> = std::fs::read_dir(dir.path().join("target"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".codiralib"))
        .collect();
    assert_eq!(libraries, ["mod.codiralib"]);
}
