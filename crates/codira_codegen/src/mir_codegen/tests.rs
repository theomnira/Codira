//! Real end-to-end proof: build a `codira_mir::Body` by hand, lower it to
//! LLVM IR via `lower_mir_body`, JIT-compile the containing module, and
//! actually *call* the resulting native machine code -- not just check
//! that IR was emitted without erroring. Every hand-built body is run
//! through `codira_mir::verify_body` first, so these tests can never
//! accidentally lock in a lowering of IR the verifier rejects.
//!
//! Also contains IR-*shape* tests (assertions on the printed LLVM IR) for
//! the structured->CFG loop flattening, where the interesting property is
//! the emitted CFG itself (phis, select-based `cf.for` direction test),
//! not just the computed value.

#![allow(clippy::items_after_statements)]
//! Lint note: each test declares its own `type TestFn = ...` alias next to
//! the JIT call that uses it. Hoisting those to module scope would put a
//! dozen near-identical aliases far from their single use site.

use codira_mir::{Attr, Body, OpKind, Region};
use inkwell::{
    context::Context,
    targets::{InitializationConfig, Target},
    values::AnyValue,
    OptimizationLevel,
};

fn init_native_target() {
    Target::initialize_native(&InitializationConfig::default())
        .expect("failed to initialize native target for JIT");
}

/// Verifies `body`, lowers it as `fn(i64) -> i64`, JIT-compiles, and runs
/// it on each input. Panics (with the LLVM verifier's diagnostics) if the
/// lowering is refused or produces invalid IR -- the shared harness for
/// every single-argument integer test below.
fn run_i64_unary(body: &Body, inputs: &[i64]) -> Vec<i64> {
    codira_mir::verify_body(body).expect("hand-built test body failed the MIR verifier");
    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_test");
    let builder = context.create_builder();

    let i64_ty = context.i64_type();
    let fn_type = i64_ty.fn_type(&[i64_ty.into()], false);
    let function = module.add_function("test_fn", fn_type, None);
    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);

    let arg = function.get_nth_param(0).unwrap();
    let result = super::lower_mir_body(&context, &builder, &module, function, body, &[arg])
        .expect("expected a lowered result");
    builder.build_return(Some(&result)).unwrap();
    assert!(function.verify(true), "generated function failed to verify");

    let engine = module
        .create_jit_execution_engine(OptimizationLevel::None)
        .expect("failed to create JIT execution engine");
    type TestFn = unsafe extern "C" fn(i64) -> i64;
    let compiled: inkwell::execution_engine::JitFunction<'_, TestFn> =
        unsafe { engine.get_function("test_fn") }.expect("function not found in JIT module");
    inputs
        .iter()
        .map(|&x| unsafe { compiled.call(x) })
        .collect()
}

/// As [`run_i64_unary`] but `fn() -> i64` -- for bodies with no runtime
/// arguments (loop and tuple tests over constants).
fn run_i64_nullary(body: &Body) -> i64 {
    codira_mir::verify_body(body).expect("hand-built test body failed the MIR verifier");
    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_test");
    let builder = context.create_builder();

    let fn_type = context.i64_type().fn_type(&[], false);
    let function = module.add_function("test_fn", fn_type, None);
    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);

    let result = super::lower_mir_body(&context, &builder, &module, function, body, &[])
        .expect("expected a lowered result");
    builder.build_return(Some(&result)).unwrap();
    assert!(function.verify(true), "generated function failed to verify");

    let engine = module
        .create_jit_execution_engine(OptimizationLevel::None)
        .expect("failed to create JIT execution engine");
    type TestFn = unsafe extern "C" fn() -> i64;
    let compiled: inkwell::execution_engine::JitFunction<'_, TestFn> =
        unsafe { engine.get_function("test_fn") }.expect("function not found in JIT module");
    unsafe { compiled.call() }
}

