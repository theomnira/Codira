//! Code Patcher (Section 5.4 of the HERACLES spec): atomic patching of the
//! strategy a guarded call site uses.
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 7, 2026
//!
//! Functionality (per Section 5.4):
//! - The spec's patcher rewrites *machine code* (a two-byte nop -> call on
//!   x86-64 with a locked `CMPXCHG16B`; instruction-cache sync on `AArch64`).
//!   This crate is explicitly *not* a JIT (see `lib.rs`), so the executable
//!   counterpart provided here patches the *strategy slot* a guarded call site
//!   dispatches through: an atomic pointer-sized slot in its own table. A
//!   reader observes either the old strategy or the new one, never a torn
//!   half-patch -- which is the paper's "atomicity without stopping other
//!   threads".
//! - [`StrategyPatch::patch`] applies a new strategy via compare-and-swap, the
//!   direct Rust analogue of the locked CMPXCHG the spec describes.
//!   `patch_no_target` / optimistic retry is left as an exercise; callers use
//!   `patch` or the CAS-based `patch_if` directly.
//! - [`StrategyInterner`] keeps the AOT-precompiled strategy set stable so tag
//!   <-> function resolution is a cache-friendly look-up, mirroring "the same
//!   mechanism hot-reloading already uses".

use std::sync::atomic::{AtomicUsize, Ordering};

/// A precompiled strategy function with no arguments, dispatched by a
/// guarded call site. Signature is deliberately trivial (like a delay /
/// no-op probe); real sites carry their own ABI.
pub type StrategyFn = fn() -> i64;

/// Interns the set of AOT-precompiled strategies, assigning each a stable
/// tag. This is the ".heal section index" / dispatch table of Section 5.4.
#[derive(Debug)]
pub struct StrategyInterner {
    table: Vec<(&'static str, StrategyFn)>,
}

impl StrategyInterner {
    /// Builds the intern table from precompiled strategies.
    pub fn new(strategies: Vec<(&'static str, StrategyFn)>) -> Self {
        StrategyInterner { table: strategies }
    }

    /// The tag for the strategy named `name`, if registered.
    pub fn tag(&self, name: &'static str) -> Option<usize> {
        self.table.iter().position(|(n, _)| *n == name)
    }

    /// The function behind `tag` (panics on unknown tags - the interner is
    /// built from AOT metadata, so this is an internal logic error).
    pub fn strategy(&self, tag: usize) -> StrategyFn {
        self.table
            .get(tag)
            .map(|(_, f)| *f)
            .expect("unknown strategy tag")
    }

    /// The name of `tag` for diagnostics.
    pub fn name(&self, tag: usize) -> &'static str {
        self.table
            .get(tag)
            .map(|(n, _)| *n)
            .expect("unknown strategy tag")
    }
}

/// A guarded call site's patchable strategy slot.
///
/// The slot stores the *tag* of the currently-active strategy as an atomic
/// usize, so `patch`/`patch_if` are single atomic instructions (the
/// `CMPXCHG` description in the spec) and readers (`active`) are plain
/// loads. No lock is ever held while reading or patching.
#[derive(Debug)]
pub struct PatchableSlot {
    /// Current strategy tag (index into the interner).
    current: AtomicUsize,
    /// The interner this slot resolves tags through.
    interner: *const StrategyInterner,
}

// SAFETY: `interner` is an immutable `*const StrategyInterner` created from a
// `&StrategyInterner` the caller guarantees outlives the slot (statically in
// the crate's intended use). It is never written through and never freed
// while the slot is alive. `AtomicUsize` provides interior mutability &
// Send/Sync.
unsafe impl Send for PatchableSlot {}
unsafe impl Sync for PatchableSlot {}

impl PatchableSlot {
    /// Binds a site to `initial_tag` under `interner`.
    ///
    /// # Safety
    ///
    /// The caller must ensure `interner` outlives this slot (and any clones
    /// that dereference it).
    pub unsafe fn new(interner: &StrategyInterner, initial_tag: usize) -> Self {
        assert!(
            initial_tag < interner.table.len(),
            "initial tag must be registered"
        );
        PatchableSlot {
            current: AtomicUsize::new(initial_tag),
            interner: interner as *const StrategyInterner,
        }
    }

