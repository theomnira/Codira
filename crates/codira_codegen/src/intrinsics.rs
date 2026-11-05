//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::ffi;

use inkwell::{context::Context, targets::TargetData, types::FunctionType};

use crate::ir::dispatch_table::FunctionPrototype;

#[macro_use]
mod macros;

/// Defines the properties of an intrinsic function that can be called from
/// Codira. These functions are mostly used internally.
pub trait Intrinsic: Sync {
    /// Returns the prototype of the intrinsic
    fn prototype(&self) -> FunctionPrototype;

    /// Returns the IR type for the function
    fn ir_type<'ink>(&self, context: &'ink Context, target: &TargetData) -> FunctionType<'ink>;
}

intrinsics! {
    /// Allocates memory for the specified `type` in the allocator referred to by `alloc_handle`.
    pub fn new(type_handle: *const ffi::c_void, alloc_handle: *mut ffi::c_void) -> *const *mut ffi::c_void;

    /// Allocates memory for an array of the specified `type` in the allocator referred to by
    /// `alloc_handle` with at least enough capacity to hold `length` elements.
    ///
    /// Note that the elements in the array are left uninitialized.
    pub fn new_array(type_handle: *const ffi::c_void, length: usize, alloc_handle: *mut ffi::c_void) -> *const *mut ffi::c_void;
}

runtime_functions! {
    /// Writes `len` bytes of UTF-8 starting at `data` to standard output.
    pub fn print_str(data: *const u8, len: usize) -> () = "codira.print_str";

    /// Writes a signed integer in decimal.
    pub fn print_i64(value: i64) -> () = "codira.print_i64";

    /// Writes an unsigned integer in decimal.
    pub fn print_u64(value: u64) -> () = "codira.print_u64";

    /// Writes a 128-bit signed integer, passed as its low and high halves
    /// because a 128-bit integer has no portable C calling convention.
    pub fn print_i128(low: u64, high: u64) -> () = "codira.print_i128";

    /// Writes a 128-bit unsigned integer, passed as its low and high halves.
    pub fn print_u128(low: u64, high: u64) -> () = "codira.print_u128";

    /// Writes an `f32` in its shortest round-trip form.
    pub fn print_f32(value: f32) -> () = "codira.print_f32";

    /// Writes an `f64` in its shortest round-trip form.
    pub fn print_f64(value: f64) -> () = "codira.print_f64";

    /// Writes `true` or `false`. Passed as a byte rather than an `i1`: the C
    /// ABI only defines the low byte of a `bool` argument, and LLVM makes no
    /// promise about the rest of the register for an `i1`.
    pub fn print_bool(value: u8) -> () = "codira.print_bool";

    /// Called by fuel-instrumented code when the fuel counter drops below
    /// zero. The runtime ends the call (see `codira_runtime`); if a host's
    /// handler returns, execution continues with the counter still negative.
    pub fn fuel_exhausted() -> () = "codira.fuel_exhausted";
}

runtime_functions! {
    /// Opens a `scope`: returns a handle that owns the scope's tasks and
    /// channels.
    pub fn scope_begin() -> usize = "codira.scope_begin";

    /// Runs `entry(copy of args[..size])` as a task of `scope`.
    pub fn scope_spawn(scope: usize, entry: usize, args: usize, size: usize) -> () = "codira.scope_spawn";

    /// Waits for every task of `scope`, then frees its channels and the
    /// scope itself.
    pub fn scope_end(scope: usize) -> () = "codira.scope_end";

    /// Creates a channel of `capacity` elements of `element_size` bytes,
    /// owned by `scope`.
    pub fn channel_new(scope: usize, capacity: usize, element_size: usize) -> usize = "codira.channel_new";

    /// Sends the element at address `value`, waiting while the channel is full.
    pub fn channel_send(channel: usize, value: usize) -> () = "codira.channel_send";

    /// Receives the next element into address `out`, waiting while there is none.
    pub fn channel_receive(channel: usize, out: usize) -> () = "codira.channel_receive";

    /// Performs one of `count` channel operations laid out at `cases` (three
    /// `usize`s each: channel, 0 = receive / 1 = send, data address) and
    /// returns its index; returns `count` at once if `has_default` is set and
    /// none can proceed.
    pub fn channel_select(cases: usize, count: usize, has_default: usize) -> usize = "codira.channel_select";

    /// Creates the two endpoints of a session in `scope`, writing their
    /// handles to the two `usize`s at `out`.
    pub fn session_open(scope: usize, out: usize) -> () = "codira.session_open";

    /// Sends the `size` bytes at `value` to the other endpoint.
    pub fn session_send(endpoint: usize, value: usize, size: usize) -> () = "codira.session_send";

    /// Receives the other endpoint's next message into the `size` bytes at `out`.
    pub fn session_receive(endpoint: usize, out: usize, size: usize) -> () = "codira.session_receive";
}

