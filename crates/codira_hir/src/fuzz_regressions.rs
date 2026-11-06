//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Regression tests for crashes the compiler fuzzer found
//!   (`crates/codira_compiler/tests/fuzz_compiler.rs`): each program, as the
//!   fuzzer reduced it, must be rejected with diagnostics instead of
//!   panicking the type checker.
use crate::utils::tests::diagnostics;

/// Checks that `source` is rejected with diagnostics (and does not panic).
fn rejected(source: &str) -> String {
    let diagnostics = diagnostics(source);
    assert!(
        !diagnostics.is_empty(),
        "`{source}` was accepted without a diagnostic"
    );
    diagnostics
}

/// The lexer reads `5e` as a float literal; parsing it as a number panicked.
#[test]
fn an_exponent_without_digits_is_an_invalid_literal() {
    rejected("func d(5e");
    let diagnostics = rejected("func d() -> f64 { 5e }");
    assert!(diagnostics.contains("invalid literal"), "{diagnostics}");
}

/// A `.` at the end of the input lexed as a tuple index without digits, and
/// lowering the field access panicked parsing its number.
#[test]
fn a_field_access_cut_off_at_the_end_of_the_input() {
    rejected("func s{e.");
}

/// A `{` where a match arm should start sent the parser into a loop that
/// ended in its "the parser seems stuck" panic.
#[test]
fn a_brace_where_a_match_arm_should_start() {
    rejected("{match s{{");
}

/// Diagnostics about an expression or type the compiler made itself
/// (error recovery, desugaring, the type of `self`) unwrapped its missing
/// syntax. They are now reported on the function, never dropped.
#[test]
fn diagnostics_without_syntax_are_reported_on_the_function() {
    // The operand of `-` is missing: "cannot apply" on a missing operand.
    rejected("func u{-");
    // `self` outside an `extend` block: its type has no syntax.
    rejected("func n(self");
    let diagnostics = rejected("func n(self) {}");
    assert!(diagnostics.contains("undefined type"), "{diagnostics}");
    // An `if let` cut short inside an `if` condition.
    rejected("func s{if if let t");
}

/// A function declared without a body outside an `extern` block (the
/// fuzzer moved the `}` of one) was accepted, and code generation then
/// panicked on its missing body.
#[test]
fn a_function_without_a_body_outside_an_extern_block() {
    let found = rejected("func g(x: u32) -> f32;\npublic func main() -> f32 { g(1u32) }");
    assert!(found.contains("a function needs a body"), "{found}");
    // Still fine where a body may be left out.
    assert_eq!(diagnostics("extern func g(x: u32) -> f32;"), "");
    assert_eq!(diagnostics("trait T { func f(self) -> i64; }"), "");
}

/// A tuple index too large for `usize` panicked parsing it.
#[test]
fn a_tuple_index_too_large_for_usize() {
    rejected("func n{p.34028236692093846346");
    let found = rejected("func f(t: (i64, i64)) -> i64 { t.340282366920938463463374607431768211456 }");
    assert!(found.contains("non-existent field"), "{found}");
}

/// A `{` inside an import list sent the parser into a loop that ended in
/// its "the parser seems stuck" panic.
#[test]
fn a_brace_inside_an_import_list() {
    rejected("import x.{{");
}
