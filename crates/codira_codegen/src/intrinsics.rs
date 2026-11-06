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

    /// Opens a `group` whose tasks return `result_size` bytes each.
    pub fn group_begin(result_size: usize) -> usize = "codira.group_begin";

    /// Runs `entry(copy of args[..size], result slot)` as a task of `group`.
    pub fn group_spawn(group: usize, entry: usize, args: usize, size: usize) -> () = "codira.group_spawn";

    /// Waits for every task of `group`; returns how many it has.
    pub fn group_end(group: usize) -> usize = "codira.group_end";

    /// The lowest index of a task of `group` that returned an `Err`, or the
    /// task count.
    pub fn group_first_error(group: usize) -> usize = "codira.group_first_error";

    /// Copies the result of task `index` of `group` to address `out`.
    pub fn group_result(group: usize, index: usize, out: usize) -> () = "codira.group_result";

    /// Frees `group` and the results it kept.
    pub fn group_free(group: usize) -> () = "codira.group_free";

    /// 1 if the running task has been cancelled, 0 otherwise.
    pub fn task_cancelled() -> usize = "codira.task_cancelled";

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
pub fn task_functions() -> [&'static dyn Intrinsic; 17] {
    [
        &scope_begin,
        &scope_spawn,
        &scope_end,
        &group_begin,
        &group_spawn,
        &group_end,
        &group_first_error,
        &group_result,
        &group_free,
        &task_cancelled,
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

runtime_functions! {
    /// A new `String` of the `len` bytes at `data` (valid UTF-8), written to
    /// `out`. `ty` is the `[u8]` type handle, `alloc` the allocator handle.
    pub fn string_from_bytes(data: *const u8, len: usize, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_from_bytes";

    /// `buf/start/len + data[..data_len]`, appended in place when it can be.
    pub fn string_append(buf: *const ffi::c_void, start: usize, len: usize, data: *const u8, data_len: usize, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_append";

    /// A new `String` of `a` followed by `b`.
    pub fn string_concat(a: *const u8, a_len: usize, b: *const u8, b_len: usize, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_concat";

    /// 1 when the two strings' bytes are equal.
    pub fn string_eq(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> u8 = "codira.string_eq";

    /// -1, 0 or 1 by byte order.
    pub fn string_cmp(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> i64 = "codira.string_cmp";

    /// FNV-1a 64 of the bytes.
    pub fn string_hash(data: *const u8, len: usize) -> u64 = "codira.string_hash";

    /// The number of code points.
    pub fn string_char_count(data: *const u8, len: usize) -> usize = "codira.string_char_count";

    /// Byte offset of the first match, or -1.
    pub fn string_find(hay: *const u8, hay_len: usize, needle: *const u8, len: usize) -> i64 = "codira.string_find";

    /// Byte offset of the last match, or -1.
    pub fn string_rfind(hay: *const u8, hay_len: usize, needle: *const u8, len: usize) -> i64 = "codira.string_rfind";

    /// 1 when `needle` occurs in `hay`.
    pub fn string_contains(hay: *const u8, hay_len: usize, needle: *const u8, len: usize) -> u8 = "codira.string_contains";

    /// 1 when `s` starts with `p`.
    pub fn string_starts_with(s: *const u8, s_len: usize, p: *const u8, p_len: usize) -> u8 = "codira.string_starts_with";

    /// 1 when `s` ends with `p`.
    pub fn string_ends_with(s: *const u8, s_len: usize, p: *const u8, p_len: usize) -> u8 = "codira.string_ends_with";

    /// The pieces between occurrences of `sep`, as a `[String]` (`ty`);
    /// panics at the location given on an empty separator.
    pub fn string_split(buf: *const ffi::c_void, start: usize, len: usize, sep: *const u8, sep_len: usize, ty: *const ffi::c_void, alloc: *mut ffi::c_void, file: *const u8, file_len: usize, line: u32, column: u32) -> *const ffi::c_void = "codira.string_split";

    /// The lines, as a `[String]` (`ty`).
    pub fn string_lines(buf: *const ffi::c_void, start: usize, len: usize, ty: *const ffi::c_void, alloc: *mut ffi::c_void) -> *const ffi::c_void = "codira.string_lines";

    /// Trims (`mode` 0 both ends, 1 the start, 2 the end): returns the new
    /// length and writes the offset to `out_offset`.
    pub fn string_trim(data: *const u8, len: usize, mode: u8, out_offset: *mut u8) -> usize = "codira.string_trim";

    /// ASCII upper- (`upper` 1) or lower-casing into a new `String`.
    pub fn string_change_case(data: *const u8, len: usize, upper: u8, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_change_case";

    /// Every `from` replaced by `to`, into a new `String`.
    pub fn string_replace(data: *const u8, len: usize, from: *const u8, from_len: usize, to: *const u8, to_len: usize, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_replace";

    /// `count` copies (a negative signed count is none), into a new `String`.
    pub fn string_repeat(data: *const u8, len: usize, count: u64, signed: u8, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_repeat";

    /// Panics at the location given unless `start..end` is a valid slice of
    /// the string: in range, ordered, on character boundaries.
    pub fn string_check_slice(data: *const u8, len: usize, start: u64, start_signed: u8, end: u64, end_signed: u8, file: *const u8, file_len: usize, line: u32, column: u32) -> () = "codira.string_check_slice";

    /// 1 and the value at `out` when the string is a decimal `i64`.
    pub fn string_parse_i64(data: *const u8, len: usize, out: *mut u8) -> u8 = "codira.string_parse_i64";

    /// 1 and the value at `out` when the string is an `f64`.
    pub fn string_parse_f64(data: *const u8, len: usize, out: *mut u8) -> u8 = "codira.string_parse_f64";

    /// A signed integer in decimal, as a new `String`.
    pub fn string_from_i64(value: i64, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_from_i64";

    /// An unsigned integer in decimal, as a new `String`.
    pub fn string_from_u64(value: u64, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_from_u64";

    /// A 128-bit integer, as its halves, in decimal.
    pub fn string_from_i128(low: u64, high: u64, signed: u8, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_from_i128";

    /// An `f64` as `print` writes it.
    pub fn string_from_f64(value: f64, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_from_f64";

    /// An `f32` as `print` writes it.
    pub fn string_from_f32(value: f32, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_from_f32";

    /// `true` or `false`.
    pub fn string_from_bool(value: u8, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_from_bool";

    /// The code points as a new `[u32]` (`ty`).
    pub fn string_chars(data: *const u8, len: usize, ty: *const ffi::c_void, alloc: *mut ffi::c_void) -> *const ffi::c_void = "codira.string_chars";

    /// A copy of the bytes as a new `[u8]` (`ty`).
    pub fn string_to_bytes(data: *const u8, len: usize, ty: *const ffi::c_void, alloc: *mut ffi::c_void) -> *const ffi::c_void = "codira.string_to_bytes";

    /// 1 and a new `String` at `out` when the `[u8]` holds valid UTF-8.
    pub fn string_from_utf8(array: *const ffi::c_void, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> u8 = "codira.string_from_utf8";

    /// An integer (as two halves, its signedness and its size in bits)
    /// written as a format spec says, as a new `String`. The spec is a fill
    /// character, `FormatSpec::abi_flags`, a width and a precision.
    pub fn string_format_int(low: u64, high: u64, signed: u8, bits: u32, fill: u32, flags: u32, width: i64, precision: i64, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_format_int";

    /// A float (an `f32` widened when `is_f32` is set) written as a spec says.
    pub fn string_format_float(value: f64, is_f32: u8, fill: u32, flags: u32, width: i64, precision: i64, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_format_float";

    /// Text padded and truncated as a spec says.
    pub fn string_format_text(data: *const u8, len: usize, fill: u32, flags: u32, width: i64, precision: i64, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.string_format_text";

    /// `buf/start/len` with `data` appended quoted, as a Codira literal.
    pub fn debug_quote(buf: *const ffi::c_void, start: usize, len: usize, data: *const u8, data_len: usize, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.debug_quote";

    /// `buf/start/len` with a line break and `depth` indentation levels.
    pub fn debug_newline(buf: *const ffi::c_void, start: usize, len: usize, depth: i64, ty: *const ffi::c_void, alloc: *mut ffi::c_void, out: *mut u8) -> () = "codira.debug_newline";

    /// 1 when the class instance `object` is not already being printed (it
    /// is then recorded until `debug_leave`), 0 on a reference cycle.
    pub fn debug_enter(object: *const ffi::c_void) -> u8 = "codira.debug_enter";

    /// Ends the innermost instance `debug_enter` recorded.
    pub fn debug_leave() -> () = "codira.debug_leave";

    /// Writes `Error: <text>` to standard error and exits with status 1:
    /// what `main` does with an `Err` it returns.
    pub fn exit_with_error(data: *const u8, len: usize) -> () = "codira.exit_with_error";
}

/// Every runtime function a `String` operation may need, collected together
/// like [`print_functions`]: a module that uses strings at all gets them all.
pub fn string_functions() -> [&'static dyn Intrinsic; 38] {
    [
        &string_from_bytes,
        &string_append,
        &string_concat,
        &string_eq,
        &string_cmp,
        &string_hash,
        &string_char_count,
        &string_find,
        &string_rfind,
        &string_contains,
        &string_starts_with,
        &string_ends_with,
        &string_split,
        &string_lines,
        &string_trim,
        &string_change_case,
        &string_replace,
        &string_repeat,
        &string_check_slice,
        &string_parse_i64,
        &string_parse_f64,
        &string_from_i64,
        &string_from_u64,
        &string_from_i128,
        &string_from_f64,
        &string_from_f32,
        &string_from_bool,
        &string_chars,
        &string_to_bytes,
        &string_from_utf8,
        &string_format_int,
        &string_format_float,
        &string_format_text,
        &debug_quote,
        &debug_newline,
        &debug_enter,
        &debug_leave,
        &exit_with_error,
    ]
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
