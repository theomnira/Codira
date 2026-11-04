//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - Tests of traits: declarations, implementations, bounds, the
//!   diagnostics for each rule, which function a trait-method call reaches
//!   at each instance, and how the effect analysis treats such a call.

use std::fmt::Write;

use codira_hir_input::WithFixture;

use crate::{
    mock::MockDatabase, mono, realtime::Effect, utils::tests::diagnostics, Expr, HirDatabase,
    ModuleDef, Package,
};

/// A program that uses every supported form checks without a diagnostic:
/// required and default methods, a supertrait, implementations for a
/// struct, a class, a primitive and a generic struct (with a bounded
/// `extend`), bounds in a parameter list and in a `where` clause, `A + B`,
/// an associated function called on a type parameter, a default method
/// called on a concrete type, and a bounded parameter passed on to
/// another bounded function.
#[test]
fn every_supported_form_checks_clean() {
    insta::assert_snapshot!(diagnostics(
        r#"
    trait Show {
        func show(self) -> i64;
        func twice(self) -> i64 { self.show() * 2 }
    }

    trait Named: Show {
        func id(self) -> i64;
    }

    trait Make {
        func make() -> Self;
    }

    struct Point { x: i64, y: i64 }
    class Counter { n: i64 }
    struct Box[T] { v: T }

    extend Point: Show {
        func show(self) -> i64 { self.x + self.y }
    }
    extend Point: Named {
        func id(self) -> i64 { 7 }
    }
    extend Point: Make {
        func make() -> Point { Point { x: 1, y: 2 } }
    }
    extend Counter: Show {
        func show(self) -> i64 { self.n }
        func twice(self) -> i64 { 0 }
    }
    extend i64: Show {
        func show(self) -> i64 { self }
    }
    extend Box[T: Show]: Show {
        func show(self) -> i64 { self.v.show() + 100 }
    }

    func show_it[T: Show](x: T) -> i64 { x.show() + x.twice() }
    func both[T](x: T) -> i64 where T: Named { x.id() + x.show() + show_it(x) }
    func plus[T: Show + Make](x: T) -> i64 { T.make().show() + x.show() }

    func main() -> i64 {
        let p = Point { x: 1, y: 2 };
        let b = Box { v: p };
        let n: i64 = 5;
        show_it(p) + show_it(Counter { n: 3 }) + show_it(n) + show_it(b)
            + both(p) + plus(p) + p.twice() + p.show()
    }
    "#
    ));
}

/// Each way an implementation can fail to match its trait.
#[test]
fn implementation_must_match_its_trait() {
    insta::assert_snapshot!(diagnostics(
        r#"
    trait Show {
        func show(self) -> i64;
        func other(self, x: Self) -> bool;
    }
    trait Named: Show {
        func id(self) -> i64;
    }
    struct A { x: i64 }
    struct B { x: i64 }
    struct C { x: i64 }

    extend A: Show {
        func show(self) -> i64 { 1 }
    }
    extend B: Show {
        func show(self) -> bool { true }
        func other(self, x: A) -> bool { true }
        func extra(self) -> i64 { 2 }
    }
    extend C: Named {
        func id(self) -> i64 { 3 }
    }
    extend A: Named {
        func id() -> i64 { 3 }
    }
    "#
    ));
}

/// A type implements a trait at most once, and a block can only implement
/// traits, defined ones.
#[test]
fn duplicate_and_unresolved_implementations() {
    insta::assert_snapshot!(diagnostics(
        r#"
    trait Show {
        func show(self) -> i64;
    }
    struct A { x: i64 }
    struct Box[T] { v: T }
    extend A: Show {
        func show(self) -> i64 { 1 }
    }
    extend A: Show {
        func show(self) -> i64 { 2 }
    }
    extend Box[T]: Show {
        func show(self) -> i64 { 1 }
    }
    extend Box[i64]: Show {
        func show(self) -> i64 { 2 }
    }
    extend A: Missing {}
    extend A: B {}
    struct B { y: i64 }
    extend bool {}
    "#
    ));
}

/// At a call of a bounded function, the argument's type must implement the
/// bound: the error names the type, the trait and the parameter. A
/// generic implementation applies only where its own bounds hold.
#[test]
fn a_call_must_meet_the_bounds() {
    insta::assert_snapshot!(diagnostics(
        r#"
    trait Show {
        func show(self) -> i64;
    }
    trait Other {
        func other(self) -> i64;
    }
    struct A { x: i64 }
    struct Box[T] { v: T }
    extend A: Show {
        func show(self) -> i64 { 1 }
    }
    extend Box[T: Show]: Show {
        func show(self) -> i64 { self.v.show() }
    }

    func show_it[T: Show](x: T) -> i64 { x.show() }
    func needs_other[T: Other](x: T) -> i64 { x.other() }
    func forward[T: Show](x: T) -> i64 { show_it(x) + needs_other(x) }

    func main() -> i64 {
        // An integer literal is an `i32` unless something says otherwise,
        // and only `A` implements `Show` here.
        show_it(A { x: 1 }) + show_it(5) + show_it(Box { v: A { x: 1 } })
            + show_it(Box { v: 2.5 })
    }
    "#
    ));
}

