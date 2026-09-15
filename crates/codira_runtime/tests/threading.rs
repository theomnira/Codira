//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_runtime::Runtime;

// Ensures the [`Runtime`] is Send
trait IsSend: Send {}

#[allow(unused)]
impl IsSend for Runtime {}
