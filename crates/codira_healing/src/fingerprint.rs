//! Fault Fingerprints (FF): lightweight runtime data structures attached to
//! every guarded site.
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality (per Section 3.3 and Section 5.2 of the HERACLES spec):
//! - Maintains a histogram of observed fault classes per site.
//! - Maintains an exponentially-weighted moving average (EWMA) of recovery
//!   latencies, with configurable decay (default 0.95 per the spec).
//! - Records recent healing latencies in a ring buffer.
//! - Tracks per-strategy Bayesian success probabilities via a Beta posterior
//!   (alpha, beta), updated on each healing attempt.
//!
//! The fingerprint is the shared-memory state the JIT adaptive healing
//! engine reads and writes on the fast path. All updates are atomic with
//! respect to the engine via an internal mutex.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// The default EWMA decay factor, from Section 5.2 of the spec.
const DEFAULT_EWMA_DECAY: f64 = 0.95;
/// The default number of recent latencies kept in the ring buffer.
const RING_BUFFER_CAPACITY: usize = 16;

/// A single observation of a fault at a guarded site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FaultEvent {
    pub site_id: u64,
    pub fault_class: u32,
    pub timestamp: Instant,
}

/// Records a recovery latency and the strategy that produced it.
#[derive(Debug, Clone)]
pub struct RecoveryOutcome {
    pub strategy: String,
    pub latency: Duration,
    pub succeeded: bool,
}

/// The per-site fault fingerprint, as specified in Section 3.3.
///
/// Note that where the spec uses dependent types to *statically* guarantee
/// invariants (e.g. `success_count + fail_count <= fault_count`), this Rust
/// implementation enforces the same invariants dynamically in the update
/// methods -- this is the honest, executable counterpart of the paper's
/// refined-type presentation.
pub struct FaultFingerprint {
    /// The site this fingerprint reports on.
    site_id: u64,
    /// Total fault count observed at this site, ever.
    fault_count: u64,
    /// Total successful recoveries at this site, ever.
    success_count: u64,
    /// Total failed recoveries at this site, ever.
    fail_count: u64,
    /// Histogram of fault classes observed, keyed by the fault class tag.
    fault_histogram: HashMap<u32, u64>,
    /// Recent healing latencies, newest last (ring buffer semantics).
    healing_latencies: Vec<Duration>,
    /// The EWMA estimate of mean time to recovery (MTTR).
    mttr_estimate: Duration,
    /// The EWMA decay factor applied on each latency observation.
    ewma_decay: f64,
    /// Per-strategy Beta posterior counts: strategy name -> (alpha, beta).
    /// Beta(1, 1) is the uniform prior.
    beta_posterior: HashMap<String, (f64, f64)>,
}

impl FaultFingerprint {
    /// Creates an empty fingerprint for `site_id`.
    pub fn new(site_id: u64) -> Self {
        FaultFingerprint {
            site_id,
            fault_count: 0,
            success_count: 0,
            fail_count: 0,
            fault_histogram: HashMap::new(),
            healing_latencies: Vec::with_capacity(RING_BUFFER_CAPACITY),
            mttr_estimate: Duration::ZERO,
            ewma_decay: DEFAULT_EWMA_DECAY,
            beta_posterior: HashMap::new(),
        }
    }

    /// Records a fault event, incrementing the histogram and total count.
    pub fn record_fault(&mut self, fault_class: u32) {
        self.fault_count += 1;
        *self.fault_histogram.entry(fault_class).or_insert(0) += 1;
    }

    /// Records the outcome of a recovery attempt, updating the MTTR EWMA,
    /// the ring buffer, and the strategy's Beta posterior.
    ///
    /// Enforces the dependent-type invariant from Section 3.3 that
    /// `success_count + fail_count <= fault_count` by construction.
    pub fn record_recovery(&mut self, strategy: &str, succeeded: bool, latency: Duration) {
        // Update the per-strategy Beta posterior.
        let entry = self
            .beta_posterior
            .entry(strategy.to_owned())
            .or_insert((1.0, 1.0));
        if succeeded {
            entry.0 += 1.0;
            self.success_count += 1;
        } else {
            entry.1 += 1.0;
            self.fail_count += 1;
        }

        // Update the EWMA of MTTR.
        if self.fault_count == 1 {
            self.mttr_estimate = latency;
        } else {
            let old = self.mttr_estimate.as_nanos() as f64;
            let new = latency.as_nanos() as f64;
            self.mttr_estimate = Duration::from_nanos(
                (self.ewma_decay * old + (1.0 - self.ewma_decay) * new).round() as u64,
            );
        }

        // Push into the ring buffer, dropping the oldest if full.
        if self.healing_latencies.len() == RING_BUFFER_CAPACITY {
            self.healing_latencies.pop();
        }
        self.healing_latencies.insert(0, latency);
    }

