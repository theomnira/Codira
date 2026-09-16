//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use crate::{
    ast::{self, AstNode},
    SourceFile, SyntaxKind,
};

#[test]
fn class_and_trait_and_extend() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        trait Shape {
            func area(self) -> f64;
            func perimeter(self) -> f64 { 0.0 }
        }

        class Animal {
            var name: i32
        }

        class Dog: Animal, Shape {
            var breed: i32
        }

        extend Dog {
            override func area(self) -> f64 { 0.0 }
        }

        extend Animal {
            func speak(self) -> i32 { 0 }
        }

        extend Animal: Shape {
            func perimeter(self) -> f64 { 1.0 }
        }
        "#,
    )
    .debug_dump());
}

#[test]
fn enum_and_match() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        enum Shape {
            Circle(radius: f64)
            Rectangle(width: f64, height: f64)
            Point
        }

        func area(shape: Shape) -> f64 {
            match shape {
                Shape.Circle(radius) -> radius,
                Shape.Rectangle(width, height) -> width,
                Shape.Point -> 0.0,
                42 -> 1.0,
                _ -> 0.0,
            }
        }
        "#,
    )
    .debug_dump());
}

#[test]
fn comptime_and_generics_and_where() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        comptime func fibonacci(n: i32) -> i32 {
            n
        }

        struct Box[T] {
            value: T
        }

        func max[T](a: T, b: T) -> T where T: Comparable {
            a
        }

        func use_it() -> i32 {
            comptime {
                fibonacci(10)
            }
        }
        "#,
    )
    .debug_dump());
}

#[test]
fn effects_and_handlers() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        effect Logger {
            func log(message: i32)
        }

        func risky() uses Logger -> i32 {
            perform Logger.log(1)
            42
        }

        func run() -> i32 {
            handle risky() {
                Logger.log(message) -> {
                    resume()
                }
            }
        }
        "#,
    )
    .debug_dump());
}

#[test]
fn attributes_and_extern_blocks() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        @derive(Show)
        @heal(on: 1, strategies: 2)
        struct Point {
            x: f64
        }

        extern "C" {
            func sqrt(x: f64) -> f64;
        }

        @export("C")
        public func codira_add(a: i32, b: i32) -> i32 { a }
        "#,
    )
    .debug_dump());
}

#[test]
fn optional_types_and_force_unwrap() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        func find(x: i32) -> i32? {
            nil
        }

        func run() -> i32 {
            let forced = find(1)!;
            forced
        }
        "#,
    )
    .debug_dump());
}

#[test]
fn var_bindings_and_macro_def() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        macro derive_show(target: i32) -> i32 {
            target
        }

        func counter() -> i32 {
            var count = 0;
            count += 1;
            count
        }
        "#,
    )
    .debug_dump());
}

#[test]
fn refinement_types_and_supervisor() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        type RecordId = i32 { x | x > 0 };

        func fetch_record(id: i32 { x | x > 0 }) -> RecordId {
            id
        }

        supervisor DatabaseConnection {
            strategy: 1,
            max_restarts: 5,

            child ConnPool {
                strategy: 2,
            }
        }
        "#,
    )
    .debug_dump());
}

/// Covers the ownership/hardware/concurrency scaffolding keywords in
/// `spec/LANGUAGE_SPEC.md` section 14: `consuming`/`borrowing` parameter
/// conventions, `~Trait` inheritance-list opt-out, `@target(..)` (which
/// reuses the pre-existing generic `@name(..)` attribute grammar), and
/// `spawn <expr>`.
#[test]
fn ownership_and_concurrency_scaffolding() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        struct GPUBuffer: ~Copyable {
            len: i32,
        }

        func consume_it(consuming buf: GPUBuffer) -> i32 {
            buf.len
        }

        func borrow_it(borrowing buf: GPUBuffer) -> i32 {
            buf.len
        }

        @target(gpu)
        func compute() -> i32 {
            spawn compute();
            42
        }
        "#,
    )
    .debug_dump());
}

