use codira_hir_input::WithFixture;
use codira_mir::{CastKind, CastMode, OpKind, TypeId};

use crate::{
    mir_lower::lower_function_to_generator, mock::MockDatabase, HirDatabase, ModuleDef, Package,
};

/// Parses `content`, finds the first function declared in it, and lowers
/// it to a `codira_mir::Generator`. Panics (via the trailing `.unwrap()`
/// left to callers) if there's no function or lowering bails -- tests
/// below assert on the `Option` directly where a `None` is expected.
fn lower_first_fn(content: &str) -> Option<codira_mir::Generator> {
    let db = MockDatabase::with_files(content);
    let func = Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.declarations(&db))
        .find_map(|item| match item {
            ModuleDef::Function(f) => Some(f),
            _ => None,
        })
        .expect("no function found in source");
    lower_function_to_generator(&db, func)
}

#[test]
fn lowers_generic_param_reference() {
    // `func add[N](x: i64) -> i64 { x + N }`
    let generator = lower_first_fn("func add[N](x: i64) -> i64 { x + N }")
        .expect("expected a lowerable generator");

    assert_eq!(generator.name, "add");
    assert_eq!(generator.params.len(), 1);
    assert_eq!(generator.params[0].name, "N");

    // The body should be exactly one Add op referencing an Arg and a
    // ParamRef -- i.e. real, not just "didn't crash".
    let result = generator.body.result().expect("body has a result");
    let op = generator.body.get(result);
    assert_eq!(op.kind, OpKind::Add);
    assert_eq!(op.operands.len(), 2);

    let lhs_kind = &generator.body.get(op.operands[0]).kind;
    let rhs_kind = &generator.body.get(op.operands[1]).kind;
    assert_eq!(*lhs_kind, OpKind::Arg(0));
    assert_eq!(*rhs_kind, OpKind::ParamRef("N".into()));
}

#[test]
fn lowers_let_bindings_and_if() {
    // `func clamp_low[Lo](x: i64) -> i64 { let y = x; if y < Lo { Lo } else { y }
    // }`
    let generator = lower_first_fn(
        "func clamp_low[Lo](x: i64) -> i64 { let y = x; if y < Lo { Lo } else { y } }",
    )
    .expect("expected a lowerable generator");

    let result = generator.body.result().expect("body has a result");
    let op = generator.body.get(result);
    assert_eq!(op.kind, OpKind::If);
    assert_eq!(op.regions.len(), 2);
    // `then` branch is just `Lo` -> a single ParamRef op.
    let then_result = op.regions[0].body.result().expect("then has a result");
    assert_eq!(
        op.regions[0].body.get(then_result).kind,
        OpKind::ParamRef("Lo".into())
    );
    // `else` branch is `y`, which was let-bound to `x` (Arg(0)).
    let else_result = op.regions[1].body.result().expect("else has a result");
    assert_eq!(op.regions[1].body.get(else_result).kind, OpKind::Arg(0));
}

#[test]
fn lowers_bitwise_and_shift_ops() {
    // `func mask(x: i64) -> i64 { (x & 255) << 1 }` -- the bitwise/shift
    // operators now map to real codira_mir ops instead of bailing.
    let generator = lower_first_fn("func mask(x: i64) -> i64 { (x & 255) << 1 }")
        .expect("expected a lowerable generator");
    codira_mir::verify_body(&generator.body).unwrap();

    let result = generator.body.result().expect("body has a result");
    let shl = generator.body.get(result);
    assert_eq!(shl.kind, OpKind::Shl);
    let and = generator.body.get(shl.operands[0]);
    assert_eq!(and.kind, OpKind::BitAnd);
    assert_eq!(generator.body.get(and.operands[0]).kind, OpKind::Arg(0));
    assert_eq!(
        generator.body.get(and.operands[1]).kind,
        OpKind::Const(codira_mir::Attr::Int(255))
    );
}

