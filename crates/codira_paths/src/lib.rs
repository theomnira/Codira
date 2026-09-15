//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
//! - Provides path abstraction types (`AbsPath`, `AbsPathBuf`, `RelativePath`,
//!   `RelativePathBuf`) that decouple the compiler from OS-specific path
//!   representations.
pub mod abs_path;

pub use abs_path::{AbsPath, AbsPathBuf};
pub use relative_path::{RelativePath, RelativePathBuf};