/// Covers the remaining `spec/LANGUAGE_SPEC.md` section-14 scaffolding:
/// `def` (Python-style dynamic functions with untyped parameters),
/// `inout`/`mut`/`out` parameter conventions, postfix `x^` transfer,
/// `<-ch` channel receive, and `ch <- x` channel send.
#[test]
fn transfer_channels_and_def() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        def greet(name, count = 3) -> nil {
            nil
        }

        func swap(inout a: i32, inout b: i32) {
            let tmp = a^;
            a = b^;
            b = tmp;
        }

        func pipeline(ch: Channel[i32]) -> i32 {
            let got = <-ch;
            ch <- got + 1;
            got
        }
        "#,
    )
    .debug_dump());
}

#[test]
fn tuple_record() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        public struct Foo(public i32, i32);
        "#
    )
    .debug_dump());
}

#[test]
fn method_call() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        func main() {
            a.foo();
            a.0.foo();
            a.0.0.foo();
            a.0 .f32();
        }
        "#
    )
    .debug_dump());
}

#[test]
fn index_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func main() {
        let a = [1,2,3,4]
        let b = a[0];
        let c = a[b];
        a[0] = c;
        let a = { [3,4,5] }[1];
    }"#,
    )
    .debug_dump());
}

#[test]
fn array_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func main() {
        let a = [1,2,3,]
        let a = []
        let a = [call(123)]
        let a = [Struct { }, Struct { }]
    }"#,
    )
    .debug_dump());
}

#[test]
fn missing_field_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        value.
    }"#,
    )
    .debug_dump());
}

#[test]
fn impl_block() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        extend Foo {}
        extend Bar {
            func bar() {}
            struct Baz {}
        }
        public extend FooBar {}
        "#
    )
    .debug_dump());
}

#[test]
fn array_type() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func main(a: [int]) {
        let a:[[bool]];
    }"#,
    )
    .debug_dump());
}

#[test]
fn empty() {
    insta::assert_snapshot!(SourceFile::parse(r#""#).debug_dump(), @"SOURCE_FILE@0..0
");
}

#[test]
fn function() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    // Source file comment

    // Comment that belongs to the function
    func a() {}
    func b(value:number) {}
    public func d() {}
    public func c()->never {}
    func b(value:number)->number {}"#,
    )
    .debug_dump());
}

#[test]
fn block() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        let a;
        let b:i32;
        let c:string;
    }"#,
    )
    .debug_dump());
}

#[test]
fn literals() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        let a = true;
        let b = false;
        let c = 1;
        let d = 1.12;
        let e = "Hello, world!"
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn struct_def() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    struct Foo      // error: expected a ';', or a '{'
    struct Foo;
    struct Foo;;    // error: expected a declaration
    struct Foo {}
    struct Foo {};
    struct Foo {,}; // error: expected a field declaration
    struct Foo {
        a: f64,
    }
    struct Foo {
        a: f64,
        b: i32,
    };
    struct Foo()
    struct Foo();
    struct Foo(,);  // error: expected a type
    struct Foo(f64)
    struct Foo(f64,);
    struct Foo(f64, i32)
    "#,
    )
    .debug_dump());
}

#[test]
fn unary_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        let a = --3;
        let b = !!true;
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn binary_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        let a = 3+4*5
        let b = 3*4+10/2
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn expression_statement() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        let a = "hello"
        let b = "world"
        let c
        b = "Hello, world!"
        !-5+2*(a+b);
        -3
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn function_calls() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func bar(i:number) { }
    func foo(i:number) {
      bar(i+1)
    }
    func baz(self) { }
    func qux(self, i:number) { }
    func foo(self i:number) { } // error: expected comma
    "#,
    )
    .debug_dump());
}

