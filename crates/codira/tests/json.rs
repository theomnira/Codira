//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Tests of `std.json.json` through the real `codira` binary: one Codira
//!   program (`HARNESS`) reads a file of cases, parses each with the
//!   options it names, and prints the result: the compact and the pretty
//!   text and whether both parse back to an equal value, or the error's
//!   kind, offset, line and column.
//! - Conformance: accepted and rejected texts derived from ECMA-404 and
//!   IETF RFC 8259, with the expected output or error.
//! - Property tests with seeded random values: the compact and pretty texts
//!   Codira prints are byte for byte those `serde_json` prints, and parse
//!   back to equal values.
//! - `serde_json` as the oracle on mutated texts (bytes inserted, deleted,
//!   replaced, truncated): both accept or both reject, and what both accept
//!   they read alike.
//! - Random byte strings and every truncation of valid texts: the parser
//!   never crashes, and agrees with `serde_json` on which are JSON.
//! - Nesting limits: the default, a configured one and the clamp, with the
//!   printer and equality run at the deepest nesting allowed.

use std::{
    collections::BTreeMap,
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;

/// The Codira side: reads `cases.bin` (records of a flags byte, a 4-byte
/// little-endian `max_depth`, a 4-byte length and the text), and for each
/// prints either
///
///     ok <compact round-trips: 0/1> <pretty round-trips: 0/1> <compact length> <pretty length>
///     <compact text>
///     <pretty text>
///
/// or `err <kind> <offset> <line> <column>`. Flag 1 keeps the last of
/// duplicate keys. The texts printed are parsed back with the case's
/// options.
const HARNESS: &str = r#"
import std.json.json.{Json, JsonError, JsonErrorKind, ParseOptions, DuplicateKeys, parse_with, parse_bytes_with, to_string, to_pretty_string, equals}
import std.fs.file.{read_file}
import std.result.result.{Result}

func kind_name(kind: JsonErrorKind) -> str {
    match kind {
        JsonErrorKind.UnexpectedEnd -> "UnexpectedEnd",
        JsonErrorKind.ExpectedValue -> "ExpectedValue",
        JsonErrorKind.ExpectedKey -> "ExpectedKey",
        JsonErrorKind.ExpectedColon -> "ExpectedColon",
        JsonErrorKind.ExpectedCommaOrBracket -> "ExpectedCommaOrBracket",
        JsonErrorKind.ExpectedCommaOrBrace -> "ExpectedCommaOrBrace",
        JsonErrorKind.TrailingComma -> "TrailingComma",
        JsonErrorKind.TrailingCharacters -> "TrailingCharacters",
        JsonErrorKind.InvalidLiteral -> "InvalidLiteral",
        JsonErrorKind.InvalidNumber -> "InvalidNumber",
        JsonErrorKind.NumberOutOfRange -> "NumberOutOfRange",
        JsonErrorKind.ControlCharacter -> "ControlCharacter",
        JsonErrorKind.InvalidEscape -> "InvalidEscape",
        JsonErrorKind.InvalidUnicodeEscape -> "InvalidUnicodeEscape",
        JsonErrorKind.LoneSurrogate -> "LoneSurrogate",
        JsonErrorKind.UnterminatedString -> "UnterminatedString",
        JsonErrorKind.DuplicateKey -> "DuplicateKey",
        JsonErrorKind.DepthLimit -> "DepthLimit",
        JsonErrorKind.InvalidUtf8 -> "InvalidUtf8",
    }
}

func u32_at(b: [u8], at: usize) -> usize {
    (b[at] as usize) | ((b[at + 1] as usize) << 8) | ((b[at + 2] as usize) << 16) | ((b[at + 3] as usize) << 24)
}

func round_trips(text: String, value: Json, options: ParseOptions) -> i64 {
    match parse_with(text, options) {
        Result.Ok(back) -> if equals(back, value) { 1 } else { 0 },
        Result.Err(_) -> 0,
    }
}

public func main() {
    let data = read_file("cases.bin").unwrap()
    var at: usize = 0
    while at < data.len() {
        let flags = data[at]
        let depth = u32_at(data, at + 1)
        let len = u32_at(data, at + 5)
        at += 9
        let bytes = data[at..at + len]
        at += len
        let duplicates = if (flags & 1) != 0 { DuplicateKeys.KeepLast } else { DuplicateKeys.Reject }
        let options = ParseOptions { max_depth: depth, duplicate_keys: duplicates }
        match parse_bytes_with(bytes, options) {
            Result.Ok(v) -> {
                let compact = to_string(v)
                let pretty = to_pretty_string(v)
                println("ok ", round_trips(compact, v, options), " ", round_trips(pretty, v, options), " ", compact.len(), " ", pretty.len())
                println(compact)
                println(pretty)
            },
            Result.Err(e) -> {
                println("err ", kind_name(e.kind), " ", e.offset, " ", e.line, " ", e.column)
            },
        }
    }
}
"#;

/// The standard library of this source tree.
fn std_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../std")
        .canonicalize()
        .unwrap()
}

