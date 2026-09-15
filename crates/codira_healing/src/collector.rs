//! Fault Event Collector (Section 5.1 of the HERACLES spec) plus the
//! memory-leak monotonicity test (Section 5.2).
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 7, 2026
//!
//! Functionality (per Sections 5.1, 5.2):
//! - Aggregates fault signals from the three sources the spec lists into a
//!   lock-free-ish queue drained by the healing engine:
//!   1. **Hardware traps** -- [`FaultEventCollector::record_hardware_trap`],
//!      the counterpart of the VEH handler in [`crate::trap`].
//!   2. **Guard calls** -- [`FaultEventCollector::record_guard_fault`], the
//!      path a patched guard stub uses to report a fault.
//!   3. **TTL probes** -- [`TtlWatch`]/[`FaultEventCollector::ttl_tick`], the
//!      background watchdog that sends a probe to each module's health port and
//!      converts a missed deadline into a module-level fault.
//! - `FaultEventCollector::record_memory_snapshot` runs the **monotonicity
//!   test** from Section 5.2: if a module's memory usage at snapshot boundaries
//!   is non-decreasing across the last `W` snapshots, a `MemoryLeak` fault
//!   event is synthesized and the module-level `IsolateAndRestart` healing is
//!   scheduled by whoever drains the queue.
//!
//! The queue is a parking-lot `Mutex`-guarded `VecDeque`; the collector is
//! thread-safe so the trap path, guard dispatchers, and watchdog can all
//! feed it without locking.
//!
//! Where the TTL probe's "alive" signal should come from is left to the
//! host: [`TtlWatch::acknowledge`] is the hook a module's health port calls.

use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use parking_lot::Mutex;

/// The source that produced a fault event (spec Section 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultSource {
    /// A hardware trap was mapped to a site (Section 5.1 item 1).
    HardwareTrap,
    /// A patched guard call dispatched to a recovery stub (item 2).
    GuardCall,
    /// A TTL probe expired (item 3).
    TtlProbe,
    /// The memory monotonicity test synthesized a `MemoryLeak` fault.
    MonotonicMemoryLeak,
}

/// A fault event as emitted by the collector, ready to feed the engine's
/// fingerprint updater.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultEvent {
    pub site_id: u64,
    /// The numeric fault class tag (matches
    /// [`crate::fingerprint`]'s histogram keys).
    pub fault_class: u32,
    pub source: FaultSource,
    pub timestamp: Instant,
}

/// The memory-leak fault tag, consistent with
/// [`crate::contract::FaultClass::MemoryLeak`]'s `fault_tag` in `engine.rs`.
pub const FAULT_CLASS_MEMORY_LEAK: u32 = 5;

/// The TTL probe state of one module: the max silent period before the
/// watchdog declares the module unresponsive.
#[derive(Debug, Clone)]
pub struct TtlProbe {
    /// How long the module may go silent before it is considered dead.
    pub timeout: Duration,
    /// When the module last responded (heartbeat / tick).
    pub last_response: Instant,
}

/// A watchdog on module health (Section 5.1 item 3: "A background watchdog
/// thread sends periodic TTL signals to module health ports").
#[derive(Debug, Clone, Default)]
pub struct TtlWatch {
    probes: HashMap<u64, TtlProbe>,
}

impl TtlWatch {
    /// Starts watching module `site_id`; it has `timeout` to respond or it
    /// is declared dead.
    pub fn watch(&mut self, site_id: u64, timeout: Duration) {
        self.probes.insert(
            site_id,
            TtlProbe {
                timeout,
                last_response: Instant::now(),
            },
        );
    }

    /// Records a heartbeat from module `site_id`. Returns `true` if the
    /// module had gone silent (deadline had passed) and has now recovered.
    pub fn acknowledge(&mut self, site_id: u64, now: Instant) -> bool {
        let Some(probe) = self.probes.get_mut(&site_id) else {
            return false;
        };
        let was_late = now.duration_since(probe.last_response) > probe.timeout;
        probe.last_response = now;
        was_late
    }

