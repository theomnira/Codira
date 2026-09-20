//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: September 21, 2026
//!
//! Functionality:
//! - The hooks a panicking Codira program calls to report and terminate.

use std::io::Write;

/// Writes `count` bytes from `buf` to standard error.
///
/// # Why this is here and not in the standard library
///
/// `std/sys/terminate.code` needs to print a panic message, and there is no
/// way for Codira to reach standard error on its own. The portable C
/// spelling is `fwrite(buf, 1, count, stderr)`, and `stderr` is a macro:
/// there is no symbol of that name to declare `extern "C"`, so a Codira
/// declaration cannot name it. Going around it with a file descriptor means
/// `write` on POSIX and `_write` on Windows, which is a per-platform
/// declaration in a standard library that otherwise has none.
///
/// The runtime already owns platform I/O, so the hook lives here and is one
/// symbol on every target.
///
/// # Safety
///
/// `buf` must point to at least `count` readable bytes. A null `buf` or a
/// zero `count` writes nothing and returns, rather than being undefined:
/// an empty panic message is not a reason to fault inside the panic handler.
#[no_mangle]
pub unsafe extern "C" fn codira_write_stderr(buf: *const u8, count: usize) {
    if buf.is_null() || count == 0 {
        return;
    }

    // SAFETY: the caller guarantees `count` readable bytes at `buf`.
    let bytes = unsafe { std::slice::from_raw_parts(buf, count) };

    // A failed write is deliberately ignored. This is called while
    // terminating, and there is nowhere left to report that reporting
    // failed -- propagating it would only replace a panic message with a
    // different one.
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(bytes);
    let _ = stderr.flush();
}

/// Terminates the process abnormally, without unwinding or flushing.
///
/// Separate from `codira_write_stderr` so that a panic prints its message
/// before the process goes away: a single combined hook would have to decide
/// the message format, which belongs to the standard library.
#[no_mangle]
pub extern "C" fn codira_abort() -> ! {
    std::process::abort()
}

/// Terminates the process with `code`, running the usual at-exit handling.
#[no_mangle]
pub extern "C" fn codira_exit(code: i32) -> ! {
    std::process::exit(code)
}

/// Whether this build has debug assertions enabled.
///
/// Reported by the runtime rather than by a compiler intrinsic because it is
/// a property of the build the program is running *in*, and the runtime is
/// what that build produced.
#[no_mangle]
pub extern "C" fn codira_is_debug() -> bool {
    cfg!(debug_assertions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writing_nothing_is_not_a_fault() {
        // Called on the panic path, where faulting would replace the
        // message the user needs with a crash inside the reporter.
        unsafe {
            codira_write_stderr(std::ptr::null(), 0);
            codira_write_stderr(std::ptr::null(), 16);
            codira_write_stderr(b"ignored".as_ptr(), 0);
        }
    }

    #[test]
    fn debug_flag_matches_this_build() {
        assert_eq!(codira_is_debug(), cfg!(debug_assertions));
    }
}
