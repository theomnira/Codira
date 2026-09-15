//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#[macro_use]
mod util;

use codira_test::CompileAndRunTestDriver;

#[test]
fn unknown_function() {
    const EXPECTED_FN_NAME: &str = "may";

    let driver = CompileAndRunTestDriver::new(
        r"
    public func main() -> i32 { 5 }
    ",
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let result: Result<i32, _> = driver.runtime.invoke(EXPECTED_FN_NAME, ());
    let err = result.unwrap_err();

    assert_eq!(
        err.to_string(),
        format!("failed to obtain function '{EXPECTED_FN_NAME}', no such function exists.")
    );
}

#[test]
fn exact_case_sensitive_match_exists_function() {
    const EXPECTED_FN_NAME: &str = "Foo";

    let driver = CompileAndRunTestDriver::new(
        r"
    public func main() -> i32 { 5 }
    public func foo() -> i32 { 4 }
    public func bar() -> i32 { 3 }
    ",
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let result: Result<i32, _> = driver.runtime.invoke(EXPECTED_FN_NAME, ());
    let err = result.unwrap_err();

    assert_eq!(
        err.to_string(),
        format!(
            "failed to obtain function '{}', no such function exists. There is a function with a similar name: {}",
            EXPECTED_FN_NAME, EXPECTED_FN_NAME.to_lowercase()
        )
    );
}

#[test]
fn close_match_exists_function() {
    const EXPECTED_FN_NAME: &str = "calculatedistance";

    let driver = CompileAndRunTestDriver::new(
        r"
    public func main() -> i32 { 5 }
    public func calculate_distance() -> i32 { 4 }
    public func bar() -> i32 { 3 }
    ",
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let result: Result<i32, _> = driver.runtime.invoke(EXPECTED_FN_NAME, ());
    let err = result.unwrap_err();

    assert_eq!(
        err.to_string(),
        format!(
            "failed to obtain function '{EXPECTED_FN_NAME}', no such function exists. There is a function with a similar name: calculate_distance"
        )
    );
}

#[test]
fn no_close_match_exists_function() {
    const EXPECTED_FN_NAME: &str = "calculate";

    let driver = CompileAndRunTestDriver::new(
        r"
    public func main() -> i32 { 5 }
    public func calculate_distance() -> i32 { 4 }
    ",
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let result: Result<i32, _> = driver.runtime.invoke(EXPECTED_FN_NAME, ());
    let err = result.unwrap_err();

    assert_eq!(
        err.to_string(),
        format!("failed to obtain function '{EXPECTED_FN_NAME}', no such function exists.")
    );
}

#[test]
fn multiple_match_exists_function() {
    const EXPECTED_FN_NAME: &str = "foobar";

    let driver = CompileAndRunTestDriver::new(
        r"
    public func main() -> i32 { 5 }
    public func foobar_a() -> i32 { 4 }
    public func foobar_b() -> i32 { 4 }
    ",
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let result: Result<i32, _> = driver.runtime.invoke(EXPECTED_FN_NAME, ());
    let err = result.unwrap_err();

    assert_eq!(
        err.to_string(),
        format!(
            "failed to obtain function '{EXPECTED_FN_NAME}', no such function exists. There is a function with a similar name: foobar_b"
        )
    );
}