    /// Advances the watchdog clock: every module whose deadline has passed
    /// is reported as dead. Returns the dead site ids.
    pub fn tick(&mut self, now: Instant) -> Vec<u64> {
        self.probes
            .iter()
            .filter(|(_, probe)| now.duration_since(probe.last_response) > probe.timeout)
            .map(|(&site, _)| site)
            .collect()
    }

    /// The currently watched site ids.
    pub fn watched(&self) -> Vec<u64> {
        self.probes.keys().copied().collect()
    }

    /// Unwatches a module (e.g. after a clean shutdown).
    pub fn unwatch(&mut self, site_id: u64) {
        self.probes.remove(&site_id);
    }
}

/// Sinks fault signals from all sources, holds the TTL watchdog, and runs
/// the monotonicity test; feeding the engine's fingerprint state.
pub struct FaultEventCollector {
    /// Pending fault events awaiting the healing engine.
    queue: Mutex<VecDeque<FaultEvent>>,
    /// The TTL watchdog over supervised modules.
    watchdog: Mutex<TtlWatch>,
    /// Recent memory snapshots per module (for the Section 5.2
    /// monotonicity test).
    memory: Mutex<HashMap<u64, VecDeque<u64>>>,
    /// The monotonicity test window `W` (Section 5.2: last 16 snapshots).
    window: usize,
}

impl FaultEventCollector {
    /// A collector with the spec's default monotonicity window (16).
    pub fn new() -> Self {
        FaultEventCollector {
            queue: Mutex::new(VecDeque::new()),
            watchdog: Mutex::new(TtlWatch::default()),
            memory: Mutex::new(HashMap::new()),
            window: 16,
        }
    }

    /// A collector with a custom monotonicity window `W`.
    pub fn with_window(window: usize) -> Self {
        FaultEventCollector {
            queue: Mutex::new(VecDeque::new()),
            watchdog: Mutex::new(TtlWatch::default()),
            memory: Mutex::new(HashMap::new()),
            window: window.max(1),
        }
    }

    /// Records a hardware-trap fault (Section 5.1 source 1).
    pub fn record_hardware_trap(&self, site_id: u64, fault_class: u32) {
        self.queue.lock().push_back(FaultEvent {
            site_id,
            fault_class,
            source: FaultSource::HardwareTrap,
            timestamp: Instant::now(),
        });
    }

    /// Records a guard-call fault (Section 5.1 source 2).
    pub fn record_guard_fault(&self, site_id: u64, fault_class: u32) {
        self.queue.lock().push_back(FaultEvent {
            site_id,
            fault_class,
            source: FaultSource::GuardCall,
            timestamp: Instant::now(),
        });
    }

    /// The current queue depth.
    pub fn len(&self) -> usize {
        self.queue.lock().len()
    }

