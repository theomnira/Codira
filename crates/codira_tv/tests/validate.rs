//! Copyright (c) 2026 Omnira CJSC
//!
//! End-to-end translation-validation tests: build `before`/`after` bodies
//! with the public `codira_mir` API, structurally verify them (every body
//! a pass would hand the validator must pass `verify_body`), then check
//! the verdict.
//!
//! These run unconditionally, the same way `codira_smt`'s own tests do:
//! Z3 is a hard build-time requirement of that crate (its build script
//! fails the build if no install is found), so if this test binary linked
//! at all, the solver is present.

use codira_mir::{verify_body, Attr, Body, OpId, OpKind, Region};
use codira_tv::{validate_rewrite, Validator, Verdict};

fn arg(body: &mut Body, i: u32) -> OpId {
    body.push(OpKind::Arg(i), [])
}

fn int(body: &mut Body, v: i64) -> OpId {
    body.push(OpKind::Const(Attr::Int(v)), [])
}

/// Verifies both bodies (the validator's contract is verified IR) and
/// runs the default validator.
fn validate(before: &Body, after: &Body) -> Verdict {
    verify_body(before).expect("before body must be structurally valid");
    verify_body(after).expect("after body must be structurally valid");
    validate_rewrite(before, after)
}

#[test]
fn add_zero_is_identity() {
    // before: x + 0        after: x
    let mut before = Body::new();
    let x = arg(&mut before, 0);
    let zero = int(&mut before, 0);
    before.push(OpKind::Add, [x, zero]);

    let mut after = Body::new();
    arg(&mut after, 0);

    // Add is overflow-capable in the wrapping-i64 semantics (vacuously
    // here, but the encoder is deliberately syntactic about the caveat).
    assert_eq!(
        validate(&before, &after),
        Verdict::Proven {
            modulo_overflow: true
        }
    );
}

#[test]
fn mul_two_equals_shl_one() {
    // before: x * 2        after: x << 1
    let mut before = Body::new();
    let x = arg(&mut before, 0);
    let two = int(&mut before, 2);
    before.push(OpKind::Mul, [x, two]);

    let mut after = Body::new();
    let x2 = arg(&mut after, 0);
    let one = int(&mut after, 1);
    after.push(OpKind::Shl, [x2, one]);

    assert_eq!(
        validate(&before, &after),
        Verdict::Proven {
            modulo_overflow: true
        }
    );
}

#[test]
fn add_then_sub_cancels() {
    // before: (a + b) - b        after: a
    let mut before = Body::new();
    let a = arg(&mut before, 0);
    let b = arg(&mut before, 1);
    let sum = before.push(OpKind::Add, [a, b]);
    before.push(OpKind::Sub, [sum, b]);

    let mut after = Body::new();
    arg(&mut after, 0);

    assert_eq!(
        validate(&before, &after),
        Verdict::Proven {
            modulo_overflow: true
        }
    );
}

#[test]
fn sub_one_is_not_add_one() {
    // before: x - 1        after: x + 1   -- wrong on every input.
    let mut before = Body::new();
    let x = arg(&mut before, 0);
    let one = int(&mut before, 1);
    before.push(OpKind::Sub, [x, one]);

    let mut after = Body::new();
    let x2 = arg(&mut after, 0);
    let one2 = int(&mut after, 1);
    after.push(OpKind::Add, [x2, one2]);

    match validate(&before, &after) {
        Verdict::Refuted { args: Some(w) } => {
            assert_eq!(w.len(), 1, "one argument, one witness value");
            // Any value refutes this one; sanity-check the witness anyway.
            let x = w[0];
            assert_ne!(x.wrapping_sub(1), x.wrapping_add(1));
        }
        other => panic!("expected Refuted with a witness, got {other:?}"),
    }
}

#[test]
fn if_with_identical_branches_collapses() {
    // before: if (c) { a } else { a }        after: a
    // Exercises the fresh-variable ite encoding and bool argument
    // inference (c is only ever used as a cf.if condition).
    let mut then_region = Body::new();
    arg(&mut then_region, 1);
    let mut else_region = Body::new();
    arg(&mut else_region, 1);

    let mut before = Body::new();
    let c = arg(&mut before, 0);
    before.push_with_regions(
        OpKind::If,
        [c],
        [Region::new(then_region), Region::new(else_region)],
    );

    let mut after = Body::new();
    arg(&mut after, 1);

    // No arithmetic anywhere: the proof is exact, no overflow caveat.
    assert_eq!(
        validate(&before, &after),
        Verdict::Proven {
            modulo_overflow: false
        }
    );
}

#[test]
fn loops_are_unsupported() {
    // before: while (v < 10) { v = v + 1 } starting at 0        after: 10
    // The rewrite happens to be sound, but loops are outside the
    // fragment -- the honest answer is Unsupported, not Proven.
    let mut cond = Body::new();
    let cv = cond.push(OpKind::BlockArg(0), []);
    let limit = int(&mut cond, 10);
    cond.push(OpKind::Lt, [cv, limit]);

    let mut loop_body = Body::new();
    let bv = loop_body.push(OpKind::BlockArg(0), []);
    let one = int(&mut loop_body, 1);
    let next = loop_body.push(OpKind::Add, [bv, one]);
    loop_body.push(OpKind::Yield, [next]);

    let mut before = Body::new();
    let init = int(&mut before, 0);
    before.push_with_regions(
        OpKind::While,
        [init],
        [Region::with_args(1, cond), Region::with_args(1, loop_body)],
    );

    let mut after = Body::new();
    int(&mut after, 10);

    match validate(&before, &after) {
        Verdict::Unsupported { reason } => {
            assert!(
                reason.contains("loop"),
                "reason should name loops: {reason}"
            );
        }
        other => panic!("expected Unsupported for a loop, got {other:?}"),
    }
}

