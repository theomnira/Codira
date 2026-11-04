//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - One test per classification rule of `codira api-diff`.

use codira_hir_input::WithFixture;

use super::{api_diff, ApiChange, ApiDiff, Bump};
use crate::mock::MockDatabase;

fn diff(old: &str, new: &str) -> ApiDiff {
    let (old, _) = MockDatabase::with_single_file(old);
    let (new, _) = MockDatabase::with_single_file(new);
    api_diff(&old, &new)
}

/// The single change, asserting there is exactly one.
fn only(diff: &ApiDiff) -> &ApiChange {
    assert_eq!(diff.changes.len(), 1, "{:#?}", diff.changes);
    &diff.changes[0]
}

#[test]
fn added_and_removed_items_and_private_ones_ignored() {
    let d = diff(
        "public func a() -> i32 { 1 }\nfunc hidden() -> i32 { 1 }",
        "public func b() -> i32 { 1 }\nfunc hidden(x: i64) -> i64 { x }",
    );
    assert_eq!(d.changes.len(), 2, "{:#?}", d.changes);
    assert_eq!(d.changes[0].summary, "removes the function `a()`");
    assert_eq!(d.changes[0].bump, Some(Bump::Major));
    assert_eq!(d.changes[1].summary, "adds the function `b()`");
    assert_eq!(d.changes[1].bump, Some(Bump::Minor));
    assert_eq!(d.required(), Bump::Major);
}

#[test]
fn a_body_change_is_a_patch() {
    let d = diff(
        "public func f(x: i64) -> i64 { x + 1 }",
        "public func f(x: i64) -> i64 { 1 + x }",
    );
    assert!(d.changes.is_empty(), "{:#?}", d.changes);
    assert_eq!(d.required(), Bump::Patch);
    assert_eq!(d.sentence(), "changes no public API");
}

