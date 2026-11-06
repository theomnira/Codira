//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of `std.error.error`: the `Error` trait, `AnyError`,
//!   `context` and `with_context`, `?` converting any `Error` through
//!   `AnyError`'s `@error_conversion`, and what `main` does with an `Err`.
//!   Programs are built and run by the real `codira` binary against this
//!   source tree's standard library.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// The standard library of this source tree, named explicitly so that the
/// tests never pick up another checkout's.
fn std_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../std")
        .canonicalize()
        .unwrap()
}

/// Writes a package whose `src/` holds `files` (`(name, source)` pairs).
fn package_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_error_context")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    for (name, source) in files {
        std::fs::write(dir.path().join("src").join(name), source).unwrap();
    }
    dir
}

/// `codira run` of a one-file program: its exit code, stdout and stderr.
fn run(source: &str) -> (Option<i32>, String, String) {
    let dir = package_with(&[("mod.code", source)]);
    let output: Output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("run")
        .arg("--manifest-path")
        .arg(dir.path().join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .env("CODIRA_STD_PATH", std_dir())
        .output()
        .expect("failed to spawn codira");
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    (
        output.status.code(),
        text(&output.stdout),
        text(&output.stderr),
    )
}

/// Runs `source`, asserting success; returns its stdout.
fn run_ok(source: &str) -> String {
    let (code, stdout, stderr) = run(source);
    assert_eq!(code, Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    stdout
}

/// A path that does not exist.
fn missing_path() -> String {
    let dir = tempfile::tempdir().unwrap();
    dir.path()
        .join("no/such/config.toml")
        .to_str()
        .unwrap()
        .to_owned()
}

#[test]
fn a_three_level_context_chain_through_question_mark_and_read_file() {
    let missing = missing_path();
    let program = r#"
import std.error.error.{AnyError}
import std.fs.file.{read_file}
import std.result.result.{Result}

func load(path: str) -> Result[[u8], AnyError] {
    read_file(path).context(f"reading {path}")
}

func configure(path: str) -> Result[i64, AnyError] {
    let bytes = load(path).context("loading the configuration")?
    Result.Ok(bytes.len() as i64)
}

func start(path: str) -> Result[i64, AnyError] {
    let n = configure(path).context("starting the server")?
    Result.Ok(n)
}

public func main() {
    match start("{missing}") {
        Result.Ok(n) -> println(n),
        Result.Err(e) -> {
            println(f"{e}")
            println(e.root_cause())
            println(e.error_type()!)
            for part in e.chain() {
                println("- ", part)
            }
        }
    }
}
"#
    .replace("{missing}", &missing);
    let out = run_ok(&program);
    assert_eq!(
        out,
        format!(
            "starting the server: loading the configuration: reading {missing}: file not \
             found\nfile not found\nFileError\n- starting the server\n- loading the \
             configuration\n- reading {missing}\n- file not found\n"
        )
    );
}

#[test]
fn question_mark_converts_every_error_type_and_passes_any_error_on() {
    let work = tempfile::tempdir().unwrap();
    let bad = work.path().join("bad.txt");
    std::fs::write(&bad, "40 7x 1").unwrap();
    let good = work.path().join("good.txt");
    std::fs::write(&good, "40 -3\n5").unwrap();
    let program = r#"
import std.error.error.{AnyError, Error}
import std.fs.file.{read_file}
import std.text.ascii.{next_word, parse_i64}
import std.result.result.{Result}

struct Limit { max: i64, got: i64 }
extend Limit: Error {
    func message(self) -> String { f"{self.got} is over the limit of {self.max}" }
}

// Wraps another error, which `source` names.
struct Retry { attempts: i64, last: String }
extend Retry: Error {
    func message(self) -> String { f"gave up after {self.attempts} attempts" }
    func source(self) -> String? { self.last }
}

func sum(path: str) -> Result[i64, AnyError] {
    let bytes = read_file(path)?                     // a FileError
    var total: i64 = 0
    var at: usize = 0
    while let (start, end) = next_word(bytes, at) {
        total += parse_i64(bytes, start, end)?       // a ParseError
        at = end
    }
    if total > 100 {
        let over: Result[i64, Limit] = Result.Err(Limit { max: 100, got: total })
        return Result.Ok(over?)                      // a program's own error
    }
    Result.Ok(total)
}

func twice(path: str) -> Result[i64, AnyError] {
    let n = sum(path)?                               // an AnyError, as it is
    Result.Ok(n * 2)
}

func retried() -> Result[i64, AnyError] {
    let r: Result[i64, Retry] = Result.Err(Retry { attempts: 3, last: "timed out" })
    Result.Ok(r?)
}

func report(r: Result[i64, AnyError]) {
    match r {
        Result.Ok(n) -> println(n),
        Result.Err(e) -> println(e.error_type()!, ": ", e.to_string()),
    }
}

public func main() {
    report(twice("{good}"))
    report(twice("{bad}"))
    report(twice("{bad}.missing"))
    report(retried())
    let big = AnyError.new("from a message").context("doing a thing")
    println(big.to_string(), " ", big.error_type() == nil)
}
"#
    .replace("{good}", good.to_str().unwrap())
    .replace("{bad}", bad.to_str().unwrap());
    let out = run_ok(&program);
    assert_eq!(
        out,
        "84\nParseError: invalid digit at byte 4\nFileError: file not found\nRetry: gave up \
         after 3 attempts: timed out\ndoing a thing: from a message true\n"
    );
}

#[test]
fn an_over_limit_error_is_reported_through_its_own_type() {
    let work = tempfile::tempdir().unwrap();
    let big = work.path().join("big.txt");
    std::fs::write(&big, "90 20").unwrap();
    let program = r#"
import std.error.error.{AnyError, Error}
import std.result.result.{Result}

struct Limit { max: i64, got: i64 }
extend Limit: Error {
    func message(self) -> String { f"{self.got} is over the limit of {self.max}" }
}

func check(total: i64) -> Result[i64, AnyError] {
    if total > 100 {
        let over: Result[i64, Limit] = Result.Err(Limit { max: 100, got: total })
        return Result.Ok(over.context("checking the total")?)
    }
    Result.Ok(total)
}

public func main() {
    match check(110) {
        Result.Ok(n) -> println(n),
        Result.Err(e) -> println(e.error_type()!, ": ", e.to_string()),
    }
}
"#;
    let out = run_ok(program);
    assert_eq!(
        out,
        "Limit: checking the total: 110 is over the limit of 100\n"
    );
}

#[test]
fn with_context_computes_the_context_only_for_an_error() {
    let missing = missing_path();
    let program = r#"
import std.error.error.{AnyError}
import std.fs.file.{read_file}
import std.result.result.{Result}

class Calls { n: i64 }

public func main() {
    let calls = Calls { n: 0 }
    let ok: Result[i64, AnyError] = Result.Ok(5)
    let still = ok.with_context(func() -> String { calls.n += 1; "unused".to_string() })
    println(still.unwrap_or(0), " ", calls.n)
    let failed = read_file("{missing}").with_context(func() -> String {
        calls.n += 1
        f"opening attempt {calls.n}"
    })
    match failed {
        Result.Ok(_) -> println("read"),
        Result.Err(e) -> println(e.to_string(), " ", calls.n),
    }
}
"#
    .replace("{missing}", &missing);
    let out = run_ok(&program);
    assert_eq!(out, "5 0\nopening attempt 1: file not found 1\n");
}

#[test]
fn main_returning_an_err_prints_the_chain_and_exits_with_status_1() {
    let missing = missing_path();
    let program = r#"
import std.error.error.{AnyError}
import std.fs.file.{read_file}
import std.result.result.{Result}

public func main() -> Result[(), AnyError] {
    print("before the error")
    let bytes = read_file("{missing}").context("reading the configuration")?
    println(bytes.len())
    Result.Ok(())
}
"#
    .replace("{missing}", &missing);
    let (code, stdout, stderr) = run(&program);
    assert_eq!(code, Some(1), "stdout:\n{stdout}\nstderr:\n{stderr}");
    // What was printed before the error is flushed first.
    assert_eq!(stdout, "before the error");
    assert_eq!(
        stderr, "Error: reading the configuration: file not found\n",
        "{stderr}"
    );
}

#[test]
fn main_returning_ok_returns_its_value_to_the_host() {
    let program = r#"
import std.error.error.{AnyError}
import std.result.result.{Result}

public func main() -> Result[i64, AnyError] {
    Result.Ok(42)
}
"#;
    assert_eq!(run_ok(program), "42\n");
    let program = r#"
import std.error.error.{AnyError}
import std.result.result.{Result}

public func main() -> Result[(), AnyError] {
    println("done")
    Result.Ok(())
}
"#;
    assert_eq!(run_ok(program), "done\n");
}

#[test]
fn main_reports_errors_of_any_type() {
    // A `String` error is written as it is; an enum without `to_string` in
    // its debug form; a number as `to_string` writes it.
    for (error_type, value, expected) in [
        ("String", "\"no luck\".to_string()", "Error: no luck\n"),
        ("Why", "Why.Code(7)", "Error: Why.Code(7)\n"),
        ("i64", "-3", "Error: -3\n"),
    ] {
        let program = format!(
            r#"
import std.result.result.{{Result}}
enum Why {{ Code(i64), Other }}

public func main() -> Result[i64, {error_type}] {{
    Result.Err({value})
}}
"#
        );
        let (code, stdout, stderr) = run(&program);
        assert_eq!(code, Some(1), "stdout:\n{stdout}\nstderr:\n{stderr}");
        assert_eq!(stderr, expected);
    }
}

#[test]
fn a_type_that_is_not_an_error_is_rejected() {
    let (code, _, stderr) = run(r#"
import std.error.error.{AnyError}
import std.result.result.{Result}

struct Oops { code: i64 }

func fails() -> Result[i64, Oops] { Result.Err(Oops { code: 1 }) }

func pass_on() -> Result[i64, AnyError] {
    Result.Ok(fails()?)
}

public func main() {
    let _ = pass_on()
}
"#);
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains(
            "`?` cannot pass this `Oops` error on as a `AnyError`: `AnyError.from_error` takes \
             errors whose type implements `Error`, and `Oops` does not; implement it with \
             `extend Oops: Error { .. }`"
        ),
        "{stderr}"
    );

    let (code, _, stderr) = run(r#"
import std.error.error.{AnyError}
import std.result.result.{Result}

struct Oops { code: i64 }

public func main() {
    let r: Result[i64, Oops] = Result.Err(Oops { code: 1 })
    let _ = r.context("doing a thing")
}
"#);
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains("the type `Oops` does not implement the trait `Error`"),
        "{stderr}"
    );
}

#[test]
fn a_programs_own_error_conversion_is_used_by_question_mark() {
    // The rule is keyed on the attribute: nothing here is the standard
    // library's.
    let out = run_ok(
        r#"
enum Result[T, E] { Ok(T), Err(E) }
trait Describe { func describe(self) -> String; }
struct Report { lines: [String] }
extend Report {
    @error_conversion
    func from[E: Describe](error: E) -> Report { Report { lines: [error.describe()] } }
}
struct Disk { free: i64 }
extend Disk: Describe { func describe(self) -> String { f"only {self.free} bytes free" } }

func save() -> Result[i64, Disk] { Result.Err(Disk { free: 12 }) }
func run() -> Result[i64, Report] { Result.Ok(save()? + 1) }

public func main() {
    match run() {
        Result.Ok(n) -> println(n),
        Result.Err(r) -> println(r.lines[0]),
    }
}
"#,
    );
    assert_eq!(out, "only 12 bytes free\n");
}
