//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Asynchronous preemption of compiled code for a stop: a collector that
//!   has waited more than [`PREEMPT_AFTER`] for a running thread sends it
//!   `SIGURG`; the handler parks the thread if the signal interrupted code
//!   of a loaded Codira library, and does nothing otherwise.
//!
//! # Why
//!
//! Loops nothing bounds poll on their back-edges, but calls do not: a poll
//! at every function entry cost recursive integer code such as `fib` 30% or
//! more (its body is a few cycles per call). Recursion that neither
//! allocates nor loops would then hold up a collection until it returned.
//! Instead the collector interrupts such a thread, as Go does since 1.14.
//!
//! # Why parking in the handler is safe
//!
//! - The handler parks only when the interrupted instruction is inside the
//!   executable segments of a loaded Codira library. Compiled code never
//!   holds a runtime lock (it takes locks only inside calls into the
//!   runtime, whose code is not in those segments), so the locks parking
//!   takes cannot be held by the interrupted code.
//! - The thread parks as at a poll: its whole stack is scanned
//!   conservatively, the signal frame with every register of the
//!   interrupted code included (the handler runs on the thread's own
//!   stack; it is installed without `SA_ONSTACK`). Frame records may be
//!   half built at an arbitrary instruction; the scan validates what it
//!   reads from them, and skips nothing in a thread parked this way.
//! - Code of a library that also links C code (native bitcode) is in the
//!   same segments; such code that holds a lock of its own while being
//!   interrupted only waits longer, since the collector does not need it.
//!
//! Only on Linux (x86-64 and AArch64), where the interrupted instruction can
//! be read from the signal context. Elsewhere nothing is interrupted, and
//! recursion without loops or allocations holds up a collection.

use std::sync::atomic::{AtomicUsize, Ordering};

/// How long a stop waits for a running thread before interrupting it.
pub(crate) const PREEMPT_AFTER: std::time::Duration = std::time::Duration::from_millis(1);

/// Executable ranges of loaded libraries: `[lo, hi)` pairs, `0, 0` when
/// free. Fixed-size and lock-free, because the signal handler reads it.
const SLOTS: usize = 512;

#[allow(clippy::declare_interior_mutable_const)]
const FREE: AtomicUsize = AtomicUsize::new(0);
static LOWS: [AtomicUsize; SLOTS] = [FREE; SLOTS];
static HIGHS: [AtomicUsize; SLOTS] = [FREE; SLOTS];

/// Returns true if `pc` is in the code of a loaded Codira library.
fn in_codira_code(pc: usize) -> bool {
    (0..SLOTS).any(|slot| {
        let hi = HIGHS[slot].load(Ordering::Acquire);
        hi != 0 && pc < hi && pc >= LOWS[slot].load(Ordering::Acquire)
    })
}

/// The slots of one library's code ranges; frees them when dropped.
#[derive(Debug, Default)]
pub(crate) struct CodeRanges(Vec<usize>);

impl Drop for CodeRanges {
    fn drop(&mut self) {
        for &slot in &self.0 {
            HIGHS[slot].store(0, Ordering::Release);
            LOWS[slot].store(0, Ordering::Release);
        }
    }
}

/// Serializes registration (the handler only reads).
static REGISTER: parking_lot::Mutex<()> = parking_lot::const_mutex(());