/// Finds the function named `name` in `content` and lowers it.
fn lower_named_fn(content: &str, name: &str) -> Option<codira_mir::Generator> {
    let db = MockDatabase::with_files(content);
    let func = Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.declarations(&db))
        .find_map(|item| match item {
            ModuleDef::Function(f) if f.name(&db).to_string() == name => Some(f),
            _ => None,
        })
        .expect("function not found");
    lower_function_to_generator(&db, func)
}

#[test]
fn lowers_function_calls_to_core_call() {
    // `helper()` lowers to `core.call @helper` -- the symbol is the
    // callee's plain name (matching `Generator::name`), and resolution is
    // deferred to a `GeneratorStore` per `OpKind::Call`'s doc.
    let source = r#"
        func helper(x: i64) -> i64 { x + 1 }
        func uses_helper() -> i64 { helper(41) }
        "#;
    let generator = lower_named_fn(source, "uses_helper").expect("expected a lowerable generator");
    codira_mir::verify_body(&generator.body).unwrap();

    let result = generator.body.result().expect("body has a result");
    let call = generator.body.get(result);
    assert_eq!(call.kind, OpKind::Call("helper".into()));
    assert_eq!(call.operands.len(), 1);
    assert_eq!(
        generator.body.get(call.operands[0]).kind,
        OpKind::Const(codira_mir::Attr::Int(41))
    );

    // End to end: put both lowered generators in a store and let the
    // comptime interpreter resolve the call -- the whole point of
    // matching the symbol to `Generator::name`.
    let helper = lower_named_fn(source, "helper").expect("helper should lower");
    let mut store = codira_mir::GeneratorStore::new();
    store.add_generator(helper);
    assert_eq!(
        codira_comptime::eval_body_with(
            &generator.body,
            &codira_comptime::Env::new(),
            Some(&store)
        )
        .unwrap(),
        codira_comptime::Value::Int(42)
    );
}

#[test]
fn bails_out_on_method_calls() {
    // A method call has no bare-symbol callee -- must stay `None`, not
    // guess at a mangled name.
    let generator = lower_named_fn(
        r#"
        struct S {}
        impl S { pub func m(self) -> i64 { 1 } }
        func uses_method(s: S) -> i64 { s.m() }
        "#,
        "uses_method",
    );
    assert!(generator.is_none());
}

#[test]
fn bails_out_on_calling_a_parameter() {
    // `x(1)` where `x` is a runtime parameter is a call through a value,
    // not a symbol reference -- must bail rather than emit
    // `core.call @x`.
    let generator = lower_named_fn("func apply(x: i64) -> i64 { x(1) }", "apply");
    assert!(generator.is_none());
}

// ---------------------------------------------------------------------------
// Salsa query tests: HirDatabase::mir_generator / elaborate_generator
// ---------------------------------------------------------------------------

#[test]
fn elaborate_generator_query_produces_correct_result() {
    // `func double[N]() -> i64 { N * 2 }`, elaborated with N=21 via the
    // salsa query (not the bare `codira_comptime::elaborate` function
    // directly) should fully constant-fold to 42, same as the
    // hand-built-Generator tests in codira_comptime itself -- this is the
    // proof the query wiring doesn't change the answer.
    let db = MockDatabase::with_files("func double[N]() -> i64 { N * 2 }");
    let func = Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.declarations(&db))
        .find_map(|item| match item {
            ModuleDef::Function(f) => Some(f),
            _ => None,
        })
        .expect("no function found");

    let bindings = vec![("N".into(), codira_comptime::Value::Int(21))];
    let body = db
        .elaborate_generator(func, bindings)
        .expect("expected an elaborated body");

    assert_eq!(
        codira_comptime::eval_body(&body, &codira_comptime::Env::new()).unwrap(),
        codira_comptime::Value::Int(42)
    );
}

