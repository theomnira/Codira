use codira_mir::{
    print_body, verify_body, Attr, Body, FoldError, Generator, GeneratorParam, GeneratorStore,
    OpKind, Region,
};

use crate::{elaborate, eval_body, eval_body_with, Env, EvalCtx, EvalError, Value};

// ---------------------------------------------------------------------------
// Straight-line interpreter tests
// ---------------------------------------------------------------------------

#[test]
fn comptime_two_plus_two_is_four() {
    // The literal example from spec/EIDOS_ARCHITECTURE.md §5:
    // `comptime { 2 + 2 }` should evaluate to 4 at compile time.
    let mut body = Body::new();
    let a = body.push(OpKind::Const(Attr::Int(2)), []);
    let b = body.push(OpKind::Const(Attr::Int(2)), []);
    body.push(OpKind::Add, [a, b]);
    verify_body(&body).unwrap();

    let result = eval_body(&body, &Env::new()).unwrap();
    assert_eq!(result, Value::Int(4));
}

#[test]
fn comptime_arithmetic_precedence_via_explicit_ops() {
    // `(3 * 4) - 5`
    let mut body = Body::new();
    let three = body.push(OpKind::Const(Attr::Int(3)), []);
    let four = body.push(OpKind::Const(Attr::Int(4)), []);
    let mul = body.push(OpKind::Mul, [three, four]);
    let five = body.push(OpKind::Const(Attr::Int(5)), []);
    body.push(OpKind::Sub, [mul, five]);

    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Int(7));
}

#[test]
fn comptime_if_picks_then_branch() {
    // `if 1 < 2 { 10 } else { 20 }`
    let mut body = Body::new();
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    let two = body.push(OpKind::Const(Attr::Int(2)), []);
    let cond = body.push(OpKind::Lt, [one, two]);

    let mut then_body = Body::new();
    then_body.push(OpKind::Const(Attr::Int(10)), []);
    let mut else_body = Body::new();
    else_body.push(OpKind::Const(Attr::Int(20)), []);

    body.push_with_regions(
        OpKind::If,
        [cond],
        [Region::new(then_body), Region::new(else_body)],
    );
    verify_body(&body).unwrap();

    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Int(10));
}

#[test]
fn comptime_if_picks_else_branch() {
    // `if false { 10 } else { 20 }`
    let mut body = Body::new();
    let cond = body.push(OpKind::Const(Attr::Bool(false)), []);
    let mut then_body = Body::new();
    then_body.push(OpKind::Const(Attr::Int(10)), []);
    let mut else_body = Body::new();
    else_body.push(OpKind::Const(Attr::Int(20)), []);

    body.push_with_regions(
        OpKind::If,
        [cond],
        [Region::new(then_body), Region::new(else_body)],
    );

    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Int(20));
}

#[test]
fn generic_param_specialization_binds_n() {
    // Elaborating `SIMD[f32, N]`'s hypothetical bounds-check body with N=4:
    // `N == 4`
    let mut body = Body::new();
    let n_ref = body.push(OpKind::ParamRef("N".into()), []);
    let four = body.push(OpKind::Const(Attr::Int(4)), []);
    body.push(OpKind::Eq, [n_ref, four]);

    let mut env = Env::new();
    env.bind("N", Value::Int(4));

    assert_eq!(eval_body(&body, &env).unwrap(), Value::Bool(true));
}

#[test]
fn unbound_param_ref_is_a_clean_error_not_a_panic() {
    let mut body = Body::new();
    body.push(OpKind::ParamRef("T".into()), []);

    let err = eval_body(&body, &Env::new()).unwrap_err();
    assert_eq!(err, EvalError::UnresolvedParam("T".into()));
}

#[test]
fn division_by_zero_is_a_clean_error_not_a_panic() {
    let mut body = Body::new();
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    let zero = body.push(OpKind::Const(Attr::Int(0)), []);
    body.push(OpKind::Div, [one, zero]);

    // Pure-op failures arrive wrapped in `EvalError::Fold` -- fold_op is
    // the single source of pure-op semantics (see interp's module doc).
    assert_eq!(
        eval_body(&body, &Env::new()).unwrap_err(),
        EvalError::Fold(FoldError::DivideByZero)
    );
}