#[test]
fn a_changed_parameter_type_is_major_and_a_rename_is_not() {
    let d = diff(
        "public func f(x: i64) -> i64 { x }",
        "public func f(x: i32) -> i64 { 0 }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Major));
    assert_eq!(only(&d).summary, "changes the signature of `f()`");

    // The same clause over a renamed parameter: positionally the same
    // contract, an equivalent rewrite.
    let d = diff(
        "public func f(x: i64) -> i64 requires x > 0 { x }",
        "public func f(y: i64) -> i64 requires y > 0 { y }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Patch), "{:#?}", d.changes);
}

#[test]
fn an_added_effect_is_major_and_a_removed_one_minor() {
    let effects =
        "effect Net { func send(x: i32) -> i32 }\neffect Log { func log(x: i32) -> i32 }\n";
    let d = diff(
        &format!("{effects}public func parse(x: i32) uses Log -> i32 {{ x }}"),
        &format!("{effects}public func parse(x: i32) uses Net -> i32 {{ x }}"),
    );
    assert_eq!(d.changes.len(), 2, "{:#?}", d.changes);
    assert_eq!(d.changes[0].summary, "adds `Net` to `parse()`");
    assert_eq!(d.changes[0].bump, Some(Bump::Major));
    assert_eq!(d.changes[1].summary, "removes `Log` from `parse()`");
    assert_eq!(d.changes[1].bump, Some(Bump::Minor));
}

#[test]
fn a_strengthened_requires_is_major_with_the_input_it_now_refuses() {
    let d = diff(
        "public func f(x: i64) -> i64 requires x >= 0 { x }",
        "public func f(x: i64) -> i64 requires x > 0 { x }",
    );
    let change = only(&d);
    assert_eq!(change.bump, Some(Bump::Major));
    assert_eq!(change.summary, "strengthens the precondition of `f()`");
    assert!(
        change
            .details
            .iter()
            .any(|d| d == "accepted before, refused now: x = 0"),
        "{:#?}",
        change.details
    );
}

#[test]
fn a_weakened_requires_is_minor_and_an_equivalent_one_a_patch() {
    let d = diff(
        "public func f(x: i64) -> i64 requires x > 10 { x }",
        "public func f(x: i64) -> i64 requires x > 0 { x }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Minor));
    assert_eq!(only(&d).summary, "weakens the precondition of `f()`");

    let d = diff(
        "public func f(x: i64) -> i64 requires x > 0 { x }",
        "public func f(x: i64) -> i64 requires x >= 1 { x }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Patch));
}

#[test]
fn ensures_weakened_is_major_and_strengthened_minor() {
    let d = diff(
        "public func decode(x: i64) -> i64 ensures result >= 0 { 0 }",
        "public func decode(x: i64) -> i64 ensures result >= -1 { 0 }",
    );
    let change = only(&d);
    assert_eq!(change.bump, Some(Bump::Major));
    assert_eq!(change.summary, "weakens the postcondition of `decode()`");
    assert!(
        change.details.iter().any(|d| d.contains("result = -1")),
        "{:#?}",
        change.details
    );

    let d = diff(
        "public func decode(x: i64) -> i64 ensures result >= -1 { 0 }",
        "public func decode(x: i64) -> i64 ensures result >= 0 { 0 }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Minor));
    assert_eq!(
        only(&d).summary,
        "strengthens the postcondition of `decode()`"
    );
}

#[test]
fn ensures_are_compared_only_for_inputs_old_callers_can_pass() {
    // Weaker only where `x < 0`, which the precondition rules out.
    let d = diff(
        "public func f(x: i64) -> i64 requires x >= 0 ensures result >= x { x }",
        "public func f(x: i64) -> i64 requires x >= 0 ensures result >= x || x < 0 { x }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Patch), "{:#?}", d.changes);
}

#[test]
fn refinements_on_parameters_and_results_count_as_contracts() {
    let d = diff(
        "public func f(x: i64 { v | v >= 0 }) -> i64 { x }",
        "public func f(x: i64 { v | v >= 5 }) -> i64 { x }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Major));
    assert_eq!(only(&d).summary, "strengthens the precondition of `f()`");

    // Through a (private) alias: an inline refinement on the return type
    // reads as the body.
    let d = diff(
        "type Out = i64 { v | v > 0 }
public func g() -> Out { 1 }",
        "type Out = i64 { v | v != 0 }
public func g() -> Out { 1 }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Major));
    assert_eq!(only(&d).summary, "weakens the postcondition of `g()`");
}

#[test]
fn an_unencodable_clause_change_is_undecided_not_guessed() {
    let d = diff(
        "public func f(x: f64) -> f64 requires x > 0.0 { x }",
        "public func f(x: f64) -> f64 requires x > 1.0 { x }",
    );
    let change = only(&d);
    assert_eq!(change.bump, None);
    assert!(
        change.summary.ends_with("(undecided)"),
        "{}",
        change.summary
    );
    assert_eq!(d.undecided().count(), 1);
    assert_eq!(d.required(), Bump::Major, "undecided might be breaking");
}

#[test]
fn struct_layouts_and_alias_refinements() {
    let d = diff(
        "public struct Point { x: f64, y: f64 }",
        "public struct Point { x: f64, y: f64, z: f64 }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Major));
    assert_eq!(only(&d).summary, "changes the layout of `Point`");

    let d = diff(
        "public type Small = i64 { v | v < 10 }",
        "public type Small = i64 { v | v < 20 }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Major));
    let d = diff(
        "public type Small = i64 { v | v < 10 }",
        "public type Small = i64 { v | v <= 9 }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Patch));
}

#[test]
fn the_report_reads_as_one_sentence() {
    let effects = "effect Net { func send(x: i32) -> i32 }\n";
    let d = diff(
        &format!(
            "{effects}public func parse(x: i32) -> i32 {{ x }}\npublic func decode(x: i64) -> \
             i64 ensures result >= 0 {{ 0 }}"
        ),
        &format!(
            "{effects}public func parse(x: i32) uses Net -> i32 {{ x }}\npublic func decode(x: \
             i64) -> i64 {{ 0 }}"
        ),
    );
    assert_eq!(
        d.sentence(),
        "weakens the postcondition of `decode()` and adds `Net` to `parse()`"
    );
}

#[test]
fn generic_functions_compare_their_type_parameters_by_position() {
    let d = diff(
        "public func first[T](a: T, b: T) -> T { a }",
        "public func first[U](a: U, b: U) -> U { a }",
    );
    assert!(d.changes.is_empty(), "renaming T: {:#?}", d.changes);

    let d = diff(
        "public func first[T](a: T, b: T) -> T { a }",
        "public func first[T, V](a: T, b: V) -> T { a }",
    );
    assert_eq!(only(&d).bump, Some(Bump::Major));
    assert_eq!(only(&d).summary, "changes the signature of `first()`");
}

#[test]
fn a_widened_envelope_is_major_and_a_narrowed_one_minor() {
    // The signature is the same; a private callee starts allocating.
    let quiet = "func helper(x: i64) -> i64 { x + 1 }\n\
                 public func f(x: i64) -> i64 { helper(x) }";
    let noisy = "func helper(x: i64) -> i64 { println(\"hi\"); x + 1 }\n\
                 public func f(x: i64) -> i64 { helper(x) }";
    let d = diff(quiet, noisy);
    let change = only(&d);
    assert_eq!(change.bump, Some(Bump::Major), "{:#?}", d.changes);
    assert!(
        change
            .summary
            .starts_with("widens the semantic envelope of `f()` (allocation: none -> any"),
        "{}",
        change.summary
    );
    assert!(
        change
            .details
            .iter()
            .any(|d| d.starts_with("allocation is any now: calls `helper`")),
        "{:#?}",
        change.details
    );
    assert_eq!(d.required(), Bump::Major);

    let d = diff(noisy, quiet);
    let change = only(&d);
    assert_eq!(change.bump, Some(Bump::Minor), "{:#?}", d.changes);
    assert!(
        change
            .summary
            .starts_with("narrows the semantic envelope of `f()` (allocation: any -> none"),
        "{}",
        change.summary
    );
    assert_eq!(d.required(), Bump::Minor);
}
