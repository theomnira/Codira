//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
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

#[test]
fn array_index_assign() {
    test_snapshot_unoptimized(
        "array_index_assign",
        r"
    public func main() {
        let a = [1,2,3,4,]
        a[1] = 100
    }
    ",
    );
}

#[test]
fn array_index() {
    test_snapshot(
        "array_index",
        r"
    public func main() -> i8 {
        let a = [1,2,3,4,]
        a[3]
    }
    ",
    );
}

#[test]
fn array_literal() {
    test_snapshot_unoptimized(
        "array_literal",
        r"
    public func main() {
        let a = [1,2,3,4,]
    }
    ",
    );
}

#[test]
fn multi_file() {
    test_snapshot(
        "multi_file",
        r"
    //- /mod.code
    import foo.get_value

    public func main() -> i32 {
        get_value()
    }

    //- /foo.code
    internal func get_value() -> i32 {
        3
    }
    ",
    );
}

#[test]
fn issue_262() {
    test_snapshot(
        "issue_262",
        r"
    func foo() -> i32 {
        let bar = {
            let b = 3;
            return b + 3;
        };

        // This code will never be executed
        let a = 3 + 4;
        a
    }",
    );
}

#[test]
fn issue_225() {
    test_snapshot(
        "issue_225",
        r#"
    class Num {
        value: i64,
    }

    public func foo(b: i64) {
        Num { value: b }.value;
    }

    public func bar(b: i64) {
        { let a = Num { value: b }; a}.value;
    }
        "#,
    );
}

#[test]
fn issue_228_never_if() {
    test_snapshot(
        "issue_228_never_if",
        r#"
    public func fact(n: usize) -> usize {
   	    if n == 0 {return 1} else {return n * (n-1)}
   	    return 2;
    }
    "#,
    );
}

#[test]
fn issue_228() {
    test_snapshot(
        "issue_228",
        r#"
    public func fact(n: usize) -> usize {
   	    if n == 0 {return 1} else {n * (n-1)}
    }
    "#,
    );
}

#[test]
fn issue_128() {
    test_snapshot(
        "issue_128",
        r#"
    // resources/script.code
    extern func thing(n: i32);
    extern func print(n: i32) -> i32;

    public func main() {
        // 1st
        print(1);
        thing(5);

        // 2nd
        print(2);
        thing(78);
    }
    "#,
    );
}

#[test]
fn issue_133() {
    test_snapshot(
        "issue_133",
        r#"
    func do_the_things(n: i32) -> i32 {
        n + 7
    }
    
    public func main() {
        do_the_things(3);
    }
    "#,
    );
}

#[test]
fn literal_types() {
    test_snapshot_unoptimized(
        "literal_types",
        r"
    public func main(){
        let a = 123;
        let a = 123u8;
        let a = 123u16;
        let a = 123u32;
        let a = 123u64;
        let a = 123u128;
        let a = 1_000_000_u32;
        let a = 123i8;
        let a = 123i16;
        let a = 123i32;
        let a = 123i64;
        let a = 123123123123123123123123123123123i128;
        let a = 1_000_000_i32;
        let a = 1_000_123.0e-2;
        let a = 1_000_123.0e-2f32;
        let a = 1_000_123.0e-2f64;
    }

    public func add(a:u32) -> u32 {
        a + 12u32
    }",
    );
}

#[test]
fn function() {
    test_snapshot(
        "function",
        r#"
    public func main() {
    }
    "#,
    );
}

#[test]
fn return_type() {
    test_snapshot(
        "return_type",
        r#"
    public func main() -> i32 {
      0
    }
    "#,
    );
}

#[test]
fn function_arguments() {
    test_snapshot(
        "function_arguments",
        r#"
    public func main(a:i32) -> i32 {
      a
    }
    "#,
    );
}

#[test]
fn assignment_op_bool() {
    test_snapshot(
        "assignment_op_bool",
        r#"
    public func assign(a: bool, b: bool) -> bool {
        a = b;
        a
    }
    // TODO: Add errors
    // a += b;
    // a *= b;
    // a -= b;
    // a /= b;
    // a %= b;
    "#,
    );
}

