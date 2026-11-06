//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - What postfix `?` compiles to, read off the optimized LLVM IR: the
//!   success path straight-line, the error path weighted cold
//!   (`branch_weights`), and -- when the fallible function is inlined, as
//!   std.text.ascii's `@inline` `next_word` and `parse_i64` are into the
//!   module that calls them -- no `Result` left at all: no call through the
//!   dispatch table, no `Result` in memory, no allocation.
//! - The same programs run, with errors passed on correctly through the
//!   inlined functions.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn std_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../std")
        .canonicalize()
        .unwrap()
}

fn package_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_error_propagation")
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

fn codira(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .env("CODIRA_STD_PATH", std_dir())
        .output()
        .expect("failed to spawn codira")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// The `-O 2` IR of `src/mod.code`, and the program's output.
fn ir_and_output(files: &[(&str, &str)]) -> (String, String) {
    let dir = package_with(files);
    let output = codira(dir.path(), &["build", "--emit-ir", "-O", "2"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let ir = std::fs::read_to_string(dir.path().join("target/mod.ll")).unwrap();
    let run = codira(dir.path(), &["run"]);
    assert!(
        run.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        text(&run.stdout),
        text(&run.stderr)
    );
    (ir, text(&run.stdout))
}

/// The text of the IR function definition whose name contains `name`.
fn function_ir<'a>(ir: &'a str, name: &str) -> &'a str {
    let start = ir
        .match_indices("define ")
        .map(|(start, _)| start)
        .find(|&start| {
            ir[start..]
                .lines()
                .next()
                .is_some_and(|line| line.contains(&format!("@{name}(")))
        })
        .unwrap_or_else(|| panic!("no `{name}` in:\n{ir}"));
    let end = ir[start..].find("\n}").map_or(ir.len(), |e| start + e + 2);
    &ir[start..end]
}

/// The functions loaded from the dispatch table in `body`: every
/// `%<name>_ptr = load ptr, ptr getelementptr .. @dispatchTable`.
fn table_calls(body: &str) -> Vec<String> {
    body.lines()
        .filter(|line| line.contains("@dispatchTable"))
        .filter_map(|line| {
            let name = line.trim().strip_prefix('%')?;
            let name = &name[..name.find(" =")?];
            Some(
                name.trim_end_matches(|c: char| c == '.' || c.is_ascii_digit())
                    .to_string(),
            )
        })
        .collect()
}

const SUM_WORDS: &str = r#"
import std.result.result.{Result}
import std.text.ascii.{next_word, parse_i64, ParseError}

enum Problem { Parse(ParseError), Negative(usize) }

// examples/results' loop: `?` on std.text.ascii's `parse_i64`, wrapping
// its error in `Problem.Parse`.
public func sum_words(bytes: [u8]) -> Result[i64, Problem] {
    var total: i64 = 0
    var at: usize = 0
    while let (start, end) = next_word(bytes, at) {
        total += parse_i64(bytes, start, end)?
        at = end
    }
    Result.Ok(total)
}

// A local fallible step and `?` on it, three deep.
func check(x: i64, i: usize) -> Result[i64, usize] {
    if x < 0 { return Result.Err(i) }
    Result.Ok(x)
}

func scaled(x: i64, i: usize) -> Result[i64, usize] {
    Result.Ok(check(x, i)? * 3)
}

public func total(xs: [i64]) -> Result[i64, Problem] {
    var sum: i64 = 0
    var i: usize = 0
    while i < xs.len() {
        sum += scaled(xs[i], i)?
        i += 1
    }
    Result.Ok(sum)
}

func show(r: Result[i64, Problem]) {
    match r {
        Result.Ok(v) -> println(v),
        Result.Err(Problem.Parse(ParseError.InvalidDigit(at))) -> println("digit ", at),
        Result.Err(Problem.Parse(_)) -> println("parse"),
        Result.Err(Problem.Negative(at)) -> println("negative ", at),
    }
}

public func main() {
    show(sum_words("12 -7  100 865".to_string().bytes()))
    show(sum_words("1 2 x3".to_string().bytes()))
    show(sum_words("99999999999999999999".to_string().bytes()))
    show(total([1, 2, 3]))
    show(total([1, -2, 3]))
}
"#;

/// `?` on a call of std.text.ascii's `parse_i64` in a loop over
/// `next_word`: both are `@inline`, so the module compiles them in, and
/// the `Result` folds into their branches -- no call through the dispatch
/// table, no `Result` in memory (`alloca`), no allocation: the `?` is no
/// longer a branch of its own. The same for `?` on a local function three
/// calls deep.
#[test]
fn question_mark_on_an_inlined_callee_leaves_no_result_behind() {
    let (ir, stdout) = ir_and_output(&[("mod.code", SUM_WORDS)]);
    assert_eq!(stdout, "970\ndigit 4\nparse\n18\nnegative 1\n");

    for name in ["sum_words", "total"] {
        let body = function_ir(&ir, name);
        // Only the panics of the bounds checks are calls into the runtime.
        let calls: Vec<String> = table_calls(body)
            .into_iter()
            .filter(|name| !name.starts_with("codira.panic"))
            .collect();
        assert!(calls.is_empty(), "`{name}` calls {calls:?}:\n{body}");
        assert!(
            !keeps_in_memory(body),
            "`{name}` keeps a value in memory:\n{body}"
        );
        // The function's own `Result[i64, Problem]` is its return value;
        // the callees' `Result`s are gone.
        for callee in [
            "Result[i64, ascii::text::std::ParseError]",
            "Result[i64, usize]",
        ] {
            assert!(
                !body.contains(callee),
                "`{name}` builds a `{callee}`:\n{body}"
            );
        }
    }
}

/// When the fallible function is not inlined -- a public function of
/// another module, called through the dispatch table -- `?` is a test of
/// the returned tag weighted towards `Ok`: the error path is out of line
/// (the branch carries `branch_weights`), and building the caller's
/// `Err` calls nothing.
#[test]
fn question_mark_on_a_call_weights_the_error_path_cold() {
    let (ir, stdout) = ir_and_output(&[
        (
            "mod.code",
            r#"
import std.result.result.{Result}
import root.steps.{step}

enum Problem { Step(i64), Other }

public func run(n: i64) -> Result[i64, Problem] {
    var sum: i64 = 0
    var i: i64 = 0
    while i < n {
        sum += step(i)?
        i += 1
    }
    Result.Ok(sum)
}

public func main() {
    match run(10) {
        Result.Ok(v) -> println(v),
        Result.Err(Problem.Step(at)) -> println("step ", at),
        Result.Err(_) -> println("other"),
    }
    match run(20) {
        Result.Ok(v) -> println(v),
        Result.Err(Problem.Step(at)) -> println("step ", at),
        Result.Err(_) -> println("other"),
    }
}
"#,
        ),
        (
            "steps.code",
            r#"
import std.result.result.{Result}

public func step(i: i64) -> Result[i64, i64] {
    if i == 15 { return Result.Err(i) }
    Result.Ok(i * 2)
}
"#,
        ),
    ]);
    assert_eq!(stdout, "90\nstep 15\n");
    let body = function_ir(&ir, "run");
    assert_eq!(table_calls(body), vec!["step_ptr".to_string()], "{body}");
    // The branch on the returned tag is the one with weights.
    let weighted = body
        .lines()
        .filter(|line| line.trim_start().starts_with("br i1") && line.contains("!prof"))
        .count();
    assert!(weighted >= 1, "no weighted branch in:\n{body}");
    // A `Result[i64, i64]` is two words: returned in registers, not
    // through memory.
    assert!(!keeps_in_memory(body), "{body}");
}

/// Whether `body` has a stack slot other than the frame record the
/// collector's root pass adds (`%gc.frame`, which holds no `Result`).
fn keeps_in_memory(body: &str) -> bool {
    body.lines()
        .any(|line| line.contains("alloca") && !line.contains("%gc.frame"))
}