// ---------------------------------------------------------------------------
// Bitwise / shift / float ops (delegated to fold_op)
// ---------------------------------------------------------------------------

#[test]
fn bitwise_and_shift_ops_evaluate() {
    let mut body = Body::new();
    let six = body.push(OpKind::Const(Attr::Int(6)), []);
    let three = body.push(OpKind::Const(Attr::Int(3)), []);
    body.push(OpKind::BitAnd, [six, three]);
    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Int(2));

    let mut body = Body::new();
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    let four = body.push(OpKind::Const(Attr::Int(4)), []);
    body.push(OpKind::Shl, [one, four]);
    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Int(16));

    // Right shift is arithmetic (sign-propagating), per OpKind::Shr's doc.
    let mut body = Body::new();
    let neg_eight = body.push(OpKind::Const(Attr::Int(-8)), []);
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    body.push(OpKind::Shr, [neg_eight, one]);
    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Int(-4));
}

#[test]
fn out_of_range_shift_is_a_fold_error() {
    let mut body = Body::new();
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    let sixty_four = body.push(OpKind::Const(Attr::Int(64)), []);
    body.push(OpKind::Shl, [one, sixty_four]);

    assert_eq!(
        eval_body(&body, &Env::new()).unwrap_err(),
        EvalError::Fold(FoldError::ShiftOutOfRange)
    );
}

#[test]
fn float_arithmetic_evaluates() {
    // `1.5 + 2.25` -- exact in binary floating point, so `==` is safe.
    let mut body = Body::new();
    let a = body.push(OpKind::Const(Attr::float(1.5)), []);
    let b = body.push(OpKind::Const(Attr::float(2.25)), []);
    body.push(OpKind::Add, [a, b]);
    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Float(3.75));

    // Mixed int/float promotes (fold_op's documented behavior).
    let mut body = Body::new();
    let two = body.push(OpKind::Const(Attr::Int(2)), []);
    let f = body.push(OpKind::Const(Attr::float(3.5)), []);
    body.push(OpKind::Mul, [two, f]);
    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Float(7.0));
}

#[test]
fn and_is_non_short_circuiting() {
    // `false && (1 / 0 == 0)`: strict top-to-bottom evaluation reaches
    // the division before the `and`, so this errors instead of
    // short-circuiting to `false`. Short-circuiting is `cf.if`'s job --
    // see the interp module doc.
    let mut body = Body::new();
    let f = body.push(OpKind::Const(Attr::Bool(false)), []);
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    let zero = body.push(OpKind::Const(Attr::Int(0)), []);
    let div = body.push(OpKind::Div, [one, zero]);
    let cmp = body.push(OpKind::Eq, [div, zero]);
    body.push(OpKind::And, [f, cmp]);

    assert_eq!(
        eval_body(&body, &Env::new()).unwrap_err(),
        EvalError::Fold(FoldError::DivideByZero)
    );
}

// ---------------------------------------------------------------------------
// Tuples
// ---------------------------------------------------------------------------

#[test]
fn tuple_pack_and_project() {
    let mut body = Body::new();
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    let t = body.push(OpKind::Const(Attr::Bool(true)), []);
    let tuple = body.push(OpKind::Tuple, [one, t]);
    body.push(OpKind::TupleGet(1), [tuple]);
    verify_body(&body).unwrap();

    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Bool(true));
}

#[test]
fn tuple_projection_out_of_range_is_malformed_tuple() {
    let mut body = Body::new();
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    let tuple = body.push(OpKind::Tuple, [one]);
    body.push(OpKind::TupleGet(5), [tuple]);

    assert_eq!(
        eval_body(&body, &Env::new()).unwrap_err(),
        EvalError::MalformedTuple
    );
}

#[test]
fn tuple_as_pure_op_operand_is_a_type_mismatch() {
    // Tuples have no Attr form; feeding one to `core.add` must be a
    // clean type error, never a panic inside fold_op.
    let mut body = Body::new();
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    let tuple = body.push(OpKind::Tuple, [one]);
    body.push(OpKind::Add, [tuple, one]);

    assert_eq!(
        eval_body(&body, &Env::new()).unwrap_err(),
        EvalError::TypeMismatch {
            expected: "a scalar (int, bool, float, string, or unit)",
            found: "tuple",
        }
    );
}