#[test]
fn logic_op_bool() {
    test_snapshot(
        "logic_op_bool",
        r#"
    public func and(a: bool, b: bool) -> bool {
        a && b
    }
    public func or(a: bool, b: bool) -> bool {
        a || b
    }    
    "#,
    );
}

#[test]
fn assignment_op_struct() {
    test_snapshot(
        "assignment_op_struct",
        r#"
    public struct Value(i32, i32);
    public class Heap(f64, f64);

    public func assign_value(a: Value, b: Value) -> Value {
        a = b;
        a
    }

    public func assign_heap(a: Heap, b: Heap) -> Heap {
        a = b;
        a
    }
    // TODO: Add errors
    // a += b;
    // a *= b;
    // a -= b;
    // a /= b;
    // a %= b;
    "#,
    );
}

macro_rules! test_number_operator_types {
    ($(
        $ty:ident
     ),+) => {
        $(
            paste::item! {
                #[test]
                fn [<assignment_op_ $ty>]() {
                    test_snapshot(
                        &format!("assignment_op_{ty}", ty = stringify!($ty)),
                        &format!(r#"
    public func assign(a: {ty}, b: {ty}) -> {ty} {{
        a = b;
        a
    }}
    public func assign_add(a: {ty}, b: {ty}) -> {ty} {{
        a += b;
        a
    }}
    public func assign_subtract(a: {ty}, b: {ty}) -> {ty} {{
        a -= b;
        a
    }}
    public func assign_multiply(a: {ty}, b: {ty}) -> {ty} {{
        a *= b;
        a
    }}
    public func assign_divide(a: {ty}, b: {ty}) -> {ty} {{
        a /= b;
        a
    }}
    public func assign_remainder(a: {ty}, b: {ty}) -> {ty} {{
        a %= b;
        a
    }}
                        "#, ty = stringify!($ty),
                    ));
                }

                #[test]
                fn [<arithmetic_op_ $ty>]() {
                    test_snapshot(
                        &format!("arithmetic_op_{ty}", ty = stringify!($ty)),
                        &format!(r#"
    public func add(a: {ty}, b: {ty}) -> {ty} {{ a + b }}
    public func subtract(a: {ty}, b: {ty}) -> {ty} {{ a - b }}
    public func multiply(a: {ty}, b: {ty}) -> {ty} {{ a * b }}
    public func divide(a: {ty}, b: {ty}) -> {ty} {{ a / b }}
    public func remainder(a: {ty}, b: {ty}) -> {ty} {{ a % b }}
                        "#, ty = stringify!($ty),
                    ));
                }
            }
        )+
    };
}

test_number_operator_types!(f32, f64, i8, i16, i32, i64, i128, u8, u16, u32, u64, u128);

macro_rules! test_compare_operator_types {
    ($(
        $ty:ident
     ),+) => {
        $(
            paste::item! {
                #[test]
                fn [<compare_op_ $ty>]() {
                    test_snapshot(
                        &format!("compare_op_{ty}", ty = stringify!($ty)),
                        &format!(r#"
    public func equals(a: {ty}, b: {ty}) -> bool {{ a == b }}
    public func not_equal(a: {ty}, b: {ty}) -> bool {{ a != b}}
    public func less(a: {ty}, b: {ty}) -> bool {{ a < b }}
    public func less_equal(a: {ty}, b: {ty}) -> bool {{ a <= b }}
    public func greater(a: {ty}, b: {ty}) -> bool {{ a > b }}
    public func greater_equal(a: {ty}, b: {ty}) -> bool {{ a >= b }}
                        "#, ty = stringify!($ty),
                    ));
                }
            }
        )+
    };
}

test_compare_operator_types!(bool, f32, f64, i8, i16, i32, i64, i128, u8, u16, u32, u64, u128);

macro_rules! test_negate_operator_types  {
    ($(
        $ty:ident
     ),+) => {
        $(
            paste::item! {
                #[test]
                fn [<negate_op_ $ty>]() {
                    test_snapshot(
                        &format!("negate_op_{ty}", ty = stringify!($ty)),
                        &format!(r#"
    public func negate(a: {ty}) -> {ty} {{ -a }}
                        "#, ty = stringify!($ty),
                    ));
                }
            }
        )+
    };
}

test_negate_operator_types!(f32, f64, i8, i16, i32, i64, i128);

