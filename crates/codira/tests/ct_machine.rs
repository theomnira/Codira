//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests of the machine-code constant-time check (`codira build
//!   --check-constant-time`): the constant-time examples pass it at `-O0`,
//!   `-O2` and `-O3`; code the source checker accepts but whose machine code
//!   breaks a rule is caught, with the function and the instruction named;
//!   and the repository's `oblivious` functions are run through it.
//!
//! Most tests build for an x86-64 host, or for x86-64 Linux on a host of
//! another architecture; one cross-compiles for `aarch64-unknown-linux-gnu`
//! and checks the library without running it.

use std::{collections::BTreeSet, ffi::OsString, path::Path};

use codira::{run_with_args, ExitStatus};

/// The constant-time example of `tests/constant_time.rs`.
const EXAMPLES: &str = r#"
oblivious func ct_max(a: u64, b: u64) -> u64 {
    ct_select(a > b, a, b)
}

@constant_time
func ct_equal(a: secret u64, b: secret u64) -> secret bool {
    let diff: secret u64 = a ^ b;
    diff == 0
}

public func max_of(a: u64, b: u64) -> u64 {
    declassify(ct_max(a, b))
}

public func equal(a: u64, b: u64) -> bool {
    declassify(ct_equal(a, b))
}
"#;

fn write_package(dir: &Path, source: &str) {
    std::fs::write(
        dir.join("codira.toml"),
        "[package]\nname=\"ct\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/mod.code"), source).unwrap();
}

fn build(dir: &Path, extra: &[&str]) -> Result<ExitStatus, String> {
    let mut args: Vec<OsString> = vec![
        "codira".into(),
        "build".into(),
        "--manifest-path".into(),
        dir.join("codira.toml").into(),
    ];
    args.extend(extra.iter().map(OsString::from));
    run_with_args(args).map_err(|e| format!("{e:#}"))
}

/// Builds `source` at `-O<opt>` with the check, and returns the names of the
/// functions it reports (empty when the build passes).
fn failing_functions(source: &str, opt: &str) -> (BTreeSet<String>, String) {
    failing_functions_with(source, opt, X86_64)
}

/// The tests are of the x86-64 rules: on a host of another architecture
/// (an AArch64 Mac) they cross-compile for x86-64 Linux, whose library the
/// check reads just the same.
const X86_64: &[&str] = if cfg!(target_arch = "x86_64") {
    &[]
} else {
    &["--target", "x86_64-unknown-linux-gnu"]
};

/// The library a build with the arguments `extra` writes: under the target's
/// own directory when it names one with `--target` and that is where the
/// library is, else `target/mod.codiralib`.
fn library(dir: &Path, extra: &[&str]) -> std::path::PathBuf {
    let triple = extra
        .iter()
        .position(|arg| *arg == "--target")
        .and_then(|at| extra.get(at + 1));
    match triple.map(|triple| dir.join("target").join(triple).join("mod.codiralib")) {
        Some(cross) if cross.is_file() => cross,
        _ => dir.join("target/mod.codiralib"),
    }
}

fn failing_functions_with(source: &str, opt: &str, extra: &[&str]) -> (BTreeSet<String>, String) {
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), source);
    let mut args = vec!["--check-constant-time", "-O", opt];
    args.extend_from_slice(extra);
    match build(project.path(), &args) {
        Ok(status) => {
            assert_eq!(
                status,
                ExitStatus::Success,
                "-O{opt}: the source was rejected"
            );
            assert!(library(project.path(), extra).is_file());
            (BTreeSet::new(), String::new())
        }
        Err(message) => {
            assert!(
                message.contains("breaks the machine-code constant-time rules"),
                "-O{opt}: {message}"
            );
            // A library that fails the check is not left behind.
            assert!(!library(project.path(), extra).exists());
            let names = message
                .lines()
                .filter_map(|line| line.trim().strip_prefix('`'))
                .filter_map(|line| line.split_once('`').map(|(name, _)| name.to_string()))
                .collect();
            (names, message)
        }
    }
}