// ---------------------------------------------------------------------------
// Loops
// ---------------------------------------------------------------------------

/// `var i = 5; while i > 0 { i = i - 1 }; i` as a `cf.while` with one
/// loop-carried value.
fn countdown_while_body() -> Body {
    let mut body = Body::new();
    let init = body.push(OpKind::Const(Attr::Int(5)), []);

    let mut cond = Body::new();
    let i = cond.push(OpKind::BlockArg(0), []);
    let zero = cond.push(OpKind::Const(Attr::Int(0)), []);
    cond.push(OpKind::Gt, [i, zero]);

    let mut loop_body = Body::new();
    let i = loop_body.push(OpKind::BlockArg(0), []);
    let one = loop_body.push(OpKind::Const(Attr::Int(1)), []);
    let dec = loop_body.push(OpKind::Sub, [i, one]);
    loop_body.push(OpKind::Yield, [dec]);

    body.push_with_regions(
        OpKind::While,
        [init],
        [Region::with_args(1, cond), Region::with_args(1, loop_body)],
    );
    body
}

#[test]
fn while_countdown_single_carried_value() {
    let body = countdown_while_body();
    verify_body(&body).unwrap();
    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Int(0));
}

#[test]
fn while_with_two_carried_values_returns_a_tuple() {
    // `(i, acc) = (5, 0); while i > 0 { (i, acc) = (i - 1, acc + i) }`
    // -- computes 5+4+3+2+1 = 15; final carried values come back as a
    // tuple per the N > 1 result convention on OpKind::While.
    let mut body = Body::new();
    let init_i = body.push(OpKind::Const(Attr::Int(5)), []);
    let init_acc = body.push(OpKind::Const(Attr::Int(0)), []);

    let mut cond = Body::new();
    let i = cond.push(OpKind::BlockArg(0), []);
    let zero = cond.push(OpKind::Const(Attr::Int(0)), []);
    cond.push(OpKind::Gt, [i, zero]);

    let mut loop_body = Body::new();
    let i = loop_body.push(OpKind::BlockArg(0), []);
    let acc = loop_body.push(OpKind::BlockArg(1), []);
    let one = loop_body.push(OpKind::Const(Attr::Int(1)), []);
    let next_i = loop_body.push(OpKind::Sub, [i, one]);
    let next_acc = loop_body.push(OpKind::Add, [acc, i]);
    loop_body.push(OpKind::Yield, [next_i, next_acc]);

    body.push_with_regions(
        OpKind::While,
        [init_i, init_acc],
        [Region::with_args(2, cond), Region::with_args(2, loop_body)],
    );
    verify_body(&body).unwrap();

    assert_eq!(
        eval_body(&body, &Env::new()).unwrap(),
        Value::Tuple(vec![Value::Int(0), Value::Int(15)])
    );
}

/// `for iv in start..end (step 1) { acc = acc + iv }` -- built as a
/// `cf.for` with one carried value; body region args are [iv, acc].
fn sum_for_body(start: i64, end: i64) -> Body {
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(start)), []);
    let end = body.push(OpKind::Const(Attr::Int(end)), []);
    let step = body.push(OpKind::Const(Attr::Int(1)), []);
    let init = body.push(OpKind::Const(Attr::Int(0)), []);

    let mut loop_body = Body::new();
    let iv = loop_body.push(OpKind::BlockArg(0), []);
    let acc = loop_body.push(OpKind::BlockArg(1), []);
    let next = loop_body.push(OpKind::Add, [acc, iv]);
    loop_body.push(OpKind::Yield, [next]);

    body.push_with_regions(
        OpKind::For,
        [start, end, step, init],
        [Region::with_args(2, loop_body)],
    );
    body
}

#[test]
fn for_loop_sums_zero_to_ten() {
    let body = sum_for_body(0, 10);
    verify_body(&body).unwrap();
    // 0 + 1 + ... + 9 = 45 (`end` is exclusive: iv < end for step > 0).
    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Int(45));
}