/// The float-capable harness variant: `fn(f64) -> f64`. Exists because
/// the integer harnesses above fix the LLVM function type to `i64`s;
/// float bodies need a float signature end to end.
fn run_f64_unary(body: &Body, inputs: &[f64]) -> Vec<f64> {
    codira_mir::verify_body(body).expect("hand-built test body failed the MIR verifier");
    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_test_f64");
    let builder = context.create_builder();

    let f64_ty = context.f64_type();
    let fn_type = f64_ty.fn_type(&[f64_ty.into()], false);
    let function = module.add_function("test_fn", fn_type, None);
    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);

    let arg = function.get_nth_param(0).unwrap();
    let result = super::lower_mir_body(&context, &builder, &module, function, body, &[arg])
        .expect("expected a lowered result");
    builder.build_return(Some(&result)).unwrap();
    assert!(function.verify(true), "generated function failed to verify");

    let engine = module
        .create_jit_execution_engine(OptimizationLevel::None)
        .expect("failed to create JIT execution engine");
    type TestFn = unsafe extern "C" fn(f64) -> f64;
    let compiled: inkwell::execution_engine::JitFunction<'_, TestFn> =
        unsafe { engine.get_function("test_fn") }.expect("function not found in JIT module");
    inputs
        .iter()
        .map(|&x| unsafe { compiled.call(x) })
        .collect()
}

#[test]
fn jit_runs_arithmetic_with_a_runtime_argument() {
    // `fn(x: i64) -> i64 { x + 5 }`
    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_test");
    let builder = context.create_builder();

    let i64_ty = context.i64_type();
    let fn_type = i64_ty.fn_type(&[i64_ty.into()], false);
    let function = module.add_function("add_five", fn_type, None);
    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);

    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let five = body.push(OpKind::Const(Attr::Int(5)), []);
    body.push(OpKind::Add, [x, five]);

    let arg = function.get_nth_param(0).unwrap();
    let result = super::lower_mir_body(&context, &builder, &module, function, &body, &[arg])
        .expect("expected a lowered result");
    builder.build_return(Some(&result)).unwrap();

    assert!(function.verify(true), "generated function failed to verify");

    let engine = module
        .create_jit_execution_engine(OptimizationLevel::None)
        .expect("failed to create JIT execution engine");

    type AddFive = unsafe extern "C" fn(i64) -> i64;
    let add_five: inkwell::execution_engine::JitFunction<'_, AddFive> =
        unsafe { engine.get_function("add_five") }.expect("function not found in JIT module");

    assert_eq!(unsafe { add_five.call(10) }, 15);
    assert_eq!(unsafe { add_five.call(-5) }, 0);
    assert_eq!(unsafe { add_five.call(0) }, 5);
}

#[test]
fn jit_runs_real_conditional_branching() {
    // `fn(x: i64) -> i64 { if x < 10 { 1 } else { 0 } }`
    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_test_if");
    let builder = context.create_builder();

    let i64_ty = context.i64_type();
    let fn_type = i64_ty.fn_type(&[i64_ty.into()], false);
    let function = module.add_function("is_small", fn_type, None);
    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);

    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let ten = body.push(OpKind::Const(Attr::Int(10)), []);
    let cond = body.push(OpKind::Lt, [x, ten]);

    let mut then_body = Body::new();
    then_body.push(OpKind::Const(Attr::Int(1)), []);
    let mut else_body = Body::new();
    else_body.push(OpKind::Const(Attr::Int(0)), []);

    body.push_with_regions(
        OpKind::If,
        [cond],
        [Region::new(then_body), Region::new(else_body)],
    );

    let arg = function.get_nth_param(0).unwrap();
    let result = super::lower_mir_body(&context, &builder, &module, function, &body, &[arg])
        .expect("expected a lowered result");
    builder.build_return(Some(&result)).unwrap();

    assert!(function.verify(true), "generated function failed to verify");

    let engine = module
        .create_jit_execution_engine(OptimizationLevel::None)
        .expect("failed to create JIT execution engine");

    type IsSmall = unsafe extern "C" fn(i64) -> i64;
    let is_small: inkwell::execution_engine::JitFunction<'_, IsSmall> =
        unsafe { engine.get_function("is_small") }.expect("function not found in JIT module");

    assert_eq!(unsafe { is_small.call(5) }, 1);
    assert_eq!(unsafe { is_small.call(20) }, 0);
    assert_eq!(unsafe { is_small.call(10) }, 0);
}

