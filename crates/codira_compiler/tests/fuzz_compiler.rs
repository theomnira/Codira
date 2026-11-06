//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The compiler fuzzing gate, on stable Rust: a seeded mutation fuzzer
//!   over a corpus of real Codira sources (`std/`, `examples/`, and the
//!   seeds below). Every mutated program goes through the parser and the
//!   type checker; those that type-check also go through code generation.
//!   The compiler may reject a program with diagnostics, but must never
//!   panic. A panic is reported with the seed, the case number and the
//!   program, written to `CARGO_TARGET_TMPDIR/fuzz/`.
//!
//! # Mutations
//!
//! Deleting, duplicating and moving spans; inserting tokens (keywords,
//! operators, brackets, extreme literals); replacing identifiers with
//! others from the corpus; swapping lines; truncating; and replacing
//! numeric literals with boundary values. Most mutants are syntax errors,
//! which exercises error recovery; the rest stress name resolution, type
//! inference and the checks behind them.
//!
//! # Run length
//!
//! `CODIRA_FUZZ_SECS` runs for that many seconds and `CODIRA_FUZZ_SEED`
//! sets the seed (printed). Without them a plain `cargo test` runs 250
//! cases from a fixed seed, the same on every machine. The reduced crashes
//! found so far are regression tests in `codira_hir`'s `fuzz_regressions`
//! (and next to the fix where it is elsewhere).
//!
//! # What it cannot catch
//!
//! Wrong code that compiles (that is the differential tests' job), hangs
//! shorter than the watchdog's 60 seconds, and crashes that need program
//! shapes the mutations do not reach from the corpus. Deep nesting that
//! overflows the stack ends the process instead of panicking; it is tested
//! in a child process by `deep_nesting_does_not_overflow_the_stack`.
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use codira_compiler::{Config, Driver, PathOrInline, RelativePathBuf};

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

const TOKENS: &[&str] = &[
    "{",
    "}",
    "(",
    ")",
    "[",
    "]",
    "<",
    ">",
    ",",
    ";",
    ":",
    "::",
    ".",
    "..",
    "..=",
    "=",
    "==",
    "!=",
    "+",
    "-",
    "*",
    "/",
    "%",
    "&&",
    "||",
    "!",
    "?",
    "??",
    "->",
    "=>",
    "<-",
    "&",
    "|",
    "^",
    "<<",
    ">>",
    "+=",
    "@",
    "#",
    "$",
    "'",
    "\"",
    "\\",
    "func",
    "let",
    "var",
    "if",
    "else",
    "while",
    "for",
    "in",
    "loop",
    "match",
    "return",
    "break",
    "continue",
    "class",
    "struct",
    "enum",
    "type",
    "extend",
    "trait",
    "public",
    "import",
    "scope",
    "spawn",
    "channel",
    "effect",
    "handle",
    "perform",
    "resume",
    "uses",
    "nil",
    "true",
    "false",
    "self",
    "Self",
    "requires",
    "ensures",
    "secret",
    "extern",
    "as",
    "i8",
    "i32",
    "i64",
    "u8",
    "u64",
    "i128",
    "f32",
    "f64",
    "bool",
    "str",
    "String",
    "[i64]",
    "i64?",
    "Result[i64, str]",
    "Map[str, i64]",
    "@noalloc",
    "@nopanic",
    "@realtime",
    "@asm",
    "@target(gpu)",
    "\n",
    " ",
    "\t",
    "é",
    "🦀",
    "\u{0}",
    "/*",
    "*/",
    "//",
    "\"\\u{110000}\"",
    "'\\x'",
];

const NUMBERS: &[&str] = &[
    "0",
    "-1",
    "1",
    "9223372036854775807",
    "-9223372036854775808",
    "9223372036854775808",
    "18446744073709551616",
    "340282366920938463463374607431768211455",
    "340282366920938463463374607431768211456",
    "0x",
    "0xFFFFFFFFFFFFFFFFFFFF",
    "0b2",
    "1e309",
    "1e-400",
    "0.0",
    "1_000",
    "1__",
    "255u8",
    "256u8",
    "-129i8",
    "1.5e",
    "00000000000000000000000000000001",
];