#[test]
fn the_constant_time_examples_pass_at_every_level() {
    for opt in ["0", "2", "3"] {
        let (failing, message) = failing_functions(EXAMPLES, opt);
        assert!(failing.is_empty(), "-O{opt}:\n{message}");
    }
}

/// `ct_select` on 8-bit integers: one LLVM `select` marked `!unpredictable`
/// with no branch in the IR (what `tests/constant_time.rs` checks), but at
/// `-O0` the x86-64 backend expands it into a conditional jump on the
/// secret condition. The source checker accepts it.
const SCALED: &str = r#"
@constant_time
func scaled(s: secret u64, p: u64, q: u64) -> secret u64 {
    s ^ (p / q)
}
"#;

const SELECT_U8: &str = r#"
oblivious func sel_u8(c: bool, a: u8, b: u8) -> u8 {
    ct_select(c, a, b)
}
"#;

#[test]
fn an_8_bit_ct_select_is_branch_free_at_every_level() {
    // The x86-64 back end has no 8-bit conditional move; codegen widens the
    // operands to 32 bits so that -O0 does not lower the select through a
    // conditional jump on the secret condition (it used to: `jb`/`jl`).
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), SELECT_U8);
    assert_eq!(
        build(project.path(), &["--emit-ir", "-O", "0"]),
        Ok(ExitStatus::Success)
    );
    let ir = std::fs::read_to_string(project.path().join("target/mod.ll")).unwrap();
    let start = ir.find("define i8 @sel_u8").expect("sel_u8 in the IR");
    let body = &ir[start..start + ir[start..].find("\n}").unwrap()];
    assert!(
        body.contains("select i1") && body.contains("i32") && body.contains("!unpredictable"),
        "{body}"
    );
    assert!(!body.contains("br i1"), "{body}");

    for opt in ["0", "2", "3"] {
        let (failing, message) = failing_functions(SELECT_U8, opt);
        assert!(failing.is_empty(), "-O{opt}:\n{message}");
    }
}

#[test]
fn a_call_to_a_function_that_is_not_constant_time_is_caught() {
    // Passing a public value to a function that is not constant-time is
    // allowed by the source rules; the call is still code nobody checked.
    let source = r#"
func helper(p: u64) -> u64 {
    p * 3 + 1
}

@constant_time
func mask(s: secret u64, p: u64) -> secret u64 {
    s ^ helper(p)
}
"#;
    let (failing, message) = failing_functions(source, "0");
    assert_eq!(failing, BTreeSet::from(["mask".to_string()]), "{message}");
    assert!(
        message.contains("call to code that is not checked") && message.contains("`helper`"),
        "{message}"
    );
}

#[test]
fn a_bool_ct_select_reads_no_secret_dependent_address_at_o0() {
    // At -O0 the back end folded a `select` of two loaded locals into a load
    // from a `select` of their stack addresses: an access at an address
    // chosen by the secret. Codegen now freezes the operands, which keeps the
    // select on values (a `cmov` between registers). The rule itself is
    // tested on hand-built machine code in `codira_ctcheck`'s unit tests.
    let source = r#"
oblivious func sel_bool(c: bool, a: bool, b: bool) -> bool {
    ct_select(c, a, b)
}
"#;
    for opt in ["0", "2"] {
        let (failing, message) = failing_functions(source, opt);
        assert!(failing.is_empty(), "-O{opt}:\n{message}");
    }
}

#[test]
fn public_division_is_still_a_variable_time_instruction() {
    // Dividing public values is allowed by the source rules; rule (b)
    // forbids the instruction outright, and says the operands are public.
    let source = r#"
@constant_time
func scaled(s: secret u64, p: u64, q: u64) -> secret u64 {
    s ^ (p / q)
}
"#;
    for opt in ["0", "2"] {
        let (failing, message) = failing_functions(source, opt);
        assert_eq!(failing, BTreeSet::from(["scaled".to_string()]), "{message}");
        assert!(message.contains("variable-time instruction"), "{message}");
        assert!(
            message.contains("the operands were not found to depend on a secret"),
            "{message}"
        );
    }
}