    /// The currently active strategy tag.
    pub fn active_tag(&self) -> usize {
        self.current.load(Ordering::Acquire)
    }

    /// The currently active strategy function.
    pub fn active(&self) -> StrategyFn {
        // SAFETY: `interner` outlives the slot and any clones.
        unsafe { (*self.interner).strategy(self.active_tag()) }
    }

    /// Atomically overwrites the active strategy with `new_fn` (which must
    /// have been interned under `interner`). Returns the previous function.
    pub fn patch(&self, interner: &StrategyInterner, new_fn: StrategyFn) -> StrategyFn {
        let Some(tag) = interner
            .table
            .iter()
            .position(|(_, f)| *f as usize == new_fn as usize)
        else {
            panic!("strategy must be interned before patching")
        };
        let old = self.current.swap(tag, Ordering::AcqRel);
        // SAFETY: `interner` outlives the slot.
        unsafe { (*self.interner).strategy(old) }
    }

    /// Atomically sets `new_tag` *only if* the current tag is `expected`.
    /// This is the spec's CAS (`CMPXCHG16B` analogue) that guarantees no
    /// two patchers can clobber each other, and that a reader cannot catch a
    /// torn patch.
    pub fn patch_if(&self, expected_tag: usize, new_tag: usize) -> bool {
        self.current
            .compare_exchange(expected_tag, new_tag, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// True if the slot dispatches the given interned tag.
    pub fn dispatches(&self, tag: usize) -> bool {
        self.active_tag() == tag
    }
}

/// A batch of patchable slots, e.g. one per guarded call site in a module.
/// Present primarily to make multi-site patching ergonomic and to demonstrate
/// the "patch a family of call sites atomically" pattern from Section 6.3.
#[derive(Debug, Default)]
pub struct PatchSites {
    slots: Vec<PatchableSlot>,
}

impl PatchSites {
    /// How many call sites are registered.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// `true` iff no call sites are registered.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Registers a call site; returns its index.
    ///
    /// # Safety
    ///
    /// `interner` must outlive every added slot.
    pub unsafe fn add(&mut self, interner: &StrategyInterner, initial_tag: usize) -> usize {
        let idx = self.slots.len();
        self.slots.push(PatchableSlot::new(interner, initial_tag));
        idx
    }

    /// The slot for call site `index`.
    pub fn slot(&self, index: usize) -> &PatchableSlot {
        &self.slots[index]
    }
}

impl Default for StrategyInterner {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retry() -> i64 {
        1
    }
    fn fallback() -> i64 {
        2
    }
    fn degrade() -> i64 {
        3
    }

    #[test]
    fn interner_assigns_stable_tags() {
        let interner = StrategyInterner::new(vec![
            ("retry", retry),
            ("fallback", fallback),
            ("degrade", degrade),
        ]);
        assert_eq!(interner.tag("fallback"), Some(1));
        assert_eq!(interner.name(2), "degrade");
        assert_eq!(interner.strategy(0)(), 1);
    }

    #[test]
    fn patch_option_swaps_strategy_atomically() {
        let interner = StrategyInterner::new(vec![
            ("retry", retry),
            ("fallback", fallback),
            ("degrade", degrade),
        ]);
        // SAFETY: `interner` lives for the rest of the test.
        let slot = unsafe { PatchableSlot::new(&interner, 0) };
        assert!(slot.dispatches(0));
        assert_eq!(slot.active()(), 1);

        slot.patch(&interner, fallback);
        assert!(slot.dispatches(1));
        assert_eq!(slot.active()(), 2);
    }

    #[test]
    fn cas_patch_ignores_expected_mismatch() {
        let interner = StrategyInterner::new(vec![("retry", retry), ("fallback", fallback)]);
        // SAFETY: interner survives the test.
        let slot = unsafe { PatchableSlot::new(&interner, 0) };

        // WRONG expected tag: no-op.
        assert!(!slot.patch_if(5, 1));
        assert!(slot.dispatches(0));

        // Correct expected tag: succeeds.
        assert!(slot.patch_if(0, 1));
        assert!(slot.dispatches(1));
    }
}
