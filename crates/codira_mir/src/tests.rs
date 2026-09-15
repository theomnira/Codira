//! Copyright (c) 2026 Omnira CJSC
use crate::{
    print_body, verify_body, Attr, Body, Generator, GeneratorParam, GeneratorStore, OpKind, Region,
};

/// Builds the canonical countdown loop used across the core tests:
/// `while (x > 0) { x = x - 1 }` with one loop-carried value starting at
/// `init`.
fn countdown_while(init: i64) -> Body {
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(init)), []);

    let mut cond = Body::new();
    let x = cond.push(OpKind::BlockArg(0), []);
    let zero = cond.push(OpKind::Const(Attr::Int(0)), []);
    cond.push(OpKind::Gt, [x, zero]);

    let mut step = Body::new();
    let x = step.push(OpKind::BlockArg(0), []);
    let one = step.push(OpKind::Const(Attr::Int(1)), []);
    let next = step.push(OpKind::Sub, [x, one]);
    step.push(OpKind::Yield, [next]);

    body.push_with_regions(
        OpKind::While,
        [start],
        [Region::with_args(1, cond), Region::with_args(1, step)],
    );
    body
}

#[test]
fn builds_straight_line_arithmetic() {
    // `2 + 2`
    let mut body = Body::new();
    let two_a = body.push(OpKind::Const(Attr::Int(2)), []);
    let two_b = body.push(OpKind::Const(Attr::Int(2)), []);
    let sum = body.push(OpKind::Add, [two_a, two_b]);

    assert_eq!(body.result(), Some(sum));
    assert_eq!(body.get(sum).operands.as_slice(), &[two_a, two_b]);
}

#[test]
fn builds_if_with_nested_regions() {
    // `if true { 1 } else { 0 }`
    let mut body = Body::new();
    let cond = body.push(OpKind::Const(Attr::Bool(true)), []);

    let mut then_body = Body::new();
    then_body.push(OpKind::Const(Attr::Int(1)), []);
    let mut else_body = Body::new();
    else_body.push(OpKind::Const(Attr::Int(0)), []);

    let if_op = body.push_with_regions(
        OpKind::If,
        [cond],
        [Region::new(then_body), Region::new(else_body)],
    );

    let op = body.get(if_op);
    assert_eq!(op.regions.len(), 2);
    assert!(op.regions[0].body.result().is_some());
    assert!(op.regions[1].body.result().is_some());
}

#[test]
fn generator_store_round_trips() {
    let mut store = GeneratorStore::new();
    let mut body = Body::new();
    body.push(OpKind::ParamRef("N".into()), []);

    let id = store.add_generator(Generator {
        name: "identity".into(),
        params: vec![GeneratorParam { name: "N".into() }],
        body,
    });

    let generator = store.generator(id);
    assert_eq!(generator.name, "identity");
    assert_eq!(generator.params.len(), 1);
    assert_eq!(generator.params[0].name, "N");
}

#[test]
fn empty_body_has_no_result() {
    let body = Body::new();
    assert!(body.is_empty());
    assert_eq!(body.result(), None);
}

#[test]
fn while_loop_verifies_and_prints() {
    let body = countdown_while(10);
    verify_body(&body).expect("well-formed loop must verify");

    assert_eq!(
        print_body(&body),
        "\
%0 = core.const 10
%1 = cf.while(%0) {
cond(1): {
  %0 = cf.block_arg 0
  %1 = core.const 0
  %2 = core.gt %0, %1
}
body(1): {
  %0 = cf.block_arg 0
  %1 = core.const 1
  %2 = core.sub %0, %1
  cf.yield %2
}
}
"
    );
}

#[test]
fn verifier_rejects_missing_yield() {
    // A While whose body region forgets its `cf.yield`.
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(1)), []);

    let mut cond = Body::new();
    cond.push(OpKind::Const(Attr::Bool(false)), []);

    let mut step = Body::new();
    step.push(OpKind::BlockArg(0), []);

    body.push_with_regions(
        OpKind::While,
        [start],
        [Region::with_args(1, cond), Region::with_args(1, step)],
    );
    assert!(verify_body(&body).is_err());
}