#[test]
fn elaborate_generator_query_is_memoized() {
    // The concrete "incremental caching" claim from architecture doc
    // §4.1: calling the salsa query twice with identical inputs must
    // return the *same* Arc allocation (a cache hit), not recompute and
    // allocate a fresh one.
    let db = MockDatabase::with_files("func double[N]() -> i64 { N * 2 }");
    let func = Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.declarations(&db))
        .find_map(|item| match item {
            ModuleDef::Function(f) => Some(f),
            _ => None,
        })
        .expect("no function found");

    let bindings = vec![("N".into(), codira_comptime::Value::Int(21))];
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
fn elaborate_generator_query_distinguishes_different_bindings() {
    // A different cache key (different N) must produce a genuinely
    // different, correctly-computed result -- not an incorrectly reused
    // cache entry from a different specialization.
    let db = MockDatabase::with_files("func double[N]() -> i64 { N * 2 }");
    let func = Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.declarations(&db))
        .find_map(|item| match item {
            ModuleDef::Function(f) => Some(f),
            _ => None,
        })
        .expect("no function found");

    let body_21 = db
        .elaborate_generator(func, vec![("N".into(), codira_comptime::Value::Int(21))])
        .unwrap();
    let body_5 = db
        .elaborate_generator(func, vec![("N".into(), codira_comptime::Value::Int(5))])
        .unwrap();

    assert_eq!(
        codira_comptime::eval_body(&body_21, &codira_comptime::Env::new()).unwrap(),
        codira_comptime::Value::Int(42)
    );
    assert_eq!(
        codira_comptime::eval_body(&body_5, &codira_comptime::Env::new()).unwrap(),
        codira_comptime::Value::Int(10)
    );
}

// ---------------------------------------------------------------------------
// `expr as Type` (S6 stage 3): HIR casts -> typed Eidos MIR
// ---------------------------------------------------------------------------

/// Lowers `content`'s first function and holds it to *both* `codira_mir`
/// contracts: the structural verifier and the RFC-001 type checker. Every
/// cast test goes through here, so "the lowering produces a checkable body"
/// is asserted once and cannot be forgotten in a new case.
fn lower_checked(content: &str) -> codira_mir::Generator {
    let generator = lower_first_fn(content).expect("expected a lowerable generator");
    codira_mir::verify_body(&generator.body).expect("lowered body must verify");
    codira_mir::check_types(&generator.body).expect("lowered body must type-check");
    generator
}

/// Asserts the body's result op is a `core.cast` and returns everything the
/// matrix is about: `(kind, mode, operand type, result type)`.
fn cast_op(content: &str) -> (CastKind, CastMode, TypeId, TypeId) {
    let generator = lower_checked(content);
    let result = generator.body.result().expect("body has a result");
    let op = generator.body.get(result);
    let OpKind::Cast(kind, mode) = op.kind else {
        panic!("expected the body to end in a core.cast, got {:?}", op.kind);
    };
    assert_eq!(op.operands.len(), 1, "core.cast takes exactly one operand");
    (kind, mode, generator.body.get(op.operands[0]).ty, op.ty)
}

/// True if `body` contains a `core.cast` op anywhere, regions included.
fn has_cast(body: &codira_mir::Body) -> bool {
    body.iter().any(|(_, op)| {
        matches!(op.kind, OpKind::Cast(..))
            || op.regions.iter().any(|region| has_cast(&region.body))
    })
}

#[test]
fn int_narrowing_cast_emits_trunc() {
    assert_eq!(
        cast_op("func narrow(x: i64) -> i32 { x as i32 }"),
        (
            CastKind::Trunc,
            CastMode::Wrapping,
            TypeId::I64,
            TypeId::I32
        )
    );
    // Narrowing truncates regardless of signedness on either side.
    assert_eq!(
        cast_op("func narrow(x: u64) -> u8 { x as u8 }"),
        (CastKind::Trunc, CastMode::Wrapping, TypeId::U64, TypeId::U8)
    );
}

#[test]
fn signed_widening_cast_emits_sext() {
    assert_eq!(
        cast_op("func widen(x: i32) -> i64 { x as i64 }"),
        (CastKind::Sext, CastMode::Wrapping, TypeId::I32, TypeId::I64)
    );
}

