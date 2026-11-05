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
//! - Programs import `std.bit`, `std.math.constants`, `std.math.fast`,
//!   `std.hashlib.hash`, `std.base64.base64` and `std.random.rng` and check
//!   what they print against values computed independently: bit operations
//!   exactly, the constants as the IEEE 754 values Rust has, the fast approximations
//!   against Rust's libm within the error each states, FNV-1a and the
//!   base16/32/64 encodings against the published test vectors, the
//!   generator against a reference xoshiro256**.
//! - `std.math.math` against Rust's libm, `std.time.time`'s durations,
//!   clocks and sleep, `std.complex.complex` against exact values, and
//!   `std.sys.terminate`'s `panic` (its message on standard error and an
//!   abnormal exit): all four call `panic`, which returns `Never`.
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
import std.collections.list.{list_new}

public func main() {}
"#;
    let dir = package_with(&[("mod.code", program)]);
    let output = codira(dir.path(), "build", &std_dir());
    let stderr = text(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("--> std/collections/list.code:"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "error: the standard-library module `std.collections.list` \
             (std/collections/list.code) has"
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
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".codiralib"))
        .collect();
    assert_eq!(libraries, ["mod.codiralib"]);
}

/// `bytes("...")`: the bytes of a string literal as a `[u8]`, and
/// `same(a, "...")`: whether a `[u8]` holds exactly those bytes.
const BYTES: &str = r#"
extern "codira-intrinsic" {
    func str_data(s: str) -> usize;
    func str_len(s: str) -> usize;
    func load_u8(addr: usize) -> u8;
}

func bytes(s: str) -> [u8] {
    var out = array_filled(0 as u8, str_len(s))
    var i: usize = 0
    while i < str_len(s) {
        out[i] = load_u8(str_data(s) + i)
        i += 1
    }
    out
}

func same(a: [u8], s: str) -> bool {
    let b = bytes(s)
    if a.len() != b.len() { return false }
    var i: usize = 0
    while i < a.len() {
        if a[i] != b[i] { return false }
        i += 1
    }
    true
}
"#;

/// The lines of `stdout` that are not empty.
fn lines(stdout: &str) -> Vec<&str> {
    stdout.lines().filter(|l| !l.is_empty()).collect()
}

