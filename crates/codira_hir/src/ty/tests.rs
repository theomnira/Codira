//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{fmt::Write, sync::Arc};

use codira_hir_input::WithFixture;

use crate::{
    code_model::AssocItem, diagnostics::DiagnosticSink, expr::BodySourceMap, mock::MockDatabase,
    HirDisplay, InferenceResult, ModuleDef, Package,
};

#[test]
fn issue_354() {
    insta::assert_snapshot!(infer(
        r#"
    func value() -> i64 { 6 }

    public func main() {
        let t = 2;
        t = loop { break value(); };
    }"#),
    @"
    20..25 '{ 6 }': i64
    22..23 '6': i64
    46..97 '{     ...; }; }': ()
    56..57 't': i64
    60..61 '2': i64
    67..68 't': i64
    67..94 't = lo...e(); }': ()
    71..94 'loop {...e(); }': i64
    76..94 '{ brea...e(); }': never
    78..91 'break value()': never
    84..89 'value': function value() -> i64
    84..91 'value()': i64
    ");
}

#[test]
fn array_element_assignment() {
    insta::assert_snapshot!(infer(
        r"
    func main() {
        let a = [1,2,3,4,5]
        a[2] = 4u8
    }",
    ), @"
    12..54 '{     ... 4u8 }': ()
    22..23 'a': [u8]
    26..37 '[1,2,3,4,5]': [u8]
    27..28 '1': u8
    29..30 '2': u8
    31..32 '3': u8
    33..34 '4': u8
    35..36 '5': u8
    42..43 'a': [u8]
    42..46 'a[2]': u8
    42..52 'a[2] = 4u8': ()
    44..45 '2': i32
    49..52 '4u8': u8
    ");
}

#[test]
fn array_is_place_expr() {
    insta::assert_snapshot!(infer(
        r"
    func main() {
        let a = [1,2,3,4,5]
        a = [5,6,7]
        a[0] = 0;
        [1,2,3][0] = 4
    }",
    ), @"
    12..88 '{     ... = 4 }': ()
    22..23 'a': [i32]
    26..37 '[1,2,3,4,5]': [i32]
    27..28 '1': i32
    29..30 '2': i32
    31..32 '3': i32
    33..34 '4': i32
    35..36 '5': i32
    42..43 'a': [i32]
    42..53 'a = [5,6,7]': ()
    46..53 '[5,6,7]': [i32]
    47..48 '5': i32
    49..50 '6': i32
    51..52 '7': i32
    58..59 'a': [i32]
    58..62 'a[0]': i32
    58..66 'a[0] = 0': ()
    60..61 '0': i32
    65..66 '0': i32
    72..79 '[1,2,3]': [i32]
    72..82 '[1,2,3][0]': i32
    72..86 '[1,2,3][0] = 4': ()
    73..74 '1': i32
    75..76 '2': i32
    77..78 '3': i32
    80..81 '0': i32
    85..86 '4': i32
    ");
}

#[test]
fn infer_array_structs() {
    insta::assert_snapshot!(infer(
        r"
        struct Foo;

    func main() -> Foo {
        let a = [Foo, Foo, Foo];
        a[2]
    }",
    ), @"
    36..77 '{     ...a[2] }': Foo
    46..47 'a': [Foo]
    50..65 '[Foo, Foo, Foo]': [Foo]
    51..54 'Foo': Foo
    56..59 'Foo': Foo
    61..64 'Foo': Foo
    71..72 'a': [Foo]
    71..75 'a[2]': Foo
    73..74 '2': i32
    ");
}

#[test]
fn infer_array() {
    insta::assert_snapshot!(infer(
        r"
    func main() -> u8 {
        let b = 5;
        let c = 3;
        let a = [1,b,4,c];
        a[3]
    }",
    ), @"
    18..83 '{     ...a[3] }': u8
    28..29 'b': u8
    32..33 '5': u8
    43..44 'c': u8
    47..48 '3': u8
    58..59 'a': [u8]
    62..71 '[1,b,4,c]': [u8]
    63..64 '1': u8
    65..66 'b': u8
    67..68 '4': u8
    69..70 'c': u8
    77..78 'a': [u8]
    77..81 'a[3]': u8
    79..80 '3': i32
    ");
}

// `private_access` used to exercise old Codira's Rust-style fine-grained
// visibility (`pub(super)`/`pub(package)`) via `package::`/`super::`
// multi-segment paths in expression position. Codira intentionally
// simplifies this to a 3-tier model (private-by-default, `internal`,
// `public` -- see spec/LANGUAGE_SPEC.md section 1) and expression-position
// paths are single-segment only (cross-module values are brought into
// scope with `import`, see section 2), so this is rewritten rather than
// mechanically translated: private stays visible to the declaring module
// and its descendants (checked from `foo/baz.code`, a submodule of `foo`),
// while `bar.code` (an unrelated sibling) and `mod.code` (foo's ancestor,
// not a descendant) may only reach `internal`/`public` items.
#[test]
fn private_access() {
    insta::assert_snapshot!(infer(
        r#"
    //- /foo.code
    struct Foo {};
    struct Bar(i64);
    struct Baz;
    func foo() {}
    type FooBar = Foo;

    internal struct IntFoo {};
    internal struct IntBar(i64);
    internal struct IntBaz;
    internal func int_foo() {}
    internal type IntFooBar = IntFoo;

    public struct PubFoo {};
    public struct PubBar(i64);
    public struct PubBaz;
    public func pub_foo() {}
    public type PubFooBar = PubFoo;

    //- /bar.code
    import super.foo.Foo // private access
    import super.foo.Bar // private access
    import super.foo.Baz // private access
    import super.foo.FooBar // private access
    import super.foo.foo // private access

    import super.foo.IntFoo
    import super.foo.IntBar
    import super.foo.IntBaz
    import super.foo.IntFooBar
    import super.foo.int_foo

    import super.foo.PubFoo
    import super.foo.PubBar
    import super.foo.PubBaz
    import super.foo.PubFooBar
    import super.foo.pub_foo

    func main() {
        let a = IntFoo {};
        let a = IntBar(3);
        let a = IntBaz;
        let a = IntFooBar{};
        int_foo();

        let a = PubFoo {};
        let a = PubBar(3);
        let a = PubBaz;
        let a = PubFooBar{};
        pub_foo();
    }

    //- /foo/baz.code
    import super.Foo
    import super.Bar
    import super.Baz
    import super.FooBar
    import super.foo

    func main() {
        let a = Foo {};
        let a = Bar(3);
        let a = Baz;
        let a = FooBar{};
        foo();
    }

    //- /mod.code
    import foo.Foo // private access
    import foo.foo // private access

    import foo.IntFoo
    import foo.int_foo

    func main() {
        let a = IntFoo {};
        int_foo();
    }
    "#),
    @"
    7..14: unresolved import
    40..47: unresolved import
    7..20: unresolved import
    46..59: unresolved import
    85..98: unresolved import
    124..140: unresolved import
    166..179: unresolved import
    117..158 '{     ...o(); }': ()
    127..128 'a': IntFoo
    131..140 'IntFoo {}': IntFoo
    146..153 'int_foo': function int_foo() -> ()
    146..155 'int_foo()': ()
    461..677 '{     ...o(); }': ()
    471..472 'a': IntFoo
    475..484 'IntFoo {}': IntFoo
    494..495 'a': IntBar
    498..504 'IntBar': ctor IntBar(i64) -> IntBar
    498..507 'IntBar(3)': IntBar
    505..506 '3': i64
    517..518 'a': IntBaz
    521..527 'IntBaz': IntBaz
    537..538 'a': IntFooBar
    541..552 'IntFooBar{}': IntFooBar
    558..565 'int_foo': function int_foo() -> ()
    558..567 'int_foo()': ()
    578..579 'a': PubFoo
    582..591 'PubFoo {}': PubFoo
    601..602 'a': PubBar
    605..611 'PubBar': ctor PubBar(i64) -> PubBar
    605..614 'PubBar(3)': PubBar
    612..613 '3': i64
    624..625 'a': PubBaz
    628..634 'PubBaz': PubBaz
    644..645 'a': PubFooBar
    648..659 'PubFooBar{}': PubFooBar
    665..672 'pub_foo': function pub_foo() -> ()
    665..674 'pub_foo()': ()
    55..57 '{}': ()
    182..184 '{}': ()
    316..318 '{}': ()
    101..194 '{     ...o(); }': ()
    111..112 'a': Foo
    115..121 'Foo {}': Foo
    131..132 'a': Bar
    135..138 'Bar': ctor Bar(i64) -> Bar
    135..141 'Bar(3)': Bar
    139..140 '3': i64
    151..152 'a': Baz
    155..158 'Baz': Baz
    168..169 'a': FooBar
    172..180 'FooBar{}': FooBar
    186..189 'foo': function foo() -> ()
    186..191 'foo()': ()
    ");
}

// The old-syntax version of this test used `super::Foo`/`package::Foo` as
// *expressions* mid-body and expected that to fail ("// undefined value" /
// "// mismatched type" -- this was never valid Codira, only as a *type*).
// `::` no longer exists at all, and expression-position paths are
// single-segment only (see spec/LANGUAGE_SPEC.md section 2), so the closest
// equivalent is using the bare `super`/`root` keyword alone as a value
// expression -- which is likewise invalid, since a module isn't a value.
// Type positions (`self.Foo`, `root.Foo`, `root.foo.Foo`) are genuinely
// supported multi-segment paths and are migrated directly.
#[test]
fn scoped_path() {
    insta::assert_snapshot!(infer(
        r"
    //- /mod.code
    struct Foo;

    func main() -> self.Foo {
        Foo
    }

    func bar() -> Foo {
        super  // not a value
    }

    func baz() -> Foo {
        root  // not a value
    }

    //- /foo.code
    struct Foo;

    func bar() -> Foo {
        super  // not a value
    }

    func baz() -> root.Foo {
        super.Foo
    }

    func nested() -> self.Foo {
        root.foo.Foo
    }
    "),
    @"
    74..79: undefined value
    123..127: undefined value
    37..42: undefined value
    91..96: undefined value
    91..100: attempted to access a non-existent field in a struct.
    136..140: undefined value
    136..144: attempted to access a non-existent field in a struct.
    136..148: attempted to access a non-existent field in a struct.
    37..48 '{     Foo }': Foo
    43..46 'Foo': Foo
    68..97 '{     ...alue }': Foo
    74..79 'super': {unknown}
    117..145 '{     ...alue }': Foo
    123..127 'root': {unknown}
    31..60 '{     ...alue }': Foo
    37..42 'super': {unknown}
    85..102 '{     ....Foo }': Foo
    91..96 'super': {unknown}
    91..100 'super.Foo': {unknown}
    130..150 '{     ....Foo }': Foo
    136..140 'root': {unknown}
    136..144 'root.foo': {unknown}
    136..148 'root.foo.Foo': {unknown}
    ");
}

#[test]
fn comparison_not_implemented_for_struct() {
    insta::assert_snapshot!(infer(
        r"
    struct Foo;

    func main() -> bool {
        Foo == Foo
    }"),
    @"
    39..49: cannot apply binary operator
    33..51 '{     ... Foo }': bool
    39..42 'Foo': Foo
    39..49 'Foo == Foo': bool
    46..49 'Foo': Foo
    ");
}

#[test]
fn infer_literals() {
    insta::assert_snapshot!(infer(
        r"
        func integer() -> i32 {
            0
        }

        func large_unsigned_integer() -> u128 {
            0
        }

        func with_let() -> u16 {
            let b = 4;
            let a = 4;
            a
        }
    "),
    @"
    22..31 '{     0 }': i32
    28..29 '0': i32
    71..80 '{     0 }': u128
    77..78 '0': u128
    105..144 '{     ...   a }': u16
    115..116 'b': i32
    119..120 '4': i32
    130..131 'a': u16
    134..135 '4': u16
    141..142 'a': u16
    ");
}

#[test]
fn infer_suffix_literals() {
    insta::assert_snapshot!(infer(
        r"
    func main(){
        123;
        123u8;
        123u16;
        123u32;
        123u64;
        123u128;
        1_000_000_u32;
        123i8;
        123i16;
        123i32;
        123i64;
        123i128;
        1_000_000_i32;
        1_000_123.0e-2;
        1_000_123.0e-2f32;
        1_000_123.0e-2f64;
        9999999999999999999999999999999999999999999_f64;
    }

    func add(a:u32) -> u32 {
        a + 12u32
    }

    func errors() {
        0b22222; // invalid literal
        0b00010_f32; // non-10 base f64
        0o71234_f32; // non-10 base f64
        1234_foo; // invalid suffix
        1234.0_bar; // invalid suffix
        9999999999999999999999999999999999999999999; // too large
        256_u8; // literal out of range for `u8`
        128_i8; // literal out of range for `i8`
        12712371237123_u32; // literal out of range `u32`
        9999999999999999999999999; // literal out of range `i32`
    }
    "),
    @"
    364..371: invalid literal value
    396..407: binary float literal is not supported
    432..443: octal float literal is not supported
    468..476: invalid suffix `foo`
    500..510: invalid suffix `bar`
    534..577: int literal is too large
    596..602: literal out of range for `u8`
    641..647: literal out of range for `i8`
    686..704: literal out of range for `u32`
    740..765: literal out of range for `i32`
    11..300 '{     ...f64; }': ()
    17..20 '123': i32
    26..31 '123u8': u8
    37..43 '123u16': u16
    49..55 '123u32': u32
    61..67 '123u64': u64
    73..80 '123u128': u128
    86..99 '1_000_000_u32': u32
    105..110 '123i8': i8
    116..122 '123i16': i16
    128..134 '123i32': i32
    140..146 '123i64': i64
    152..159 '123i128': i128
    165..178 '1_000_000_i32': i32
    184..198 '1_000_123.0e-2': f64
    204..221 '1_000_...e-2f32': f32
    227..244 '1_000_...e-2f64': f64
    250..297 '999999...99_f64': f64
    311..312 'a': u32
    325..342 '{     ...2u32 }': u32
    331..332 'a': u32
    331..340 'a + 12u32': u32
    335..340 '12u32': u32
    358..798 '{     ...i32` }': ()
    364..371 '0b22222': i32
    396..407 '0b00010_f32': f32
    432..443 '0o71234_f32': f32
    468..476 '1234_foo': i32
    500..510 '1234.0_bar': f64
    534..577 '999999...999999': i32
    596..602 '256_u8': u8
    641..647 '128_i8': i8
    686..704 '127123...23_u32': u32
    740..765 '999999...999999': i32
    ");
}

#[test]
fn infer_invalid_struct_type() {
    insta::assert_snapshot!(infer(
        r"
    func main(){
        let a = Foo {b: 3};
    }"),
    @"
    25..28: undefined type
    11..38 '{     ... 3}; }': ()
    21..22 'a': {unknown}
    25..35 'Foo {b: 3}': {unknown}
    33..34 '3': i32
    ");
}

#[test]
fn infer_conditional_return() {
    insta::assert_snapshot!(infer(
        r#"
    func foo(a:int)->i32 {
        if a > 4 {
            return 4;
        }
        a
    }

    func bar(a:i32)->i32 {
        if a > 4 {
            return 4;
        } else {
            return 1;
        }
    }
    "#),
    @"
    11..14: undefined type
    9..10 'a': {unknown}
    21..69 '{     ...   a }': i32
    27..61 'if a >...     }': ()
    30..31 'a': {unknown}
    30..35 'a > 4': bool
    34..35 '4': i32
    36..61 '{     ...     }': never
    46..54 'return 4': never
    53..54 '4': i32
    66..67 'a': {unknown}
    80..81 'a': i32
    92..165 '{     ...   } }': i32
    98..163 'if a >...     }': i32
    101..102 'a': i32
    101..106 'a > 4': bool
    105..106 '4': i32
    107..132 '{     ...     }': never
    117..125 'return 4': never
    124..125 '4': i32
    138..163 '{     ...     }': never
    148..156 'return 1': never
    155..156 '1': i32
    ");
}

#[test]
fn infer_return() {
    insta::assert_snapshot!(infer(
        r#"
    func test()->i32 {
        return; // error: mismatched type
        return 5;
    }
    "#),
    @"
    23..29: `return;` in a function whose return type is not `()`
    17..72 '{     ...n 5; }': never
    23..29 'return': never
    61..69 'return 5': never
    68..69 '5': i32
    ");
}

#[test]
fn infer_call_method() {
    insta::assert_snapshot!(infer(
        r#"
    struct Foo {}

    extend Foo {
        func with_self(self) -> Self {
            self
        }
    }

    func main() {
        let a = Foo {};
        a.with_self();
    }
    "#
    ));
}

/// The `data struct` derive (`crate::data_derive`, see
/// `spec/LANGUAGE_SPEC.md` §16) synthesizes an `eq` method purely from
/// source text spliced in before parsing -- this test is the actual proof
/// that the synthesized body type-checks (not just parses): a call site
/// resolves `eq` to `bool`, and no diagnostics are produced for either the
/// synthesized method body or the call.
#[test]
fn data_struct_eq_type_checks() {
    insta::assert_snapshot!(infer(
        r#"
    data struct Point {
        x: f64,
        y: f64,
    }

    func main() {
        let a = Point { x: 1.0, y: 2.0 };
        let b = Point { x: 1.0, y: 2.0 };
        a.eq(b);
    }
    "#
    ));
}

/// `expr::validator::effect_obligation` proof, negative case: a caller
/// invoking an effectful function without declaring the effect itself gets
/// a real diagnostic (not just "designed" per
/// `spec/LANGUAGE_SPEC.md` §6's previous "designed above but not
/// implemented" status).
#[test]
fn undeclared_effect_is_a_real_diagnostic() {
    insta::assert_snapshot!(infer(
        r#"
    effect Logger {
        func log(message: String)
    }

    func risky() uses Logger -> i32 {
        42
    }

    func caller() -> i32 {
        risky()
    }
    "#
    ));
}

/// Same shape as `undeclared_effect_is_a_real_diagnostic`, but the caller
/// *does* declare `uses Logger` -- proves the check isn't just always-fire,
/// it actually looks at the caller's own declared effects.
#[test]
fn declared_effect_produces_no_diagnostic() {
    insta::assert_snapshot!(infer(
        r#"
    effect Logger {
        func log(message: String)
    }

    func risky() uses Logger -> i32 {
        42
    }

    func caller() uses Logger -> i32 {
        risky()
    }
    "#
    ));
}

/// `crate::refinement_check` proof, end to end through the real diagnostic
/// pipeline (not just the module's own unit tests): a `type` alias to an
/// unsatisfiable refinement predicate gets a real diagnostic, backed by an
/// actual Z3 UNSAT proof -- the first real (not just designed) use of
/// `codira_smt` from the compiler pipeline itself.
#[test]
fn unsatisfiable_refinement_type_is_a_real_diagnostic() {
    insta::assert_snapshot!(infer(
        r#"
    type Impossible = i32 { x | x > 0 && x < 0 };
    "#
    ));
}

/// Same shape, but the predicate is satisfiable -- proves this isn't a
/// blanket "refinement types are always flagged" false positive.
#[test]
fn satisfiable_refinement_type_produces_no_diagnostic() {
    insta::assert_snapshot!(infer(
        r#"
    type Positive = i32 { x | x > 0 };
    "#
    ));
}

/// `crate::heal_check` proof, end to end through the real diagnostic
/// pipeline: a `@heal(..., postcondition: ...)` healing contract whose
/// postcondition can never be satisfied gets a real, SMT-backed
/// diagnostic.
#[test]
fn unsatisfiable_healing_postcondition_is_a_real_diagnostic() {
    insta::assert_snapshot!(infer(
        r#"
    @heal(on: [Timeout], postcondition: result > 0 && result < 0)
    func risky() -> i32 {
        1
    }
    "#
    ));
}

/// Same shape, satisfiable postcondition -- proves this isn't a blanket
/// "any @heal is flagged" false positive.
#[test]
fn satisfiable_healing_postcondition_produces_no_diagnostic() {
    insta::assert_snapshot!(infer(
        r#"
    @heal(on: [Timeout], postcondition: result >= 0)
    func risky() -> i32 {
        1
    }
    "#
    ));
}

/// `expr::validator::move_check` proof, negative case: a `consuming`
/// parameter used twice in an `@strict` function is a real diagnostic.
#[test]
fn use_after_consume_is_a_real_diagnostic() {
    insta::assert_snapshot!(infer(
        r#"
    @strict
    func take(consuming buf: i32) -> i32 {
        buf + buf
    }
    "#
    ));
}

/// Same shape, but the function is *not* `@strict` -- proves the check is
/// genuinely opt-in, not a blanket rule that just happens to have a name
/// suggesting it's scoped.
#[test]
fn double_use_without_strict_produces_no_diagnostic() {
    insta::assert_snapshot!(infer(
        r#"
    func take(consuming buf: i32) -> i32 {
        buf + buf
    }
    "#
    ));
}

/// A single use of a `consuming` parameter in an `@strict` function is
/// exactly the intended, unflagged case -- proves this isn't "any mention
/// of a consuming param is an error."
#[test]
fn single_use_in_strict_function_produces_no_diagnostic() {
    insta::assert_snapshot!(infer(
        r#"
    @strict
    func take(consuming buf: i32) -> i32 {
        buf
    }
    "#
    ));
}

#[test]
fn infer_call_method_not_in_scope() {
    insta::assert_snapshot!(infer(
        r#"
    //- /foo.code
    public struct Foo {}

    extend Foo {
        func with_self(self) -> Self {
            self
        }
    }

    //- /mod.code
    import foo.Foo

    func main() {
        let a = Foo {};
        a.with_self();
    }
    "#
    ));
}

// Uses a plain top-level `make_foo` function rather than an associated
// `Foo.new()`/`extend`-block constructor to obtain the instance: there is
// currently no working call syntax for associated (self-less) functions --
// see the "associated/static function calls" note in
// spec/LANGUAGE_SPEC.md's implementation-status ledger. The actual thing
// under test here (private vs. public field access across a module
// boundary) doesn't depend on how the instance was constructed.
#[test]
fn infer_access_private_field() {
    insta::assert_snapshot!(infer(
        r#"
    //- /foo.code
    public struct Foo {
        private: i32
        public public: i32,
    }

    public func make_foo() -> Foo {
        Foo { private: 0, public: 0 }
    }

    //- /mod.code
    import foo.make_foo

    func main() {
        let foo = make_foo();
        let a = foo.private;
        let b = foo.public;
    }
    "#
    ));
}

#[test]
fn infer_call_assoc_as_method() {
    insta::assert_snapshot!(infer(
        r#"
    public struct Foo {}

    extend Foo {
        func assoc() {}
    }

    func main() {
        let a = Foo {};
        a.assoc();
    }
    "#
    ));
}

#[test]
fn infer_self_param() {
    insta::assert_snapshot!(infer(
        r#"
    struct Foo {
        a: i32
    }

    extend Foo {
        func with_self(self) -> Self {
            self
        }
    }
    "#
    ));
}

#[test]
fn infer_self_field() {
    insta::assert_snapshot!(infer(
        r#"
    struct Foo {
        a: i32
    }

    extend Foo {
        func self_a(self) -> i32 {
            self.a
        }

        func self_b(self) -> i32 {
            self.b  // error: attempted to access a non-existent field in a struct.
        }
    }
    "#
    ));
}

// There is currently no working call syntax for associated (self-less)
// functions declared in an `extend` block: `Type::name()` no longer parses
// (`::` was removed from the language), and `Type.name()` (postfix `.` on a
// bare type name) doesn't resolve either -- `infer_method_call` in
// `ty/infer.rs` always infers its receiver expression as a *value* first,
// so a bare type name is treated as an attempted (and here invalid) unit
// struct literal, never as "the type itself" for associated-function
// lookup. `lookup_method`'s `AssociationMode::WithoutSelf` path exists and
// is exercised (as a "did you mean" hint on a failed instance-method
// lookup), so the underlying resolution machinery is there; only the
// expression-level call syntax to reach it is missing. See
// spec/LANGUAGE_SPEC.md's implementation-status ledger.
#[test]
#[ignore = "no working call syntax for associated/static functions yet -- see comment above"]
fn infer_assoc_function() {
    insta::assert_snapshot!(infer(
        r#"
    struct Foo {
        a: i32
    }

    extend Foo {
        func new() -> Self {
            Self { a: 3 }
        }
    }

    func main() {
        let a = Foo.new();
    }
    "#
    ));
}

#[test]
#[ignore = "no working call syntax for associated/static functions yet -- see infer_assoc_function"]
fn infer_access_hidden_assoc_function() {
    insta::assert_snapshot!(infer(
        r#"
    //- /foo.code
    public struct Foo {
        a: i32
    }

    extend Foo {
        func new(){}
    }

    //- /mod.code
    import foo.Foo

    func main() {
        Foo.new();
    }
    "#
    ));
}

#[test]
fn infer_basics() {
    insta::assert_snapshot!(infer(
        r#"
    func test(a:i32, b:f64, c:never, d:bool) -> bool {
        a;
        b;
        c;
        d
    }
    "#),
    @"
    10..11 'a': i32
    17..18 'b': f64
    24..25 'c': never
    33..34 'd': bool
    49..79 '{     ...   d }': never
    55..56 'a': i32
    62..63 'b': f64
    69..70 'c': never
    76..77 'd': bool
    ");
}

#[test]
fn infer_branching() {
    insta::assert_snapshot!(infer(
        r#"
    func test() {
        let a = if true { 3 } else { 4 }
        let b = if true { 3 }               // Missing else branch
        let c = if true { 3; }
        let d = if true { 5 } else if false { 3 } else { 4 }
        let e = if true { 5.0 } else { 5 }  // Mismatched branches
    }
    "#),
    @"
    63..76: missing else branch
    210..236: mismatched branches
    12..262 '{     ...ches }': ()
    22..23 'a': i32
    26..50 'if tru... { 4 }': i32
    29..33 'true': bool
    34..39 '{ 3 }': i32
    36..37 '3': i32
    45..50 '{ 4 }': i32
    47..48 '4': i32
    59..60 'b': ()
    63..76 'if true { 3 }': ()
    66..70 'true': bool
    71..76 '{ 3 }': i32
    73..74 '3': i32
    122..123 'c': ()
    126..140 'if true { 3; }': ()
    129..133 'true': bool
    134..140 '{ 3; }': ()
    136..137 '3': i32
    149..150 'd': i32
    153..197 'if tru... { 4 }': i32
    156..160 'true': bool
    161..166 '{ 5 }': i32
    163..164 '5': i32
    172..197 'if fal... { 4 }': i32
    175..180 'false': bool
    181..186 '{ 3 }': i32
    183..184 '3': i32
    192..197 '{ 4 }': i32
    194..195 '4': i32
    206..207 'e': f64
    210..236 'if tru... { 5 }': f64
    213..217 'true': bool
    218..225 '{ 5.0 }': f64
    220..223 '5.0': f64
    231..236 '{ 5 }': i32
    233..234 '5': i32
    ");
}

#[test]
fn void_return() {
    insta::assert_snapshot!(infer(
        r#"
    func bar() {
        let a = 3;
    }
    func foo(a:i32) {
        let c = bar()
    }
    "#),
    @"
    11..29 '{     ...= 3; }': ()
    21..22 'a': i32
    25..26 '3': i32
    39..40 'a': i32
    46..67 '{     ...ar() }': ()
    56..57 'c': ()
    60..63 'bar': function bar() -> ()
    60..65 'bar()': ()
    ");
}

#[test]
fn place_expressions() {
    insta::assert_snapshot!(infer(
        r#"
    func foo(a:i32) {
        a += 3;
        3 = 5; // error: invalid left hand side of expression
    }
    "#),
    @"
    34..35: invalid left hand side of expression
    9..10 'a': i32
    16..89 '{     ...sion }': ()
    22..23 'a': i32
    22..28 'a += 3': ()
    27..28 '3': i32
    34..35 '3': i32
    34..39 '3 = 5': ()
    38..39 '5': i32
    ");
}

#[test]
fn update_operators() {
    insta::assert_snapshot!(infer(
        r#"
    func foo(a:i32, b:f64) {
        a += 3;
        a -= 3;
        a *= 3;
        a /= 3;
        a %= 3;
        b += 3.0;
        b -= 3.0;
        b *= 3.0;
        b /= 3.0;
        b %= 3.0;
        a *= 3.0; // mismatched type
        b *= 3; // mismatched type
    }
    "#),
    @"
    164..167: mismatched type
    197..198: mismatched type
    9..10 'a': i32
    16..17 'b': f64
    23..220 '{     ...type }': ()
    29..30 'a': i32
    29..35 'a += 3': ()
    34..35 '3': i32
    41..42 'a': i32
    41..47 'a -= 3': ()
    46..47 '3': i32
    53..54 'a': i32
    53..59 'a *= 3': ()
    58..59 '3': i32
    65..66 'a': i32
    65..71 'a /= 3': ()
    70..71 '3': i32
    77..78 'a': i32
    77..83 'a %= 3': ()
    82..83 '3': i32
    89..90 'b': f64
    89..97 'b += 3.0': ()
    94..97 '3.0': f64
    103..104 'b': f64
    103..111 'b -= 3.0': ()
    108..111 '3.0': f64
    117..118 'b': f64
    117..125 'b *= 3.0': ()
    122..125 '3.0': f64
    131..132 'b': f64
    131..139 'b /= 3.0': ()
    136..139 '3.0': f64
    145..146 'b': f64
    145..153 'b %= 3.0': ()
    150..153 '3.0': f64
    159..160 'a': i32
    159..167 'a *= 3.0': ()
    164..167 '3.0': f64
    192..193 'b': f64
    192..198 'b *= 3': ()
    197..198 '3': i32
    ");
}

#[test]
fn infer_unary_ops() {
    insta::assert_snapshot!(infer(
        r#"
    func foo(a: i32, b: bool) {
        a = -a;
        b = !b;
    }
        "#),
    @"
    9..10 'a': i32
    17..18 'b': bool
    26..53 '{     ... !b; }': ()
    32..33 'a': i32
    32..38 'a = -a': ()
    36..38 '-a': i32
    37..38 'a': i32
    44..45 'b': bool
    44..50 'b = !b': ()
    48..50 '!b': bool
    49..50 'b': bool
    ");
}

#[test]
fn invalid_unary_ops() {
    insta::assert_snapshot!(infer(
        r#"
    func bar(a: f64, b: bool) {
        a = !a; // mismatched type
        b = -b; // mismatched type
    }
        "#),
    @"
    37..38: cannot apply unary operator
    68..69: cannot apply unary operator
    9..10 'a': f64
    17..18 'b': bool
    26..91 '{     ...type }': ()
    32..33 'a': f64
    32..38 'a = !a': ()
    36..38 '!a': {unknown}
    37..38 'a': f64
    63..64 'b': bool
    63..69 'b = -b': ()
    67..69 '-b': {unknown}
    68..69 'b': bool
    ");
}

#[test]
fn infer_loop() {
    insta::assert_snapshot!(infer(
        r#"
    func foo() {
        loop {}
    }
    "#),
    @"
    11..26 '{     loop {} }': never
    17..24 'loop {}': never
    22..24 '{}': ()
    ");
}

#[test]
fn infer_break() {
    insta::assert_snapshot!(infer(
        r#"
    func foo()->i32 {
        break; // error: not in a loop
        loop { break 3; break 3.0; } // error: mismatched type
        let a:i32 = loop { break 3.0; } // error: mismatched type
        loop { break 3; }
        let a:i32 = loop { break loop { break 3; } }
        loop { break loop { break 3.0; } } // error: mismatched type
    }
    "#),
    @"
    22..27: `break` outside of a loop
    73..82: mismatched type
    135..144: mismatched type
    269..278: mismatched type
    16..311 '{     ...type }': never
    22..27 'break': never
    57..85 'loop {...3.0; }': i32
    62..85 '{ brea...3.0; }': never
    64..71 'break 3': never
    70..71 '3': i32
    73..82 'break 3.0': never
    79..82 '3.0': f64
    120..121 'a': i32
    128..147 'loop {...3.0; }': i32
    133..147 '{ break 3.0; }': never
    135..144 'break 3.0': never
    141..144 '3.0': f64
    178..195 'loop {...k 3; }': i32
    183..195 '{ break 3; }': never
    185..192 'break 3': never
    191..192 '3': i32
    204..205 'a': i32
    212..244 'loop {...3; } }': i32
    217..244 '{ brea...3; } }': never
    219..242 'break ...k 3; }': never
    225..242 'loop {...k 3; }': i32
    230..242 '{ break 3; }': never
    232..239 'break 3': never
    238..239 '3': i32
    249..283 'loop {...0; } }': i32
    254..283 '{ brea...0; } }': never
    256..281 'break ...3.0; }': never
    262..281 'loop {...3.0; }': i32
    267..281 '{ break 3.0; }': never
    269..278 'break 3.0': never
    275..278 '3.0': f64
    ");
}

#[test]
fn infer_while() {
    insta::assert_snapshot!(infer(
        r#"
    func foo() {
        let n = 0;
        while n < 3 { n += 1; };
        while n < 3 { n += 1; break; };
        while n < 3 { break 3; };   // error: break with value can only appear in a loop
        while n < 3 { loop { break 3; }; };
    }
    "#),
    @"
    111..118: `break` with value can only appear in a `loop`
    11..219 '{     ...; }; }': ()
    21..22 'n': i32
    25..26 '0': i32
    32..55 'while ...= 1; }': ()
    38..39 'n': i32
    38..43 'n < 3': bool
    42..43 '3': i32
    44..55 '{ n += 1; }': ()
    46..47 'n': i32
    46..52 'n += 1': ()
    51..52 '1': i32
    61..91 'while ...eak; }': ()
    67..68 'n': i32
    67..72 'n < 3': bool
    71..72 '3': i32
    73..91 '{ n +=...eak; }': never
    75..76 'n': i32
    75..81 'n += 1': ()
    80..81 '1': i32
    83..88 'break': never
    97..121 'while ...k 3; }': ()
    103..104 'n': i32
    103..108 'n < 3': bool
    107..108 '3': i32
    109..121 '{ break 3; }': never
    111..118 'break 3': never
    182..216 'while ...; }; }': ()
    188..189 'n': i32
    188..193 'n < 3': bool
    192..193 '3': i32
    194..216 '{ loop...; }; }': ()
    196..213 'loop {...k 3; }': i32
    201..213 '{ break 3; }': never
    203..210 'break 3': never
    209..210 '3': i32
    ");
}

#[test]
fn invalid_binary_ops() {
    insta::assert_snapshot!(infer(
        r#"
    func foo() {
        let b = false;
        let n = 1;
        let _ = b + n; // error: invalid binary operation
    }
    "#),
    @"
    59..64: cannot apply binary operator
    11..102 '{     ...tion }': ()
    21..22 'b': bool
    25..30 'false': bool
    40..41 'n': i32
    44..45 '1': i32
    59..60 'b': bool
    59..64 'b + n': i32
    63..64 'n': i32
    ");
}

#[test]
fn struct_decl() {
    insta::assert_snapshot!(infer(
        r#"
    struct Foo;
    class Bar {
        f: f64,
        i: i32,
    }
    struct Baz(f64, i32);


    func main() {
        let foo: Foo;
        let bar: Bar;
        let baz: Baz;
    }
    "#),
    @"
    86..143 '{     ...Baz; }': ()
    96..99 'foo': Foo
    114..117 'bar': Bar
    132..135 'baz': Baz
    ");
}

#[test]
fn struct_lit() {
    insta::assert_snapshot!(infer(
        r#"
    struct Foo;
    struct Bar {
        a: f64,
    }
    struct Baz(f64, i32);

    func main() {
        let a: Foo = Foo;
        let b: Bar = Bar { a: 1.23, };
        let c = Baz(1.23, 1);

        let a = Foo{}; // error: mismatched struct literal kind. expected `unit struct`, found `record`
        let a = Foo(); // error: mismatched struct literal kind. expected `unit struct`, found `tuple`
        let b = Bar; // error: mismatched struct literal kind. expected `record`, found `unit struct`
        let b = Bar(); // error: mismatched struct literal kind. expected `record`, found `tuple`
        let b = Bar{}; // error: missing record fields: a
        let c = Baz; // error: mismatched struct literal kind. expected `tuple`, found `unit struct`
        let c = Baz{}; // error: mismatched struct literal kind. expected `tuple`, found `record`
        let c = Baz(); // error: this tuple struct literal has 2 fields but 0 fields were supplied
    }
    "#),
    @"
    172..177: mismatched struct literal kind. expected `unit struct`, found `record`
    272..277: mismatched struct literal kind. expected `unit struct`, found `tuple`
    371..374: mismatched struct literal kind. expected `record`, found `unit struct`
    469..472: mismatched struct literal kind. expected `record`, found `tuple`
    563..568: missing record fields:
    - a

    617..620: mismatched struct literal kind. expected `tuple`, found `unit struct`
    714..719: mismatched struct literal kind. expected `tuple`, found `record`
    808..813: this tuple struct literal has 2 fields but 0 fields were supplied
    74..892 '{     ...lied }': ()
    84..85 'a': Foo
    93..96 'Foo': Foo
    106..107 'b': Bar
    115..131 'Bar { ....23, }': Bar
    124..128 '1.23': f64
    141..142 'c': Baz
    145..148 'Baz': ctor Baz(f64, i32) -> Baz
    145..157 'Baz(1.23, 1)': Baz
    149..153 '1.23': f64
    155..156 '1': i32
    168..169 'a': Foo
    172..177 'Foo{}': Foo
    268..269 'a': Foo
    272..275 'Foo': Foo
    272..277 'Foo()': Foo
    367..368 'b': Bar
    371..374 'Bar': Bar
    465..466 'b': Bar
    469..472 'Bar': Bar
    469..474 'Bar()': Bar
    559..560 'b': Bar
    563..568 'Bar{}': Bar
    613..614 'c': ctor Baz(f64, i32) -> Baz
    617..620 'Baz': ctor Baz(f64, i32) -> Baz
    710..711 'c': Baz
    714..719 'Baz{}': Baz
    804..805 'c': Baz
    808..811 'Baz': ctor Baz(f64, i32) -> Baz
    808..813 'Baz()': Baz
    ");
}

#[test]
fn struct_field_visibility() {
    insta::assert_snapshot!(infer(
        r#"
    //- /foo.code
    public struct Foo(public i32, i32)

    public func make_foo() -> Foo {
        Foo(1, 2)
    }

    //- /mod.code
    import foo.make_foo

    func main() {
        let foo = make_foo();
        let a = foo.0;
        let b = foo.1;
    }"#
    ));
}

#[test]
fn struct_field_index() {
    insta::assert_snapshot!(infer(
        r#"
    struct Foo {
        a: f64,
        b: i32,
    }
    struct Bar(f64, i32)
    struct Baz;

    func main() {
        let foo = Foo { a: 1.23, b: 4 };
        foo.a
        foo.b
        foo.c // error: attempted to access a non-existent field in a struct.
        let bar = Bar(1.23, 4);
        bar.0
        bar.1
        bar.2 // error: attempted to access a non-existent field in a struct.
        let baz = Baz;
        baz.a // error: attempted to access a non-existent field in a struct.
        let f = 1.0
        f.0; // error: attempted to access a non-existent field in a struct.
    }
    "#),
    @"
    148..153: attempted to access a non-existent field in a struct.
    270..275: attempted to access a non-existent field in a struct.
    363..368: attempted to access a non-existent field in a struct.
    453..456: attempted to access a non-existent field in a struct.
    85..523 '{     ...uct. }': ()
    95..98 'foo': Foo
    101..122 'Foo { ...b: 4 }': Foo
    110..114 '1.23': f64
    119..120 '4': i32
    128..131 'foo': Foo
    128..133 'foo.a': f64
    138..141 'foo': Foo
    138..143 'foo.b': i32
    148..151 'foo': Foo
    148..153 'foo.c': {unknown}
    226..229 'bar': Bar
    232..235 'Bar': ctor Bar(f64, i32) -> Bar
    232..244 'Bar(1.23, 4)': Bar
    236..240 '1.23': f64
    242..243 '4': i32
    250..253 'bar': Bar
    250..255 'bar.0': f64
    260..263 'bar': Bar
    260..265 'bar.1': i32
    270..273 'bar': Bar
    270..275 'bar.2': {unknown}
    348..351 'baz': Baz
    354..357 'Baz': Baz
    363..366 'baz': Baz
    363..368 'baz.a': {unknown}
    441..442 'f': f64
    445..448 '1.0': f64
    453..454 'f': f64
    453..456 'f.0': {unknown}
    ");
}

#[test]
fn primitives() {
    insta::assert_snapshot!(infer(
        r#"
    func unsigned_primitives(a: u8, b: u16, c: u32, d: u64, e: u128, f: usize, g: u32) -> u8 { a }
    func signed_primitives(a: i8, b: i16, c: i32, d: i64, e: i128, f: isize, g: i32) -> i8 { a }
    func float_primitives(a: f32, b: f64, c: f64) -> f32 { a }
    "#),
    @"
    25..26 'a': u8
    32..33 'b': u16
    40..41 'c': u32
    48..49 'd': u64
    56..57 'e': u128
    65..66 'f': usize
    75..76 'g': u32
    89..94 '{ a }': u8
    91..92 'a': u8
    118..119 'a': i8
    125..126 'b': i16
    133..134 'c': i32
    141..142 'd': i64
    149..150 'e': i128
    158..159 'f': isize
    168..169 'g': i32
    182..187 '{ a }': i8
    184..185 'a': i8
    210..211 'a': f32
    218..219 'b': f64
    226..227 'c': f64
    241..246 '{ a }': f32
    243..244 'a': f32
    ");
}

#[test]
fn extern_fn() {
    insta::assert_snapshot!(infer(
        r#"
    extern func foo(a:i32, b:i32) -> i32;
    func main() {
        foo(3,4);
    }

    extern func with_body() {}    // extern functions cannot have bodies

    struct S;
    extern func with_non_primitive(s:S);  // extern functions can only have primitives as parameters
    extern func with_non_primitive_return() -> S;  // extern functions can only have primitives as parameters
    "#),
    @"
    69..95: extern functions cannot have bodies
    182..183: extern functions can only have primitives as parameter- and return types
    289..290: extern functions can only have primitives as parameter- and return types
    16..17 'a': i32
    23..24 'b': i32
    50..67 '{     ...,4); }': ()
    56..59 'foo': function foo(i32, i32) -> i32
    56..64 'foo(3,4)': i32
    60..61 '3': i32
    62..63 '4': i32
    93..95 '{}': ()
    180..181 's': S
    ");
}

#[test]
fn infer_type_alias() {
    insta::assert_snapshot!(infer(
        r#"
    type Foo = i32;
    type Bar = Foo;
    type Baz = UnknownType;  // error: undefined type

    func main(a: Foo) {
        let b: Bar = a;
    }
    "#),
    @"
    43..54: undefined type
    93..94 'a': Foo
    101..124 '{     ...= a; }': ()
    111..112 'b': Bar
    120..121 'a': Foo
    ");
}

#[test]
fn recursive_alias() {
    insta::assert_snapshot!(infer(
        r#"
    struct Foo {}
    type Foo = Foo;

    type A = B;
    type B = A;

    func main() {
        let a: Foo;  // error: unknown type
        let b: A;    // error: unknown type
        let c: B;    // error: unknown type
    }
    "#),
    @"
    14..29: the name `Foo` is defined multiple times
    40..41: cyclic type
    52..53: cyclic type
    68..191 '{     ...type }': ()
    78..79 'a': Foo
    118..119 'b': A
    158..159 'c': B
    ");
}

// ---------------------------------------------------------------------------
// `expr as Type` casts (S6). The legality matrix these exercise lives in
// `crate::ty::cast`.
// ---------------------------------------------------------------------------

#[test]
fn infer_cast_literal_to_int() {
    insta::assert_snapshot!(infer(
        r"
    func main() -> i64 {
        1 as i64
    }"),
    @"
    19..35 '{     ... i64 }': i64
    25..26 '1': i32
    25..33 '1 as i64': i64
    ");
}

#[test]
fn infer_cast_int_to_float() {
    insta::assert_snapshot!(infer(
        r"
    func main(x: i32) -> f64 {
        x as f64
    }"),
    @"
    10..11 'x': i32
    25..41 '{     ... f64 }': f64
    31..32 'x': i32
    31..39 'x as f64': f64
    ");
}

#[test]
fn infer_cast_chained() {
    insta::assert_snapshot!(infer(
        r"
    func main(x: u8) -> i64 {
        x as i32 as i64
    }"),
    @"
    10..11 'x': u8
    24..47 '{     ... i64 }': i64
    30..31 'x': u8
    30..38 'x as i32': i32
    30..45 'x as i32 as i64': i64
    ");
}

#[test]
fn infer_cast_identity() {
    insta::assert_snapshot!(infer(
        r"
    func main(x: i32) -> i32 {
        x as i32
    }"),
    @"
    10..11 'x': i32
    25..41 '{     ... i32 }': i32
    31..32 'x': i32
    31..39 'x as i32': i32
    ");
}

#[test]
fn infer_cast_numeric_matrix() {
    insta::assert_snapshot!(infer(
        r"
    func main(i: i32, u: u64, f: f32, d: f64, b: bool) {
        let narrow = i as i8;
        let widen_signed = i as i64;
        let widen_unsigned = u as u128;
        let reinterpret = i as u32;
        let to_float = i as f64;
        let from_float = f as i16;
        let from_float_unsigned = d as u8;
        let float_widen = f as f64;
        let float_narrow = d as f32;
        let from_bool = b as i32;
    }"),
    @"
    10..11 'i': i32
    18..19 'u': u64
    26..27 'f': f32
    34..35 'd': f64
    42..43 'b': bool
    51..375 '{     ...i32; }': ()
    61..67 'narrow': i8
    70..71 'i': i32
    70..77 'i as i8': i8
    87..99 'widen_signed': i64
    102..103 'i': i32
    102..110 'i as i64': i64
    120..134 'widen_unsigned': u128
    137..138 'u': u64
    137..146 'u as u128': u128
    156..167 'reinterpret': u32
    170..171 'i': i32
    170..178 'i as u32': u32
    188..196 'to_float': f64
    199..200 'i': i32
    199..207 'i as f64': f64
    217..227 'from_float': i16
    230..231 'f': f32
    230..238 'f as i16': i16
    248..267 'from_f...signed': u8
    270..271 'd': f64
    270..277 'd as u8': u8
    287..298 'float_widen': f64
    301..302 'f': f32
    301..309 'f as f64': f64
    319..331 'float_narrow': f32
    334..335 'd': f64
    334..342 'd as f32': f32
    352..361 'from_bool': i32
    364..365 'b': bool
    364..372 'b as i32': i32
    ");
}

#[test]
fn infer_cast_to_bool_is_an_error() {
    insta::assert_snapshot!(infer(
        r"
    func main(i: i32, f: f64, b: bool) {
        let a = i as bool;
        let c = f as bool;
        let d = b as bool;
    }"),
    @"
    49..58: cannot cast to `bool` with `as`; use a comparison instead
    72..81: cannot cast to `bool` with `as`; use a comparison instead
    10..11 'i': i32
    18..19 'f': f64
    26..27 'b': bool
    35..107 '{     ...ool; }': ()
    45..46 'a': bool
    49..50 'i': i32
    49..58 'i as bool': bool
    68..69 'c': bool
    72..73 'f': f64
    72..81 'f as bool': bool
    91..92 'd': bool
    95..96 'b': bool
    95..104 'b as bool': bool
    ");
}

#[test]
fn infer_cast_bool_to_float_is_an_error() {
    insta::assert_snapshot!(infer(
        r"
    func main(b: bool) {
        let a = b as f64;
    }"),
    @"
    33..41: `bool` can only be cast to an integer type
    10..11 'b': bool
    19..44 '{     ...f64; }': ()
    29..30 'a': f64
    33..34 'b': bool
    33..41 'b as f64': f64
    ");
}

#[test]
fn infer_cast_aggregates_is_an_error() {
    insta::assert_snapshot!(infer(
        r"
    struct Foo;

    func main() {
        let a = Foo as i32;
        let b = 1 as Foo;
        let c = [1, 2, 3] as i32;
    }"),
    @"
    39..49: `as` can only convert between primitive numeric types
    63..71: `as` can only convert between primitive numeric types
    85..101: `as` can only convert between primitive numeric types
    25..104 '{     ...i32; }': ()
    35..36 'a': i32
    39..42 'Foo': Foo
    39..49 'Foo as i32': i32
    59..60 'b': Foo
    63..64 '1': i32
    63..71 '1 as Foo': Foo
    81..82 'c': i32
    85..94 '[1, 2, 3]': [i32]
    85..101 '[1, 2,...as i32': i32
    86..87 '1': i32
    89..90 '2': i32
    92..93 '3': i32
    ");
}

#[test]
fn infer_cast_through_type_alias() {
    insta::assert_snapshot!(infer(
        r"
    type Byte = u8;
    type Flag = bool;

    func main(x: i32) {
        let a = x as Byte;
        let b = x as Flag;
    }"),
    @"
    90..99: cannot cast to `bool` with `as`; use a comparison instead
    45..46 'x': i32
    53..102 '{     ...lag; }': ()
    63..64 'a': Byte
    67..68 'x': i32
    67..76 'x as Byte': Byte
    86..87 'b': Flag
    90..91 'x': i32
    90..99 'x as Flag': Flag
    ");
}

#[test]
fn infer_cast_unresolved_target_does_not_cascade() {
    insta::assert_snapshot!(infer(
        r"
    func main(x: i32) {
        let a = x as DoesNotExist;
    }"),
    @"
    37..49: undefined type
    10..11 'x': i32
    18..52 '{     ...ist; }': ()
    28..29 'a': {unknown}
    32..33 'x': i32
    32..49 'x as D...tExist': {unknown}
    ");
}

fn infer(content: &str) -> String {
    let db = MockDatabase::with_files(content);

    let mut acc = String::new();

    let mut infer_def = |infer_result: Arc<InferenceResult>,
                         body_source_map: Arc<BodySourceMap>| {
        let mut types = Vec::new();

        for (pat, ty) in infer_result.type_of_pat.iter() {
            let syntax_ptr = match body_source_map.pat_syntax(pat) {
                Some(sp) => {
                    sp.map(|ast| ast.either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr()))
                }
                None => continue,
            };
            types.push((syntax_ptr, ty));
        }

        for (expr, ty) in infer_result.type_of_expr.iter() {
            let syntax_ptr = match body_source_map.expr_syntax(expr) {
                Some(sp) => {
                    sp.map(|ast| ast.either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr()))
                }
                None => continue,
            };
            types.push((syntax_ptr, ty));
        }

        // Sort ranges for consistency
        types.sort_by_key(|(src_ptr, _)| {
            (src_ptr.value.range().start(), src_ptr.value.range().end())
        });
        for (src_ptr, ty) in &types {
            let node = src_ptr.value.to_node(&src_ptr.file_syntax(&db));

            let (range, text) = (
                src_ptr.value.range(),
                node.text().to_string().replace('\n', " "),
            );
            writeln!(
                acc,
                "{:?} '{}': {}",
                range,
                ellipsize(text, 15),
                ty.display(&db)
            )
            .unwrap();
        }
    };

    let mut diags = String::new();

    let mut diag_sink = DiagnosticSink::new(|diag| {
        writeln!(diags, "{:?}: {}", diag.highlight_range(), diag.message()).unwrap();
    });

    for package in Package::all(&db).iter() {
        for module in package.modules(&db).iter() {
            module.diagnostics(&db, &mut diag_sink);
        }
    }

    for item in Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.declarations(&db))
    {
        if let ModuleDef::Function(fun) = item {
            let source_map = fun.body_source_map(&db);
            let infer_result = fun.infer(&db);
            infer_def(infer_result, source_map);
        }
    }

    for item in Package::all(&db)
        .iter()
        .flat_map(|pkg| pkg.modules(&db))
        .flat_map(|module| module.impls(&db))
    {
        for associated_item in item.items(&db) {
            let AssocItem::Function(fun) = associated_item;

            let source_map = fun.body_source_map(&db);
            let infer_result = fun.infer(&db);
            infer_def(infer_result, source_map);
        }
    }

    drop(diag_sink);

    acc.truncate(acc.trim_end().len());
    diags.truncate(diags.trim_end().len());
    [diags, acc].join("\n").trim().to_string()
}

fn ellipsize(mut text: String, max_len: usize) -> String {
    if text.len() <= max_len {
        return text;
    }
    let ellipsis = "...";
    let e_len = ellipsis.len();
    let mut prefix_len = (max_len - e_len) / 2;
    while !text.is_char_boundary(prefix_len) {
        prefix_len += 1;
    }
    let mut suffix_len = max_len - e_len - prefix_len;
    while !text.is_char_boundary(text.len() - suffix_len) {
        suffix_len += 1;
    }
    text.replace_range(prefix_len..text.len() - suffix_len, ellipsis);
    text
}

/// A tuple literal's type is the element-wise product of its parts, and `t.N`
/// projects out element `N`. Both sides already existed in the type system
/// (`TyKind::Tuple`, `lookup_field`'s tuple arm); this pins down that the new
/// `Expr::Tuple` inference arm feeds them correctly.
#[test]
fn infer_tuple() {
    insta::assert_snapshot!(infer(
        r"
    func main() -> i32 {
        let t = (1, 2.0);
        let n = t.1;
        t.0
    }",
    ));
}

/// The expectation is pushed down element-wise, not as a whole.
///
/// Both literals below are bare integers, so both would default to `i32` if
/// the annotation were only checked against the finished tuple type. Getting
/// `u8` for one and `u64` for the other is only possible if each element saw
/// its own expected type while being inferred.
#[test]
fn infer_tuple_expected_type_propagates_per_element() {
    insta::assert_snapshot!(infer(
        r"
    func main() -> u64 {
        let t: (u8, u64) = (1, 2);
        t.1
    }",
    ));
}

/// `()` is the unit type -- a zero-element tuple, the same type an omitted
/// return type produces.
#[test]
fn infer_tuple_unit() {
    insta::assert_snapshot!(infer(
        r"
    func main() -> () {
        ()
    }",
    ));
}

/// Tuples nest, and projecting through two levels resolves to the innermost
/// element's type rather than collapsing to the outer tuple.
#[test]
fn infer_tuple_nested() {
    insta::assert_snapshot!(infer(
        r"
    func main() -> u8 {
        let t = ((1, 2), 3.0);
        let inner = t.0;
        inner.1
    }",
    ));
}

#[test]
fn probe_method_call_arity() {
    insta::assert_snapshot!(infer(
        r"
    struct Counter { value: i32 }

    extend Counter {
        func bumped(self, by: i32) -> Counter {
            Counter { value: self.value + by }
        }
        func get(self) -> i32 { self.value }
    }

    func main() -> i32 {
        let c = Counter { value: 10 };
        c.bumped(5).get()
    }",
    ));
}
