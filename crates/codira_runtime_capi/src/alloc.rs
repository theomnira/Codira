//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: September 21, 2026
//!
//! Functionality:
//! - Raw, GC-independent allocation for `std/memory/unsafe_pointer.code` and
//!   `std/memory/pointer.code`.
//!
//! # Why this exists
//!
//! `codira_runtime_capi::gc` already allocates memory, but through
//! `codira_gc_alloc`, which needs a live `Runtime` handle and a registered
//! type to hand back a GC-tracked, collectable object. `UnsafePointer[T]`'s
//! `alloc`/`free` are the opposite of that on purpose -- a raw allocation
//! with a manual lifetime, for building the GC-independent collections
//! (`List`, `Dict`, arena buffers) underneath. Threading a `Runtime` handle
//! through every element store in every collection to satisfy an API that
//! does not want GC tracking at all would be exactly backwards.
//!
//! This is Rust's global allocator, exposed directly. No bookkeeping beyond
//! what `std::alloc` itself does -- the caller is who tracks the size and
//! alignment to free correctly, same as it would calling `malloc`/`free` in
//! C, which is the contract `UnsafePointer[T]`'s own doc comment already
//! states ("`count` must match the original allocation size").

use std::alloc::Layout;

/// Builds the `Layout` for `size` bytes at `align`-byte alignment.
///
/// Both are Codira `usize` values, so a value the platform's allocator
/// cannot represent -- zero alignment, an alignment that is not a power of
/// two, or a size that would overflow when rounded up to it -- is a caller
/// bug, not a recoverable condition; `Layout::from_size_align` already
/// reports exactly that distinction.
fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align)
        .unwrap_or_else(|e| panic!("invalid allocation layout (size={size}, align={align}): {e}"))
}

/// Allocates `size` bytes at `align`-byte alignment. Contents are
/// uninitialized. Returns `0` (null) on allocation failure rather than
/// aborting, so the standard library's own null checks are what a caller
/// sees, not a crash inside the allocator.
///
/// # Safety
///
/// `size` and `align` must describe a layout `std::alloc::alloc` can
/// satisfy (see `Layout::from_size_align`); the returned address must later
/// be freed with `codira_dealloc` using the *same* size and alignment, or not
/// freed at all.
#[no_mangle]
pub unsafe extern "C" fn codira_alloc(size: usize, align: usize) -> usize {
    if size == 0 {
        // A zero-sized allocation is well-defined for Rust's allocator but
        // its returned address is a dangling, non-null sentinel -- exactly
        // the kind of address `UnsafePointer` callers test against zero to
        // mean "empty" or "unallocated". Returning null here is what keeps
        // that check meaningful.
        return 0;
    }
    // SAFETY: forwarded from the caller's own safety contract.
    unsafe { std::alloc::alloc(layout(size, align)) as usize }
}

/// Allocates `size` zero-initialized bytes at `align`-byte alignment.
///
/// Same contract as `codira_alloc`.
///
/// # Safety
///
/// Same as `codira_alloc`: `size` and `align` must describe a layout
/// `std::alloc::alloc_zeroed` can satisfy, and the returned address must
/// later be freed with `codira_dealloc` using the same size and alignment,
/// or not freed at all.
#[no_mangle]
pub unsafe extern "C" fn codira_alloc_zeroed(size: usize, align: usize) -> usize {
    if size == 0 {
        return 0;
    }
    // SAFETY: forwarded from the caller's own safety contract.
    unsafe { std::alloc::alloc_zeroed(layout(size, align)) as usize }
}

/// Frees `size` bytes at `align`-byte alignment previously returned by
/// `codira_alloc`/`codira_alloc_zeroed`. A null `addr` is a no-op.
///
/// # Safety
///
/// `addr`, `size` and `align` must exactly match a prior, still-live
/// `codira_alloc`/`codira_alloc_zeroed` call; freeing the same address twice,
/// with a mismatched size or alignment, or one this allocator did not
/// return, is undefined behavior -- the same contract C's `free` has.
#[no_mangle]
pub unsafe extern "C" fn codira_dealloc(addr: usize, size: usize, align: usize) {
    if addr == 0 || size == 0 {
        return;
    }
    // SAFETY: forwarded from the caller's own safety contract.
    unsafe { std::alloc::dealloc(addr as *mut u8, layout(size, align)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_alloc_and_dealloc() {
        unsafe {
            let addr = codira_alloc(64, 8);
            assert_ne!(addr, 0);
            // Writable and readable across the full requested extent.
            let slice = std::slice::from_raw_parts_mut(addr as *mut u8, 64);
            slice.fill(0xAB);
            assert!(slice.iter().all(|&b| b == 0xAB));
            codira_dealloc(addr, 64, 8);
        }
    }

    #[test]
    fn zeroed_allocation_is_actually_zero() {
        unsafe {
            let addr = codira_alloc_zeroed(128, 16);
            assert_ne!(addr, 0);
            let slice = std::slice::from_raw_parts(addr as *const u8, 128);
            assert!(slice.iter().all(|&b| b == 0));
            codira_dealloc(addr, 128, 16);
        }
    }

    #[test]
    fn zero_size_allocation_returns_null_rather_than_a_dangling_sentinel() {
        unsafe {
            assert_eq!(codira_alloc(0, 8), 0);
            assert_eq!(codira_alloc_zeroed(0, 8), 0);
        }
    }

    #[test]
    fn freeing_null_is_a_no_op() {
        unsafe {
            codira_dealloc(0, 64, 8);
        }
    }
}