#[test]
fn patterns() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func main(_:number) {
       let a = 0;
       let _ = a;
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn arithmetic_operands() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func main() {
        let _ = a + b;
        let _ = a - b;
        let _ = a * b;
        let _ = a / b;
        let _ = a % b;
        let _ = a << b;
        let _ = a >> b;
        let _ = a & b;
        let _ = a | b;
        let _ = a ^ b;
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn assignment_operands() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func main() {
        let a = b;
        a += b;
        a -= b;
        a *= b;
        a /= b;
        a %= b;
        a <<= b;
        a >>= b;
        a &= b;
        a |= b;
        a ^= b;
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn compare_operands() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func main() {
        let _ = a == b;
        let _ = a == b;
        let _ = a != b;
        let _ = a < b;
        let _ = a > b;
        let _ = a <= b;
        let _ = a >= b;
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn logic_operands() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func main() {
        let _ = a || b;
        let _ = a && b;
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn if_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func bar() {
        if true {};
        if true {} else {};
        if true {} else if false {} else {};
        if {true} {} else {}
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn block_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func bar() {
        {3}
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn return_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        return;
        return 50;
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn loop_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        loop {}
    }"#,
    )
    .debug_dump());
}

#[test]
fn break_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        break;
        if break { 3; }
        if break 4 { 3; }
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn while_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        while true {};
        while { true } {};
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn struct_lit() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func foo() {
        U;
        S {};
        S { x, y: 32, };
        S { x: 32, y: 64 };
        TupleStruct { 0: 1 };
        T(1.23);
        T(1.23, 4,)
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn struct_field_index() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func main() {
        foo.a
        foo.a.b
        foo.0
        foo.0.1
        foo.10
        foo.01  // index: .0
        foo.0 1 // index: .0 
        foo.a.0
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn struct_and_class_memory_kind() {
    // The `(gc)`/`(value)` parenthetical annotation is gone: `struct` is
    // always a value type and `class` is always a heap-allocated,
    // garbage-collected reference type -- the keyword choice *is* the memory
    // kind now.
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    struct Foo {};
    class Baz {};
    struct Bar {};
    "#,
    )
    .debug_dump());
}

#[test]
fn data_struct_and_class() {
    // `data` (see spec/LANGUAGE_SPEC.md section 16) is a contextual keyword,
    // only promoted when immediately followed by `struct`/`class` -- it must
    // stay usable as an ordinary identifier everywhere else (a field name,
    // a binding, a plain function call).
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    data struct Point {
        x: f64,
        y: f64,
    }
    data class Actor {
        var health: i32,
    }
    public data struct Pair[T] {
        first: T,
        second: T,
    }
    func use_data_as_ident() {
        let data = 5;
        data
    }
    struct HasDataField {
        data: i32,
    }
    "#,
    )
    .debug_dump());
}

#[test]
fn visibility() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    public struct Foo {};
    internal class Baz {};
    internal func foo() {}
    internal func bar() {}
    public func baz() {}
    "#,
    )
    .debug_dump());
}

#[test]
fn extern_fn() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    public extern func foo();
    "#,
    )
    .debug_dump());
}

#[test]
fn type_alias_def() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    type Foo = i32;
    type Bar = Foo;
    "#,
    )
    .debug_dump());
}

#[test]
fn function_return_path() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        func main() -> self.Foo {}
        func main1() -> super.Foo {}
        func main2() -> root.Foo {}
        func main3() -> root.foo.Foo {}
    "#,
    )
    .debug_dump());
}

#[test]
fn use_() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
        // Simple paths
        import package_name;
        import self.item_in_scope_or_package_name;
        import self.module.Item;
        import root.Item;
        import self.some.Struct;
        import root.some_item;

        // Use tree list
        import crate.{Item};
        import self.{Item};

        // Wildcard import
        import *; // Error
        import .*; // Error
        import crate.*;
        import crate.{*};

        // Renames
        import some.path as some_name;
        import some.{
            other.path as some_other_name,
            different.path as different_name,
            yet.another.path,
            running.out.of.synonyms.for_.different.*
        };
        import Foo as _;
        "#,
    )
    .debug_dump());
}

// --------------------------------------------------------------------------
// `expr as Type` casts
// --------------------------------------------------------------------------