#[test]
fn honestly_refuses_unelaborated_param_ref() {
    // A `ParamRef` that survived to codegen (i.e. elaboration didn't bind
    // it) must not be silently miscompiled -- `lower_mir_body` should
    // return `None`, not fabricate a value.
    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_test_unresolved");
    let builder = context.create_builder();

    let i64_ty = context.i64_type();
    let fn_type = i64_ty.fn_type(&[], false);
    let function = module.add_function("unresolved", fn_type, None);
    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);

    let mut body = Body::new();
    body.push(OpKind::ParamRef("N".into()), []);

    assert!(super::lower_mir_body(&context, &builder, &module, function, &body, &[]).is_none());
}

// ---- cf.while ------------------------------------------------------------

#[test]
fn jit_runs_while_with_one_carried_value() {
    // `fn(x: i64) -> i64 { var i = x; while i < 100 { i = i * 2 }; i }`
    let mut body = Body::new();
    let init = body.push(OpKind::Arg(0), []);

    let mut cond = Body::new();
    let i = cond.push(OpKind::BlockArg(0), []);
    let hundred = cond.push(OpKind::Const(Attr::Int(100)), []);
    cond.push(OpKind::Lt, [i, hundred]);

    let mut loop_body = Body::new();
    let i = loop_body.push(OpKind::BlockArg(0), []);
    let two = loop_body.push(OpKind::Const(Attr::Int(2)), []);
    let doubled = loop_body.push(OpKind::Mul, [i, two]);
    loop_body.push(OpKind::Yield, [doubled]);

    body.push_with_regions(
        OpKind::While,
        [init],
        [Region::with_args(1, cond), Region::with_args(1, loop_body)],
    );

    // 3 -> 6 -> 12 -> 24 -> 48 -> 96 -> 192 (first value >= 100).
    // 150 exits on the *first* condition check -- while, not do-while.
    assert_eq!(run_i64_unary(&body, &[3, 150, 100]), vec![192, 150, 100]);
}

#[test]
fn while_countdown_executes_to_correct_result() {
    // `fn(x: i64) -> i64 { var i = x; while i > 0 { i = i - 1 }; i }`
    let mut body = Body::new();
    let init = body.push(OpKind::Arg(0), []);

    let mut cond = Body::new();
    let i = cond.push(OpKind::BlockArg(0), []);
    let zero = cond.push(OpKind::Const(Attr::Int(0)), []);
    cond.push(OpKind::Gt, [i, zero]);

    let mut loop_body = Body::new();
    let i = loop_body.push(OpKind::BlockArg(0), []);
    let one = loop_body.push(OpKind::Const(Attr::Int(1)), []);
    let next = loop_body.push(OpKind::Sub, [i, one]);
    loop_body.push(OpKind::Yield, [next]);

    body.push_with_regions(
        OpKind::While,
        [init],
        [Region::with_args(1, cond), Region::with_args(1, loop_body)],
    );

    // Terminates at 0 from above; negative inputs never enter the body.
    assert_eq!(run_i64_unary(&body, &[5, 0, -3]), vec![0, 0, -3]);
}

// ---- cf.for --------------------------------------------------------------

/// `for iv in start..end step step { acc = acc + iv }; acc` -- the shared
/// shape for the counted-loop tests.
fn for_sum_body(start: i64, end: i64, step: i64) -> Body {
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(start)), []);
    let end = body.push(OpKind::Const(Attr::Int(end)), []);
    let step = body.push(OpKind::Const(Attr::Int(step)), []);
    let acc0 = body.push(OpKind::Const(Attr::Int(0)), []);

    let mut loop_body = Body::new();
    let iv = loop_body.push(OpKind::BlockArg(0), []);
    let acc = loop_body.push(OpKind::BlockArg(1), []);
    let next = loop_body.push(OpKind::Add, [acc, iv]);
    loop_body.push(OpKind::Yield, [next]);

    body.push_with_regions(
        OpKind::For,
        [start, end, step, acc0],
        [Region::with_args(2, loop_body)],
    );
    body
}

#[test]
fn jit_runs_for_loop_sum() {
    // sum(0..10) == 45
    assert_eq!(run_i64_nullary(&for_sum_body(0, 10, 1)), 45);
}

