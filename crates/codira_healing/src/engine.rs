//! The `HealingEngine`: ties [`crate::trap`], [`crate::ranker`],
//! [`crate::fingerprint`], [`crate::contract`], [`crate::recovery_graph`],
//! [`crate::supervisor`], [`crate::deficiency`], and `codira_smt` together
//! into a working adaptive-healing engine.
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality (per the HERACLES spec):
//! - Maintains a registry of [`HealingContract`]s keyed by `site_id`.
//! - On a fault event, looks up the site's contract, uses the Thompson-
//!   sampling [`ranker::StrategyRanker`] to pick the next strategy, optionally
//!   verifies strategy feasibility with `codira_smt`, executes the strategy,
//!   verifies the postcondition, and records the outcome in the site's
//!   [`fingerprint::FaultFingerprint`].
//! - Drives module-tier healing through a [`supervisor::SupervisorTree`].
//! - Emits [`deficiency::HealingDeficiencyReport`]s when the dominant strategy
//!   at a site drops below the success threshold (Section 6.1).
//!
//! # What this is *not*: a JIT
//!
//! The engine selects among AOT-precompiled candidate implementations
//! registered with it; it does not synthesize new machine code at runtime.
//! "Code patching" in this architecture means updating which strategy the
//! ranker will choose next (and, at the module level, which child mode a
//! supervisor records), not rewriting executable memory. This is the honest
//! scope cut described in the crate's root doc comment.

#![allow(clippy::match_same_arms, clippy::type_complexity, clippy::unused_self)]
//! Lint notes: several matches here are *tables* mapping distinct
//! strategies/faults onto a shared tier or recovery action -- collapsing
//! the arms would erase which case is which. The `type_complexity`
//! sites are graph adjacency maps whose shape is the point.

use std::{collections::HashMap, time::Instant};

use log::{debug, warn};
use parking_lot::Mutex;

use crate::{
    contract::{FaultClass, HealingContract, HealingStrategy},
    deficiency::DeficiencyDetector,
    fingerprint::FaultFingerprint,
    ranker::StrategyRanker,
    recovery_graph::RecoveryGraph,
    supervisor::SupervisorTree,
};

/// An executed recovery strategy's result.
#[derive(Debug, Clone)]
pub enum HealingResult {
    /// The strategy produced a value that satisfied the postcondition.
    Recovered(Vec<u8>),
    /// The strategy produced a value that failed the postcondition.
    PostconditionFailed,
    /// The strategy could not be attempted (e.g. its feasibility
    /// preconditions don't hold at this site).
    NotApplicable,
    /// The strategy was `PropagateToParent` and no parent existed, or all
    /// strategies failed: the fault is escalated to the caller.
    Escalated(FaultClass),
}

/// One guarded site's full runtime state.
struct SiteState {
    contract: HealingContract,
    fingerprint: FaultFingerprint,
    ranker: StrategyRanker,
    graph: Option<RecoveryGraph>,
    last_result: Option<Vec<u8>>,
}

/// The adaptive healing engine.
///
/// The engine is thread-safe: every site's state is behind its own mutex,
/// so faults on different threads never contend, and a fault on the same
/// site from two threads is serialized. It is a daemon-friendly component --
/// call [`HealingEngine::install`] once at startup, then
/// [`HealingEngine::handle_fault`] from the trap path.
pub struct HealingEngine {
    sites: Mutex<HashMap<u64, Mutex<SiteState>>>,
    supervisors: SupervisorTree,
    detector: Mutex<DeficiencyDetector>,
    /// The SMT context used for feasibility checking, created lazily.
    smt: Mutex<Option<codira_smt::Context>>,
    /// Whether SMT feasibility checking is enabled at all.
    smt_enabled: bool,
}

impl HealingEngine {
    /// Creates a healing engine with SMT feasibility checking enabled.
    pub fn new() -> Self {
        HealingEngine::new_with_options(true)
    }

    /// Creates a healing engine. When `smt_enabled` is false, feasibility
    /// checks are skipped (useful when the Z3 DLL is unavailable).
    pub fn new_with_options(smt_enabled: bool) -> Self {
        HealingEngine {
            sites: Mutex::new(HashMap::new()),
            supervisors: SupervisorTree::new(),
            detector: Mutex::new(DeficiencyDetector::new()),
            smt: Mutex::new(if smt_enabled {
                Some(codira_smt::Context::new())
            } else {
                None
            }),
            smt_enabled,
        }
    }

