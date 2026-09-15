//! Hardware-compiler co-design (Section 2.7.4 of the HERACLES spec,
//! "Silicon-Level Immune System").
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 7, 2026
//!
//! Functionality (per Theorem 7 and Section 2.7.4):
//! - Models the fine-grained execution telemetry an open-silicon (RISC-V style)
//!   CPU architecture would expose to the compiler: a `faulty_cores[]` bitmap,
//!   a `bad_dram_rows[]` bitmap, a `throttled_cores[]` thermal bitmap, and
//!   soft-error traps keyed by program counter.
//! - The healing host uses this telemetry exactly as the paper's JIT does:
//!   * migrate work away from degraded cores (`pick_healthy_core` /
//!     `healthy_cores`),
//!   * remap object allocations so they never land on poisoned DRAM rows
//!     (`alloc_avoiding_bad_rows`), and
//!   * count soft errors per PC so the patcher can synthesize an alternate code
//!     path around degraded instructions.
//!
//! The telemetry model is deliberately pure data + pure policy (no actual
//! thread migration or ECC is performed -- that is the OS/CPU's job). This
//! module is the decision layer the rest of the healing stack consults, and
//! it implements the *information* the paper's `CSRRS 0x842` /
//! `CSRRC 0x843` interfaces carry.

use std::collections::{BTreeSet, HashMap};

/// A physical CPU core index (up to 256 cores, which covers the paper's
/// examples and typical machines).
pub type CoreId = u8;

/// Telemetry about the physical machine, as exposed to the healing engine
/// by an HERACLES-aware CPU.
#[derive(Debug, Clone, Default)]
pub struct HardwareTelemetry {
    /// Cores the silicon reports as permanently degraded (`faulty_cores[]`).
    faulty_cores: BTreeSet<CoreId>,
    /// Cores thermally throttled (`throttled_cores[]`).
    throttled_cores: BTreeSet<CoreId>,
    /// DRAM rows poisoned by re-occurring (ECC-correctable) errors
    /// (`bad_dram_rows[]`), stored as row indices.
    bad_dram_rows: BTreeSet<u64>,
    /// Soft-error counts per program counter (the `TRAP(soft_error_at_pc)`
    /// feed).
    soft_errors_by_pc: HashMap<u64, u64>,
}

/// The DRAM granule assumed for row poisoning and remapping decisions.
/// The paper's example marks the range `0x14000-0x15FFF` (8 KiB) as bad --
/// two granules of this size.
pub const DRAM_ROW_GRANULE: u64 = 4096;

impl HardwareTelemetry {
    /// A telemetry snapshot with no degradation discovered yet.
    pub fn new() -> Self {
        HardwareTelemetry::default()
    }

    /// The silicon reports core `core` as permanently faulty
    /// (`faulty_cores[] = {2,5}`).
    pub fn report_faulty_core(&mut self, core: CoreId) {
        self.faulty_cores.insert(core);
        self.throttled_cores.remove(&core);
    }

    /// The silicon reports core `core` as thermally throttled.
    pub fn report_throttled_core(&mut self, core: CoreId) {
        self.throttled_cores.insert(core);
    }

    /// The silicon reports the DRAM row(s) covering `address` as poisoned.
    /// Stored row-aligned so `is_bad_address` is a single set lookup.
    pub fn report_bad_dram_row(&mut self, address: u64) {
        let row = address / DRAM_ROW_GRANULE;
        self.bad_dram_rows.insert(row);
    }

    /// The soft-error trap fired at program counter `pc`. Increments the
    /// per-PC counter the JIT consults when deciding whether to synthesize
    /// an alternate path around this instruction.
    pub fn note_soft_error(&mut self, pc: u64) {
        *self.soft_errors_by_pc.entry(pc).or_insert(0) += 1;
    }

    /// Whether `core` is usable: neither faulty nor throttled.
    pub fn core_is_healthy(&self, core: CoreId) -> bool {
        !self.faulty_cores.contains(&core) && !self.throttled_cores.contains(&core)
    }

    /// Whether a DRAM address lies on a poisoned row.
    pub fn is_bad_address(&self, address: u64) -> bool {
        self.bad_dram_rows.contains(&(address / DRAM_ROW_GRANULE))
    }

    /// Whether an allocation covering `[address, address + size)` touches
    /// any poisoned row.
    pub fn allocation_touches_bad_row(&self, address: u64, size: u64) -> bool {
        let end = address.saturating_add(size);
        let mut cursor = address;
        while cursor < end {
            if self.is_bad_address(cursor) {
                return true;
            }
            cursor = cursor.saturating_add(DRAM_ROW_GRANULE);
            if cursor == 0 {
                break; // overflow safety
            }
        }
        false
    }

    /// The subset of `candidates` that is healthy -- the pool hot threads
    /// may be migrated into (Section 2.7.4: "JIT migrates hot threads away
    /// from cores 2,5"). Sorted ascending for deterministic scheduling.
    pub fn healthy_cores(&self, candidates: &[CoreId]) -> Vec<CoreId> {
        let mut healthy: Vec<CoreId> = candidates
            .iter()
            .copied()
            .filter(|&c| self.core_is_healthy(c))
            .collect();
        healthy.sort_unstable();
        healthy
    }