#[test]
fn for_loop_with_negative_step_counts_down() {
    // `for iv in 3..0 step -1 { acc = acc + iv }` -> 3 + 2 + 1 = 6
    // (`iv > end` continuation for step < 0, per OpKind::For's doc).
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(3)), []);
    let end = body.push(OpKind::Const(Attr::Int(0)), []);
    let step = body.push(OpKind::Const(Attr::Int(-1)), []);
    let init = body.push(OpKind::Const(Attr::Int(0)), []);

    let mut loop_body = Body::new();
    let iv = loop_body.push(OpKind::BlockArg(0), []);
    let acc = loop_body.push(OpKind::BlockArg(1), []);
    let next = loop_body.push(OpKind::Add, [acc, iv]);
    loop_body.push(OpKind::Yield, [next]);

    body.push_with_regions(
        OpKind::For,
        [start, end, step, init],
        [Region::with_args(2, loop_body)],
    );
    verify_body(&body).unwrap();

    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Int(6));
}

#[test]
fn nested_for_loops_multiply_iteration_counts() {
    // `for i in 0..3 { for j in 0..3 { acc = acc + 1 } }` -> 9. The inner
    // loop's initial carried value is the *outer* body's block arg 1
    // (the outer accumulator), and its body reads only its own innermost
    // frame -- exercising the frame stack push/pop discipline.
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(0)), []);
    let end = body.push(OpKind::Const(Attr::Int(3)), []);
    let step = body.push(OpKind::Const(Attr::Int(1)), []);
    let init = body.push(OpKind::Const(Attr::Int(0)), []);

    let mut outer = Body::new();
    let outer_acc = outer.push(OpKind::BlockArg(1), []);
    let inner_start = outer.push(OpKind::Const(Attr::Int(0)), []);
    let inner_end = outer.push(OpKind::Const(Attr::Int(3)), []);
    let inner_step = outer.push(OpKind::Const(Attr::Int(1)), []);

    let mut inner = Body::new();
    let inner_acc = inner.push(OpKind::BlockArg(1), []);
    let one = inner.push(OpKind::Const(Attr::Int(1)), []);
    let bump = inner.push(OpKind::Add, [inner_acc, one]);
    inner.push(OpKind::Yield, [bump]);

    let inner_result = outer.push_with_regions(
        OpKind::For,
        [inner_start, inner_end, inner_step, outer_acc],
        [Region::with_args(2, inner)],
    );
    outer.push(OpKind::Yield, [inner_result]);

    body.push_with_regions(
        OpKind::For,
        [start, end, step, init],
        [Region::with_args(2, outer)],
    );
    verify_body(&body).unwrap();

    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Int(9));
}

#[test]
fn while_with_zero_carried_values_yields_unit() {
    // `while false {}` -- no carried values; result is Unit per the
    // N == 0 convention.
    let mut body = Body::new();
    let mut cond = Body::new();
    cond.push(OpKind::Const(Attr::Bool(false)), []);
    let mut loop_body = Body::new();
    loop_body.push(OpKind::Yield, []);
    body.push_with_regions(
        OpKind::While,
        [],
        [Region::with_args(0, cond), Region::with_args(0, loop_body)],
    );
    verify_body(&body).unwrap();

    assert_eq!(eval_body(&body, &Env::new()).unwrap(), Value::Unit);
}

#[test]
fn infinite_loop_exhausts_fuel_not_the_compiler() {
    // `while true {}` with a tiny fuel budget: must fail with
    // FuelExhausted, never hang.
    let mut body = Body::new();
    let mut cond = Body::new();
    cond.push(OpKind::Const(Attr::Bool(true)), []);
    let mut loop_body = Body::new();
    loop_body.push(OpKind::Yield, []);
    body.push_with_regions(
        OpKind::While,
        [],
        [Region::with_args(0, cond), Region::with_args(0, loop_body)],
    );
    verify_body(&body).unwrap();

    let ctx = EvalCtx {
        fuel: 50,
        ..EvalCtx::default()
    };
    assert_eq!(
        ctx.eval(&body, &Env::new()).unwrap_err(),
        EvalError::FuelExhausted
    );
}

