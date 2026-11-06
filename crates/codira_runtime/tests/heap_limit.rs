//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Tests of `RuntimeBuilder::heap_limit` and pacing: Codira code that
//!   allocates far more than the cap keeps running because allocation
//!   collects (with the Codira stack as conservative roots); without a cap,
//!   pacing collects and keeps the heap small; a program whose live data
//!   exceeds the cap ends with the documented diagnostic and exit status
//!   instead of crashing; the statistics count what happened.
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
fn without_a_cap_nothing_is_collected_at_a_limit() {
    let driver = CompileAndRunTestDriver::new(CHURN, |builder| builder)
        .expect("Failed to build test driver");
    let n = 20_000i64;
    let total: i64 = driver.runtime.invoke("churn", (n,)).unwrap();
    assert_eq!(total, n * (n - 1) / 2 + 7 * n + 14);
    assert_eq!(driver.runtime.gc().emergency_collections(), 0);
    assert_eq!(driver.runtime.gc().heap_limit(), None);
}

/// Without any cap, pacing collects from the allocations of Codira code:
/// the heap stays near its 4 MiB floor while the program allocates 32 MB.
#[test]
fn without_a_cap_pacing_collects() {
    let driver = CompileAndRunTestDriver::new(CHURN, |builder| builder)
        .expect("Failed to build test driver");
    let n = 2_000_000i64; // 16-byte payloads: 32 MB in total
    let total: i64 = driver.runtime.invoke("churn", (n,)).unwrap();
    assert_eq!(total, n * (n - 1) / 2 + 7 * n + 14);

    let stats = driver.runtime.stats();
    assert!(
        stats.collections.paced >= 4,
        "{} paced collections",
        stats.collections.paced
    );
    assert_eq!(stats.collections.at_limit, 0);
    assert!(stats.objects_allocated >= n as u64);
    assert!(stats.bytes_allocated >= 16 * n as u64);
    assert!(stats.objects_reclaimed >= n as u64 / 2);
    assert!(
        stats.peak_heap_bytes < 16 << 20,
        "peak {} bytes",
        stats.peak_heap_bytes
    );
    assert_eq!(stats.pauses.count, stats.collections.total());
}

/// With pacing off and no cap, nothing collects: the heap grows with the
/// allocations.
#[test]
fn with_pacing_off_and_no_cap_the_heap_grows() {
    let driver = CompileAndRunTestDriver::new(CHURN, |builder| builder.gc_growth(None))
        .expect("Failed to build test driver");
    let n = 400_000i64;
    let _: i64 = driver.runtime.invoke("churn", (n,)).unwrap();
    let stats = driver.runtime.stats();
    assert_eq!(stats.collections.total(), 0);
    assert!(stats.peak_heap_bytes >= 400_000 * 48);
}

/// Environment variable that makes [`exhaustion_child`] do its work.
const CHILD_ENV: &str = "CODIRA_HEAP_LIMIT_EXHAUSTION_CHILD";

/// Runs in a child process (see [`exhausting_the_cap_ends_the_process`]):
/// what the variable says.
#[test]
fn exhaustion_child() {
    let Some(which) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    if which == "array" {
        // An array (24 KiB) larger than the whole cap (64 KiB of blocks is
        // the least the heap takes, so the cap is below one array).
        let elements = vec!["1"; 3_000].join(", ");
        let source = format!("public func big() -> i64 {{ let a = [{elements}]; a[2999] }}");
        let driver = CompileAndRunTestDriver::new(&source, |builder| builder.heap_limit(16 * 1024))
            .expect("Failed to build test driver");
        println!("before the allocation");
        let _: i64 = driver.runtime.invoke("big", ()).unwrap();
    } else {
        // A list whose live data grows past the cap, one node at a time.
        let source = r#"
            public class Node { value: i64, next: Node? }
            public func grow(n: i64) -> i64 {
                var head: Node? = nil
                var i = 0
                while i < n {
                    head = Node { value: i, next: head }
                    i += 1
                }
                head!.value
            }
        "#;
        let driver = CompileAndRunTestDriver::new(source, |builder| {
            builder
                .heap_limit(1 << 20)
                .heap_limit_note("the limit came from the test\nraise it in the test")
        })
        .expect("Failed to build test driver");
        let _: i64 = driver.runtime.invoke("grow", (10_000_000i64,)).unwrap();
    }
    unreachable!("the allocation must not succeed");
}

fn run_exhaustion_child(which: &str) -> (std::process::Output, String) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "exhaustion_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, which)
        .output()
        .expect("failed to run the child test");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output, stderr)
}

/// A single allocation larger than the cap: the documented diagnostic and
/// exit status, after what was printed before.
#[test]
fn exhausting_the_cap_ends_the_process() {
    let (output, stderr) = run_exhaustion_child("array");
    assert_eq!(
        output.status.code(),
        Some(codira_runtime::HEAP_EXHAUSTED_EXIT_STATUS),
        "child output:\n{stderr}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("before the allocation"));
    for needle in [
        "fatal: out of memory: cannot allocate `[core::i64]`",
        "heap limit: 16384 bytes (16.0 KiB)",
        "the allocation needed 24048 more",
        "live:       0 bytes after a full collection",
    ] {
        assert!(stderr.contains(needle), "{needle:?} not in:\n{stderr}");
    }
}

/// Live data that grows past the cap: the collections at the cap reclaim
/// nothing, and the diagnostic says how much is live.
#[test]
fn live_data_beyond_the_cap_ends_the_process() {
    let (output, stderr) = run_exhaustion_child("list");
    assert_eq!(
        output.status.code(),
        Some(codira_runtime::HEAP_EXHAUSTED_EXIT_STATUS),
        "child output:\n{stderr}"
    );
    let live: usize = stderr
        .lines()
        .find_map(|line| line.trim().strip_prefix("live:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no live bytes in:\n{stderr}"));
    // Most of the 1 MiB cap holds list nodes (24-byte payloads in 64-byte
    // cells: 37% of the committed bytes).
    assert!(live > 250_000, "live {live} in:\n{stderr}");
    assert!(
        stderr.contains("heap limit: 1048576 bytes (1.00 MiB)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("  the limit came from the test\n  raise it in the test"),
        "{stderr}"
    );
}
