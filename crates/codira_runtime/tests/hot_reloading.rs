//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#[macro_use]
mod util;

use codira_runtime::StructRef;
use codira_test::CompileAndRunTestDriver;

#[test]
fn reloadable_function_single_file() {
    let mut driver = CompileAndRunTestDriver::new(
        r"
    public func main() -> i32 { 5 }
    ",
        |builder| builder,
    )
    .expect("Failed to build test driver");
    assert_invoke_eq!(i32, 5, driver, "main");

    driver.update_file(
        "mod.code",
        r"
    public func main() -> i32 { 10 }
    ",
    );
    assert_invoke_eq!(i32, 10, driver, "main");
}

#[test]
fn reloadable_function_multi_file() {
    let mut driver = CompileAndRunTestDriver::from_fixture(
        r#"
    //- /codira.toml
    [package]
    name="foo"
    version="0.0.0"

    //- /src/mod.code
    import root.foo.bar
    public func main() -> i32 { bar() }

    //- /src/foo.code
    public func bar() -> i32 { 5 }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");
    assert_invoke_eq!(i32, 5, driver, "main");

    driver.update_file(
        "foo.code",
        r#"
    public func bar() -> i32 { 10 }
    "#,
    );
    assert_invoke_eq!(i32, 10, driver, "main");
}

#[test]
fn reloadable_struct_decl_single_file() {
    let mut driver = CompileAndRunTestDriver::new(
        r#"
    public class Args {
        n: i32,
        foo: Bar,
    }
    
    class Bar {
        m: i32,
    }

    public func args() -> Args {
        Args { n: 3, foo: Bar { m: 1 }, }
    }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let args: StructRef<'_> = driver
        .runtime
        .invoke("args", ())
        .expect("Failed to call function");

    let foo_struct: StructRef<'_> = args.get("foo").expect("Failed to get struct field");
    assert_eq!(
        foo_struct
            .get::<i32>("m")
            .expect("Failed to get struct field"),
        1
    );

    let foo_struct = foo_struct.root();

    driver.update_file(
        "mod.code",
        r#"
    public class Args {
        n: i32,
        foo: Bar,
    }
    
    class Bar {
        m: i64,
    }

    public func args() -> Args {
        Args { n: 3, foo: Bar { m: 1 }, }
    }
    "#,
    );

    let foo_struct = foo_struct.as_ref(&driver.runtime);
    assert_eq!(
        foo_struct
            .get::<i64>("m")
            .expect("Failed to get struct field"),
        1
    );
}

#[test]
fn reloadable_struct_decl_multi_file() {
    let mut driver = CompileAndRunTestDriver::from_fixture(
        r#"
    //- /codira.toml
    [package]
    name="foo"
    version="0.0.0"

    //- /src/mod.code
    import root.foo.Bar
    public class Args {
        n: i32,
        foo: Bar,
    }

    public func args() -> Args {
        Args { n: 3, foo: Bar { m: 1 }, }
    }

    //- /src/foo.code
    public class Bar {
        m: i64,
    }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let args: StructRef<'_> = driver
        .runtime
        .invoke("args", ())
        .expect("Failed to call function");

    assert_eq!(args.get::<i32>("n").expect("Failed to get struct field"), 3);

    let foo_struct: StructRef<'_> = args.get("foo").expect("Failed to get struct field");
    assert_eq!(
        foo_struct
            .get::<i64>("m")
            .expect("Failed to get struct field"),
        1
    );

    let args = args.root();
    let foo_struct = foo_struct.root();

    driver.update_file(
        "mod.code",
        r#"
    import root.foo.Bar
    public class Args {
        n: i64,
        foo: Bar,
    }

    public func args() -> Args {
        Args { n: 3, foo: Bar { m: 1 }, }
    }
    "#,
    );

    let args = args.as_ref(&driver.runtime);
    assert_eq!(args.get::<i64>("n").expect("Failed to get struct field"), 3);

    let foo_struct = foo_struct.as_ref(&driver.runtime);
    assert_eq!(
        foo_struct
            .get::<i64>("m")
            .expect("Failed to get struct field"),
        1
    );
}
