//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Tests of the escape analysis: one per escape reason, the conservative
//!   cases, summaries through recursion, and the placement of each
//!   allocation.

use codira_hir_input::WithFixture;

use super::{ParamEscape, Placement, Reason};
use crate::{mock::MockDatabase, AstDatabase, HirDatabase, ModuleDef, Package};

fn function(db: &MockDatabase, name: &str) -> crate::Function {
    let modules: Vec<_> = Package::all(db)
        .iter()
        .flat_map(|pkg| pkg.modules(db))
        .collect();
    let free = modules
        .iter()
        .flat_map(|module| module.declarations(db))
        .filter_map(|def| match def {
            ModuleDef::Function(f) => Some(f),
            _ => None,
        });
    let methods = modules
        .iter()
        .flat_map(|module| module.impls(db))
        .flat_map(|imp| imp.items(db))
        .map(|item| match item {
            crate::code_model::AssocItem::Function(f) => f,
        });
    free.chain(methods)
        .find(|f| f.name(db).to_string() == name)
        .unwrap_or_else(|| panic!("no function `{name}` in fixture"))
}

/// Every allocation of `name`, one per line: its source text and placement.
fn placements(source: &str, name: &str) -> String {
    let (db, _) = MockDatabase::with_single_file(source);
    let func = function(&db, name);
    let analysis = db.escape_analysis(func);
    analysis
        .allocations
        .iter()
        .map(|a| {
            let text = a.site.as_ref().map_or_else(
                || "?".to_string(),
                |site| {
                    let root = db.parse(site.file_id).syntax_node();
                    site.value.to_node(&root).text().to_string()
                },
            );
            format!("{text}: {}", a.placement)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The single allocation of `name`.
fn placement_of(source: &str, name: &str) -> Placement {
    let (db, _) = MockDatabase::with_single_file(source);
    let analysis = db.escape_analysis(function(&db, name));
    assert_eq!(analysis.allocations.len(), 1, "{:?}", analysis.allocations);
    analysis.allocations[0].placement.clone()
}

fn summary_of(source: &str, name: &str) -> Vec<ParamEscape> {
    let (db, _) = MockDatabase::with_single_file(source);
    db.escape_summary(function(&db, name)).params.clone()
}

const CLASSES: &str = r#"
class Point { x: i64, y: i64 }
class Holder { var p: Point }
class Bag { var items: [Point] }
"#;

fn with_classes(body: &str) -> String {
    format!("{CLASSES}\n{body}")
}

fn assert_escape(source: &str, name: &str, expected: &str) {
    match placement_of(&with_classes(source), name) {
        Placement::Heap(reason) => {
            assert!(reason.is_escape(), "not an escape: {reason:?}");
            assert_eq!(reason.phrase(), expected);
        }
        Placement::Frame => panic!("expected an escape ({expected}), got the frame"),
    }
}

#[test]
fn local_object_goes_to_the_frame() {
    let source = with_classes("func sum() -> i64 { let p = Point { x: 1, y: 2 }; p.x + p.y }");
    assert_eq!(placement_of(&source, "sum"), Placement::Frame);
}

#[test]
fn returned_object_escapes() {
    assert_escape(
        "func make() -> Point { Point { x: 1, y: 2 } }",
        "make",
        "returned from `make`",
    );
    assert_escape(
        "func make() -> Point { let p = Point { x: 1, y: 2 }; return p }",
        "make",
        "returned from `make`",
    );
}

#[test]
fn returned_inside_a_value_escapes() {
    assert_escape(
        "func pair() -> (Point, i64) { let p = Point { x: 1, y: 2 }; (p, 3) }",
        "pair",
        "returned from `pair`",
    );
    assert_escape(
        "func maybe() -> Point? { let p = Point { x: 1, y: 2 }; p }",
        "maybe",
        "returned from `maybe`",
    );
}

#[test]
fn stored_into_a_parameter_escapes() {
    assert_escape(
        "func keep(h: Holder) { let p = Point { x: 1, y: 2 }; h.p = p }",
        "keep",
        "stored into field `p` of parameter `h`, which may be a heap object",
    );
}

#[test]
fn stored_into_a_heap_object_escapes() {
    assert_escape(
        "func keep(h: Holder) { h.p.x; let q = Point { x: 3, y: 4 }; let r = h; r.p = q }",
        "keep",
        "stored into field `p` of parameter `h`, which may be a heap object",
    );
    // A field read from a frame object is something the analysis did not
    // see allocated: storing into it is storing into the heap.
    let source = with_classes(
        "func nest() -> i64 {
            let h = Holder { p: Point { x: 0, y: 0 } }
            let q = Point { x: 3, y: 4 }
            h.p = q
            let inner = Bag { items: [] }
            let bags = [inner]
            bags[0].items = [q]
            0
        }",
    );
    let out = placements(&source, "nest");
    assert!(
        out.contains("[q]: escapes: stored into field `items` of a heap object"),
        "{out}"
    );
}

#[test]
fn inside_a_heap_object_escapes_transitively() {
    let source = with_classes(
        "func make() -> Holder {
            let p = Point { x: 1, y: 2 }
            Holder { p: p }
        }",
    );
    let out = placements(&source, "make");
    assert_eq!(
        out,
        "Point { x: 1, y: 2 }: escapes: stored into an object of class `Holder`, which is on the \
         heap\nHolder { p: p }: escapes: returned from `make`"
    );
}

#[test]
fn frame_object_may_hold_a_frame_object() {
    let source = with_classes(
        "func local() -> i64 {
            let p = Point { x: 1, y: 2 }
            let h = Holder { p: p }
            h.p.x
        }",
    );
    assert_eq!(
        placements(&source, "local"),
        "Point { x: 1, y: 2 }: moved to the stack\nHolder { p: p }: moved to the stack"
    );
}

