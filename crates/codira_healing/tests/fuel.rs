//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - A HERACLES guard observes fuel exhaustion in ahead-of-time compiled Codira
//!   code as `FaultKind::FuelExhausted`, gets control back at the guarded call,
//!   and the runtime keeps working after it is refuelled.
#![cfg(windows)]

use codira_healing::trap::{self, FaultKind};
use codira_test::CompileAndRunTestDriver;

#[test]
fn a_guard_catches_fuel_exhaustion_and_the_runtime_recovers() {
    let driver = CompileAndRunTestDriver::with_fuel_instrumentation(
        r#"
        public func forever() -> i64 {
            var i = 0;
            loop {
                i += 1;
            }
        }

        public func count(n: i64) -> i64 {
            var i = 0;
            while i < n {
                i += 1;
            }
            i
        }
        "#,
        |builder| builder.fuel(50_000),
    )
    .expect("Failed to build test driver");
    let runtime = &driver.runtime;

    trap::install();
    let fault = trap::protected(|| {
        let result: i64 = runtime.invoke("forever", ()).unwrap();
        result
    })
    .expect_err("an endless loop cannot finish within its fuel");
    assert_eq!(fault.kind, FaultKind::FuelExhausted);
    assert_eq!(runtime.fuel_remaining(), Some(0));

    // The abandoned call held nothing; a refuelled runtime runs code again.
    runtime.set_fuel(Some(1_000));
    let result = trap::protected(|| {
        let result: i64 = runtime.invoke("count", (100i64,)).unwrap();
        result
    });
    assert_eq!(result, Ok(100));
    assert_eq!(runtime.fuel_remaining(), Some(1_000 - 101));
}