/// Parses `func main() { let _ = <src>; }` and hands back the initializer of
/// that single `let`, asserting along the way that the source parsed without a
/// syntax error. The precedence tests below are all about the *shape* of one
/// expression, so going through the typed AST states exactly that and nothing
/// more -- unlike a whole-file snapshot, which also pins down every scrap of
/// trivia around it.
fn only_initializer(src: &str) -> ast::Expr {
    let text = format!("func main() {{ let _ = {src}; }}");
    let parse = SourceFile::parse(&text);
    assert!(
        parse.errors().is_empty(),
        "`{src}` failed to parse: {:?}",
        parse.errors()
    );

    let let_stmt = parse
        .syntax_node()
        .descendants()
        .find_map(ast::LetStmt::cast)
        .expect("a LET_STMT");
    let_stmt
        .initializer()
        .unwrap_or_else(|| panic!("`{src}` produced a `let` with no initializer"))
}

/// Unwraps `expr` as a `CastExpr`, naming what was found instead on failure.
fn as_cast(expr: &ast::Expr) -> ast::CastExpr {
    ast::CastExpr::cast(expr.syntax().clone())
        .unwrap_or_else(|| panic!("expected a CAST_EXPR, found {:?}", expr.syntax().kind()))
}

/// Unwraps `expr` as a `BinExpr`, naming what was found instead on failure.
fn as_bin(expr: &ast::Expr) -> ast::BinExpr {
    ast::BinExpr::cast(expr.syntax().clone())
        .unwrap_or_else(|| panic!("expected a BIN_EXPR, found {:?}", expr.syntax().kind()))
}

/// The simplest cast: a path operand and a primitive target type.
#[test]
fn cast_expr_simple() {
    let cast = as_cast(&only_initializer("x as i32"));
    assert_eq!(cast.expr().unwrap().syntax().text(), "x");
    assert_eq!(cast.type_ref().unwrap().syntax().text(), "i32");
}

/// Casts chain left-associatively: `x as i32 as i64` is `((x as i32) as i64)`,
/// never `x as (i32 as i64)` (which would not even name a type). Chains have
/// to work -- the stdlib uses them to widen through an intermediate width.
#[test]
fn cast_expr_chained_is_left_associative() {
    let outer = as_cast(&only_initializer("x as i32 as i64"));
    assert_eq!(outer.type_ref().unwrap().syntax().text(), "i64");

    let inner = as_cast(&outer.expr().unwrap());
    assert_eq!(inner.type_ref().unwrap().syntax().text(), "i32");
    assert_eq!(inner.expr().unwrap().syntax().text(), "x");
}

/// `as` binds tighter than every binary operator, so a cast to the left of one
/// claims only its own operand: `a as i32 + b` is `BinExpr(CastExpr(a), b)`.
#[test]
fn cast_expr_binds_tighter_than_binary_lhs() {
    let bin = as_bin(&only_initializer("a as i32 + b"));
    let lhs = as_cast(&bin.lhs().unwrap());
    assert_eq!(lhs.expr().unwrap().syntax().text(), "a");
    assert_eq!(lhs.type_ref().unwrap().syntax().text(), "i32");
    assert_eq!(bin.rhs().unwrap().syntax().text(), "b");
}

/// The mirror image: `a + b as i32` is `BinExpr(a, CastExpr(b))`. This holds
/// for the tightest-binding binary operators too (`a * b as i32`), since `as`
/// outranks all of them.
#[test]
fn cast_expr_binds_tighter_than_binary_rhs() {
    let bin = as_bin(&only_initializer("a + b as i32"));
    assert_eq!(bin.lhs().unwrap().syntax().text(), "a");
    let rhs = as_cast(&bin.rhs().unwrap());
    assert_eq!(rhs.expr().unwrap().syntax().text(), "b");
    assert_eq!(rhs.type_ref().unwrap().syntax().text(), "i32");

    let bin = as_bin(&only_initializer("a * b as i32"));
    assert_eq!(bin.rhs().unwrap().syntax().kind(), SyntaxKind::CAST_EXPR);
}