/// Small programs that type-check, so that mutants of them often do too
/// and reach code generation.
const SEEDS: &[&str] = &[
    r#"
public class Node { value: i64, next: [Node] }
public func sum(n: i64) -> i64 {
    let xs: [i64] = []
    var i = 0
    while i < n { xs.push(i * 2); i += 1 }
    var total = 0
    for x in xs { total += x }
    let nodes: [Node] = []
    nodes.push(Node { value: total, next: [] })
    total + nodes[0].value + xs.len() as i64
}
"#,
    r#"
public func strings(n: i64) -> i64 {
    let s = "a" + n.to_string() + "é"
    let parts = s.split("a")
    if s == "x" { 1 } else { s.len() as i64 + parts.len() as i64 }
}
"#,
    r#"
enum Shape { Circle(f64), Square(f64), Empty }
func area(s: Shape) -> f64 {
    match s {
        Shape::Circle(r) => 3.0 * r * r,
        Shape::Square(a) => a * a,
        Shape::Empty => 0.0,
    }
}
public func main() -> f64 { area(Shape::Circle(2.0)) + area(Shape::Square(3.0)) }
"#,
    r#"
func find(items: [i32], target: i32) -> i32? {
    for item in items { if item == target { return item } }
    nil
}
public func main() -> i32 {
    let xs = [3, 1, 4]
    if let v = find(xs, 4) { v } else { 0 - 1 }
}
"#,
    r#"
func adder(a: i64) -> func(i64) -> i64 { func(b: i64) -> i64 { a + b } }
public func main(x: i64) -> i64 {
    let f = adder(x)
    let ys = [1, 2, 3].map(func(v) { v * 3 })
    f(1) + ys.fold(0, func(acc, v) { acc + v })
}
"#,
    r#"
func square(jobs: Channel[i32], results: Channel[i32]) {
    let x = <-jobs
    results <- x * x
}
public func sum_of_squares(n: i32) -> i32 {
    scope {
        let jobs = channel(64)
        let results = channel(64)
        var i = 0
        while i < n { spawn square(jobs, results); i += 1 }
        i = 0
        while i < n { jobs <- i; i += 1 }
        var sum = 0
        i = 0
        while i < n { sum += <-results; i += 1 }
        sum
    }
}
"#,
    r#"
struct Point { x: i64, y: i64 }
extend Point {
    func len2(self) -> i64 { self.x * self.x + self.y * self.y }
}
func safe_div(a: i64, b: i64) -> Result[i64, str] {
    if b == 0 { Result::Err("zero") } else { Result::Ok(a / b) }
}
public func main(a: i64) -> i64 {
    let p = Point { x: a, y: 2 }
    match safe_div(p.len2(), a) { Result::Ok(v) => v, Result::Err(_) => 0 }
}
"#,
    r#"
func id[T](x: T) -> T { x }
@noalloc @nopanic
func add(a: i64, b: i64) -> i64 { a + b }
public func main(a: i64) -> i64 requires a > 0 { id(add(a, 1)) / a }
"#,
];

fn corpus() -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["std", "examples"] {
        collect_code(&root.join(dir), &mut files);
    }
    files.sort();
    let mut corpus: Vec<String> = SEEDS.iter().map(|s| (*s).to_owned()).collect();
    for file in files {
        if let Ok(text) = std::fs::read_to_string(&file) {
            // Very large files make each case slow without adding shapes.
            if text.len() <= 24 * 1024 {
                corpus.push(text);
            }
        }
    }
    corpus
}

fn collect_code(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_code(&path, out);
        } else if path.extension().is_some_and(|e| e == "code") {
            out.push(path);
        }
    }
}