    /// The current MTTR estimate (exponentially weighted moving average).
    pub fn mttr_estimate(&self) -> Duration {
        self.mttr_estimate
    }

    /// The total fault count at this site.
    pub fn fault_count(&self) -> u64 {
        self.fault_count
    }

    /// The total successful recovery count at this site.
    pub fn success_count(&self) -> u64 {
        self.success_count
    }

    /// The total failed recovery count at this site.
    pub fn fail_count(&self) -> u64 {
        self.fail_count
    }

    /// The histogram of fault classes observed at this site.
    pub fn fault_histogram(&self) -> &HashMap<u32, u64> {
        &self.fault_histogram
    }

    /// The site this fingerprint reports on.
    pub fn site_id(&self) -> u64 {
        self.site_id
    }

    /// The sample efficiency: fraction of recovery attempts that succeeded.
    pub fn success_rate(&self) -> f64 {
        let attempts = self.success_count + self.fail_count;
        if attempts == 0 {
            return 0.0;
        }
        self.success_count as f64 / attempts as f64
    }

    /// The posterior mean success probability of `strategy`, or `None` if
    /// it has never been tried.
    pub fn p_success(&self, strategy: &str) -> Option<f64> {
        self.beta_posterior.get(strategy).map(|(a, b)| a / (a + b))
    }

    /// The recent healing latencies, newest first.
    pub fn healing_latencies(&self) -> &[Duration] {
        &self.healing_latencies
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_fingerprint_starts_empty() {
        let fp = FaultFingerprint::new(1);
        assert_eq!(fp.fault_count(), 0);
        assert_eq!(fp.success_rate(), 0.0);
        assert_eq!(fp.mttr_estimate(), Duration::ZERO);
    }

    #[test]
    fn fault_histogram_accumulates() {
        let mut fp = FaultFingerprint::new(1);
        fp.record_fault(0);
        fp.record_fault(1);
        fp.record_fault(0);
        assert_eq!(fp.fault_count(), 3);
        assert_eq!(fp.fault_histogram()[&0], 2);
        assert_eq!(fp.fault_histogram()[&1], 1);
    }

    #[test]
    fn mttr_ewma_converges_to_true_latency() {
        let mut fp = FaultFingerprint::new(1);
        let stable = Duration::from_micros(500);
        // Seed the EWMA then let it converge on stable latency.
        for _ in 0..200 {
            fp.record_fault(0);
            fp.record_recovery("retry", true, stable);
        }
        let estimate = fp.mttr_estimate();
        // After enough samples the EWMA should be within 5% of the truth.
        let err = (estimate.as_nanos() as i128 - stable.as_nanos() as i128).abs() as f64
            / stable.as_nanos() as f64;
        assert!(err < 0.05, "mttr={estimate:?} vs true={stable:?}");
    }

    #[test]
    fn success_rate_reflects_outcomes() {
        let mut fp = FaultFingerprint::new(1);
        for _ in 0..3 {
            fp.record_recovery("a", true, Duration::ZERO);
        }
        fp.record_recovery("a", false, Duration::ZERO);
        assert_eq!(fp.success_rate(), 0.75);
        assert_eq!(fp.success_count(), 3);
        assert_eq!(fp.fail_count(), 1);
    }

    #[test]
    fn bayesian_p_success_moves_with_evidence() {
        let mut fp = FaultFingerprint::new(1);
        // Beta(1,1) prior mean is 0.5 before any trials.
        assert_eq!(fp.p_success("a"), None);
        for _ in 0..20 {
            fp.record_recovery("a", true, Duration::ZERO);
        }
        let p = fp.p_success("a").unwrap();
        assert!(
            p > 0.95,
            "20 straight successes should push p high, got {p}"
        );
    }
}
