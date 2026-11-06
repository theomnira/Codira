//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of the byte-buffer builtins through the real `codira`
//!   binary: `push_str`, `push_int` and `push_json_float` on `[u8]`,
//!   `find_json_escape` on `String`, and `string_take_utf8`.
//! - Rust is the oracle: seeded random integers are written as
//!   `to_string` writes them, random `f64` bit patterns as `serde_json`
//!   writes them (`ryu`, `null` for NaN and the infinities), and the escape
//!   search agrees with a byte-by-byte scan of random strings of every
//!   length around the 16-byte step.
//! - `string_take_utf8` takes a grown array's buffer without copying it and
//!   leaves the array (and every alias of it) empty; a small array is
//!   copied and emptied too; bytes that are not UTF-8 leave the array as it
//!   was. Strings taken this way keep their bytes while the array is
//!   written again and while other strings append to them.

use std::{fmt::Write as _, path::Path, process::Command};

/// Writes a one-file package whose `src/mod.code` is `source`.
fn package_with(source: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_bytes")
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

/// Builds and runs `source` at `-O 2`, asserting success; returns stdout.
fn run_ok(source: &str) -> String {
    let dir = package_with(source);
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(["run", "-O", "2", "--color", "disable", "--manifest-path"])
        .arg(dir.path().join("codira.toml"))
        .env("CODIRA_STD_PATH", std_dir())
        .output()
        .expect("failed to spawn codira");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert!(
        output.status.success(),
        "codira run failed:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

fn std_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../std")
        .canonicalize()
        .unwrap()
}

/// xorshift64*: deterministic values for the property tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

#[test]
fn numbers_are_written_as_rust_writes_them() {
    let mut rng = Rng(0x5eed_0001);
    let mut ints = vec![0i64, -1, 1, 9, 10, 99, 100, i64::MIN, i64::MAX];
    let mut uints = vec![0u64, u64::MAX, 1 << 63];
    let mut floats = vec![
        0.0f64,
        -0.0,
        1.0,
        0.1,
        1e-5,
        1e-4,
        1e15,
        1e16,
        1e21,
        f64::MAX,
        f64::MIN_POSITIVE,
        5e-324,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    for _ in 0..400 {
        let bits = rng.next();
        ints.push((bits >> (bits % 64)) as i64 * if bits & 1 == 0 { 1 } else { -1 });
        uints.push(rng.next() >> (rng.next() % 64));
        floats.push(f64::from_bits(rng.next()));
        // Short decimals, the common case in documents.
        floats.push((rng.next() % 2_000_000) as f64 / 1000.0 - 1000.0);
    }
    let mut source = String::from(
        "extern \"codira-intrinsic\" {\n    func __intrinsic_bitcast_u64_f64(bits: u64) -> f64;\n}\n\
         public func main() {\n    let b: [u8] = []\n",
    );
    for n in &ints {
        // `i64::MIN` has no literal: it is written as a difference.
        if *n == i64::MIN {
            writeln!(
                source,
                "    let m: i64 = -9223372036854775807 - 1\n    b.push_int(m)\n    b.push(10)"
            )
            .unwrap();
        } else {
            writeln!(source, "    b.push_int({n}i64)\n    b.push(10)").unwrap();
        }
    }
    for n in &uints {
        writeln!(source, "    b.push_int({n}u64)\n    b.push(10)").unwrap();
    }
    // Narrow types widen as their own `to_string` would write them.
    writeln!(source, "    b.push_int(-128i8)\n    b.push(10)\n    b.push_int(255u8)\n    b.push(10)\n    b.push_int(-32768i16)\n    b.push(10)\n    b.push_int(4294967295u32)\n    b.push(10)").unwrap();
    for x in &floats {
        writeln!(
            source,
            "    b.push_json_float(__intrinsic_bitcast_u64_f64({}u64))\n    b.push(10)",
            x.to_bits()
        )
        .unwrap();
    }
    source.push_str("    print(string_from_utf8(b)!)\n}\n");
    let stdout = run_ok(&source);
    let mut expected = String::new();
    for n in &ints {
        writeln!(expected, "{n}").unwrap();
    }
    for n in &uints {
        writeln!(expected, "{n}").unwrap();
    }
    expected.push_str("-128\n255\n-32768\n4294967295\n");
    for x in &floats {
        writeln!(expected, "{}", serde_json::to_string(x).unwrap()).unwrap();
    }
    assert_eq!(stdout, expected);
}

#[test]
fn push_str_appends_strings_views_and_literals() {
    let stdout = run_ok(
        r#"
public func main() {
    let b: [u8] = []
    b.push_str("héllo ")
    let s: String = "wörld, again"
    b.push_str(s.slice(1, 6))
    b.push_str("")
    var i = 0
    while i < 2000 {
        b.push_str(s)
        i += 1
    }
    println(b.len(), " ", string_from_utf8(b)!.slice(0, 26))
}
"#,
    );
    assert_eq!(
        stdout,
        format!("{} héllo örldwörld, againw\n", 7 + 5 + 2000 * 13)
    );
}

#[test]
fn find_json_escape_agrees_with_a_byte_scan() {
    let mut rng = Rng(0x5eed_0002);
    // Codira escapes: a quote, a backslash, control characters, DEL.
    let alphabet: [&str; 9] = [
        "a", "b", "\\\"", "\\\\", "\\x01", "\\x1f", " ", "é", "\\x7f",
    ];
    let mut source = String::from(
        "func scan(s: String, from: usize) -> usize {\n    var i = from\n    while i < s.len() {\n        let b = s.byte_at(i)\n        if b < 32 || b == 34 || b == 92 { return i }\n        i += 1\n    }\n    s.len()\n}\n\
         public func main() {\n    var bad = 0\n    var checked = 0\n",
    );
    let mut case = 0;
    for len in 0..70 {
        for _ in 0..3 {
            let mut lit = String::new();
            for _ in 0..len {
                // Mostly plain letters, so the matches fall at every offset.
                let pick = if rng.next() % 4 == 0 {
                    rng.next() % 9
                } else {
                    rng.next() % 2
                };
                lit.push_str(alphabet[pick as usize]);
            }
            writeln!(
                source,
                "    let s{case} = \"{lit}\".to_string()\n    var f{case}: usize = 0\n    while f{case} <= s{case}.len() + 1 {{\n        let want = scan(s{case}, if f{case} > s{case}.len() {{ s{case}.len() }} else {{ f{case} }})\n        if s{case}.find_json_escape(f{case}) != want {{ bad += 1 }}\n        checked += 1\n        f{case} += 1\n    }}"
            )
            .unwrap();
            case += 1;
        }
    }
    source.push_str(
        "    let neg: i64 = -3\n    println(bad, \" \", checked > 0, \" \", \"ab\\\"\".find_json_escape(neg), \" \", \"x\".find_json_escape(18446744073709551615u64))\n}\n",
    );
    assert_eq!(run_ok(&source), "0 true 2 1\n");
}

#[test]
fn string_take_utf8_moves_a_grown_buffer_and_empties_the_array() {
    let stdout = run_ok(
        r#"
public func main() {
    // Small: copied, and the array is left empty all the same.
    let small: [u8] = [104, 105]
    let a = string_take_utf8(small)!
    println(a, " ", small.len())
    // Grown: the buffer moves; every alias of the array sees it empty.
    let big: [u8] = []
    var i = 0
    while i < 5000 {
        big.push_int(i % 10)
        i += 1
    }
    let alias = big
    let s = string_take_utf8(big)!
    println(s.len(), " ", big.len(), " ", alias.len(), " ", big.capacity(), " ", s.slice(0, 12))
    // The array is usable again, and writing it leaves `s` alone.
    big.push_str("again")
    alias.push_str("!")
    println(s.slice(0, 12), " ", string_from_utf8(big)!)
    // Appending to the taken string uses its spare room or copies; the
    // original value is unchanged either way.
    var t = s
    t += "XYZ"
    println(t.len(), " ", s.len(), " ", t.slice(4998, 5003))
    // Not UTF-8: `nil`, and the array keeps its bytes.
    let bad: [u8] = [97, 255]
    let r = string_take_utf8(bad)
    println(r == nil, " ", bad.len())
    let grown_bad: [u8] = []
    grown_bad.reserve(100)
    grown_bad.push(255)
    println(string_take_utf8(grown_bad) == nil, " ", grown_bad.len())
    // Taking an empty array gives the empty string.
    let empty: [u8] = []
    println(string_take_utf8(empty)!.len())
}
"#,
    );
    assert_eq!(
        stdout,
        "hi 0\n5000 0 0 0 012345678901\n012345678901 again!\n5003 5000 89XYZ\ntrue 2\ntrue 1\n0\n"
    );
}

#[test]
fn byte_buffers_survive_collections() {
    // Taken strings and grown arrays alive at once, with garbage in
    // between, under a 16 MiB heap: the collector runs many times while
    // buffers are handed from arrays to strings (and shrunk on the way),
    // and every string keeps its bytes.
    let dir = package_with(
        r#"
public func main() {
    let kept: [String] = []
    var round = 0
    while round < 300 {
        let b: [u8] = []
        var i = 0
        while i < 100 + round {
            b.push_int(round)
            b.push(44)
            i += 1
        }
        let garbage: [u8] = []
        garbage.reserve(100000)
        garbage.push(1)
        kept.push(string_take_utf8(b)!)
        round += 1
    }
    var total: usize = 0
    var ok = true
    var r = 0
    for s in kept {
        total += s.len()
        // Every piece is `<round>,` repeated.
        let piece = r.to_string() + ","
        if !s.starts_with(piece) || !s.ends_with(piece) { ok = false }
        r += 1
    }
    println(kept.len(), " ", total, " ", ok, " ", kept[299].slice(0, 8))
}
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(["run", "-O", "2", "--runtime-stats", "--color", "disable", "--manifest-path"])
        .arg(dir.path().join("codira.toml"))
        .env("CODIRA_STD_PATH", std_dir())
        .env("CODIRA_HEAP_LIMIT", "16MiB")
        .output()
        .expect("failed to spawn codira");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let mut total = 0;
    for round in 0..300usize {
        total += (100 + round) * (round.to_string().len() + 1);
    }
    assert!(
        stdout.starts_with(&format!("300 {total} true 299,299,\n")),
        "{stdout}"
    );
    let stats = format!("{stdout}{stderr}");
    let collections: usize = stats
        .lines()
        .find_map(|line| line.trim().strip_prefix("collections"))
        .and_then(|rest| rest.trim().split(':').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no collection count in {stats}"));
    assert!(collections > 5, "only {collections} collections:\n{stats}");
}
