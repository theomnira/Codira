//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! Bayesian/Thompson-sampling ranking of healing strategies.
//!
//! Each registered healing strategy for a given fault site is an "arm" in a
//! multi-armed bandit: its true success probability is unknown, and every
//! time it's tried we learn a little more about it (it worked, or it
//! didn't). Thompson sampling picks the next arm to try by maintaining a
//! Beta(alpha, beta) posterior over each arm's success probability,
//! drawing one random sample from each arm's posterior, and picking
//! whichever arm's sample came out highest -- naturally balancing
//! exploring strategies we're still uncertain about against exploiting the
//! one that's worked best so far, without any tuning parameters.
//!
//! This is a real, general-purpose implementation (not a toy): the beta
//! sampling itself is delegated to `rand_distr`, a maintained statistics
//! crate, rather than a hand-rolled (and much easier to get subtly wrong)
//! gamma/beta sampler.

use std::collections::HashMap;

use parking_lot::Mutex;
use rand::Rng;
use rand_distr::{Beta, Distribution};

/// The observed (successes, failures) for one healing strategy, and the
/// Beta(1, 1) (uniform) prior they start from.
#[derive(Debug, Clone, Copy)]
struct ArmStats {
    successes: f64,
    failures: f64,
}

impl ArmStats {
    fn new() -> Self {
        // Beta(1, 1) is the uniform distribution on [0, 1]: no assumption
        // about the strategy's success rate before it's ever been tried.
        ArmStats {
            successes: 1.0,
            failures: 1.0,
        }
    }

    fn sample(&self, rng: &mut impl Rng) -> f64 {
        Beta::new(self.successes, self.failures)
            .expect("alpha/beta are always > 0 by construction")
            .sample(rng)
    }

    fn mean(&self) -> f64 {
        self.successes / (self.successes + self.failures)
    }
}

/// Ranks a fixed set of named healing strategies for a single fault site
/// using Thompson sampling.
///
/// A `StrategyRanker` is created once per distinct fault site (e.g. keyed
/// by the function + fault kind that can trigger a healing decision) and
/// accumulates evidence across every time healing is attempted there.
pub struct StrategyRanker {
    arms: Mutex<HashMap<String, ArmStats>>,
}

impl StrategyRanker {
    /// Creates a ranker with no strategies registered yet.
    pub fn new() -> Self {
        StrategyRanker {
            arms: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a strategy by name, if it isn't already known. Idempotent:
    /// calling this again for an already-registered strategy does not
    /// reset its accumulated statistics.
    pub fn register_strategy(&self, name: &str) {
        let mut arms = self.arms.lock();
        arms.entry(name.to_owned()).or_insert_with(ArmStats::new);
    }

    /// Picks the strategy Thompson sampling currently favors: draws one
    /// random sample from every registered strategy's success-probability
    /// posterior and returns the name of whichever sampled highest.
    /// Returns `None` if no strategy has been registered.
    pub fn select(&self) -> Option<String> {
        self.select_with_rng(&mut rand::thread_rng())
    }

    fn select_with_rng(&self, rng: &mut impl Rng) -> Option<String> {
        let arms = self.arms.lock();
        arms.iter()
            .map(|(name, stats)| (name.clone(), stats.sample(rng)))
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(name, _)| name)
    }

    /// Records the outcome of trying `strategy`, updating its posterior.
    pub fn record_outcome(&self, strategy: &str, succeeded: bool) {
        let mut arms = self.arms.lock();
        let stats = arms
            .entry(strategy.to_owned())
            .or_insert_with(ArmStats::new);
        if succeeded {
            stats.successes += 1.0;
        } else {
            stats.failures += 1.0;
        }
    }

    /// The current posterior mean success rate for `strategy`, if it's
    /// been registered. Useful for observability/logging, not for
    /// selection itself (which samples rather than using the mean
    /// directly, so it keeps exploring under-tried strategies).
    pub fn mean_success_rate(&self, strategy: &str) -> Option<f64> {
        self.arms.lock().get(strategy).map(ArmStats::mean)
    }
}

impl Default for StrategyRanker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use rand::{rngs::StdRng, SeedableRng};

    use super::*;

    #[test]
    fn selects_among_registered_strategies() {
        let ranker = StrategyRanker::new();
        ranker.register_strategy("retry");
        ranker.register_strategy("fallback");

        let selected = ranker.select().unwrap();
        assert!(selected == "retry" || selected == "fallback");
    }

    #[test]
    fn empty_ranker_selects_nothing() {
        let ranker = StrategyRanker::new();
        assert_eq!(ranker.select(), None);
    }

    #[test]
    fn outcomes_move_the_posterior_mean() {
        let ranker = StrategyRanker::new();
        ranker.register_strategy("a");

        let before = ranker.mean_success_rate("a").unwrap();
        assert_eq!(before, 0.5); // Beta(1, 1) mean

        for _ in 0..20 {
            ranker.record_outcome("a", true);
        }
        let after = ranker.mean_success_rate("a").unwrap();
        assert!(
            after > before,
            "20 successes should raise the mean above 0.5, got {after}"
        );
    }

    /// The real statistical property Thompson sampling is supposed to
    /// have: given enough trials, it converges to preferring the
    /// genuinely-better arm, without ever being told which one that is
    /// up front. Uses a seeded RNG so the test is deterministic.
    #[test]
    fn converges_to_the_better_arm() {
        let ranker = StrategyRanker::new();
        ranker.register_strategy("good"); // true success rate 0.9
        ranker.register_strategy("bad"); // true success rate 0.1

        let mut rng = StdRng::seed_from_u64(42);
        let mut selection_rng = StdRng::seed_from_u64(1234);

        let mut good_selected_late = 0u32;
        let trials = 2000;
        for i in 0..trials {
            let choice = ranker.select_with_rng(&mut selection_rng).unwrap();
            let true_rate = if choice == "good" { 0.9 } else { 0.1 };
            let succeeded: f64 = rng.gen();
            ranker.record_outcome(&choice, succeeded < true_rate);

            // Count selections in the last 10% of trials, once the
            // posterior has had time to concentrate.
            if i >= trials - trials / 10 && choice == "good" {
                good_selected_late += 1;
            }
        }

        // Not a tight bound (Thompson sampling still explores "bad"
        // sometimes by design), but converging correctly should heavily
        // favor "good" by the end.
        assert!(
            good_selected_late > (trials / 10) * 8 / 10,
            "expected the better arm to dominate late selections, got {good_selected_late}/{}",
            trials / 10
        );

        let good_mean = ranker.mean_success_rate("good").unwrap();
        let bad_mean = ranker.mean_success_rate("bad").unwrap();
        assert!(
            good_mean > bad_mean,
            "posterior mean should reflect the true difference: good={good_mean}, bad={bad_mean}"
        );
    }
}