#[test]
fn jit_runs_for_loop_with_negative_step() {
    // Descending iteration: `step > 0 ? iv < end : iv > end`, so
    // 10, 9, .., 1 (exclusive of end=0) sums to 55.
    assert_eq!(run_i64_nullary(&for_sum_body(10, 0, -1)), 55);
    // An ascending range walked with a negative step exits immediately
    // (iv > end is false on the first check) -- accumulator untouched.
    assert_eq!(run_i64_nullary(&for_sum_body(0, 10, -1)), 0);
}

#[test]
fn jit_runs_fibonacci_via_for_with_two_carried_values() {
    // (a, b) starts at (0, 1); each of the 10 iterations yields
    // (b, a + b); a ends as fib(10) = 55. Exercises the N > 1 loop-result
    // convention (a `Tuple`-shaped struct) *and* `TupleGet` off it.
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(0)), []);
    let end = body.push(OpKind::Const(Attr::Int(10)), []);
    let step = body.push(OpKind::Const(Attr::Int(1)), []);
    let a0 = body.push(OpKind::Const(Attr::Int(0)), []);
    let b0 = body.push(OpKind::Const(Attr::Int(1)), []);

    let mut loop_body = Body::new();
    let a = loop_body.push(OpKind::BlockArg(1), []);
    let b = loop_body.push(OpKind::BlockArg(2), []);
    let sum = loop_body.push(OpKind::Add, [a, b]);
    loop_body.push(OpKind::Yield, [b, sum]);

    let result = body.push_with_regions(
        OpKind::For,
        [start, end, step, a0, b0],
        [Region::with_args(3, loop_body)],
    );
    body.push(OpKind::TupleGet(0), [result]);

    assert_eq!(run_i64_nullary(&body), 55);
}

#[test]
fn jit_runs_block_arg_inside_if_inside_loop() {
    // The frame-transparency rule: `cf.if` regions take no block args, so
    // a `BlockArg` inside a branch resolves against the enclosing *loop*'s
    // frame. `for iv in 0..5 { acc = if iv % 2 == 0 { acc + iv } else
    // { acc } }` -- sum of evens below 5 == 0 + 2 + 4 == 6.
    let mut body = Body::new();
    let start = body.push(OpKind::Const(Attr::Int(0)), []);
    let end = body.push(OpKind::Const(Attr::Int(5)), []);
    let step = body.push(OpKind::Const(Attr::Int(1)), []);
    let acc0 = body.push(OpKind::Const(Attr::Int(0)), []);

    let mut loop_body = Body::new();
    let iv = loop_body.push(OpKind::BlockArg(0), []);
    let two = loop_body.push(OpKind::Const(Attr::Int(2)), []);
    let rem = loop_body.push(OpKind::Rem, [iv, two]);
    let zero = loop_body.push(OpKind::Const(Attr::Int(0)), []);
    let is_even = loop_body.push(OpKind::Eq, [rem, zero]);

    // Both branches reference the loop's block args from *inside* the If.
    let mut then_body = Body::new();
    let acc = then_body.push(OpKind::BlockArg(1), []);
    let iv = then_body.push(OpKind::BlockArg(0), []);
    then_body.push(OpKind::Add, [acc, iv]);
    let mut else_body = Body::new();
    else_body.push(OpKind::BlockArg(1), []);

    let next = loop_body.push_with_regions(
        OpKind::If,
        [is_even],
        [Region::new(then_body), Region::new(else_body)],
    );
    loop_body.push(OpKind::Yield, [next]);

    body.push_with_regions(
        OpKind::For,
        [start, end, step, acc0],
        [Region::with_args(2, loop_body)],
    );

    assert_eq!(run_i64_nullary(&body), 6);
}

// ---- bitwise / shift ------------------------------------------------------