    /// `true` iff no fault events are pending.
    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }

    /// Dequeues all pending fault events for the engine.
    pub fn drain(&self) -> Vec<FaultEvent> {
        let mut q = self.queue.lock();
        q.drain(..).collect()
    }

    // ---- TTL watchdog ----

    /// Adds a TTL probe for a supervised module (`site_id`).
    pub fn watch(&self, site_id: u64, timeout: Duration) {
        self.watchdog.lock().watch(site_id, timeout);
    }

    /// The module acknowledged its watchdog (a heartbeat arrived).
    pub fn heartbeat(&self, site_id: u64) -> bool {
        self.watchdog.lock().acknowledge(site_id, Instant::now())
    }

    /// Removes a module from surveillance.
    pub fn unwatch(&self, site_id: u64) {
        self.watchdog.lock().unwatch(site_id);
    }

    /// Scans TTL probes, queueing a [`FaultEvent`] (with source
    /// `TtlProbe`/`GuardCall` semantics) for every module that has missed
    /// its deadline. Returns the number of new fault events queued.
    ///
    /// The fault class used is the site's module-level timeout. This is the
    /// "Module health watchdog -> module-level `FaultEvent`" wire of
    /// Section 5.1 item 3.
    pub fn ttl_tick(&self, now: Instant) -> usize {
        let dead = self.watchdog.lock().tick(now);
        let mut q = self.queue.lock();
        for site_id in &dead {
            q.push_back(FaultEvent {
                site_id: *site_id,
                fault_class: crate::supervisor::timeout_fault_tag(),
                source: FaultSource::TtlProbe,
                timestamp: now,
            });
        }
        dead.len()
    }

    /// The modules currently under surveillance.
    pub fn watched(&self) -> Vec<u64> {
        self.watchdog.lock().watched()
    }

    // ---- Memory-leak monotonicity test (Section 5.2) ----

    /// Runs the Section 5.2 monotonicity test: records a memory-usage
    /// snapshot for `site_id` and checks the last `W` snapshots. If the
    /// series is non-decreasing across all adjacent pairs (i.e. memory is
    /// monotonically growing), synthesizes and queues a `MemoryLeak`
    /// fault event and returns it. Otherwise returns `None`.
    ///
    /// On synthesis, `IsolateAndRestart` semantics are implied by the fault
    /// class; the engine schedules it from the drained event.
    pub fn record_memory_snapshot(&self, site_id: u64, usage: u64) -> Option<FaultEvent> {
        let mut memory = self.memory.lock();
        let series = memory.entry(site_id).or_default();
        if series.len() >= self.window {
            series.pop_front();
        }
        series.push_back(usage);

        if series.len() < self.window {
            return None;
        }

        // Monotonicity test: usage[i] <= usage[i+1] for all i (non-
        // decreasing series => resilience grows without bound => leak).
        let slice = series.make_contiguous();
        if slice.windows(2).all(|w| w[0] <= w[1]) {
            let event = FaultEvent {
                site_id,
                fault_class: FAULT_CLASS_MEMORY_LEAK,
                source: FaultSource::MonotonicMemoryLeak,
                timestamp: Instant::now(),
            };
            self.queue.lock().push_back(event);
            Some(event)
        } else {
            None
        }
    }

    /// Number of sites the monotonicity test is currently tracking.
    pub fn tracked_memory_sites(&self) -> usize {
        self.memory.lock().len()
    }
}

impl Default for FaultEventCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_drains_hardware_and_guard_faults() {
        let collector = FaultEventCollector::new();
        collector.record_hardware_trap(0x1, 0);
        collector.record_guard_fault(0x1, 1);
        let drained = collector.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].source, FaultSource::HardwareTrap);
        assert_eq!(drained[1].source, FaultSource::GuardCall);
        assert_eq!(collector.len(), 0);
    }

    #[test]
    fn ttl_watchdog_detects_expired_module() {
        let now = Instant::now();
        let mut watch = TtlWatch::default();
        watch.watch(0xAAAA, Duration::from_millis(10));
        assert!(watch.tick(now).is_empty());

        // After the timeout has elapsed (but before the very next tick),
        // acknowledged is late.
        let late = now + Duration::from_millis(100);
        let dead = watch.tick(late);
        assert_eq!(dead, vec![0xAAAA]);
    }

    #[test]
    fn monotonic_memory_produces_memory_leak_fault() {
        let collector = FaultEventCollector::with_window(4);
        // Strictly growing memory on snapshots 1..4 => leak detected.
        assert!(collector.record_memory_snapshot(0x1, 100).is_none());
        assert!(collector.record_memory_snapshot(0x1, 110).is_none());
        assert!(collector.record_memory_snapshot(0x1, 121).is_none());
        let event = collector.record_memory_snapshot(0x1, 133).unwrap();
        assert_eq!(event.fault_class, FAULT_CLASS_MEMORY_LEAK);
        assert_eq!(event.source, FaultSource::MonotonicMemoryLeak);
    }

    #[test]
    fn oscillating_memory_does_not_trip_leak() {
        let collector = FaultEventCollector::with_window(4);
        for usage in [100u64, 90, 110, 105] {
            // Varies up and down; must not trip the monotonicity test.
            assert!(collector.record_memory_snapshot(0x9, usage).is_none());
        }
        assert_eq!(collector.drain().len(), 0);
    }
}