macro_rules! test_bit_operator_types  {
    ($(
        $ty:ident
     ),+) => {
        $(
            paste::item! {
                #[test]
                fn [<assign_bit_op_ $ty>]() {
                    test_snapshot(
                        &format!("assign_bit_op_{ty}", ty = stringify!($ty)),
                        &format!(r#"
    public func assign_bitand(a: {ty}, b: {ty}) -> {ty} {{
        a &= b;
        a
    }}
    public func assign_bitor(a: {ty}, b: {ty}) -> {ty} {{
        a |= b;
        a
    }}
    public func assign_bitxor(a: {ty}, b: {ty}) -> {ty} {{
        a ^= b;
        a
    }}
                        "#, ty = stringify!($ty),
                    ));
                }

                #[test]
                fn [<bit_op_ $ty>]() {
                    test_snapshot(
                        &format!("bit_op_{ty}", ty = stringify!($ty)),
                    &format!(r#"
    public func not(a: {ty}) -> {ty} {{ !a }}
    public func bitand(a: {ty}, b: {ty}) -> {ty} {{ a & b }}
    public func bitor(a: {ty}, b: {ty}) -> {ty} {{ a | b }}
    public func bitxor(a: {ty}, b: {ty}) -> {ty} {{ a ^ b }}
                        "#, ty = stringify!($ty),
                    ));
                }
            }
        )+
    };
}

test_bit_operator_types!(bool, i8, i16, i32, i64, i128, u8, u16, u32, u64, u128);

macro_rules! test_shift_operator_types  {
    ($(
        $ty:ident
     ),+) => {
        $(
            paste::item! {
                #[test]
                fn [<assign_shift_op_ $ty>]() {
                    test_snapshot(
                        &format!("assign_shift_op_{ty}", ty = stringify!($ty)),
                        &format!(r#"
    public func assign_leftshift(a: {ty}, b: {ty}) -> {ty} {{
        a <<= b;
        a
    }}
    public func assign_rightshift(a: {ty}, b: {ty}) -> {ty} {{
        a >>= b;
        a
    }}
                        "#, ty = stringify!($ty),
                    ));
                }

                #[test]
                fn [<shift_op_ $ty>]() {
                    test_snapshot(
                        &format!("shift_op_{ty}", ty = stringify!($ty)),
                        &format!(r#"
    public func leftshift(a: {ty}, b: {ty}) -> {ty} {{ a << b }}
    public func rightshift(a: {ty}, b: {ty}) -> {ty} {{ a >> b }}
                        "#, ty = stringify!($ty),
                    ));
                }
            }
        )+
    };
}

test_shift_operator_types!(i8, i16, i32, i64, i128, u8, u16, u32, u64, u128);

#[test]
fn let_statement() {
    test_snapshot(
        "let_statement",
        r#"
    public func main(a:i32) -> i32 {
      let b = a+1
      b
    }
    "#,
    );
}

#[test]
fn invalid_binary_ops() {
    test_snapshot(
        "invalid_binary_ops",
        r#"
    public func main() {
      let a = 3+3.0;
      let b = 3.0+3;
    }
    "#,
    );
}

#[test]
fn update_operators() {
    test_snapshot(
        "update_operators",
        r#"
    public func add(a:i32, b:i32) -> i32 {
      let result = a
      result += b
      result
    }

    public func subtract(a:i32, b:i32) -> i32 {
      let result = a
      result -= b
      result
    }

    public func multiply(a:i32, b:i32) -> i32 {
      let result = a
      result *= b
      result
    }

    public func divide(a:i32, b:i32) -> i32 {
      let result = a
      result /= b
      result
    }

    public func remainder(a:i32, b:i32) -> i32 {
      let result = a
      result %= b
      result
    }
    "#,
    );
}

#[test]
fn update_parameter() {
    test_snapshot(
        "update_parameter",
        r#"
    public func add_three(a:i32) -> i32 {
      a += 3;
      a
    }
    "#,
    );
}

#[test]
fn function_calls() {
    test_snapshot(
        "function_calls",
        r#"
    func add_impl(a:i32, b:i32) -> i32 {
        a+b
    }

    func add(a:i32, b:i32) -> i32 {
      add_impl(a,b)
    }

    public func test() -> i32 {
      add(4,5)
      add_impl(4,5)
      add(4,5)
    }
    "#,
    );
}