/// `as` binds *looser* than the postfix operators, so `f() as u8` casts the
/// call's result rather than trying to cast the callee.
#[test]
fn cast_expr_of_call_casts_the_result() {
    let cast = as_cast(&only_initializer("f() as u8"));
    let operand = cast.expr().unwrap();
    assert_eq!(operand.syntax().kind(), SyntaxKind::CALL_EXPR);
    assert_eq!(operand.syntax().text(), "f()");
    assert_eq!(cast.type_ref().unwrap().syntax().text(), "u8");
}

/// Looser than the prefix operators as well: `-x as i64` is `(-x) as i64`.
#[test]
fn cast_expr_of_prefix_casts_the_result() {
    let cast = as_cast(&only_initializer("-x as i64"));
    assert_eq!(
        cast.expr().unwrap().syntax().kind(),
        SyntaxKind::PREFIX_EXPR
    );
    assert_eq!(cast.type_ref().unwrap().syntax().text(), "i64");
}

/// The target is a full type, not merely a primitive name: a qualified path
/// and a generic argument list both parse. Whether such a cast is *legal* is a
/// later type-checking question, not a parsing one.
#[test]
fn cast_expr_path_and_generic_target_types() {
    let cast = as_cast(&only_initializer("x as Foo"));
    assert_eq!(cast.type_ref().unwrap().syntax().text(), "Foo");

    let cast = as_cast(&only_initializer("x as bit.Mask"));
    assert_eq!(cast.type_ref().unwrap().syntax().text(), "bit.Mask");

    let cast = as_cast(&only_initializer("x as Box[i32]"));
    assert_eq!(cast.type_ref().unwrap().syntax().text(), "Box[i32]");
}

/// A cast is an ordinary expression, so it appears wherever one does --
/// including as a `let` initializer sitting next to a type ascription, which
/// is how the stdlib writes the overwhelming majority of its conversions.
#[test]
fn cast_expr_in_let_initializer() {
    let parse = SourceFile::parse(
        r#"
    func main() {
        let widened: u64 = narrow as u64;
    }
    "#,
    );
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());

    let let_stmt = parse
        .syntax_node()
        .descendants()
        .find_map(ast::LetStmt::cast)
        .expect("a LET_STMT");
    let cast = as_cast(&let_stmt.initializer().unwrap());
    assert_eq!(cast.expr().unwrap().syntax().text(), "narrow");
    assert_eq!(cast.type_ref().unwrap().syntax().text(), "u64");
}

/// A whole-tree snapshot of the same cases, pinning down the exact `CAST_EXPR`
/// nesting and the `AS_KW` token's place inside it.
#[test]
fn cast_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func main() {
        let _ = x as i32;
        let _ = x as i32 as i64;
        let _ = a as i32 + b;
        let _ = a + b as i32;
        let _ = f() as u8;
        let _ = -x as i64;
        let _ = x as Foo;
        let _ = x as Box[i32];
        let widened: u64 = narrow as u64;
        let _ = (a as i32) * (b as i32);
    }
    "#,
    )
    .debug_dump());
}

// ===========================================================================
// Tuples
// ===========================================================================
//
// Tuples are the language's multiple-return mechanism (`frexp(x) -> (f64,
// i32)` and friends throughout `std/math`). HIR and codegen already modelled
// them -- `TypeRef::Tuple`, `TyKind::Tuple`, `HirTypes::get_tuple_type` --
// so only the grammar was missing. What these tests pin down is the one rule
// that is easy to get wrong in both positions at once: it is the *comma*,
// not the element count, that separates grouping from a 1-tuple.

/// Unwraps `expr` as a `TupleExpr`, naming what was found instead on failure.
fn as_tuple(expr: &ast::Expr) -> ast::TupleExpr {
    ast::TupleExpr::cast(expr.syntax().clone())
        .unwrap_or_else(|| panic!("expected a TUPLE_EXPR, found {:?}", expr.syntax().kind()))
}

