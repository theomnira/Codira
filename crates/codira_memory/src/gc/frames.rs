//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The runtime half of precise stack roots: the per-thread chain of frame
//!   records compiled code links itself into, the hooks a loaded library is
//!   given (`set_gc_hooks`), the poll word and poll entry point loop
//!   back-edges call, the choice between precise and conservative stack
//!   scanning, and the stress mode that collects at every safepoint.
//!
//! The record layout and the compiler's side of the contract are described
//! in `codira_codegen::gc_roots`. In short: a record names the stack slots
//! and spill words of one function activation that may hold references at
//! a safepoint call, and the machine frame `[lo, hi)` the activation
//! occupies. The collector reads the slots precisely, and leaves the frame
//! (minus the 64 bytes at its top where the prologue saved the caller's
//! callee-saved registers) out of the conservative scan of the thread's
//! stack. Everything else on the stack -- host frames, foreign frames,
//! frames of code compiled without records -- is still scanned
//! conservatively.

use std::{
    cell::Cell,
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
};

/// The header of a frame record (see `codira_codegen::gc_roots`).
#[repr(C)]
pub struct FrameRecord {
    /// The next record up the stack.
    prev: *const FrameRecord,
    /// `MAGIC << 48 | regions << 24 | words`.
    header: u64,
    /// The stack pointer of the activation after its prologue.
    lo: usize,
    /// The address of the activation's return address, or 0 if unknown.
    hi: usize,
}

/// `header >> 48` of every record.
const MAGIC: u64 = 0xC0DA;

/// Bytes at the top of a precise machine frame that stay in the
/// conservative scan: the callee-saved registers the prologue pushed.
const SAVED_REGISTERS: usize = 64;

thread_local! {
    /// The innermost frame record of this thread. No destructor: compiled
    /// code may unlink its records while thread-local storage is being torn
    /// down.
    static CHAIN: Cell<*const FrameRecord> = const { Cell::new(std::ptr::null()) };
}

/// Returns the address of the current thread's chain head. Compiled code
/// calls it (through `codira.gc.chain_fn`) at the entry of every function
/// with a frame record.
pub extern "C" fn codira_gc_chain_slot() -> *mut *const FrameRecord {
    CHAIN.with(|chain| chain.as_ptr())
}

/// The address of the current thread's chain head, as an integer the
/// thread registry keeps.
pub(crate) fn chain_slot_address() -> usize {
    codira_gc_chain_slot() as usize
}

/// The innermost record of the current thread.
pub(crate) fn chain_head() -> *const FrameRecord {
    CHAIN.with(Cell::get)
}

/// Replaces the innermost record of the current thread (after a non-local
/// exit abandoned the frames above it).
pub(crate) fn set_chain_head(head: *const FrameRecord) {
    CHAIN.with(|chain| chain.set(head));
}

/// Non-zero while compiled code must call [`codira_gc_poll`] at its next
/// poll: a stop is requested, or stress mode is on.
pub(crate) static POLL_WORD: AtomicU8 = AtomicU8::new(0);

/// What a loaded library is given through its `set_gc_hooks` export.
#[derive(Clone, Copy, Debug)]
pub struct GcHooks {
    /// `codira_gc_chain_slot`.
    pub chain_fn: *const std::ffi::c_void,
    /// The poll word.
    pub poll_word: *const std::ffi::c_void,
    /// `codira_gc_poll`.
    pub poll_fn: *const std::ffi::c_void,
}

/// Returns the hooks every library compiled with frame records and polls
/// must be given before its code runs.
pub fn hooks() -> GcHooks {
    GcHooks {
        chain_fn: codira_gc_chain_slot as *const std::ffi::c_void,
        poll_word: POLL_WORD.as_ptr() as *const std::ffi::c_void,
        poll_fn: codira_gc_poll as *const std::ffi::c_void,
    }
}

