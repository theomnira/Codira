//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Typed decoding and encoding with `std.json.json`, through the real
//!   `codira` binary: a program decodes records into its own structs with
//!   `JsonReader`'s `begin_object`/`next_key`/`read_*` methods (no tree)
//!   and encodes them back with `JsonWriter`.
//! - Round trips against `serde_json`: seeded random records, with unknown
//!   members, `null` optionals and members in random order, decode and
//!   encode to the same value `serde_json` holds (unknown members dropped).
//! - Exact error locations: a value of the wrong kind, a number out of
//!   range, a missing member and a syntax error injected at a random place
//!   of a random record are reported with the JSONPath of the value and the
//!   byte offset (and line and column) the test computed while writing the
//!   text.
//! - The streaming writer refuses misplaced calls (a value where a key
//!   must come, a key outside an object, a mismatched end, a second
//!   document, a duplicate key, too deep a nesting, an unfinished document)
//!   with the call's position and the bytes written before it, and writes
//!   through `std.fs` to a file in 64 KiB pieces.

use std::{fmt::Write as _, path::Path, process::Command};

use serde_json::{json, Value};

/// The Codira side: the record types, their decoders and encoders.
const TYPES: &str = r#"
import std.json.json.{Json, JsonReader, JsonWriter, DecodeError, DecodeErrorKind, JsonErrorKind}
import std.result.result.{Result}

struct User {
    id: i64,
    name: String,
    verified: bool,
    location: String?,
    followers: i64,
}

struct Status {
    id: i64,
    text: String,
    user: User,
    retweets: i64,
    score: f64,
    tags: [String],
    reply_to: i64?,
    // 0..=255: decoded with `read_int_in`.
    level: i64,
}

func decode_user(r: JsonReader) -> Result[User, DecodeError] {
    var id: i64? = nil
    var name: String? = nil
    var verified = false
    var location: String? = nil
    var followers: i64 = 0
    r.begin_object()?
    while let key = r.next_key()? {
        if key == "id" {
            id = r.read_i64()?
        } else if key == "name" {
            name = r.read_string()?
        } else if key == "verified" {
            verified = r.read_bool()?
        } else if key == "location" {
            location = if r.read_if_null()? { nil } else { r.read_string()? }
        } else if key == "followers" {
            followers = r.read_i64()?
        } else {
            r.skip()?
        }
    }
    if id == nil { return Result.Err(r.missing_field("id")) }
    if name == nil { return Result.Err(r.missing_field("name")) }
    Result.Ok(User { id: id!, name: name!, verified: verified, location: location, followers: followers })
}

func decode_status(r: JsonReader) -> Result[Status, DecodeError] {
    var id: i64? = nil
    var text: String? = nil
    var user: User? = nil
    var retweets: i64 = 0
    var score = 0.0
    let tags: [String] = []
    var reply_to: i64? = nil
    var level: i64 = 0
    r.begin_object()?
    while let key = r.next_key()? {
        if key == "id" {
            id = r.read_i64()?
        } else if key == "text" {
            text = r.read_string()?
        } else if key == "user" {
            user = decode_user(r)?
        } else if key == "retweets" {
            retweets = r.read_i64()?
        } else if key == "score" {
            score = r.read_f64()?
        } else if key == "tags" {
            r.begin_array()?
            while r.next_element()? {
                tags.push(r.read_string()?)
            }
        } else if key == "reply_to" {
            reply_to = if r.read_if_null()? { nil } else { r.read_i64()? }
        } else if key == "level" {
            level = r.read_int_in(0, 255)?
        } else {
            r.skip()?
        }
    }
    if id == nil { return Result.Err(r.missing_field("id")) }
    if text == nil { return Result.Err(r.missing_field("text")) }
    if user == nil { return Result.Err(r.missing_field("user")) }
    Result.Ok(Status { id: id!, text: text!, user: user!, retweets: retweets, score: score, tags: tags, reply_to: reply_to, level: level })
}

// `[Status, ...]`, then the end of the document.
func decode_all(r: JsonReader) -> Result[[Status], DecodeError] {
    let out: [Status] = []
    r.begin_array()?
    while r.next_element()? {
        out.push(decode_status(r)?)
    }
    r.finish()?
    Result.Ok(out)
}