/// What the harness printed for one case.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Ok {
        compact: String,
        pretty: String,
        compact_round_trips: bool,
        pretty_round_trips: bool,
    },
    Err {
        kind: String,
        offset: usize,
        line: usize,
        column: usize,
    },
}

impl Outcome {
    fn compact(&self) -> Option<&str> {
        match self {
            Outcome::Ok { compact, .. } => Some(compact),
            Outcome::Err { .. } => None,
        }
    }
}

/// One text to parse, with its options.
struct Case {
    text: Vec<u8>,
    max_depth: u32,
    keep_last: bool,
}

impl Case {
    fn new(text: impl Into<Vec<u8>>) -> Self {
        Self {
            text: text.into(),
            max_depth: 128,
            keep_last: false,
        }
    }
}

/// Runs the harness on `cases`; returns one outcome per case.
fn run_cases(cases: &[Case]) -> Vec<Outcome> {
    let dir = tempfile::Builder::new()
        .prefix("codira_json")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), HARNESS).unwrap();
    let mut data = Vec::new();
    for case in cases {
        data.push(u8::from(case.keep_last));
        data.extend_from_slice(&case.max_depth.to_le_bytes());
        data.extend_from_slice(&(case.text.len() as u32).to_le_bytes());
        data.extend_from_slice(&case.text);
    }
    std::fs::write(dir.path().join("cases.bin"), data).unwrap();
    let output: Output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(["run", "-O", "2", "--color", "disable", "--manifest-path"])
        .arg(dir.path().join("codira.toml"))
        .current_dir(dir.path())
        .env("CODIRA_STD_PATH", std_dir())
        .output()
        .expect("failed to spawn codira");
    assert!(
        output.status.success(),
        "the harness failed\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcomes = parse_outcomes(&output.stdout);
    assert_eq!(outcomes.len(), cases.len(), "one outcome per case");
    outcomes
}

/// Reads the harness's output (see `HARNESS`).
fn parse_outcomes(stdout: &[u8]) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    let mut at = 0;
    let line = |at: &mut usize| {
        let end = stdout[*at..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(stdout.len(), |p| *at + p);
        let text = std::str::from_utf8(&stdout[*at..end]).unwrap().to_string();
        *at = end + 1;
        text
    };
    while at < stdout.len() {
        let header = line(&mut at);
        let fields: Vec<&str> = header.split(' ').collect();
        match fields[0] {
            "ok" => {
                let compact_len: usize = fields[3].parse().unwrap();
                let pretty_len: usize = fields[4].parse().unwrap();
                let compact = std::str::from_utf8(&stdout[at..at + compact_len])
                    .unwrap()
                    .to_string();
                at += compact_len + 1;
                let pretty = std::str::from_utf8(&stdout[at..at + pretty_len])
                    .unwrap()
                    .to_string();
                at += pretty_len + 1;
                outcomes.push(Outcome::Ok {
                    compact,
                    pretty,
                    compact_round_trips: fields[1] == "1",
                    pretty_round_trips: fields[2] == "1",
                });
            }
            "err" => outcomes.push(Outcome::Err {
                kind: fields[1].to_string(),
                offset: fields[2].parse().unwrap(),
                line: fields[3].parse().unwrap(),
                column: fields[4].parse().unwrap(),
            }),
            other => panic!("unexpected output line {other:?}: {header}"),
        }
    }
    outcomes
}

