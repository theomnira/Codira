//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - End-to-end tests of `String`: programs are built and run by the real
//!   `codira` binary, and their output, panics and exit status are checked.

use std::{path::Path, process::Command};

/// Writes a one-file package whose `src/mod.code` is `source`.
fn package_with(source: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_strings")
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

/// Runs `codira run` on the package in `dir` with `extra` arguments.
fn codira_run(dir: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("run")
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .args(extra)
        .output()
        .expect("failed to spawn codira")
}

/// Builds and runs `source`, asserting success, and returns its stdout.
fn run_ok(source: &str) -> String {
    let dir = package_with(source);
    let output = codira_run(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert!(
        output.status.success(),
        "codira run failed:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Builds and runs `source`, asserting a panic (exit status 101), and returns
/// its stderr.
fn run_panics(source: &str) -> String {
    let dir = package_with(source);
    let output = codira_run(dir.path(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    assert_eq!(
        output.status.code(),
        Some(101),
        "expected a panic:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    stderr
}

/// Builds `source` and returns the compiler's diagnostics, asserting the
/// build failed.
fn build_fails(source: &str) -> String {
    let dir = package_with(source);
    let output = codira_run(dir.path(), &[]);
    assert!(!output.status.success(), "the build was expected to fail");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn construction_concatenation_and_printing() {
    let out = run_ok(
        r#"
public func main() {
    let a = "hello".to_string()
    let b = a + ", " + "world"
    println(b)
    println(b.len(), " ", b.is_empty(), " ", "".to_string().is_empty())
    let n = 42
    println("n = " + n.to_string() + ", x = " + 2.5.to_string() + ", " + true.to_string())
    println((-7i8).to_string(), " ", 18446744073709551615u64.to_string())
    println(170141183460469231731687303715884105727i128.to_string())
    let empty: String = "".to_string()
    println("[", empty, "]")
}
"#,
    );
    assert_eq!(
        out,
        "hello, world\n12 false true\nn = 42, x = 2.5, true\n-7 18446744073709551615\n\
         170141183460469231731687303715884105727\n[]\n"
    );
}

#[test]
fn appending_in_a_loop_shares_nothing_it_should_not() {
    let out = run_ok(
        r#"
public func main() {
    var s = "".to_string()
    for i in 0..5 {
        s += i.to_string()
        s += ","
    }
    println(s)
    // `a` ends at its buffer's fill: `b` and then `c` both start from it,
    // and neither may see the other's bytes.
    let a = "ab".to_string() + "c"
    let b = a + "X"
    let c = a + "Y"
    println(a, " ", b, " ", c)
    var t = a
    t += "Z"
    println(a, " ", t)
    // Appending a string to itself.
    var d = "xy".to_string()
    d += d
    d = d + d
    println(d)
    // Building a long string.
    var long = "".to_string()
    var i = 0
    while i < 10000 {
        long += "abc"
        i += 1
    }
    println(long.len())
}
"#,
    );
    assert_eq!(
        out,
        "0,1,2,3,4,\nabc abcX abcY\nabc abcZ\nxyxyxyxy\n30000\n"
    );
}

#[test]
fn equality_ordering_and_hashing() {
    let out = run_ok(
        r#"
public func main() {
    let a = "apple".to_string()
    let b = "app".to_string() + "le"
    println(a == b, " ", a != b, " ", a == "apple", " ", "apple" == a, " ", a == "apples")
    println(a < "banana", " ", "banana" < a, " ", a <= b, " ", a >= b, " ", a > "apple")
    println("abc" < "abd", " ", "é" > "z")
    println(a.hash() == b.hash(), " ", a.hash() == "pear".to_string().hash())
    println("".to_string().hash())
}
"#,
    );
    // FNV-1a 64 of nothing is its offset basis.
    assert_eq!(
        out,
        "true false true true false\ntrue false true true false\ntrue true\ntrue false\n\
         14695981039346656037\n"
    );
}

#[test]
fn lengths_chars_and_bytes() {
    let out = run_ok(
        r#"
public func main() {
    let s = "héllo, 世界".to_string()
    println(s.len(), " ", s.char_count())
    let cs = s.chars()
    println(cs.len(), " ", cs[1], " ", cs[7])
    let bs = s.bytes()
    bs[0] = 72u8
    println(bs.len(), " ", bs[0], " ", bs[1], " ", s.byte_at(0), " ", s)
    for c in "abc".to_string().chars() {
        print(c, " ")
    }
    println()
}
"#,
    );
    assert_eq!(
        out,
        "14 9\n9 233 19990\n14 72 195 104 héllo, 世界\n97 98 99 \n"
    );
}

#[test]
fn searching() {
    let out = run_ok(
        r#"
public func main() {
    let s = "the cat sat on the mat".to_string()
    println(s.find("the")!, " ", s.rfind("the")!, " ", s.find("dog") == nil)
    if let i = s.find("sat") { println("sat at ", i) }
    println(s.contains("mat"), " ", s.contains("dog"), " ", s.contains(""))
    println(s.starts_with("the"), " ", s.ends_with("mat"), " ", s.starts_with("cat"))
    let needle = "on".to_string()
    println(s.find(needle)!)
}
"#,
    );
    assert_eq!(
        out,
        "0 15 true\nsat at 8\ntrue false true\ntrue true false\n12\n"
    );
}

#[test]
fn splitting_trimming_and_slicing() {
    let out = run_ok(
        r#"
public func main() {
    let csv = "a,b,,c".to_string()
    let parts = csv.split(",")
    println(parts.len())
    for p in parts { print("[", p, "]") }
    println()
    let text = "one\ntwo\r\nthree\n".to_string()
    let lines = text.lines()
    println(lines.len(), " ", lines[1], " ", lines[2].len())
    let padded = "  \t hi there \n".to_string()
    println("[", padded.trim(), "] [", padded.trim_start(), "] [", padded.trim_end(), "]")
    println("[", "   ".to_string().trim(), "]")
    let s = "héllo".to_string()
    println(s.slice(0, 1), " ", s.slice(1, 3), " ", s.slice(3, 6), " [", s.slice(2 - 1, 1), "]")
    // A view can be appended to without disturbing what it came from.
    var head = s.slice(0, 3)
    head += "!"
    println(head, " ", s)
}
"#,
    );
    assert_eq!(
        out,
        "4\n[a][b][][c]\n3 two 5\n[hi there] [hi there \n] [  \t hi there]\n[]\n\
         h é llo []\nhé! héllo\n"
    );
}

#[test]
fn case_replace_and_repeat() {
    let out = run_ok(
        r#"
public func main() {
    let s = "Hello, Wörld".to_string()
    println(s.to_upper(), " ", s.to_lower())
    println(s.replace("l", "L"), " ", s.replace("Wörld", "you"), " ", s.replace("zz", "y"))
    println("ab".to_string().repeat(3), " [", "ab".to_string().repeat(0), "]")
}
"#,
    );
    assert_eq!(
        out,
        "HELLO, WöRLD hello, wörld\nHeLLo, WörLd Hello, you Hello, Wörld\nababab []\n"
    );
}

#[test]
fn parsing_numbers() {
    let out = run_ok(
        r#"
func show_i(s: String) {
    if let v = s.parse_i64() { println(s, " -> ", v) } else { println(s, " -> nil") }
}

func show_f(s: String) {
    if let v = s.parse_f64() { println(s, " -> ", v) } else { println(s, " -> nil") }
}

public func main() {
    show_i("42".to_string())
    show_i("-9223372036854775808".to_string())
    show_i("9223372036854775808".to_string())
    show_i(" 1".to_string())
    show_i("+7".to_string())
    show_i("".to_string())
    show_f("2.5".to_string())
    show_f("1e3".to_string())
    show_f("abc".to_string())
    let total = "10,20,30".to_string().split(",")
    var sum = 0i64
    for t in total { sum += t.parse_i64()! }
    println(sum)
}
"#,
    );
    assert_eq!(
        out,
        "42 -> 42\n-9223372036854775808 -> -9223372036854775808\n\
         9223372036854775808 -> nil\n 1 -> nil\n+7 -> 7\n -> nil\n\
         2.5 -> 2.5\n1e3 -> 1000.0\nabc -> nil\n60\n"
    );
}

#[test]
fn from_utf8_validates_and_copies() {
    let out = run_ok(
        r#"
public func main() {
    let good = [104u8, 105u8, 0xC3u8, 0xA9u8]
    let s = string_from_utf8(good)!
    good[0] = 72u8
    println(s, " ", s.len())
    let bad = [0xC3u8, 0x28u8]
    println(string_from_utf8(bad) == nil)
    println(string_from_utf8([0xEDu8, 0xA0u8, 0x80u8]) == nil)
}
"#,
    );
    assert_eq!(out, "hié 4\ntrue\ntrue\n");
}

#[test]
fn strings_in_structs_classes_arrays_and_functions() {
    let out = run_ok(
        r#"
struct Named {
    name: String,
    age: i64,
}

class Person {
    var name: String
}

func greet(who: String) -> String {
    "hello " + who
}

public func main() {
    let n = Named { name: "ann".to_string(), age: 3 }
    let p = Person { name: "bob".to_string() }
    p.name += "by"
    let names = [n.name, p.name, greet(n.name)]
    names[0] = names[0].to_upper()
    for x in names { print(x, ";") }
    println()
    let maybe: String? = nil
    println(maybe == nil)
    let opt: String? = "x".to_string()
    println(opt!)
}
"#,
    );
    assert_eq!(out, "ANN;bobby;hello ann;\ntrue\nx\n");
}

#[test]
fn slicing_off_a_char_boundary_panics_with_its_location() {
    let err = run_panics(
        r#"
public func main() {
    let s = "héllo".to_string()
    println(s.slice(0, 2))
}
"#,
    );
    assert!(
        err.contains(
            "panic at mod.code:4:13: byte index 2 is not a char boundary; it is inside 'é' (bytes 1..3)"
        ),
        "{err}"
    );
}

#[test]
fn out_of_range_indices_panic() {
    let err = run_panics(
        r#"
public func main() {
    let s = "abc".to_string()
    let n = -1
    println(s.slice(n, 2))
}
"#,
    );
    assert!(
        err.contains(
            "panic at mod.code:5:13: string slice -1..2 is out of range for a string of length 3"
        ),
        "{err}"
    );
    let err = run_panics(
        r#"
public func main() {
    let s = "abc".to_string()
    println(s.slice(2, 1))
}
"#,
    );
    assert!(
        err.contains("string slice 2..1 starts after it ends"),
        "{err}"
    );
    let err = run_panics(
        r#"
public func main() {
    let s = "abc".to_string()
    println(s.byte_at(3))
}
"#,
    );
    assert!(
        err.contains(
            "panic at mod.code:4:13: index out of bounds: the length is 3 but the index is 3"
        ),
        "{err}"
    );
    let err = run_panics(
        r#"
public func main() {
    let parts = "abc".to_string().split("")
    println(parts.len())
}
"#,
    );
    assert!(
        err.contains("`split` was given an empty separator"),
        "{err}"
    );
}

#[test]
fn type_errors_are_reported() {
    let out = build_fails(
        r#"
public func main() {
    let s = "abc".to_string()
    let a = s + 1
    let b = s.find(3)
    let e = s.nope()
}
"#,
    );
    assert!(out.contains("cannot apply binary operator"), "{out}");
    assert!(
        out.contains("`find` takes a `String` or a `str`, not `"),
        "{out}"
    );
    assert!(out.contains("method `nope` does not exist"), "{out}");
}

#[test]
fn a_str_converts_to_a_string_where_one_is_expected() {
    let out = run_ok(
        r#"
class Person {
    var name: String
}

func greet(who: String) -> String {
    if who.is_empty() { return "nobody" }
    "hi " + who
}

public func main() {
    let s: String = "typed"
    var t = s
    t = "reassigned"
    let p = Person { name: "ann" }
    p.name = "bob"
    let names: [String] = ["x", "y"]
    println(s, " ", t, " ", p.name, " ", names[1], " ", greet("cat"), " ", greet(""))
    let csv = "1,2,3".split(",")
    println(csv.len(), " ", "  pad ".trim(), "|", "abc".to_upper(), " ", "a-b".replace("-", "+"))
}
"#,
    );
    assert_eq!(out, "typed reassigned bob y hi cat nobody\n3 pad|ABC a+b\n");
}

#[test]
fn strings_are_values_that_nothing_can_change_through_another_name() {
    let out = run_ok(
        r#"
public func main() {
    let base = "ab".to_string()
    var x = base
    var y = base
    x += "1"
    y += "2"
    let parts = (x + "," + y).split(",")
    var z = parts[0]
    z += "!"
    println(base, " ", x, " ", y, " ", parts[0], " ", parts[1], " ", z)
}
"#,
    );
    assert_eq!(out, "ab ab1 ab2 ab1 ab2 ab1!\n");
}

#[test]
fn interpolated_literals() {
    let out = run_ok(
        r#"
struct Point {
    x: f64,
    y: f64,
}

class Player {
    var name: String,
    var pos: Point,
}

extend Player {
    func describe(self) -> String {
        f"{self.name} at ({self.pos.x}, {self.pos.y})"
    }
}

public func main() {
    let n = 42
    let ok = true
    let name = "ann".to_string()
    let p = Player { name: "bob", pos: Point { x: 1.5, y: -2.0 } }
    println(f"n = {n}, ok = {ok}, name = {name}!")
    println(f"{n}")
    println(f"{{literal braces}} and \"quotes\" and {name}{name}")
    println(p.describe())
    println(f"")
    var log = "".to_string()
    for i in 0..3 {
        log += f"[{i}]"
    }
    println(log)
}
"#,
    );
    assert_eq!(
        out,
        "n = 42, ok = true, name = ann!\n42\n{literal braces} and \"quotes\" and annann\n\
         bob at (1.5, -2.0)\n\n[0][1][2]\n"
    );
}

#[test]
fn interpolation_errors_are_reported() {
    let out = build_fails(
        r#"
struct Point { x: f64 }

public func main() {
    let p = Point { x: 1.0 }
    let xs = [1, 2]
    println(f"p = {p}")
    println(f"xs = {xs}")
    println(f"missing = {nope}")
    println(f"unbalanced {p.x")
    println(f"call {f(1)}")
}
"#,
    );
    assert_eq!(
        out.matches("error: method `to_string` does not exist on `Point`")
            .count(),
        1,
        "{out}"
    );
    assert_eq!(
        out.matches("error: method `to_string` does not exist on `[")
            .count(),
        1,
        "{out}"
    );
    assert!(out.contains("error: cannot find value"), "{out}");
    assert_eq!(
        out.matches("error: invalid literal value").count(),
        2,
        "{out}"
    );
}

/// `String`s run on the Cranelift backend and print what LLVM prints; a
/// string method it does not compile yet runs the program on LLVM instead,
/// with a note naming the method.
#[test]
fn jit_runs_strings_and_names_what_it_does_not_compile() {
    let dir = package_with(
        r#"
public func main() {
    let s = "jit".to_string() + "?"
    println(s)
}
"#,
    );
    let output = codira_run_with_env(dir.path(), &["--jit"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(stdout.replace("\r\n", "\n"), "jit?\n");
    assert!(stderr.contains("cranelift: compiled in"), "{stderr}");

    let dir = package_with(
        r#"
public func main() {
    println("  a b ".trim())
}
"#,
    );
    let output = codira_run_with_env(dir.path(), &["--jit"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stdout.replace("\r\n", "\n"), "a b\n");
    assert!(
        stderr.contains("note: running on the LLVM backend")
            && stderr.contains("the string method `trim`"),
        "{stderr}"
    );
}

/// `codira run` of the package in `dir` with `args`, reporting JIT timings.
fn codira_run_with_env(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("run")
        .args(args)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .args(["--color", "disable"])
        .env("CODIRA_JIT_TIMINGS", "1")
        .output()
        .unwrap()
}