#[test]
fn jit_runs_bitwise_and_shift_ops() {
    // `fn(x) { (((x << 2) >> 3) ^ (x & 12)) | (x % 7) }` -- covers Shl,
    // Shr, BitXor, BitAnd, BitOr in one executed expression. `>>` must be
    // *arithmetic* (fold.rs: `Attr::Int` is signed), which the negative
    // input locks in: a logical shift would produce a huge positive value.
    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let two = body.push(OpKind::Const(Attr::Int(2)), []);
    let three = body.push(OpKind::Const(Attr::Int(3)), []);
    let twelve = body.push(OpKind::Const(Attr::Int(12)), []);
    let seven = body.push(OpKind::Const(Attr::Int(7)), []);
    let shl = body.push(OpKind::Shl, [x, two]);
    let shr = body.push(OpKind::Shr, [shl, three]);
    let and = body.push(OpKind::BitAnd, [x, twelve]);
    let xor = body.push(OpKind::BitXor, [shr, and]);
    let rem = body.push(OpKind::Rem, [x, seven]);
    body.push(OpKind::BitOr, [xor, rem]);

    let expected = |x: i64| (((x << 2) >> 3) ^ (x & 12)) | (x % 7); // Rust i64 `>>` is arithmetic too
    assert_eq!(
        run_i64_unary(&body, &[0, 5, 100, -8, -1]),
        [0i64, 5, 100, -8, -1].map(expected).to_vec()
    );
}

#[test]
fn jit_arithmetic_shift_right_propagates_sign() {
    // The exact case from `codira_mir::fold::tests::shifts_are_checked`:
    // -8 >> 1 == -4, executed as machine code.
    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    body.push(OpKind::Shr, [x, one]);
    assert_eq!(run_i64_unary(&body, &[-8, 8]), vec![-4, 4]);
}

// ---- floats ----------------------------------------------------------------

#[test]
fn jit_runs_float_arithmetic_with_int_promotion() {
    // `fn(x: f64) -> f64 { x * 2 + 1.5 }` -- the `2` is an *integer*
    // constant, so the multiply exercises the mixed int/float promotion
    // rule (either-operand-float => fmul), matching fold.rs.
    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let two = body.push(OpKind::Const(Attr::Int(2)), []);
    let scaled = body.push(OpKind::Mul, [x, two]);
    let offset = body.push(OpKind::Const(Attr::float(1.5)), []);
    body.push(OpKind::Add, [scaled, offset]);

    assert_eq!(
        run_f64_unary(&body, &[3.25, 0.0, -1.0]),
        vec![8.0, 1.5, -0.5]
    );
}

#[test]
fn jit_runs_float_comparison_with_ordered_predicate() {
    // `fn(x: f64) -> f64 { if x < 2.5 { 1.0 } else { -1.0 } }` -- float
    // compare feeding real branching; OLT is an *ordered* predicate, so
    // NaN takes the else branch (IEEE: all orderings false on NaN).
    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let limit = body.push(OpKind::Const(Attr::float(2.5)), []);
    let cond = body.push(OpKind::Lt, [x, limit]);
    let mut then_body = Body::new();
    then_body.push(OpKind::Const(Attr::float(1.0)), []);
    let mut else_body = Body::new();
    else_body.push(OpKind::Const(Attr::float(-1.0)), []);
    body.push_with_regions(
        OpKind::If,
        [cond],
        [Region::new(then_body), Region::new(else_body)],
    );

    assert_eq!(
        run_f64_unary(&body, &[1.0, 3.0, f64::NAN]),
        vec![1.0, -1.0, -1.0]
    );
}

#[test]
fn jit_runs_float_negation() {
    // `fn(x: f64) -> f64 { -(x * 1.5) }` -- Neg on a float value is fneg.
    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let factor = body.push(OpKind::Const(Attr::float(1.5)), []);
    let scaled = body.push(OpKind::Mul, [x, factor]);
    body.push(OpKind::Neg, [scaled]);

    assert_eq!(run_f64_unary(&body, &[2.0, -4.0]), vec![-3.0, 6.0]);
}

// ---- tuples ----------------------------------------------------------------

#[test]
fn jit_runs_tuple_build_and_extract() {
    // `fn(x) { let t = (x + 1, x * 2); t.0 + t.1 }` == 3x + 1.
    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    let two = body.push(OpKind::Const(Attr::Int(2)), []);
    let a = body.push(OpKind::Add, [x, one]);
    let b = body.push(OpKind::Mul, [x, two]);
    let tuple = body.push(OpKind::Tuple, [a, b]);
    let first = body.push(OpKind::TupleGet(0), [tuple]);
    let second = body.push(OpKind::TupleGet(1), [tuple]);
    body.push(OpKind::Add, [first, second]);

    assert_eq!(run_i64_unary(&body, &[4, 0, -2]), vec![13, 1, -5]);
}

