//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - The runtime half of Codira's builtin functions: the `codira.print_*`
//!   functions a `print`/`println` call lowers to, registered in every
//!   runtime's dispatch table next to `new` and `new_array`.
//! - Output capture, so a host (or a test) can collect what Codira code printed
//!   on the current thread instead of having it go to stdout.
//!
//! The compiler side is `codira_codegen::intrinsics::print_functions`: each
//! name and signature here must match the prototype declared there, or
//! linking an assembly that prints fails with a signature mismatch.

use std::{cell::RefCell, io::Write};

use crate::{FunctionDefinition, IntoFunctionDefinition};

thread_local! {
    /// Output captured by [`capture_output`] on this thread, if capturing.
    static CAPTURE: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// Runs `f`, collecting everything Codira code prints on this thread while
/// it runs instead of writing it to standard output. Returns `f`'s result
/// together with the collected text.
///
/// Capture is per thread because Codira code runs on the thread that
/// invokes it: two tests printing concurrently each see only their own
/// output. Nested captures are not merged -- the inner one takes the output
/// it encloses.
pub fn capture_output<R>(f: impl FnOnce() -> R) -> (R, String) {
    let previous = CAPTURE.with(|c| c.borrow_mut().replace(Vec::new()));
    let result = f();
    let captured = CAPTURE.with(|c| std::mem::replace(&mut *c.borrow_mut(), previous));
    let bytes = captured.unwrap_or_default();
    (result, String::from_utf8_lossy(&bytes).into_owned())
}

/// Writes `bytes` to the capture buffer if one is active, otherwise to
/// standard output.
///
/// Write errors on stdout are ignored deliberately: a closed pipe
/// (`codira run app | head`) must not turn a `println` into a crash in the
/// middle of the program, which is what propagating it would require.
fn emit(bytes: &[u8]) {
    let captured = CAPTURE.with(|c| match c.borrow_mut().as_mut() {
        Some(buffer) => {
            buffer.extend_from_slice(bytes);
            true
        }
        None => false,
    });
    if !captured {
        let _ignored = std::io::stdout().lock().write_all(bytes);
    }
}

extern "C" fn print_str(data: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    // Safety: the compiler only passes the two halves of a `str` value, which
    // describe `len` initialized bytes of a string literal (or, later, a
    // live `String`'s buffer) for the duration of the call.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    emit(bytes);
}

extern "C" fn print_i64(value: i64) {
    emit(value.to_string().as_bytes());
}

extern "C" fn print_u64(value: u64) {
    emit(value.to_string().as_bytes());
}

extern "C" fn print_i128(low: u64, high: u64) {
    let value = ((u128::from(high) << 64) | u128::from(low)) as i128;
    emit(value.to_string().as_bytes());
}

extern "C" fn print_u128(low: u64, high: u64) {
    let value = (u128::from(high) << 64) | u128::from(low);
    emit(value.to_string().as_bytes());
}

/// Floats print in their shortest round-trip form, always with a decimal
/// point or exponent so `1.0` does not read back as an integer.
extern "C" fn print_f32(value: f32) {
    emit(format!("{value:?}").as_bytes());
}

extern "C" fn print_f64(value: f64) {
    emit(format!("{value:?}").as_bytes());
}

extern "C" fn print_bool(value: u8) {
    emit(if value != 0 { b"true" } else { b"false" });
}

/// Reads `len` bytes at `data` as text, for a message or file name the
/// compiler embedded in the program.
///
/// # Safety
///
/// `data` must point to `len` readable bytes.
pub(crate) unsafe fn embedded_str<'a>(data: *const u8, len: usize) -> std::borrow::Cow<'a, str> {
    if len == 0 {
        return std::borrow::Cow::Borrowed("");
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(data, len))
}

/// Ends the program with a panic report: `message`, at `file:line:column`.
///
/// What was printed so far is flushed first, so the report comes after it.
/// The process exits with status 101 -- the conventional panic status -- so
/// a script or test can tell a panic from an ordinary failure.
///
/// A panic is not catchable from Codira yet, and it does not unwind:
/// unwinding through compiled Codira frames would need unwind tables the
/// code generator does not promise today. What the language guarantees is
/// the part that matters most -- a bad index stops the program with its
/// location instead of reading or writing memory it does not own.
pub(crate) fn report_panic(message: &str, file: &str, line: u32, column: u32) -> ! {
    let _ignored = std::io::stdout().flush();
    eprintln!("panic at {file}:{line}:{column}: {message}");
    std::process::exit(101)
}

extern "C" fn panic(
    message: *const u8,
    message_len: usize,
    file: *const u8,
    file_len: usize,
    line: u32,
    column: u32,
) {
    // Safety: the compiler passes constant strings with their exact lengths.
    let (message, file) = unsafe {
        (
            embedded_str(message, message_len),
            embedded_str(file, file_len),
        )
    };
    report_panic(&message, &file, line, column)
}

extern "C" fn panic_index_out_of_bounds(
    index: u64,
    index_is_signed: u8,
    len: u64,
    file: *const u8,
    file_len: usize,
    line: u32,
    column: u32,
) {
    // Safety: the compiler passes a constant file name with its length.
    let file = unsafe { embedded_str(file, file_len) };
    let index = if index_is_signed != 0 {
        (index as i64).to_string()
    } else {
        index.to_string()
    };
    report_panic(
        &format!("index out of bounds: the length is {len} but the index is {index}"),
        &file,
        line,
        column,
    )
}

extern "C" fn str_eq(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> u8 {
    if a_len != b_len {
        return 0;
    }
    // An empty `str` may have a null data pointer (a zeroed enum slot does),
    // which a slice must not be built from.
    if a_len == 0 {
        return 1;
    }
    // Safety: both halves of each `str` describe `len` readable bytes.
    let (a, b) = unsafe { (embedded_str(a, a_len), embedded_str(b, b_len)) };
    u8::from(a == b)
}

/// The builtin functions every runtime registers, under the names the
/// compiler's `runtime_functions!` prototypes use.
pub(crate) fn definitions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(
            print_str as extern "C" fn(*const u8, usize),
            "codira.print_str",
        ),
        IntoFunctionDefinition::into(print_i64 as extern "C" fn(i64), "codira.print_i64"),
        IntoFunctionDefinition::into(print_u64 as extern "C" fn(u64), "codira.print_u64"),
        IntoFunctionDefinition::into(print_i128 as extern "C" fn(u64, u64), "codira.print_i128"),
        IntoFunctionDefinition::into(print_u128 as extern "C" fn(u64, u64), "codira.print_u128"),
        IntoFunctionDefinition::into(print_f32 as extern "C" fn(f32), "codira.print_f32"),
        IntoFunctionDefinition::into(print_f64 as extern "C" fn(f64), "codira.print_f64"),
        IntoFunctionDefinition::into(print_bool as extern "C" fn(u8), "codira.print_bool"),
        IntoFunctionDefinition::into(
            panic as extern "C" fn(*const u8, usize, *const u8, usize, u32, u32),
            "codira.panic",
        ),
        IntoFunctionDefinition::into(
            str_eq as extern "C" fn(*const u8, usize, *const u8, usize) -> u8,
            "codira.str_eq",
        ),
        IntoFunctionDefinition::into(
            panic_index_out_of_bounds as extern "C" fn(u64, u8, u64, *const u8, usize, u32, u32),
            "codira.panic_index_out_of_bounds",
        ),
    ]
}