func encode_user(w: JsonWriter, u: User) {
    w.begin_object()
    w.key("id")
    w.int(u.id)
    w.key("name")
    w.string(u.name)
    w.key("verified")
    w.bool(u.verified)
    w.key("location")
    if let place = u.location { w.string(place) } else { w.null() }
    w.key("followers")
    w.int(u.followers)
    w.end_object()
}

func encode_status(w: JsonWriter, s: Status) {
    w.begin_object()
    w.key("id")
    w.int(s.id)
    w.key("text")
    w.string(s.text)
    w.key("user")
    encode_user(w, s.user)
    w.key("retweets")
    w.int(s.retweets)
    w.key("score")
    w.float(s.score)
    w.key("tags")
    w.begin_array()
    for t in s.tags { w.string(t) }
    w.end_array()
    w.key("reply_to")
    if let n = s.reply_to { w.int(n) } else { w.null() }
    w.key("level")
    w.int(s.level)
    w.end_object()
}

func kind_name(kind: DecodeErrorKind) -> String {
    match kind {
        DecodeErrorKind.Syntax(_) -> "syntax".to_string(),
        DecodeErrorKind.WrongType(expected, found) -> "type:".to_string() + expected + ":" + found,
        DecodeErrorKind.MissingField(name) -> "missing:".to_string() + name,
        DecodeErrorKind.UnknownField(name) -> "unknown:".to_string() + name,
        DecodeErrorKind.OutOfRange -> "range".to_string(),
        DecodeErrorKind.Custom(text) -> "custom:".to_string() + text,
    }
}
"#;

/// Writes a package whose `src/mod.code` is `TYPES` and `main`, runs it in
/// a directory holding `files`, and returns stdout.
fn run(main: &str, files: &[(&str, &[u8])]) -> String {
    let dir = tempfile::Builder::new()
        .prefix("codira_json_typed")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), format!("{TYPES}\n{main}")).unwrap();
    for (name, bytes) in files {
        std::fs::write(dir.path().join(name), bytes).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(["run", "-O", "2", "--color", "disable", "--manifest-path"])
        .arg(dir.path().join("codira.toml"))
        .current_dir(dir.path())
        .env("CODIRA_STD_PATH", std_dir())
        .output()
        .expect("failed to spawn codira");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
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

/// xorshift64*: deterministic, so every run checks the same records.
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

    fn text(&mut self) -> String {
        const PIECES: &[&str] = &["a", "word", " ", "é", "\"", "\\", "\n", "😀", "東京", "\t", "/"];
        (0..self.below(8))
            .map(|_| PIECES[self.below(PIECES.len() as u64) as usize])
            .collect()
    }

    fn user(&mut self) -> Value {
        json!({
            "id": self.next() as i64,
            "name": self.text(),
            "verified": self.chance(20),
            "location": if self.chance(40) { Value::Null } else { Value::from(self.text()) },
            "followers": self.below(1_000_000) as i64,
        })
    }

    fn status(&mut self) -> Value {
        let tags: Vec<Value> = (0..self.below(4)).map(|_| Value::from(self.text())).collect();
        let score = (self.below(100_000) as f64) / 1000.0;
        json!({
            "id": self.next() as i64 >> 1,
            "text": self.text(),
            "user": self.user(),
            "retweets": self.below(5000) as i64,
            "score": score,
            "tags": tags,
            "reply_to": if self.chance(70) { Value::Null } else { Value::from(self.below(1 << 50) as i64) },
            "level": self.below(256) as i64,
        })
    }
}