/// xorshift64*: deterministic, so every run checks the same cases.
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

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// A character that stresses escaping and UTF-8.
    fn char(&mut self) -> char {
        const SPECIAL: &[char] = &[
            '"',
            '\\',
            '/',
            '\u{0}',
            '\u{1}',
            '\u{8}',
            '\u{9}',
            '\u{a}',
            '\u{c}',
            '\u{d}',
            '\u{1f}',
            '\u{7f}',
            'é',
            '€',
            '\u{2028}',
            '\u{2029}',
            '\u{feff}',
            '😀',
            '\u{10ffff}',
            '\u{fffd}',
            'a',
            'Z',
            ' ',
            '0',
        ];
        match self.below(4) {
            0 => SPECIAL[self.below(SPECIAL.len() as u64) as usize],
            1 => char::from_u32(self.below(0x80) as u32).unwrap(),
            2 => loop {
                if let Some(c) = char::from_u32(self.below(0x11_0000) as u32) {
                    break c;
                }
            },
            _ => char::from(b'a' + self.below(26) as u8),
        }
    }

    fn string(&mut self) -> String {
        let n = self.below(12);
        (0..n).map(|_| self.char()).collect()
    }

    fn float(&mut self) -> f64 {
        loop {
            let x = match self.below(4) {
                0 => f64::from_bits(self.next()),
                1 => (self.next() as i64 as f64) / 1000.0,
                2 => self.below(1000) as f64 / 8.0,
                _ => (self.below(2_000_000) as f64 - 1e6) * 10f64.powi(self.below(40) as i32 - 20),
            };
            if x.is_finite() {
                return x;
            }
        }
    }

    fn int(&mut self) -> i64 {
        match self.below(4) {
            0 => self.next() as i64,
            1 => [i64::MIN, i64::MAX, 0, -1, 1][self.below(5) as usize],
            _ => self.below(2000) as i64 - 1000,
        }
    }

    /// A random value nested at most `depth` deep. Objects are
    /// `serde_json`'s (sorted, unique keys), so the order Codira keeps
    /// is the order `serde_json` prints.
    fn value(&mut self, depth: u32) -> Value {
        let pick = if depth == 0 {
            self.below(6)
        } else {
            self.below(8)
        };
        match pick {
            0 => Value::Null,
            1 => Value::Bool(self.chance(50)),
            2 => Value::from(self.int()),
            3 => Value::from(self.float()),
            4 | 5 => Value::String(self.string()),
            6 => Value::Array((0..self.below(6)).map(|_| self.value(depth - 1)).collect()),
            _ => {
                let mut map = serde_json::Map::new();
                // Sometimes more than the 8 members an object scans.
                let n = if self.chance(10) {
                    9 + self.below(30)
                } else {
                    self.below(6)
                };
                for _ in 0..n {
                    map.insert(self.string(), self.value(depth - 1));
                }
                Value::Object(map)
            }
        }
    }
}

fn ok(outcome: &Outcome) -> (&str, &str) {
    match outcome {
        Outcome::Ok {
            compact,
            pretty,
            compact_round_trips,
            pretty_round_trips,
        } => {
            assert!(
                compact_round_trips,
                "{compact} does not parse back to itself"
            );
            assert!(pretty_round_trips, "{pretty} does not parse back to itself");
            (compact, pretty)
        }
        Outcome::Err { .. } => panic!("rejected: {outcome:?}"),
    }
}

