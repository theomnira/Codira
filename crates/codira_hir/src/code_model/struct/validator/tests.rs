//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#[cfg(test)]
use crate::utils::tests::*;

#[test]
fn test_private_leak_struct_fields() {
    insta::assert_snapshot!(diagnostics(
        r#"

    struct Foo(usize);
    public struct Bar(usize);

    // valid, bar is public
    public struct Baz {
        foo: Foo,
        public bar: Bar,
    }

    // invalid, Foo is private
    public struct FooBar {
        public foo: Foo,
        public bar: Bar,
    }

    // valid, FooBaz is private
    struct FooBaz {
        public foo: Foo,
        public bar: Bar,
    }

    public(package) struct BarBaz;

    // invalid, exporting public(crate) to public
    public struct FooBarBaz {
        public foo: Foo,
        public bar: Bar,
    }
    "#),
    @"
    Offset(319): expected a declaration
    Offset(320): expected a declaration
    Offset(327): expected a declaration
    195..198: can't leak private type
    433..436: can't leak private type
    ");
}
