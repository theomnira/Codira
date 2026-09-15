//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_runtime::LinkFunctionsError;
use codira_test::CompileAndRunTestDriver;

#[macro_use]
mod util;

#[test]
fn invoke() {
    let driver = CompileAndRunTestDriver::new(
        r#"
    public func sum(a: i32, b: i32) -> i32 { a + b }
        "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let result: i32 = driver.runtime.invoke("sum", (123i32, 456i32)).unwrap();
    assert_eq!(123 + 456, result);
}

#[test]
fn arrays_are_collected() {
    let driver = CompileAndRunTestDriver::new(
        r#"
    public func main() {
        let a = [1,2,3,4,5,6,7,8,9,1,2,3,4,5,6,7,8,9,1,2,3,4,5,6,7,8,9,1,2,3,4,5,6,7,8,9,]
    }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    assert!(!driver.runtime.gc_collect());
    let _: () = driver
        .runtime
        .invoke("main", ())
        .expect("error invoking main function");
    assert!(driver.runtime.gc_collect());
    assert!(!driver.runtime.gc_collect());
}

#[test]
fn arrays() {
    let driver = CompileAndRunTestDriver::new(
        r#"
    /// A version struct of explicitly 24 bits, this requires alignment.
    struct Version {
        major: u8,
        minor: u16,
    }

    /// Constructor function for a Version
    func version(major: u8, minor: u16) -> Version {
        Version { major: major, minor: minor }
    }

    /// Returns true if a is considered smaller than b
    func version_greater(a: Version, b: Version) -> bool {
        a.major > b.major || a.major == b.major && a.minor > b.minor
    }

    /// Performs bubble sort on an array of versions
    func bubble_sort(array: [Version], len: usize) {
        let i = 0;
        while i<len {
            let j = 1;
            while j<len-i {
                if version_greater(array[j-1], array[j]) {
                    let tmp = array[j];
                    array[j] = array[j-1];
                    array[j-1] = tmp;
                }
                j += 1;
            }
            i += 1;
        }
    }

    public func main() -> u16 {
        let a = [version(3,4), version(1,2), version(4,5), version(0, 9), version(1,1)]
        bubble_sort(a, 5)
        a[0].minor
    }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    assert_invoke_eq!(u16, 9, driver, "main");
}

// Regression test: a cross-module call to a function returning a
// `struct` (value-kind) used to return garbage. The dispatch table used
// for cross-module Codira-to-Codira calls is populated by the runtime from
// each module's `_wrapper` function (the same C-ABI-compatible entry
// point used for host-language marshalling), which GC-boxes a
// struct-by-value return -- but the call site was declaring and invoking
// it as if it had the plain internal signature (a raw by-value struct
// return), a genuine ABI mismatch. Fixed in `codira_codegen`'s
// `DispatchTableBuilder::collect_fn_def` (dispatch table slot type now
// matches the wrapper) and `gen_call` (boxes struct-by-value arguments and
// unboxes a struct-by-value return for dispatch-table calls, mirroring
// what `gen_fn_wrapper` does on the callee side).
#[test]
fn cross_module_struct_return() {
    let driver = CompileAndRunTestDriver::from_fixture(
        r#"
    //- /codira.toml
    [package]
    name="foo"
    version="0.0.0"

    //- /src/mod.code
    import foo.make_pair

    public func main() -> i64 {
        let p = make_pair(3, 4);
        p.a * 100 + p.b
    }

    //- /src/foo.code
    public struct Pair {
        public a: i64,
        public b: i64,
    }

    public func make_pair(x: i64, y: i64) -> Pair {
        Pair { a: x, b: y }
    }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    assert_invoke_eq!(i64, 304, driver, "main");
}

// Same regression, but for a `class` (GC-kind) struct return, which was
// already correct (both the wrapper and the internal signature agree on a
// GC reference for these) -- kept alongside the `struct` case above so a
// future change can't silently reintroduce an asymmetry between the two.
#[test]
fn cross_module_class_return() {
    let driver = CompileAndRunTestDriver::from_fixture(
        r#"
    //- /codira.toml
    [package]
    name="foo"
    version="0.0.0"

    //- /src/mod.code
    import foo.make_pair

    public func main() -> i64 {
        let p = make_pair(7, 8);
        p.a * 100 + p.b
    }

    //- /src/foo.code
    public class Pair {
        public a: i64,
        public b: i64,
    }

    public func make_pair(x: i64, y: i64) -> Pair {
        Pair { a: x, b: y }
    }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    assert_invoke_eq!(i64, 708, driver, "main");
}

#[test]
fn multiple_modules() {
    let driver = CompileAndRunTestDriver::from_fixture(
        r#"
    //- /codira.toml
    [package]
    name="foo"
    version="0.0.0"

    //- /src/mod.code
    import foo.foo

    public func main() -> i32 { foo() }

    //- /src/foo.code
    public func foo() -> i32 { 5 }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    assert_invoke_eq!(i32, 5, driver, "main");
}

#[test]
fn cyclic_modules() {
    let driver = CompileAndRunTestDriver::from_fixture(
        r#"
    //- /codira.toml
    [package]
    name="foo"
    version="0.0.0"

    //- /src/mod.code
    import foo.foo

    public func main() -> i32 { foo() }

    internal func bar() -> i32 { 5 }

    //- /src/foo.code
    import super.bar

    public func foo() -> i32 { bar() }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    assert_invoke_eq!(i32, 5, driver, "main");
}

#[test]
fn from_fixture() {
    let driver = CompileAndRunTestDriver::from_fixture(
        r#"
    //- /codira.toml
    [package]
    name="foo"
    version="0.0.0"

    //- /src/mod.code
    public func main() -> i32 { 5 }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");
    assert_invoke_eq!(i32, 5, driver, "main");
}

#[test]
fn error_assembly_not_linkable() {
    const EXPECTED_FN_NAME: &str = "dependency";

    let driver = CompileAndRunTestDriver::new(
        &format!(
            r"
    extern func {EXPECTED_FN_NAME}() -> i32;
    
    public func main() -> i32 {{ {EXPECTED_FN_NAME}() }}
    "
        ),
        |builder| builder,
    );
    assert_eq!(
        driver.unwrap_err().to_string(),
        LinkFunctionsError::MissingDependencies {
            functions: vec![EXPECTED_FN_NAME.to_string()]
        }
        .to_string()
    );
}

#[test]
fn arg_missing_bug() {
    let driver = CompileAndRunTestDriver::new(
        r"
    public func fibonacci_n() -> i64 {
        let n = arg();
        fibonacci(n)
    }

    func arg() -> i64 {
        5
    }

    func fibonacci(n: i64) -> i64 {
        if n <= 1 {
            n
        } else {
            fibonacci(n - 1) + fibonacci(n - 2)
        }
    }",
        |builder| builder,
    );
    driver.unwrap();
}

#[test]
fn cyclic_struct() {
    let driver = CompileAndRunTestDriver::new(
        r"
        public class Foo {
            foo: Foo
        }

        public class FooBar {
            bar: BarFoo
        }

        public class BarFoo {
            foo: FooBar
        }
        ",
        |builder| builder,
    )
    .unwrap();

    let foo_ty = driver.runtime.get_type_info_by_name("Foo").unwrap();
    let foo_foo_ty = foo_ty
        .as_struct()
        .unwrap()
        .fields()
        .find_by_name("foo")
        .unwrap()
        .ty();
    assert_eq!(foo_foo_ty, foo_ty);
}