/// Texts the standard accepts, with the compact text Codira prints for
/// each.
const ACCEPTED: &[(&str, &str)] = &[
    ("null", "null"),
    ("true", "true"),
    ("false", "false"),
    ("0", "0"),
    ("-0", "-0.0"),
    ("-0.0", "-0.0"),
    ("1", "1"),
    ("-1", "-1"),
    ("1234567890", "1234567890"),
    ("9223372036854775807", "9223372036854775807"),
    ("-9223372036854775808", "-9223372036854775808"),
    // Beyond `i64`: the nearest float.
    ("9223372036854775808", "9.223372036854776e18"),
    ("18446744073709551616", "1.8446744073709552e19"),
    ("1.5", "1.5"),
    ("-1.5e10", "-15000000000.0"),
    ("1E2", "100.0"),
    ("1e+2", "100.0"),
    ("1e-2", "0.01"),
    ("0.0", "0.0"),
    ("1.0e+5", "100000.0"),
    ("123.456e-789", "0.0"),
    ("1e308", "1e308"),
    ("0.1", "0.1"),
    ("2.2250738585072014e-308", "2.2250738585072014e-308"),
    ("4.9e-324", "5e-324"),
    ("1.7976931348623157e308", "1.7976931348623157e308"),
    ("[]", "[]"),
    ("{}", "{}"),
    ("[[]]", "[[]]"),
    ("[{}]", "[{}]"),
    (" \t\n\r[ 1 ,\n2\t, 3\r] \n", "[1,2,3]"),
    ("  42  ", "42"),
    ("\"\"", "\"\""),
    ("\"a\"", "\"a\""),
    ("\"\\u0000\"", "\"\\u0000\""),
    ("\"\\u001F\\u001f\"", "\"\\u001f\\u001f\""),
    ("\"\\/\"", "\"/\""),
    ("\"\\\"\\\\\\b\\f\\n\\r\\t\"", "\"\\\"\\\\\\b\\f\\n\\r\\t\""),
    ("\"é\"", "\"é\""),
    ("\"\\u00e9\\u00E9\"", "\"éé\""),
    ("\"\\uD834\\uDD1E\"", "\"𝄞\""),
    ("\"\\ud83d\\ude00\"", "\"😀\""),
    ("\"\\u2028\\u2029\"", "\"\u{2028}\u{2029}\""),
    ("\"\u{7f}\"", "\"\u{7f}\""),
    ("\"a\\u0041\"", "\"aA\""),
    ("\"\\uFEFF\"", "\"\u{feff}\""),
    ("{\"\":0}", "{\"\":0}"),
    (
        "{\"a\":{\"b\":[1,{\"c\":null}]}}",
        "{\"a\":{\"b\":[1,{\"c\":null}]}}",
    ),
    ("{\"b\":1,\"a\":2}", "{\"b\":1,\"a\":2}"),
    (
        "[1,true,false,null,\"x\",1.5,[],{}]",
        "[1,true,false,null,\"x\",1.5,[],{}]",
    ),
];

/// Texts the standard rejects, with the error Codira reports: its kind and
/// byte offset.
const REJECTED: &[(&str, &str, usize)] = &[
    ("", "UnexpectedEnd", 0),
    ("   ", "UnexpectedEnd", 3),
    ("[1,]", "TrailingComma", 3),
    ("{\"a\":1,}", "TrailingComma", 7),
    ("[,1]", "ExpectedValue", 1),
    ("[1,,2]", "ExpectedValue", 3),
    ("[1 2]", "ExpectedCommaOrBracket", 3),
    ("{\"a\":1 \"b\":2}", "ExpectedCommaOrBrace", 7),
    ("{\"a\" 1}", "ExpectedColon", 5),
    ("{a:1}", "ExpectedKey", 1),
    ("{'a':1}", "ExpectedKey", 1),
    ("{1:1}", "ExpectedKey", 1),
    ("['a']", "ExpectedValue", 1),
    ("01", "InvalidNumber", 1),
    ("-01", "InvalidNumber", 2),
    ("00", "InvalidNumber", 1),
    ("1.", "UnexpectedEnd", 2),
    ("1.e3", "InvalidNumber", 2),
    ("[1.]", "InvalidNumber", 3),
    (".5", "ExpectedValue", 0),
    ("+1", "ExpectedValue", 0),
    ("1e", "UnexpectedEnd", 2),
    ("1e+", "UnexpectedEnd", 3),
    ("[1e]", "InvalidNumber", 3),
    ("-", "UnexpectedEnd", 1),
    ("-a", "InvalidNumber", 1),
    ("- 1", "InvalidNumber", 1),
    ("1e400", "NumberOutOfRange", 0),
    ("-1e400", "NumberOutOfRange", 0),
    ("NaN", "InvalidLiteral", 0),
    ("Infinity", "InvalidLiteral", 0),
    ("-Infinity", "InvalidNumber", 1),
    ("tru", "UnexpectedEnd", 3),
    ("nul", "UnexpectedEnd", 3),
    ("trux", "InvalidLiteral", 0),
    ("truex", "InvalidLiteral", 0),
    ("True", "InvalidLiteral", 0),
    ("undefined", "InvalidLiteral", 0),
    ("/* c */ 1", "ExpectedValue", 0),
    ("// c\n1", "ExpectedValue", 0),
    ("1 2", "TrailingCharacters", 2),
    ("[1]]", "TrailingCharacters", 3),
    ("0x10", "TrailingCharacters", 1),
    ("1_000", "TrailingCharacters", 1),
    ("\"abc", "UnterminatedString", 4),
    ("\"abc\\", "UnterminatedString", 5),
    ("\"\\x41\"", "InvalidEscape", 1),
    ("\"\\'\"", "InvalidEscape", 1),
    ("\"\\U0041\"", "InvalidEscape", 1),
    ("\"\\u12\"", "InvalidUnicodeEscape", 5),
    ("\"\\u12g4\"", "InvalidUnicodeEscape", 5),
    ("\"\\u12", "UnexpectedEnd", 5),
    ("\"\\uD800\"", "LoneSurrogate", 1),
    ("\"\\uDC00\"", "LoneSurrogate", 1),
    ("\"\\uD800\\u0041\"", "LoneSurrogate", 1),
    ("\"\\uD800x\"", "LoneSurrogate", 1),
    ("\"\\uDBFF\\uDBFF\"", "LoneSurrogate", 1),
    ("\"\u{1}\"", "ControlCharacter", 1),
    ("\"a\tb\"", "ControlCharacter", 2),
    ("\"a\nb\"", "ControlCharacter", 2),
    ("[", "UnexpectedEnd", 1),
    ("[1", "UnexpectedEnd", 2),
    ("{", "UnexpectedEnd", 1),
    ("{\"a\"", "UnexpectedEnd", 4),
    ("{\"a\":", "UnexpectedEnd", 5),
    ("{\"a\":1", "UnexpectedEnd", 6),
    ("{\"a\":1,\"a\":2}", "DuplicateKey", 7),
    ("\u{feff}1", "ExpectedValue", 0),
    ("\u{a0}1", "ExpectedValue", 0),
    ("[1]\u{0}", "TrailingCharacters", 3),
];

