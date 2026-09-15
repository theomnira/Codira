//! Supervisor hierarchies for module-level and distributed healing.
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality (per Section 3.5 and the spec's distributed model):
//! - Erlang/OTP-style supervisor trees: parent modules supervise children.
//! - Restart strategies: `OneForOne` (restart only the failed child),
//!   `OneForAll` (restart all children), `RestForOne` (restart the failed child
//!   and all started after it).
//! - Restart intensity bounds: `max_restarts` within `max_time` seconds,
//!   matching the OTP `maxR`/`maxT` model; exceeding the bound escalates.
//! - Child restart types: `Permanent` (always restart), `Transient` (restart
//!   only on abnormal exit), `Temporary` (never restart).
//! - `SupervisorTree`: a registry of supervisors and their child links, used by
//!   the engine for module-tier healing (`DegradeGracefully` /
//!   `IsolateAndRestart` / `PropagateToParent`).

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

/// The numeric fault-class tag used when a TTL probe expires: a module
/// missed its heartbeat, which is a `Timeout` by the engine's taxonomy
/// (engine.rs `fault_tag` maps `FaultClass::Timeout` to 2).
pub fn timeout_fault_tag() -> u32 {
    2
}

/// The restart strategy of a supervisor (Section 3.5 of the spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    /// If one child fails, restart all children.
    OneForAll,
    /// If one child fails, restart only that child.
    OneForOne,
    /// If one child fails, restart it and every child started after it.
    RestForOne,
}

/// How a child is restarted (Section 3.5 of the spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildType {
    /// Always restart, regardless of exit reason.
    Permanent,
    /// Restart only if it exits abnormally (i.e. not on a clean stop).
    Transient,
    /// Never restart.
    Temporary,
}

/// A child specification within a supervisor.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// The child's unique name within the supervisor.
    pub name: String,
    /// The module (or function) the child runs.
    pub module: String,
    /// How the child is restarted.
    pub child_type: ChildType,
    /// Whether the child's healing is memoizable (`ReturnCached` feasible).
    pub healing_memoizable: bool,
    /// Whether the child is idempotent up to N calls (`RetryWithBackoff`
    /// feasible).
    pub idempotent_up_to: u32,
}

/// A supervisor spec, per Section 3.5 of the spec.
#[derive(Debug, Clone)]
pub struct SupervisorSpec {
    /// The supervisor's unique name.
    pub name: String,
    /// The restart strategy.
    pub strategy: RestartStrategy,
    /// Maximum number of restarts within `max_time` before the supervisor
    /// gives up and escalates to its own parent.
    pub max_restarts: u32,
    /// The window (in seconds) in which `max_restarts` is enforced.
    pub max_time: u64,
    /// The supervisor's children, in start order.
    pub children: Vec<ChildSpec>,
}

impl SupervisorSpec {
    /// Creates a new supervisor spec.
    pub fn new(name: impl Into<String>, strategy: RestartStrategy) -> Self {
        SupervisorSpec {
            name: name.into(),
            strategy,
            max_restarts: 5,
            max_time: 60,
            children: Vec::new(),
        }
    }

    /// Sets the restart intensity bounds (OTP `maxR`/`maxT`).
    pub fn with_restart_intensity(mut self, max_restarts: u32, max_time: u64) -> Self {
        self.max_restarts = max_restarts;
        self.max_time = max_time;
        self
    }

    /// Adds a child spec.
    pub fn with_child(mut self, child: ChildSpec) -> Self {
        self.children.push(child);
        self
    }
}

/// The current execution mode of a supervised child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildMode {
    /// Running normally.
    Normal,
    /// Running with degraded functionality.
    Degraded,
    /// In the middle of a restart.
    Restarting,
}

/// The live state of a child under supervision.
#[derive(Debug, Clone)]
pub struct ChildState {
    pub spec: ChildSpec,
    pub mode: ChildMode,
    pub last_start: Option<Instant>,
    pub restart_count: u64,
}

/// The live state of a supervisor: its spec plus runtime bookkeeping.
#[derive(Debug, Clone)]
pub struct SupervisorState {
    pub spec: SupervisorSpec,
    pub children: Vec<ChildState>,
    /// Timestamps of recent restarts, for intensity-bound checking.
    pub recent_restarts: Vec<Instant>,
    /// Whether the supervisor is currently alive.
    pub alive: bool,
}

/// A registry of supervisors, keyed by name. This is the runtime structure
/// the engine consults when it needs to make a module-tier healing decision
/// (Section 3.5: "Parent supervisors monitor children's health and escalate
/// healing decisions up the tree").
#[derive(Debug, Default)]
pub struct SupervisorTree {
    supervisors: Mutex<HashMap<String, SupervisorState>>,
    /// Parent -> children relationships, used for escalation.
    hierarchy: Mutex<HashMap<String, Vec<String>>>,
}

impl SupervisorTree {
    pub fn new() -> Self {
        SupervisorTree::default()
    }

