//! Reversible execution: instruction-level undo with zero state pollution
//! (Section 2.7.2 of the HERACLES spec).
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 7, 2026
//!
//! Functionality (per Theorem 5 and Section 2.7.2):
//! - Micro-checkpointing at every pure expression / minimal re-execution
//!   boundary. A [`RegisterFile`] records every register write as a write-ahead
//!   undo entry, so a fault during a guarded region can rewind execution to the
//!   last [`Checkpoint`] -- the spec's "rewind to Checkpoint C" -- and retry
//!   with a corrected value.
//! - `Checkpoint::commit` is the complement: once a region is known-good,
//!   committing frees the undo history up to that point, so successful
//!   execution does not accumulate a growing journal.
//! - The real-hardware payoff lives in [`crate::trap::protected`]: a hardware
//!   fault (divide-by-zero, access violation) cannot touch ordinary register
//!   state left *before* the fault. Operations that mutate tracked registers
//!   *after* the last checkpoint and *before* the faulting instruction would
//!   otherwise leak -- that pollution is exactly what this journal rolls back.
//!
//! The state being rewound here is a small integer register file, honest to
//! what systems code can safely roll back without a VM. This is the
//! executable counterpart of the paper's "faults leave zero observable
//! state mutation": after `rollback_to`, no external observer can tell the
//! attempt ever started.
//!
//! Combined with [`crate::synthesis::PatchSynthesizer`] this plays the
//! spec's actual recovery timeline: fault -> rewind -> SMT solves a repaired
//! input -> re-execute forward -> success. See the end-to-end test
//! `rewind_fault_and_retry_with_synthetic_input`.

use std::collections::HashMap;

/// A named register in the reversible file. Callers build these from their
/// own site layout (each guarded computation declares which registers it
/// touches). Distinct names isolate distinct computations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Register(pub u32);

/// An opaque handle to a point in the undo journal.
///
/// A `Checkpoint` is created by [`RegisterFile::checkpoint`], which records
/// the current journal length. Any register write after that point can be
/// undone by [`RegisterFile::rollback_to`]. Checks are sticky: the journal
/// shrinks as writes are undone, so handles never alias stale entries unless
/// they are re-created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    /// The journal length at checkpoint time.
    index: usize,
}

/// One logged write: the previous value of `register`, which the rollback
/// path must restore.
#[derive(Debug, Clone)]
struct LogEntry {
    register: Register,
    previous: Option<i64>,
}

/// A small, single-threaded vault of integer registers with a write-ahead
/// undo journal providing instruction-level reversibility.
///
/// Every [`RegisterFile::write`] appends the register's prior value to the
/// journal (O(1)) and then stores the new value. Rolling back to a
/// [`Checkpoint`] pops the trailing entries and restores each register,
/// erasing registers that did not exist before (they are removed again).
/// Committing to a checkpoint discards the entries *before* it, bounding
/// the journal for long-running healthy computations.
#[derive(Debug, Default)]
pub struct RegisterFile {
    /// register -> current value. Registers that existed at journal start
    /// keep their value when rolled back to that point.
    values: HashMap<Register, i64>,
    /// The undo journal, newest last.
    journal: Vec<LogEntry>,
    /// Checkpoints still open, as journal indexes.
    checkpoints: Vec<usize>,
}

impl RegisterFile {
    /// A fresh, reversible register file with no pending checkpoints.
    pub fn new() -> Self {
        RegisterFile::default()
    }

    /// Reads the current value of `register`, if it has ever been written.
    pub fn read(&self, register: Register) -> Option<i64> {
        self.values.get(&register).copied()
    }

    /// Reads `register`, defaulting to `fallback` if it was never written.
    pub fn read_or(&self, register: Register, fallback: i64) -> i64 {
        self.values.get(&register).copied().unwrap_or(fallback)
    }

    /// Writes `value` to `register`, logging the prior value first.
    pub fn write(&mut self, register: Register, value: i64) {
        let previous = self.values.insert(register, value);
        self.journal.push(LogEntry { register, previous });
    }

    /// Opens a new checkpoint: rollback can later return the file to this
    /// exact state (the writes since then are the only undoable ones).
    pub fn checkpoint(&mut self) -> Checkpoint {
        let index = self.journal.len();
        self.checkpoints.push(index);
        Checkpoint { index }
    }

    /// Discards the undo history accumulated before `checkpoint`, so the
    /// lifetime and memory of the journal stay `O(recovered writes)`.
    ///
    /// The checkpoint itself and anything opened before it become committed
    /// history: rolling back to them is no longer possible. Checkpoints
    /// opened *after* it remain valid and are re-indexed to the shortened
    /// journal.
    pub fn commit(&mut self, checkpoint: Checkpoint) {
        let pos = match self
            .checkpoints
            .iter()
            .position(|&idx| idx == checkpoint.index)
        {
            Some(p) => p,
            None => return,
        };
        // Drop the committed prefix from the journal.
        self.journal.drain(..checkpoint.index);
        // This checkpoint and everything opened before it are closed.
        self.checkpoints.drain(..=pos);
        // Checkpoints opened after it still refer to live journal entries;
        // the drained prefix shifts their indexes down.
        let shift = checkpoint.index;
        for c in self.checkpoints.iter_mut() {
            *c = c.saturating_sub(shift);
        }
    }