/// Every `oblivious` function of `tests/circuits.rs`'s operator table: each
/// binary operator, comparison, `~`, a `ct_select` and negation per type,
/// and casts between every pair of widths.
fn operator_corpus() -> String {
    const TYPES: [&str; 6] = ["i64", "u64", "i32", "u16", "i8", "u8"];
    const BINARY: [(&str, &str); 8] = [
        ("add", "+"),
        ("sub", "-"),
        ("mul", "*"),
        ("and", "&"),
        ("or", "|"),
        ("xor", "^"),
        ("shl", "<<"),
        ("shr", ">>"),
    ];
    const COMPARE: [(&str, &str); 6] = [
        ("eq", "=="),
        ("ne", "!="),
        ("lt", "<"),
        ("le", "<="),
        ("gt", ">"),
        ("ge", ">="),
    ];
    let mut src = String::new();
    for ty in TYPES {
        for (name, op) in BINARY {
            src.push_str(&format!(
                "public oblivious func {name}_{ty}(a: {ty}, b: {ty}) -> {ty} {{ a {op} b }}\n"
            ));
        }
        for (name, op) in COMPARE {
            src.push_str(&format!(
                "public oblivious func {name}_{ty}(a: {ty}, b: {ty}) -> bool {{ a {op} b }}\n"
            ));
        }
        src.push_str(&format!(
            "public oblivious func not_{ty}(a: {ty}, b: {ty}) -> {ty} {{ ~a ^ (b & 0) }}\n"
        ));
        src.push_str(&format!(
            "public oblivious func sel_{ty}(a: {ty}, b: {ty}) -> {ty} {{ ct_select(a < b, b - a, a * 3 + b) }}\n"
        ));
        if ty.starts_with('i') {
            src.push_str(&format!(
                "public oblivious func neg_{ty}(a: {ty}, b: {ty}) -> {ty} {{ -a + b }}\n"
            ));
        }
    }
    for from in TYPES {
        for to in TYPES {
            if from != to {
                src.push_str(&format!(
                    "public oblivious func cast_{from}_{to}(a: {from}, b: {from}) -> {to} {{ (a as {to}) + ((b == a) as {to}) }}\n"
                ));
            }
        }
    }
    src
}

/// The other `oblivious` / `@constant_time` functions the source checker
/// accepts in the repository's tests and documentation, one package each
/// (their names repeat).
const SOURCES: &[(&str, &str)] = &[
    (
        "tests/circuits.rs SCORE",
        r#"
oblivious func bonus(x: i64, y: i64) -> i64 {
    ct_select(x > y, 10i64, -10i64)
}

oblivious func score(x: i64, y: i64) -> i64 {
    var total = 0
    for i in 0..3 {
        total += x * (i + 1) - (y >> i)
    }
    total + bonus(x, y)
}

public func score_native(x: i64, y: i64) -> i64 {
    declassify(score(x, y))
}
"#,
    ),
    (
        "codira_circuits elaborate tests: score",
        r#"
oblivious func score(x: i64, y: i64) -> i64 {
    let base = x * 3 + y
    let bonus = ct_select(x > y, 10i64, -10i64)
    base + bonus
}
"#,
    ),
    (
        "codira_circuits elaborate tests: rotl, mix",
        r#"
oblivious func rotl(v: u32, k: u32) -> u32 {
    (v << k) | (v >> (32 - k))
}

oblivious func mix(a: u32, b: u32) -> u32 {
    var h = a
    for i in 0..4 {
        if i == 2 {
            continue
        }
        h = rotl(h ^ b, 5) + (i as u32) * (100 / 7)
    }
    var j = 0
    while j < 3 {
        h += 1
        j += 1
    }
    h
}
"#,
    ),
    (
        "codira_circuits elaborate tests: f",
        r#"
oblivious func f(x: u8, y: i16) -> i16 {
    let s: secret u8 = x + 200
    let wide = s as i16
    declassify(wide * y) - (y >> 15)
}
"#,
    ),
    (
        "codira_hir verify tests: selects, fine",
        r#"
oblivious func selects(a: u64, b: u64, pick: bool) -> u64 {
    ct_select(pick, a, b)
}

@constant_time
func fine(s: secret bool, a: u64, b: u64) -> secret u64 {
    ct_select(s, a, b)
}
"#,
    ),
    (
        "agents/USING_CODIRA.md and tests/constant_time.rs",
        EXAMPLES,
    ),
];

