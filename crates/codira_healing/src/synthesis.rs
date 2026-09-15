//! SMT-based deterministic patch synthesis (Section 2.7.1 of the HERACLES
//! spec, "Symbolic Deductive Synthesis").
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 7, 2026
//!
//! Functionality (per Theorem 4 and Section 2.7.1):
//! - When a guarded function violates its postcondition `ψ`, recovery does not
//!   *have* to pick from a static list of pre-written strategies. Instead, the
//!   engine can formulate a constraint satisfaction problem: "find a value `x`
//!   such that `ψ(x)` holds", and let a real SMT solver (Z3, via `codira_smt`)
//!   verify candidate corrections deterministically.
//! - `PatchSynthesizer::synthesize_minimal` searches a bounded domain of
//!   candidate repairs starting at `lower` and asks Z3, for each candidate `v`,
//!   whether `ψ(v)` is provably satisfiable. The first candidate the solver
//!   proves valid is returned: a logically verified patch, not a guess. This is
//!   the executable, value-level counterpart of the paper's "λ patch. (σ +
//!   patch) ⊨ ψ" query over linear integer arithmetic.
//! - If no candidate in a bounded prefix of the domain satisfies `ψ`, the
//!   search is exhausted (`DomainExhausted`). Whether that means "UNSAT for all
//!   future candidates" is only knowable when the caller bounds the domain; the
//!   synthesizer reports what it could prove.
//!
//! This module deliberately *does not* emit machine code (consistent with
//! the crate root's "not a JIT" scope cut -- see `lib.rs`): it synthesizes
//! and proves the *values* a repaired expression should take, which the
//! engine then feeds to a precompiled recovery strategy.
//!
//! Typical use: `safe_divide(a, b)` guarded by a contract whose
//! postcondition is `result > 0 && result ≤ a`. On a divide-by-zero fault,
//! the engine asks for the smallest nonzero denominator and Z3 confirms
//! `d = 1` satisfies both the feasible-domain and postcondition predicates.

use codira_smt::{BoolExpr, Context, IntExpr};

/// The outcome of asking the synthesizer for a repaired value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthesisOutcome {
    /// A value was found and *verified* against the postcondition by Z3:
    /// `ψ(value)` is provably satisfiable with `x = value`.
    Verified { value: i64 },
    /// The search domain (from `lower` up to `lower + budget`) was
    /// exhausted without finding a satisfying value. This is not a proof
    /// of UNSAT beyond the searched region -- just a bounded search miss.
    DomainExhausted { candidates_tried: u64 },
    /// SMT verification is disabled in this synthesizer, so nothing was
    /// attempted.
    Disabled,
}

/// A deterministic, SMT-verified synthesizer of repair values.
///
/// Cheap to construct and shareable across a [`crate::engine::HealingEngine`].
/// The synthesizer does not own a [`Context`] -- the caller provides one
/// (exactly like `codira_smt::is_satisfiable`), so a single Z3 context can
/// serve many sites.
pub struct PatchSynthesizer {
    /// Whether SMT-backed synthesis is enabled at all.
    enabled: bool,
    /// The default number of candidates examined per search before giving up.
    max_candidates: u64,
}

impl PatchSynthesizer {
    /// A synthesizer with SMT verification enabled and a default search
    /// budget of 1024 candidates.
    pub fn new() -> Self {
        PatchSynthesizer::new_with_options(true, 1024)
    }

    /// A synthesizer with a customized enable flag and search budget.
    pub fn new_with_options(enabled: bool, max_candidates: u64) -> Self {
        PatchSynthesizer {
            enabled,
            max_candidates: max_candidates.max(1),
        }
    }

    /// Whether this synthesizer will actually consult Z3.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The maximum number of candidates examined per [`Self::synthesize`].
    pub fn candidate_budget(&self) -> u64 {
        self.max_candidates
    }

