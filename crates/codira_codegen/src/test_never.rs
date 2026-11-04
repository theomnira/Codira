//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - Functions returning `never`: their LLVM type returns `void` and they
//!   carry `noreturn`; every call to one -- direct, to an `extern "C"`
//!   function through the dispatch table, or to another module's function
//!   through it -- is followed by `unreachable`; and the reflected
//!   signature names `()` as the result, as the runtime's definitions of
//!   its non-returning hooks do.
//! - A `uses` function returning `never` is not `noreturn`: an abort out of
//!   it comes back by an ordinary return.

use std::cell::RefCell;

use codira_hir::{diagnostics::DiagnosticSink, HirDatabase};
use codira_hir_input::{SourceDatabase, WithFixture};
use codira_target::spec::Target;
use inkwell::{context::Context, OptimizationLevel};

use crate::{
    code_gen::{AssemblyBuilder, CodeGenContext},
    ir::{file::gen_file_ir, file_group::gen_file_group_ir},
    mock::MockDatabase,
    CodeGenDatabase,
};

/// The unoptimised LLVM IR of every module group of `fixture`, keyed by the
/// group's file path; panics if the fixture has diagnostics. Each group is
/// also built into an object, so symbol generation (the reflected
/// signatures) is exercised.
fn ir_of(fixture: &str) -> Vec<(String, String)> {
    let mut db = MockDatabase::with_files(fixture);
    db.set_optimization_level(OptimizationLevel::None);
    db.set_target(Target::host_target().unwrap());

    let messages = RefCell::new(Vec::new());
    let mut sink = DiagnosticSink::new(|diag| messages.borrow_mut().push(diag.message()));
    for module in codira_hir::Package::all(&db)
        .into_iter()
        .flat_map(|package| package.modules(&db))
    {
        module.diagnostics(&db, &mut sink);
    }
    drop(sink);
    let messages = messages.into_inner();
    assert!(messages.is_empty(), "{messages:?}");

    let llvm_context = Context::create();
    let code_gen = CodeGenContext::new(&llvm_context, &db);
    let partition = db.module_partition();
    partition
        .iter()
        .map(|(group_id, group)| {
            let group_ir = gen_file_group_ir(&code_gen, group);
            let file_ir = gen_file_ir(&code_gen, &group_ir, group);
            file_ir
                .llvm_module
                .verify()
                .unwrap_or_else(|e| panic!("invalid IR: {}", e.to_string()));
            AssemblyBuilder::new(&code_gen, &partition, group_id)
                .build()
                .expect("failed to build the object file");
            (
                group.relative_file_path().to_string(),
                file_ir.llvm_module.print_to_string().to_string(),
            )
        })
        .collect()
}

/// The text of the definition of `@name` in `ir`, from `define` to its
/// closing brace.
fn definition<'a>(ir: &'a str, name: &str) -> &'a str {
    let start = ir
        .lines()
        .position(|line| line.starts_with("define") && line.contains(&format!("@{name}(")))
        .unwrap_or_else(|| panic!("no definition of `{name}` in\n{ir}"));
    let begin = ir.lines().take(start).map(|l| l.len() + 1).sum::<usize>();
    let rest = &ir[begin..];
    let end = rest.find("\n}").expect("a definition ends with `}`");
    &rest[..end + 2]
}

/// The attribute group numbers (`#N`) on `@name`'s definition line.
fn attribute_groups<'a>(ir: &'a str, name: &str) -> Vec<&'a str> {
    let line = definition(ir, name).lines().next().unwrap();
    line.split_whitespace()
        .filter(|word| word.starts_with('#'))
        .collect()
}

/// Whether `@name` carries `noreturn`, through any of its attribute groups.
fn is_noreturn(ir: &str, name: &str) -> bool {
    attribute_groups(ir, name).iter().any(|group| {
        ir.lines().any(|line| {
            line.starts_with(&format!("attributes {group} ")) && line.contains("noreturn")
        })
    })
}

/// The instruction right after the first call to `callee` in `body`.
fn after_call<'a>(body: &'a str, callee: &str) -> &'a str {
    let mut lines = body
        .lines()
        .skip_while(|line| !(line.contains("call ") && line.contains(callee)));
    lines
        .next()
        .unwrap_or_else(|| panic!("no call to `{callee}` in\n{body}"));
    lines.next().unwrap().trim()
}

#[test]
fn a_function_returning_never_is_void_and_noreturn() {
    let ir = ir_of(
        r"
    func stop() -> never { loop {} }
    public func main() { if false { stop() } }
    ",
    );
    let (_, ir) = &ir[0];
    let stop = definition(ir, "stop");
    assert!(stop.starts_with("define void @stop("), "{stop}");
    assert!(is_noreturn(ir, "stop"), "{ir}");
    assert!(!is_noreturn(ir, "main"), "{ir}");
    assert_eq!(after_call(definition(ir, "main"), "@stop"), "unreachable");
}

#[test]
fn never_in_one_branch_of_a_value_if_and_after_return() {
    let ir = ir_of(
        r"
    func stop() -> never { loop {} }
    public func pick(c: bool) -> i64 {
        if c { 5 } else { stop() }
    }
    public func early(c: bool) -> i64 {
        if !c { return stop() }
        7
    }
    ",
    );
    let (_, ir) = &ir[0];
    assert_eq!(after_call(definition(ir, "pick"), "@stop"), "unreachable");
    assert_eq!(after_call(definition(ir, "early"), "@stop"), "unreachable");
}

#[test]
fn a_public_function_returning_never_has_a_noreturn_void_wrapper() {
    let ir = ir_of(
        r"
    public struct P { x: i64 }
    public func stop(p: P) -> never { loop {} }
    ",
    );
    let (_, ir) = &ir[0];
    let wrapper = definition(ir, "stop_wrapper");
    assert!(
        wrapper.starts_with("define void @stop_wrapper("),
        "{wrapper}"
    );
    assert!(is_noreturn(ir, "stop_wrapper"), "{ir}");
    assert_eq!(after_call(wrapper, "@stop"), "unreachable");
}

#[test]
fn calls_through_the_dispatch_table_are_followed_by_unreachable() {
    let ir = ir_of(
        r#"
    //- /mod.code
    import other.{fail_with}
    extern "C" {
        func codira_exit(code: i32) -> never;
    }
    public func main(c: bool) -> i64 {
        if c { codira_exit(3) }
        if !c { fail_with(4) }
        1
    }

    //- /other.code
    extern "C" {
        func codira_exit(code: i32) -> never;
    }
    public func fail_with(code: i32) -> never {
        codira_exit(code)
    }
    "#,
    );
    let main_ir = &ir
        .iter()
        .find(|(_, ir)| ir.contains("define i64 @main("))
        .expect("the root module")
        .1;
    let main = definition(main_ir, "main");
    assert_eq!(after_call(main, "codira_exit"), "unreachable", "{main}");
    assert_eq!(after_call(main, "fail_with"), "unreachable", "{main}");
}

#[test]
fn a_uses_function_returning_never_is_not_noreturn() {
    let ir = ir_of(
        r"
    effect Fail {
        func fail(code: i64) -> never
    }
    func give_up(code: i64) uses Fail -> never {
        perform Fail.fail(code)
    }
    public func run() -> i64 {
        handle give_up(3) {
            Fail.fail(code) -> code * 10
        }
    }
    ",
    );
    let (_, ir) = &ir[0];
    assert!(!is_noreturn(ir, "give_up"), "{ir}");
}
