//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! Adaptive self-healing runtime for Codira.
//!
//! Implements the pieces of `spec/self_healing_programming_language.md`
//! that are genuinely achievable as real, working systems code (see each
//! module's own doc comment for what it does and, just as importantly,
//! what it deliberately does not attempt):
//!
//! - [`trap`]: hardware-fault (access violation / SIGSEGV, floating-point
//!   exception / SIGFPE) interception and routing to a Rust handler, running on
//!   this process's real OS-level exception mechanism.
//! - [`ranker`]: Thompson-sampling ranking of healing strategies.
//! - [`engine`]: ties the above together with `codira_smt` (feasibility
//!   checking) and `codira_runtime`'s dispatch table (the same mechanism
//!   hot-reloading already uses) to apply a chosen strategy by swapping which
//!   precompiled implementation a faulting function calls.
//! - [`collector`]: the Section 5.1 fault-event collector -- ingests
//!   hardware-trap, guard-call, and TTL-probe signals, runs the Section 5.2
//!   memory-leak monotonicity test, and hands events to the engine.
//! - [`synthesis`]: Section 2.7.1 (Theorem 4), SMT-based patch synthesis.
//! - [`reversible`]: Section 2.7.2 (Theorem 5), the reversible-computation
//!   block with write-ahead journals and checkpoint rollback.
//! - [`algebraic_repair`]: Section 2.7.3 (Theorem 6), algebraic reboot of a BST
//!   data structure via rebalance.
//! - [`hardware`]: Section 2.7.4 (Theorem 7), hardware-telemetry-aware
//!   allocation (faulty cores, bad DRAM rows).
//! - [`patcher`]: Section 5.4, atomic strategy-slot patching (the non-JIT
//!   analogue of the spec's nop->call rewrite).
//!
//! What this is *not*: a JIT in the strict sense of generating new machine
//! code at runtime. The engine selects among AOT-precompiled candidate
//! implementations rather than synthesizing new code on the fly -- see
//! `engine`'s doc comment for the honest reasoning behind that scope cut.

pub mod algebraic_repair;
pub mod collector;
pub mod contract;
pub mod deficiency;
pub mod engine;
pub mod fingerprint;
pub mod hardware;
pub mod patcher;
pub mod ranker;
pub mod recovery_graph;
pub mod reversible;
pub mod supervisor;
pub mod synthesis;
pub mod trap;
