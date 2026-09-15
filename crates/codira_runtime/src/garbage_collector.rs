//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_memory::gc;

/// Defines the garbage collector used by the `Runtime`.
pub type GarbageCollector = gc::MarkSweep<gc::NoopObserver<gc::Event>>;

pub type GcRootPtr = gc::GcRootPtr<GarbageCollector>;