fn register_range(lo: usize, hi: usize, ranges: &mut CodeRanges) {
    let _register = REGISTER.lock();
    if let Some(slot) = (0..SLOTS).find(|&slot| HIGHS[slot].load(Ordering::Acquire) == 0) {
        LOWS[slot].store(lo, Ordering::Release);
        HIGHS[slot].store(hi, Ordering::Release);
        ranges.0.push(slot);
    }
    // Out of slots: that library's code is just never interrupted.
}

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod imp {
    use super::{in_codira_code, register_range, CodeRanges};

    /// Registers the executable segments of the loaded object that contains
    /// `address` (a library's poll byte), and installs the handler.
    pub(crate) fn register_library_of(address: usize) -> CodeRanges {
        struct Search {
            address: usize,
            ranges: Vec<(usize, usize)>,
        }
        unsafe extern "C" fn visit(
            info: *mut libc::dl_phdr_info,
            _size: libc::size_t,
            data: *mut libc::c_void,
        ) -> libc::c_int {
            let search = &mut *data.cast::<Search>();
            let info = &*info;
            let base = info.dlpi_addr as usize;
            let headers =
                std::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum));
            let contains = headers.iter().any(|h| {
                h.p_type == libc::PT_LOAD && {
                    let lo = base + h.p_vaddr as usize;
                    search.address >= lo && search.address < lo + h.p_memsz as usize
                }
            });
            if !contains {
                return 0;
            }
            for h in headers {
                if h.p_type == libc::PT_LOAD && h.p_flags & libc::PF_X != 0 {
                    let lo = base + h.p_vaddr as usize;
                    search.ranges.push((lo, lo + h.p_memsz as usize));
                }
            }
            1
        }
        let mut search = Search {
            address,
            ranges: Vec::new(),
        };
        // Safety: the callback only reads the program headers it is given.
        unsafe {
            libc::dl_iterate_phdr(Some(visit), std::ptr::addr_of_mut!(search).cast());
        }
        let mut ranges = CodeRanges::default();
        for (lo, hi) in search.ranges {
            register_range(lo, hi, &mut ranges);
        }
        install();
        ranges
    }

    static INSTALL: std::sync::Once = std::sync::Once::new();

    fn install() {
        INSTALL.call_once(|| {
            // Safety: a plain `sigaction` with a handler that only reads
            // atomics and the signal context before deciding to park.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handler as *const () as usize;
                action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(libc::SIGURG, &action, std::ptr::null_mut());
            }
        });
    }

    extern "C" fn handler(_signal: libc::c_int, _info: *mut libc::siginfo_t, context: *mut libc::c_void) {
        // Safety: the kernel passes a valid `ucontext_t`.
        let pc = unsafe { interrupted_pc(&*context.cast::<libc::ucontext_t>()) };
        if in_codira_code(pc) {
            // Keep errno for the interrupted code (parking may set it).
            let errno = unsafe { *libc::__errno_location() };
            crate::gc::safepoint::park_preempted();
            unsafe { *libc::__errno_location() = errno };
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn interrupted_pc(context: &libc::ucontext_t) -> usize {
        context.uc_mcontext.gregs[libc::REG_RIP as usize] as usize
    }

    #[cfg(target_arch = "aarch64")]
    fn interrupted_pc(context: &libc::ucontext_t) -> usize {
        context.uc_mcontext.pc as usize
    }

    /// The calling thread's handle, for [`interrupt`].
    pub(crate) fn current_thread() -> usize {
        // Safety: always callable.
        unsafe { libc::pthread_self() as usize }
    }

    /// Sends the preemption signal to `thread` (a [`current_thread`] of a
    /// registered, so still alive, thread).
    pub(crate) fn interrupt(thread: usize) {
        if thread != 0 {
            // Safety: the thread is alive (it is registered).
            unsafe { libc::pthread_kill(thread as libc::pthread_t, libc::SIGURG) };
        }
    }
}

#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
mod imp {
    use super::CodeRanges;

    pub(crate) fn register_library_of(_address: usize) -> CodeRanges {
        CodeRanges::default()
    }

    pub(crate) fn current_thread() -> usize {
        0
    }

    pub(crate) fn interrupt(_thread: usize) {}
}

pub(crate) use imp::{current_thread, interrupt, register_library_of};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_register_and_free() {
        let mut ranges = CodeRanges::default();
        register_range(0x1000_0000, 0x1000_1000, &mut ranges);
        assert!(in_codira_code(0x1000_0800));
        assert!(!in_codira_code(0x1000_1000));
        drop(ranges);
        assert!(!in_codira_code(0x1000_0800));
    }
}