#[test]
fn for_step_zero_is_malformed_not_a_hang() {
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(0)), []);
    let end = body.push(OpKind::Const(Attr::Int(10)), []);
    let step = body.push(OpKind::Const(Attr::Int(0)), []);
    let mut loop_body = Body::new();
    loop_body.push(OpKind::Yield, []);
    body.push_with_regions(
        OpKind::For,
        [start, end, step],
        [Region::with_args(1, loop_body)],
    );

    assert_eq!(
        eval_body(&body, &Env::new()).unwrap_err(),
        EvalError::MalformedLoop("cf.for step must be non-zero")
    );
}

#[test]
fn yield_outside_a_loop_body_is_malformed() {
    let mut body = Body::new();
    body.push(OpKind::Yield, []);

    assert_eq!(
        eval_body(&body, &Env::new()).unwrap_err(),
        EvalError::MalformedLoop("cf.yield is only valid as the last op of a loop body region")
    );
}

#[test]
fn malformed_loop_shapes_are_clean_errors() {
    // A `cf.for` with too few operands (the shape the old interpreter
    // reported as UnsupportedOp): now a MalformedLoop, still not a panic.
    let mut body = Body::new();
    body.push_with_regions(OpKind::For, [], []);
    assert_eq!(
        eval_body(&body, &Env::new()).unwrap_err(),
        EvalError::MalformedLoop("cf.for takes at least three operands (start, end, step)")
    );
}

#[test]
fn loop_textual_form_is_locked() {
    // Snapshot of the printer's loop syntax over a real, evaluating body
    // (the countdown loop above) -- locks the `cf.while`/`cond`/`body`/
    // `cf.yield` textual form the way codira_mir's own print tests lock
    // the straight-line form.
    let body = countdown_while_body();
    let printed = print_body(&body);
    let expected = "\
%0 = core.const 5
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
";
    assert_eq!(printed, expected);
}

// ---------------------------------------------------------------------------
// Calls (core.call via GeneratorStore)
// ---------------------------------------------------------------------------

fn store_with_add_one() -> GeneratorStore {
    let mut store = GeneratorStore::new();
    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    body.push(OpKind::Add, [x, one]);
    store.add_generator(Generator {
        name: "add_one".into(),
        params: vec![],
        body,
    });
    store
}

#[test]
fn call_resolves_through_the_generator_store() {
    let store = store_with_add_one();
    let mut body = Body::new();
    let forty_one = body.push(OpKind::Const(Attr::Int(41)), []);
    body.push(OpKind::Call("add_one".into()), [forty_one]);
    verify_body(&body).unwrap();

    assert_eq!(
        eval_body_with(&body, &Env::new(), Some(&store)).unwrap(),
        Value::Int(42)
    );
}

#[test]
fn call_without_a_store_is_unknown_callee() {
    let mut body = Body::new();
    let forty_one = body.push(OpKind::Const(Attr::Int(41)), []);
    body.push(OpKind::Call("add_one".into()), [forty_one]);

    assert_eq!(
        eval_body(&body, &Env::new()).unwrap_err(),
        EvalError::UnknownCallee("add_one".into())
    );
}

#[test]
fn call_to_a_missing_symbol_is_unknown_callee() {
    let store = store_with_add_one();
    let mut body = Body::new();
    body.push(OpKind::Call("nope".into()), []);

    assert_eq!(
        eval_body_with(&body, &Env::new(), Some(&store)).unwrap_err(),
        EvalError::UnknownCallee("nope".into())
    );
}