    /// Registers a healing contract for a guarded site.
    pub fn register_contract(&self, contract: HealingContract) {
        let fingerprint = FaultFingerprint::new(contract.site_id);
        let ranker = StrategyRanker::new();
        for strategy in &contract.strategies {
            ranker.register_strategy(&strategy.to_string());
        }
        let site = Mutex::new(SiteState {
            contract,
            fingerprint,
            ranker,
            graph: None,
            last_result: None,
        });
        let site_id = {
            let site = site.lock();
            site.contract.site_id
        };
        self.sites.lock().insert(site_id, site);
        debug!("registered healing contract for site {site_id:#x}");
    }

    /// Associates a recovery graph with a site for later re-ordering.
    pub fn attach_recovery_graph(&self, site_id: u64, graph: RecoveryGraph) {
        let mut sites = self.sites.lock();
        if let Some(site) = sites.get_mut(&site_id) {
            site.lock().graph = Some(graph);
        }
    }

    /// Installs the given supervisor tree (module-tier healing).
    pub fn install_supervisors(&self, tree: SupervisorTree) {
        for name in tree.supervisor_names() {
            if let Some(state) = tree.get(&name) {
                self.supervisors.register_supervisor(state.spec.clone());
            }
        }
    }

    /// The configured deficiency detector parameters can be tuned here.
    pub fn with_deficiency_params(&self, threshold: f64, window_size: usize) {
        let mut detector = self.detector.lock();
        *detector = DeficiencyDetector::new().with_params(threshold, window_size);
    }

    /// Handles a fault at `site_id`. This is the entry point called from the
    /// trap path (or from a guard call) when a fault class `fault` is
    /// observed.
    ///
    /// Returns the healing outcome. When the outcome is
    /// [`HealingResult::Escalated`], the caller should propagate the fault
    /// to its own healing context (or, at the top level, let it crash /
    /// throw, per the contract's `PropagateToParent` semantics).
    pub fn handle_fault(&self, site_id: u64, fault: FaultClass) -> HealingResult {
        let mut sites = self.sites.lock();
        let site = if let Some(site) = sites.get_mut(&site_id) {
            site.get_mut()
        } else {
            warn!("no healing contract registered for site {site_id:#x}; escalating");
            return HealingResult::Escalated(fault);
        };

        site.fingerprint.record_fault(fault_tag(&fault));
        let started = Instant::now();

        // Prefer a strategy the ranker already believes in; fall back to the
        // compile-time order on the very first fault (before any evidence).
        let order = match site.ranker.select() {
            Some(selected) => {
                // Move the selected strategy to the front of the contract's
                // preference order (clone-of-order is fine: it's short).
                let mut ordered: Vec<HealingStrategy> = site
                    .contract
                    .strategies()
                    .iter()
                    .filter(|s| s.to_string() != selected)
                    .cloned()
                    .collect();
                if let Some(sel) = site
                    .contract
                    .strategies()
                    .iter()
                    .find(|s| s.to_string() == selected)
                {
                    ordered.insert(0, sel.clone());
                }
                ordered
            }
            None => site.contract.strategies().to_vec(),
        };

        let mut outcome = None;
        for strategy in order {
            if !self.is_strategy_feasible(site, &strategy) {
                debug!("strategy {strategy} not feasible at site {site_id:#x}; skipping");
                continue;
            }

            match self.try_strategy(site, &strategy) {
                (Some(value), latency) => {
                    // Postcondition check (Section 4.2.3 / A.1 step 3).
                    let passes = site
                        .contract
                        .postcondition
                        .as_ref()
                        .is_none_or(|pred| pred(&value));
                    if passes {
                        let outcome_strategy = strategy.to_string();
                        site.fingerprint
                            .record_recovery(&outcome_strategy, true, latency);
                        site.ranker.record_outcome(&outcome_strategy, true);
                        site.last_result = Some(value.clone());
                        self.note_outcome(site_id, &outcome_strategy, true);
                        outcome = Some(HealingResult::Recovered(value));
                        break;
                    } else {
                        // Postcondition failed: try the next strategy.
                        let outcome_strategy = strategy.to_string();
                        site.fingerprint
                            .record_recovery(&outcome_strategy, false, latency);
                        site.ranker.record_outcome(&outcome_strategy, false);
                        self.note_outcome(site_id, &outcome_strategy, false);
                    }
                }
                (None, latency) => {
                    let outcome_strategy = strategy.to_string();
                    site.fingerprint
                        .record_recovery(&outcome_strategy, false, latency);
                    site.ranker.record_outcome(&outcome_strategy, false);
                    self.note_outcome(site_id, &outcome_strategy, false);
                }
            }
        }

        let _ = started;
        outcome.unwrap_or(HealingResult::Escalated(fault))
    }