/// Identifier-like words of the corpus, for replacements.
fn words(corpus: &[String]) -> Vec<String> {
    let mut words: Vec<String> = corpus
        .iter()
        .flat_map(|text| {
            text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .filter(|w| !w.is_empty() && w.len() < 24)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    words.sort();
    words.dedup();
    words
}

/// The char boundary at or before `i`.
fn floor(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn span(rng: &mut Rng, text: &str, max: usize) -> (usize, usize) {
    let start = floor(text, rng.below(text.len() + 1));
    let end = floor(text, start + 1 + rng.below(max));
    (start, end.max(start))
}

/// Applies 1 to 4 random mutations.
fn mutate(rng: &mut Rng, text: &str, words: &[String]) -> String {
    let mut text = text.to_owned();
    for _ in 0..1 + rng.below(4) {
        match rng.below(9) {
            0 => {
                let (a, b) = span(rng, &text, 48);
                text.replace_range(a..b, "");
            }
            1 => {
                let (a, b) = span(rng, &text, 64);
                let piece = text[a..b].to_owned();
                let at = floor(&text, rng.below(text.len() + 1));
                text.insert_str(at, &piece);
            }
            2 | 3 => {
                let at = floor(&text, rng.below(text.len() + 1));
                let token = TOKENS[rng.below(TOKENS.len())];
                text.insert_str(at, token);
            }
            4 => {
                // Replace an identifier-like word.
                let at = floor(&text, rng.below(text.len() + 1));
                let start = text[..at]
                    .char_indices()
                    .rev()
                    .find(|&(_, c)| !(c.is_alphanumeric() || c == '_'))
                    .map_or(0, |(i, c)| i + c.len_utf8());
                let end = text[at..]
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .map_or(text.len(), |i| at + i);
                if start < end && !words.is_empty() {
                    let word = &words[rng.below(words.len())];
                    text.replace_range(start..end, word);
                }
            }
            5 => {
                let mut lines: Vec<&str> = text.lines().collect();
                if lines.len() > 1 {
                    let a = rng.below(lines.len());
                    let b = rng.below(lines.len());
                    lines.swap(a, b);
                }
                text = lines.join("\n");
            }
            6 => {
                let at = floor(&text, rng.below(text.len() + 1));
                text.truncate(at);
            }
            7 => {
                // Replace a numeric literal with a boundary value.
                let digits: Vec<usize> = text
                    .char_indices()
                    .filter(|(_, c)| c.is_ascii_digit())
                    .map(|(i, _)| i)
                    .collect();
                if !digits.is_empty() {
                    let start = digits[rng.below(digits.len())];
                    let end = text[start..]
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
                        .map_or(text.len(), |i| start + i);
                    text.replace_range(start..end, NUMBERS[rng.below(NUMBERS.len())]);
                }
            }
            _ => {
                // Move a span.
                let (a, b) = span(rng, &text, 80);
                let piece: String = text[a..b].to_owned();
                text.replace_range(a..b, "");
                let at = floor(&text, rng.below(text.len() + 1));
                text.insert_str(at, &piece);
            }
        }
    }
    text
}

/// What compiling one program did.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Rejected,
    Compiled,
}

/// A compiler that keeps one driver (and its database) between programs,
/// recreating it after a panic.
struct Compiler {
    out_dir: tempfile::TempDir,
    driver: Option<Driver>,
}

impl Compiler {
    fn new() -> Self {
        Self {
            out_dir: tempfile::TempDir::new().unwrap(),
            driver: None,
        }
    }

    fn driver(&mut self, text: &str) -> &mut Driver {
        if self.driver.is_none() {
            let config = Config {
                out_dir: Some(self.out_dir.path().to_path_buf()),
                ..Config::default()
            };
            let input = PathOrInline::Inline {
                rel_path: RelativePathBuf::from("mod.code"),
                contents: text.to_owned(),
            };
            let (driver, _) = Driver::with_file(config, input).expect("driver");
            self.driver = Some(driver);
        } else {
            self.driver
                .as_mut()
                .unwrap()
                .set_file_text("mod.code", text)
                .unwrap();
        }
        self.driver.as_mut().unwrap()
    }

    /// Compiles `text`: diagnostics, and code generation when there are no
    /// errors. Returns the panic message if the compiler panicked.
    fn compile(&mut self, text: &str, codegen: bool) -> Result<Outcome, String> {
        IN_COMPILE.set(true);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let driver = self.driver(text);
            // Warnings count too: only programs without any diagnostic
            // reach code generation.
            let has_errors = driver
                .collect_diagnostics()
                .iter()
                .filter(|file| file.relative_path == "mod.code")
                .any(|file| !file.diagnostics.is_empty());
            if has_errors {
                Outcome::Rejected
            } else {
                if codegen {
                    // A refusal to generate code is an error, not a crash.
                    let _ = driver.write_all_assemblies(false);
                }
                Outcome::Compiled
            }
        }));
        IN_COMPILE.set(false);
        result.map_err(|payload| {
            self.driver = None;
            LAST_PANIC
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| panic_message(&payload))
        })
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic>".to_owned())
}

/// The message and location of the last panic, captured by the hook.
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

thread_local! {
    /// Set while the compiler runs on the fuzzing thread: its panics are
    /// findings, reported with the program; any other panic is printed.
    static IN_COMPILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn install_quiet_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        if IN_COMPILE.get() {
            *LAST_PANIC.lock().unwrap() = Some(format!("{message} at {location}"));
        } else {
            eprintln!("panicked at {location}: {message}");
        }
    }));
}

/// How long a run goes on: `CODIRA_FUZZ_SECS` seconds when set (the gate
/// runner), otherwise a fixed number of cases, so a plain `cargo test` is
/// the same run on every machine.
enum Length {
    Time(Instant),
    Cases(u64),
}