#[test]
fn division_by_variable_is_unsupported() {
    // before: x / y        after: x
    let mut before = Body::new();
    let x = arg(&mut before, 0);
    let y = arg(&mut before, 1);
    before.push(OpKind::Div, [x, y]);

    let mut after = Body::new();
    arg(&mut after, 0);

    match validate(&before, &after) {
        Verdict::Unsupported { reason } => assert!(
            reason.contains("non-constant divisor"),
            "reason should name the divisor: {reason}"
        ),
        other => panic!("expected Unsupported for variable division, got {other:?}"),
    }
}

#[test]
fn nonlinear_multiplication_is_unsupported() {
    // before: x * y        after: y * x -- true, but not in LIA's power.
    let mut before = Body::new();
    let x = arg(&mut before, 0);
    let y = arg(&mut before, 1);
    before.push(OpKind::Mul, [x, y]);

    let mut after = Body::new();
    let y2 = arg(&mut after, 1);
    let x2 = arg(&mut after, 0);
    after.push(OpKind::Mul, [y2, x2]);

    match validate(&before, &after) {
        Verdict::Unsupported { reason } => assert!(
            reason.contains("nonlinear"),
            "reason should say nonlinear: {reason}"
        ),
        other => panic!("expected Unsupported for x*y, got {other:?}"),
    }
}

#[test]
fn div_then_mul_roundtrip_is_refuted() {
    // A deliberately wrong "optimization": (x / 2) * 2  ==>  x.
    // Wrong for every odd x; the truncated-division encoding must catch
    // it and the probe should surface an odd witness (x = 1 is the first
    // candidate that works).
    let mut before = Body::new();
    let x = arg(&mut before, 0);
    let two = int(&mut before, 2);
    let half = before.push(OpKind::Div, [x, two]);
    let two_again = int(&mut before, 2);
    before.push(OpKind::Mul, [half, two_again]);

    let mut after = Body::new();
    arg(&mut after, 0);

    match validate(&before, &after) {
        Verdict::Refuted { args: Some(w) } => {
            assert_eq!(w.len(), 1);
            let x = w[0];
            assert!(x % 2 != 0, "only odd inputs refute x/2*2 == x, got {x}");
            assert_ne!((x / 2) * 2, x, "witness must actually disagree");
        }
        other => panic!("expected Refuted with an odd witness, got {other:?}"),
    }
}

#[test]
fn shr_is_floor_not_truncated_division() {
    // x >> 1  is NOT  x / 2: they differ on negative odd x (floor vs
    // truncation). The two encodings are distinct on purpose; the
    // validator must refute this plausible-looking "strength reduction".
    let mut before = Body::new();
    let x = arg(&mut before, 0);
    let one = int(&mut before, 1);
    before.push(OpKind::Shr, [x, one]);

    let mut after = Body::new();
    let x2 = arg(&mut after, 0);
    let two = int(&mut after, 2);
    after.push(OpKind::Div, [x2, two]);

    match validate(&before, &after) {
        Verdict::Refuted { args: Some(w) } => {
            let x = w[0];
            assert!(x < 0 && x % 2 != 0, "only negative odd x differ, got {x}");
            assert_ne!(x >> 1, x / 2);
        }
        other => panic!("expected Refuted, got {other:?}"),
    }
}

#[test]
fn not_eq_equals_ne() {
    // before: !(a == b)        after: a != b
    // Pure boolean/comparison body: the proof carries no overflow caveat.
    let mut before = Body::new();
    let a = arg(&mut before, 0);
    let b = arg(&mut before, 1);
    let eq = before.push(OpKind::Eq, [a, b]);
    before.push(OpKind::Not, [eq]);

    let mut after = Body::new();
    let a2 = arg(&mut after, 0);
    let b2 = arg(&mut after, 1);
    after.push(OpKind::Ne, [a2, b2]);

    assert_eq!(
        validate(&before, &after),
        Verdict::Proven {
            modulo_overflow: false
        }
    );
}

#[test]
fn conflicting_argument_sorts_are_unsupported() {
    // before uses arg0 as an integer (x + 1); after uses it as a boolean
    // (x && true). Sort inference must refuse rather than guess.
    let mut before = Body::new();
    let x = arg(&mut before, 0);
    let one = int(&mut before, 1);
    before.push(OpKind::Add, [x, one]);

    let mut after = Body::new();
    let x2 = arg(&mut after, 0);
    let t = after.push(OpKind::Const(Attr::Bool(true)), []);
    after.push(OpKind::And, [x2, t]);

    match validate(&before, &after) {
        Verdict::Unsupported { reason } => assert!(
            reason.contains("both an integer and a boolean"),
            "reason should describe the sort conflict: {reason}"
        ),
        other => panic!("expected Unsupported for a sort conflict, got {other:?}"),
    }
}

#[test]
fn witness_search_can_be_disabled() {
    // Same refutation as sub_one_is_not_add_one, but with the bounded
    // probe turned off: still Refuted, just without a concrete witness.
    let mut before = Body::new();
    let x = arg(&mut before, 0);
    let one = int(&mut before, 1);
    before.push(OpKind::Sub, [x, one]);

    let mut after = Body::new();
    let x2 = arg(&mut after, 0);
    let one2 = int(&mut after, 1);
    after.push(OpKind::Add, [x2, one2]);

    verify_body(&before).unwrap();
    verify_body(&after).unwrap();
    assert_eq!(
        Validator::new()
            .without_witness_search()
            .validate(&before, &after),
        Verdict::Refuted { args: None }
    );
}