/// Every accepted text gives the expected compact text, round-trips, and is
/// accepted by `serde_json` with the same value; every rejected one gives
/// the expected error, and `serde_json` rejects it too (but for a duplicate
/// key, which it keeps, and a byte order mark, which it rejects too).
#[test]
fn conformance_with_the_standard() {
    let mut cases: Vec<Case> = ACCEPTED.iter().map(|(text, _)| Case::new(*text)).collect();
    cases.extend(REJECTED.iter().map(|(text, _, _)| Case::new(*text)));
    // Bytes that are not UTF-8, through `parse_bytes`.
    let not_utf8: &[(&[u8], usize)] = &[
        (b"\"\xff\"", 1),
        (b"[1,\"\xc0\x80\"]", 4),
        (b"\"\xed\xa0\x80\"", 1),
        (b"\"ab\xe2\x82\"", 3),
        (b"\xf5", 0),
    ];
    cases.extend(not_utf8.iter().map(|(bytes, _)| Case::new(bytes.to_vec())));
    let outcomes = run_cases(&cases);

    for ((text, expected), outcome) in ACCEPTED.iter().zip(&outcomes) {
        let (compact, _) = ok(outcome);
        assert_eq!(compact, *expected, "the compact text of {text:?}");
        let ours: Value = serde_json::from_str(compact).unwrap();
        let theirs: Value = serde_json::from_str(text).unwrap();
        assert!(same(&ours, &theirs), "{text:?}: {ours} vs {theirs}");
    }
    for ((text, kind, offset), outcome) in REJECTED.iter().zip(&outcomes[ACCEPTED.len()..]) {
        match outcome {
            Outcome::Err {
                kind: got,
                offset: at,
                ..
            } => {
                assert_eq!(
                    (got.as_str(), *at),
                    (*kind, *offset),
                    "the error of {text:?}"
                );
            }
            Outcome::Ok { compact, .. } => panic!("accepted {text:?} as {compact}"),
        }
        if *kind != "DuplicateKey" {
            assert!(
                serde_json::from_str::<Value>(text).is_err(),
                "serde_json accepts {text:?}"
            );
        }
    }
    for ((_, offset), outcome) in not_utf8
        .iter()
        .zip(&outcomes[ACCEPTED.len() + REJECTED.len()..])
    {
        assert!(
            matches!(outcome, Outcome::Err { kind, offset: at, .. } if kind == "InvalidUtf8" && at == offset),
            "{outcome:?}"
        );
    }
}

/// Numbers compare by value, as `f64` when either is not an `i64`: a
/// Codira `Float` holds an integer beyond `i64` that `serde_json` holds
/// exactly as a `u64`.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(x), Some(y)) => x == y,
            _ => x.as_f64().unwrap().to_bits() == y.as_f64().unwrap().to_bits(),
        },
        (Value::Array(xs), Value::Array(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| same(x, y))
        }
        (Value::Object(xs), Value::Object(ys)) => {
            xs.len() == ys.len()
                && xs
                    .iter()
                    .all(|(k, x)| ys.get(k).is_some_and(|y| same(x, y)))
        }
        _ => a == b,
    }
}

