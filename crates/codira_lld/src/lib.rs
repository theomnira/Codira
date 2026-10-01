//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 7, 2026
//!
//! Functionality:
//! - A wrapper around LLD in Rust Programming Language.

use std::{
    ffi::{CStr, CString},
    os::raw::{c_char, c_int},
};

#[repr(C)]
struct LldInvokeResult {
    success: bool,
    can_run_again: bool,
    messages: *const c_char,
}

#[repr(C)]
pub enum LldFlavor {
    Elf = 0,
    Wasm = 1,
    MachO = 2,
    Coff = 3,
}

extern "C" {
    fn codira_lld_link(
        flavor: LldFlavor,
        argc: c_int,
        argv: *const *const c_char,
    ) -> LldInvokeResult;
    fn codira_link_free_result(result: *mut LldInvokeResult);
}

pub enum LldError {
    StringConversionError,
}

pub struct LldResult {
    success: bool,
    can_run_again: bool,
    messages: String,
}

impl LldResult {
    /// Whether LLD may be invoked again in this process.
    ///
    /// LLD's drivers keep global state, which `lld::lldMain` tears down after
    /// every link. When that teardown cannot be trusted -- LLD caught a crash
    /// in its recovery context, say -- it reports so here, and any further
    /// in-process link would run on corrupted state. A long-lived caller must
    /// check this after *every* call, successful or not, and stop calling
    /// `link` for the rest of the process once it is `false`.
    pub fn can_run_again(&self) -> bool {
        self.can_run_again
    }

    pub fn ok(self) -> Result<(), String> {
        if self.success {
            Ok(())
        } else {
            Err(self.messages)
        }
    }
}

/// Invokes LLD of the given flavor with the specified arguments.
///
/// Calls are serialized: LLD is not thread-safe, so concurrent callers queue
/// behind a process-wide mutex inside the wrapper.
pub fn link(target: LldFlavor, args: &[String]) -> LldResult {
    // Prepare arguments. An argument with an interior NUL cannot be passed
    // to LLD at all; LLD was not run, so it is safe to run again.
    let Ok(c_args) = args
        .iter()
        .map(|arg| CString::new(arg.as_bytes()))
        .collect::<Result<Vec<CString>, _>>()
    else {
        return LldResult {
            success: false,
            can_run_again: true,
            messages: String::from("linker argument contains an interior NUL byte"),
        };
    };
    let args: Vec<*const c_char> = c_args.iter().map(|arg| arg.as_ptr()).collect();

    // Invoke LLD
    let mut lld_result = unsafe { codira_lld_link(target, args.len() as c_int, args.as_ptr()) };

    // Get the messages from the invocation
    let messages = if lld_result.messages.is_null() {
        String::new()
    } else {
        unsafe {
            CStr::from_ptr(lld_result.messages)
                .to_string_lossy()
                .to_string()
        }
    };

    // Construct the result
    let result = LldResult {
        success: lld_result.success,
        can_run_again: lld_result.can_run_again,
        messages,
    };

    // Release the result
    unsafe { codira_link_free_result(&mut lld_result as *mut LldInvokeResult) };
    // `LldInvokeResult` is a plain repr(C) struct with no Drop impl; the
    // message buffer it owned was already released by
    // `codira_link_free_result` above. The explicit drop was a no-op.

    result
}
