//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The long-duration service gate: a Codira "server" with long-lived
//!   state (a cache and a table of sessions, both of fixed size, held by the
//!   host as a rooted object) serves a request loop with steady allocation
//!   churn -- request and response strings, splitting, replaced cache
//!   entries and sessions -- for a configurable time, split into windows.
//!   It checks that the server's state stays intact, that memory stays
//!   bounded, and that throughput does not degrade.
//!
//! # What is checked, per window
//!
//! - Every request verifies the cache entry and session it touches (their
//!   contents are derived from their keys); any mismatch fails the run.
//! - Live bytes after a collection at the end of the window (the state is
//!   bounded by construction, so live bytes must plateau): the largest
//!   value in the second half of the run must stay within 1.5 times (plus
//!   64 KiB) the largest in the first half.
//! - Committed bytes: within the heap limit when one is set; otherwise
//!   pacing must have collected, the peak must stay under 64 MiB, and
//!   committed bytes follow the same plateau rule as live bytes, with 1 MiB
//!   of slack.
//! - Throughput (requests per second): the last window must reach half the
//!   median of the early windows. This catches gross slowdowns (a heap that
//!   fragments into a crawl, collections that get slower as the run goes
//!   on), not a few percent. It is enforced only when `CODIRA_SOAK_SECS` is
//!   set (the gate runner runs the test alone); in a plain `cargo test`,
//!   among other tests, it is printed.
//!
//! # Run length
//!
//! `CODIRA_SOAK_SECS` (default 4) sets the total time, in 8 windows. The
//! gate runner uses minutes; run it for hours locally with
//! `CODIRA_SOAK_SECS=7200`.
//!
//! # What it cannot catch
//!
//! Collections here are the heap limit's or pacing's, and one at the end of
//! each window (for the measurement). Growth
//! slower than the plateau rule's slack within the run time, leaks outside
//! the GC heap, and latency spikes (only window averages are measured) go
//! unnoticed.
use std::time::{Duration, Instant};

use codira_memory::gc::GcRuntime;
use codira_runtime::{RootedStruct, StructRef};
use codira_test::CompileAndRunTestDriver;

const SERVICE: &str = r#"
    public class Entry { key: i64, generation: i64, value: String }
    public class Session { id: i64, items: [i64], note: String }
    public class Server { cache: [Entry], sessions: [Session], next_session: i64, served: i64 }

    func value_of(key: i64, generation: i64) -> String {
        "v" + key.to_string() + ":" + generation.to_string()
    }

    func new_session(id: i64) -> Session {
        let items: [i64] = []
        var j = 0
        while j < id % 8 + 1 {
            items.push(id * 10 + j)
            j += 1
        }
        Session { id: id, items: items, note: "session-" + id.to_string() }
    }

    func entry_bad(e: Entry, key: i64) -> i64 {
        if e.key != key || e.value != value_of(key, e.generation) { 1 } else { 0 }
    }

    func session_bad(s: Session) -> i64 {
        if s.note != "session-" + s.id.to_string()
            || s.items.len() as i64 != s.id % 8 + 1
            || s.items[0] != s.id * 10 { 1 } else { 0 }
    }

    public func new_server(cache_n: i64, sessions_n: i64) -> Server {
        let s = Server { cache: [], sessions: [], next_session: sessions_n, served: 0 }
        var i = 0
        while i < cache_n {
            s.cache.push(Entry { key: i, generation: 0, value: value_of(i, 0) })
            i += 1
        }
        i = 0
        while i < sessions_n {
            s.sessions.push(new_session(i))
            i += 1
        }
        s
    }

    // Serves `n` requests; returns the total served so far, or minus the
    // number of corrupt objects seen.
    public func handle_batch(s: Server, seed: i64, n: i64) -> i64 {
        var x = seed
        var bad = 0
        var i = 0
        while i < n {
            x = (x * 1103515245 + 12345) % 2147483648
            let r = x / 65536
            let key = r % s.cache.len() as i64
            let request = "GET /item/" + key.to_string() + "?n=" + i.to_string()
            let parts = request.split("/")
            if parts[2] != key.to_string() + "?n=" + i.to_string() { bad += 1 }

            let entry = s.cache[key as usize]
            bad += entry_bad(entry, key)
            if r % 8 == 0 {
                // Replace a long-lived object: the old one becomes garbage.
                let g = entry.generation + 1
                s.cache[key as usize] = Entry { key: key, generation: g, value: value_of(key, g) }
            }

            let slot = (r / 7) % s.sessions.len() as i64
            let session = s.sessions[slot as usize]
            bad += session_bad(session)
            if r % 16 == 1 {
                s.sessions[slot as usize] = new_session(s.next_session)
                s.next_session += 1
            }

            let response = "200 " + s.cache[key as usize].value + " " + session.note + " " + "x".repeat(r % 64)
            if response.len() < 8 { bad += 1 }
            i += 1
        }
        s.served += n
        if bad > 0 { 0 - bad } else { s.served }
    }
