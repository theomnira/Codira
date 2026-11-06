//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The synchronization primitives of the Cascade epoch protocol: `std`
//!   normally, `loom`'s model-checked versions when built with
//!   `RUSTFLAGS="--cfg loom"`.

#[cfg(loom)]
pub(crate) use loom::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
#[cfg(not(loom))]
pub(crate) use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

/// Locks `mutex`, ignoring poisoning: the protected data is consistent after
/// every critical section (they never unwind half-way).
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> impl std::ops::DerefMut<Target = T> + '_ {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