#[test]
fn the_repository_oblivious_functions() {
    let mut sources = vec![("tests/circuits.rs operators", operator_corpus())];
    sources.extend(SOURCES.iter().map(|&(l, s)| (l, s.to_string())));

    // What each level is known to report. `-O0`: `mix` and `score` keep
    // their (public) loop branches.
    let known_o0: BTreeSet<&str> = ["mix", "score"].into();

    for opt in ["0", "2", "3"] {
        let mut all = BTreeSet::new();
        for (label, source) in &sources {
            let (failing, message) = failing_functions(source, opt);
            if !failing.is_empty() {
                eprintln!("-O{opt} {label}:\n{message}");
            }
            all.extend(failing);
        }
        let all: BTreeSet<&str> = all.iter().map(String::as_str).collect();
        let expected = if opt == "0" {
            known_o0.clone()
        } else {
            BTreeSet::new()
        };
        assert_eq!(all, expected, "-O{opt}");
    }
}

#[test]
fn aarch64_libraries_are_checked_too() {
    // Rules (a)-(c) only: the `AArch64` backend keeps the 8-bit select a
    // `csel`, and the public loops of `mix` still branch at -O0.
    let target = ["--target", "aarch64-unknown-linux-gnu"];
    let (failing, message) = failing_functions_with(&operator_corpus(), "0", &target);
    assert!(failing.is_empty(), "{message}");
    let (failing, message) = failing_functions_with(SOURCES[2].1, "0", &target);
    assert_eq!(failing, BTreeSet::from(["mix".to_string()]), "{message}");
    assert!(message.contains("(AArch64)"), "{message}");
    assert!(message.contains("conditional branch"), "{message}");
    for opt in ["2", "3"] {
        let (failing, message) = failing_functions_with(SOURCES[2].1, opt, &target);
        assert!(failing.is_empty(), "-O{opt}: {message}");
    }
}

#[test]
fn the_library_entry_point_reports_every_function() {
    // `--certify` keeps the function symbols the check needs, without
    // running it.
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), &format!("{EXAMPLES}{SELECT_U8}{SCALED}"));
    assert_eq!(
        build(project.path(), &[&["--certify", "-O", "0"], X86_64].concat()),
        Ok(ExitStatus::Success)
    );
    let bytes = std::fs::read(library(project.path(), X86_64)).unwrap();
    let int = |secret| codira_ctcheck::Param {
        class: codira_ctcheck::ParamClass::Integer,
        secret,
    };
    let spec = |symbol: &str, params| codira_ctcheck::FunctionSpec {
        symbol: symbol.to_string(),
        name: symbol.to_string(),
        params: Some(params),
        secret_result: true,
    };
    let report = codira_ctcheck::check_image(
        &bytes,
        &[
            spec("ct_max", vec![int(true), int(true)]),
            spec("ct_equal", vec![int(true), int(true)]),
            spec("sel_u8", vec![int(true), int(true), int(true)]),
            spec("scaled", vec![int(true), int(false), int(false)]),
            spec("no_such_function", vec![]),
        ],
    )
    .unwrap();
    assert_eq!(report.arch, codira_ctcheck::Arch::X86_64);
    let by_name = |name: &str| report.functions.iter().find(|f| f.name == name).unwrap();
    assert!(by_name("ct_max").passed(), "{report:#?}");
    assert!(by_name("ct_equal").passed(), "{report:#?}");
    assert!(by_name("ct_max").addresses_checked);
    assert!(by_name("sel_u8").passed(), "{report:#?}");
    let scaled = by_name("scaled");
    assert_eq!(scaled.violations.len(), 1, "{scaled:#?}");
    assert_eq!(
        scaled.violations[0].rule,
        codira_ctcheck::Rule::VariableTime
    );
    let missing = by_name("no_such_function");
    assert!(!missing.was_checked());
    assert!(missing.notes[0].contains("no symbol"), "{missing:#?}");
}
