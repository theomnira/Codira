//! Healing Contracts (HC): type-checked declarations binding fault classes,
//! ordered recovery strategies, and postconditions to a guarded declaration.
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Defines the fault classes a guarded site may encounter (`FaultClass`).
//! - Defines the ordered recovery strategies from Table 1 of the HERACLES spec:
//!   `ReturnDefault`, `ReturnCached`, `RetryWithBackoff`,
//!   `SubstituteAlternate`, `DegradeGracefully`, `IsolateAndRestart`, and
//!   `PropagateToParent`.
//! - Defines the four healing tiers with their latency budgets (Section 6.2).
//! - Provides the `HealingContract` type binding (F, S, ψ): the set of fault
//!   classes, the ordered strategy list, and the postcondition predicate.
//!
//! This module is the AOT-visible representation of a contract: the compiler
//! resolves these from source annotations and the engine consumes them at
//! runtime. What it deliberately does *not* do is execute anything -- the
//! execution semantics live in [`crate::engine`].

#![allow(clippy::match_same_arms, clippy::type_complexity, clippy::unused_self)]
//! Lint notes: several matches here are *tables* mapping distinct
//! strategies/faults onto a shared tier or recovery action -- collapsing
//! the arms would erase which case is which. The `type_complexity`
//! sites are graph adjacency maps whose shape is the point.

use std::fmt;

/// The tier of healing, from the latency budget in Section 6.2 of the spec.
///
/// Tiers are ordered by latency budget and by the scope of what they can
/// recover. The engine uses the tier to decide how aggressively a strategy
/// may run (e.g. an expression-tier strategy must never stall the caller).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HealingTier {
    /// Expression-level healing: target latency < 1 µs, transparent to the
    /// caller. Scope is a single expression or basic block.
    Expression,
    /// Function-level healing: target latency < 10 ms. Scope is a complete
    /// function call plus retry logic.
    Function,
    /// Module-level healing: target latency < 500 ms. Scope is module
    /// restart and state restoration from a snapshot.
    Module,
    /// Incremental recompilation feedback: offline / asynchronous. Emitted
    /// as a Healing Deficiency Report for offline AOT recompilation.
    Recompilation,
}

impl HealingTier {
    /// The maximum latency budget for this tier, as a `Duration`.
    pub fn latency_budget(&self) -> std::time::Duration {
        match self {
            HealingTier::Expression => std::time::Duration::from_micros(1),
            HealingTier::Function => std::time::Duration::from_millis(10),
            HealingTier::Module => std::time::Duration::from_millis(500),
            HealingTier::Recompilation => std::time::Duration::MAX,
        }
    }
}

impl fmt::Display for HealingTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            HealingTier::Expression => "expression",
            HealingTier::Function => "function",
            HealingTier::Module => "module",
            HealingTier::Recompilation => "recompilation",
        };
        f.write_str(s)
    }
}

/// A class of fault that can occur at a guarded site.
///
/// This is the disjoint union of system faults (NULL, OOM, TIMEOUT, ...)
/// from the formal definition of a Healing Contract in Section 3.2 of the
/// spec. The variants are deliberately few and coarse: they map to
/// observably-distinct failure modes the engine can route to different
/// recovery strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultClass {
    /// A null (or otherwise invalid) pointer was dereferenced.
    NullDereference,
    /// An index or offset was out of bounds.
    OutOfBounds,
    /// An operation took longer than its deadline.
    Timeout,
    /// A connection was refused by a remote peer.
    ConnectionRefused,
    /// Memory could not be allocated (out of memory).
    OutOfMemory,
    /// A memory leak was detected via the monotonicity test.
    MemoryLeak,
    /// A hardware trap: access violation / SIGSEGV.
    AccessViolation,
    /// A hardware trap: integer or float division by zero / SIGFPE.
    ArithmeticFault,
    /// An arbitrary, user-defined fault identified by its string name.
    Custom(&'static str),
}

impl FaultClass {
    /// All "built-in" fault classes, in the order the spec's Table 1
    /// implies for strategy applicability. Used by the compiler when a
    /// contract omits an explicit `on` clause and a conservative fault
    /// domain must be assumed.
    pub const ALL: &'static [FaultClass] = &[
        FaultClass::NullDereference,
        FaultClass::OutOfBounds,
        FaultClass::Timeout,
        FaultClass::ConnectionRefused,
        FaultClass::OutOfMemory,
        FaultClass::MemoryLeak,
        FaultClass::AccessViolation,
        FaultClass::ArithmeticFault,
    ];
}