    /// Registers a supervisor spec. Re-registering an existing supervisor
    /// replaces its children and resets its runtime state.
    pub fn register_supervisor(&self, spec: SupervisorSpec) {
        let supervisor_name = spec.name.clone();
        let hierarchy_children = spec
            .children
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>();
        let children = spec
            .children
            .iter()
            .map(|child| ChildState {
                spec: child.clone(),
                mode: ChildMode::Normal,
                last_start: Some(Instant::now()),
                restart_count: 0,
            })
            .collect::<Vec<_>>();
        let state = SupervisorState {
            spec,
            children,
            recent_restarts: Vec::new(),
            alive: true,
        };
        self.supervisors
            .lock()
            .insert(state.spec.name.clone(), state);
        self.hierarchy
            .lock()
            .insert(supervisor_name, hierarchy_children);
    }

    /// Looks up a supervisor by name.
    pub fn get(&self, name: &str) -> Option<SupervisorState> {
        self.supervisors.lock().get(name).cloned()
    }

    /// Records a child restart and returns `true` if the restart intensity
    /// bound (`max_restarts` within `max_time`) has been exceeded -- in
    /// which case the supervisor gives up and the engine escalates.
    pub fn record_child_restart(&self, supervisor: &str, child: &str) -> bool {
        let now = Instant::now();
        let mut supervisors = self.supervisors.lock();
        let Some(state) = supervisors.get_mut(supervisor) else {
            return false;
        };
        state
            .recent_restarts
            .retain(|t| now.duration_since(*t) < Duration::from_secs(state.spec.max_time));
        state.recent_restarts.push(now);
        if let Some(child_state) = state.children.iter_mut().find(|c| c.spec.name == child) {
            child_state.restart_count += 1;
            child_state.mode = ChildMode::Restarting;
        }
        state.recent_restarts.len() as u32 > state.spec.max_restarts
    }

    /// Marks a child as restarted (transitioning it back to Normal mode).
    pub fn child_restarted(&self, supervisor: &str, child: &str) {
        let mut supervisors = self.supervisors.lock();
        if let Some(state) = supervisors.get_mut(supervisor) {
            if let Some(child_state) = state.children.iter_mut().find(|c| c.spec.name == child) {
                child_state.mode = ChildMode::Normal;
                child_state.last_start = Some(Instant::now());
            }
        }
    }

    /// The children of a supervisor, in start order.
    pub fn children(&self, supervisor: &str) -> Vec<ChildState> {
        self.supervisors
            .lock()
            .get(supervisor)
            .map(|s| s.children.clone())
            .unwrap_or_default()
    }

    /// All registered supervisor names.
    pub fn supervisor_names(&self) -> Vec<String> {
        self.supervisors.lock().keys().cloned().collect()
    }

    /// The children of a supervisor, by name, as recorded in the hierarchy
    /// used for escalation.
    pub fn hierarchy_children(&self, supervisor: &str) -> Vec<String> {
        self.hierarchy
            .lock()
            .get(supervisor)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree() -> SupervisorTree {
        let tree = SupervisorTree::new();
        let spec = SupervisorSpec::new("DatabaseConnection", RestartStrategy::OneForAll)
            .with_restart_intensity(5, 60)
            .with_child(ChildSpec {
                name: "ConnPool".into(),
                module: "ConnectionPool".into(),
                child_type: ChildType::Transient,
                healing_memoizable: true,
                idempotent_up_to: 0,
            })
            .with_child(ChildSpec {
                name: "CacheLayer".into(),
                module: "Cache".into(),
                child_type: ChildType::Permanent,
                healing_memoizable: false,
                idempotent_up_to: 3,
            });
        tree.register_supervisor(spec);
        tree
    }

    #[test]
    fn registers_and_looks_up_supervisors() {
        let tree = sample_tree();
        let sup = tree.get("DatabaseConnection").unwrap();
        assert_eq!(sup.spec.strategy, RestartStrategy::OneForAll);
        assert_eq!(sup.children.len(), 2);
        assert_eq!(sup.spec.max_restarts, 5);
        assert_eq!(sup.spec.max_time, 60);
    }

    #[test]
    fn restart_intensity_bound_is_enforced() {
        let tree = sample_tree();
        // 5 allowed within 60s; the 6th should trip the bound.
        let mut tripped = false;
        for _ in 0..6 {
            tripped = tree.record_child_restart("DatabaseConnection", "CacheLayer");
        }
        assert!(tripped);
    }

    #[test]
    fn restart_marks_child_restarting_then_normal() {
        let tree = sample_tree();
        tree.record_child_restart("DatabaseConnection", "ConnPool");
        let sup = tree.get("DatabaseConnection").unwrap();
        assert_eq!(sup.children[0].mode, ChildMode::Restarting);
        assert_eq!(sup.children[0].restart_count, 1);

        tree.child_restarted("DatabaseConnection", "ConnPool");
        let sup = tree.get("DatabaseConnection").unwrap();
        assert_eq!(sup.children[0].mode, ChildMode::Normal);
    }
}