#[test]
fn std_bit_operations_are_exact() {
    let program = r#"
import std.bit.bit.{count_leading_zeros, count_leading_zeros_u32, count_trailing_zeros,
                    pop_count, pop_count_u32, bit_reverse, bit_reverse_u32, byte_swap,
                    byte_swap_u32, byte_swap_u16, bit_width, bit_ceil, bit_ceil_u32,
                    bit_floor, rotate_bits_left, rotate_bits_right, rotate_bits_left_u32,
                    is_power_of_two}
import std.bit.mask.{create_mask, create_mask_u32, extract_bits, insert_bits, bit_select,
                     set_bit, clear_bit, toggle_bit, test_bit, lowest_set_bit,
                     highest_set_bit, clear_lowest_set_bit, mask_up_to_bit}

public func main() {
    println(count_leading_zeros(0))
    println(count_leading_zeros(1))
    println(count_leading_zeros(0x00F0000000000000))
    println(count_leading_zeros_u32(0x00010000))
    println(count_trailing_zeros(0))
    println(count_trailing_zeros(0x80))
    println(pop_count(0xFFFFFFFFFFFFFFFF))
    println(pop_count(0x00FF00FF))
    println(pop_count_u32(0xF0F0F0F0))
    println(bit_reverse(1))
    println(bit_reverse_u32(0x00000003))
    println(byte_swap(0x0102030405060708))
    println(byte_swap_u32(0x11223344))
    println(byte_swap_u16(0xABCD))
    println(bit_width(0))
    println(bit_width(255))
    println(bit_width(256))
    println(bit_ceil(0))
    println(bit_ceil(5))
    println(bit_ceil(1024))
    println(bit_ceil_u32(1025))
    println(bit_floor(0))
    println(bit_floor(1000))
    println(rotate_bits_left(0x8000000000000001, 1))
    println(rotate_bits_right(1, 1))
    println(rotate_bits_left(0x0123456789ABCDEF, 64))
    println(rotate_bits_left_u32(0x80000001, 4))
    println(is_power_of_two(0))
    println(is_power_of_two(4096))
    println(is_power_of_two(4097))
    println(create_mask(4, 8))
    println(create_mask(0, 64))
    println(create_mask_u32(28, 4))
    println(extract_bits(0xABCD, 8, 4))
    println(insert_bits(0xFF00, 0xA, 4, 4))
    println(bit_select(0xFF00, 0x1234, 0xABCD))
    println(set_bit(0, 63))
    println(clear_bit(0xFF, 0))
    println(toggle_bit(0b1010, 1))
    println(test_bit(5, 2))
    println(test_bit(5, 1))
    println(lowest_set_bit(0xB4))
    println(lowest_set_bit(0))
    println(highest_set_bit(0xB4))
    println(clear_lowest_set_bit(0xB4))
    println(mask_up_to_bit(3))
    println(mask_up_to_bit(63))
}
"#;
    let (_dir, stdout) = run_ok(&[("mod.code", program)]);
    let expected: Vec<String> = vec![
        "64".into(),
        "63".into(),
        0x00F0_0000_0000_0000u64.leading_zeros().to_string(),
        0x0001_0000u32.leading_zeros().to_string(),
        "64".into(),
        "7".into(),
        "64".into(),
        "16".into(),
        "16".into(),
        1u64.reverse_bits().to_string(),
        3u32.reverse_bits().to_string(),
        0x0102_0304_0506_0708u64.swap_bytes().to_string(),
        0x1122_3344u32.swap_bytes().to_string(),
        0xABCDu16.swap_bytes().to_string(),
        "0".into(),
        "8".into(),
        "9".into(),
        "1".into(),
        "8".into(),
        "1024".into(),
        "2048".into(),
        "0".into(),
        "512".into(),
        0x8000_0000_0000_0001u64.rotate_left(1).to_string(),
        1u64.rotate_right(1).to_string(),
        0x0123_4567_89AB_CDEFu64.to_string(),
        0x8000_0001u32.rotate_left(4).to_string(),
        "false".into(),
        "true".into(),
        "false".into(),
        0xFF0u64.to_string(),
        u64::MAX.to_string(),
        0xF000_0000u32.to_string(),
        "11".into(),
        0xFFA0u64.to_string(),
        ((0x1234u64 & 0xFF00) | (0xABCD & !0xFF00u64)).to_string(),
        (1u64 << 63).to_string(),
        "254".into(),
        "8".into(),
        "true".into(),
        "false".into(),
        "4".into(),
        "0".into(),
        "128".into(),
        0xB0u64.to_string(),
        "15".into(),
        u64::MAX.to_string(),
    ];
    assert_eq!(lines(&stdout), expected);
}

#[test]
fn std_math_constants_are_the_ieee_values() {
    let program = r#"
import std.math.constants.{PI, TAU, E, F64_EPSILON, F64_MIN_POSITIVE, F64_MAX, F32_MAX,
                           INF, NEG_INF, NAN, I64_MIN, U64_MAX, I32_MAX}

public func main() {
    println(PI())
    println(TAU())
    println(E())
    println(F64_EPSILON())
    println(F64_MIN_POSITIVE())
    println(F64_MAX())
    println(F32_MAX())
    println(INF() > F64_MAX())
    println(NEG_INF() < -F64_MAX())
    println(NAN() != NAN())
    println(I64_MIN())
    println(U64_MAX())
    println(I32_MAX())
}
"#;
    let (_dir, stdout) = run_ok(&[("mod.code", program)]);
    let out = lines(&stdout);
    let f = |i: usize| out[i].parse::<f64>().unwrap();
    assert_eq!(f(0), std::f64::consts::PI);
    assert_eq!(f(1), std::f64::consts::TAU);
    assert_eq!(f(2), std::f64::consts::E);
    assert_eq!(f(3), f64::EPSILON);
    assert_eq!(f(4), f64::MIN_POSITIVE);
    assert_eq!(f(5), f64::MAX);
    assert_eq!(out[6].parse::<f32>().unwrap(), f32::MAX);
    assert_eq!(&out[7..10], ["true", "true", "true"]);
    assert_eq!(out[10], i64::MIN.to_string());
    assert_eq!(out[11], u64::MAX.to_string());
    assert_eq!(out[12], i32::MAX.to_string());
}