impl fmt::Display for FaultClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FaultClass::NullDereference => "NullDereference",
            FaultClass::OutOfBounds => "OutOfBounds",
            FaultClass::Timeout => "Timeout",
            FaultClass::ConnectionRefused => "ConnectionRefused",
            FaultClass::OutOfMemory => "OutOfMemory",
            FaultClass::MemoryLeak => "MemoryLeak",
            FaultClass::AccessViolation => "AccessViolation",
            FaultClass::ArithmeticFault => "ArithmeticFault",
            FaultClass::Custom(name) => name,
        };
        f.write_str(s)
    }
}

/// A type-safe recovery strategy for a healing contract (Table 1 of the
/// spec). Each strategy carries the tier it operates in and the static /
/// dependent type-safety guarantee it provides.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HealingStrategy {
    /// Return the zero value of the return type. Tier: Expression.
    /// Type safety: Static.
    ReturnDefault,
    /// Return the last successful return value cached at this call site.
    /// Tier: Expression. Type safety: Dependent (requires `@memoizable`).
    ReturnCached,
    /// Retry the function up to `n` times with exponential backoff.
    /// Tier: Function. Type safety: Static (requires `@idempotent_up_to_n`).
    RetryWithBackoff(u32),
    /// Call an alternate function proven refinement-equivalent to the
    /// original. Tier: Function. Type safety: Dependent (SMT-checked).
    SubstituteAlternate(&'static str),
    /// Disable the feature and continue with reduced functionality.
    /// Tier: Module. Type safety: Static.
    DegradeGracefully,
    /// Restart the module, restoring from its last committed snapshot.
    /// Tier: Module. Type safety: Static (requires `@snapshot`).
    IsolateAndRestart,
    /// Escalate to the enclosing healing contract (or throw an exception
    /// if there is no parent). Tier: Any. Type safety: Static.
    PropagateToParent,
}

impl HealingStrategy {
    /// The tier this strategy executes in.
    pub fn tier(&self) -> HealingTier {
        match self {
            HealingStrategy::ReturnDefault | HealingStrategy::ReturnCached => {
                HealingTier::Expression
            }
            HealingStrategy::RetryWithBackoff(_) | HealingStrategy::SubstituteAlternate(_) => {
                HealingTier::Function
            }
            HealingStrategy::DegradeGracefully | HealingStrategy::IsolateAndRestart => {
                HealingTier::Module
            }
            HealingStrategy::PropagateToParent => HealingTier::Expression,
        }
    }
}

impl fmt::Display for HealingStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HealingStrategy::ReturnDefault => f.write_str("ReturnDefault"),
            HealingStrategy::ReturnCached => f.write_str("ReturnCached"),
            HealingStrategy::RetryWithBackoff(n) => {
                write!(f, "RetryWithBackoff({n})")
            }
            HealingStrategy::SubstituteAlternate(alt) => write!(f, "SubstituteAlternate({alt})"),
            HealingStrategy::DegradeGracefully => f.write_str("DegradeGracefully"),
            HealingStrategy::IsolateAndRestart => f.write_str("IsolateAndRestart"),
            HealingStrategy::PropagateToParent => f.write_str("PropagateToParent"),
        }
    }
}

/// A postcondition: a refinement predicate over the function's return type.
///
/// The spec defines ψ : τ₂ → Bool such that ∀ x. ψ(f(x)). This is enforced
/// at runtime by the engine *before* a recovered result is returned to the
/// caller; if ψ(r) is false, the next strategy in the contract's list is
/// attempted. Postconditions are provided as closures so the engine can
/// check arbitrary values without knowing the concrete return type.
pub type Postcondition = Box<dyn Fn(&[u8]) -> bool + Send + Sync>;

/// A Healing Contract (HC) for a guarded function, per Section 3.2.
///
/// Formally: HC = (F, S, ψ) where
///   - F is the finite set of fault classes,
///   - S is the ordered list of recovery strategies s₁, s₂, ..., sₖ,
///   - ψ is the postcondition such that ∀ x. ψ(f(x)).
///
/// The engine stores contracts keyed by `site_id` (see
/// [`crate::engine::HealingEngine`]). The strategy list is ordered by
/// preference at compile time; the engine's Bayesian ranker re-orders it at
/// runtime (Section 5.3).
pub struct HealingContract {
    /// A stable identifier for the guarded site (function + fault domain).
    pub site_id: u64,
    /// The fault classes this site may encounter. If empty, ALL is used.
    pub fault_classes: Vec<FaultClass>,
    /// The ordered recovery strategies, best (cheapest / most likely to
    /// succeed) first. At least one must be present.
    pub strategies: Vec<HealingStrategy>,
    /// The postcondition ψ: recovered results must satisfy this predicate
    /// before being returned to the caller. If `None`, the postcondition
    /// is trivially true (no check performed).
    pub postcondition: Option<Postcondition>,
    /// The default value factory, used by `ReturnDefault`. The closure
    /// must produce a value valid for the guarded function's return type.
    pub default_value: Option<Box<dyn Fn() -> Vec<u8> + Send + Sync>>,
    /// Names of alternate implementations for `SubstituteAlternate`,
    /// keyed by the alternate's registered name. The engine looks these up
    /// in its dispatch table at runtime.
    pub alternates: Vec<&'static str>,
    /// Whether this site may cache its last successful result
    /// (`ReturnCached` feasibility requires `@memoizable`).
    pub memoizable: bool,
    /// Whether this site is safe to call multiple times
    /// (`RetryWithBackoff` feasibility requires `@idempotent_up_to_n_calls`).
    pub idempotent_up_to: u32,
    /// The module boundary this contract belongs to, if any, used for
    /// module-tier healing.
    pub module: Option<String>,
}