#[test]
fn jit_runs_heterogeneous_tuple() {
    // `(x < 10, x + 1)` -- an (i1, i64) struct; project the i64 side out
    // and use the bool side as an If condition. Locks in that tuples are
    // typed per element, not forced to a homogeneous element type.
    let mut body = Body::new();
    let x = body.push(OpKind::Arg(0), []);
    let ten = body.push(OpKind::Const(Attr::Int(10)), []);
    let one = body.push(OpKind::Const(Attr::Int(1)), []);
    let flag = body.push(OpKind::Lt, [x, ten]);
    let inc = body.push(OpKind::Add, [x, one]);
    let tuple = body.push(OpKind::Tuple, [flag, inc]);
    let cond = body.push(OpKind::TupleGet(0), [tuple]);

    let mut then_body = Body::new();
    then_body.push(OpKind::Const(Attr::Int(100)), []);
    let mut else_body = Body::new();
    else_body.push(OpKind::Const(Attr::Int(200)), []);
    let selected = body.push_with_regions(
        OpKind::If,
        [cond],
        [Region::new(then_body), Region::new(else_body)],
    );
    let extracted = body.push(OpKind::TupleGet(1), [tuple]);
    body.push(OpKind::Add, [selected, extracted]);

    // x < 10: 100 + (x + 1); else 200 + (x + 1).
    assert_eq!(run_i64_unary(&body, &[4, 40]), vec![105, 241]);
}

// ---- IR-shape tests --------------------------------------------------------

#[test]
fn for_loop_lowers_to_rotated_cfg_with_select_direction_test() {
    // The structured->CFG flattening contract (module doc "Loop
    // flattening scheme"): a `cf.for` becomes cond/body/exit blocks, the
    // carried values become phis, and -- because the step is an SSA value
    // whose sign is statically unknowable -- the exit test is a `select`
    // between `iv < end` and `iv > end`.
    let body = for_sum_body(0, 10, 1);
    codira_mir::verify_body(&body).expect("hand-built test body failed the MIR verifier");
    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_shape_for");
    let builder = context.create_builder();
    let fn_type = context.i64_type().fn_type(&[], false);
    let function = module.add_function("shape_for", fn_type, None);
    builder.position_at_end(context.append_basic_block(function, "entry"));

    let result = super::lower_mir_body(&context, &builder, &module, function, &body, &[])
        .expect("expected a lowered result");
    builder.build_return(Some(&result)).unwrap();
    assert!(function.verify(true), "generated function failed to verify");

    let printed = function.print_to_string().to_string();
    for expected in [
        "mir_for_cond",
        "mir_for_body",
        "mir_for_exit",
        "mir_for_iv = phi",
        "mir_for_carry = phi",
        // With a constant step, inkwell constant-folds the `step > 0`
        // icmp to `true` while building, so the select's condition is a
        // literal here -- the select itself (both directions' compares
        // feeding it) is the shape being locked in.
        "select i1",
        "%mir_for_lt_end",
        "%mir_for_gt_end",
    ] {
        assert!(
            printed.contains(expected),
            "expected `{expected}` in lowered IR:\n{printed}"
        );
    }
}

