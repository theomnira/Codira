//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Precise stack roots and safepoint polls for code compiled through
//!   LLVM. The passes themselves are in `cpp/gc_roots.cpp`; this module
//!   runs them and documents the contract with the runtime
//!   (`codira_memory::gc::frames`).
//!
//! # Frame records
//!
//! After the assembly module is linked and before its final optimization,
//! every function that makes a *safepoint call* (any call but an LLVM
//! intrinsic, inline assembly or a call marked `codira-gc-leaf`) gets a
//! frame record on its stack:
//!
//! ```text
//! offset  0  prev     the next record up the stack (the caller's side)
//!         8  header   0xC0DA << 48 | regions << 24 | words
//!        16  lo       the stack pointer after the prologue
//!        24  hi       the address of the return address (x86-64 SysV), or 0
//!        32  regions  `regions` pairs of (address, size): stack slots that
//!                     may hold a reference
//!            words    `words` spill words
//! ```
//!
//! On entry the function links the record into the calling thread's chain
//! (whose head the runtime hands out through `codira.gc.chain_fn`), and on
//! every return it unlinks it. Every value that may hold a GC reference and
//! is live across a safepoint call, or is an argument of one, is stored
//! into a spill word when it is defined (an argument passed on the stack
//! lives in the caller's outgoing-argument area, inside the frame the
//! collector skips, and the callee may hold it only there). A value "may hold a reference" unless it is provably the
//! address of a stack slot, a global or a constant; integers only when
//! `ptrtoint` made them from such a value. A stack slot is a region unless
//! every use of it provably keeps references out of it.
//!
//! The record escapes (the chain head points at it), so the optimizer must
//! keep each spill word up to date at every call that may read memory --
//! exactly the safepoint calls. A call the optimizer proves reads no memory
//! cannot reach an allocation (every allocation is a call to the runtime,
//! which the optimizer knows nothing about); it can only reach a poll, and
//! a poll scans the whole thread conservatively (below).
//!
//! The runtime scans each record's regions and words precisely (only
//! there; integers elsewhere in the frame are not roots) and, where `hi` is
//! known, skips the machine frame `[lo, hi - 64)` in its conservative scan
//! of the thread's stack. The top 64 bytes stay in the conservative scan:
//! that is where the prologue pushed the callee-saved registers, which hold
//! the caller's values.
//!
//! A function the pass cannot handle (an `invoke`, a `musttail` call, a
//! dynamic stack slot that may hold a reference, `returns_twice`) gets no
//! record, is marked `codira-gc-conservative` and `noinline`, and its calls
//! `noinline`: its frame is scanned conservatively, and no precise record
//! may claim it.
//!
//! # Polls
//!
//! After the final optimization (so they never stand in the way of
//! vectorization), the back-edge of every loop whose trip count the
//! optimizer cannot bound when the loop is entered, and the entry of every
//! function that makes a safepoint call (which is what any recursion goes
//! through), gets a poll: a load of the byte `codira.gc.poll_word` points at, and a
//! branch to a cold call of `codira.gc.poll_fn` when it is set. A thread
//! parked at a poll is scanned conservatively from top to bottom: the
//! optimizer ran before the poll existed, so spill words need not be up to
//! date there.
//!
//! A loop the optimizer can bound (scalar evolution computes a symbolic
//! maximum of its back-edge count: a counted loop, a loop over an array or
//! a range, one that may also leave early) gets no poll: it ends after at
//! most that many iterations, and the loops and calls around it poll. The
//! price is latency: a collection may wait for such a loop to finish,
//! however many iterations it has.
//!
//! # Interface for other back ends
//!
//! A back end that emits its own stack maps (Cranelift has
//! `StackMap`s at safepoints) can take part without these passes by building
//! the same records: link one per frame into the chain, describe the slots
//! that hold references as regions or words, and set `hi` to 0 (no frame
//! is then skipped by the conservative scan) or to the address of the
//! frame's return address if it guarantees the same prologue layout. Code
//! without records is always scanned conservatively.

use inkwell::module::Module;

/// Reserved for strip mining (polling a bounded loop every this many
/// iterations); not used yet: bounded loops are not polled at all.
pub const MAX_UNPOLLED_TRIPS: u32 = 4096;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GcRootStats {
    /// Functions given a precise frame record.
    pub precise: u32,
    /// Functions left to the conservative scan.
    pub conservative: u32,
    /// Values spilled into records.
    pub spilled: u32,
    /// Stack slots registered as regions.
    pub regions: u32,
}

extern "C" {
    fn codira_gc_insert_roots(module: llvm_sys::prelude::LLVMModuleRef, out: *mut GcRootStats);
    fn codira_gc_insert_polls(module: llvm_sys::prelude::LLVMModuleRef, max_unpolled: u32) -> u32;
}

/// Whether to emit frame records and polls. On unless `CODIRA_GC_FRAMES=0`
/// is set at compile time (for measuring their cost).
pub(crate) fn enabled() -> bool {
    !matches!(std::env::var("CODIRA_GC_FRAMES").as_deref(), Ok("0"))
}

/// Whether a call to `function` is bracketed by a foreign transition
/// (`codira.gc.foreign_enter` / `foreign_leave`), so a collection runs while
/// it executes instead of waiting for it: an `extern` function declared
/// `@noalloc` (a trusted assertion that it neither allocates nor calls back
/// into Codira code), with frame records on.
pub(crate) fn is_foreign_transition(
    db: &dyn codira_hir::HirDatabase,
    function: codira_hir::Function,
) -> bool {
    enabled()
        && function.is_extern(db)
        && codira_hir::realtime::declared_forbidden(db, function)
            .contains(codira_hir::realtime::Effect::Alloc)
}

/// Runs the frame-record pass over `module` (see the module docs).
pub(crate) fn insert_roots(module: &Module<'_>) -> GcRootStats {
    let mut stats = GcRootStats::default();
    // SAFETY: the module is valid for the duration of the call and nothing
    // else uses it meanwhile.
    unsafe { codira_gc_insert_roots(module.as_mut_ptr(), &mut stats) };
    stats
}

/// Runs the poll pass over `module`; returns the number of polls.
pub(crate) fn insert_polls(module: &Module<'_>) -> u32 {
    // SAFETY: as in `insert_roots`.
    unsafe { codira_gc_insert_polls(module.as_mut_ptr(), MAX_UNPOLLED_TRIPS) }
}