    /// Picks a core for the next thread: `preferred` if healthy, otherwise
    /// the first healthy candidate, otherwise `None` (everything degraded --
    /// the host must escalate, e.g. to `DegradeGracefully`).
    pub fn pick_healthy_core(&self, preferred: CoreId, candidates: &[CoreId]) -> Option<CoreId> {
        if self.core_is_healthy(preferred) {
            return Some(preferred);
        }
        self.healthy_cores(candidates).into_iter().next()
    }

    /// Finds the first granule-aligned address at or after `start` whose
    /// whole `[address, address + size)` span avoids poisoned rows. This is
    /// the JIT's "remap object allocations around bad rows" step.
    pub fn alloc_avoiding_bad_rows(&self, start: u64, size: u64) -> u64 {
        // The paper's range `0x14000-0x15FFF` is an 8 KiB span; when the
        // allocator has to skip it, it advances a whole granule at a time.
        let mut candidate = start.div_ceil(DRAM_ROW_GRANULE) * DRAM_ROW_GRANULE;
        let mut guard = 0u64;
        loop {
            guard += 1;
            assert!(
                guard < 1 << 24,
                "allocation failed to find a safe address within a sane search space"
            );
            if !self.allocation_touches_bad_row(candidate, size) {
                return candidate;
            }
            candidate = candidate.saturating_add(DRAM_ROW_GRANULE);
        }
    }

    /// The number of soft errors observed at `pc`.
    pub fn soft_errors_at(&self, pc: u64) -> u64 {
        self.soft_errors_by_pc.get(&pc).copied().unwrap_or(0)
    }

    /// All PCs that have seen soft errors, with their counts.
    pub fn soft_error_summary(&self) -> Vec<(u64, u64)> {
        let mut summary: Vec<(u64, u64)> = self
            .soft_errors_by_pc
            .iter()
            .map(|(&pc, &count)| (pc, count))
            .collect();
        summary.sort_unstable();
        summary
    }

    /// Whether the soft-error threshold for `pc` has been crossed; past
    /// this point the patcher is expected to stop dispatching to the code
    /// at `pc` and route through a synthesized alternate.
    pub fn soft_errors_exceed(&self, pc: u64, threshold: u64) -> bool {
        threshold > 0 && self.soft_errors_at(pc) >= threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faulty_core_is_excluded_from_healthy_pool() {
        let mut telemetry = HardwareTelemetry::new();
        telemetry.report_faulty_core(2);
        telemetry.report_faulty_core(5);
        let healthy = telemetry.healthy_cores(&[0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(healthy, vec![0, 1, 3, 4, 6]);
    }

    #[test]
    fn preferred_healthy_core_is_kept() {
        let mut telemetry = HardwareTelemetry::new();
        telemetry.report_faulty_core(2);
        assert_eq!(telemetry.pick_healthy_core(3, &[0, 1, 2, 3]), Some(3));
        assert_eq!(telemetry.pick_healthy_core(2, &[0, 1, 2, 3]), Some(0));
    }

    #[test]
    fn fully_degraded_machine_returns_none() {
        let mut telemetry = HardwareTelemetry::new();
        telemetry.report_faulty_core(0);
        telemetry.report_throttled_core(1);
        assert_eq!(telemetry.pick_healthy_core(0, &[0, 1]), None);
    }

    #[test]
    fn allocation_skips_poisoned_rows() {
        let mut telemetry = HardwareTelemetry::new();
        // Mark the paper's example range 0x14000-0x15FFF (rows 5 and 6).
        telemetry.report_bad_dram_row(0x14000);
        telemetry.report_bad_dram_row(0x15000);
        assert!(telemetry.is_bad_address(0x14ABC));
        assert!(!telemetry.is_bad_address(0x13ABC));

        // An 8 KiB allocation starting right before the poisoned rows must
        // be remapped to just past them.
        let address = telemetry.alloc_avoiding_bad_rows(0x13000, 0x2000);
        assert_eq!(address, 0x16000);
        assert!(!telemetry.allocation_touches_bad_row(address, 0x2000));
    }

    #[test]
    fn small_allocation_can_skip_into_hole() {
        let mut telemetry = HardwareTelemetry::new();
        telemetry.report_bad_dram_row(0x14000);
        // A single-granule allocation must land in the first free granule.
        assert_eq!(telemetry.alloc_avoiding_bad_rows(0x14000, 0x1000), 0x15000);
    }

    #[test]
    fn soft_errors_accumulate_and_threshold() {
        let mut telemetry = HardwareTelemetry::new();
        telemetry.note_soft_error(0x4000);
        telemetry.note_soft_error(0x4000);
        telemetry.note_soft_error(0x4020);
        assert_eq!(telemetry.soft_errors_at(0x4000), 2);
        assert_eq!(telemetry.soft_errors_at(0x4020), 1);
        assert!(telemetry.soft_errors_exceed(0x4000, 2));
        assert!(!telemetry.soft_errors_exceed(0x4000, 3));
        assert_eq!(
            telemetry.soft_error_summary(),
            vec![(0x4000, 2), (0x4020, 1)]
        );
    }
}