#[test]
fn recursive_factorial_terminates_and_is_correct() {
    // `fn fact(n) = if n <= 1 { 1 } else { n * fact(n - 1) }` -- bounded
    // recursion through `cf.if` well inside the depth limit, with the
    // recursive call sitting *inside* a region (exercising arg
    // propagation into `cf.if` regions).
    let mut store = GeneratorStore::new();
    let mut fact = Body::new();
    let n = fact.push(OpKind::Arg(0), []);
    let one = fact.push(OpKind::Const(Attr::Int(1)), []);
    let cond = fact.push(OpKind::Le, [n, one]);

    let mut base = Body::new();
    base.push(OpKind::Const(Attr::Int(1)), []);

    let mut recurse = Body::new();
    let n = recurse.push(OpKind::Arg(0), []);
    let one = recurse.push(OpKind::Const(Attr::Int(1)), []);
    let n_minus_one = recurse.push(OpKind::Sub, [n, one]);
    let sub_fact = recurse.push(OpKind::Call("fact".into()), [n_minus_one]);
    recurse.push(OpKind::Mul, [n, sub_fact]);

    fact.push_with_regions(
        OpKind::If,
        [cond],
        [Region::new(base), Region::new(recurse)],
    );
    verify_body(&fact).unwrap();
    store.add_generator(Generator {
        name: "fact".into(),
        params: vec![],
        body: fact,
    });

    let mut main = Body::new();
    let five = main.push(OpKind::Const(Attr::Int(5)), []);
    main.push(OpKind::Call("fact".into()), [five]);

    assert_eq!(
        eval_body_with(&main, &Env::new(), Some(&store)).unwrap(),
        Value::Int(120)
    );
}

#[test]
fn unbounded_recursion_hits_the_call_depth_limit() {
    // `fn forever() = forever()` -- must fail with CallDepthExceeded (the
    // depth limit trips long before the fuel budget: one op per frame).
    let mut store = GeneratorStore::new();
    let mut body = Body::new();
    body.push(OpKind::Call("forever".into()), []);
    store.add_generator(Generator {
        name: "forever".into(),
        params: vec![],
        body,
    });

    let mut main = Body::new();
    main.push(OpKind::Call("forever".into()), []);

    assert_eq!(
        eval_body_with(&main, &Env::new(), Some(&store)).unwrap_err(),
        EvalError::CallDepthExceeded {
            limit: crate::DEFAULT_CALL_DEPTH
        }
    );
}

#[test]
fn callee_shares_the_module_level_env() {
    // Generator parameter bindings are shared module-level comptime
    // bindings (see interp module doc): a callee referencing `N` sees the
    // caller's Env.
    let mut store = GeneratorStore::new();
    let mut body = Body::new();
    body.push(OpKind::ParamRef("N".into()), []);
    store.add_generator(Generator {
        name: "get_n".into(),
        params: vec![GeneratorParam { name: "N".into() }],
        body,
    });

    let mut main = Body::new();
    main.push(OpKind::Call("get_n".into()), []);

    let mut env = Env::new();
    env.bind("N", Value::Int(7));
    assert_eq!(
        eval_body_with(&main, &env, Some(&store)).unwrap(),
        Value::Int(7)
    );
}

// ---------------------------------------------------------------------------
// Elaborator tests
// ---------------------------------------------------------------------------

#[test]
fn elaboration_preserves_runtime_argument() {
    // `fn add[N](x) = x + N`, elaborated with N=5.
    // The runtime argument `x` must survive elaboration untouched -- it is
    // not something elaboration is allowed to specialize away.
    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let n = body.push(OpKind::ParamRef("N".into()), []);
    body.push(OpKind::Add, [x, n]);

    let generator = Generator {
        name: "add".into(),
        params: vec![GeneratorParam { name: "N".into() }],
        body,
    };

    let elaborated = elaborate(&generator, &[("N".into(), Value::Int(5))]);

    // The elaborated body still genuinely depends on a runtime argument:
    // evaluating it with no argument binding must fail with
    // RuntimeArgument, not silently produce a wrong constant.
    assert_eq!(
        eval_body(&elaborated, &Env::new()).unwrap_err(),
        EvalError::RuntimeArgument(0)
    );
}