/// Parses `src` as a function's return type and hands back the typed node.
fn only_return_type(src: &str) -> ast::TypeRef {
    let text = format!("func f() -> {src} {{ }}");
    let parse = SourceFile::parse(&text);
    assert!(
        parse.errors().is_empty(),
        "`{src}` failed to parse: {:?}",
        parse.errors()
    );
    parse
        .syntax_node()
        .descendants()
        .find_map(ast::RetType::cast)
        .expect("a RET_TYPE")
        .type_ref()
        .unwrap_or_else(|| panic!("`{src}` produced a `->` with no type"))
}

/// `(A, B)` is a tuple type whose element list is exactly what was written.
#[test]
fn tuple_type_multiple_elements() {
    let ty = only_return_type("(f64, i32)");
    let tuple = ast::TupleType::cast(ty.syntax().clone()).expect("a TUPLE_TYPE");
    let fields: Vec<_> = tuple
        .fields()
        .map(|f| f.syntax().text().to_string())
        .collect();
    assert_eq!(fields, vec!["f64", "i32"]);
}

/// `()` is the unit type: a tuple type with no elements. HIR lowers this to
/// `TypeRef::Tuple(vec![])`, the very thing `TypeRefMapBuilder::unit`
/// produces, so an explicitly written `()` and an omitted return type end up
/// as the same type.
#[test]
fn tuple_type_unit_has_no_elements() {
    let ty = only_return_type("()");
    let tuple = ast::TupleType::cast(ty.syntax().clone()).expect("a TUPLE_TYPE");
    assert_eq!(tuple.fields().count(), 0);
}

/// `(A)` is grouping, not a 1-tuple -- it parses to a `PAREN_TYPE`, which HIR
/// lowers straight through to `A`. Without this rule `(i32)` and `i32` would
/// silently become different types.
#[test]
fn tuple_type_single_without_comma_is_grouping() {
    let ty = only_return_type("(i32)");
    assert_eq!(ty.syntax().kind(), SyntaxKind::PAREN_TYPE);
    let paren = ast::ParenType::cast(ty.syntax().clone()).expect("a PAREN_TYPE");
    assert_eq!(paren.type_ref().unwrap().syntax().text(), "i32");
}

/// `(A,)` *is* a 1-tuple. The trailing comma is the only way to write one, so
/// it has to survive parsing rather than being swallowed as punctuation.
#[test]
fn tuple_type_single_with_comma_is_a_one_tuple() {
    let ty = only_return_type("(i32,)");
    let tuple = ast::TupleType::cast(ty.syntax().clone()).expect("a TUPLE_TYPE");
    assert_eq!(tuple.fields().count(), 1);
}

/// Tuple types nest, and compose with the other type constructors (`[T]`,
/// `T?`) in both directions.
#[test]
fn tuple_type_nests_and_composes() {
    let ty = only_return_type("((i32, i32), [f64], bool?)");
    let tuple = ast::TupleType::cast(ty.syntax().clone()).expect("a TUPLE_TYPE");
    let fields: Vec<_> = tuple.fields().map(|f| f.syntax().kind()).collect();
    assert_eq!(
        fields,
        vec![
            SyntaxKind::TUPLE_TYPE,
            SyntaxKind::ARRAY_TYPE,
            SyntaxKind::OPTIONAL_TYPE
        ]
    );
}

/// The expression side mirrors the type side element for element.
#[test]
fn tuple_expr_multiple_elements() {
    let tuple = as_tuple(&only_initializer("(1, 2.0, x)"));
    let exprs: Vec<_> = tuple
        .exprs()
        .map(|e| e.syntax().text().to_string())
        .collect();
    assert_eq!(exprs, vec!["1", "2.0", "x"]);
}

/// `()` is the unit value, and it is a `TUPLE_EXPR` with no elements -- the
/// exact expression-position analogue of the unit type.
#[test]
fn tuple_expr_unit_has_no_elements() {
    let expr = only_initializer("()");
    let tuple = as_tuple(&expr);
    assert_eq!(tuple.exprs().count(), 0);
}

