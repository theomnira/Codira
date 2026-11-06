//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - Collections requested by a host thread while Codira code builds, slices
//!   and compares strings. In a test binary of its own: a collection stops
//!   every thread of the process that runs Codira code, so beside the other
//!   string tests it would stall them (a hot reload waiting for its update,
//!   for one) rather than test anything.
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use codira_test::CompileAndRunTestDriver;

const STRESS: &str = r#"
    public class Holder { s: String, n: i64 }

    func expected(seed: i64, k: i64) -> String {
        "s" + seed.to_string() + "-" + k.to_string() + "-é"
    }

    func check(h: Holder, seed: i64) -> i64 {
        if h.s == expected(seed, h.n) { 0 } else { 1 }
    }

    public func churn(seed: i64, rounds: i64) -> i64 {
        // Live only through this frame and the objects it reaches.
        let keep = Holder { s: expected(seed, 7), n: 7 }
        var built = "".to_string()
        let parts = "x,yy,zzz".to_string().split(",")
        var bad = 0
        var i = 0
        while i < rounds {
            let junk = expected(0 - i, i) + "garbage".repeat(3)
            built += "ab"
            let h = Holder { s: expected(seed, i), n: i }
            bad += check(h, seed)
            bad += check(keep, seed)
            if parts[1] != "yy" { bad += 1 }
            if junk.len() == 0 { bad += 1 }
            i += 1
        }
        if built.len() != (rounds * 2) as usize { bad += 1 }
        var j = 0
        while j < built.len() {
            if built.byte_at(j) != 97u8 + (j % 2) as u8 { bad += 1 }
            j += 1
        }
        bad
    }
"#;

/// Runs the three rounds of `churn` and returns how long they took.
fn rounds(driver: &CompileAndRunTestDriver) -> Duration {
    let start = Instant::now();
    for seed in 1..4i64 {
        let bad: i64 = driver.runtime.invoke("churn", (seed, 20_000i64)).unwrap();
        assert_eq!(bad, 0, "a collection freed or changed a live string");
    }
    start.elapsed()
}

/// A host thread collects in a loop, with no pause of its own, while Codira
/// code builds, slices and compares strings.
///
/// Collections stop the world. Before collections requested from outside
/// waited for the mutator window (`GcPolicy`), a host that requested the next
/// stop as soon as the previous one ended let the Codira thread make about
/// one allocation per collection: this loop's half a million allocations
/// took from seconds to minutes, and the collecting thread had to sleep
/// between collections. Now every request waits until the Codira thread has
/// run for the window since the previous collection, and a request is
/// satisfied by any collection that starts while it waits (a paced one,
/// usually, under the default policy), so the rounds take about as long as
/// without the collecting thread.
#[test]
fn host_collections_keep_live_strings_intact() {
    let driver = CompileAndRunTestDriver::new(STRESS, |builder| builder)
        .expect("Failed to build test driver");
    // Warm up (first-call costs), then time the rounds with nobody else
    // collecting.
    rounds(&driver);
    let gc = driver.runtime.gc();
    let alone_before = gc.heap_stats().collections;
    let alone = rounds(&driver);
    let alone_collections = gc.heap_stats().collections.total() - alone_before.total();

    let before = gc.heap_stats().collections;
    let done = AtomicBool::new(false);
    let (with_collector, calls, coalesced) = std::thread::scope(|s| {
        let host = s.spawn(|| {
            let (mut calls, mut coalesced) = (0u64, 0u64);
            while !done.load(Ordering::Relaxed) {
                let report = gc.collect_blocking().unwrap();
                calls += 1;
                coalesced += u64::from(report.coalesced);
            }
            (calls, coalesced)
        });
        let elapsed = {
            // Stops the collecting thread even when an assertion fails.
            struct SetOnDrop<'a>(&'a AtomicBool);
            impl Drop for SetOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            let _done = SetOnDrop(&done);
            rounds(&driver)
        };
        let (calls, coalesced) = host.join().unwrap();
        (elapsed, calls, coalesced)
    });
    let after = gc.heap_stats().collections;
    let explicit = after.explicit - before.explicit;
    let paced = after.paced - before.paced;
    eprintln!(
        "churn rounds: {alone:?} alone ({alone_collections} collections); {with_collector:?} \
         with a host thread collecting in a loop: {calls} calls, {explicit} collections of \
         their own, {coalesced} satisfied by another, {paced} paced"
    );
    assert!(calls >= 3, "the host thread made only {calls} calls");
    assert_eq!(calls, explicit + coalesced);
    assert!(
        explicit + paced >= 5,
        "only {} collections",
        explicit + paced
    );
    // The policy bounds what requests from outside stop the world for to a
    // fifth of the time; allow for a loaded machine.
    assert!(
        with_collector < alone * 2 + Duration::from_millis(500),
        "{with_collector:?} with the collecting thread, {alone:?} without"
    );
}