/// Every runtime function `scope`, `spawn` and channels lower to, collected
/// together like [`print_functions`].
pub fn task_functions() -> [&'static dyn Intrinsic; 10] {
    [
        &scope_begin,
        &scope_spawn,
        &scope_end,
        &channel_new,
        &channel_send,
        &channel_receive,
        &channel_select,
        &session_open,
        &session_send,
        &session_receive,
    ]
}

runtime_functions! {
    /// Reports a panic -- `message` at `file:line:column` -- and terminates
    /// the program. Never returns.
    pub fn panic(message: *const u8, message_len: usize, file: *const u8, file_len: usize, line: u32, column: u32) -> () = "codira.panic";

    /// Compares two strings' bytes: 1 if equal, 0 if not.
    pub fn str_eq(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> u8 = "codira.str_eq";

    /// Reports an out-of-range index into an array of length `len`, at
    /// `file:line:column`, and terminates the program. Never returns. The
    /// index travels as 64 bits plus its signedness, so `-1` reads as `-1`.
    pub fn panic_index_out_of_bounds(index: u64, index_is_signed: u8, len: u64, file: *const u8, file_len: usize, line: u32, column: u32) -> () = "codira.panic_index_out_of_bounds";
}

/// Every runtime function a `print`/`println` call may need, collected
/// together so the dispatch table has all of them whenever a module prints
/// anything at all. Which ones a given call uses depends on its argument
/// types; listing them all costs one pointer each and keeps the collection
/// pass from having to repeat code generation's type dispatch.
pub fn print_functions() -> [&'static dyn Intrinsic; 8] {
    [
        &print_str,
        &print_i64,
        &print_u64,
        &print_i128,
        &print_u128,
        &print_f32,
        &print_f64,
        &print_bool,
    ]
}

runtime_functions! {
    /// Makes room in `array` for at least `min_capacity` elements: moves its
    /// elements to a larger buffer (the capacity at least doubles). The
    /// array keeps its handle.
    pub fn array_grow(array: *const *mut ffi::c_void, min_capacity: usize, alloc_handle: *mut ffi::c_void) -> () = "codira.array_grow";

    /// Sorts the `len` elements at `data`, of the element kind `kind`, in
    /// place (unstable; floats by IEEE 754 total order).
    pub fn array_sort(data: *mut u8, len: usize, kind: u32) -> () = "codira.array_sort";

    /// Searches the sorted `len` elements at `data` for the value at `key`:
    /// an index of an equal element, or -1.
    pub fn array_binary_search(data: *mut u8, len: usize, kind: u32, key: *const u8) -> i64 = "codira.array_binary_search";

    /// Reverses the `len` elements of `stride` bytes at `data` in place.
    pub fn array_reverse(data: *mut u8, len: usize, stride: usize) -> () = "codira.array_reverse";
}

/// Every runtime function the array methods lower to (`push`, `sort`, ...),
/// collected together like [`print_functions`].
pub fn array_functions() -> [&'static dyn Intrinsic; 4] {
    [&array_grow, &array_sort, &array_binary_search, &array_reverse]
}
