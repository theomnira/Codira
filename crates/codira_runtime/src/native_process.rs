//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - The termination hooks of `std/sys/terminate.code` (`panic`, `exit`,
//!   `abort`, `assert`) for programs the runtime loads: writing to
//!   standard error, aborting, exiting, and whether this is a debug build.
//!
//! `codira_runtime_capi` exports the same four as C symbols, for a C host
//! that links it. A program run by `codira run` or `Runtime::builder` has no
//! such host: nothing in its process exports them, so without these
//! definitions in the dispatch table a program importing
//! `std.sys.terminate` fails to load ("missing dependencies"). They are
//! registered by name, not exported, so the two never clash at link time.
//!
//! `codira_abort` and `codira_exit` do not return. They are registered with
//! a unit return because the dispatch table describes a call's ABI, and a
//! call that never returns has the ABI of one that returns nothing.

use std::io::Write;

use crate::{FunctionDefinition, IntoFunctionDefinition};

/// Writes `count` bytes from `buf` to standard error and flushes. A null
/// `buf` or a zero `count` writes nothing; a failed write is ignored (this
/// is called while terminating, and there is nowhere left to report it).
///
/// The program passes `count` readable bytes at `buf` (a `str`'s data and
/// length), as for every address the runtime is handed (`native_hash`).
extern "C" fn write_stderr(buf: usize, count: usize) {
    if buf == 0 || count == 0 {
        return;
    }
    // Safety: the caller guarantees `count` readable bytes at `buf`.
    let bytes = unsafe { std::slice::from_raw_parts(buf as *const u8, count) };
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(bytes);
    let _ = stderr.flush();
}

/// Terminates the process abnormally, without unwinding or flushing.
extern "C" fn abort() {
    std::process::abort()
}

/// Terminates the process with `code`, running the usual at-exit handling.
extern "C" fn exit(code: i32) {
    std::process::exit(code)
}

/// Whether the runtime was built with debug assertions.
extern "C" fn is_debug() -> bool {
    cfg!(debug_assertions)
}

/// Returns the definitions of every function in this module, for the
/// runtime's dispatch table.
pub(crate) fn functions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(
            write_stderr as extern "C" fn(usize, usize),
            "codira_write_stderr",
        ),
        IntoFunctionDefinition::into(abort as extern "C" fn(), "codira_abort"),
        IntoFunctionDefinition::into(exit as extern "C" fn(i32), "codira_exit"),
        IntoFunctionDefinition::into(is_debug as extern "C" fn() -> bool, "codira_is_debug"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writing_nothing_is_not_a_fault() {
        // Null or empty, which the function checks before reading.
        write_stderr(0, 0);
        write_stderr(0, 16);
        write_stderr(b"ignored".as_ptr() as usize, 0);
    }

    #[test]
    fn debug_flag_matches_this_build() {
        assert_eq!(is_debug(), cfg!(debug_assertions));
    }
}