/// The poll compiled code calls from loop back-edges and function entries
/// when the poll word is set. The whole thread is then scanned
/// conservatively: spill words need not be up to date at a poll.
pub extern "C" fn codira_gc_poll() {
    super::safepoint::poll_from_code();
}

/// How stacks are scanned for roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootMode {
    /// Frame records are read precisely and the machine frames they
    /// describe are left out of the conservative scan; everything else is
    /// scanned conservatively. The default.
    Precise,
    /// Every word of every stack is scanned conservatively (frame records
    /// included). Nothing a compiled frame holds can be missed by the
    /// compiler's analysis, at the cost of integers retaining objects.
    Conservative,
}

const MODE_UNSET: u8 = 0;
const MODE_PRECISE: u8 = 1;
const MODE_CONSERVATIVE: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(MODE_UNSET);

/// Returns how stacks are scanned: [`set_root_mode`], else the
/// `CODIRA_GC_ROOTS` environment variable (`precise` or `conservative`),
/// else [`RootMode::Precise`].
pub fn root_mode() -> RootMode {
    match MODE.load(Ordering::Relaxed) {
        MODE_PRECISE => RootMode::Precise,
        MODE_CONSERVATIVE => RootMode::Conservative,
        _ => {
            let mode = match std::env::var("CODIRA_GC_ROOTS").as_deref() {
                Ok("conservative") => RootMode::Conservative,
                _ => RootMode::Precise,
            };
            set_root_mode(mode);
            mode
        }
    }
}

/// Selects how stacks are scanned from the next collection on.
pub fn set_root_mode(mode: RootMode) {
    let value = match mode {
        RootMode::Precise => MODE_PRECISE,
        RootMode::Conservative => MODE_CONSERVATIVE,
    };
    MODE.store(value, Ordering::Relaxed);
}

/// 0: not read yet; 1: off; n + 1: collect at every n-th safepoint.
static STRESS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Safepoints this thread passed since its last stress collection.
    static STRESS_COUNT: Cell<usize> = const { Cell::new(0) };
}

/// Returns whether stress mode is on: safepoints (allocations and polls)
/// collect. [`set_stress`], else `CODIRA_GC_STRESS=<n>` (collect at every
/// n-th safepoint of a thread; `1` is every one).
pub fn stress() -> bool {
    stress_period() != 0
}