#[test]
fn verifier_rejects_out_of_range_block_arg() {
    let mut body = Body::new();
    let cond = body.push(OpKind::Const(Attr::Bool(true)), []);

    // An If region referencing block arg 0 -- but If regions carry none,
    // and there is no enclosing loop, so index 0 is out of range.
    let mut then_body = Body::new();
    then_body.push(OpKind::BlockArg(0), []);
    let mut else_body = Body::new();
    else_body.push(OpKind::Const(Attr::Int(0)), []);

    body.push_with_regions(
        OpKind::If,
        [cond],
        [Region::new(then_body), Region::new(else_body)],
    );
    assert!(verify_body(&body).is_err());
}

#[test]
fn if_region_sees_enclosing_loop_args() {
    // `while (a > 0) { if (a > 5) { a - 2 } else { a - 1 } -> yield }`:
    // the If's branches reference the *loop's* block arg -- legal, because
    // If regions are transparent for block-argument scoping.
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(9)), []);

    let mut cond = Body::new();
    let a = cond.push(OpKind::BlockArg(0), []);
    let zero = cond.push(OpKind::Const(Attr::Int(0)), []);
    cond.push(OpKind::Gt, [a, zero]);

    let mut step = Body::new();
    let a = step.push(OpKind::BlockArg(0), []);
    let five = step.push(OpKind::Const(Attr::Int(5)), []);
    let big = step.push(OpKind::Gt, [a, five]);

    let mut fast = Body::new();
    let a_in = fast.push(OpKind::BlockArg(0), []);
    let two = fast.push(OpKind::Const(Attr::Int(2)), []);
    fast.push(OpKind::Sub, [a_in, two]);

    let mut slow = Body::new();
    let a_in = slow.push(OpKind::BlockArg(0), []);
    let one = slow.push(OpKind::Const(Attr::Int(1)), []);
    slow.push(OpKind::Sub, [a_in, one]);

    let next = step.push_with_regions(OpKind::If, [big], [Region::new(fast), Region::new(slow)]);
    step.push(OpKind::Yield, [next]);

    body.push_with_regions(
        OpKind::While,
        [start],
        [Region::with_args(1, cond), Region::with_args(1, step)],
    );
    verify_body(&body).expect("If regions inherit the loop's block args");
}

#[test]
fn for_loop_verifies() {
    // `for iv in 0..10 step 1 { acc = acc + iv }` with one carried value.
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(0)), []);
    let end = body.push(OpKind::Const(Attr::Int(10)), []);
    let step_c = body.push(OpKind::Const(Attr::Int(1)), []);
    let init = body.push(OpKind::Const(Attr::Int(0)), []);

    let mut loop_body = Body::new();
    let iv = loop_body.push(OpKind::BlockArg(0), []);
    let acc = loop_body.push(OpKind::BlockArg(1), []);
    let next = loop_body.push(OpKind::Add, [acc, iv]);
    loop_body.push(OpKind::Yield, [next]);

    body.push_with_regions(
        OpKind::For,
        [start, end, step_c, init],
        [Region::with_args(2, loop_body)],
    );
    verify_body(&body).expect("well-formed for loop must verify");
}

#[test]
fn verifier_rejects_yield_outside_loop() {
    let mut body = Body::new();
    let v = body.push(OpKind::Const(Attr::Int(1)), []);
    body.push(OpKind::Yield, [v]);
    assert!(verify_body(&body).is_err());
}

#[test]
fn generator_store_resolves_call_symbols() {
    let mut store = GeneratorStore::new();
    let mut body = Body::new();
    body.push(OpKind::Arg(0), []);
    store.add_generator(Generator {
        name: "double".into(),
        params: vec![],
        body,
    });

    assert!(store.generator_by_name("double").is_some());
    assert!(store.generator_by_name("missing").is_none());
}

// ---- typed MIR (RFC-001 phase 1) ----------------------------------------

use crate::{check_types, CastKind, CastMode, TypeId};

#[test]
fn untyped_bodies_still_verify() {
    // The phase-1 invariant: every existing body, built with the untyped
    // `push` API, must continue to verify and type-check.
    let body = countdown_while(10);
    verify_body(&body).expect("structural");
    check_types(&body).expect("untyped ops are skipped, not rejected");
    assert_eq!(body.untyped_count(), body.len(), "nothing typed yet");
}

