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
use std::sync::atomic::{AtomicBool, Ordering};

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

/// A host thread collects in a loop while Codira code builds, slices and
/// compares strings.
///
/// The collecting thread pauses between collections. Collections stop the
/// world, and a host that requests the next stop as soon as the previous one
/// ends lets the Codira thread make about one allocation per collection: this
/// loop's half a million allocations then took from seconds to minutes,
/// depending on how the scheduler interleaved the two threads. Paced, the test
/// still runs thousands of collections at arbitrary points of the string code.
#[test]
fn host_collections_keep_live_strings_intact() {
    let driver = CompileAndRunTestDriver::new(STRESS, |builder| builder)
        .expect("Failed to build test driver");
    let gc = driver.runtime.gc();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                gc.collect();
                std::thread::sleep(std::time::Duration::from_micros(100));
            }
        });
        // Stops the collecting thread even when an assertion fails.
        struct SetOnDrop<'a>(&'a AtomicBool);
        impl Drop for SetOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _done = SetOnDrop(&done);
        for seed in 1..4i64 {
            let bad: i64 = driver.runtime.invoke("churn", (seed, 20_000i64)).unwrap();
            assert_eq!(bad, 0, "a collection freed or changed a live string");
        }
    });
}