/// The stress period, 0 when off.
fn stress_period() -> usize {
    match STRESS.load(Ordering::Relaxed) {
        0 => {
            let period = std::env::var("CODIRA_GC_STRESS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            set_stress(period);
            period
        }
        value => value - 1,
    }
}

/// Returns true if stress mode wants this safepoint of the current thread
/// to collect.
pub(crate) fn stress_due() -> bool {
    let period = stress_period();
    if period == 0 {
        return false;
    }
    STRESS_COUNT.with(|count| {
        let next = count.get() + 1;
        if next >= period {
            count.set(0);
            true
        } else {
            count.set(next);
            false
        }
    })
}

type StressCollector = std::sync::Arc<dyn Fn() + Send + Sync>;

static STRESS_COLLECTOR: parking_lot::Mutex<Option<StressCollector>> =
    parking_lot::const_mutex(None);

/// Sets what a poll calls to collect in stress mode (the runtime's
/// collector).
pub fn set_stress_collector(collect: Option<StressCollector>) {
    *STRESS_COLLECTOR.lock() = collect;
}

pub(crate) fn stress_collector() -> Option<StressCollector> {
    STRESS_COLLECTOR.lock().clone()
}

/// Sets stress mode: collect at every `period`-th safepoint of a thread
/// (0 turns it off).
pub fn set_stress(period: usize) {
    STRESS.store(period + 1, Ordering::Relaxed);
    super::safepoint::refresh_poll_word();
}

/// The roots a stopped thread's stack holds.
#[derive(Default, Debug)]
pub(crate) struct StackRoots {
    /// Words of the stack that are not inside a precise frame: may or may
    /// not be references.
    pub conservative: Vec<usize>,
    /// Words of frame records' slots: hold references, or pointers that are
    /// not GC references, or zero -- never integers that look like
    /// references.
    pub precise: Vec<usize>,
}

/// One parked thread as the collector sees it.
pub(crate) struct ParkedStack {
    /// The frozen range `[low, high)` of its stack.
    pub low: usize,
    pub high: usize,
    /// Its innermost frame record.
    pub head: *const FrameRecord,
    /// It parked at a poll: scan it conservatively throughout.
    pub conservative: bool,
}

/// Reads `[from, to)` word by word.
///
/// # Safety
///
/// The range must be mapped and readable.
unsafe fn push_words(out: &mut Vec<usize>, from: usize, to: usize) {
    let word = std::mem::size_of::<usize>();
    let mut address = (from + word - 1) & !(word - 1);
    while address + word <= to {
        out.push(std::ptr::read_volatile(address as *const usize));
        address += word;
    }
}

impl ParkedStack {
    /// Collects the roots of this stack into `out`.
    ///
    /// # Safety
    ///
    /// The thread must be parked, so `[low, high)` and every record in it
    /// are frozen.
    pub(crate) unsafe fn roots(&self, mode: RootMode, out: &mut StackRoots) {
        let word = std::mem::size_of::<usize>();
        let mut skip: Vec<(usize, usize)> = Vec::new();
        let mut record = self.head;
        let mut walked = 0usize;
        while !record.is_null() {
            let address = record as usize;
            // A record lives in a frame of this stack, above the park
            // point. Anything else means the chain is not what the compiler
            // promised: stop trusting it (everything is scanned
            // conservatively anyway).
            if address < self.low || address + std::mem::size_of::<FrameRecord>() > self.high {
                skip.clear();
                break;
            }
            let header = std::ptr::read_volatile(std::ptr::addr_of!((*record).header));
            if header >> 48 != MAGIC {
                skip.clear();
                break;
            }
            let regions = ((header >> 24) & 0xFF_FFFF) as usize;
            let words = (header & 0xFF_FFFF) as usize;
            let base = address + std::mem::size_of::<FrameRecord>();
            for index in 0..regions {
                let descriptor = base + index * 2 * word;
                let start = std::ptr::read_volatile(descriptor as *const usize);
                let size = std::ptr::read_volatile((descriptor + word) as *const usize);
                if start >= self.low && start.saturating_add(size) <= self.high {
                    push_words(&mut out.precise, start, start + size);
                }
            }
            let spill = base + regions * 2 * word;
            if spill + words * word > self.high {
                skip.clear();
                break;
            }
            push_words(&mut out.precise, spill, spill + words * word);

            let lo = std::ptr::read_volatile(std::ptr::addr_of!((*record).lo));
            let hi = std::ptr::read_volatile(std::ptr::addr_of!((*record).hi));
            if hi != 0 && lo >= self.low && hi <= self.high && lo + SAVED_REGISTERS < hi {
                skip.push((lo, hi - SAVED_REGISTERS));
            }
            record = std::ptr::read_volatile(std::ptr::addr_of!((*record).prev));
            walked += 1;
            if walked > (self.high - self.low) / std::mem::size_of::<FrameRecord>() {
                // A cycle: not a chain the compiler built.
                skip.clear();
                break;
            }
        }
        if mode == RootMode::Conservative || self.conservative || !cfg!(all(target_arch = "x86_64", not(windows))) {
            skip.clear();
        }
        skip.sort_unstable();
        let mut cursor = self.low;
        for (start, end) in skip {
            if start > cursor {
                push_words(&mut out.conservative, cursor, start);
            }
            cursor = cursor.max(end);
        }
        if cursor < self.high {
            push_words(&mut out.conservative, cursor, self.high);
        }
    }
}
