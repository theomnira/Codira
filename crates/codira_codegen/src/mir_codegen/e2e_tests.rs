//! End-to-end proof (`spec/KGEN_SUPERSET_STATUS.md` roadmap item 6 /
//! session-2 item #20): real Codira source, parsed and lowered through the
//! actual `codira_hir` salsa database -- not a hand-built `codira_mir::Body`
//! like `super::tests` uses -- specialized via the `elaborate_generator`
//! incremental query, lowered to LLVM IR by `lower_mir_body`, JIT-compiled,
//! and executed. This is the full pipeline the architecture doc promises:
//! `codira_hir::mir_lower` -> `codira_comptime::elaborate` (via the salsa
//! query) -> `codira_codegen::mir_codegen` -> native code.

#![allow(clippy::items_after_statements)]
//! Lint note: each test declares its own `type TestFn = ...` alias next to
//! the JIT call that uses it. Hoisting those to module scope would put a
//! dozen near-identical aliases far from their single use site.

use codira_hir::{HirDatabase, ModuleDef, Package};
use inkwell::{
    context::Context,
    targets::{InitializationConfig, Target},
    OptimizationLevel,
};

use crate::mock::MockDatabase;

fn init_native_target() {
    Target::initialize_native(&InitializationConfig::default())
        .expect("failed to initialize native target for JIT");
}

#[test]
fn end_to_end_specialize_elaborate_codegen_jit_run() {
    // `func double[N]() -> i64 { N * 2 }`, specialized with N = 21.
    init_native_target();

    let (db, _file_id) = MockDatabase::with_single_file("func double[N]() -> i64 { N * 2 }");
    let func = Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.declarations(&db))
        .find_map(|item| match item {
            ModuleDef::Function(f) => Some(f),
            _ => None,
        })
        .expect("no function found in source");

    let bindings = vec![("N".into(), codira_comptime::Value::Int(21))];
    let elaborated = db
        .elaborate_generator(func, bindings)
        .expect("expected an elaborated body");

    // Elaboration should have folded `N * 2` all the way down to `42` --
    // same claim `codira_hir::mir_lower::tests` verifies via
    // `codira_comptime::eval_body` directly; here it's verified by actually
    // running the compiled machine code instead.
    let context = Context::create();
    let module = context.create_module("e2e_test");
    let builder = context.create_builder();

    let i64_ty = context.i64_type();
    let fn_type = i64_ty.fn_type(&[], false);
    let function = module.add_function("double_21", fn_type, None);
    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);

    let result = super::lower_mir_body(&context, &builder, &module, function, &elaborated, &[])
        .expect("expected a lowered result");
    builder.build_return(Some(&result)).unwrap();

    assert!(function.verify(true), "generated function failed to verify");

    let engine = module
        .create_jit_execution_engine(OptimizationLevel::None)
        .expect("failed to create JIT execution engine");

    type DoubleTwentyOne = unsafe extern "C" fn() -> i64;
    let double_21: inkwell::execution_engine::JitFunction<'_, DoubleTwentyOne> =
        unsafe { engine.get_function("double_21") }.expect("function not found in JIT module");

    assert_eq!(unsafe { double_21.call() }, 42);
}

#[test]
fn end_to_end_is_memoized_across_codegen_calls() {
    // The salsa memoization proof (`codira_hir::mir_lower::tests::
    // elaborate_generator_query_is_memoized`) from the codegen consumer's
    // point of view: two calls into the query with identical `(func,
    // bindings)` from code that looks exactly like what a real codegen
    // driver would do must hit the cache, not recompute.
    let (db, _file_id) = MockDatabase::with_single_file("func triple[N]() -> i64 { N * 3 }");
    let func = Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.declarations(&db))
        .find_map(|item| match item {
            ModuleDef::Function(f) => Some(f),
            _ => None,
        })
        .expect("no function found in source");

    let bindings = vec![("N".into(), codira_comptime::Value::Int(7))];
    let first = db
        .elaborate_generator(func, bindings.clone())
        .expect("expected an elaborated body");
    let second = db
        .elaborate_generator(func, bindings)
        .expect("expected an elaborated body");

    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "salsa should have served the second call from cache instead of recomputing"
    );
}

