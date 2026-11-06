//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Runtime metrics of the collector: heap size now and at its peak,
//!   allocation totals, collections by cause, what they reclaimed, time spent
//!   with the world stopped and the distribution of pauses (a log-linear
//!   histogram, so recording costs the same whatever the program's length).
//! - The report `codira run --runtime-stats` prints.
use std::{fmt, time::Duration};

/// Why a collection ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Pacing: the heap grew by the policy's percentage over what the
    /// previous collection left live (see [`crate::gc::GcPolicy`]).
    Pacing,
    /// An allocation would have exceeded the heap limit.
    HeapLimit,
    /// `MarkSweep::request_collection`.
    Requested,
    /// `MarkSweep::collect_blocking`.
    Explicit,
}

/// Collections counted by cause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CollectionCounts {
    /// Started by pacing.
    pub paced: u64,
    /// Started by an allocation at the heap limit.
    pub at_limit: u64,
    /// Asked for with `request_collection`.
    pub requested: u64,
    /// Asked for with `collect_blocking`.
    pub explicit: u64,
}

impl CollectionCounts {
    /// All collections.
    pub fn total(&self) -> u64 {
        self.paced + self.at_limit + self.requested + self.explicit
    }

    pub(crate) fn count(&mut self, trigger: Trigger) {
        match trigger {
            Trigger::Pacing => self.paced += 1,
            Trigger::HeapLimit => self.at_limit += 1,
            Trigger::Requested => self.requested += 1,
            Trigger::Explicit => self.explicit += 1,
        }
    }
}

/// The distribution of pauses. A pause is the time from the collector
/// asking every thread running Codira code to stop until it lets them go
/// again: waiting for them to reach a safepoint, marking, sweeping, and (at
/// the heap limit) retrying the allocation that collected.
///
/// The percentiles come from a histogram with 16 buckets per power of two:
/// each is the upper end of its bucket, at most 6.25% above the true value
/// (and never above `max`, which is exact).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PauseSummary {
    /// The number of pauses.
    pub count: u64,
    /// All pauses together.
    pub total: Duration,
    /// The median pause.
    pub p50: Duration,
    /// The 99th percentile.
    pub p99: Duration,
    /// The longest pause.
    pub max: Duration,
}

/// What a collection, or the collection a request was folded into, did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CollectionReport {
    /// Objects freed.
    pub reclaimed_objects: usize,
    /// Payload bytes of the objects freed (headers and size-class rounding
    /// not included).
    pub reclaimed_bytes: usize,
    /// How long the world was stopped (see [`PauseSummary`]).
    pub pause: Duration,
    /// The request was satisfied by a collection that started after it was
    /// made (by another thread, or by an allocation), so no collection of
    /// its own ran; the other fields describe that collection.
    pub coalesced: bool,
}

impl CollectionReport {
    /// Returns true if the collection freed at least one object.
    pub fn reclaimed_anything(&self) -> bool {
        self.reclaimed_objects > 0
    }
}

/// Statistics of a garbage-collected heap, from `MarkSweep::heap_stats`.
///
/// Byte counts named "payload" exclude the 32-byte header of every object
/// and the rounding up to size classes; `heap_bytes` and `peak_heap_bytes`
/// are what the heap holds from the system and include both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Bytes the heap holds from the system now: size-class blocks (cached
    /// empty ones included), large objects and out-of-line payloads.
    pub heap_bytes: usize,
    /// The most `heap_bytes` has been.
    pub peak_heap_bytes: usize,
    /// The heap limit, if any.
    pub heap_limit: Option<usize>,
    /// Payload bytes of the objects the last collection kept plus everything
    /// allocated since.
    pub live_bytes: usize,
    /// Payload bytes the last collection kept.
    pub live_bytes_after_last_collection: usize,
    /// The `live_bytes` above which pacing asks for the next collection;
    /// `None` when pacing is off.
    pub next_paced_collection_at: Option<usize>,
    /// Payload bytes allocated, ever (a growing array counts each new
    /// buffer).
    pub bytes_allocated: u64,
    /// Objects allocated, ever.
    pub objects_allocated: u64,
    /// Collections, by cause.
    pub collections: CollectionCounts,
    /// Payload bytes the collections freed.
    pub bytes_reclaimed: u64,
    /// Objects the collections freed.
    pub objects_reclaimed: u64,
    /// Time with the world stopped for collections: the sum of the pauses.
    pub collection_time: Duration,
    /// Time from asking the threads to stop until all had stopped, summed
    /// over the collections (part of `collection_time`).
    pub time_to_safepoint: Duration,
    /// The distribution of pauses.
    pub pauses: PauseSummary,
}