#[test]
fn if_statement() {
    test_snapshot(
        "if_statement",
        r#"
    public func foo(a:i32) -> i32 {
        let b = if a > 3 {
            let c = if a > 4 {
                a+1
            } else {
                a+3
            }
            c
        } else {
            a-1
        }
        b
    }
    "#,
    );
}

#[test]
fn void_return() {
    test_snapshot(
        "void_return",
        r#"
    func bar() {
        let a = 3;
    }
    public func foo(a:i32) {
        let c = bar()
    }
    "#,
    );
}

#[test]
fn fibonacci() {
    test_snapshot(
        "fibonacci",
        r#"
    public func fibonacci(n:i32) -> i32 {
        if n <= 1 {
            n
        } else {
            fibonacci(n-1) + fibonacci(n-2)
        }
    }
    "#,
    );
}

#[test]
fn fibonacci_loop() {
    test_snapshot(
        "fibonacci_loop",
        r#"
    public func fibonacci(n:i32) -> i32 {
        let a = 0;
        let b = 1;
        let i = 1;
        loop {
            if i > n {
                return a
            }
            let sum = a + b;
            a = b;
            b = sum;
            i += 1;
        }
    }
    "#,
    );
}

#[test]
fn loop_issue_llvm13() {
    // A bug was surfaced by switching to LLVM13. When using a loop in code an exit
    // block was generated which didnt have a predecessor (because nobody jumped
    // to it), this caused LLVM13 to crash.
    test_snapshot(
        "loop_issue_llvm13",
        r#"
    public func issue() -> i32 {
        loop {
        }
    }
    "#,
    );
}

#[test]
fn shadowing() {
    test_snapshot(
        "shadowing",
        r#"
    public func foo(a:i32) -> i32 {
        let a = a+1;
        {
            let a = a+2;
        }
        a+3
    }

    public func bar(a:i32) -> i32 {
        let a = a+1;
        let a = {
            let a = a+2;
            a
        }
        a+3
    }
    "#,
    );
}

#[test]
fn return_expr() {
    test_snapshot(
        "return_expr",
        r#"
    public func main() -> i32 {
        return 5;
        let a = 3; // Nothing regarding this statement should be in the IR
    }
    "#,
    );
}

#[test]
fn conditional_return_expr() {
    test_snapshot(
        "conditional_return_expr",
        r#"
    public func main(a:i32) -> i32 {
        if a > 4 {
            return a;
        }
        a - 1
    }
    "#,
    );
}

#[test]
fn never_conditional_return_expr() {
    test_snapshot(
        "never_conditional_return_expr",
        r#"
    public func main(a:i32) -> i32 {
        if a > 4 {
            return a;
        } else {
            return a - 1;
        }
    }
    "#,
    );
}

#[test]
fn true_is_true() {
    test_snapshot(
        "true_is_true",
        r#"
    public func test_true() -> bool {
        true
    }

    public func test_false() -> bool {
        false
    }"#,
    );
}

#[test]
fn loop_expr() {
    test_snapshot(
        "loop_expr",
        r#"
    public func foo() {
        loop {}
    }
    "#,
    );
}

#[test]
fn loop_break_expr() {
    test_snapshot(
        "loop_break_expr",
        r#"
    public func foo(n:i32) -> i32 {
        loop {
            if n > 5 {
                break n;
            }
            if n > 10 {
                break 10;
            }
            n += 1;
        }
    }
    "#,
    );
}

#[test]
fn while_expr() {
    test_snapshot(
        "while_expr",
        r#"
    public func foo(n:i32) {
        while n<3 {
            n += 1;
        };

        // This will be completely optimized out
        while n<4 {
            break;
        };
    }
    "#,
    );
}

#[test]
fn struct_test() {
    test_snapshot_unoptimized(
        "struct_test",
        r#"
    struct Bar(f64, i32, bool, Foo);
    struct Foo { a: i32 };
    struct Baz;
    public func foo() {
        let a: Foo = Foo { a: 5 };
        let b: Bar = Bar(1.23, a.a, true, a);
        let c: Baz = Baz;
    }
    "#,
    );
}

