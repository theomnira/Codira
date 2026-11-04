//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - The platform half of the conservative stack scan: capturing the
//!   callee-saved registers and finding the base of the current thread's
//!   stack. `gc::safepoint` uses them to record the stack range of every
//!   thread that parks for a collection.
//!
//! Generated code has no stack maps, so the collector cannot know which stack
//! slots hold handles. Treating every word that looks like a pointer into a
//! live object as a root is safe as long as objects never move (they don't:
//! handles are pinned headers) and every thread that may hold references is
//! scanned; `gc::safepoint` makes sure of that.

/// Calls `f` with the value of every callee-saved register and every
/// pointer-aligned word between this function's frame and the base of the
/// current thread's stack. Returns `false` (without calling `f`) when the
/// platform is not supported.
#[cfg(test)]
#[inline(never)]
pub(crate) fn scan_current_thread(f: &mut dyn FnMut(usize)) -> bool {
    let Some(high) = stack_base() else {
        return false;
    };
    let mut registers = [0usize; 16];
    if !capture_callee_saved_registers(&mut registers) {
        return false;
    }
    for &register in &registers {
        f(register);
    }

    // Every caller frame lives above this one. Callee-saved registers that
    // this function itself uses were spilled by its prologue, at addresses
    // above `registers`, so they are covered by the scan as well.
    let word = std::mem::size_of::<usize>();
    let mut address = (std::ptr::addr_of!(registers) as usize) & !(word - 1);
    while address + word <= high {
        // Safety: `[address, high)` is the live part of the current thread's
        // stack, which is mapped and readable.
        f(unsafe { std::ptr::read_volatile(address as *const usize) });
        address += word;
    }
    std::hint::black_box(&registers);
    true
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) fn capture_callee_saved_registers(out: &mut [usize; 16]) -> bool {
    // rbx, rbp, r12-r15 are callee-saved on every x86-64 ABI; rdi and rsi are
    // also callee-saved on Windows (storing them elsewhere is harmless).
    unsafe {
        std::arch::asm!(
            "mov [{p}], rbx",
            "mov [{p} + 8], rbp",
            "mov [{p} + 16], r12",
            "mov [{p} + 24], r13",
            "mov [{p} + 32], r14",
            "mov [{p} + 40], r15",
            "mov [{p} + 48], rdi",
            "mov [{p} + 56], rsi",
            p = in(reg) out.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    }
    true
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn capture_callee_saved_registers(out: &mut [usize; 16]) -> bool {
    unsafe {
        std::arch::asm!(
            "stp x19, x20, [{p}]",
            "stp x21, x22, [{p}, #16]",
            "stp x23, x24, [{p}, #32]",
            "stp x25, x26, [{p}, #48]",
            "stp x27, x28, [{p}, #64]",
            "stp x29, x30, [{p}, #80]",
            p = in(reg) out.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    }
    true
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(crate) fn capture_callee_saved_registers(_out: &mut [usize; 16]) -> bool {
    false
}

/// Returns the highest address of the current thread's stack.
#[cfg(windows)]
pub(crate) fn stack_base() -> Option<usize> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadStackLimits(low_limit: *mut usize, high_limit: *mut usize);
    }
    let (mut low, mut high) = (0usize, 0usize);
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    (high > low).then_some(high)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn stack_base() -> Option<usize> {
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut address: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let result = libc::pthread_attr_getstack(&attr, &mut address, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        (result == 0).then(|| address as usize + size)
    }
}

#[cfg(target_vendor = "apple")]
pub(crate) fn stack_base() -> Option<usize> {
    let base = unsafe { libc::pthread_get_stackaddr_np(libc::pthread_self()) } as usize;
    (base != 0).then_some(base)
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
)))]
pub(crate) fn stack_base() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_a_value_held_by_a_caller() {
        let marker = std::hint::black_box(0x5eed_c0de_usize * 7);
        let mut seen = false;
        let supported = scan_current_thread(&mut |word| seen |= word == marker);
        std::hint::black_box(marker);
        if supported {
            assert!(seen);
        }
    }
}