/// Formats a byte count with a binary unit: `512 B`, `1.5 KiB`, `4.0 GiB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Formats a duration for the report: `850 ns`, `12.3 us`, `4.56 ms`,
/// `1.20 s`.
pub fn format_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    let three = |value: f64, unit: &str| {
        if value >= 100.0 {
            format!("{value:.0} {unit}")
        } else if value >= 10.0 {
            format!("{value:.1} {unit}")
        } else {
            format!("{value:.2} {unit}")
        }
    };
    if nanos < 1_000 {
        format!("{nanos} ns")
    } else if nanos < 1_000_000 {
        three(nanos as f64 / 1e3, "us")
    } else if nanos < 1_000_000_000 {
        three(nanos as f64 / 1e6, "ms")
    } else {
        three(nanos as f64 / 1e9, "s")
    }
}

/// Formats a count with thousands separators: `25,000,123`.
fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

impl fmt::Display for GcStats {
    /// The report `codira run --runtime-stats` prints, one quantity per
    /// line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = |n: usize| format_bytes(n as u64);
        let limit = self.heap_limit.map_or_else(|| "none".to_string(), bytes);
        writeln!(
            f,
            "heap         {} now, {} peak, limit {limit}",
            bytes(self.heap_bytes),
            bytes(self.peak_heap_bytes),
        )?;
        let next = self.next_paced_collection_at.map_or_else(
            || "pacing off".to_string(),
            |at| format!("next paced collection above {}", bytes(at)),
        );
        writeln!(
            f,
            "live         {} after the last collection, {} now ({next})",
            bytes(self.live_bytes_after_last_collection),
            bytes(self.live_bytes),
        )?;
        writeln!(
            f,
            "allocated    {} in {} objects",
            format_bytes(self.bytes_allocated),
            format_count(self.objects_allocated),
        )?;
        writeln!(
            f,
            "reclaimed    {} in {} objects",
            format_bytes(self.bytes_reclaimed),
            format_count(self.objects_reclaimed),
        )?;
        let counts = &self.collections;
        writeln!(
            f,
            "collections  {}: {} paced, {} at the heap limit, {} requested, {} explicit",
            counts.total(),
            counts.paced,
            counts.at_limit,
            counts.requested,
            counts.explicit,
        )?;
        writeln!(
            f,
            "gc time      {} with the world stopped ({} of it waiting for threads to stop)",
            format_duration(self.collection_time),
            format_duration(self.time_to_safepoint),
        )?;
        if self.pauses.count == 0 {
            write!(f, "pauses       none")
        } else {
            write!(
                f,
                "pauses       p50 {}, p99 {}, max {}",
                format_duration(self.pauses.p50),
                format_duration(self.pauses.p99),
                format_duration(self.pauses.max),
            )
        }
    }
}

/// Sub-buckets per power of two, as a number of bits.
const SUB_BITS: u32 = 4;
/// Sub-buckets per power of two.
const SUB: usize = 1 << SUB_BITS;
/// Buckets for every `u64` nanosecond count.
const BUCKETS: usize = (64 - SUB_BITS as usize + 1) * SUB;

/// A log-linear histogram of durations in nanoseconds: exact below 16 ns,
/// then 16 buckets per power of two.
#[derive(Clone)]
pub(crate) struct PauseHistogram {
    buckets: Vec<u64>,
    count: u64,
    total: Duration,
    max: Duration,
}

impl Default for PauseHistogram {
    fn default() -> Self {
        Self {
            buckets: vec![0; BUCKETS],
            count: 0,
            total: Duration::ZERO,
            max: Duration::ZERO,
        }
    }
}

fn bucket_of(nanos: u64) -> usize {
    if nanos < SUB as u64 {
        nanos as usize
    } else {
        let msb = 63 - nanos.leading_zeros();
        let shift = msb - SUB_BITS;
        let sub = ((nanos >> shift) as usize) & (SUB - 1);
        (shift as usize + 1) * SUB + sub
    }
}