#[test]
fn unsigned_widening_cast_emits_zext_not_sext() {
    // THE classic cast bug: the extension is chosen by the **source**'s
    // signedness, not the target's. `u32 as i64` must zero-extend -- if it
    // sign-extended, `0xFFFF_FFFF as u32 as i64` would be -1 instead of
    // 4294967295.
    let (kind, mode, source, target) = cast_op("func widen(x: u32) -> i64 { x as i64 }");
    assert_eq!(kind, CastKind::Zext, "u32 -> i64 must zero-extend");
    assert_ne!(kind, CastKind::Sext);
    assert_eq!(
        (mode, source, target),
        (CastMode::Wrapping, TypeId::U32, TypeId::I64)
    );

    // And the mirror image: a *signed* source widening into an *unsigned*
    // target still sign-extends.
    let (kind, ..) = cast_op("func widen(x: i32) -> u64 { x as u64 }");
    assert_eq!(kind, CastKind::Sext, "i32 -> u64 must sign-extend");
}

#[test]
fn signed_int_to_float_emits_sitofp() {
    assert_eq!(
        cast_op("func to_float(x: i32) -> f64 { x as f64 }"),
        (
            CastKind::SiToFp,
            CastMode::Wrapping,
            TypeId::I32,
            TypeId::F64
        )
    );
}

#[test]
fn unsigned_int_to_float_emits_uitofp() {
    let (kind, mode, source, target) = cast_op("func to_float(x: u32) -> f64 { x as f64 }");
    assert_eq!(
        kind,
        CastKind::UiToFp,
        "u32 -> f64 must use the unsigned conversion"
    );
    assert_ne!(kind, CastKind::SiToFp);
    assert_eq!(
        (mode, source, target),
        (CastMode::Wrapping, TypeId::U32, TypeId::F64)
    );
}

#[test]
fn float_to_int_picks_signedness_from_the_target() {
    assert_eq!(
        cast_op("func to_int(x: f64) -> i32 { x as i32 }"),
        (
            CastKind::FpToSi,
            CastMode::Wrapping,
            TypeId::F64,
            TypeId::I32
        )
    );
    // Float -> int is the one direction where the *target*'s signedness
    // decides, not the source's (a float has none).
    assert_eq!(
        cast_op("func to_int(x: f64) -> u32 { x as u32 }"),
        (
            CastKind::FpToUi,
            CastMode::Wrapping,
            TypeId::F64,
            TypeId::U32
        )
    );
}

#[test]
fn float_narrowing_emits_fptrunc() {
    assert_eq!(
        cast_op("func narrow(x: f64) -> f32 { x as f32 }"),
        (
            CastKind::FpTrunc,
            CastMode::Wrapping,
            TypeId::F64,
            TypeId::F32
        )
    );
}

#[test]
fn float_widening_emits_fpext() {
    assert_eq!(
        cast_op("func widen(x: f32) -> f64 { x as f64 }"),
        (
            CastKind::FpExt,
            CastMode::Wrapping,
            TypeId::F32,
            TypeId::F64
        )
    );
}

#[test]
fn identity_cast_emits_no_cast_op() {
    // `x as i64` where `x: i64` is not "a cast that happens to be cheap" --
    // it is the operand, full stop. Emitting a no-op cast would leave the
    // e-graph and every later pass with a node to see through.
    let generator = lower_checked("func ident(x: i64) -> i64 { x as i64 }");
    assert!(
        !has_cast(&generator.body),
        "an identity cast must emit no op at all, got: {}",
        codira_mir::print_body(&generator.body)
    );

    let result = generator.body.result().expect("body has a result");
    let op = generator.body.get(result);
    assert_eq!(
        op.kind,
        OpKind::Arg(0),
        "the cast must lower to its operand"
    );
    assert_eq!(op.ty, TypeId::I64);
    assert_eq!(generator.body.len(), 1, "exactly one op: the operand");
}

#[test]
fn same_width_signedness_change_emits_nothing() {
    // Neither a narrowing nor a widening, and *not* a Bitcast either: an
    // LLVM integer type carries no signedness, so `i32` and `u32` are the
    // same machine type. Signedness lives in the HIR `Ty` and in the
    // choice of operations (sext vs zext, sdiv vs udiv). Emitting a
    // bitcast would be a no-op instruction and a third Int->Int operation
    // that spec/LANGUAGE_SPEC.md section 18.5 does not list.
    let generator = lower_checked("func reinterpret(x: i32) -> u32 { x as u32 }");
    assert_eq!(
        generator.body.len(),
        1,
        "exactly one op: the operand, with no conversion emitted"
    );
}