impl Length {
    fn from_env() -> Self {
        match std::env::var("CODIRA_FUZZ_SECS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
        {
            Some(secs) => Length::Time(Instant::now() + Duration::from_secs_f64(secs)),
            None => Length::Cases(DEFAULT_CASES),
        }
    }

    fn goes_on(&self, done: u64) -> bool {
        match self {
            Length::Time(deadline) => Instant::now() < *deadline,
            Length::Cases(cases) => done < *cases,
        }
    }
}

/// The cases a plain `cargo test` runs.
const DEFAULT_CASES: u64 = 250;

/// `CODIRA_FUZZ_SEED`, or a fixed seed: a plain `cargo test` must not find
/// a new crash on someone else's change. The gate runner draws fresh ones.
fn seed() -> u64 {
    std::env::var("CODIRA_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0xC0D1_F022)
}

/// Saves a crashing program and returns where.
fn save_crash(seed: u64, case: u64, text: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fuzz");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("crash-{seed:x}-{case}.code"));
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn mutated_programs_never_crash_the_compiler() {
    let corpus = corpus();
    let words = words(&corpus);
    let seed = seed();
    let length = Length::from_env();
    let case = Arc::new(AtomicU64::new(0));

    // A watchdog: a single case that takes a minute is a hang.
    let watched = case.clone();
    std::thread::spawn(move || {
        let mut last = u64::MAX;
        loop {
            std::thread::sleep(Duration::from_secs(60));
            let now = watched.load(Ordering::SeqCst);
            if now == last {
                eprintln!("fuzz: case {now} of seed {seed:#x} hung for a minute");
                std::process::abort();
            }
            last = now;
        }
    });

    install_quiet_panic_hook();
    let mut compiler = Compiler::new();
    let mut rng = Rng(seed);
    let mut crashes = Vec::new();
    let (mut compiled, mut rejected) = (0u64, 0u64);
    while length.goes_on(case.load(Ordering::SeqCst)) {
        let n = case.fetch_add(1, Ordering::SeqCst);
        // Mostly the small seeds, which reach the type checker and code
        // generation more often.
        let base = if rng.below(2) == 0 {
            &corpus[rng.below(SEEDS.len())]
        } else {
            &corpus[rng.below(corpus.len())]
        };
        let program = mutate(&mut rng, base, &words);
        match compiler.compile(&program, true) {
            Ok(Outcome::Compiled) => compiled += 1,
            Ok(Outcome::Rejected) => rejected += 1,
            Err(message) => {
                let path = save_crash(seed, n, &program);
                crashes.push(format!(
                    "case {n}: {message}\n  program: {}",
                    path.display()
                ));
            }
        }
    }
    let _ = std::panic::take_hook();
    eprintln!(
        "fuzz: seed {seed:#x}, {} cases: {compiled} compiled, {rejected} rejected, {} crashed",
        case.load(Ordering::SeqCst),
        crashes.len()
    );
    assert!(
        crashes.is_empty(),
        "the compiler panicked:\n{}",
        crashes.join("\n")
    );
}

/// The crash site of a panic message (`... at file:line`).
fn crash_site(message: &str) -> &str {
    message.rsplit(" at ").next().unwrap_or(message)
}

/// Reduces the crashing program at `CODIRA_FUZZ_REDUCE` by deleting lines,
/// then shorter and shorter spans of characters, while the compiler still
/// panics at the same place, and writes the result next to it with
/// `.min.code` appended.
#[test]
#[ignore = "a tool: set CODIRA_FUZZ_REDUCE to a crashing program"]
fn reduce_crash() {
    let Some(path) = std::env::var_os("CODIRA_FUZZ_REDUCE") else {
        return;
    };
    let path = PathBuf::from(path);
    let mut text = std::fs::read_to_string(&path).unwrap();
    install_quiet_panic_hook();
    let mut compiler = Compiler::new();
    let site = match compiler.compile(&text, true) {
        Err(message) => crash_site(&message).to_owned(),
        Ok(outcome) => panic!("the program does not crash the compiler ({outcome:?})"),
    };
    let mut still_crashes = |candidate: &str| match compiler.compile(candidate, true) {
        Err(message) => crash_site(&message) == site,
        Ok(_) => false,
    };
    // Whole lines first.
    let mut changed = true;
    while changed {
        changed = false;
        let lines: Vec<&str> = text.lines().collect();
        for i in (0..lines.len()).rev() {
            let candidate: String = lines
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .map(|(_, l)| *l)
                .collect::<Vec<_>>()
                .join("\n");
            if still_crashes(&candidate) {
                text = candidate;
                changed = true;
                break;
            }
        }
    }
    // Then spans of characters, longest first.
    for width in [32usize, 16, 8, 4, 2, 1] {
        let mut start = 0;
        while start < text.len() {
            let a = floor(&text, start);
            let b = floor(&text, a + width);
            if a < b {
                let mut candidate = text.clone();
                candidate.replace_range(a..b, "");
                if still_crashes(&candidate) {
                    text = candidate;
                    continue;
                }
            }
            start = a + text[a..].chars().next().map_or(1, char::len_utf8);
        }
    }
    let out = path.with_extension("min.code");
    std::fs::write(&out, &text).unwrap();
    let _ = std::panic::take_hook();
    eprintln!("reduced to {} bytes at {site}:\n{text}", text.len());
}