"#;

const BATCH: i64 = 500;
const WINDOWS: usize = 8;
/// Without a heap limit, pacing must keep the heap under this.
const PACED_PEAK_BOUND: usize = 64 * 1024 * 1024;

struct Window {
    requests: i64,
    seconds: f64,
    live: usize,
    committed: usize,
}

fn soak_time() -> Duration {
    let secs = std::env::var("CODIRA_SOAK_SECS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(4.0);
    Duration::from_secs_f64(secs)
}

fn soak(heap_limit: Option<usize>) {
    let driver = CompileAndRunTestDriver::new(SERVICE, |builder| match heap_limit {
        Some(limit) => builder.heap_limit(limit),
        None => builder,
    })
    .expect("Failed to build test driver");
    let runtime = &driver.runtime;
    let gc = runtime.gc();
    let server: RootedStruct = {
        let _guard = codira_runtime::enter_codira();
        let server: StructRef<'_> = runtime.invoke("new_server", (512i64, 128i64)).unwrap();
        server.root()
    };

    let window_time = soak_time() / WINDOWS as u32;
    let mut windows = Vec::new();
    let mut served = 0i64;
    let mut seed = 1i64;
    for _ in 0..WINDOWS {
        let start = Instant::now();
        let mut requests = 0;
        while requests == 0 || start.elapsed() < window_time {
            let total: i64 = runtime
                .invoke("handle_batch", (server.as_ref(runtime), seed, BATCH))
                .unwrap();
            assert!(total >= 0, "batch {seed}: {} corrupt objects", -total);
            served += BATCH;
            assert_eq!(total, served, "the server lost count");
            requests += BATCH;
            seed += 1;
        }
        let seconds = start.elapsed().as_secs_f64();
        gc.collect_blocking().unwrap();
        windows.push(Window {
            requests,
            seconds,
            live: gc.stats().allocated_memory,
            committed: gc.committed_bytes(),
        });
    }

    eprintln!("soak ({heap_limit:?} limit): window, requests/s, live bytes, committed bytes");
    for (i, w) in windows.iter().enumerate() {
        eprintln!(
            "  {i}: {:>10.0} {:>10} {:>10}",
            w.requests as f64 / w.seconds,
            w.live,
            w.committed
        );
    }

    let half = WINDOWS / 2;
    let first_live = windows[..half].iter().map(|w| w.live).max().unwrap();
    let last_live = windows[half..].iter().map(|w| w.live).max().unwrap();
    assert!(
        last_live <= first_live * 3 / 2 + 64 * 1024,
        "live bytes grew from {first_live} to {last_live}"
    );
    match heap_limit {
        Some(limit) => {
            for w in &windows {
                assert!(
                    w.committed <= limit,
                    "{} committed over a {limit} limit",
                    w.committed
                );
            }
            assert!(
                gc.emergency_collections() > 0,
                "the limit was never reached"
            );
        }
        None => {
            let peak = gc.heap_stats().peak_heap_bytes;
            assert!(
                peak <= PACED_PEAK_BOUND,
                "pacing let the heap reach {peak} bytes"
            );
            assert!(
                gc.heap_stats().collections.paced > 0,
                "pacing never collected"
            );
            let first = windows[..half].iter().map(|w| w.committed).max().unwrap();
            let last = windows[half..].iter().map(|w| w.committed).max().unwrap();
            assert!(
                last <= first * 3 / 2 + 1024 * 1024,
                "committed bytes grew from {first} to {last}"
            );
        }
    }
    // Throughput depends on what else the machine runs, so it is only
    // enforced when the gate runner sets the duration (and runs the test
    // alone); otherwise it is reported.
    if std::env::var_os("CODIRA_SOAK_SECS").is_none() {
        return finish(gc, server);
    }
    // Window 0 warms up; compare the last window with windows 1 to 3.
    let mut early: Vec<f64> = windows[1..4]
        .iter()
        .map(|w| w.requests as f64 / w.seconds)
        .collect();
    early.sort_by(f64::total_cmp);
    let last = windows.last().unwrap();
    let last_rate = last.requests as f64 / last.seconds;
    assert!(
        last_rate >= early[1] / 2.0,
        "throughput fell from {:.0} to {last_rate:.0} requests/s",
        early[1]
    );

    finish(gc, server);
}

/// Drops the server: everything it held must be freed.
fn finish(
    gc: &codira_memory::gc::MarkSweep<codira_memory::gc::NoopObserver<codira_memory::gc::Event>>,
    server: RootedStruct,
) {
    drop(server);
    gc.collect_blocking().unwrap();
    assert_eq!(gc.stats().allocated_memory, 0, "the server's state leaked");
}

#[test]
fn a_service_with_a_heap_limit_stays_bounded_and_keeps_its_throughput() {
    soak(Some(64 * 64 * 1024));
}

#[test]
fn a_paced_service_stays_bounded_and_keeps_its_throughput() {
    soak(None);
}