#[test]
fn chained_casts_nest_left_to_right() {
    // `x as i32 as i64` is `((x as i32) as i64)`: a Trunc feeding a Sext,
    // *not* a single i64 -> i64 identity. The intermediate narrowing is
    // observable (it discards the high bits), so it must survive.
    let generator = lower_checked("func chain(x: i64) -> i64 { x as i32 as i64 }");

    let result = generator.body.result().expect("body has a result");
    let outer = generator.body.get(result);
    assert_eq!(outer.kind, OpKind::Cast(CastKind::Sext, CastMode::Wrapping));
    assert_eq!(outer.ty, TypeId::I64);

    let inner = generator.body.get(outer.operands[0]);
    assert_eq!(
        inner.kind,
        OpKind::Cast(CastKind::Trunc, CastMode::Wrapping)
    );
    assert_eq!(inner.ty, TypeId::I32);
    assert_eq!(generator.body.get(inner.operands[0]).kind, OpKind::Arg(0));
}

#[test]
fn a_cast_can_feed_an_arithmetic_op_and_still_type_check() {
    // The point of typing the ops at all: `check_types` now enforces that
    // both operands of the `Add` agree with its result. Without the cast
    // this body would be an i32 added to an i64 and nothing would notice.
    let generator = lower_checked("func mix(a: i64, b: i32) -> i64 { a + b as i64 }");

    let result = generator.body.result().expect("body has a result");
    let add = generator.body.get(result);
    assert_eq!(add.kind, OpKind::Add);
    assert_eq!(add.ty, TypeId::I64);
    assert_eq!(generator.body.get(add.operands[0]).ty, TypeId::I64);

    let cast = generator.body.get(add.operands[1]);
    assert_eq!(cast.kind, OpKind::Cast(CastKind::Sext, CastMode::Wrapping));
    assert_eq!(cast.ty, TypeId::I64);
}

#[test]
fn casts_inside_if_branches_lower_and_check() {
    // Regions are checked too (`check_types` recurses), so a cast in a
    // branch is held to the same contract as one at the top level.
    let generator =
        lower_checked("func pick(c: bool, a: i32, b: i64) -> i64 { if c { a as i64 } else { b } }");

    let result = generator.body.result().expect("body has a result");
    let if_op = generator.body.get(result);
    assert_eq!(if_op.kind, OpKind::If);
    assert_eq!(if_op.ty, TypeId::I64);
    assert_eq!(generator.body.get(if_op.operands[0]).ty, TypeId::BOOL);

    let then_body = &if_op.regions[0].body;
    let then_result = then_body.result().expect("then has a result");
    assert_eq!(
        then_body.get(then_result).kind,
        OpKind::Cast(CastKind::Sext, CastMode::Wrapping)
    );
    assert_eq!(then_body.get(then_result).ty, TypeId::I64);
}

#[test]
fn a_cast_through_a_let_binding_lowers() {
    // Locals are re-lowered from their initializer, so the cast has to be
    // re-derived (and re-typed) into whichever body needs it.
    let generator = lower_checked("func via_let(x: i32) -> i64 { let y = x as i64; y }");
    let result = generator.body.result().expect("body has a result");
    let op = generator.body.get(result);
    assert_eq!(op.kind, OpKind::Cast(CastKind::Sext, CastMode::Wrapping));
    assert_eq!(op.ty, TypeId::I64);
}

#[test]
fn illegal_casts_decline_rather_than_guess() {
    // Each of these is a type error inference has already reported. There
    // is no operation that means them, so there is nothing to emit -- and
    // emitting *something* would turn a diagnosed error into wrong code.
    assert!(
        lower_first_fn("func to_bool(x: i32) -> bool { x as bool }").is_none(),
        "integer -> bool is illegal; use a comparison"
    );
    assert!(
        lower_first_fn("func b_to_f(b: bool) -> f64 { b as f64 }").is_none(),
        "bool only converts to integers"
    );
    assert!(
        lower_first_fn("struct S { x: i32 } func s_cast(s: S) -> i32 { s as i32 }").is_none(),
        "aggregates are not castable with `as`"
    );
}