/// Writes `value` (an object or array of the generator's shapes) as JSON
/// with its members in a shuffled order, sometimes an unknown member, and
/// random white space; returns the text.
fn write_shuffled(rng: &mut Rng, value: &Value, out: &mut String) {
    let space = |rng: &mut Rng, out: &mut String| {
        if rng.chance(20) {
            out.push_str([" ", "\n", "\t", "  "][rng.below(4) as usize]);
        }
    };
    match value {
        Value::Object(map) => {
            let mut members: Vec<(&String, &Value)> = map.iter().collect();
            for i in (1..members.len()).rev() {
                members.swap(i, rng.below(i as u64 + 1) as usize);
            }
            out.push('{');
            let mut first = true;
            let mut extra = rng.chance(30);
            for (key, member) in members {
                if !first {
                    out.push(',');
                }
                first = false;
                space(rng, out);
                if extra && rng.chance(50) {
                    extra = false;
                    out.push_str("\"unknown\":{\"deep\":[1,{\"x\":null}],\"s\":\"\\u00e9\"},");
                    space(rng, out);
                }
                write!(out, "{}", Value::from(key.as_str())).unwrap();
                space(rng, out);
                out.push(':');
                space(rng, out);
                write_shuffled(rng, member, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                space(rng, out);
                write_shuffled(rng, item, out);
            }
            out.push(']');
        }
        other => write!(out, "{other}").unwrap(),
    }
}

const ROUND_TRIP_MAIN: &str = r#"
import std.fs.file.{read_file}
import std.json.json.{default_options}

public func main() {
    let bytes = read_file("records.json").unwrap()
    let r = JsonReader.from_bytes(bytes, default_options()).unwrap()
    match decode_all(r) {
        Result.Ok(all) -> {
            let w = JsonWriter.new()
            w.begin_array()
            for s in all { encode_status(w, s) }
            w.end_array()
            println(w.finish().unwrap())
        },
        Result.Err(e) -> println("error ", e.message()),
    }
}
"#;

#[test]
fn random_records_decode_and_encode_like_serde_json() {
    let mut rng = Rng(0x7e5_7ed_0001);
    let records: Vec<Value> = (0..400).map(|_| rng.status()).collect();
    let mut text = String::new();
    // Every member of the type is generated; unknown ones are added and
    // must be dropped.
    write_shuffled(&mut rng, &Value::Array(records.clone()), &mut text);
    // The text is JSON, and reads as the records (unknown members aside).
    assert!(serde_json::from_str::<Value>(&text).is_ok());
    let stdout = run(ROUND_TRIP_MAIN, &[("records.json", text.as_bytes())]);
    let ours: Value = serde_json::from_str(stdout.trim_end()).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let expected = Value::Array(records);
    assert_eq!(ours, expected);
}

/// One way to break a record, and what the decoder must say.
struct Broken {
    text: String,
    kind: String,
    path: String,
    offset: usize,
}

/// A random record array where one value is replaced by something the
/// decoder must refuse; the offset is recorded while writing.
fn broken(rng: &mut Rng) -> Broken {
    let count = 1 + rng.below(4) as usize;
    let target = rng.below(count as u64) as usize;
    // Which member of the target status, and how it breaks.
    let field = rng.below(7);
    let mut text = String::from("[");
    let mut found = None;
    for i in 0..count {
        if i > 0 {
            text.push_str(", ");
        }
        let status = rng.status();
        let Value::Object(map) = status else { unreachable!() };
        text.push('{');
        let mut first = true;
        for (key, value) in &map {
            let skip = i == target && field == 6 && key == "text";
            if skip {
                continue;
            }
            if !first {
                text.push(',');
            }
            first = false;
            write!(text, "\"{key}\":").unwrap();
            let here = text.len();
            let path = format!("$[{i}].{key}");
            match (i == target, field, key.as_str()) {
                (true, 0, "retweets") => {
                    text.push_str("\"many\"");
                    found = Some(("type:an integer:a string".to_string(), path, here));
                }
                (true, 1, "user") => {
                    // Inside the nested object: `name` is a number.
                    text.push_str("{\"id\":1,\"name\":");
                    let at = text.len();
                    text.push_str("12.5}");
                    found = Some(("type:a string:a float".to_string(), format!("{path}.name"), at));
                }
                (true, 2, "tags") => {
                    text.push_str("[\"a\",\"b\",");
                    let at = text.len();
                    text.push_str("true]");
                    found = Some(("type:a string:a boolean".to_string(), format!("{path}[2]"), at));
                }
                (true, 3, "level") => {
                    text.push_str("256");
                    found = Some(("range".to_string(), path, here));
                }
                (true, 4, "score") => {
                    text.push_str("[1]");
                    found = Some(("type:a number:an array".to_string(), path, here));
                }
                (true, 5, "id") => {
                    // A syntax error: a lone surrogate inside a string,
                    // where the parser reports it (the backslash).
                    text.push_str("\"x\\ud800\"");
                    found = Some(("syntax".to_string(), path, here + 2));
                }
                _ => write!(text, "{value}").unwrap(),
            }
        }
        text.push('}');
        if i == target && field == 6 {
            // `text` is missing: reported at the object's closing brace.
            let at = text.len() - 1;
            found = Some(("missing:text".to_string(), format!("$[{i}]"), at));
        }
    }
    text.push(']');
    let (kind, path, offset) = found.expect("the generator broke the target record");
    Broken { text, kind, path, offset }
}

#[test]
fn decoding_errors_name_the_path_and_the_byte_offset() {
    let mut rng = Rng(0x7e5_7ed_0002);
    let cases: Vec<Broken> = (0..120).map(|_| broken(&mut rng)).collect();
    // One case per line (the texts have no raw line breaks).
    let mut all = String::new();
    for case in &cases {
        assert!(!case.text.contains('\n'));
        all.push_str(&case.text);
        all.push('\n');
    }
    let main = format!(
        r#"
import std.fs.file.{{read_file}}
import std.json.json.{{default_options}}

public func main() {{
    let all = string_from_utf8(read_file("cases.txt").unwrap())!
    for text in all.lines() {{
        let r = JsonReader.with_options(text, default_options())
        match decode_all(r) {{
            Result.Ok(_) -> println("decoded"),
            Result.Err(e) -> {{
                let path = e.path
                let offset = e.offset
                let line = e.line
                let column = e.column
                println(kind_name(e.kind), " ", path, " ", offset, " ", line, " ", column)
            }},
        }}
    }}
}}
"#
    );
    let stdout = run(&main, &[("cases.txt", all.as_bytes())]);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), cases.len());
    for (case, line) in cases.iter().zip(lines) {
        // Line and column: the records are on one line, the column counts
        // characters.
        let column = case.text[..case.offset].chars().count() + 1;
        let expected = format!("{} {} {} 1 {}", case.kind, case.path, case.offset, column);
        assert_eq!(line, expected, "in {}", case.text);
    }
}