#[test]
fn typed_arithmetic_checks() {
    let mut body = Body::new();
    let a = body.push_typed(OpKind::Const(Attr::Int(2)), [], TypeId::I32);
    let b = body.push_typed(OpKind::Const(Attr::Int(3)), [], TypeId::I32);
    body.push_typed(OpKind::Add, [a, b], TypeId::I32);
    check_types(&body).expect("i32 + i32 -> i32");
    assert_eq!(body.untyped_count(), 0);
}

#[test]
fn mixed_width_arithmetic_is_rejected() {
    // The bug the type checker exists to catch: an i32 operand feeding an
    // i64 add. Today this would lower to 64-bit arithmetic behind a
    // 32-bit signature.
    let mut body = Body::new();
    let a = body.push_typed(OpKind::Const(Attr::Int(2)), [], TypeId::I32);
    let b = body.push_typed(OpKind::Const(Attr::Int(3)), [], TypeId::I64);
    body.push_typed(OpKind::Add, [a, b], TypeId::I64);
    assert!(check_types(&body).is_err(), "i32 + i64 must not typecheck");
}

#[test]
fn signedness_is_part_of_type_identity() {
    // u64 and i64 must not be interchangeable: confusing them selects
    // bvudiv for signed division, a silent miscompile that only shows up
    // above 2^63.
    let mut body = Body::new();
    let a = body.push_typed(OpKind::Const(Attr::Int(1)), [], TypeId::U64);
    let b = body.push_typed(OpKind::Const(Attr::Int(2)), [], TypeId::I64);
    body.push_typed(OpKind::Div, [a, b], TypeId::I64);
    assert!(check_types(&body).is_err(), "u64 / i64 must not typecheck");
}

#[test]
fn comparison_must_produce_bool() {
    let mut body = Body::new();
    let a = body.push_typed(OpKind::Const(Attr::Int(1)), [], TypeId::I32);
    let b = body.push_typed(OpKind::Const(Attr::Int(2)), [], TypeId::I32);
    body.push_typed(OpKind::Lt, [a, b], TypeId::I32);
    assert!(
        check_types(&body).is_err(),
        "comparison result must be bool"
    );

    let mut ok = Body::new();
    let a = ok.push_typed(OpKind::Const(Attr::Int(1)), [], TypeId::I32);
    let b = ok.push_typed(OpKind::Const(Attr::Int(2)), [], TypeId::I32);
    ok.push_typed(OpKind::Lt, [a, b], TypeId::BOOL);
    check_types(&ok).expect("i32 < i32 -> bool");
}

#[test]
fn float_yielded_into_int_carried_value_is_caught() {
    // Exactly the hole reported during codegen work: a loop body yielding
    // a float into an int-initialized carried value. Invisible before
    // types; a type error at the loop op now.
    let mut body = Body::new();
    let start = body.push_typed(OpKind::Const(Attr::Int(0)), [], TypeId::I64);

    let mut cond = Body::new();
    cond.push_typed(OpKind::Const(Attr::Bool(true)), [], TypeId::BOOL);

    let mut step = Body::new();
    let wrong = step.push_typed(OpKind::Const(Attr::float(1.0)), [], TypeId::F64);
    step.push(OpKind::Yield, [wrong]);

    body.push_with_regions(
        OpKind::While,
        [start],
        [Region::with_args(1, cond), Region::with_args(1, step)],
    );

    verify_body(&body).expect("structurally fine -- that is the point");
    assert!(
        check_types(&body).is_err(),
        "yielding f64 into an i64 carried value must be a type error"
    );
}