#[test]
fn captured_by_a_closure_escapes() {
    let source = with_classes(
        "func capture() -> i64 {
            let p = Point { x: 1, y: 2 }
            let f = func() -> i64 { p.x }
            f()
        }",
    );
    let out = placements(&source, "capture");
    assert_eq!(
        out,
        "Point { x: 1, y: 2 }: escapes: captured by a closure\n\
         func() -> i64 { p.x }: escapes: passed to a call through a function value, which the \
         analysis cannot see"
    );
    let source = with_classes("func make() -> i64 { let f = func() -> i64 { 1 }; 0 }");
    assert_eq!(
        placement_of(&source, "make"),
        Placement::Heap(Reason::ClosureObject)
    );
}

#[test]
fn passed_to_a_function_that_keeps_it_escapes() {
    assert_escape(
        "func stash(h: Holder, p: Point) { h.p = p }
         func caller(h: Holder) { let p = Point { x: 1, y: 2 }; stash(h, p) }",
        "caller",
        "passed to `stash` as `p`, which is stored into field `p` of parameter `h`, which may be \
         a heap object",
    );
}

#[test]
fn passed_to_a_function_that_only_reads_it_stays_in_the_frame() {
    let source = with_classes(
        "func norm(p: Point) -> i64 { p.x * p.x + p.y * p.y }
         func caller() -> i64 { let p = Point { x: 1, y: 2 }; norm(p) }",
    );
    assert_eq!(placement_of(&source, "caller"), Placement::Frame);
    assert_eq!(summary_of(&source, "norm"), vec![ParamEscape::NoEscape]);
}

#[test]
fn returned_by_the_callee_flows_into_the_result() {
    let source = with_classes(
        "func id(p: Point) -> Point { p }
         func caller() -> i64 { let p = Point { x: 1, y: 2 }; id(p).x }
         func leak() -> Point { let p = Point { x: 1, y: 2 }; id(p) }",
    );
    assert_eq!(summary_of(&source, "id"), vec![ParamEscape::Returned]);
    assert_eq!(placement_of(&source, "caller"), Placement::Frame);
    assert!(matches!(
        placement_of(&source, "leak"),
        Placement::Heap(Reason::Returned { .. })
    ));
}

#[test]
fn methods_summarise_self() {
    let source = with_classes(
        "extend Point { func len2(self) -> i64 { self.x * self.x + self.y * self.y } }
         func caller() -> i64 { let p = Point { x: 1, y: 2 }; p.len2() }",
    );
    assert_eq!(summary_of(&source, "len2"), vec![ParamEscape::NoEscape]);
    assert_eq!(placement_of(&source, "caller"), Placement::Frame);
}

#[test]
fn recursion_reaches_a_fixpoint() {
    let source = with_classes(
        "func down(p: Point, n: i64) -> i64 { if n == 0 { p.x } else { down(p, n - 1) } }
         func ping(h: Holder, p: Point, n: i64) { if n > 0 { pong(h, p, n - 1) } }
         func pong(h: Holder, p: Point, n: i64) { if n == 1 { h.p = p } else { ping(h, p, n) } }",
    );
    assert_eq!(
        summary_of(&source, "down"),
        vec![ParamEscape::NoEscape, ParamEscape::NoEscape]
    );
    let ping = summary_of(&source, "ping");
    assert!(matches!(ping[1], ParamEscape::Escapes(_)), "{ping:?}");
    assert_eq!(ping[0], ParamEscape::NoEscape);
}

#[test]
fn sent_to_a_task_escapes() {
    assert_escape(
        "func work(p: Point) -> i64 { p.x }
         func start() { let p = Point { x: 1, y: 2 }; scope { spawn work(p) } }",
        "start",
        "sent to a task by `spawn`",
    );
}

#[test]
fn sent_on_a_channel_escapes() {
    assert_escape(
        "func send() { scope { let ch = channel(1); let p = Point { x: 1, y: 2 }; ch <- p } }",
        "send",
        "sent on a channel",
    );
}

#[test]
fn passed_to_an_extern_function_escapes() {
    assert_escape(
        "extern func host_keep(p: Point);
         func give() { let p = Point { x: 1, y: 2 }; host_keep(p) }",
        "give",
        "passed to `host_keep`, an `extern` function, which the analysis cannot see into",
    );
}