#[test]
fn error_messages_read_well() {
    let stdout = run(
        r#"
import std.json.json.{default_options}

public func main() {
    let r = JsonReader.new("[{\"id\": 1, \"text\": \"t\", \"user\": {\"id\": 2, \"name\": false}}]")
    match decode_all(r) {
        Result.Ok(_) -> println("decoded"),
        Result.Err(e) -> println(e.message()),
    }
    let s = JsonReader.new("[{\"id\": 1}]")
    match decode_all(s) {
        Result.Ok(_) -> println("decoded"),
        Result.Err(e) -> println(e.message()),
    }
    let t = JsonReader.new("[]  x")
    match decode_all(t) {
        Result.Ok(_) -> println("decoded"),
        Result.Err(e) -> println(e.message()),
    }
    let u = JsonReader.new("{\"we ird\": [0, {\"\": 1}]}")
    u.begin_object().unwrap()
    u.next_key().unwrap()
    u.begin_array().unwrap()
    u.read_i64().unwrap()
    u.begin_object().unwrap()
    u.next_key().unwrap()
    println(u.path(), " ", u.read_string().err()!.message())
}
"#,
        &[],
    );
    assert_eq!(
        stdout,
        "expected a string, found a boolean at $[0].user.name, line 1, column 51 (byte 50)\n\
         missing field `text` at $[0], line 1, column 10 (byte 9)\n\
         trailing characters after the value at $, line 1, column 5 (byte 4)\n\
         $[\"we ird\"][1][\"\"] expected a string, found an integer at $[\"we ird\"][1][\"\"], line 1, column 21 (byte 20)\n"
    );
}