#[test]
fn cast_width_legality_is_enforced() {
    // trunc must narrow, ext must widen, bitcast must preserve width.
    let bad_trunc = {
        let mut b = Body::new();
        let x = b.push_typed(OpKind::Const(Attr::Int(1)), [], TypeId::I32);
        b.push_typed(
            OpKind::Cast(CastKind::Trunc, CastMode::Wrapping),
            [x],
            TypeId::I64,
        );
        b
    };
    assert!(check_types(&bad_trunc).is_err(), "trunc must narrow");

    let bad_bitcast = {
        let mut b = Body::new();
        let x = b.push_typed(OpKind::Const(Attr::Int(1)), [], TypeId::I32);
        b.push_typed(
            OpKind::Cast(CastKind::Bitcast, CastMode::Wrapping),
            [x],
            TypeId::I64,
        );
        b
    };
    assert!(
        check_types(&bad_bitcast).is_err(),
        "bitcast must preserve width"
    );

    let good = {
        let mut b = Body::new();
        let x = b.push_typed(OpKind::Const(Attr::Int(1)), [], TypeId::I64);
        b.push_typed(
            OpKind::Cast(CastKind::Trunc, CastMode::Wrapping),
            [x],
            TypeId::I32,
        );
        b
    };
    verify_body(&good).unwrap();
    check_types(&good).expect("i64 -> i32 trunc is legal");
}

#[test]
fn int_float_conversions_check_categories() {
    let mut b = Body::new();
    let x = b.push_typed(OpKind::Const(Attr::Int(1)), [], TypeId::I32);
    // SiToFp from int to float: legal.
    b.push_typed(
        OpKind::Cast(CastKind::SiToFp, CastMode::Wrapping),
        [x],
        TypeId::F64,
    );
    check_types(&b).expect("i32 -> f64 sitofp is legal");

    let mut wrong = Body::new();
    let y = wrong.push_typed(OpKind::Const(Attr::Int(1)), [], TypeId::I32);
    // SiToFp to an int result: nonsense.
    wrong.push_typed(
        OpKind::Cast(CastKind::SiToFp, CastMode::Wrapping),
        [y],
        TypeId::I64,
    );
    assert!(check_types(&wrong).is_err(), "sitofp must produce a float");
}

#[test]
fn checked_casts_print_distinctly() {
    // The Wrapping/Checked axis must be visible in the IR text, since it
    // is the difference between a foldable conversion and one carrying an
    // undischarged proof obligation.
    let mut b = Body::new();
    let x = b.push_typed(OpKind::Const(Attr::Int(300)), [], TypeId::I64);
    b.push_typed(
        OpKind::Cast(CastKind::Trunc, CastMode::Checked),
        [x],
        TypeId::U8,
    );
    let text = print_body(&b);
    assert!(text.contains("core.trunc.checked"), "got:\n{text}");
}

#[test]
fn constant_casts_fold_away() {
    // `300 as u8` must collapse to the literal 44 before the e-graph ever
    // sees it -- otherwise every constant conversion becomes an e-class of
    // its own, which is pure node bloat for a value already known.
    use crate::pass::{PassContext, PassManager};
    let mut body = Body::new();
    let three_hundred = body.push_typed(OpKind::Const(Attr::Int(300)), [], TypeId::I64);
    body.push_typed(
        OpKind::Cast(CastKind::Trunc, CastMode::Wrapping),
        [three_hundred],
        TypeId::U8,
    );
    check_types(&body).expect("well-typed narrowing cast");

    crate::pass::default_pipeline()
        .run_to_fixpoint(&mut body, &PassContext::default(), 8)
        .expect("pipeline keeps the IR valid");

    assert_eq!(
        print_body(&body),
        "\
%0 = core.const 44
",
        "300 as u8 folds to 44"
    );
    let _ = PassManager::new();
}

#[test]
fn checked_casts_are_never_folded() {
    // A Checked cast carries an undischarged proof obligation. Folding it
    // would silently adopt wrapping semantics for a conversion whose whole
    // point is that the wrap has not been ruled out yet.
    use crate::pass::PassContext;
    let mut body = Body::new();
    let v = body.push_typed(OpKind::Const(Attr::Int(300)), [], TypeId::I64);
    body.push_typed(
        OpKind::Cast(CastKind::Trunc, CastMode::Checked),
        [v],
        TypeId::U8,
    );

    crate::pass::default_pipeline()
        .run_to_fixpoint(&mut body, &PassContext::default(), 8)
        .expect("pipeline keeps the IR valid");

    assert!(
        print_body(&body).contains("core.trunc.checked"),
        "a checked cast must survive constant folding, got:\n{}",
        print_body(&body)
    );
}