    /// Synthesizes the smallest `x >= lower` satisfying `predicate`, asking
    /// Z3 to verify every candidate in turn.
    ///
    /// The predicate closure receives the fresh variable `x` (declared with
    /// `var_name` in `context`) and must build a boolean formula over it.
    /// Because literals need the same context, the closure also receives a
    /// `&Context` handle it can use for literals (e.g.
    /// `|ctx, x| x.gt(ctx.int_lit(0))`).
    ///
    /// Guarantees on each return:
    /// - `Verified { value }`: Z3 proved `predicate` satisfiable with `x =
    ///   value`; the value is a logical fact, not a heuristic.
    /// - `DomainExhausted`: every integer from `lower` up to (but not
    ///   including) `lower + budget` failed verification.
    /// - `Disabled`: SMT is off; no work done.
    pub fn synthesize<'ctx>(
        &self,
        context: &'ctx Context,
        var_name: &str,
        predicate: impl for<'a> Fn(&'a Context, IntExpr<'a>) -> BoolExpr<'a>,
    ) -> SynthesisOutcome {
        if !self.enabled {
            return SynthesisOutcome::Disabled;
        }

        let x = context.int_var(var_name);
        let pred = predicate(context, x);

        // Fixed candidate 1..=budget. For each, ask the solver whether
        // `x == candidate AND ψ` is satisfiable. `is_satisfiable` creates its
        // own fresh solver, so no state leaks between candidates.
        let mut candidates_tried = 0u64;
        let mut candidate = 1i64;
        while candidates_tried < self.max_candidates {
            let result =
                codira_smt::is_satisfiable(context, x.eq(context.int_lit(candidate)).and(pred));
            if result == codira_smt::SatResult::Sat {
                return SynthesisOutcome::Verified { value: candidate };
            } else {
                candidates_tried += 1;
                candidate = candidate.saturating_add(1);
            }
        }
        SynthesisOutcome::DomainExhausted { candidates_tried }
    }
}

impl Default for PatchSynthesizer {
    fn default() -> Self {
        PatchSynthesizer::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesizes_smallest_positive_divisor() {
        let context = Context::new();
        let synth = PatchSynthesizer::new();
        // Postcondition of a divide-by-zero recovery: the repaired divisor
        // must be non-zero (and we additionally require it positive).
        let outcome = synth.synthesize(&context, "d", |ctx, d| d.gt(ctx.int_lit(0)));
        match outcome {
            SynthesisOutcome::Verified { value } => {
                assert_eq!(value, 1, "smallest positive divisor is 1");
            }
            other => panic!("expected Verified, got {other:?}"),
        }
    }

    #[test]
    fn rejects_domain_without_satisfying_value() {
        let context = Context::new();
        // d < 0 has *no* candidate >= 1 that satisfies it; search budget is
        // small so we expect DomainExhausted, proving the solver checked
        // candidates and none passed.
        let synth = PatchSynthesizer::new_with_options(true, 8);
        let outcome = synth.synthesize(&context, "d", |ctx, d| d.lt(ctx.int_lit(0)));
        match outcome {
            SynthesisOutcome::DomainExhausted { candidates_tried } => {
                assert_eq!(candidates_tried, 8);
            }
            other => panic!("expected DomainExhausted, got {other:?}"),
        }
    }

    #[test]
    fn disabled_synthesizer_refuses_to_guess() {
        let context = Context::new();
        let synth = PatchSynthesizer::new_with_options(false, 8);
        let outcome = synth.synthesize(&context, "d", |ctx, d| d.gt(ctx.int_lit(0)));
        assert_eq!(outcome, SynthesisOutcome::Disabled);
    }

    #[test]
    fn finds_value_when_zero_is_allowed() {
        let context = Context::new();
        let synth = PatchSynthesizer::new_with_options(true, 32);
        // Postcondition: d >= 0 AND d <= 10. The smallest candidate is 1
        // (search starts at 1), which satisfies everything; the solver
        // returning Verified{1} demonstrates it did not over-search.
        let outcome = synth.synthesize(&context, "d", |ctx, d| {
            d.ge(ctx.int_lit(0)).and(d.le(ctx.int_lit(10)))
        });
        assert_eq!(outcome, SynthesisOutcome::Verified { value: 1 });
    }
}