/// `(a)` stays a `PAREN_EXPR`, so adding tuples did not change what
/// parenthesising an expression means. This is the case most at risk from
/// the comma rule, and the reason `a * (b + c)` still groups rather than
/// building a 1-tuple.
#[test]
fn tuple_expr_single_without_comma_is_grouping() {
    let expr = only_initializer("(a + b)");
    assert_eq!(expr.syntax().kind(), SyntaxKind::PAREN_EXPR);
}

/// `(a,)` is a 1-tuple, matching `(A,)` on the type side.
#[test]
fn tuple_expr_single_with_comma_is_a_one_tuple() {
    let tuple = as_tuple(&only_initializer("(a,)"));
    assert_eq!(tuple.exprs().count(), 1);
}

/// A tuple element is a full expression, so a comma inside a nested call's
/// argument list belongs to that call and must not split the tuple.
#[test]
fn tuple_expr_elements_are_full_expressions() {
    let tuple = as_tuple(&only_initializer("(f(a, b), c + d)"));
    let exprs: Vec<_> = tuple
        .exprs()
        .map(|e| e.syntax().text().to_string())
        .collect();
    assert_eq!(exprs, vec!["f(a, b)", "c + d"]);
}

/// Tuple element access reuses the ordinary postfix `.field` machinery, so
/// `t.0` is a `FIELD_EXPR` -- the same node `point.x` produces. Inference
/// tells the two apart by the receiver's type (`Name::as_tuple_index`).
#[test]
fn tuple_field_access_is_a_field_expr() {
    let expr = only_initializer("t.0");
    assert_eq!(expr.syntax().kind(), SyntaxKind::FIELD_EXPR);
}

/// A whole-tree snapshot of the cases above, pinning down the exact
/// `TUPLE_EXPR`/`PAREN_EXPR` split and where the commas land.
#[test]
fn tuple_expr() {
    insta::assert_snapshot!(SourceFile::parse(
        r#"
    func f() -> (f64, i32) {
        let _ = ();
        let _ = (a);
        let _ = (a,);
        let _ = (1, 2.0, x);
        let _ = (f(a, b), c + d);
        let _ = t.0 + t.1;
        let nested: ((i32, i32), f64) = ((1, 2), 3.0);
        (1.0, 2)
    }
    "#,
    )
    .debug_dump());
}

// ===========================================================================
// Statement termination across line breaks
// ===========================================================================
//
// LANGUAGE_SPEC section 1: "`;` ... is **never** required at the end of a
// line-terminated statement." Honouring that needs the grammar to know one
// whitespace-derived fact -- whether a postfix `(` or `[` begins a line --
// because those are the only postfix operators that can also start a
// statement. Everything else about the grammar stays whitespace-insensitive.
//
// The failure mode this prevents is the dangerous kind: before the rule
// existed, the case below parsed *successfully* as `f()(x) + g(2)`, so the
// code compiled and meant something other than it read.

/// A `(` on a new line starts a statement; it does not call the previous
/// line's result.
#[test]
fn newline_before_paren_ends_the_statement() {
    let parse = SourceFile::parse(
        "func main() -> i64 {
    let x = f()
    (x) + g(2)
}",
    );
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());

    // The `let` initializer must be exactly `f()` -- not `f()(x)`.
    let let_stmt = parse
        .syntax_node()
        .descendants()
        .find_map(ast::LetStmt::cast)
        .expect("a LET_STMT");
    assert_eq!(let_stmt.initializer().unwrap().syntax().text(), "f()");
}

/// The same rule for `[`: a new line starting with `[` is an array literal
/// statement, not an index into the previous expression.
#[test]
fn newline_before_bracket_ends_the_statement() {
    let parse = SourceFile::parse(
        "func main() -> i64 {
    let x = f()
    [1, 2][0]
}",
    );
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());

    let let_stmt = parse
        .syntax_node()
        .descendants()
        .find_map(ast::LetStmt::cast)
        .expect("a LET_STMT");
    assert_eq!(let_stmt.initializer().unwrap().syntax().text(), "f()");
}

