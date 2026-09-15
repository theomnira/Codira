//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
mod path_resolution;
mod per_ns;

pub use path_resolution::ReachedFixedPoint;

pub use self::per_ns::{Namespace, PerNs};
