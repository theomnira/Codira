//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Tests of `RuntimeBuilder::heap_limit`: Codira code that allocates far more
//!   than the cap keeps running because allocation collects (with the Codira
//!   stack as conservative roots), and a program whose live data exceeds the
//!   cap aborts with a message instead of crashing.
use std::process::Command;

use codira_test::CompileAndRunTestDriver;

const BLOCK: usize = 64 * 1024;

const CHURN: &str = r#"
    public class Node {
        value: i64,
        twice: i64,
    }

    public func churn(n: i64) -> i64 {
        // `keep` is referenced only from this function's frame: if the
        // collector that runs inside the allocations below missed it, its
        // cell would be reused by a later `Node` and `keep.value` would change.
        let keep = Node { value: 7, twice: 14 };
        var total = 0;
        var i = 0;
        while i < n {
            let node = Node { value: i, twice: i * 2 };
            total += node.twice - node.value + keep.value;
            i += 1;
        }
        total + keep.twice
    }
"#;

#[test]
fn allocation_beyond_the_cap_collects_and_continues() {
    let driver = CompileAndRunTestDriver::new(CHURN, |builder| builder.heap_limit(4 * BLOCK))
        .expect("Failed to build test driver");

    let n = 200_000i64;
    let total: i64 = driver.runtime.invoke("churn", (n,)).unwrap();
    assert_eq!(total, n * (n - 1) / 2 + 7 * n + 14);

    let gc = driver.runtime.gc();
    assert_eq!(gc.heap_limit(), Some(4 * BLOCK));
    assert!(gc.emergency_collections() > 0, "the cap was never reached");
    assert!(gc.committed_bytes() <= 4 * BLOCK);
}

#[test]
fn without_a_cap_nothing_is_collected_during_allocation() {
    let driver = CompileAndRunTestDriver::new(CHURN, |builder| builder)
        .expect("Failed to build test driver");
    let n = 20_000i64;
    let total: i64 = driver.runtime.invoke("churn", (n,)).unwrap();
    assert_eq!(total, n * (n - 1) / 2 + 7 * n + 14);
    assert_eq!(driver.runtime.gc().emergency_collections(), 0);
}

/// Environment variable that makes [`exhaustion_child`] do its work.
const CHILD_ENV: &str = "CODIRA_HEAP_LIMIT_EXHAUSTION_CHILD";

/// Runs in a child process (see [`exhausting_the_cap_aborts_with_a_message`]):
/// allocates an array (24 KiB) larger than the whole cap (16 KiB) from Codira
/// code.
#[test]
fn exhaustion_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let elements = vec!["1"; 3_000].join(", ");
    let source = format!("public func big() -> i64 {{ let a = [{elements}]; a[2999] }}");
    let driver = CompileAndRunTestDriver::new(&source, |builder| builder.heap_limit(16 * 1024))
        .expect("Failed to build test driver");
    let _: i64 = driver.runtime.invoke("big", ()).unwrap();
    unreachable!("the allocation must not succeed");
}

#[test]
fn exhausting_the_cap_aborts_with_a_message() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "exhaustion_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .expect("failed to run the child test");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "child succeeded:\n{stderr}");
    assert!(
        stderr.contains("codira runtime: cannot allocate `[core::i64]`")
            && stderr.contains("GC heap limit of 16384 bytes exceeded"),
        "unexpected child output:\n{stderr}"
    );
}