#[test]
fn while_loop_lowers_to_rotated_cfg_with_carried_phi() {
    let mut body = Body::new();
    let init = body.push(OpKind::Const(Attr::Int(1)), []);
    let mut cond = Body::new();
    let i = cond.push(OpKind::BlockArg(0), []);
    let limit = cond.push(OpKind::Const(Attr::Int(10)), []);
    cond.push(OpKind::Lt, [i, limit]);
    let mut loop_body = Body::new();
    let i = loop_body.push(OpKind::BlockArg(0), []);
    let one = loop_body.push(OpKind::Const(Attr::Int(1)), []);
    let next = loop_body.push(OpKind::Add, [i, one]);
    loop_body.push(OpKind::Yield, [next]);
    body.push_with_regions(
        OpKind::While,
        [init],
        [Region::with_args(1, cond), Region::with_args(1, loop_body)],
    );
    codira_mir::verify_body(&body).expect("hand-built test body failed the MIR verifier");

    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_shape_while");
    let builder = context.create_builder();
    let fn_type = context.i64_type().fn_type(&[], false);
    let function = module.add_function("shape_while", fn_type, None);
    builder.position_at_end(context.append_basic_block(function, "entry"));

    let result = super::lower_mir_body(&context, &builder, &module, function, &body, &[])
        .expect("expected a lowered result");
    builder.build_return(Some(&result)).unwrap();
    assert!(function.verify(true), "generated function failed to verify");

    let printed = function.print_to_string().to_string();
    for expected in [
        "mir_while_cond",
        "mir_while_body",
        "mir_while_exit",
        "mir_while_carry = phi",
    ] {
        assert!(
            printed.contains(expected),
            "expected `{expected}` in lowered IR:\n{printed}"
        );
    }
}

// ---- honesty rules ---------------------------------------------------------

#[test]
fn honestly_refuses_string_constants() {
    // No string ABI at this layer yet -- module doc.
    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_test_str");
    let builder = context.create_builder();
    let fn_type = context.i64_type().fn_type(&[], false);
    let function = module.add_function("str_const", fn_type, None);
    builder.position_at_end(context.append_basic_block(function, "entry"));

    let mut body = Body::new();
    body.push(OpKind::Const(Attr::Str("hello".into())), []);
    codira_mir::verify_body(&body).expect("body is structurally valid");
    assert!(super::lower_mir_body(&context, &builder, &module, function, &body, &[]).is_none());
}

#[test]
fn honestly_refuses_zero_carried_value_loops() {
    // N == 0 loops result in `Unit`, which has no value representation
    // here -- refused before any block is emitted (module doc).
    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_test_unit_loop");
    let builder = context.create_builder();
    let fn_type = context.i64_type().fn_type(&[], false);
    let function = module.add_function("unit_loop", fn_type, None);
    builder.position_at_end(context.append_basic_block(function, "entry"));

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
    codira_mir::verify_body(&body).expect("body is structurally valid");
    assert!(super::lower_mir_body(&context, &builder, &module, function, &body, &[]).is_none());
}

#[test]
fn honestly_refuses_misplaced_yield_and_unknown_callee() {
    init_native_target();
    let context = Context::create();
    let module = context.create_module("mir_codegen_test_refusals");
    let builder = context.create_builder();
    let fn_type = context.i64_type().fn_type(&[], false);
    let function = module.add_function("refusals", fn_type, None);
    builder.position_at_end(context.append_basic_block(function, "entry"));

    // A top-level `cf.yield` is malformed IR: the verifier rejects it and
    // the lowering refuses it, independently.
    let mut yield_body = Body::new();
    let v = yield_body.push(OpKind::Const(Attr::Int(1)), []);
    yield_body.push(OpKind::Yield, [v]);
    assert!(codira_mir::verify_body(&yield_body).is_err());
    assert!(
        super::lower_mir_body(&context, &builder, &module, function, &yield_body, &[]).is_none()
    );

    // A `core.call` to a symbol absent from the callee table.
    let mut call_body = Body::new();
    call_body.push(OpKind::Call("missing".into()), []);
    codira_mir::verify_body(&call_body).expect("body is structurally valid");
    assert!(
        super::lower_mir_body(&context, &builder, &module, function, &call_body, &[]).is_none()
    );
}

// ---- typed casts (RFC-001 section 1.5) ----------------------------------

#[test]
fn jit_runs_truncate_and_sign_extend_roundtrip() {
    // (x as i32) as i64 -- a narrowing followed by a sign-extending
    // widening. The i32 round trip must actually discard the high bits
    // and re-fill from the sign, which is the observable difference
    // between a real cast and a no-op.
    use codira_mir::{CastKind, CastMode, TypeId};
    let mut body = Body::new();
    let x = body.push_typed(OpKind::Arg(0), [], TypeId::I64);
    let narrow = body.push_typed(
        OpKind::Cast(CastKind::Trunc, CastMode::Wrapping),
        [x],
        TypeId::I32,
    );
    body.push_typed(
        OpKind::Cast(CastKind::Sext, CastMode::Wrapping),
        [narrow],
        TypeId::I64,
    );
    codira_mir::check_types(&body).expect("well-typed cast chain");

    let inputs = [
        0i64,
        1,
        -1,
        i64::from(i32::MAX),
        i64::from(i32::MIN),
        0x1_0000_0000,
        0xFFFF_FFFF,
    ];
    let got = run_i64_unary(&body, &inputs);
    let want: Vec<i64> = inputs.iter().map(|&v| i64::from(v as i32)).collect();
    assert_eq!(got, want, "trunc+sext must match Rust's `as i32 as i64`");
}