#[test]
fn field_expr() {
    test_snapshot(
        "field_expr",
        r#"
    public struct Bar(f64, Foo);
    public struct Foo { a: i32 };

    func bar_1(bar: Bar) -> Foo {
        bar.1
    }

    func foo_a(foo: Foo) -> i32 {
        foo.a
    }

    public func bar_1_foo_a(bar: Bar) -> i32 {
        foo_a(bar_1(bar))
    }

    public func main() -> i32 {
        let a: Foo = Foo { a: 5 };
        let b: Bar = Bar(1.23, a);
        let aa_lhs = a.a + 2;
        let aa_rhs = 2 + a.a;
        aa_lhs + aa_rhs
    }
    "#,
    );
}

#[test]
fn field_crash() {
    test_snapshot_unoptimized(
        "field_crash",
        r#"
    class Foo { a: i32 };

    public func main(c:i32) -> i32 {
        let b = Foo { a: c + 5 }
        b.a
    }
    "#,
    );
}

#[test]
fn gc_struct() {
    test_snapshot_unoptimized(
        "gc_struct",
        r#"
    class Foo { a: i32, b: i32 };

    public func foo() {
        let a = Foo { a: 3, b: 4 };
        a.b += 3;
        let b = a;
    }
    "#,
    );
}

#[test]
fn intrinsic_load_and_store() {
    // `load_*`/`store_*` must become a bare `inttoptr` plus a `load`/`store`
    // with no call left behind: the whole point is that reaching a
    // caller-owned buffer costs one instruction, not a function call.
    test_snapshot_unoptimized(
        "intrinsic_load_and_store",
        r#"
    extern "codira-intrinsic" {
        func load_u32(addr: usize) -> u32;
        func store_u32(addr: usize, value: u32);
    }
    public func copy_one(src: usize, dst: usize) {
        store_u32(dst, load_u32(src));
    }
    "#,
    );
}

#[test]
fn intrinsic_bit_counting() {
    // `ctlz_u32` must lower to `@llvm.ctlz.i32`, which becomes a single
    // `LZCNT`/`CLZ`. The second argument is the is-zero-poison flag and must
    // be false, so `ctlz_u32(0)` is 32 rather than undefined.
    test_snapshot_unoptimized(
        "intrinsic_bit_counting",
        r#"
    extern "codira-intrinsic" {
        func ctlz_u32(value: u32) -> u32;
        func cttz_u32(value: u32) -> u32;
        func popcount_u32(value: u32) -> u32;
    }
    public func counts(x: u32) -> u32 {
        ctlz_u32(x) + cttz_u32(x) + popcount_u32(x)
    }
    "#,
    );
}

#[test]
fn intrinsic_load_widths() {
    // Every width maps to its own LLVM type and alignment; a `load_u8` that
    // silently loaded 4 bytes would read past a caller's buffer.
    test_snapshot_unoptimized(
        "intrinsic_load_widths",
        r#"
    extern "codira-intrinsic" {
        func load_u8(addr: usize) -> u8;
        func load_u16(addr: usize) -> u16;
        func load_u64(addr: usize) -> u64;
        func load_f32(addr: usize) -> f32;
        func load_f64(addr: usize) -> f64;
    }
    public func widths(p: usize) -> f64 {
        load_f64(p) + load_f32(p) as f64
    }
    public func ints(p: usize) -> u64 {
        load_u64(p) + load_u16(p) as u64 + load_u8(p) as u64
    }
    "#,
    );
}

#[test]
fn extern_fn() {
    test_snapshot(
        "extern_fn",
        r#"
    extern func add(a:i32, b:i32) -> i32;
    public func main() {
        add(3,4);
    }
    "#,
    );
}

#[test]
fn private_fn_only() {
    test_snapshot(
        "private_fn_only",
        r#"
    func private_main() {
        let a = 1;
    }
    "#,
    );
}