#[test]
fn the_writer_refuses_misplaced_calls() {
    let stdout = run(
        r#"
import std.json.json.{JsonWriteError, JsonWriteErrorKind}

func show(w: JsonWriter) {
    match w.finish() {
        Result.Ok(text) -> println("ok ", text),
        Result.Err(e) -> println(e.message()),
    }
}

public func main() {
    let a = JsonWriter.new()
    a.begin_object()
    a.int(1)
    a.end_object()
    show(a)
    let b = JsonWriter.new()
    b.key("k")
    show(b)
    let c = JsonWriter.new()
    c.begin_array()
    c.end_object()
    show(c)
    let d = JsonWriter.new()
    d.int(1)
    d.int(2)
    show(d)
    let e = JsonWriter.new()
    e.begin_object()
    e.key("k")
    e.int(1)
    e.key("k")
    show(e)
    let f = JsonWriter.new()
    f.set_max_depth(3)
    f.begin_array()
    f.begin_array()
    f.begin_array()
    f.begin_array()
    show(f)
    let g = JsonWriter.new()
    g.begin_array()
    g.int(1)
    show(g)
    let h = JsonWriter.new()
    show(h)
    let i = JsonWriter.new()
    i.begin_object()
    i.key("a")
    i.key("b")
    show(i)
    let j = JsonWriter.new()
    j.begin_object()
    j.key("a")
    j.end_object()
    show(j)
    // The writer is reusable after `finish`, even after an error.
    j.begin_array()
    j.float(0.1)
    j.float(1.0 / 0.0)
    j.string("\x01")
    j.end_array()
    show(j)
    // Keys of sibling objects are independent; nested objects' too.
    let k = JsonWriter.new()
    k.begin_array()
    k.begin_object()
    k.key("x")
    k.begin_object()
    k.key("x")
    k.int(1)
    k.end_object()
    k.end_object()
    k.begin_object()
    k.key("x")
    k.null()
    k.end_object()
    k.end_array()
    show(k)
}
"#,
        &[],
    );
    assert_eq!(
        stdout,
        "a value where an object key must come (call 1, byte 1)\n\
         a key outside an object or after another key (call 0, byte 0)\n\
         an end that does not close the open array or object (call 1, byte 1)\n\
         a second value after the document's one value (call 1, byte 1)\n\
         duplicate key in an object (call 3, byte 6)\n\
         arrays and objects nested deeper than the limit (call 3, byte 3)\n\
         the document is not complete (call 2, byte 2)\n\
         the document is not complete (call 0, byte 0)\n\
         a key outside an object or after another key (call 2, byte 5)\n\
         an end that does not close the open array or object (call 2, byte 5)\n\
         ok [0.1,null,\"\\u0001\"]\n\
         ok [{\"x\":{\"x\":1}},{\"x\":null}]\n"
    );
}

#[test]
fn the_writer_streams_to_a_file() {
    let stdout = run(
        r#"
import std.fs.fs.{fs_open, fs_close, FS_WRITE, FS_CREATE, FS_TRUNCATE}
import std.fs.file.{read_file}
import std.json.json.{parse, to_string}

public func main() {
    let fd = fs_open("out.json", FS_WRITE() | FS_CREATE() | FS_TRUNCATE())
    let w = JsonWriter.to_file(fd)
    w.begin_array()
    var i: i64 = 0
    while i < 30000 {
        w.begin_object()
        w.key("i")
        w.int(i)
        w.key("s")
        w.string("é\"x")
        w.end_object()
        i += 1
    }
    w.end_array()
    let total = w.finish_file().unwrap()
    fs_close(fd)
    let bytes = read_file("out.json").unwrap()
    let back = parse(string_from_utf8(bytes)!).unwrap()
    println(total, " ", bytes.len(), " ", back.len(), " ", to_string(back.at(29999)!))
}
"#,
        &[],
    );
    let one = |i: usize| format!("{{\"i\":{i},\"s\":\"é\\\"x\"}}").len();
    let total: usize = 2 + (0..30000).map(one).sum::<usize>() + 29999;
    assert_eq!(
        stdout,
        format!("{total} {total} 30000 {{\"i\":29999,\"s\":\"é\\\"x\"}}\n")
    );
}