#[test]
fn end_to_end_call_between_two_lowered_functions() {
    // The multi-function execution harness for `core.call`: two MIR
    // bodies lowered into the *same* LLVM module, where `main` calls
    // `helper` through `lower_mir_body_with_callees`'s symbol table (the
    // codegen-side counterpart of `codira_mir::GeneratorStore::
    // generator_by_name` -- KGEN's `#kgen.genref` resolution). The bodies
    // are hand-built here because the HIR-side lowering of calls is
    // concurrent work; what this locks in is the codegen contract: the
    // call's operands become the callee's `Arg`s, and the callee's return
    // value is the call op's value.
    use codira_mir::{Attr, Body, OpKind};
    use rustc_hash::FxHashMap;

    init_native_target();
    let context = Context::create();
    let module = context.create_module("e2e_call_test");
    let builder = context.create_builder();
    let i64_ty = context.i64_type();
    let fn_type = i64_ty.fn_type(&[i64_ty.into()], false);

    // `func helper(x: i64) -> i64 { x * 2 + 1 }`
    let helper_fn = module.add_function("helper", fn_type, None);
    builder.position_at_end(context.append_basic_block(helper_fn, "entry"));
    let mut helper_body = Body::new();
    let x = helper_body.push(OpKind::Arg(0), []);
    let two = helper_body.push(OpKind::Const(Attr::Int(2)), []);
    let one = helper_body.push(OpKind::Const(Attr::Int(1)), []);
    let doubled = helper_body.push(OpKind::Mul, [x, two]);
    helper_body.push(OpKind::Add, [doubled, one]);
    codira_mir::verify_body(&helper_body).expect("helper body failed the MIR verifier");
    let helper_arg = helper_fn.get_nth_param(0).unwrap();
    let helper_result = super::lower_mir_body(
        &context,
        &builder,
        &module,
        helper_fn,
        &helper_body,
        &[helper_arg],
    )
    .expect("expected a lowered helper result");
    builder.build_return(Some(&helper_result)).unwrap();
    assert!(helper_fn.verify(true), "helper failed to verify");

    // `func main(x: i64) -> i64 { helper(x + 3) }`
    let main_fn = module.add_function("call_main", fn_type, None);
    builder.position_at_end(context.append_basic_block(main_fn, "entry"));
    let mut main_body = Body::new();
    let x = main_body.push(OpKind::Arg(0), []);
    let three = main_body.push(OpKind::Const(Attr::Int(3)), []);
    let shifted = main_body.push(OpKind::Add, [x, three]);
    main_body.push(OpKind::Call("helper".into()), [shifted]);
    codira_mir::verify_body(&main_body).expect("main body failed the MIR verifier");

    let mut callees = FxHashMap::default();
    callees.insert(smol_str::SmolStr::new("helper"), helper_fn);
    let main_arg = main_fn.get_nth_param(0).unwrap();
    let main_result = super::lower_mir_body_with_callees(
        &context,
        &builder,
        &module,
        main_fn,
        &main_body,
        &[main_arg],
        &callees,
    )
    .expect("expected a lowered main result");
    builder.build_return(Some(&main_result)).unwrap();
    assert!(main_fn.verify(true), "main failed to verify");

    // Through the *original* entry point (no callee table), the same body
    // must honestly refuse rather than guess at the symbol.
    let orphan_fn = module.add_function("orphan", fn_type, None);
    builder.position_at_end(context.append_basic_block(orphan_fn, "entry"));
    let orphan_arg = orphan_fn.get_nth_param(0).unwrap();
    assert!(super::lower_mir_body(
        &context,
        &builder,
        &module,
        orphan_fn,
        &main_body,
        &[orphan_arg]
    )
    .is_none());
    // Terminate the refused function's entry block so the module as a
    // whole stays JIT-compilable (a refusal leaves the block open by
    // design -- the caller owns the recovery).
    builder.build_return(Some(&orphan_arg)).unwrap();

    let engine = module
        .create_jit_execution_engine(OptimizationLevel::None)
        .expect("failed to create JIT execution engine");
    type CallMain = unsafe extern "C" fn(i64) -> i64;
    let call_main: inkwell::execution_engine::JitFunction<'_, CallMain> =
        unsafe { engine.get_function("call_main") }.expect("function not found in JIT module");

    // helper(x + 3) = (x + 3) * 2 + 1
    assert_eq!(unsafe { call_main.call(4) }, 15);
    assert_eq!(unsafe { call_main.call(-3) }, 1);
}