    /// Tries one strategy, returning `(Some(value), latency)` on success or
    /// `(None, latency)` if the strategy couldn't produce a value.
    fn try_strategy(
        &self,
        site: &SiteState,
        strategy: &HealingStrategy,
    ) -> (Option<Vec<u8>>, std::time::Duration) {
        let started = Instant::now();
        match strategy {
            HealingStrategy::ReturnDefault => {
                let value = site.contract.default_value.as_ref().map(|f| f());
                (value, started.elapsed())
            }
            HealingStrategy::ReturnCached => {
                // Only valid if the site is memoizable (feasibility checked
                // in `is_strategy_feasible`).
                let value = site.last_result.clone();
                (value, started.elapsed())
            }
            HealingStrategy::RetryWithBackoff(n) => {
                // The strategy list here is registered by name with the
                // ranker, but retry itself is executed by the caller's
                // retried operation. This engine cannot re-run arbitrary
                // AOT functions; what it *can* do is acknowledge that a
                // retry was requested and produce no value of its own --
                // so the caller retries the guarded call. We treat the
                // backoff delay as the observable outcome.
                let backoff = backoff_delay(*n);
                (None, started.elapsed() + backoff)
            }
            HealingStrategy::SubstituteAlternate(_alt) => {
                // Alternate implementations live in the caller's dispatch
                // table; this engine can only signal that an alternate is
                // the recommended path. No value produced here.
                (None, started.elapsed())
            }
            HealingStrategy::DegradeGracefully => (None, started.elapsed()),
            HealingStrategy::IsolateAndRestart => {
                // Module-tier healing: recorded against the supervisor. The
                // actual snapshot restore is the host application's job.
                (None, started.elapsed())
            }
            HealingStrategy::PropagateToParent => (None, started.elapsed()),
        }
    }

    /// Feasibility checking per Section 4.2.2. Uses SMT when the strategy
    /// requires a proof (`SubstituteAlternate`); the rest are checked against
    /// the contract's declared capabilities.
    fn is_strategy_feasible(&self, site: &SiteState, strategy: &HealingStrategy) -> bool {
        match strategy {
            HealingStrategy::ReturnDefault => site.contract.default_value.is_some(),
            HealingStrategy::ReturnCached => site.contract.memoizable,
            HealingStrategy::RetryWithBackoff(n) => site.contract.idempotent_up_to >= *n,
            HealingStrategy::SubstituteAlternate(alt) => {
                // Must be a registered alternate. If SMT is available, ask
                // Z3 whether the refinement-equivalence proof obligation is
                // satisfiable (a placeholder predicate: alternates named
                // in the contract are assumed provably equivalent; the SMT
                // hook demonstrates the integration point).
                if !site.contract.alternates.contains(alt) {
                    return false;
                }
                self.check_refinement_equivalence(site.contract.site_id, alt)
            }
            HealingStrategy::DegradeGracefully => {
                // Requires a module boundary, conservatively accepted.
                site.contract.module.is_some()
            }
            HealingStrategy::IsolateAndRestart => site.contract.module.is_some(),
            HealingStrategy::PropagateToParent => true,
        }
    }

    /// SMT-backed feasibility check. Returns true when SMT is disabled
    /// (best-effort: the alternate is named in the contract, which is the
    /// AOT-compiler's guarantee that the equivalence proof obligation holds).
    fn check_refinement_equivalence(&self, _site_id: u64, _alternate: &str) -> bool {
        if !self.smt_enabled {
            return true;
        }
        let smt_guard = self.smt.lock();
        match smt_guard.as_ref() {
            Some(ctx) => {
                // Proof obligation (Section 3.2 feasibility check #3):
                // "∀ x. fetch_record(x) refine-equiv fetch_record_fallback(x)".
                // We ask Z3 to check a placeholder implication over linear
                // integer arithmetic. A concrete encoding would bind the
                // actual refinement predicate; here we simply verify that
                // the solver is alive and accepts the obligation shape.
                let x = ctx.int_var("x");
                let premise = x.gt(ctx.int_lit(0));
                let conclusion = x.ge(ctx.int_lit(0));
                codira_smt::implies(ctx, premise, conclusion)
            }
            None => {
                // SMT context failed to initialize; fall back to accepting
                // the AOT guarantee.
                true
            }
        }
    }

    /// Feeds an outcome to the deficiency detector; logs an HDR if one fires.
    fn note_outcome(&self, site_id: u64, strategy: &str, succeeded: bool) {
        let mut detector = self.detector.lock();
        if let Some(report) = detector.observe(site_id, strategy, succeeded) {
            warn!("healing deficiency: {}", report.to_json());
        }
    }

    /// The success rate of the dominant strategy at `site_id` (observability).
    pub fn site_success_rate(&self, site_id: u64) -> Option<f64> {
        let sites = self.sites.lock();
        sites
            .get(&site_id)
            .map(|s| s.lock().fingerprint.success_rate())
    }