#[test]
fn jit_runs_zero_extend_differs_from_sign_extend() {
    // The same narrowing, widened with zext instead: high bits zero.
    // If zext and sext were confused, only negative inputs would reveal
    // it -- which is exactly the class of bug the typed IR exists to
    // prevent, so it gets an explicit test.
    use codira_mir::{CastKind, CastMode, TypeId};
    let mut body = Body::new();
    let x = body.push_typed(OpKind::Arg(0), [], TypeId::I64);
    let narrow = body.push_typed(
        OpKind::Cast(CastKind::Trunc, CastMode::Wrapping),
        [x],
        TypeId::I32,
    );
    body.push_typed(
        OpKind::Cast(CastKind::Zext, CastMode::Wrapping),
        [narrow],
        TypeId::I64,
    );

    let inputs = [0i64, 1, -1, -2, i64::from(i32::MIN)];
    let got = run_i64_unary(&body, &inputs);
    let want: Vec<i64> = inputs.iter().map(|&v| i64::from(v as i32 as u32)).collect();
    assert_eq!(got, want, "trunc+zext must match `as i32 as u32 as i64`");
}

#[test]
fn jit_runs_int_float_roundtrip() {
    // x as f64 as i64 -- truncation toward zero through the float domain.
    use codira_mir::{CastKind, CastMode, TypeId};
    let mut body = Body::new();
    let x = body.push_typed(OpKind::Arg(0), [], TypeId::I64);
    let as_float = body.push_typed(
        OpKind::Cast(CastKind::SiToFp, CastMode::Wrapping),
        [x],
        TypeId::F64,
    );
    let half = body.push_typed(OpKind::Const(Attr::float(2.0)), [], TypeId::F64);
    let divided = body.push_typed(OpKind::Div, [as_float, half], TypeId::F64);
    body.push_typed(
        OpKind::Cast(CastKind::FpToSi, CastMode::Wrapping),
        [divided],
        TypeId::I64,
    );
    codira_mir::check_types(&body).expect("well-typed int/float chain");

    let inputs = [0i64, 1, 2, 3, 7, -7, 100];
    let got = run_i64_unary(&body, &inputs);
    let want: Vec<i64> = inputs.iter().map(|&v| (v as f64 / 2.0) as i64).collect();
    assert_eq!(
        got, want,
        "sitofp / fdiv / fptosi must match the f64 round trip"
    );
}

#[test]
fn checked_casts_are_refused_until_the_obligation_is_discharged() {
    // A Checked cast carries an undischarged proof obligation. Lowering
    // it as a silent truncation would be precisely the miscompile the
    // mode exists to prevent, so codegen must decline.
    use codira_mir::{CastKind, CastMode, TypeId};
    let mut body = Body::new();
    let x = body.push_typed(OpKind::Arg(0), [], TypeId::I64);
    body.push_typed(
        OpKind::Cast(CastKind::Trunc, CastMode::Checked),
        [x],
        TypeId::U8,
    );
    codira_mir::verify_body(&body).expect("structurally valid");

    init_native_target();
    let context = Context::create();
    let module = context.create_module("checked_cast_test");
    let builder = context.create_builder();
    let i64_ty = context.i64_type();
    let fn_type = i64_ty.fn_type(&[i64_ty.into()], false);
    let function = module.add_function("test_fn", fn_type, None);
    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);
    let arg = function.get_nth_param(0).unwrap();

    assert!(
        super::lower_mir_body(&context, &builder, &module, function, &body, &[arg]).is_none(),
        "a Checked cast must be refused, not silently truncated"
    );
}
