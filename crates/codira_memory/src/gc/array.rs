//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
/// An array in Codira is represented in memory by a header followed by the rest
/// of the bytes.
#[repr(C)]
pub struct ArrayHeader {
    pub length: usize,
    pub capacity: usize,
}