#[test]
fn incremental_compilation() {
    let (mut db, file_id) = MockDatabase::with_single_file(
        r#"
        struct Foo(i32);

        public func foo(foo: Foo) -> i32 {
            foo.0
        }
        "#,
    );
    db.set_optimization_level(OptimizationLevel::Default);
    db.set_target(Target::host_target().unwrap());

    let module_group_id = db
        .module_partition()
        .group_for_file(file_id)
        .expect("could not find ModuleGroupId for file");

    {
        let events = db.log_executed(|| {
            db.target_assembly(module_group_id);
        });
        assert!(
            format!("{events:?}").contains("package_defs"),
            "{events:#?}"
        );
        assert!(format!("{events:?}").contains("assembly"), "{events:#?}");
    }

    db.set_optimization_level(OptimizationLevel::Aggressive);

    {
        let events = db.log_executed(|| {
            db.target_assembly(module_group_id);
        });
        println!("events: {events:?}");
        assert!(
            !format!("{events:?}").contains("package_defs"),
            "{events:#?}"
        );
        assert!(format!("{events:?}").contains("assembly"), "{events:#?}");
    }

    // TODO: Try to disconnect `group_ir` and `file_ir`
    // TODO: Add support for multiple files in a group
}

#[test]
fn nested_structs() {
    test_snapshot(
        "nested_structs",
        r#"
    public class GcStruct(f32, f32);
    public struct ValueStruct(f32, f32);

    public class GcWrapper(GcStruct, ValueStruct)
    public struct ValueWrapper(GcStruct, ValueStruct);

    public func new_gc_struct(a: f32, b: f32) -> GcStruct {
        GcStruct(a, b)
    }

    public func new_value_struct(a: f32, b: f32) -> ValueStruct {
        ValueStruct(a, b)
    }

    public func new_gc_wrapper(a: GcStruct, b: ValueStruct) -> GcWrapper {
        GcWrapper(a, b)
    }

    public func new_value_wrapper(a: GcStruct, b: ValueStruct) -> ValueWrapper {
        ValueWrapper(a, b)
    }
    "#,
    );
}

#[test]
fn nested_private_fn() {
    test_snapshot(
        "nested_private_fn",
        r#"
    func nested_private_fn() -> i32 {
        1
    }

    func private_fn() -> i32 {
        nested_private_fn()
    }

    public func main() -> i32 {
        private_fn()
    }
    "#,
    );
}

#[test]
fn nested_private_extern_fn() {
    test_snapshot(
        "nested_private_extern_fn",
        r#"
    extern func extern_fn() -> f32;

    func private_fn() -> f32 {
        extern_fn()
    }

    public func main() -> f32 {
        private_fn()
    }
    "#,
    );
}

#[test]
fn nested_private_recursive_fn() {
    test_snapshot(
        "nested_private_recursive_fn",
        r#"
    func private_fn() -> f32 {
        private_fn()
    }

    public func main() -> f32 {
        private_fn()
    }
    "#,
    );
}

#[test]
fn nested_private_recursive_fn_with_args() {
    test_snapshot(
        "nested_private_recursive_fn_with_args",
        r#"
    extern func other() -> i32;

    func private_fn(a: i32) -> f32 {
        private_fn(a)
    }

    public func main() -> f32 {
        private_fn(other())
    }
    "#,
    );
}

fn test_snapshot(name: &str, text: &str) {
    test_snapshot_with_optimization(name, text, OptimizationLevel::Default);
}

fn test_snapshot_unoptimized(name: &str, text: &str) {
    test_snapshot_with_optimization(name, text, OptimizationLevel::None);
}