/// Errors carry the line and the column (in characters) besides the offset.
#[test]
fn errors_name_line_column_and_offset() {
    let cases = [
        ("[1,\n  NaN]", ("InvalidLiteral", 6, 2, 3)),
        ("{\n\"é\": x}", ("InvalidLiteral", 8, 2, 6)),
        ("\n\n\n[1,]", ("TrailingComma", 6, 4, 4)),
        ("\"😀😀\" 1", ("TrailingCharacters", 11, 1, 6)),
        ("", ("UnexpectedEnd", 0, 1, 1)),
    ];
    let outcomes = run_cases(&cases.map(|(text, _)| Case::new(text)));
    for ((text, (kind, offset, line, column)), outcome) in cases.iter().zip(&outcomes) {
        assert_eq!(
            *outcome,
            Outcome::Err {
                kind: kind.to_string(),
                offset: *offset,
                line: *line,
                column: *column,
            },
            "{text:?}"
        );
    }
}

/// Random values: what Codira prints, compact and pretty, is byte for byte
/// what `serde_json` prints (the same member order, escapes and float
/// texts), and both texts parse back to an equal value.
#[test]
fn random_values_print_like_serde_json_and_round_trip() {
    let mut rng = Rng(0x5eed_0f15_0a50_17e5);
    let values: Vec<Value> = (0..2500).map(|_| rng.value(5)).collect();
    let cases: Vec<Case> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            // Alternate the compact and the pretty form as input.
            let text = if i % 2 == 0 {
                serde_json::to_string(v).unwrap()
            } else {
                serde_json::to_string_pretty(v).unwrap()
            };
            Case::new(text)
        })
        .collect();
    let outcomes = run_cases(&cases);
    for (value, outcome) in values.iter().zip(&outcomes) {
        let (compact, pretty) = ok(outcome);
        assert_eq!(compact, serde_json::to_string(value).unwrap());
        assert_eq!(pretty, serde_json::to_string_pretty(value).unwrap());
    }
}

/// Documents larger than the printer's 64 KiB chunk, with strings longer
/// than its 1 KiB limit for copying byte by byte (which it appends in runs
/// between escapes), are printed like `serde_json` too: no character is
/// split where a chunk ends.
#[test]
fn large_documents_and_long_strings_print_like_serde_json() {
    let mut rng = Rng(0x10_6e57_2a11);
    let values: Vec<Value> = (0..2)
        .map(|_| {
            let items = (0..45)
                .map(|_| {
                    let n = rng.below(2500);
                    let s: String = (0..n).map(|_| rng.char()).collect();
                    if rng.chance(30) {
                        let mut map = serde_json::Map::new();
                        map.insert(s, rng.value(3));
                        Value::Object(map)
                    } else {
                        Value::String(s)
                    }
                })
                .collect();
            Value::Array(items)
        })
        .collect();
    let cases: Vec<Case> = values
        .iter()
        .map(|v| Case::new(serde_json::to_string(v).unwrap()))
        .collect();
    let outcomes = run_cases(&cases);
    for (value, outcome) in values.iter().zip(&outcomes) {
        let (compact, pretty) = ok(outcome);
        assert!(compact.len() > 65536);
        assert_eq!(compact, serde_json::to_string(value).unwrap());
        assert_eq!(pretty, serde_json::to_string_pretty(value).unwrap());
    }
}

/// A random change to a valid text.
fn mutate(rng: &mut Rng, text: &str) -> String {
    const PIECES: &[&str] = &[
        "{", "}", "[", "]", ",", ":", "\"", "\\", "0", "1", "-", "+", ".", "e", "E", " ", "\n",
        "t", "n", "u", "\\u", "\\ud800", "\\udc00", "00", "1e999", "NaN", "\u{1}", "é", "\t",
        "true", "null", "\"a\"",
    ];
    let mut chars: Vec<char> = text.chars().collect();
    for _ in 0..1 + rng.below(3) {
        let at = rng.below(chars.len() as u64 + 1) as usize;
        match rng.below(4) {
            0 => {
                let piece = PIECES[rng.below(PIECES.len() as u64) as usize];
                chars.splice(at..at, piece.chars());
            }
            1 if at < chars.len() => {
                chars.remove(at);
            }
            2 if at < chars.len() => {
                let piece = PIECES[rng.below(PIECES.len() as u64) as usize];
                chars.splice(at..at + 1, piece.chars());
            }
            _ => chars.truncate(at),
        }
    }
    chars.into_iter().collect()
}