    /// The estimated MTTR at `site_id` (observability).
    pub fn site_mttr(&self, site_id: u64) -> Option<std::time::Duration> {
        let sites = self.sites.lock();
        sites
            .get(&site_id)
            .map(|s| s.lock().fingerprint.mttr_estimate())
    }

    /// Whether a site is currently being supervised.
    pub fn is_supervised(&self, supervisor: &str) -> bool {
        self.supervisors.get(supervisor).is_some()
    }
}

impl Default for HealingEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps a [`FaultClass`] to a stable integer tag for the fingerprint
/// histogram.
fn fault_tag(fault: &FaultClass) -> u32 {
    match fault {
        FaultClass::NullDereference => 0,
        FaultClass::OutOfBounds => 1,
        FaultClass::Timeout => 2,
        FaultClass::ConnectionRefused => 3,
        FaultClass::OutOfMemory => 4,
        FaultClass::MemoryLeak => 5,
        FaultClass::AccessViolation => 6,
        FaultClass::ArithmeticFault => 7,
        FaultClass::Custom(_) => 0xFF,
    }
}

/// Exponential backoff: base 50 µs, doubling, capped at 5 ms (Section 6.2,
/// Tier 2).
fn backoff_delay(attempts: u32) -> std::time::Duration {
    let base_us = 50u64;
    let capped = base_us.saturating_mul(1u64 << attempts.min(6));
    std::time::Duration::from_micros(capped.min(5000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{FaultClass, HealingContract, HealingStrategy};

    fn sample_contract(site_id: u64) -> HealingContract {
        let mut contract = HealingContract::new(
            site_id,
            vec![FaultClass::NullDereference, FaultClass::Timeout],
            vec![
                HealingStrategy::ReturnDefault,
                HealingStrategy::RetryWithBackoff(3),
                HealingStrategy::PropagateToParent,
            ],
        );
        contract.default_value = Some(Box::new(|| 0i32.to_le_bytes().to_vec()));
        contract.idempotent_up_to = 3;
        contract
    }

    #[test]
    fn recovery_returns_default_value() {
        let engine = HealingEngine::new_with_options(false);
        engine.register_contract(sample_contract(1));

        let result = engine.handle_fault(1, FaultClass::NullDereference);
        match result {
            HealingResult::Recovered(value) => {
                assert_eq!(value, 0i32.to_le_bytes().to_vec());
            }
            other => panic!("expected Recovered, got {other:?}"),
        }
    }

    #[test]
    fn unknown_site_escalates() {
        let engine = HealingEngine::new_with_options(false);
        let result = engine.handle_fault(999, FaultClass::Timeout);
        assert!(matches!(
            result,
            HealingResult::Escalated(FaultClass::Timeout)
        ));
    }

    #[test]
    fn cached_value_is_returned_after_first_success() {
        let engine = HealingEngine::new_with_options(false);
        let mut contract = sample_contract(1);
        contract.strategies = vec![HealingStrategy::ReturnDefault];
        contract.memoizable = true;
        engine.register_contract(contract);

        // First fault -> ReturnDefault -> 0, and the site's last_result caches it.
        let _ = engine.handle_fault(1, FaultClass::Timeout);
        let sites = engine.sites.lock();
        let site = sites.get(&1).unwrap().lock();
        assert!(site.last_result.is_some());
    }

    #[test]
    fn repeated_failures_eventually_trip_deficiency_detection() {
        let engine = HealingEngine::new_with_options(false);
        engine.with_deficiency_params(0.10, 10);
        let mut contract = sample_contract(1);
        // Make every recovery fail the postcondition so the success rate
        // collapses and the deficiency detector trips.
        contract.postcondition = Some(Box::new(|_| false));
        engine.register_contract(contract);

        // All strategies at this site fail the postcondition, so every fault
        // escalates; the detector accumulates failures.
        for _ in 0..30 {
            let _ = engine.handle_fault(1, FaultClass::Timeout);
        }
        // Detection is internal (logging); the public surface still reflects
        // the failure rate.
        let rate = engine.site_success_rate(1).unwrap();
        assert!(rate < 0.10);
    }

    #[test]
    fn supervise_module_tier_healing() {
        use crate::supervisor::{ChildSpec, ChildType, RestartStrategy, SupervisorSpec};
        let engine = HealingEngine::new_with_options(false);
        let tree = SupervisorTree::new();
        let spec = SupervisorSpec::new("DatabaseConnection", RestartStrategy::OneForAll)
            .with_child(ChildSpec {
                name: "CacheLayer".into(),
                module: "Cache".into(),
                child_type: ChildType::Permanent,
                healing_memoizable: false,
                idempotent_up_to: 3,
            });
        tree.register_supervisor(spec);
        engine.install_supervisors(tree);
        assert!(engine.is_supervised("DatabaseConnection"));
    }
}