/// Each fast approximation, over a grid of inputs, against Rust's libm,
/// within the error its comment states.
#[test]
fn std_math_fast_is_within_its_stated_error() {
    let program = r#"
import std.math.fast.{fast_rsqrt, fast_sqrt, fast_sin, fast_cos, fast_exp, fast_log,
                      fast_ipow_f64}

public func main() {
    var i: i64 = -400
    while i <= 400 {
        let x = (i as f32) * (0.01 as f32)
        print(x) print(" ") print(fast_sin(x)) print(" ") print(fast_cos(x)) print(" ")
        println(fast_exp(x))
        i += 1
    }
    var j: i64 = 1
    while j <= 1000 {
        let y = (j as f32) * (0.0137 as f32)
        print(y) print(" ") print(fast_log(y)) print(" ") print(fast_rsqrt(y)) print(" ")
        println(fast_sqrt(y))
        j += 1
    }
    println(fast_sin(100.0 as f32))
    println(fast_exp(-50.0 as f32))
    println(fast_exp(50.0 as f32))
    println(fast_log(1.0e30 as f32))
    println(fast_ipow_f64(2.0, 10))
    println(fast_ipow_f64(1.5, 3))
}
"#;
    let (_dir, stdout) = run_ok(&[("mod.code", program)]);
    let out = lines(&stdout);
    let nums = |line: &str| -> Vec<f64> {
        line.split(' ')
            .map(|n| f64::from(n.parse::<f32>().unwrap()))
            .collect()
    };
    let (mut sin, mut cos, mut exp) = (0f64, 0f64, 0f64);
    for line in &out[..801] {
        let v = nums(line);
        sin = sin.max((v[1] - v[0].sin()).abs());
        cos = cos.max((v[2] - v[0].cos()).abs());
        exp = exp.max(((v[3] - v[0].exp()) / v[0].exp()).abs());
    }
    let (mut log, mut rsqrt, mut sqrt) = (0f64, 0f64, 0f64);
    for line in &out[801..1801] {
        let v = nums(line);
        log = log.max((v[1] - v[0].ln()).abs());
        rsqrt = rsqrt.max(((v[2] - 1.0 / v[0].sqrt()) * v[0].sqrt()).abs());
        sqrt = sqrt.max(((v[3] - v[0].sqrt()) / v[0].sqrt()).abs());
    }
    // The bounds std/math/fast.code states.
    assert!(sin < 7e-5, "fast_sin: {sin}");
    assert!(cos < 7e-5, "fast_cos: {cos}");
    assert!(exp < 1e-4, "fast_exp: {exp}");
    assert!(log < 5.4e-4, "fast_log: {log}");
    assert!(rsqrt < 1.75e-3, "fast_rsqrt: {rsqrt}");
    assert!(sqrt < 1.75e-3, "fast_sqrt: {sqrt}");

    let tail = |i: usize| f64::from(out[1801 + i].parse::<f32>().unwrap());
    assert!((tail(0) - 100f64.sin()).abs() < 7e-5, "{}", tail(0));
    assert!(((tail(1) - (-50f64).exp()) / (-50f64).exp()).abs() < 1e-4);
    assert!(((tail(2) - 50f64.exp()) / 50f64.exp()).abs() < 1e-4);
    assert!((tail(3) - 1.0e30f64.ln()).abs() < 5.4e-4 + 1e-5);
    assert_eq!(&out[1805..], ["1024.0", "3.375"]);
}