impl HealingContract {
    /// Creates a new healing contract with the given site id, fault classes
    /// and strategy list.
    pub fn new(
        site_id: u64,
        fault_classes: Vec<FaultClass>,
        strategies: Vec<HealingStrategy>,
    ) -> Self {
        assert!(
            !strategies.is_empty(),
            "a healing contract requires at least one recovery strategy"
        );
        HealingContract {
            site_id,
            fault_classes,
            strategies,
            postcondition: None,
            default_value: None,
            alternates: Vec::new(),
            memoizable: false,
            idempotent_up_to: 0,
            module: None,
        }
    }

    /// Whether this contract guards against the given fault class. An
    /// empty fault-class set means "guards against everything".
    pub fn guards(&self, fault: &FaultClass) -> bool {
        self.fault_classes.is_empty() || self.fault_classes.contains(fault)
    }

    /// The strategies, in the engine's *current* preference order.
    ///
    /// Initially this is the compile-time order. The engine re-orders this
    /// as its Bayesian ranker learns which strategies actually succeed at
    /// this site (Section 5.3).
    pub fn strategies(&self) -> &[HealingStrategy] {
        &self.strategies
    }

    /// Adds a postcondition predicate.
    pub fn with_postcondition(mut self, predicate: Postcondition) -> Self {
        self.postcondition = Some(predicate);
        self
    }

    /// Adds a default value factory for the `ReturnDefault` strategy.
    pub fn with_default(mut self, default: Box<dyn Fn() -> Vec<u8> + Send + Sync>) -> Self {
        self.default_value = Some(default);
        self
    }
}

impl fmt::Display for HealingContract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HC(site={:#x}, faults=[", self.site_id)?;
        for (i, fault) in self.fault_classes.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{fault}")?;
        }
        write!(f, "], strategies=[")?;
        for (i, strategy) in self.strategies.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{strategy}")?;
        }
        write!(f, "])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_requires_at_least_one_strategy() {
        let result =
            std::panic::catch_unwind(|| HealingContract::new(1, vec![FaultClass::Timeout], vec![]));
        assert!(result.is_err());
    }

    #[test]
    fn empty_fault_set_guards_everything() {
        let contract = HealingContract::new(1, vec![], vec![HealingStrategy::ReturnDefault]);
        assert!(contract.guards(&FaultClass::NullDereference));
        assert!(contract.guards(&FaultClass::Timeout));
    }

    #[test]
    fn fault_guarding_is_explicit() {
        let contract = HealingContract::new(
            1,
            vec![FaultClass::Timeout],
            vec![HealingStrategy::ReturnDefault],
        );
        assert!(contract.guards(&FaultClass::Timeout));
        assert!(!contract.guards(&FaultClass::NullDereference));
    }

    #[test]
    fn tier_latency_budgets_are_monotonic() {
        assert!(HealingTier::Expression.latency_budget() < HealingTier::Function.latency_budget());
        assert!(HealingTier::Function.latency_budget() < HealingTier::Module.latency_budget());
    }

    #[test]
    fn strategy_tiers_match_spec_table_1() {
        assert_eq!(
            HealingStrategy::ReturnDefault.tier(),
            HealingTier::Expression
        );
        assert_eq!(
            HealingStrategy::ReturnCached.tier(),
            HealingTier::Expression
        );
        assert_eq!(
            HealingStrategy::RetryWithBackoff(3).tier(),
            HealingTier::Function
        );
        assert_eq!(
            HealingStrategy::SubstituteAlternate("f2").tier(),
            HealingTier::Function
        );
        assert_eq!(
            HealingStrategy::DegradeGracefully.tier(),
            HealingTier::Module
        );
        assert_eq!(
            HealingStrategy::IsolateAndRestart.tier(),
            HealingTier::Module
        );
    }
}