#[test]
fn bool_to_int_declines_pending_a_codira_mir_rule() {
    // KNOWN GAP, deliberately a decline rather than a silent hole. The
    // language spec says `bool as Int` is a zero-extension, and
    // `ty::cast::check_cast` agrees (`CastOp::Convert(Zext, Wrapping)`).
    // But `codira_mir::verify::check_types` admits only `as_int()` operands
    // for `Zext`/`Sext`, and `TypeId::BOOL` is not an integer type, so a
    // correctly-typed `bool -> i32` zext would fail the very checker this
    // pass exists to satisfy. Rather than emit it with the operand left
    // untyped -- which would pass only by hiding from the checker -- this
    // case declines until `check_types`' extension arm accepts a `BOOL`
    // source. This test is the tripwire: when that rule lands it fails, and
    // the guard in `lower_expr`'s `Expr::Cast` arm comes out.
    assert!(lower_first_fn("func flag(b: bool) -> i32 { b as i32 }").is_none());
}

// ---------------------------------------------------------------------------
// RFC-001 phase 2: the ops this pass emits carry their types
// ---------------------------------------------------------------------------

#[test]
fn lowered_ops_carry_their_types() {
    // The phase-2 migration metric (`Body::untyped_count`): a body built
    // only from concretely-typed HIR must come out fully typed.
    let generator = lower_checked("func typed(x: i64, y: i64) -> bool { x + 1 < y }");
    assert_eq!(
        generator.body.untyped_count(),
        0,
        "every op in a concretely-typed body should carry a type: {}",
        codira_mir::print_body(&generator.body)
    );

    let result = generator.body.result().expect("body has a result");
    let lt = generator.body.get(result);
    assert_eq!(
        lt.ty,
        TypeId::BOOL,
        "a comparison is bool, not its operands' type"
    );
    let add = generator.body.get(lt.operands[0]);
    assert_eq!(add.ty, TypeId::I64);
    // The literal is unified with `x`, so it is an i64 const, not an i32.
    assert_eq!(generator.body.get(add.operands[1]).ty, TypeId::I64);
}

#[test]
fn float_ops_carry_their_types() {
    let generator = lower_checked("func f(x: f32, y: f32) -> f32 { x * y }");
    let result = generator.body.result().expect("body has a result");
    assert_eq!(generator.body.get(result).ty, TypeId::F32);
    assert_eq!(generator.body.untyped_count(), 0);
}

#[test]
fn untypable_hir_types_stay_untyped_rather_than_guessing() {
    // A generic parameter has no settled type until elaboration, so
    // `param.ref` must stay `UNTYPED` -- and the enclosing `Add` must still
    // type-check, because `check_types` skips untyped operands.
    let generator = lower_checked("func add[N](x: i64) -> i64 { x + N }");
    let result = generator.body.result().expect("body has a result");
    let add = generator.body.get(result);
    assert_eq!(generator.body.get(add.operands[0]).ty, TypeId::I64);
    assert!(
        generator.body.get(add.operands[1]).ty.is_untyped(),
        "a generic parameter's type is not known here; it must not be invented"
    );
}

#[test]
fn every_previously_lowering_body_still_type_checks() {
    // Typing the pre-existing ops must not make any body that used to lower
    // fail the new checker -- a regression net over the pre-S6 subset.
    for source in [
        "func mask(x: i64) -> i64 { (x & 255) << 1 }",
        "func clamp_low[Lo](x: i64) -> i64 { let y = x; if y < Lo { Lo } else { y } }",
        "func neg(x: f64) -> f64 { -x }",
        "func add[N](x: i64) -> i64 { x + N }",
        "func lit() -> i64 { 1 + 2 }",
    ] {
        lower_checked(source);
    }
}
