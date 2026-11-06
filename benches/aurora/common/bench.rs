//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - What every Rust baseline of the AURORA suite shares, included with
//!   `#[path = "../common/bench.rs"] mod bench;`: the kernel name from the
//!   command line, the monotonic clock, the `@@ ns=.. check=..` line the
//!   runner reads (with the peak resident set from `/proc/self/status`), the
//!   xorshift generator the Codira programs use, and latency percentiles.
#![allow(dead_code)]

use std::time::Instant;

/// The kernel to run: the first command-line argument.
pub fn kernel() -> String {
    std::env::args().nth(1).expect("usage: <program> <kernel>")
}

pub fn now() -> Instant {
    Instant::now()
}

/// Prints the line the runner reads: the kernel's time since `start` and
/// its check value, then the peak resident set.
pub fn report(start: Instant, check: u64) {
    let ns = start.elapsed().as_nanos();
    println!("@@ ns={ns} check={check}");
    report_rss();
}

/// Prints the peak resident set (after a kernel that printed its own line).
pub fn report_rss() {
    println!("@@ {}", peak_rss());
}

pub fn unknown(kernel: &str) -> ! {
    eprintln!("unknown kernel `{kernel}`");
    std::process::exit(2)
}

fn peak_rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .map(|r| r.trim().trim_end_matches("kB").trim().to_string())
        })
        .map(|kb| format!("rss_peak_kb={kb}"))
        .unwrap_or_default()
}

/// The xorshift64 generator of the Codira programs (`next` there).
pub struct Rng(pub u64);
impl Rng {
    pub fn new() -> Self {
        Rng(0x9E3779B97F4A7C15)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// `@@ p50_ns=.. p95_ns=.. p99_ns=.. p999_ns=.. max_ns=..` of `lat`
/// (sorted here), with the same index rule as the Codira programs:
/// the element at `len * q / 1000` for q in thousandths.
pub fn report_latencies(lat: &mut [u64]) {
    lat.sort_unstable();
    let at = |q: usize| lat[(lat.len() * q / 1000).min(lat.len() - 1)];
    println!(
        "@@ p50_ns={} p95_ns={} p99_ns={} p999_ns={} max_ns={}",
        at(500),
        at(950),
        at(990),
        at(999),
        lat[lat.len() - 1]
    );
}
