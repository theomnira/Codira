//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of format specs in interpolation holes and of the
//!   debug form: programs are built and run by the real `codira` binary.
//!   Format specs are checked against Rust's `format!` on seeded random
//!   values, widths and precisions; the debug form against golden text and,
//!   for the shapes Rust's `#[derive(Debug)]` writes the same way, against
//!   Rust.

use std::{path::Path, process::Command};

/// Writes a one-file package whose `src/mod.code` is `source`.
fn package_with(source: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_formatting")
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

/// Runs `codira run` on the package in `dir`.
fn codira_run(dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("run")
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .output()
        .expect("failed to spawn codira")
}

/// Builds and runs `source`, asserting success, and returns its stdout.
fn run_ok(source: &str) -> String {
    let dir = package_with(source);
    let output = codira_run(dir.path());
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    assert!(
        output.status.success(),
        "codira run failed:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Builds `source` and returns the compiler's diagnostics, asserting the
/// build failed.
fn build_fails(source: &str) -> String {
    let dir = package_with(source);
    let output = codira_run(dir.path());
    assert!(!output.status.success(), "the build was expected to fail");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// xorshift64*: small, fast and deterministic, so every run checks the
/// same cases.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// One spec: the hole as Codira writes it (`v` is the value, `w` and `p`
/// the width and precision variables) and the same spec for Rust.
type Case<T> = (&'static str, fn(T, usize, usize) -> String);

/// Builds a program that prints, for each `(value, width, precision)`, one
/// line of every case's hole separated by `|`, runs it, and compares each
/// line with what Rust's `format!` writes.
fn check_against_rust<T: Copy>(
    element_type: &str,
    values: &[(String, T)],
    rng: &mut Rng,
    max_precision: u64,
    cases: &[Case<T>],
) {
    let widths: Vec<usize> = values.iter().map(|_| rng.below(28) as usize).collect();
    let precisions: Vec<usize> = values
        .iter()
        .map(|_| rng.below(max_precision + 1) as usize)
        .collect();
    let list = |items: Vec<String>| items.join(", ");
    let line: Vec<String> = cases
        .iter()
        .map(|(hole, _)| format!("{{{hole}}}"))
        .collect();
    let source = format!(
        "public func main() {{\n    let vs: [{element_type}] = [{}]\n    let ws: [i64] = [{}]\n    \
         let ps: [i64] = [{}]\n    for i in 0..{} {{\n        let v = vs[i]\n        let w = \
         ws[i]\n        let p = ps[i]\n        println(f\"{}\")\n    }}\n}}\n",
        list(values.iter().map(|(text, _)| text.clone()).collect()),
        list(widths.iter().map(ToString::to_string).collect()),
        list(precisions.iter().map(ToString::to_string).collect()),
        values.len(),
        line.join("|"),
    );
    let out = run_ok(&source);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), values.len(), "{out}");
    for (index, ((text, value), got)) in values.iter().zip(lines).enumerate() {
        let (w, p) = (widths[index], precisions[index]);
        let expected: Vec<String> = cases.iter().map(|(_, rust)| rust(*value, w, p)).collect();
        let got: Vec<&str> = got.split('|').collect();
        for ((hole, _), (expected, got)) in cases.iter().zip(expected.iter().zip(got)) {
            assert_eq!(
                got, expected,
                "`{{{hole}}}` with v = {text}, w = {w}, p = {p}"
            );
        }
    }
}

#[test]
fn integer_specs_match_rust() {
    let mut rng = Rng(0x5eed_f00d_1234_5678);
    let mut values: Vec<(String, i64)> = [0, 1, -1, 42, -42, i64::MAX, i64::MIN + 1]
        .into_iter()
        .map(|v| (v.to_string(), v))
        .collect();
    values.push(("-9223372036854775807 - 1".to_owned(), i64::MIN));
    for _ in 0..120 {
        let v = match rng.below(3) {
            0 => rng.next() as i64,
            1 => rng.below(20001) as i64 - 10000,
            _ => (rng.next() >> rng.below(64)) as i64,
        };
        let text = if v == i64::MIN {
            "(-9223372036854775807 - 1)".to_owned()
        } else {
            format!("({v})")
        };
        values.push((text, v));
    }
    let cases: &[Case<i64>] = &[
        ("v", |v, _, _| format!("{v}")),
        ("v:{w}", |v, w, _| format!("{v:w$}")),
        ("v:>{w}", |v, w, _| format!("{v:>w$}")),
        ("v:<{w}", |v, w, _| format!("{v:<w$}")),
        ("v:^{w}", |v, w, _| format!("{v:^w$}")),
        ("v:*^{w}", |v, w, _| format!("{v:*^w$}")),
        ("v:0{w}", |v, w, _| format!("{v:0w$}")),
        ("v:+0{w}", |v, w, _| format!("{v:+0w$}")),
        ("v:_<+{w}", |v, w, _| format!("{v:_<+w$}")),
        ("v:+", |v, _, _| format!("{v:+}")),
        ("v:08", |v, _, _| format!("{v:08}")),
        ("v:x", |v, _, _| format!("{v:x}")),
        ("v:#x", |v, _, _| format!("{v:#x}")),
        ("v:#0{w}X", |v, w, _| format!("{v:#0w$X}")),
        ("v:>{w}o", |v, w, _| format!("{v:>w$o}")),
        ("v:#b", |v, _, _| format!("{v:#b}")),
        ("v:.^#{w}b", |v, w, _| format!("{v:.^#w$b}")),
        ("v:+x", |v, _, _| format!("{v:+x}")),
        ("v:e", |v, _, _| format!("{v:e}")),
        ("v:+{w}E", |v, w, _| format!("{v:+w$E}")),
        ("v:.{p}e", |v, _, p| format!("{v:.p$e}")),
        ("v:>{w}.{p}e", |v, w, p| format!("{v:>w$.p$e}")),
        ("v:?", |v, _, _| format!("{v:?}")),
        ("v:>{w}?", |v, w, _| format!("{v:>w$?}")),
    ];
    check_against_rust("i64", &values, &mut rng, 12, cases);
}

#[test]
fn narrow_and_unsigned_integer_specs_match_rust() {
    let mut rng = Rng(0xfeed_beef);
    let narrow: Vec<(String, i8)> = (0..60)
        .map(|_| {
            let v = rng.next() as i8;
            let text = if v == i8::MIN {
                "(-127 - 1)".to_owned()
            } else {
                format!("({v})")
            };
            (text, v)
        })
        .collect();
    let cases: &[Case<i8>] = &[
        ("v:x", |v, _, _| format!("{v:x}")),
        ("v:#06X", |v, _, _| format!("{v:#06X}")),
        ("v:b", |v, _, _| format!("{v:b}")),
        ("v:o", |v, _, _| format!("{v:o}")),
        ("v:>{w}", |v, w, _| format!("{v:>w$}")),
        ("v:+0{w}", |v, w, _| format!("{v:+0w$}")),
    ];
    check_against_rust("i8", &narrow, &mut rng, 0, cases);

    let wide: Vec<(String, u64)> = (0..60)
        .map(|_| {
            let v = rng.next() >> rng.below(64);
            (v.to_string(), v)
        })
        .chain([(u64::MAX.to_string(), u64::MAX)])
        .collect();
    let cases: &[Case<u64>] = &[
        ("v", |v, _, _| format!("{v}")),
        ("v:#x", |v, _, _| format!("{v:#x}")),
        ("v:#0{w}x", |v, w, _| format!("{v:#0w$x}")),
        ("v:^{w}", |v, w, _| format!("{v:^w$}")),
        ("v:+", |v, _, _| format!("{v:+}")),
        ("v:.{p}e", |v, _, p| format!("{v:.p$e}")),
    ];
    check_against_rust("u64", &wide, &mut rng, 20, cases);
}

/// A float that stresses rounding and the special forms.
fn random_float(rng: &mut Rng) -> f64 {
    match rng.below(8) {
        0 => [0.5, 1.5, 2.5, 0.125, 0.375, 1e21, 1e-7, 0.0, 1.0, 0.1][rng.below(10) as usize],
        1 => [f64::MAX, f64::MIN_POSITIVE, 5e-324, 1e300, 123_456_789.0][rng.below(5) as usize],
        2 => (rng.below(2_000_001) as f64 - 1_000_000.0) / 1000.0,
        3 => (rng.below(20_001) as f64 - 10_000.0) / 8.0,
        4 => loop {
            let v = f64::from_bits(rng.next());
            if v.is_finite() {
                break v;
            }
        },
        _ => (rng.next() as i64 as f64) / (1u64 << rng.below(60)) as f64,
    }
}

/// `v` as a Codira expression of type `f64` that is exactly `v`.
fn float_literal(v: f64) -> String {
    if v.is_nan() {
        "(0.0 / 0.0)".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 {
            "(1.0 / 0.0)"
        } else {
            "(-1.0 / 0.0)"
        }
        .to_owned()
    } else if v == 0.0 && v.is_sign_negative() {
        "(-0.0)".to_owned()
    } else if v < 0.0 {
        format!("(-{:?})", -v)
    } else {
        format!("{v:?}")
    }
}

#[test]
fn float_specs_match_rust_rounding_included() {
    let mut rng = Rng(0x0dd_ba11_cafe_f00d);
    let mut values: Vec<(String, f64)> = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.0,
        0.0,
        2.5,
        -2.5,
        0.045,
        1.005,
        9.995,
    ]
    .into_iter()
    .map(|v| (float_literal(v), v))
    .collect();
    for _ in 0..200 {
        let v = random_float(&mut rng);
        let v = if rng.below(2) == 0 { -v } else { v };
        values.push((float_literal(v), v));
    }
    let cases: &[Case<f64>] = &[
        ("v", |v, _, _| format!("{v:?}")),
        ("v:>{w}", |v, w, _| format!("{v:>w$?}")),
        ("v:.{p}", |v, _, p| format!("{v:.p$}")),
        ("v:>{w}.{p}", |v, w, p| format!("{v:>w$.p$}")),
        ("v:<{w}.{p}", |v, w, p| format!("{v:<w$.p$}")),
        ("v:*^{w}.{p}", |v, w, p| format!("{v:*^w$.p$}")),
        ("v:+.{p}", |v, _, p| format!("{v:+.p$}")),
        ("v:0{w}.{p}", |v, w, p| format!("{v:0w$.p$}")),
        ("v:+0{w}", |v, w, _| format!("{v:+0w$?}")),
        ("v:.3", |v, _, _| format!("{v:.3}")),
        ("v:>10.3", |v, _, _| format!("{v:>10.3}")),
        ("v:.0", |v, _, _| format!("{v:.0}")),
        ("v:e", |v, _, _| format!("{v:e}")),
        ("v:.{p}e", |v, _, p| format!("{v:.p$e}")),
        ("v:>{w}.{p}E", |v, w, p| format!("{v:>w$.p$E}")),
        ("v:+0{w}e", |v, w, _| format!("{v:+0w$e}")),
        ("v:?", |v, _, _| format!("{v:?}")),
    ];
    check_against_rust("f64", &values, &mut rng, 17, cases);
}

#[test]
fn f32_specs_are_written_as_f32() {
    let mut rng = Rng(0xf32);
    let values: Vec<(String, f32)> = (0..80)
        .map(|_| {
            let v = random_float(&mut rng) as f32;
            let v = if v.is_finite() { v } else { 1.5 };
            // Exactly representable, so the `f64` literal converts exactly.
            (format!("({} as f32)", float_literal(f64::from(v))), v)
        })
        .collect();
    let cases: &[Case<f32>] = &[
        ("v", |v, _, _| format!("{v:?}")),
        ("v:>{w}.{p}", |v, w, p| format!("{v:>w$.p$}")),
        ("v:e", |v, _, _| format!("{v:e}")),
        ("v:+.{p}e", |v, _, p| format!("{v:+.p$e}")),
    ];
    check_against_rust("f32", &values, &mut rng, 12, cases);
}

#[test]
fn text_and_bool_specs_match_rust() {
    let mut rng = Rng(7);
    let words = [
        "",
        "a",
        "héllo",
        "世界!",
        "\u{1F600}x",
        "long words here",
        "x y",
    ];
    let values: Vec<(String, &str)> = (0..80)
        .map(|_| {
            let s = words[rng.below(words.len() as u64) as usize];
            (format!("{s:?}"), s)
        })
        .collect();
    let cases: &[Case<&str>] = &[
        ("v", |v, _, _| v.to_owned()),
        ("v:{w}", |v, w, _| format!("{v:w$}")),
        ("v:>{w}", |v, w, _| format!("{v:>w$}")),
        ("v:^{w}", |v, w, _| format!("{v:^w$}")),
        ("v:é^{w}.{p}", |v, w, p| format!("{v:é^w$.p$}")),
        ("v:-<{w}.{p}", |v, w, p| format!("{v:-<w$.p$}")),
        ("v:.{p}", |v, _, p| format!("{v:.p$}")),
        ("v:.2", |v, _, _| format!("{v:.2}")),
    ];
    check_against_rust("String", &values, &mut rng, 8, cases);

    let flags: Vec<(String, bool)> = (0..20)
        .map(|_| {
            let b = rng.below(2) == 0;
            (b.to_string(), b)
        })
        .collect();
    let cases: &[Case<bool>] = &[
        ("v:>{w}", |v, w, _| format!("{v:>w$}")),
        ("v:*^{w}", |v, w, _| format!("{v:*^w$}")),
        ("v:?", |v, _, _| format!("{v:?}")),
    ];
    check_against_rust("bool", &flags, &mut rng, 0, cases);
}

#[test]
fn widths_and_precisions_read_fields_and_any_integer_type() {
    let out = run_ok(
        r#"
struct Layout { width: u8, digits: i32 }
public func main() {
    let l = Layout { width: 9, digits: 2 }
    let x = 3.14159
    let negative: i64 = -4
    let n = 42
    println(f"[{x:>{l.width}.{l.digits}}] [{n:>{negative}}] [{x:.{negative}}]")
}
"#,
    );
    // A negative width or precision counts as zero.
    assert_eq!(out, "[     3.14] [42] [3]\n");
}

#[derive(Debug)]
#[allow(dead_code)]
struct Point {
    x: i64,
    y: f64,
}

#[derive(Debug)]
#[allow(dead_code)]
struct Line {
    from: Point,
    to: Point,
    tags: Vec<i64>,
    pair: (bool, f64),
    name: String,
    empty: Vec<i64>,
}

#[derive(Debug)]
#[allow(dead_code)]
struct Wrap(i64, Point);

#[derive(Debug)]
struct Unit;

#[test]
fn the_debug_form_of_structs_arrays_and_tuples_is_rusts() {
    let out = run_ok(
        r#"
struct Point { x: i64, y: f64 }
struct Line { from: Point, to: Point, tags: [i64], pair: (bool, f64), name: String, empty: [i64] }
struct Wrap(i64, Point)
struct Unit;
public func main() {
    let line = Line {
        from: Point { x: 1, y: -2.5 },
        to: Point { x: -3, y: 1e21 },
        tags: [7, 8, 9],
        pair: (true, 0.1),
        name: "a line",
        empty: [],
    }
    let w = Wrap(5, Point { x: 0, y: 0.0 })
    let lines = [line, line]
    let one = (4,)
    println(f"{line:?}")
    println(f"{line:#?}")
    println(f"{w:?}")
    println(f"{w:#?}")
    println(Unit.debug_string())
    println(f"{lines:?}")
    println(f"{one:?}")
    println(f"{one:#?}")
}
"#,
    );
    let line = Line {
        from: Point { x: 1, y: -2.5 },
        to: Point { x: -3, y: 1e21 },
        tags: vec![7, 8, 9],
        pair: (true, 0.1),
        name: "a line".to_owned(),
        empty: Vec::new(),
    };
    let w = Wrap(5, Point { x: 0, y: 0.0 });
    let lines = [&line, &line];
    let one = (4,);
    let expected =
        format!("{line:?}\n{line:#?}\n{w:?}\n{w:#?}\n{Unit:?}\n{lines:?}\n{one:?}\n{one:#?}\n");
    assert_eq!(out, expected);
}

#[test]
fn the_debug_form_of_enums_optionals_strings_and_generics() {
    let out = run_ok(
        r#"
enum Shape { Circle(f64), Rect(f64, f64), Dot }
enum Tree[T] { Leaf(T), Node([Tree[T]]) }
struct Box[T] { value: T }
struct Named { name: String, tag: str, alias: String? }

func show[T](x: T) -> String { f"<{x:?}>" }

public func main() {
    let shapes = [Shape.Circle(1.5), Shape.Rect(2.0, 3.0), Shape.Dot]
    println(f"{shapes:?}")
    println(f"{shapes:#?}")
    let tree = Tree.Node([Tree.Leaf(1), Tree.Node([Tree.Leaf(2), Tree.Leaf(3)])])
    println(f"{tree:?}")
    let quoted = Named { name: "tab\there \"q\" back\\slash\n", tag: "ü", alias: nil }
    println(f"{quoted:?}")
    let aliased = Named { name: "", tag: "", alias: "x\x01".to_string() }
    println(aliased.debug_string())
    let maybe: i64? = 3
    let none: i64? = nil
    println(f"{maybe:?} {none:?}")
    let boxed = Box { value: Box { value: [true, false] } }
    println(f"{boxed:?}")
    println(show(boxed))
    println(show(2.5))
    println(show("s"))
    println(show(shapes[0]))
    let f = func(x: i64) -> i64 { x }
    println(f"{f:?}")
}
"#,
    );
    let expected = "\
[Shape.Circle(1.5), Shape.Rect(2.0, 3.0), Shape.Dot]
[
    Shape.Circle(
        1.5,
    ),
    Shape.Rect(
        2.0,
        3.0,
    ),
    Shape.Dot,
]
Tree.Node([Tree.Leaf(1), Tree.Node([Tree.Leaf(2), Tree.Leaf(3)])])
Named { name: \"tab\\there \\\"q\\\" back\\\\slash\\n\", tag: \"ü\", alias: nil }
Named { name: \"\", tag: \"\", alias: \"x\\x01\" }
3 nil
Box { value: Box { value: [true, false] } }
<Box { value: Box { value: [true, false] } }>
<2.5>
<\"s\">
<Shape.Circle(1.5)>
<func>
";
    assert_eq!(out, expected);
}

#[test]
fn a_reference_cycle_prints_a_marker_and_sharing_prints_twice() {
    let out = run_ok(
        r#"
class Node { value: i64, next: Node? }
struct Pair { a: Node, b: Node }
public func main() {
    let a = Node { value: 1, next: nil }
    let b = Node { value: 2, next: a }
    a.next = b
    println(f"{a:?}")
    println(f"{b:#?}")
    let leaf = Node { value: 3, next: nil }
    let shared = Pair { a: leaf, b: leaf }
    println(f"{shared:?}")
    let selfish = Node { value: 4, next: nil }
    selfish.next = selfish
    println(selfish.debug_string())
}
"#,
    );
    let expected = "\
Node { value: 1, next: Node { value: 2, next: Node { .. } } }
Node {
    value: 2,
    next: Node {
        value: 1,
        next: Node { .. },
    },
}
Pair { a: Node { value: 3, next: nil }, b: Node { value: 3, next: nil } }
Node { value: 4, next: Node { .. } }
";
    assert_eq!(out, expected);
}

#[test]
fn a_padded_debug_form_and_types_from_other_modules() {
    let dir = package_with(
        r#"
import shapes.{Square, make}
import std.result.result.{Result}
public func main() {
    let s = make(2)
    println(f"[{s:>20?}] [{s:<20?}]")
    let r: Result[i64, Square] = Result.Err(s)
    println(f"{r:?}")
    let n = 7
    println(f"[{n:>4?}]")
}
"#,
    );
    std::fs::write(
        dir.path().join("src/shapes.code"),
        "public struct Square { side: i64 }\npublic func make(side: i64) -> Square { Square { \
         side: side } }\n",
    )
    .unwrap();
    let output = codira_run(dir.path());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The padded form pads the whole text; a private field is printed too.
    assert_eq!(
        stdout,
        "[  Square { side: 2 }] [Square { side: 2 }  ]\nResult.Err(Square { side: 2 })\n[   7]\n"
    );
}

#[test]
fn a_spec_that_does_not_apply_fails_the_build_at_the_literal() {
    let diags = build_fails(
        r#"
public func main() {
    let n = 3
    let s = f"{n:.2}"
    println(s)
}
"#,
    );
    assert!(
        diags
            .contains("the format spec `.2` does not apply to a `i32`: integers have no precision"),
        "{diags}"
    );
    assert!(diags.contains("mod.code:4:13"), "{diags}");
    let diags = build_fails(
        r#"
public func main() {
    let n = 3
    println(f"{n:>8z}")
}
"#,
    );
    assert!(
        diags.contains("invalid format spec: `>8z`: unknown format type `z`"),
        "{diags}"
    );
}

#[test]
fn formatting_allocates_for_the_effect_analysis() {
    let diags = build_fails(
        r#"
struct P { x: i64 }
@noalloc
func a(n: i64) -> String { f"{n:>4}" }
@noalloc
func b(p: P) -> String { p.debug_string() }
public func main() { println(a(1), b(P { x: 2 })) }
"#,
    );
    assert!(
        diags.contains("`a` is `@noalloc` but allocates here: formatting a hole (`{..:>4}`)"),
        "{diags}"
    );
    assert!(
        diags.contains("`b` is `@noalloc` but allocates here: `debug_string` allocates"),
        "{diags}"
    );
}