/// What a body may call on a type parameter: only what its bounds declare,
/// and one of them unambiguously. Operators are not resolved through
/// traits.
#[test]
fn method_calls_on_a_type_parameter_go_through_its_bounds() {
    insta::assert_snapshot!(diagnostics(
        r#"
    trait Show {
        func show(self) -> i64;
    }
    trait Also {
        func show(self) -> i64;
    }
    func unbounded[T](x: T) -> i64 { x.show() }
    func missing[T: Show](x: T) -> i64 { x.len() }
    func ambiguous[T: Show + Also](x: T) -> i64 { x.show() }
    func assoc[T: Show](x: T) -> i64 { T.make() }
    func ops[T: Show](a: T, b: T) -> bool { a < b }
    "#
    ));
}

/// What this version does not support is reported where it is written.
#[test]
fn unsupported_trait_forms_are_reported() {
    insta::assert_snapshot!(diagnostics(
        r#"
    trait Iterable[E] {
        func next(self) -> E;
    }
    trait Generic {
        func map[U](self, u: U) -> U;
    }
    trait Contracted {
        func pos(self) -> i64 ensures result > 0;
    }
    trait Loop1: Loop2 {
        func a(self);
    }
    trait Loop2: Loop1 {
        func b(self);
    }
    trait Bad: i64 {
        func c(self);
    }
    trait Show {
        func show(self) -> i64;
    }
    struct Bounded[T: Show] { v: T }
    func f[T: Undefined](x: T) {}
    func g[T](x: T) where i64: Show {}
    func h[T: Iterable[i64]](x: T) {}
    "#
    ));
}

/// Every non-generic function's reachable instances, each with the
/// function every method call in its body reaches there.
fn instance_calls(fixture: &str) -> String {
    let (db, _) = MockDatabase::with_single_file(fixture);
    let roots: Vec<_> = Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.all_functions(&db))
        .filter(|f| !f.is_generic(&db))
        .collect();
    let instances = mono::collect_instances(&db, roots, |_| false).unwrap();
    let mut out = String::new();
    for (instance, infer) in instances {
        writeln!(out, "{}", instance.name(&db)).unwrap();
        let body = instance.function().body(&db);
        let mut calls: Vec<_> = body
            .exprs()
            .filter(|(_, e)| matches!(e, Expr::MethodCall { .. }))
            .filter_map(|(e, _)| Some((e, infer.method_resolution(e)?)))
            .collect();
        calls.sort_by_key(|(e, _)| u32::from(e.into_raw()));
        for (expr, callee) in calls {
            let callee = crate::Function::from(callee);
            let args = infer
                .instantiation(expr)
                .map(|args| mono::Instance::new(callee, args.clone()).name(&db))
                .unwrap_or_else(|| callee.full_name(&db));
            let owner = match callee.trait_container(&db) {
                Some(t) => format!(" (trait {})", t.name(&db)),
                None => String::new(),
            };
            writeln!(out, "  -> {args}{owner}").unwrap();
        }
    }
    out
}

/// Monomorphisation resolves each trait-method call to the implementation
/// for the instance's type -- or to the default body, instantiated at that
/// type, when the implementation does not override it.
#[test]
fn instances_call_the_implementation_for_their_type() {
    insta::assert_snapshot!(instance_calls(
        r#"
    trait Show {
        func show(self) -> i64;
        func twice(self) -> i64 { self.show() * 2 }
    }
    struct Point { x: i64 }
    struct Box[T] { v: T }
    class Counter { n: i64 }
    extend Point: Show {
        func show(self) -> i64 { self.x }
    }
    extend Counter: Show {
        func show(self) -> i64 { self.n }
        func twice(self) -> i64 { 0 }
    }
    extend i64: Show {
        func show(self) -> i64 { self }
    }
    extend Box[T: Show]: Show {
        func show(self) -> i64 { self.v.show() }
    }
    func show_it[T: Show](x: T) -> i64 { x.show() + x.twice() }
    func main() -> i64 {
        let n: i64 = 3;
        show_it(Point { x: 1 }) + show_it(Counter { n: 2 }) + show_it(n)
            + show_it(Box { v: Point { x: 4 } })
    }
    "#
    ));
}

/// The effect analysis runs once per generic body, before an
/// implementation is chosen, so a trait-method call on a type parameter is
/// unknown: every effect. A `@noalloc` caller of such a function is
/// rejected even if every implementation happens to be allocation-free.
#[test]
fn effect_analysis_treats_a_bound_call_as_unknown() {
    let source = r#"
    trait Show {
        func show(self) -> i64;
    }
    struct Point { x: i64 }
    extend Point: Show {
        func show(self) -> i64 { self.x }
    }
    func show_it[T: Show](x: T) -> i64 { x.show() }
    @noalloc
    func direct(p: Point) -> i64 { p.show() }
    @noalloc
    func through_bound(p: Point) -> i64 { show_it(p) }
    "#;
    let (db, _) = MockDatabase::with_single_file(source);
    let function = |name: &str| {
        Package::all(&db)
            .iter()
            .flat_map(|pkg| pkg.modules(&db))
            .flat_map(|module| module.declarations(&db))
            .find_map(|def| match def {
                ModuleDef::Function(f) if f.name(&db).to_string() == name => Some(f),
                _ => None,
            })
            .unwrap()
    };
    let generic = db.effect_summary(function("show_it")).effects;
    for effect in Effect::ALL {
        assert!(generic.contains(effect), "missing {effect:?}");
    }
    assert!(db.effect_summary(function("direct")).effects.is_empty());
    insta::assert_snapshot!(diagnostics(source));
}