/// `serde_json` as the oracle on texts near JSON: Codira accepts exactly
/// what it accepts (duplicate keys kept, as it does), and reads the same
/// value.
#[test]
fn mutated_texts_agree_with_serde_json() {
    let mut rng = Rng(0xbad_c0de_f00d);
    let mut texts = Vec::new();
    for i in 0..4000 {
        let v = rng.value(4);
        let text = if i % 3 == 0 {
            serde_json::to_string_pretty(&v).unwrap()
        } else {
            serde_json::to_string(&v).unwrap()
        };
        texts.push(mutate(&mut rng, &text));
    }
    let cases: Vec<Case> = texts
        .iter()
        .map(|t| Case {
            text: t.clone().into_bytes(),
            max_depth: 128,
            keep_last: true,
        })
        .collect();
    let outcomes = run_cases(&cases);
    let mut accepted = 0;
    for (text, outcome) in texts.iter().zip(&outcomes) {
        let theirs = serde_json::from_str::<Value>(text);
        match (outcome.compact(), theirs) {
            (Some(compact), Ok(theirs)) => {
                accepted += 1;
                let ours: Value = serde_json::from_str(compact).unwrap();
                assert!(same(&ours, &theirs), "{text:?}: {compact} vs {theirs}");
            }
            (None, Err(_)) => {}
            (ours, theirs) => panic!("{text:?}: codira {ours:?}, serde_json {theirs:?}"),
        }
    }
    // The mutations leave a good share valid, so both paths are exercised.
    assert!(accepted > 400 && accepted < 3600, "{accepted} accepted");
}

/// Random bytes and every prefix of valid texts: the parser answers every
/// one (the harness reaching its end shows it never crashed), rejects what
/// is not UTF-8, and agrees with `serde_json` on the rest.
#[test]
fn random_bytes_and_truncations_never_crash() {
    let mut rng = Rng(0x0dd_ba11);
    let mut cases = Vec::new();
    for _ in 0..3000 {
        let n = rng.below(40) as usize;
        let alphabet = b"{}[],:\"\\0123456789-+.eEtrufalsn \n\t\x00\x1f\x7f\xc3\xa9\xff";
        let bytes: Vec<u8> = (0..n)
            .map(|_| {
                if rng.chance(70) {
                    alphabet[rng.below(alphabet.len() as u64) as usize]
                } else {
                    rng.next() as u8
                }
            })
            .collect();
        cases.push(Case::new(bytes));
    }
    for _ in 0..40 {
        let text = serde_json::to_string(&rng.value(4)).unwrap();
        for end in 0..=text.len() {
            cases.push(Case::new(text.as_bytes()[..end].to_vec()));
        }
    }
    let outcomes = run_cases(&cases);
    for (case, outcome) in cases.iter().zip(&outcomes) {
        match std::str::from_utf8(&case.text) {
            Err(_) => assert!(
                matches!(outcome, Outcome::Err { kind, .. } if kind == "InvalidUtf8"),
                "{:?}: {outcome:?}",
                case.text
            ),
            Ok(text) => {
                let theirs = serde_json::from_str::<Value>(text);
                match (outcome, theirs) {
                    (Outcome::Ok { compact, .. }, Ok(theirs)) => {
                        let ours: Value = serde_json::from_str(compact).unwrap();
                        assert!(same(&ours, &theirs), "{text:?}");
                    }
                    (Outcome::Err { kind, .. }, Ok(_)) if kind == "DuplicateKey" => {}
                    (Outcome::Err { .. }, Err(_)) => {}
                    (ours, theirs) => panic!("{text:?}: codira {ours:?}, serde_json {theirs:?}"),
                }
            }
        }
    }
}

fn nested(depth: usize) -> String {
    let mut text = String::new();
    for i in 0..depth {
        text.push_str(if i % 2 == 0 { "[" } else { "{\"k\":" });
    }
    text.push('1');
    for i in (0..depth).rev() {
        text.push(if i % 2 == 0 { ']' } else { '}' });
    }
    text
}