#[test]
fn elaboration_folds_pure_generator_to_a_single_constant() {
    // `fn double[N]() = N * 2`, elaborated with N=5 -- no runtime
    // arguments at all, so the *entire* body should collapse to one
    // `Const(10)` op instead of retaining the multiplication. This is the
    // concrete "does less work than naive substitution" claim from the
    // module doc / architecture doc §7.
    let mut body = Body::new();
    let n = body.push(OpKind::ParamRef("N".into()), []);
    let two = body.push(OpKind::Const(Attr::Int(2)), []);
    body.push(OpKind::Mul, [n, two]);

    let generator = Generator {
        name: "double".into(),
        params: vec![GeneratorParam { name: "N".into() }],
        body,
    };

    let elaborated = elaborate(&generator, &[("N".into(), Value::Int(5))]);

    assert_eq!(eval_body(&elaborated, &Env::new()).unwrap(), Value::Int(10));
    // The performance claim, checked structurally: the naive substitution
    // would still contain 3 ops (ParamRef-turned-Const, the literal 2, and
    // the Mul); constant folding collapses that to exactly 1.
    assert_eq!(elaborated.iter().count(), 1);
}

#[test]
fn elaboration_folds_bitwise_ops() {
    // `fn shifted[N]() = N << 2`, N=3 -> Const(12) -- the new fold_op
    // delegation covers the bitwise/shift kinds too.
    let mut body = Body::new();
    let n = body.push(OpKind::ParamRef("N".into()), []);
    let two = body.push(OpKind::Const(Attr::Int(2)), []);
    body.push(OpKind::Shl, [n, two]);

    let generator = Generator {
        name: "shifted".into(),
        params: vec![GeneratorParam { name: "N".into() }],
        body,
    };

    let elaborated = elaborate(&generator, &[("N".into(), Value::Int(3))]);
    assert_eq!(eval_body(&elaborated, &Env::new()).unwrap(), Value::Int(12));
    assert_eq!(elaborated.iter().count(), 1);
}

#[test]
fn elaboration_inlines_the_taken_if_branch() {
    // `fn pick[Flag]() = if Flag { 1 } else { 2 + 2 }`, elaborated with
    // Flag=true. The whole `if` should disappear -- only the `then`
    // branch's (folded) value should remain reachable.
    let mut body = Body::new();
    let flag = body.push(OpKind::ParamRef("Flag".into()), []);

    let mut then_body = Body::new();
    then_body.push(OpKind::Const(Attr::Int(1)), []);

    let mut else_body = Body::new();
    let a = else_body.push(OpKind::Const(Attr::Int(2)), []);
    let b = else_body.push(OpKind::Const(Attr::Int(2)), []);
    else_body.push(OpKind::Add, [a, b]);

    body.push_with_regions(
        OpKind::If,
        [flag],
        [Region::new(then_body), Region::new(else_body)],
    );

    let generator = Generator {
        name: "pick".into(),
        params: vec![GeneratorParam {
            name: "Flag".into(),
        }],
        body,
    };

    let elaborated = elaborate(&generator, &[("Flag".into(), Value::Bool(true))]);

    assert_eq!(eval_body(&elaborated, &Env::new()).unwrap(), Value::Int(1));
    // Only the taken branch's single op should have been spliced in --
    // the untaken `else` branch's ops must not appear in the result body.
    assert_eq!(elaborated.iter().count(), 1);
}

#[test]
fn elaboration_supports_partial_specialization() {
    // Binding nothing leaves `param.ref` exactly as written -- elaboration
    // with an empty binding set must not fabricate a value for an
    // unresolved parameter.
    let mut body = Body::new();
    body.push(OpKind::ParamRef("T".into()), []);

    let generator = Generator {
        name: "identity".into(),
        params: vec![GeneratorParam { name: "T".into() }],
        body,
    };

    let elaborated = elaborate(&generator, &[]);

    assert_eq!(
        eval_body(&elaborated, &Env::new()).unwrap_err(),
        EvalError::UnresolvedParam("T".into())
    );
}