#[test]
fn a_call_through_a_function_value_is_unknown() {
    assert_escape(
        "func give(f: func(Point) -> i64) -> i64 { let p = Point { x: 1, y: 2 }; f(p) }",
        "give",
        "passed to a call through a function value, which the analysis cannot see",
    );
}

#[test]
fn a_trait_method_on_a_generic_type_is_unknown() {
    let source = with_classes(
        "trait Show { func show(self) -> i64 }
         extend Point: Show { func show(self) -> i64 { self.x } }
         func through[T: Show](t: T) -> i64 { t.show() }
         func give() -> i64 { let p = Point { x: 1, y: 2 }; through(p) }",
    );
    let summary = summary_of(&source, "through");
    match &summary[0] {
        ParamEscape::Escapes(Reason::Unknown { what }) => {
            assert!(what.contains("trait method on a generic type"), "{what}")
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(placement_of(&source, "give"), Placement::Heap(_)));
}

#[test]
fn a_loop_iteration_may_not_keep_its_object() {
    let source = with_classes(
        "func last(n: i64) -> i64 {
            var keep = Point { x: 0, y: 0 }
            var total = 0
            for i in 0..n {
                let p = Point { x: i, y: i }
                total += p.x
                let q = Point { x: i, y: 0 }
                keep = q
            }
            total + keep.x
        }",
    );
    assert_eq!(
        placements(&source, "last"),
        "Point { x: 0, y: 0 }: moved to the stack\n\
         Point { x: i, y: i }: moved to the stack\n\
         Point { x: i, y: 0 }: stays on the heap: kept past its loop iteration in `keep` (its \
         frame slot is reused each iteration)"
    );
}

#[test]
fn arrays_of_known_small_length_go_to_the_frame() {
    let source = with_classes(
        "func sum() -> i64 {
            let xs = [1, 2, 3, 4]
            var total = 0
            for x in xs { total += x }
            total + xs[0] + xs.len() as i64
        }
        func long() -> i64 {
            let xs = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
            xs[0]
        }
        func grown() -> i64 { let xs = [1, 2]; xs.push(3); xs[2] }",
    );
    assert_eq!(placement_of(&source, "sum"), Placement::Frame);
    assert_eq!(
        placement_of(&source, "long"),
        Placement::Heap(Reason::TooLong { len: 17 })
    );
    match placement_of(&source, "grown") {
        Placement::Heap(reason) => {
            assert_eq!(
                reason.phrase(),
                "grown by `push`, which the runtime does on the heap"
            )
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn elements_of_a_heap_array_are_on_the_heap() {
    assert_escape(
        "func fill(bag: Bag) { let p = Point { x: 1, y: 2 }; bag.items.push(p) }",
        "fill",
        "stored into an element of parameter `bag`, which may be a heap object",
    );
    let source = with_classes(
        "func local() -> i64 { let p = Point { x: 1, y: 2 }; let xs = [p]; xs.push(p); xs[0].x }",
    );
    let out = placements(&source, "local");
    assert!(
        out.contains(
            "Point { x: 1, y: 2 }: escapes: stored into an object of array literal of 1 \
                       elements, which is on the heap"
        ),
        "{out}"
    );
}

#[test]
fn derived_arrays_stay_on_the_heap() {
    let source =
        with_classes("func double(xs: [i64]) -> i64 { let ys = xs.map(func(x) { x * 2 }); ys[0] }");
    assert_eq!(
        placement_of(&source, "double"),
        Placement::Heap(Reason::RuntimeLength)
    );
}

#[test]
fn debug_printing_does_not_escape() {
    let source = with_classes("func show() -> String { let p = Point { x: 1, y: 2 }; f\"{p:?}\" }");
    assert_eq!(placement_of(&source, "show"), Placement::Frame);
}

#[test]
fn calls_into_another_module_are_unknown() {
    let db = MockDatabase::with_files(
        r#"
    //- /mod.code
    import geometry.{ Point, norm }
    func caller() -> i64 { let p = Point { x: 1 }; norm(p) }
    //- /geometry.code
    public class Point { x: i64 }
    public func norm(p: Point) -> i64 { p.x }
    "#,
    );
    let func = function(&db, "caller");
    let analysis = db.escape_analysis(func);
    match &analysis.allocations[0].placement {
        Placement::Heap(Reason::Unknown { what }) => {
            assert!(what.contains("in another module"), "{what}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn allocations_in_a_closure_body_stay_on_the_heap() {
    let source = with_classes(
        "func outer() -> i64 { let f = func() -> i64 { let p = Point { x: 1, y: 2 }; p.x }; f() }",
    );
    let out = placements(&source, "outer");
    assert!(
        out.contains("Point { x: 1, y: 2 }: stays on the heap: allocated inside a closure body"),
        "{out}"
    );
}

#[test]
fn optional_returned_by_question_mark_escapes() {
    let source = with_classes(
        "func first(p: Point?) -> i64? { let q = p?; Some(q.x) }
         func make() -> Point? { let p: Point? = Point { x: 1, y: 2 }; let q = p?; nil }",
    );
    assert!(matches!(
        placement_of(&source, "make"),
        Placement::Heap(Reason::Returned { .. })
    ));
}
