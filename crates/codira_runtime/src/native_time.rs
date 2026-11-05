//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - Clock services the runtime provides to Codira programs through
//!   `extern "C"` declarations (`std/time/time.code`): the wall clock, a
//!   monotonic clock in nanoseconds, and sleeping.
//!
//! Each is the Rust standard library's (`SystemTime`, `Instant`,
//! `thread::sleep`), which is the platform's clock: `clock_gettime` on
//! Linux and the BSDs, `mach_absolute_time`/`clock_gettime` on macOS,
//! `QueryPerformanceCounter` and `GetSystemTimePreciseAsFileTime` on
//! Windows. Nothing here panics: these are called from generated code,
//! where unwinding is not defined.

use std::{
    sync::OnceLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{FunctionDefinition, IntoFunctionDefinition};

/// Seconds since the Unix epoch (1970-01-01 00:00:00 UTC) by the wall
/// clock, with its sub-second part. Negative for a clock set before 1970.
/// The wall clock can jump (it is set by the user and by NTP); measure
/// intervals with [`codira_time_monotonic_ns`].
pub extern "C" fn codira_time_now() -> f64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs_f64(),
        Err(before) => -before.duration().as_secs_f64(),
    }
}

/// Nanoseconds by a monotonic clock, counted from the first call in this
/// process (so the first call returns about 0). Never decreases; only
/// differences mean anything. Saturates at `u64::MAX` (after 584 years).
pub extern "C" fn codira_time_monotonic_ns() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = *ORIGIN.get_or_init(Instant::now);
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Blocks the calling thread for at least `nanos` nanoseconds (longer when
/// the scheduler says so). `0` returns at once. A sleeping thread is parked
/// for the collector: it does not hold up a collection.
pub extern "C" fn codira_time_sleep_ns(nanos: u64) {
    if nanos > 0 {
        codira_memory::gc::safepoint::blocking(|| std::thread::sleep(Duration::from_nanos(nanos)));
    }
}

/// Returns the definitions of every function in this module, for the
/// runtime's dispatch table.
pub(crate) fn functions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(codira_time_now as extern "C" fn() -> f64, "codira_time_now"),
        IntoFunctionDefinition::into(
            codira_time_monotonic_ns as extern "C" fn() -> u64,
            "codira_time_monotonic_ns",
        ),
        IntoFunctionDefinition::into(
            codira_time_sleep_ns as extern "C" fn(u64),
            "codira_time_sleep_ns",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wall_clock_is_after_2020() {
        // 2020-01-01 00:00:00 UTC.
        assert!(codira_time_now() > 1_577_836_800.0);
    }

    #[test]
    fn the_monotonic_clock_does_not_go_back_and_sleep_waits() {
        let a = codira_time_monotonic_ns();
        codira_time_sleep_ns(2_000_000);
        let b = codira_time_monotonic_ns();
        assert!(b >= a + 2_000_000, "{a} {b}");
        codira_time_sleep_ns(0);
    }
}
