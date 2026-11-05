//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - Unit tests for the formatting rules.

use super::*;

fn fmt(text: &str) -> String {
    let once = format_source(text).unwrap_or_else(|e| panic!("{e}\n---\n{text}"));
    let twice = format_source(&once).unwrap();
    assert_eq!(once, twice, "not idempotent");
    once
}

#[test]
fn reindents_blocks() {
    assert_eq!(
        fmt("func f() -> i32 {\nlet x = 1;\n        if x > 0 {\n  return x;\n}\nreturn 0;\n}\n"),
        "func f() -> i32 {\n    let x = 1;\n    if x > 0 {\n        return x;\n    }\n    return 0;\n}\n"
    );
}

#[test]
fn normalizes_spacing() {
    assert_eq!(
        fmt("func  f( a : i32 ,b:i32 )->i32{\n    return a+b*2;\n}\n"),
        "func f(a: i32, b: i32) -> i32 {\n    return a + b * 2;\n}\n"
    );
}

#[test]
fn prefix_operators_and_calls() {
    assert_eq!(
        fmt("func f(x: i32) -> i32 {\n    let y = - x;\n    return g (y , ! true);\n}\n"),
        "func f(x: i32) -> i32 {\n    let y = -x;\n    return g(y, !true);\n}\n"
    );
}

#[test]
fn keeps_comments_and_one_blank_line() {
    let src = "// header\n\n\n\nfunc f() {   // trailing\n\n    let x = 1;    \n\n\n    /* block\n       comment */\n    let y = 2;\n\n}\n";
    assert_eq!(
        fmt(src),
        "// header\n\nfunc f() {   // trailing\n    let x = 1;\n\n    /* block\n       comment */\n    let y = 2;\n}\n"
    );
}

#[test]
fn trailing_whitespace_and_line_endings() {
    assert_eq!(
        fmt("func f() {\r\n\tlet x = 1;   // c   \r\n}\r\n\r\n\r\n"),
        "func f() {\n    let x = 1;   // c\n}\n"
    );
}

#[test]
fn closer_lines_up_with_its_opener() {
    assert_eq!(
        fmt("func f() {\n    g(1, {\n    2\n    }, 3);\n}\n"),
        "func f() {\n    g(1, {\n        2\n    }, 3);\n}\n"
    );
}

#[test]
fn continuation_lines() {
    assert_eq!(
        fmt("func f() -> i32 {\nlet x = 1\n+ 2;\nlet y =\n3;\nreturn x;\n}\n"),
        "func f() -> i32 {\n    let x = 1\n        + 2;\n    let y =\n        3;\n    return x;\n}\n"
    );
}

#[test]
fn else_on_its_own_line() {
    assert_eq!(
        fmt("func f(b: i32) -> i32 {\nlet x = if b == 1 { 2 }\nelse if b == 2 { 3 }\nelse { 4 };\nif b > 0 {\nreturn 1;\n}\nelse {\nreturn x;\n}\n}\n"),
        "func f(b: i32) -> i32 {\n    let x = if b == 1 { 2 }\n        else if b == 2 { 3 }\n        else { 4 };\n    if b > 0 {\n        return 1;\n    }\n    else {\n        return x;\n    }\n}\n"
    );
}

#[test]
fn imports() {
    assert_eq!(
        fmt("import root.a.{ B,c }\nimport root.d.{E}\n"),
        "import root.a.{ B, c }\nimport root.d.{ E }\n"
    );
}

#[test]
fn line_breaks_are_never_moved() {
    // The grammar is newline-sensitive: a statement may end at a line
    // break. The formatter keeps every break where it was.
    let src = "func f() {\n    let x = 1\n    let y = 2\n}\n";
    assert_eq!(fmt(src), src);
}

#[test]
fn doc_comment_stays_attached() {
    // A whitespace-only line between a doc comment and its function does
    // not detach the comment; formatting must not turn it into a blank
    // line that would.
    let src = "/// Docs.\n    \nfunc f() {}\n";
    let out = fmt(src);
    assert_eq!(out, "/// Docs.\nfunc f() {}\n");
    // A real blank line keeps the comment detached.
    assert_eq!(fmt("// note\n\nfunc f() {}\n"), "// note\n\nfunc f() {}\n");
}

#[test]
fn does_not_join_composite_operators() {
    // `a < = b` is not `a <= b`; removing the space would change the
    // parse, so the space stays (the source has an error here anyway).
    assert!(!can_join("<", "="));
    assert!(!can_join("-", ">"));
    assert!(!can_join("/", "/"));
    assert!(!can_join("1", "."));
    assert!(!can_join("a", "b"));
    assert!(can_join("f", "("));
    assert!(can_join("x", ","));
}

#[test]
fn refuses_syntax_errors() {
    match format_source("func f( {\n") {
        Err(FormatError::Syntax(errors)) => assert!(!errors.is_empty()),
        other => panic!("expected a syntax error, got {other:?}"),
    }
}

#[test]
fn empty_and_whitespace_only_files() {
    assert_eq!(fmt(""), "");
    assert_eq!(fmt("\n\n  \n"), "");
    assert_eq!(fmt("// only a comment"), "// only a comment\n");
}

#[test]
fn equivalence_check_detects_changes() {
    assert!(check_equivalent("func f() {}\n", "func  f( ) { }\n").is_ok());
    assert!(check_equivalent("func f() {}\n", "func g() {}\n").is_err());
    assert!(check_equivalent("let x = 1\nlet y = 2\n", "let x = 1 let y = 2\n").is_err());
    assert!(check_equivalent("// a\nfunc f() {}\n", "// b\nfunc f() {}\n").is_err());
}