fn test_snapshot_with_optimization(name: &str, text: &str, opt: OptimizationLevel) {
    let mut db = MockDatabase::with_files(text);
    db.set_optimization_level(opt);
    db.set_target(Target::host_target().unwrap());

    // Build and extra diagnostics
    let messages = RefCell::new(Vec::new());
    let mut sink = DiagnosticSink::new(|diag| {
        let file_id = diag.source().file_id;
        let line_index = db.line_index(file_id);
        let source_root_id = db.file_source_root(file_id);
        let source_root = db.source_root(source_root_id);
        let relative_path = source_root.relative_path(file_id);
        let line_col = line_index.line_col(diag.highlight_range().start());
        messages.borrow_mut().push(format!(
            "{} ({}:{}): error: {}",
            relative_path,
            line_col.line + 1,
            line_col.col_utf16 + 1,
            diag.message()
        ));
    });
    for module in codira_hir::Package::all(&db)
        .into_iter()
        .flat_map(|package| package.modules(&db))
    {
        module.diagnostics(&db, &mut sink);
    }
    drop(sink);
    let messages = messages.into_inner();

    // Setup code generation
    let llvm_context = Context::create();
    let code_gen = CodeGenContext::new(&llvm_context, &db);
    let module_parition = db.module_partition();

    let value = if messages.is_empty() {
        itertools::Itertools::intersperse(module_parition.iter().map(|(module_group_id, module_group)| {
            let group_ir = gen_file_group_ir(&code_gen, module_group);
            let file_ir = gen_file_ir(&code_gen, &group_ir, module_group);

            let group_ir = group_ir.llvm_module.print_to_string().to_string();
            // println!("=== GROUP IR:\n {} ",&group_ir);
            let file_ir = file_ir.llvm_module.print_to_string().to_string();
            // println!("=== FILE IR:\n {} ",&file_ir);

            // To ensure that we test symbol generation
            let module_builder = AssemblyBuilder::new(&code_gen, &module_parition, module_group_id);
            let _obj_file = module_builder.build().expect("Failed to build object file");

            format!(
                "; == FILE IR ({}) =====================================\n{}\n; == GROUP IR ({}) ====================================\n{}",
                module_group.relative_file_path(),
                file_ir,
                module_group.relative_file_path(),
                group_ir
            )
        }),String::from("\n")).collect::<String>()
    } else {
        itertools::Itertools::intersperse(messages.into_iter(), String::from("\n"))
            .collect::<String>()
    };

    insta::assert_snapshot!(name, value, text);
}

// ---- Eidos whole-function constant folding ------------------------------
//
// These run at `OptimizationLevel::None` deliberately. LLVM's own pipeline
// is switched off, so anything folded in the snapshots below was folded by
// the Eidos mid-level stack (`codira_mir` passes + `codira_egraph`
// equality saturation + `codira_comptime` evaluation) in
// `crate::eidos_fold`, not by the backend.

#[test]
fn eidos_folds_arithmetic_body() {
    test_snapshot_unoptimized(
        "eidos_folds_arithmetic_body",
        r#"
    public func answer() -> i64 {
        40 + 2
    }"#,
    );
}

#[test]
fn eidos_folds_across_a_call() {
    // `outer` is only constant *after* `inner` is inlined -- this is the
    // interprocedural fold that the `inline` pass enables.
    test_snapshot_unoptimized(
        "eidos_folds_across_a_call",
        r#"
    public func inner() -> i64 {
        6 * 7
    }

    public func outer() -> i64 {
        inner() + 0
    }"#,
    );
}

#[test]
fn eidos_declines_when_a_parameter_is_involved() {
    // Not constant: the fold must decline and leave ordinary lowering in
    // place, so the parameter still reaches the arithmetic.
    test_snapshot_unoptimized(
        "eidos_declines_when_a_parameter_is_involved",
        r#"
    public func add_one(a: i64) -> i64 {
        a + 1
    }"#,
    );
}

// ---- `as` casts through the PRODUCTION path -----------------------------
//
// These go through `gen_file_ir` -> `BodyIrGenerator::gen_expr`, the path a
// real `codira build` takes -- not the MIR pipeline. An adversarial review
// of S6 found that every `as` cast ICEd here while the MIR path worked,
// because the two paths are independent. These tests pin the production
// path specifically.

#[test]
fn cast_int_widening_and_narrowing() {
    test_snapshot_unoptimized(
        "cast_int_widening_and_narrowing",
        r#"
    public func widen_signed(a: i32) -> i64 { a as i64 }
    public func widen_unsigned(a: u32) -> i64 { a as i64 }
    public func narrow(a: i64) -> i32 { a as i32 }"#,
    );
}

#[test]
fn cast_int_float_conversions() {
    test_snapshot_unoptimized(
        "cast_int_float_conversions",
        r#"
    public func to_float(a: i32) -> f64 { a as f64 }
    public func from_float(a: f64) -> i32 { a as i32 }
    public func narrow_float(a: f64) -> f32 { a as f32 }"#,
    );
}

#[test]
fn cast_identity_emits_nothing() {
    test_snapshot_unoptimized(
        "cast_identity_emits_nothing",
        r#"
    public func same(a: i32) -> i32 { a as i32 }"#,
    );
}