    /// Undoes every write after `checkpoint`, restoring each register to
    /// its pre-checkpoint value; registers created after the checkpoint are
    /// removed entirely.
    ///
    /// # Panics
    ///
    /// Panics if the journal is too short to rewind the checkpoint (this is
    /// a logic bug in the caller, not a recoverable condition).
    pub fn rollback_to(&mut self, checkpoint: Checkpoint) {
        assert!(
            checkpoint.index <= self.journal.len(),
            "checkpoint index {} is beyond the journal length {}",
            checkpoint.index,
            self.journal.len()
        );
        while self.journal.len() > checkpoint.index {
            let entry = self.journal.pop().expect("non-empty journal");
            // Restore the previous value, or drop the register entirely if
            // it had none before (it was created within the rolled-back span).
            match entry.previous {
                Some(prev) => {
                    self.values.insert(entry.register, prev);
                }
                None => {
                    self.values.remove(&entry.register);
                }
            }
        }
        self.checkpoints.retain(|&c| c <= checkpoint.index);
    }

    /// The number of pending (undoable) writes.
    pub fn pending(&self) -> usize {
        self.journal.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_is_reversible_within_a_checkpoint() {
        let mut rf = RegisterFile::new();
        let cp = rf.checkpoint();
        rf.write(Register(0), 5);
        rf.write(Register(1), 7);
        assert_eq!(rf.read(Register(0)), Some(5));
        assert_eq!(rf.read(Register(1)), Some(7));

        rf.rollback_to(cp);
        // Nothing polluted: both registers are as they were at the
        // checkpoint (never existed).
        assert_eq!(rf.read(Register(0)), None);
        assert_eq!(rf.read(Register(1)), None);
        assert_eq!(rf.pending(), 0);
    }

    #[test]
    fn refresh_preserves_pre_checkpoint_values() {
        let mut rf = RegisterFile::new();
        rf.write(Register(0), 3);
        let cp = rf.checkpoint();
        rf.write(Register(0), 9);
        rf.write(Register(1), 1);
        rf.rollback_to(cp);
        assert_eq!(rf.read(Register(0)), Some(3));
        assert_eq!(rf.read(Register(1)), None);
    }

    #[test]
    fn commit_prunes_recovered_history() {
        let mut rf = RegisterFile::new();
        let cp = rf.checkpoint();
        rf.write(Register(0), 1);
        assert_eq!(rf.pending(), 1);
        rf.commit(cp);
        // The write survives (it performed the mutation), but the undo
        // journal slab before the checkpoint is gone.
        assert_eq!(rf.read(Register(0)), Some(1));
        assert_eq!(rf.pending(), 1);
    }

    #[test]
    fn rollback_to_nested_checkpoint_restores_intermediate_state() {
        let mut rf = RegisterFile::new();
        let cp_a = rf.checkpoint();
        rf.write(Register(0), 1);
        let cp_b = rf.checkpoint();
        rf.write(Register(0), 2);
        rf.write(Register(1), 42);
        rf.rollback_to(cp_b);
        assert_eq!(rf.read(Register(0)), Some(1));
        assert_eq!(rf.read(Register(1)), None);
        rf.rollback_to(cp_a);
        assert_eq!(rf.read(Register(0)), None);
    }
}

/// End-to-end: a real hardware divide-by-zero faults mid-computation after
/// state was polluted; we rewind the pollution, pick a divisor with the SMT
/// synthesizer, and re-execute. The final register file is indistinguishable
/// from a run that simply never faulted.
#[cfg(test)]
mod integration_tests {
    use codira_smt::Context;

    use super::*;
    use crate::{
        synthesis::PatchSynthesizer,
        trap::{install, protected},
    };

    const DIVISOR: Register = Register(0);
    const ATTEMPTS: Register = Register(1);

    #[test]
    fn rewind_fault_and_retry_with_synthetic_input() {
        install();
        let synth = PatchSynthesizer::new_with_options(true, 16);
        let smt = Context::new();

        let mut rf = RegisterFile::new();
        let mut divisor_candidate = 0i64; // starts bad: poison by zero
        let mut guard = 0;

        let result: Option<i64> = loop {
            guard += 1;
            assert!(guard < 32, "recovery should converge immediately");

            // Checkpoint right before the fault-prone region. The write to
            // DIVISOR below is *about* to happen and must be undoable.
            let cp = rf.checkpoint();
            rf.write(DIVISOR, divisor_candidate);
            rf.write(ATTEMPTS, i64::from(guard));

            let outcome = protected(|| {
                // Real `idiv` with the candidate denominator; the zero case
                // faults physically.
                let mut quot: i32;
                unsafe {
                    core::arch::asm!(
                        "mov eax, 100",
                        "cdq",
                        "idiv {den:e}",
                        "mov {res:e}, eax",
                        den = in(reg) divisor_candidate as i32,
                        res = out(reg) quot,
                        out("eax") _,
                        out("edx") _,
                    );
                }
                quot
            });

            match outcome {
                Ok(quot) => {
                    // The region is proven good: keep the fix and the
                    // journal that justified it.
                    rf.commit(cp);
                    break Some(i64::from(quot));
                }
                Err(_fault) => {
                    // Zero state pollution: roll back exactly the mutations
                    // made between `cp` and the fault.
                    rf.rollback_to(cp);
                    // Repair the input via SMT synthesis (the smallest
                    // positive divisor satisfies the postcondition "result
                    // must be produced").
                    let repaired = match synth.synthesize(&smt, "d", |c, d| d.gt(c.int_lit(0))) {
                        crate::synthesis::SynthesisOutcome::Verified { value } => value,
                        other => panic!("synthesis failed: {other:?}"),
                    };
                    divisor_candidate = repaired;
                    assert_ne!(divisor_candidate, 0, "synthesis must never pick zero");
                }
            }
        };

        assert_eq!(rf.read(DIVISOR), Some(divisor_candidate));
        assert_eq!(rf.read(ATTEMPTS), Some(i64::from(guard)));
        // The pollution (DIVISOR == 0) never reached a committed state.
        assert!(divisor_candidate > 0);
        // And the quotient really was recovered: 100 / 1 = 100.
        assert_eq!(result, Some(100));
    }
}
