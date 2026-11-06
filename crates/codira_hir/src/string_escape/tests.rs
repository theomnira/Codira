//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Tests of the `String` escape analysis: which format plans of a body may
//!   be kept past the call.

use codira_hir_input::WithFixture;

use crate::{mock::MockDatabase, Expr, Function, HirDatabase, ModuleDef, Package};

/// The function named `name` in the single-file fixture loaded into `db`.
fn function(db: &MockDatabase, name: &str) -> Function {
    Package::all(db)
        .iter()
        .flat_map(|pkg| pkg.modules(db))
        .flat_map(|module| module.declarations(db))
        .find_map(|def| match def {
            ModuleDef::Function(f) if f.name(db).to_string() == name => Some(f),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}` in fixture"))
}

/// Whether each format plan (`f"..."` literal, as a `+` chain or a lone
/// hole) of `name`'s body is kept, in source order.
fn kept_plans(source: &str, name: &str) -> Vec<bool> {
    let (db, _) = MockDatabase::with_single_file(source);
    let func = function(&db, name);
    let body = func.body(&db);
    let infer = db.infer(func.id.into());
    let mut plans: Vec<_> = body
        .exprs()
        .filter(|(id, expr)| {
            infer[*id].is_string()
                && matches!(
                    expr,
                    Expr::BinaryOp {
                        op: Some(crate::BinaryOp::ArithOp(crate::ArithOp::Add)),
                        ..
                    }
                )
                // The outermost `+` of a chain only.
                && !body.exprs().any(|(_, parent)| {
                    matches!(parent, Expr::BinaryOp { lhs, op: Some(crate::BinaryOp::ArithOp(crate::ArithOp::Add)), .. } if lhs == id)
                })
        })
        .map(|(id, _)| id)
        .collect();
    plans.sort_by_key(|id| id.into_raw());
    plans
        .into_iter()
        .map(|id| super::is_kept(&db, func, id))
        .collect()
}

#[test]
fn a_returned_or_stored_plan_is_kept_and_a_read_one_is_not() {
    let source = r#"
class Entry { key: String }
func label(i: i64) -> String {
    let probe = f"probe {i}"
    println(probe.len())
    f"item-{i}"
}
func store(i: i64, xs: [String]) -> Entry {
    let a = f"a{i}"
    if a == f"b{i}" { println(a) }
    xs.push(f"c{i}")
    Entry { key: f"d{i}" }
}
"#;
    assert_eq!(kept_plans(source, "label"), [false, true]);
    assert_eq!(kept_plans(source, "store"), [false, false, true, true]);
}

#[test]
fn a_local_is_kept_when_any_use_keeps_it() {
    let source = r#"
class Entry { key: String }
func flows(i: i64, e: Entry) -> i64 {
    let a = f"a{i}"
    var b = f"b{i}"
    b = a
    let c = f"c{i}"
    let d = c.trim()
    e.key = d
    let t = f"t{i}"
    let u = t + "!"
    println(u)
    b.len() as i64
}
"#;
    // `a` flows into `b`, which is only read; `c`'s view `d` is stored; `t`
    // is the first piece of `u`, which is only printed.
    assert_eq!(kept_plans(source, "flows"), [false, false, true, false, false]);
}

#[test]
fn an_argument_is_kept_when_the_callee_keeps_its_parameter() {
    let source = r#"
class Entry { key: String }
func check(s: String) -> usize { s.len() }
func keep(s: String) -> Entry { Entry { key: s } }
func pass_on(s: String) -> usize { check(s) + keep(s).key.len() }
func countdown(s: String, n: i64) -> usize {
    if n == 0 { s.len() } else { countdown(s, n - 1) }
}
func caller(i: i64) -> usize {
    let n = check(f"x{i}")
    let k = keep(f"y{i}")
    let p = pass_on(f"z{i}")
    n + p + countdown(f"w{i}", 3) + k.key.len()
}
"#;
    assert_eq!(kept_plans(source, "caller"), [false, true, true, false]);
    let (db, _) = MockDatabase::with_single_file(source);
    assert!(!super::param_is_kept(&db, function(&db, "check"), 0));
    assert!(super::param_is_kept(&db, function(&db, "keep"), 0));
    // Recursion ends: `countdown` only reads `s` on every path.
    assert!(!super::param_is_kept(&db, function(&db, "countdown"), 0));
}

#[test]
fn closures_keep_what_they_capture_and_return() {
    let source = r#"
func make(i: i64) -> usize {
    let captured = f"c{i}"
    let f = func() -> usize { captured.len() }
    let g = func(n: i64) -> String { f"r{n}" }
    f() + g(i).len()
}
"#;
    assert_eq!(kept_plans(source, "make"), [true, true]);
}