/// The largest value in `bucket`.
fn bucket_upper(bucket: usize) -> u64 {
    if bucket < SUB {
        bucket as u64
    } else {
        let shift = (bucket / SUB - 1) as u32;
        let lower = ((SUB + bucket % SUB) as u64) << shift;
        lower + ((1u64 << shift) - 1)
    }
}

impl PauseHistogram {
    pub(crate) fn record(&mut self, pause: Duration) {
        let nanos = u64::try_from(pause.as_nanos()).unwrap_or(u64::MAX);
        self.buckets[bucket_of(nanos)] += 1;
        self.count += 1;
        self.total = self.total.saturating_add(pause);
        self.max = self.max.max(pause);
    }

    /// The `q` quantile (`0 < q <= 1`): the upper end of the bucket holding
    /// the pause of rank `ceil(q * count)`, capped at the maximum.
    pub(crate) fn quantile(&self, q: f64) -> Duration {
        if self.count == 0 {
            return Duration::ZERO;
        }
        let rank = ((q * self.count as f64).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (bucket, &n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return Duration::from_nanos(bucket_upper(bucket)).min(self.max);
            }
        }
        self.max
    }

    pub(crate) fn summary(&self) -> PauseSummary {
        PauseSummary {
            count: self.count,
            total: self.total,
            p50: self.quantile(0.50),
            p99: self.quantile(0.99),
            max: self.max,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_cover_every_value_in_order() {
        let mut previous = 0;
        for nanos in (0..100_000u64).chain([u64::MAX / 2, u64::MAX]) {
            let bucket = bucket_of(nanos);
            assert!(bucket < BUCKETS);
            assert!(bucket >= previous || nanos > 100_000);
            assert!(bucket_upper(bucket) >= nanos, "{nanos} above its bucket");
            previous = bucket;
        }
        assert_eq!(bucket_upper(bucket_of(u64::MAX)), u64::MAX);
    }

    #[test]
    fn a_bucket_is_at_most_a_sixteenth_wide() {
        for nanos in [16u64, 17, 100, 1_000, 123_456, 9_999_999_999] {
            let upper = bucket_upper(bucket_of(nanos));
            assert!(
                (upper - nanos) as f64 <= nanos as f64 / 16.0,
                "{nanos}: {upper}"
            );
        }
    }

    #[test]
    fn quantiles_of_a_known_distribution() {
        let mut histogram = PauseHistogram::default();
        // 1..=1000 microseconds, once each.
        for micros in 1..=1000u64 {
            histogram.record(Duration::from_micros(micros));
        }
        let summary = histogram.summary();
        assert_eq!(summary.count, 1000);
        assert_eq!(summary.max, Duration::from_micros(1000));
        let near = |got: Duration, want_us: f64| {
            let got = got.as_nanos() as f64 / 1e3;
            assert!(
                got >= want_us && got <= want_us * 1.0625,
                "{got} us, expected about {want_us}"
            );
        };
        near(summary.p50, 500.0);
        near(summary.p99, 990.0);
        assert_eq!(summary.total, Duration::from_micros(500_500));
    }

    #[test]
    fn an_empty_histogram_reports_zero() {
        let summary = PauseHistogram::default().summary();
        assert_eq!(summary, PauseSummary::default());
    }

    #[test]
    fn byte_and_duration_formats() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.50 KiB");
        assert_eq!(format_bytes(512 << 20), "512 MiB");
        assert_eq!(format_bytes(15 << 30), "15.0 GiB");
        assert_eq!(format_duration(Duration::from_nanos(850)), "850 ns");
        assert_eq!(format_duration(Duration::from_micros(1234)), "1.23 ms");
        assert_eq!(format_count(25_000_123), "25,000,123");
        assert_eq!(format_count(999), "999");
    }

    #[test]
    fn the_report_names_every_quantity() {
        let stats = GcStats {
            heap_bytes: 3 << 20,
            peak_heap_bytes: 5 << 20,
            heap_limit: Some(1 << 30),
            collections: CollectionCounts {
                paced: 2,
                at_limit: 1,
                requested: 0,
                explicit: 0,
            },
            ..GcStats::default()
        };
        let report = stats.to_string();
        for needle in [
            "heap         3.00 MiB now, 5.00 MiB peak, limit 1.00 GiB",
            "collections  3: 2 paced, 1 at the heap limit",
            "pauses       none",
        ] {
            assert!(report.contains(needle), "{needle:?} not in\n{report}");
        }
    }
}