/// `std.hashlib.hash` is FNV-1a (64-bit), which is not cryptographic; the
/// vectors are FNV-1a's published ones.
#[test]
fn std_hashlib_hash_is_fnv1a() {
    let program = format!(
        "{BYTES}
import std.hashlib.hash.{{hash_bytes, hash_u64, hash_i64, hash_f64, hash_bool, combine_hash,
                         combine_hash_ordered, combine_hash_unordered, Hashable, hash}}

struct Id {{ v: u64 }}
extend Id: Hashable {{ func hash_value(self) -> u64 {{ hash_u64(self.v) }} }}

public func main() {{
    println(hash_bytes(bytes(\"\")))
    println(hash_bytes(bytes(\"a\")))
    println(hash_bytes(bytes(\"foobar\")))
    println(hash_u64(0x0807060504030201))
    println(hash_i64(-1) == hash_u64(0xFFFFFFFFFFFFFFFF))
    println(hash_f64(0.0) == hash_f64(-0.0))
    let nan: f64 = 0.0 / 0.0
    println(hash_f64(nan) == hash_f64(-nan))
    println(hash_bool(true) == hash_u64(1))
    println(combine_hash(1, 2))
    let hs: [u64] = [1, 2, 3]
    let rev: [u64] = [3, 2, 1]
    println(combine_hash_ordered(hs) == combine_hash_ordered(rev))
    println(combine_hash_unordered(hs) == combine_hash_unordered(rev))
    println(hash(Id {{ v: 7 }}) == hash_u64(7))
}}
"
    );
    let (_dir, stdout) = run_ok(&[("mod.code", &program)]);
    let fnv1a = |data: &[u8]| {
        data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    let combine = |seed: u64, value: u64| {
        seed ^ value
            .wrapping_add(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(seed << 6)
            .wrapping_add(seed >> 2)
    };
    assert_eq!(
        lines(&stdout),
        [
            0xcbf2_9ce4_8422_2325u64.to_string(),
            0xaf63_dc4c_8601_ec8cu64.to_string(),
            0x8594_4171_f739_67e8u64.to_string(),
            fnv1a(&[1, 2, 3, 4, 5, 6, 7, 8]).to_string(),
            "true".into(),
            "true".into(),
            "true".into(),
            "true".into(),
            combine(1, 2).to_string(),
            "false".into(),
            "true".into(),
            "true".into(),
        ]
    );
}

/// Base64, base32 and base16 of the standard's test strings ("", "f",
/// "fo", ..., "foobar"; RFC 4648, section 10), both ways.
#[test]
fn std_base64_matches_the_standard_test_vectors() {
    let vectors: [(&str, &str, &str, &str); 7] = [
        ("", "", "", ""),
        ("f", "Zg==", "MY======", "66"),
        ("fo", "Zm8=", "MZXQ====", "666F"),
        ("foo", "Zm9v", "MZXW6===", "666F6F"),
        ("foob", "Zm9vYg==", "MZXW6YQ=", "666F6F62"),
        ("fooba", "Zm9vYmE=", "MZXW6YTB", "666F6F6261"),
        ("foobar", "Zm9vYmFy", "MZXW6YTBOI======", "666F6F626172"),
    ];
    let mut main = String::new();
    for (plain, b64, b32, b16) in vectors {
        for (encode, decode, encoded) in [
            ("b64_encode", "b64_decode", b64),
            ("b32_encode", "b32_decode", b32),
            ("b16_encode", "b16_decode", b16),
        ] {
            main.push_str(&format!(
                "    println(same({encode}(bytes(\"{plain}\")), \"{encoded}\"))\n    \
                 println(same({decode}(bytes(\"{encoded}\")), \"{plain}\"))\n"
            ));
        }
    }
    let program = format!(
        "{BYTES}
import std.base64.base64.{{b64_encode, b64_decode, b32_encode, b32_decode, b16_encode,
                           b16_decode, b64_encoded_len, b32_encoded_len}}

public func main() {{
{main}
    // Lower-case hex and base32, and line breaks in base64, are accepted.
    println(same(b16_decode(bytes(\"666f6F\")), \"foo\"))
    println(same(b64_decode(bytes(\"Zm9v\\nYmFy\\n\")), \"foobar\"))
    println(same(b32_decode(bytes(\"mzxw6ytboi======\")), \"foobar\"))
    // Invalid: odd-length or non-hex base16, `=` opening a base64 group.
    println(b16_decode(bytes(\"666\")).len())
    println(b16_decode(bytes(\"6G\")).len())
    println(b64_decode(bytes(\"=Zm9\")).len())
    // Every byte value survives a round trip.
    var all = array_filled(0 as u8, 256)
    var i: usize = 0
    while i < 256 {{ all[i] = i as u8; i += 1 }}
    let back = b64_decode(b64_encode(all))
    var ok = back.len() == 256
    i = 0
    while ok && i < 256 {{ ok = back[i] == all[i]; i += 1 }}
    println(ok)
    println(b64_encoded_len(256))
    println(b32_encoded_len(6))
}}
"
    );
    let (_dir, stdout) = run_ok(&[("mod.code", &program)]);
    let out = lines(&stdout);
    assert_eq!(out.len(), 7 * 6 + 9, "{stdout}");
    assert!(out[..7 * 6].iter().all(|l| *l == "true"), "{stdout}");
    assert_eq!(
        &out[7 * 6..],
        ["true", "true", "true", "0", "0", "0", "true", "344", "16"]
    );
}

/// `std.random.rng` against a reference `xoshiro256**` seeded by `SplitMix64`,
/// written here from the published algorithms. (Not cryptographic, and not
/// tested as if it were: only that it is that generator.)
#[test]
fn std_random_rng_is_xoshiro256_starstar() {
    struct Xoshiro([u64; 4]);
    impl Xoshiro {
        fn seeded(seed: u64) -> Self {
            let mut x = seed;
            let mut next = || {
                x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = x;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            };
            Xoshiro([next(), next(), next(), next()])
        }
        fn next(&mut self) -> u64 {
            let s = &mut self.0;
            let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
            let t = s[1] << 17;
            s[2] ^= s[0];
            s[3] ^= s[1];
            s[1] ^= s[2];
            s[0] ^= s[3];
            s[2] ^= t;
            s[3] = s[3].rotate_left(45);
            result
        }
        fn range(&mut self, low: i64, high: i64) -> i64 {
            let span = (high as u64).wrapping_sub(low as u64).wrapping_add(1);
            let limit = u64::MAX - (u64::MAX % span);
            loop {
                let x = self.next();
                if x < limit {
                    return (low as u64).wrapping_add(x % span) as i64;
                }
            }
        }
    }

    let program = r#"
import std.random.rng.{rng_seeded, rng_next_u64, rng_next_f64, rng_range_i64, rng_next_bool}

public func main() {
    let r = rng_seeded(0)
    var i = 0
    while i < 5 { println(rng_next_u64(r)); i += 1 }
    let q = rng_seeded(0xDEADBEEF)
    println(rng_next_u64(q))
    println(rng_next_f64(q))
    var lo: i64 = 100
    var hi: i64 = -100
    var sum: i64 = 0
    i = 0
    while i < 20000 {
        let v = rng_range_i64(q, -3, 3)
        if v < lo { lo = v }
        if v > hi { hi = v }
        sum += v
        i += 1
    }
    println(lo)
    println(hi)
    println(sum)
    println(rng_range_i64(q, 5, 5))
    println(rng_next_bool(q) || true)
}
"#;
    let (_dir, stdout) = run_ok(&[("mod.code", program)]);
    let out = lines(&stdout);

    let mut r = Xoshiro::seeded(0);
    let want: Vec<String> = (0..5).map(|_| r.next().to_string()).collect();
    assert_eq!(&out[..5], want);
    let mut q = Xoshiro::seeded(0xDEAD_BEEF);
    assert_eq!(out[5], q.next().to_string());
    let f = (q.next() >> 11) as f64 / 9_007_199_254_740_992.0;
    assert_eq!(out[6].parse::<f64>().unwrap(), f);
    let sum: i64 = (0..20000).map(|_| q.range(-3, 3)).sum();
    assert_eq!(&out[7..], ["-3", "3", &sum.to_string(), "5", "true"]);
}

// Each of the modules below calls `panic`, which returns `Never`: a
// function the code generator compiles as `void` and `noreturn`, with
// `unreachable` after every call to it.

#[test]
fn std_math_math_matches_libm() {
    let program = r#"
import std.math.math.{sin, cos, tan, sqrt, exp, log, log2, log10, pow, atan2, asin, cbrt,
                      floor, ceil, round, trunc, abs, hypot, gcd, lcm, factorial, ipow,
                      ilog2, isnan, isinf, isfinite, inf, nan, lerp, to_radians}

public func main() {
    println(sin(0.5))
    println(cos(0.5))
    println(tan(0.5))
    println(sqrt(2.0))
    println(exp(1.0))
    println(log(10.0))
    println(log2(1024.0))
    println(log10(0.001))
    println(pow(2.0, 0.5))
    println(atan2(1.0, -1.0))
    println(asin(0.5))
    println(cbrt(27.0))
    println(floor(-2.5))
    println(ceil(-2.5))
    println(round(-2.5))
    println(trunc(-2.7))
    println(abs(-3.0))
    println(hypot(3.0, 4.0))
    println(gcd(48, -18))
    println(lcm(4, 6))
    println(factorial(20))
    println(ipow(3, 13))
    println(ilog2(1025))
    println(isnan(nan()))
    println(isinf(-inf()))
    println(isfinite(1.0))
    println(lerp(2.0, 4.0, 0.25))
    println(to_radians(180.0))
}
"#;
    let (_dir, stdout) = run_ok(&[("mod.code", program)]);
    let out = lines(&stdout);
    let f = |i: usize| out[i].parse::<f64>().unwrap();
    // Within two units in the last place of Rust's libm.
    let close = |got: f64, want: f64| (got - want).abs() <= 4.5e-16 * want.abs().max(1.0);
    let want = [
        0.5f64.sin(),
        0.5f64.cos(),
        0.5f64.tan(),
        2f64.sqrt(),
        1f64.exp(),
        10f64.ln(),
        10.0,
        -3.0,
        2f64.sqrt(),
        1f64.atan2(-1.0),
        0.5f64.asin(),
        3.0,
        -3.0,
        -2.0,
        -3.0,
        -2.0,
        3.0,
        5.0,
    ];
    for (i, want) in want.into_iter().enumerate() {
        assert!(close(f(i), want), "line {i}: {} != {want}", out[i]);
    }
    let pi = std::f64::consts::PI.to_string();
    assert_eq!(
        &out[18..],
        [
            "6",
            "12",
            "2432902008176640000",
            "1594323",
            "10",
            "true",
            "true",
            "true",
            "2.5",
            pi.as_str(),
        ]
    );
}

#[test]
fn std_time_measures_and_sleeps() {
    let program = r#"
import std.time.time.{duration_from_secs, duration_from_millis, duration_from_nanos,
                      duration_as_micros, duration_as_secs_f64, duration_subsec_nanos,
                      duration_add, duration_sub, duration_mul, duration_div, duration_lt,
                      duration_as_nanos, instant_now, instant_elapsed, sleep_ms, now}

public func main() {
    let d = duration_add(duration_from_secs(1), duration_from_millis(750))
    println(duration_as_micros(d))
    println(duration_as_secs_f64(d))
    println(duration_subsec_nanos(duration_from_nanos(3000000123)))
    println(duration_as_nanos(duration_sub(duration_from_millis(1), duration_from_millis(2))))
    println(duration_as_nanos(duration_div(duration_mul(d, 4), 7)))
    println(duration_lt(duration_from_millis(1), d))
    let start = instant_now()
    sleep_ms(20)
    println(duration_as_nanos(instant_elapsed(start)) >= 20000000)
    println(now())
}
"#;
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let (_dir, stdout) = run_ok(&[("mod.code", program)]);
    let out = lines(&stdout);
    assert_eq!(
        &out[..7],
        ["1750000", "1.75", "123", "0", "1000000000", "true", "true"]
    );
    let now: f64 = out[7].parse().unwrap();
    assert!(now >= before && now < before + 600.0, "{now} {before}");
}

#[test]
fn std_complex_arithmetic() {
    let program = r#"
import std.complex.complex.{complex_new, complex_i, complex_mul, complex_div, complex_powi,
                            complex_norm, complex_arg, complex_sqrt, complex_inv}

public func main() {
    let ii = complex_mul(complex_i(), complex_i())
    println(ii.real)
    println(ii.imag)
    let q = complex_div(complex_new(1.0, 2.0), complex_new(3.0, 4.0))
    println(q.real)
    println(q.imag)
    let p = complex_powi(complex_new(1.0, 1.0), -2)
    println(p.real)
    println(p.imag)
    println(complex_norm(complex_new(3.0, 4.0)))
    println(complex_arg(complex_new(0.0, 1.0)))
    let r = complex_sqrt(complex_new(-4.0, 0.0))
    println(r.real)
    println(r.imag)
    let inv = complex_inv(complex_new(0.0, 2.0))
    println(inv.real)
    println(inv.imag)
}
"#;
    let (_dir, stdout) = run_ok(&[("mod.code", program)]);
    let out = lines(&stdout);
    // i*i = -1; (1+2i)/(3+4i) = (11+2i)/25; (1+i)^-2 = 1/(2i) = -i/2;
    // sqrt(-4) = 2i; 1/(2i) = -i/2.
    let want = [
        -1.0,
        0.0,
        0.44,
        0.08,
        0.0,
        -0.5,
        5.0,
        std::f64::consts::FRAC_PI_2,
        0.0,
        2.0,
        0.0,
        -0.5,
    ];
    for (i, w) in want.into_iter().enumerate() {
        let got: f64 = out[i].parse().unwrap();
        assert!((got - w).abs() <= 1e-15, "line {i}: {got} != {w}");
    }
}

#[test]
fn std_sys_terminate_panics_with_its_message() {
    let program = r#"
import std.sys.terminate.{panic, assert, write_stderr}

public func main() {
    assert(1 + 1 == 2)
    println(1)
    write_stderr("to stderr\n")
    panic("boom")
}
"#;
    let dir = package_with(&[("mod.code", program)]);
    let output = codira(dir.path(), "run", &std_dir());
    assert!(!output.status.success());
    assert_eq!(text(&output.stdout), "1\n");
    let stderr = text(&output.stderr);
    assert!(stderr.contains("to stderr\npanic: boom\n"), "{stderr}");
}

#[test]
fn std_collections_optional_unwraps_and_panics_on_none() {
    let program = r#"
import std.collections.optional.{some, none, is_some, is_none, unwrap, unwrap_or, or_opt}

public func main() {
    let a = some(41)
    let b = none(0)
    println(is_some(a))
    println(is_none(b))
    println(unwrap(a) + 1)
    println(unwrap_or(b, 7))
    println(unwrap(or_opt(b, some(9))))
    unwrap(b)
    println(2)
}
"#;
    let dir = package_with(&[("mod.code", program)]);
    let output = codira(dir.path(), "run", &std_dir());
    assert!(!output.status.success());
    assert_eq!(text(&output.stdout), "true\ntrue\n42\n7\n9\n");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("panic: Optional.unwrap: called on None\n"),
        "{stderr}"
    );
}

/// `std.result.result`, `std.fs.file` and `std.text.ascii` import together:
/// a file is read as a `Result`, its words parsed with `?`, and a missing
/// file and a bad word come back as errors.
#[test]
fn std_result_fs_file_and_text_ascii_read_and_parse_a_file() {
    let work = tempfile::tempdir().unwrap();
    let good = work.path().join("good.txt");
    std::fs::write(&good, "40 -3\n5\n").unwrap();
    let bad = work.path().join("bad.txt");
    std::fs::write(&bad, "7 x").unwrap();
    let program = r#"
import std.result.result.{Result}
import std.fs.file.{read_file, FileError}
import std.text.ascii.{next_word, parse_i64, ParseError}

enum Problem { Io(FileError), Parse(ParseError) }

func total(path: str) -> Result[i64, Problem] {
    let bytes = read_file(path)?
    var sum: i64 = 0
    var at: usize = 0
    while let (start, end) = next_word(bytes, at) {
        sum += parse_i64(bytes, start, end)?
        at = end
    }
    Result.Ok(sum)
}

func show(r: Result[i64, Problem]) {
    match r {
        Result.Ok(v) -> println(v),
        Result.Err(Problem.Io(FileError.NotFound)) -> println("not found"),
        Result.Err(Problem.Io(_)) -> println("io"),
        Result.Err(Problem.Parse(ParseError.InvalidDigit(at))) -> println(at),
        Result.Err(Problem.Parse(_)) -> println("parse"),
    }
}

public func main() {
    show(total("{good}"))
    show(total("{bad}"))
    show(total("{good}.missing"))
}
"#
    .replace("{good}", good.to_str().unwrap())
    .replace("{bad}", bad.to_str().unwrap());
    let (_dir, stdout) = run_ok(&[("mod.code", &program)]);
    assert_eq!(stdout, "42\n2\nnot found\n");
}

/// `Map`, `Set`, `Deque`, `BinaryHeap` and `Counter` from `std.collections`,
/// on the growable arrays, against values worked out by hand.
#[test]
fn std_collections_map_set_deque_heap_and_counter() {
    let program = r#"
import std.collections.map.{Map}
import std.collections.set.{Set}
import std.collections.deque.{Deque}
import std.collections.binary_heap.{BinaryHeap}
import std.collections.counter.{Counter}
import std.cmp.cmp.{min_of, max_of}

public func main() {
    let m: Map[i64, i64] = Map.new()
    for i in 0..1000 {
        m.insert(i, i * i);
    }
    println(m.len(), " ", m.get(10)!, " ", m.get(1000) == nil, " ", m.contains_key(999))
    println(m.insert(10, 5)!, " ", m.get(10)!, " ", m.len())
    for i in 0..500 {
        m.remove(i * 2);
    }
    println(m.len(), " ", m.get(10) == nil, " ", m.get(11)!, " ", m.get_or(10, -1))
    println(m.keys().len(), " ", m.values().sum())
    m.clear()
    println(m.len(), " ", m.get(11) == nil)

    let ages: Map[str, f64] = Map.new()
    ages.insert("ada", 36.5)
    ages.insert("alan", 41.0)
    println(ages.get("ada")!, " ", ages.get("bob") == nil, " ", ages.len())

    let s: Set[str] = Set.new()
    println(s.insert("a"), " ", s.insert("b"), " ", s.insert("a"), " ", s.len())
    println(s.contains("b"), " ", s.remove("b"), " ", s.remove("b"), " ", s.len())
    let t = Set.from_array([1, 2, 3, 4])
    let u = Set.from_array([3, 4, 5])
    println(t.union(u).len(), " ", t.intersection(u).len(), " ", t.difference(u).len())
    println(t.is_subset(u), " ", Set.from_array([3]).is_subset(u))

    let q: Deque[i64] = Deque.new()
    for i in 0..10 { q.push_back(i) }
    for i in 1..5 { q.push_front(-i) }
    println(q.len(), " ", q.front()!, " ", q.back()!, " ", q.get(4))
    println(q.pop_back()!, " ", q.pop_front()!, " ", q.len())
    var total = 0
    while let x = q.pop_front() { total += x }
    println(total, " ", q.is_empty(), " ", q.pop_back() == nil)

    let h: BinaryHeap[i64] = BinaryHeap.new()
    for x in [5, 1, 8, 3, 9, 2, 8] { h.push(x) }
    print(h.peek()!, ":")
    while let x = h.pop() { print(" ", x) }
    println()
    let sorted = BinaryHeap.from_array(["pear", "fig", "apple"]).to_sorted_array()
    println(sorted[0], " ", sorted[2], " ", min_of(3, 2), " ", max_of(1.5, 0.5))

    let words = Counter.from_array(["a", "b", "a", "c", "a", "b"])
    println(words.count("a"), " ", words.count("z"), " ", words.len(), " ", words.total())
    println(words.most_common()!, " ", words.subtract("a"), " ", words.count("a"))
}
"#;
    let (_dir, stdout) = run_ok(&[("mod.code", program)]);
    assert_eq!(
        stdout,
        "1000 100 true true\n\
         100 5 1000\n\
         500 true 121 -1\n\
         500 166666500\n\
         0 true\n\
         36.5 true 2\n\
         true true false 2\n\
         true true false 1\n\
         5 2 2\n\
         false true\n\
         14 -4 9 0\n\
         9 -4 12\n\
         30 true true\n\
         9: 9 8 8 5 3 2 1\n\
         apple pear 2 1.5\n\
         3 0 3 6\n\
         a true 2\n"
    );
}

/// `Map[i64, i64]` replays random insertions, lookups and removals the same
/// way Rust's `HashMap` does.
#[test]
fn std_collections_map_matches_rust_hash_map() {
    let mut state = 0x1234_5678_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut reference = std::collections::HashMap::new();
    let mut code = String::from(
        "import std.collections.map.{Map}\n\npublic func main() {\n    \
         let m: Map[i64, i64] = Map.new()\n",
    );
    let mut expected = String::new();
    for _ in 0..1500 {
        let key = (next() % 300) as i64;
        let value = (next() % 1000) as i64;
        match next() % 4 {
            0 | 1 => {
                code.push_str(&format!(
                    "    if let o = m.insert({key}, {value}) {{ print(o, \",\") }} \
                     else {{ print(\"-,\") }}\n"
                ));
                match reference.insert(key, value) {
                    Some(old) => expected.push_str(&format!("{old},")),
                    None => expected.push_str("-,"),
                }
            }
            2 => {
                code.push_str(&format!(
                    "    if let o = m.remove({key}) {{ print(o, \",\") }} \
                     else {{ print(\"-,\") }}\n"
                ));
                match reference.remove(&key) {
                    Some(old) => expected.push_str(&format!("{old},")),
                    None => expected.push_str("-,"),
                }
            }
            _ => {
                code.push_str(&format!("    print(m.get_or({key}, -1), \",\")\n"));
                let found = reference.get(&key).copied().unwrap_or(-1);
                expected.push_str(&format!("{found},"));
            }
        }
    }
    code.push_str("    println(m.len())\n}\n");
    expected.push_str(&format!("{}\n", reference.len()));
    let (_dir, stdout) = run_ok(&[("mod.code", &code)]);
    assert_eq!(stdout, expected);
}