/// Tuples lower to anonymous LLVM structs built with `insertvalue`, with no
/// allocation and no runtime call -- the property that makes them usable as
/// the stdlib's multiple-return mechanism (`frexp(x) -> (f64, i32)`).
///
/// Unoptimised so the aggregate construction is actually visible; the
/// optimised form is covered by `tuple_is_zero_overhead` below.
#[test]
fn tuple_expr() {
    test_snapshot_unoptimized(
        "tuple_expr",
        r#"
    public func divmod(a: i32, b: i32) -> (i32, i32) {
        (a / b, a % b)
    }

    public func first(t: (i32, f64)) -> i32 {
        t.0
    }

    public func second(t: (i32, f64)) -> f64 {
        t.1
    }

    public func nested(a: i32) -> i32 {
        let t = ((a, a), a);
        t.0.1
    }

    public func unit() -> () {
        ()
    }"#,
    );
}

/// With optimisation on, a tuple used only to carry two values out of a
/// function disappears entirely: the aggregate is scalarised and the whole
/// body folds to a constant. This is the guarantee that matters -- returning
/// a tuple costs nothing versus returning the values some other way, so
/// stdlib signatures never have to trade clarity for speed.
#[test]
fn tuple_is_zero_overhead() {
    test_snapshot(
        "tuple_is_zero_overhead",
        r#"
    func divmod(a: i32, b: i32) -> (i32, i32) {
        (a / b, a % b)
    }

    public func main() -> i32 {
        let qr = divmod(17, 5);
        qr.0 * 100 + qr.1
    }"#,
    );
}

/// Methods take their receiver as LLVM parameter 0, ahead of the value
/// parameters; associated functions (no `self`) take none. Both go through
/// the same `gen_call` a free function does, so they share its dispatch
/// table and hot-reload behaviour rather than living on a parallel path.
#[test]
fn method_call() {
    test_snapshot_unoptimized(
        "method_call",
        r#"
    public struct Counter { value: i32 };

    extend Counter {
        func get(self) -> i32 {
            self.value
        }

        func bumped(self, by: i32) -> Counter {
            Counter { value: self.value + by }
        }

        func make(start: i32) -> Counter {
            Counter { value: start }
        }
    }

    public func chained(start: i32, by: i32) -> i32 {
        Counter.make(start).bumped(by).get()
    }"#,
    );
}

/// `Type.assoc_fn(..)` is static member access (`LANGUAGE_SPEC` section 3),
/// not a method call on a value. Inference recognises the type-path receiver
/// and resolves without a `self`; codegen passes no receiver because the
/// *callee* has no `self` parameter.
#[test]
fn static_member_access() {
    test_snapshot(
        "static_member_access",
        r#"
    struct Counter { value: i32 };

    extend Counter {
        func make(start: i32) -> Counter {
            Counter { value: start }
        }
        func get(self) -> i32 {
            self.value
        }
    }

    public func main() -> i32 {
        Counter.make(7).get()
    }"#,
    );
}

/// Tuple patterns destructure with `extractvalue` and no branch: the arity is
/// fixed by the type and inference has already checked it, so there is no
/// test to emit. Nesting falls out by recursion, and `_` binds nothing.
#[test]
fn tuple_pattern() {
    test_snapshot_unoptimized(
        "tuple_pattern",
        r#"
    public func sum_pair(p: (i64, i64)) -> i64 {
        let (x, y) = p;
        x + y
    }

    public func destructured_param((a, b): (i64, i64)) -> i64 {
        a + b
    }

    public func nested(v: ((i64, i64), i64)) -> i64 {
        let ((p, q), r) = v;
        p + q + r
    }

    public func discards(p: (i64, i64)) -> i64 {
        let (_, keep) = p;
        keep
    }"#,
    );
}

/// The payoff for tuple returns: naming both results of a multi-value
/// function at once. Optimised, so the whole thing folds to a constant --
/// destructuring costs nothing.
#[test]
fn tuple_pattern_is_zero_overhead() {
    test_snapshot(
        "tuple_pattern_is_zero_overhead",
        r#"
    func divmod(a: i64, b: i64) -> (i64, i64) {
        (a / b, a % b)
    }

    public func main() -> i64 {
        let (q, r) = divmod(47, 5);
        q * 100 + r
    }"#,
    );
}