/// Arrays and objects may nest `max_depth` deep (128 by default), and the
/// error at one more names the byte that would go deeper. A limit above
/// `MAX_DEPTH_LIMIT()` (1000) is clamped, and the parser, the printer and
/// equality handle 1000 levels.
#[test]
fn nesting_is_limited() {
    let with_depth = |depth: usize, max_depth: u32| Case {
        text: nested(depth).into_bytes(),
        max_depth,
        keep_last: false,
    };
    let offset_of_level = |level: usize| {
        // The byte that opens nesting level `level` (1-based).
        let text = nested(level);
        let mut opened = 0;
        text.bytes()
            .position(|b| {
                if b == b'[' || b == b'{' {
                    opened += 1;
                }
                opened == level
            })
            .unwrap()
    };
    let cases = vec![
        with_depth(128, 128),
        with_depth(129, 128),
        with_depth(10, 10),
        with_depth(11, 10),
        with_depth(1, 0),
        with_depth(1000, 1000),
        with_depth(1001, 100_000),
        with_depth(1000, 100_000),
        Case {
            text: b"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[".repeat(50),
            max_depth: 100_000,
            keep_last: false,
        },
    ];
    let outcomes = run_cases(&cases);
    let depth_error = |outcome: &Outcome| match outcome {
        Outcome::Err { kind, offset, .. } if kind == "DepthLimit" => *offset,
        other => panic!("not a depth error: {other:?}"),
    };
    assert_eq!(ok(&outcomes[0]).0, nested(128));
    assert_eq!(depth_error(&outcomes[1]), offset_of_level(129));
    assert_eq!(ok(&outcomes[2]).0, nested(10));
    assert_eq!(depth_error(&outcomes[3]), offset_of_level(11));
    assert_eq!(depth_error(&outcomes[4]), 0);
    // `ok` checks that both texts of the 1000-deep value parse back to it
    // (`equals` at that depth); the pretty text has a line per level.
    let (compact, pretty) = ok(&outcomes[5]);
    assert_eq!(compact, nested(1000));
    assert_eq!(pretty.lines().count(), 2 * 1000 + 1);
    assert_eq!(depth_error(&outcomes[6]), offset_of_level(1001));
    assert_eq!(ok(&outcomes[7]).0, nested(1000));
    assert_eq!(depth_error(&outcomes[8]), 1000);
}

/// Duplicate keys: rejected by default at the second key; with `KeepLast`
/// the last value wins at the first key's position, as in `serde_json`.
/// Objects of more than 8 members (which use a hash index) too.
#[test]
fn duplicate_keys_follow_the_documented_rule() {
    let wide_keys: Vec<String> = (0..20).map(|i| format!("\"k{i}\":{i}")).collect();
    let wide = format!("{{{},\"k3\":\"again\"}}", wide_keys.join(","));
    let cases = vec![
        Case::new("{\"a\":1,\"b\":2,\"a\":3}"),
        Case {
            text: b"{\"a\":1,\"b\":2,\"a\":3}".to_vec(),
            max_depth: 128,
            keep_last: true,
        },
        Case::new(wide.clone()),
        Case {
            text: wide.clone().into_bytes(),
            max_depth: 128,
            keep_last: true,
        },
        Case::new("{\"a\":{\"x\":1},\"b\":{\"x\":2}}"),
    ];
    let outcomes = run_cases(&cases);
    assert!(
        matches!(&outcomes[0], Outcome::Err { kind, offset: 13, .. } if kind == "DuplicateKey")
    );
    assert_eq!(ok(&outcomes[1]).0, "{\"a\":3,\"b\":2}");
    let second_k3 = wide.rfind("\"k3\"").unwrap();
    assert!(
        matches!(&outcomes[2], Outcome::Err { kind, offset, .. } if kind == "DuplicateKey" && *offset == second_k3),
        "{:?}",
        outcomes[2]
    );
    let expected: BTreeMap<String, Value> = (0..20)
        .map(|i| {
            let v = if i == 3 {
                Value::from("again")
            } else {
                Value::from(i)
            };
            (format!("k{i}"), v)
        })
        .collect();
    let ours: Value = serde_json::from_str(ok(&outcomes[3]).0).unwrap();
    assert_eq!(ours, serde_json::to_value(expected).unwrap());
    assert!(ok(&outcomes[3])
        .0
        .starts_with("{\"k0\":0,\"k1\":1,\"k2\":2,\"k3\":\"again\",\"k4\":4"));
    assert_eq!(ok(&outcomes[4]).0, "{\"a\":{\"x\":1},\"b\":{\"x\":2}}");
}
