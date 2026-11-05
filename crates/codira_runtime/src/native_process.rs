//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - The termination hooks of `std/sys/terminate.code` (`panic`, `exit`,
//!   `abort`, `assert`) for programs the runtime loads: writing to
//!   standard error, aborting, exiting, and whether this is a debug build.
//! - The process's command-line arguments and environment, a byte at a
//!   time, for `std/sys/arg.code`.
//!
//! `codira_runtime_capi` exports the same four as C symbols, for a C host
//! that links it. A program run by `codira run` or `Runtime::builder` has no
//! such host: nothing in its process exports them, so without these
//! definitions in the dispatch table a program importing
//! `std.sys.terminate` fails to load ("missing dependencies"). They are
//! registered by name, not exported, so the two never clash at link time.
//!
//! `codira_abort` and `codira_exit` do not return: `std.sys.terminate`
//! declares them `-> Never`. They are registered with a unit return because
//! that is what the compiler makes of a `never` result: the function is
//! `void` (and `noreturn`) in LLVM, and its reflected signature names `()`
//! as the result, so the signature check at load time compares `()` with
//! `()`. That the call does not come back is in the caller's code (an
//! `unreachable` after it), not in the signature.

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

/// The process's command-line arguments as UTF-8 (a byte that is not is
/// replaced by U+FFFD), read once.
fn arguments() -> &'static [String] {
    static ARGS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    ARGS.get_or_init(|| {
        std::env::args_os()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    })
}

/// The number of command-line arguments (the program's name included), for
/// `std/sys/arg.code`.
extern "C" fn args_count() -> i64 {
    arguments().len() as i64
}

/// The byte length of argument `index`, or -1 when there is none.
extern "C" fn arg_len(index: i64) -> i64 {
    usize::try_from(index)
        .ok()
        .and_then(|index| arguments().get(index))
        .map_or(-1, |arg| arg.len() as i64)
}

/// Byte `at` of argument `index`, or -1 when either is out of range.
extern "C" fn arg_byte(index: i64, at: i64) -> i32 {
    let arg = usize::try_from(index).ok().and_then(|index| arguments().get(index));
    let at = usize::try_from(at).ok();
    match (arg, at) {
        (Some(arg), Some(at)) => arg.as_bytes().get(at).map_or(-1, |&b| i32::from(b)),
        _ => -1,
    }
}

/// The process's environment as `NAME=value` entries in UTF-8 (lossy),
/// read once: a variable set later is not seen.
fn environment() -> &'static [String] {
    static ENV: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    ENV.get_or_init(|| {
        std::env::vars_os()
            .map(|(name, value)| {
                format!("{}={}", name.to_string_lossy(), value.to_string_lossy())
            })
            .collect()
    })
}

/// The number of environment entries.
extern "C" fn env_count() -> i64 {
    environment().len() as i64
}

/// The byte length of environment entry `index`, or -1.
extern "C" fn env_entry_len(index: i64) -> i64 {
    usize::try_from(index)
        .ok()
        .and_then(|index| environment().get(index))
        .map_or(-1, |entry| entry.len() as i64)
}

/// Byte `at` of environment entry `index`, or -1.
extern "C" fn env_entry_byte(index: i64, at: i64) -> i32 {
    let entry = usize::try_from(index).ok().and_then(|index| environment().get(index));
    match (entry, usize::try_from(at).ok()) {
        (Some(entry), Some(at)) => entry.as_bytes().get(at).map_or(-1, |&b| i32::from(b)),
        _ => -1,
    }
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
        IntoFunctionDefinition::into(args_count as extern "C" fn() -> i64, "codira_args_count"),
        IntoFunctionDefinition::into(arg_len as extern "C" fn(i64) -> i64, "codira_arg_len"),
        IntoFunctionDefinition::into(
            arg_byte as extern "C" fn(i64, i64) -> i32,
            "codira_arg_byte",
        ),
        IntoFunctionDefinition::into(env_count as extern "C" fn() -> i64, "codira_env_count"),
        IntoFunctionDefinition::into(
            env_entry_len as extern "C" fn(i64) -> i64,
            "codira_env_entry_len",
        ),
        IntoFunctionDefinition::into(
            env_entry_byte as extern "C" fn(i64, i64) -> i32,
            "codira_env_entry_byte",
        ),
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
    fn the_non_returning_hooks_have_the_signature_of_a_never_result() {
        // The compiler gives a function returning `never` the result `()`
        // in its reflected signature; these must match it, or a program
        // calling them fails to load with a mismatched signature.
        let unit = <() as codira_memory::HasStaticType>::type_info().clone();
        for definition in functions() {
            let name = definition.prototype.name.as_str();
            if matches!(name, "codira_abort" | "codira_exit") {
                assert_eq!(definition.prototype.signature.return_type, unit, "{name}");
            }
        }
    }

    #[test]
    fn debug_flag_matches_this_build() {
        assert_eq!(is_debug(), cfg!(debug_assertions));
    }
}