#[test]
fn elaboration_substitutes_params_inside_loop_regions() {
    // `fn sum_to[N]() = for iv in 0..N { acc = acc + iv }`, N=10 -> 45.
    // The `param.ref` sits in the loop's *operands*; a second one inside
    // the loop body region checks recursion into regions too:
    // `fn sum_scaled[N]() = for iv in 0..N { acc = acc + iv * (N - N + 1) }`
    // is overkill -- instead the body multiplies by ParamRef("M").
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(0)), []);
    let end = body.push(OpKind::ParamRef("N".into()), []);
    let step = body.push(OpKind::Const(Attr::Int(1)), []);
    let init = body.push(OpKind::Const(Attr::Int(0)), []);

    let mut loop_body = Body::new();
    let iv = loop_body.push(OpKind::BlockArg(0), []);
    let acc = loop_body.push(OpKind::BlockArg(1), []);
    let m = loop_body.push(OpKind::ParamRef("M".into()), []);
    let scaled = loop_body.push(OpKind::Mul, [iv, m]);
    let next = loop_body.push(OpKind::Add, [acc, scaled]);
    loop_body.push(OpKind::Yield, [next]);

    body.push_with_regions(
        OpKind::For,
        [start, end, step, init],
        [Region::with_args(2, loop_body)],
    );

    let generator = Generator {
        name: "sum_scaled".into(),
        params: vec![
            GeneratorParam { name: "N".into() },
            GeneratorParam { name: "M".into() },
        ],
        body,
    };

    let elaborated = elaborate(
        &generator,
        &[("N".into(), Value::Int(10)), ("M".into(), Value::Int(2))],
    );

    // The elaborated body must still be verifier-clean -- in particular
    // its loop region must have kept `num_args == 2` through elaboration
    // *and* compaction (the num_args-resetting bug this asserts against).
    verify_body(&elaborated).unwrap();

    let for_op = elaborated.get(elaborated.result().unwrap());
    assert_eq!(for_op.kind, OpKind::For);
    assert_eq!(for_op.regions[0].num_args, 2);
    // Both param.refs became constants: N in the loop operands...
    assert_eq!(
        elaborated.get(for_op.operands[1]).kind,
        OpKind::Const(Attr::Int(10))
    );
    // ...and M inside the loop body region.
    assert!(for_op.regions[0]
        .body
        .iter()
        .any(|(_, op)| op.kind == OpKind::Const(Attr::Int(2))));
    assert!(!for_op.regions[0]
        .body
        .iter()
        .any(|(_, op)| matches!(op.kind, OpKind::ParamRef(_))));

    // And it still computes the right thing: sum(iv * 2 for iv in 0..10).
    assert_eq!(eval_body(&elaborated, &Env::new()).unwrap(), Value::Int(90));
}

#[test]
fn compact_preserves_while_region_num_args() {
    // Elaborating a generator that *contains* a while loop but binds no
    // parameters still runs compact() over it -- the regression test for
    // the `Region::new(compact(..))` num_args reset: the returned loop
    // must keep cond/body num_args == 1 and stay verifier-clean.
    let generator = Generator {
        name: "countdown".into(),
        params: vec![],
        body: countdown_while_body(),
    };

    let elaborated = elaborate(&generator, &[]);
    verify_body(&elaborated).unwrap();

    let while_op = elaborated.get(elaborated.result().unwrap());
    assert_eq!(while_op.kind, OpKind::While);
    assert_eq!(while_op.regions[0].num_args, 1);
    assert_eq!(while_op.regions[1].num_args, 1);
    assert_eq!(eval_body(&elaborated, &Env::new()).unwrap(), Value::Int(0));
}

#[test]
fn elaboration_leaves_tuple_valued_bindings_as_param_refs() {
    // Tuples have no Attr form, so a tuple-valued parameter cannot become
    // a `core.const` -- elaboration leaves the `param.ref` in place and
    // the interpreter's Env resolves it instead (see elaborate's module
    // doc scope notes).
    let mut body = Body::new();
    body.push(OpKind::ParamRef("P".into()), []);
    let generator = Generator {
        name: "identity".into(),
        params: vec![GeneratorParam { name: "P".into() }],
        body,
    };

    let pair = Value::Tuple(vec![Value::Int(1), Value::Int(2)]);
    let elaborated = elaborate(&generator, &[("P".into(), pair.clone())]);

    assert_eq!(
        elaborated.get(elaborated.result().unwrap()).kind,
        OpKind::ParamRef("P".into())
    );
    let mut env = Env::new();
    env.bind("P", pair.clone());
    assert_eq!(eval_body(&elaborated, &env).unwrap(), pair);
}
