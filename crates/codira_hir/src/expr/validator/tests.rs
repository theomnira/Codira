//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#[cfg(test)]
use crate::utils::tests::*;

#[test]
fn test_uninitialized_access() {
    insta::assert_snapshot!(
        diagnostics(r#"
    func foo() {
        let a:i64;
        let b = a + 3;
    }
    "#), @"40..41: use of possibly-uninitialized variable"
    );
}

#[test]
fn test_uninitialized_access_if() {
    insta::assert_snapshot!(diagnostics(
        r#"
    func foo() {
        let a:i64;
        if true { a = 3; } else { a = 4; }
        let b = a + 4;  // correct, `a` is initialized either way
    }

    func bar() {
        let a:i64;
        if true { a = 3; }
        let b = a + 4;  // `a` is possibly-unitialized
    }

    func baz() {
        let a:i64;
        if true { return } else { a = 4 };
        let b = a + 4;  // correct, `a` is initialized either way
    }

    func foz() {
        let a:i64;
        if true { a = 4 } else { return };
        let b = a + 4;  // correct, `a` is initialized either way
    }

    func boz() {
        let a:i64;
        return;
        let b = a + 4;  // `a` is not initialized but this is dead code anyway
    }
    "#,
    ), @"195..196: use of possibly-uninitialized variable");
}

#[test]
fn test_uninitialized_access_while() {
    insta::assert_snapshot!(diagnostics(
        r#"
    func foo(b:i64) {
        let a:i64;
        while b < 4 { b += 1; a = b; a += 1; }
        let c = a + 4;  // `a` is possibly-unitialized
    }
    "#,
    ), @"88..89: use of possibly-uninitialized variable");
}

#[test]
fn test_free_type_alias_without_type_ref() {
    insta::assert_snapshot!(diagnostics(
        r#"
    type Foo; // `Foo` must have a target type
    "#,
    ), @"0..9: free type alias without type ref");
}

#[test]
fn test_private_leak_function_return() {
    insta::assert_snapshot!(diagnostics(
        r#"
    struct Foo(usize);

    public func bar() -> Foo { // Foo is not public
        Foo(0)
    }

    public func baz(a: usize, b: usize) -> Foo {
        Foo(2)
    }

    public struct FooBar(usize);

    public func FooBaz() -> FooBar {
        FooBar(0)
    }

    func BarBaz() -> FooBar {
        FooBar(1)
    }
    "#,
    ), @"
    41..44: can't leak private type
    121..124: can't leak private type
    ");
}

#[test]
fn test_private_leak_function_args() {
    insta::assert_snapshot!(diagnostics(
        r#"
    struct Foo(usize);

    public func bar(a: Foo, b: isize) -> usize{ // Foo is not public
        0
    }

    public func baz(a: isize, b: Foo) -> isize {
        -1
    }

    public struct FooBar(usize);

    public func FooBaz(a: FooBar) -> FooBar {
        a
    }

    func BarBaz(a: isize, b: FooBar) -> isize {
        a
    }
    "#,
    ), @"
    39..42: can't leak private type
    123..126: can't leak private type
    ");
}

#[test]
fn test_private_leak_function_scoped() {
    insta::assert_snapshot!(diagnostics(
        r#"
    // Illegal, Bar has a smaller scope than this use statement
    public(super) struct Bar;

    // Illegal, Bar has a smaller scope than this function
    public func baz() -> Bar {
        Bar
    }
    "#,
    ), @"
    Offset(66): expected a declaration
    Offset(67): expected a declaration
    Offset(72): expected a declaration
    163..166: can't leak private type
    ");
}

#[test]
fn test_private_leak_alias() {
    insta::assert_snapshot!(diagnostics(
        r#"
    type Bar = usize;

    public func baz() -> Bar {
        0
    }
    "#,
    ), @"40..43: can't leak private type");
}

#[test]
fn test_type_alias_with_private_struct() {
    insta::assert_snapshot!(diagnostics(
        r#"
    struct Foo;

    public type Bar = Foo;
    "#,
    ), @"13..35: struct `Foo` is private");
}

#[test]
fn test_type_alias_with_private_type_alias() {
    insta::assert_snapshot!(diagnostics(
        r#"
    type Foo = i32;

    public type Bar = Foo;
    "#,
    ), @"17..39: type alias `Foo` is private");
}