/// On the *same* line, `(` and `[` still mean call and index -- the rule is
/// about line breaks only, so nothing about ordinary expressions changes.
#[test]
fn same_line_paren_and_bracket_still_call_and_index() {
    let call = only_initializer("f()(x)");
    assert_eq!(call.syntax().kind(), SyntaxKind::CALL_EXPR);
    assert_eq!(call.syntax().text(), "f()(x)");

    let index = only_initializer("f()[0]");
    assert_eq!(index.syntax().kind(), SyntaxKind::INDEX_EXPR);
}

/// A `(` continuing a genuinely unfinished line still opens an argument
/// list: the rule keys on the line break before the `(`, not on where the
/// callee started, so an argument list split across lines is unaffected.
#[test]
fn multiline_argument_list_is_unaffected() {
    let parse = SourceFile::parse(
        "func main() -> i64 {
    let x = g(
        1,
        2
    )
    x
}",
    );
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());

    let let_stmt = parse
        .syntax_node()
        .descendants()
        .find_map(ast::LetStmt::cast)
        .expect("a LET_STMT");
    assert_eq!(
        let_stmt.initializer().unwrap().syntax().kind(),
        SyntaxKind::CALL_EXPR
    );
}

/// A leading-dot continuation is deliberately exempt: no statement can begin
/// with `.`, so `value\n    .method()` has only one possible reading and
/// stays idiomatic method chaining.
#[test]
fn newline_before_dot_still_chains() {
    let parse = SourceFile::parse(
        "func main() -> i64 {
    let x = f()
        .method()
        .other()
    x
}",
    );
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());

    let let_stmt = parse
        .syntax_node()
        .descendants()
        .find_map(ast::LetStmt::cast)
        .expect("a LET_STMT");
    assert_eq!(
        let_stmt.initializer().unwrap().syntax().kind(),
        SyntaxKind::METHOD_CALL_EXPR
    );
}

/// An explicit `;` is still accepted and means the same thing, so existing
/// semicolon-terminated code is unaffected by the rule.
#[test]
fn explicit_semicolon_still_terminates() {
    let parse = SourceFile::parse(
        "func main() -> i64 {
    let x = f();
    (x) + g(2)
}",
    );
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());

    let let_stmt = parse
        .syntax_node()
        .descendants()
        .find_map(ast::LetStmt::cast)
        .expect("a LET_STMT");
    assert_eq!(let_stmt.initializer().unwrap().syntax().text(), "f()");
}

/// A tuple pattern destructures a tuple, and follows the same comma rule as
/// the tuple type and tuple expression: `(a)` is grouping, `(a,)` is a
/// 1-tuple.
#[test]
fn tuple_pat_destructures() {
    let parse = SourceFile::parse(
        "func main() -> i64 {
    let (x, y) = p
    let ((a, b), c) = q
    let (_, keep) = r
    let (single) = s
    let (one,) = t
    x
}",
    );
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());

    let kinds: Vec<_> = parse
        .syntax_node()
        .descendants()
        .filter_map(ast::LetStmt::cast)
        .map(|it| it.pat().unwrap().syntax().kind())
        .collect();
    assert_eq!(
        kinds,
        vec![
            SyntaxKind::TUPLE_PAT,
            SyntaxKind::TUPLE_PAT,
            SyntaxKind::TUPLE_PAT,
            // `(single)` is grouping, so it is a PAREN_PAT -- HIR lowers it
            // straight through to the inner `BIND_PAT`.
            SyntaxKind::PAREN_PAT,
            // `(one,)` is a genuine 1-tuple; the trailing comma is what
            // makes the two distinguishable at all.
            SyntaxKind::TUPLE_PAT,
        ]
    );
}

/// A tuple pattern is irrefutable, so it is legal in a parameter position
/// where a literal or tuple-struct pattern would not be.
#[test]
fn tuple_pat_in_parameter() {
    let parse = SourceFile::parse("func f(p: (i32, i32)) -> i32 { p.0 }");
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());

    let parse = SourceFile::parse("func f((a, b): (i32, i32)) -> i32 { a + b }");
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());
}
