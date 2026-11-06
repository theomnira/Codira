//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - `GcPolicy`: when the collector runs besides at the heap limit (pacing:
//!   collect once the heap has grown by a percentage over what the previous
//!   collection left live), and how long Codira code runs undisturbed
//!   between collections requested from outside (fairness).
use std::time::Duration;

/// When the collector runs besides at the heap limit, and how collections
/// requested from outside share time with Codira code.
///
/// # Pacing
///
/// With `growth_percent = Some(g)`, the heap asks for a collection once the
/// payload bytes of its objects (those the previous collection kept plus
/// everything allocated since) exceed `max(min_heap, live * (100 + g) /
/// 100)`, where `live` is what the previous collection kept. The next
/// allocation by a thread running Codira code then collects. So a program
/// with a small live set collects every `min_heap` bytes of allocation, and
/// one with a large live set lets the heap grow to about `(100 + g) / 100`
/// times that set -- the rule of Go's `GOGC`. The check happens where an
/// allocation takes the heap lock (refilling a thread's run of cells, a
/// large object, growing an array); the per-object fast path only reads one
/// flag.
///
/// Pacing counts payload bytes, without the 32-byte object headers and the
/// rounding to size classes; the heap limit counts committed memory, which
/// includes both. A heap limit stays the hard cap either way: reaching it
/// collects, whatever pacing says.
///
/// `growth_percent = None` turns pacing off: the collector then runs only
/// at the heap limit and when asked to, and without a limit a program that
/// keeps allocating grows until the system runs out of memory.
///
/// # Fairness
///
/// A collection stops every thread running Codira code. A host that asks
/// for collections back to back would leave those threads about one
/// allocation per collection. So a collection requested from outside
/// (`MarkSweep::collect_blocking`, `MarkSweep::request_collection`) first
/// waits until Codira code has run for at least
/// [`GcPolicy::mutator_window`] since the previous collection of any kind
/// ended: `min_mutator_window`, or `pause_multiple` times the previous
/// pause if that is longer. Under a host that collects in a loop, the world
/// is then stopped for at most `1 / (1 + pause_multiple)` of the time. The
/// wait is skipped when no other thread is inside Codira code, since there
/// is nobody to starve. Collections that allocations start (pacing, the
/// heap limit) do not wait: they are part of the allocating code's own
/// progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcPolicy {
    /// Collect once the heap has grown by this percentage over what the
    /// previous collection left live; `None` collects only at the heap
    /// limit and on request.
    pub growth_percent: Option<u32>,
    /// Pacing does not collect while the heap holds fewer payload bytes than
    /// this.
    pub min_heap: usize,
    /// The least time Codira code runs between a collection and the next one
    /// requested from outside.
    pub min_mutator_window: Duration,
    /// The window is at least this multiple of the previous pause.
    pub pause_multiple: u32,
}

/// cbindgen:ignore
impl GcPolicy {
    /// The default growth: the heap may double over its live set, as with
    /// Go's default `GOGC=100`.
    pub const DEFAULT_GROWTH_PERCENT: u32 = 100;
    /// The default floor of pacing: 4 MiB, Go's minimum heap goal. Below it
    /// a collection would cost more than the memory it could give back is
    /// worth.
    pub const DEFAULT_MIN_HEAP: usize = 4 << 20;
    /// The default least time Codira code runs between a collection and the
    /// next one requested from outside.
    pub const DEFAULT_MIN_MUTATOR_WINDOW: Duration = Duration::from_millis(1);
    /// By default, the window is at least four times the previous pause, so
    /// collections requested from outside stop the world for at most a fifth
    /// of the time.
    pub const DEFAULT_PAUSE_MULTIPLE: u32 = 4;

    /// The same policy with pacing off: collect only at the heap limit and
    /// on request.
    pub fn without_pacing(self) -> Self {
        Self {
            growth_percent: None,
            ..self
        }
    }

    /// The same policy with pacing at `percent` growth (see the type's
    /// documentation).
    pub fn with_growth_percent(self, percent: Option<u32>) -> Self {
        Self {
            growth_percent: percent,
            ..self
        }
    }

    /// How long Codira code runs at least between a collection whose pause
    /// was `last_pause` and the next collection requested from outside.
    pub fn mutator_window(&self, last_pause: Duration) -> Duration {
        self.min_mutator_window
            .max(last_pause.saturating_mul(self.pause_multiple))
    }
}

impl Default for GcPolicy {
    fn default() -> Self {
        Self {
            growth_percent: Some(Self::DEFAULT_GROWTH_PERCENT),
            min_heap: Self::DEFAULT_MIN_HEAP,
            min_mutator_window: Self::DEFAULT_MIN_MUTATOR_WINDOW,
            pause_multiple: Self::DEFAULT_PAUSE_MULTIPLE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_the_minimum_or_a_multiple_of_the_pause() {
        let policy = GcPolicy::default();
        assert_eq!(
            policy.mutator_window(Duration::from_micros(10)),
            Duration::from_millis(1)
        );
        assert_eq!(
            policy.mutator_window(Duration::from_millis(3)),
            Duration::from_millis(12)
        );
    }

    #[test]
    fn without_pacing_keeps_the_rest() {
        let policy = GcPolicy::default().without_pacing();
        assert_eq!(policy.growth_percent, None);
        assert_eq!(policy.min_heap, GcPolicy::DEFAULT_MIN_HEAP);
    }
}
