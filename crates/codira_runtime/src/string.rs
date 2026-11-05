//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - The runtime half of `String`: the `codira.string_*` functions compiled
//!   string operations call, registered in every runtime's dispatch table.
//!   The compiler side is `codira_codegen::intrinsics::string_functions`;
//!   names and signatures must match.
//!
//! # Representation
//!
//! A `String` value is three words, [`RawString`]: a reference to a buffer,
//! the offset of its first byte in the buffer and its length in bytes. The
//! buffer is an ordinary GC array of `u8` (`[u8]`), so the collector traces
//! it, a heap snapshot saves it and a hot reload keeps it with no code of
//! their own. The array's `length` is the buffer's *fill*: every byte below
//! it belongs to some string value, every byte above it to none.
//!
//! # Immutability and appending
//!
//! The bytes a value covers are never written again, so a `String` can be
//! shared freely: `let b = a` copies three words and nothing can change `b`
//! through `a`. Appending (`s + t`, `s += t`) writes in place only when `s`
//! ends exactly at the fill and the buffer has room; it then raises the fill
//! with a compare-and-swap, so two appends racing on one buffer cannot both
//! win. Any other append copies into a new buffer, doubling the capacity
//! when `s` was at the fill (it is being built up) and exact otherwise. A
//! loop of `s += piece` is therefore amortized O(1) per byte, like Rust's
//! `String::push_str`, without a separate builder type.
//!
//! # UTF-8
//!
//! Every function that makes a string makes valid UTF-8 from valid UTF-8:
//! literals are validated by the compiler, `string_from_utf8` validates its
//! bytes, slicing checks character boundaries, and the transformations
//! (case mapping, replacing, splitting, trimming, number formatting) are
//! the ones of Rust's `str`, which preserve validity. The functions that
//! read a string therefore read it as `str` without validating it again.

use std::{
    ffi::c_void,
    mem::ManuallyDrop,
    sync::{atomic::AtomicUsize, atomic::Ordering, Arc},
};

use codira_memory::{gc::Array as _, Type};

use crate::{
    builtins::{embedded_str, report_panic},
    get_allocator, get_type_info, FunctionDefinition, GarbageCollector, IntoFunctionDefinition,
};

/// The three words of a `String` value, as the compiler lays it out.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawString {
    /// The `[u8]` array object holding the bytes (a GC handle).
    pub buf: *const *mut u8,
    /// Offset of the first byte in the buffer's elements.
    pub start: usize,
    /// Length in bytes.
    pub len: usize,
}

/// The header of a GC array payload (`codira_memory`'s `ArrayHeader`), with
/// the fill read and raised atomically.
#[repr(C)]
struct BufHeader {
    length: AtomicUsize,
    capacity: usize,
}

/// Offset of the elements of a `[u8]` (or any array of elements aligned to
/// at most 8 bytes) from the start of its payload.
const DATA_OFFSET: usize = std::mem::size_of::<BufHeader>();

const _: () = assert!(DATA_OFFSET == 16);
const _: () = assert!(std::mem::size_of::<RawString>() == 3 * std::mem::size_of::<usize>());

/// The header of the array object `buf`.
///
/// # Safety
///
/// `buf` must be a live array handle.
unsafe fn header<'a>(buf: *const *mut u8) -> &'a BufHeader {
    &*(*buf).cast::<BufHeader>()
}

/// The first element of the array object `buf`.
///
/// # Safety
///
/// `buf` must be a live array handle whose elements are bytes, or `u32`s,
/// or `String`s (all start at [`DATA_OFFSET`]).
unsafe fn elements(buf: *const *mut u8) -> *mut u8 {
    (*buf).add(DATA_OFFSET)
}

/// The bytes `[start, start + len)` of the string buffer `buf`.
///
/// # Safety
///
/// `buf` must be a live `[u8]` handle covering that range, or null with
/// `len == 0`.
unsafe fn string_bytes<'a>(buf: *const *mut u8, start: usize, len: usize) -> &'a [u8] {
    if len == 0 || buf.is_null() {
        return &[];
    }
    std::slice::from_raw_parts(elements(buf).add(start), len)
}

/// `len` bytes at `data`, which may be null or dangling when `len` is 0.
///
/// # Safety
///
/// `data` must point to `len` readable bytes when `len > 0`.
unsafe fn bytes_at<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if len == 0 || data.is_null() {
        &[]
    } else {
        std::slice::from_raw_parts(data, len)
    }
}

/// Reads bytes that are valid UTF-8 by construction (see the module docs).
///
/// # Safety
///
/// `bytes` must be valid UTF-8.
unsafe fn text(bytes: &[u8]) -> &str {
    debug_assert!(std::str::from_utf8(bytes).is_ok(), "a string is not UTF-8");
    std::str::from_utf8_unchecked(bytes)
}

/// The array type and the collector an allocating function was given.
struct Alloc {
    ty: ManuallyDrop<Type>,
    gc: ManuallyDrop<Arc<GarbageCollector>>,
}

impl Alloc {
    /// # Safety
    ///
    /// The handles must be ones generated code passes: a type handle from its
    /// type table and the runtime's allocator handle.
    unsafe fn new(ty: *const c_void, alloc: *mut c_void) -> Self {
        Self {
            ty: ManuallyDrop::new(get_type_info(ty)),
            gc: ManuallyDrop::new(get_allocator(alloc)),
        }
    }

    /// Allocates an array of the type this was made with with room for
    /// `capacity` elements and a fill of `length`. Every allocation is a
    /// safepoint; the caller's inputs stay alive across it because their
    /// handles (or pointers into them) are on this thread's stack, which a
    /// collection scans conservatively.
    fn array(&self, capacity: usize, length: usize) -> *const *mut u8 {
        let mut handle = match self.gc.try_alloc_array(&self.ty, capacity) {
            Ok(handle) => handle,
            Err(err) => out_of_memory(&self.ty, &err.to_string()),
        };
        // Safety: the elements below `length` are written by the caller
        // before the array is reachable from Codira code.
        unsafe { handle.set_length(length) };
        let raw: codira_memory::gc::RawGcPtr = handle.as_raw().into();
        raw.cast()
    }

    /// A new string buffer holding `parts` one after another, with room for
    /// `capacity` bytes (at least their total).
    fn string(&self, parts: &[&[u8]], capacity: usize) -> RawString {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        let buf = self.array(capacity.max(len), len);
        let mut offset = 0;
        for part in parts {
            // Safety: the new buffer has room for `len` bytes, and no part
            // can overlap it (it did not exist when they were made).
            unsafe {
                std::ptr::copy_nonoverlapping(part.as_ptr(), elements(buf).add(offset), part.len());
            }
            offset += part.len();
        }
        RawString { buf, start: 0, len }
    }
}

#[cold]
fn out_of_memory(ty: &Type, err: &str) -> ! {
    eprintln!(
        "codira runtime: cannot allocate `{}`: {err}. Aborting.",
        ty.name()
    );
    std::process::abort()
}

/// Writes a string result to the out-pointer generated code passed.
///
/// # Safety
///
/// `out` must point to a `String` slot.
unsafe fn write(out: *mut u8, value: RawString) {
    out.cast::<RawString>().write_unaligned(value);
}

/// `"literal".to_string()` and every string made from bytes known to be
/// UTF-8: a new buffer of exactly `len` bytes.
extern "C" fn string_from_bytes(
    data: *const u8,
    len: usize,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    // Safety: generated code passes a `str` and its own handles.
    unsafe {
        let alloc = Alloc::new(ty, alloc);
        let value = alloc.string(&[bytes_at(data, len)], len);
        write(out, value);
    }
}

/// `a + b` and `a += b` for a `String` `a`: appends in place when `a` ends at
/// its buffer's fill and there is room, otherwise copies (see the module
/// docs).
#[allow(clippy::too_many_arguments)]
extern "C" fn string_append(
    buf: *const c_void,
    start: usize,
    len: usize,
    data: *const u8,
    data_len: usize,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    // Safety: generated code passes a live `String` and a `String` or `str`.
    unsafe {
        let value = append(buf.cast(), start, len, bytes_at(data, data_len), ty, alloc);
        write(out, value);
    }
}

/// See [`string_append`].
///
/// # Safety
///
/// As for [`string_append`].
unsafe fn append(
    buf: *const *mut u8,
    start: usize,
    len: usize,
    extra: &[u8],
    ty: *const c_void,
    alloc: *mut c_void,
) -> RawString {
    let current = RawString { buf, start, len };
    if extra.is_empty() {
        return current;
    }
    let mut at_fill = false;
    if !buf.is_null() {
        let header = header(buf);
        let end = start + len;
        let fill = header.length.load(Ordering::Acquire);
        at_fill = fill == end;
        if at_fill
            && header.capacity - end >= extra.len()
            && header
                .length
                .compare_exchange(end, end + extra.len(), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            // The bytes past `end` belonged to no value, and the swap made
            // them this one's alone.
            std::ptr::copy_nonoverlapping(extra.as_ptr(), elements(buf).add(end), extra.len());
            return RawString {
                buf,
                start,
                len: len + extra.len(),
            };
        }
    }
    let needed = len
        .checked_add(extra.len())
        .unwrap_or_else(|| report_capacity_overflow());
    let capacity = if at_fill {
        needed.max(len.saturating_mul(2)).max(32)
    } else {
        needed
    };
    let alloc = Alloc::new(ty, alloc);
    // `string_bytes` is read after the allocation: the old buffer is kept
    // alive by `buf`, which is still in use here.
    let old = string_bytes(buf, start, len);
    alloc.string(&[old, extra], capacity)
}

#[cold]
fn report_capacity_overflow() -> ! {
    eprintln!("codira runtime: a string would exceed the address space. Aborting.");
    std::process::abort()
}

/// `a + b` where `a` is a `str`: a new buffer of exactly both.
extern "C" fn string_concat(
    a: *const u8,
    a_len: usize,
    b: *const u8,
    b_len: usize,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    // Safety: generated code passes two `String`s or `str`s and its handles.
    unsafe {
        let alloc = Alloc::new(ty, alloc);
        let (a, b) = (bytes_at(a, a_len), bytes_at(b, b_len));
        let value = alloc.string(&[a, b], a.len() + b.len());
        write(out, value);
    }
}

/// `a == b`: 1 when the bytes are equal.
extern "C" fn string_eq(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> u8 {
    if a_len != b_len {
        return 0;
    }
    // Safety: generated code passes two `String`s or `str`s.
    unsafe { u8::from(bytes_at(a, a_len) == bytes_at(b, b_len)) }
}

/// `a < b` and friends: -1, 0 or 1 by byte order, which for UTF-8 is the
/// order of the code points (Rust's `Ord for str`).
extern "C" fn string_cmp(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> i64 {
    // Safety: as for `string_eq`.
    let (a, b) = unsafe { (bytes_at(a, a_len), bytes_at(b, b_len)) };
    match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// FNV-1a 64 of the bytes, as `std.hashlib.hash.hash_bytes` computes it.
/// Fast and deterministic, and not resistant to hash flooding: an attacker
/// who controls the keys of a table hashed with it can make every key
/// collide.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

extern "C" fn string_hash(data: *const u8, len: usize) -> u64 {
    // Safety: generated code passes a `String`.
    fnv1a(unsafe { bytes_at(data, len) })
}

extern "C" fn string_char_count(data: *const u8, len: usize) -> usize {
    // Safety: generated code passes a valid `String`.
    unsafe { text(bytes_at(data, len)).chars().count() }
}

/// `s.find(p)`: the byte offset of the first match, or -1.
extern "C" fn string_find(hay: *const u8, hay_len: usize, needle: *const u8, len: usize) -> i64 {
    // Safety: generated code passes two valid strings.
    let found = unsafe { text(bytes_at(hay, hay_len)).find(text(bytes_at(needle, len))) };
    found.map_or(-1, |at| at as i64)
}

/// `s.rfind(p)`: the byte offset of the last match, or -1.
extern "C" fn string_rfind(hay: *const u8, hay_len: usize, needle: *const u8, len: usize) -> i64 {
    // Safety: as for `string_find`.
    let found = unsafe { text(bytes_at(hay, hay_len)).rfind(text(bytes_at(needle, len))) };
    found.map_or(-1, |at| at as i64)
}

extern "C" fn string_contains(hay: *const u8, hay_len: usize, needle: *const u8, len: usize) -> u8 {
    u8::from(string_find(hay, hay_len, needle, len) >= 0)
}

extern "C" fn string_starts_with(s: *const u8, s_len: usize, p: *const u8, p_len: usize) -> u8 {
    // Safety: generated code passes two strings.
    unsafe { u8::from(bytes_at(s, s_len).starts_with(bytes_at(p, p_len))) }
}

extern "C" fn string_ends_with(s: *const u8, s_len: usize, p: *const u8, p_len: usize) -> u8 {
    // Safety: generated code passes two strings.
    unsafe { u8::from(bytes_at(s, s_len).ends_with(bytes_at(p, p_len))) }
}

/// Makes a `[String]` of the pieces of `s` at the byte ranges `pieces`,
/// which share `s`'s buffer.
///
/// # Safety
///
/// `s` must be live, every range must be within it and on character
/// boundaries, and `ty`/`alloc` must be the handles for `[String]`.
unsafe fn string_array(
    s: RawString,
    pieces: &[(usize, usize)],
    ty: *const c_void,
    alloc: *mut c_void,
) -> *const *mut u8 {
    let alloc = Alloc::new(ty, alloc);
    let array = alloc.array(pieces.len(), pieces.len());
    let slots = elements(array).cast::<RawString>();
    for (index, &(offset, len)) in pieces.iter().enumerate() {
        slots.add(index).write(RawString {
            buf: s.buf,
            start: s.start + offset,
            len,
        });
    }
    array
}

/// The byte range of `piece`, a subslice of `whole`.
fn range_in(whole: &str, piece: &str) -> (usize, usize) {
    (piece.as_ptr() as usize - whole.as_ptr() as usize, piece.len())
}

/// `s.split(sep)`: the pieces of `s` between the occurrences of `sep`, as
/// Rust's `str::split` with a string pattern. An empty `sep` panics: what
/// splitting on nothing should mean is not obvious enough to guess.
#[allow(clippy::too_many_arguments)]
extern "C" fn string_split(
    buf: *const c_void,
    start: usize,
    len: usize,
    sep: *const u8,
    sep_len: usize,
    ty: *const c_void,
    alloc: *mut c_void,
    file: *const u8,
    file_len: usize,
    line: u32,
    column: u32,
) -> *const c_void {
    let buf = buf.cast::<*mut u8>();
    // Safety: generated code passes a live `String`, a `String` or `str`
    // separator, its handles and a constant file name.
    unsafe {
        if sep_len == 0 {
            report_panic(
                "`split` was given an empty separator",
                &embedded_str(file, file_len),
                line,
                column,
            );
        }
        let whole = text(string_bytes(buf, start, len));
        let sep = text(bytes_at(sep, sep_len));
        let pieces: Vec<(usize, usize)> = whole.split(sep).map(|p| range_in(whole, p)).collect();
        string_array(RawString { buf, start, len }, &pieces, ty, alloc).cast()
    }
}

/// `s.lines()`, as Rust's `str::lines`: split at `\n`, a `\r` before it
/// dropped, and no empty piece after a final newline.
extern "C" fn string_lines(
    buf: *const c_void,
    start: usize,
    len: usize,
    ty: *const c_void,
    alloc: *mut c_void,
) -> *const c_void {
    let buf = buf.cast::<*mut u8>();
    // Safety: as for `string_split`.
    unsafe {
        let whole = text(string_bytes(buf, start, len));
        let pieces: Vec<(usize, usize)> = whole.lines().map(|p| range_in(whole, p)).collect();
        string_array(RawString { buf, start, len }, &pieces, ty, alloc).cast()
    }
}

/// `trim`, `trim_start` (`mode` 1) and `trim_end` (`mode` 2): writes the
/// offset of the trimmed view to `out_offset` and returns its length. White
/// space is Unicode's `White_Space`, as for Rust's `str::trim`.
extern "C" fn string_trim(data: *const u8, len: usize, mode: u8, out_offset: *mut u8) -> usize {
    // Safety: generated code passes a valid `String` and a `usize` slot.
    unsafe {
        let whole = text(bytes_at(data, len));
        let trimmed = match mode {
            1 => whole.trim_start(),
            2 => whole.trim_end(),
            _ => whole.trim(),
        };
        let (offset, len) = if trimmed.is_empty() {
            (0, 0)
        } else {
            range_in(whole, trimmed)
        };
        out_offset.cast::<usize>().write_unaligned(offset);
        len
    }
}

/// `to_upper` (`upper` 1) and `to_lower`: ASCII letters only; every other
/// byte, and so every non-ASCII character, is kept as it is.
extern "C" fn string_change_case(
    data: *const u8,
    len: usize,
    upper: u8,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    // Safety: generated code passes a valid `String` and its handles.
    unsafe {
        let alloc = Alloc::new(ty, alloc);
        let value = alloc.string(&[bytes_at(data, len)], len);
        let bytes = std::slice::from_raw_parts_mut(elements(value.buf), len);
        if upper != 0 {
            bytes.make_ascii_uppercase();
        } else {
            bytes.make_ascii_lowercase();
        }
        write(out, value);
    }
}

/// `s.replace(from, to)`, as Rust's `str::replace` (an empty `from`
/// inserts `to` before every character and at the end).
#[allow(clippy::too_many_arguments)]
extern "C" fn string_replace(
    data: *const u8,
    len: usize,
    from: *const u8,
    from_len: usize,
    to: *const u8,
    to_len: usize,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    // Safety: generated code passes three valid strings and its handles.
    unsafe {
        let replaced = text(bytes_at(data, len))
            .replace(text(bytes_at(from, from_len)), text(bytes_at(to, to_len)));
        let alloc = Alloc::new(ty, alloc);
        let value = alloc.string(&[replaced.as_bytes()], replaced.len());
        write(out, value);
    }
}

/// `s.repeat(n)`; a negative `n` (passed as its two's complement with
/// `signed` set) repeats nothing.
extern "C" fn string_repeat(
    data: *const u8,
    len: usize,
    count: u64,
    signed: u8,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    let count = if signed != 0 && (count as i64) < 0 {
        0
    } else {
        usize::try_from(count).unwrap_or(usize::MAX)
    };
    // Safety: generated code passes a valid `String` and its handles.
    unsafe {
        let bytes = bytes_at(data, len);
        let total = len
            .checked_mul(count)
            .unwrap_or_else(|| report_capacity_overflow());
        let alloc = Alloc::new(ty, alloc);
        let value = alloc.string(&[], total);
        let target = elements(value.buf);
        for index in 0..count {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.add(index * len), len);
        }
        header(value.buf).length.store(total, Ordering::Release);
        write(out, RawString { len: total, ..value });
    }
}

/// Formats an index passed as 64 bits and its signedness.
fn index_text(index: u64, signed: u8) -> String {
    if signed != 0 {
        (index as i64).to_string()
    } else {
        index.to_string()
    }
}

/// The check before `s.slice(start, end)`: both in range, `start <= end`,
/// and both on character boundaries, or a panic at the call's location.
/// Indices travel as 64 bits plus their signedness, so `-1` reads as `-1`.
#[allow(clippy::too_many_arguments)]
extern "C" fn string_check_slice(
    data: *const u8,
    len: usize,
    start: u64,
    start_signed: u8,
    end: u64,
    end_signed: u8,
    file: *const u8,
    file_len: usize,
    line: u32,
    column: u32,
) {
    let negative = |value: u64, signed: u8| signed != 0 && (value as i64) < 0;
    let fail = |message: String| -> ! {
        // Safety: generated code passes a constant file name.
        let file = unsafe { embedded_str(file, file_len) };
        report_panic(&message, &file, line, column)
    };
    let range = || {
        format!(
            "{}..{}",
            index_text(start, start_signed),
            index_text(end, end_signed)
        )
    };
    if negative(start, start_signed)
        || negative(end, end_signed)
        || start > len as u64
        || end > len as u64
    {
        fail(format!(
            "string slice {} is out of range for a string of length {len}",
            range()
        ));
    }
    if start > end {
        fail(format!("string slice {} starts after it ends", range()));
    }
    // Safety: generated code passes a valid `String`.
    let whole = unsafe { text(bytes_at(data, len)) };
    for index in [start as usize, end as usize] {
        if !whole.is_char_boundary(index) {
            let char_start = (0..index).rev().find(|&i| whole.is_char_boundary(i)).unwrap_or(0);
            let ch = whole[char_start..].chars().next().unwrap_or('\u{FFFD}');
            fail(format!(
                "byte index {index} is not a char boundary; it is inside {ch:?} (bytes {char_start}..{})",
                char_start + ch.len_utf8()
            ));
        }
    }
}

/// `s.parse_i64()`: 1 and the value at `out`, or 0. Accepts what Rust's
/// `i64::from_str` accepts: an optional `+` or `-` and decimal digits, no
/// white space.
extern "C" fn string_parse_i64(data: *const u8, len: usize, out: *mut u8) -> u8 {
    // Safety: generated code passes a valid `String` and an `i64` slot.
    unsafe {
        match text(bytes_at(data, len)).parse::<i64>() {
            Ok(value) => {
                out.cast::<i64>().write_unaligned(value);
                1
            }
            Err(_) => 0,
        }
    }
}

/// `s.parse_f64()`: as Rust's `f64::from_str` (decimal or exponent form,
/// `inf`, `NaN`, no white space).
extern "C" fn string_parse_f64(data: *const u8, len: usize, out: *mut u8) -> u8 {
    // Safety: generated code passes a valid `String` and an `f64` slot.
    unsafe {
        match text(bytes_at(data, len)).parse::<f64>() {
            Ok(value) => {
                out.cast::<f64>().write_unaligned(value);
                1
            }
            Err(_) => 0,
        }
    }
}

/// A formatted number or `bool`, as `print` writes it, as a new string.
fn formatted(args: std::fmt::Arguments<'_>, ty: *const c_void, alloc: *mut c_void, out: *mut u8) {
    use std::io::Write;
    let mut buffer = [0u8; 64];
    let mut cursor = std::io::Cursor::new(&mut buffer[..]);
    let written = if cursor.write_fmt(args).is_ok() {
        cursor.position() as usize
    } else {
        // Longer than any number is; format on the heap.
        let text = std::fmt::format(args);
        // Safety: generated code passes its handles and a `String` slot.
        unsafe {
            let alloc = Alloc::new(ty, alloc);
            write(out, alloc.string(&[text.as_bytes()], text.len()));
        }
        return;
    };
    // Safety: as above.
    unsafe {
        let alloc = Alloc::new(ty, alloc);
        write(out, alloc.string(&[&buffer[..written]], written));
    }
}

extern "C" fn string_from_i64(value: i64, ty: *const c_void, alloc: *mut c_void, out: *mut u8) {
    formatted(format_args!("{value}"), ty, alloc, out);
}

extern "C" fn string_from_u64(value: u64, ty: *const c_void, alloc: *mut c_void, out: *mut u8) {
    formatted(format_args!("{value}"), ty, alloc, out);
}

/// A 128-bit integer, as its two halves (see `codira.print_i128`).
extern "C" fn string_from_i128(
    low: u64,
    high: u64,
    signed: u8,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    let bits = (u128::from(high) << 64) | u128::from(low);
    if signed != 0 {
        formatted(format_args!("{}", bits as i128), ty, alloc, out);
    } else {
        formatted(format_args!("{bits}"), ty, alloc, out);
    }
}

extern "C" fn string_from_f64(value: f64, ty: *const c_void, alloc: *mut c_void, out: *mut u8) {
    formatted(format_args!("{value:?}"), ty, alloc, out);
}

extern "C" fn string_from_f32(value: f32, ty: *const c_void, alloc: *mut c_void, out: *mut u8) {
    formatted(format_args!("{value:?}"), ty, alloc, out);
}

extern "C" fn string_from_bool(value: u8, ty: *const c_void, alloc: *mut c_void, out: *mut u8) {
    let text = if value != 0 { "true" } else { "false" };
    formatted(format_args!("{text}"), ty, alloc, out);
}

/// `s.chars()`: the code points as a new `[u32]`.
extern "C" fn string_chars(
    data: *const u8,
    len: usize,
    ty: *const c_void,
    alloc: *mut c_void,
) -> *const c_void {
    // Safety: generated code passes a valid `String` and the `[u32]` handles.
    unsafe {
        let whole = text(bytes_at(data, len));
        let count = whole.chars().count();
        let alloc = Alloc::new(ty, alloc);
        let array = alloc.array(count, count);
        let slots = elements(array).cast::<u32>();
        // The source is re-read after the allocation: `data` points into a
        // buffer the caller's `String` keeps alive.
        let whole = text(bytes_at(data, len));
        for (index, ch) in whole.chars().enumerate() {
            slots.add(index).write(u32::from(ch));
        }
        array.cast()
    }
}

/// `s.bytes()`: a copy of the bytes as a new `[u8]`, which the program may
/// change without affecting any string.
extern "C" fn string_to_bytes(
    data: *const u8,
    len: usize,
    ty: *const c_void,
    alloc: *mut c_void,
) -> *const c_void {
    // Safety: generated code passes a `String` and the `[u8]` handles.
    unsafe {
        let alloc = Alloc::new(ty, alloc);
        let array = alloc.array(len, len);
        std::ptr::copy_nonoverlapping(bytes_at(data, len).as_ptr(), elements(array), len);
        array.cast()
    }
}

/// `string_from_utf8(bytes)`: 1 and a new string at `out` when the array
/// holds valid UTF-8, else 0. The bytes are copied: the array stays the
/// program's to change.
extern "C" fn string_from_utf8(
    array: *const c_void,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) -> u8 {
    let array = array.cast::<*mut u8>();
    // Safety: generated code passes a live `[u8]`, its handles and a slot.
    unsafe {
        let len = header(array).length.load(Ordering::Acquire);
        if std::str::from_utf8(std::slice::from_raw_parts(elements(array), len)).is_err() {
            return 0;
        }
        let alloc = Alloc::new(ty, alloc);
        // The bytes are read again after the allocation: `array` is still in
        // use here, so it is alive, and arrays never change length.
        let bytes = std::slice::from_raw_parts(elements(array), len);
        write(out, alloc.string(&[bytes], len));
        1
    }
}

/// The functions compiled string operations call, under the names the
/// compiler's `runtime_functions!` prototypes use.
pub(crate) fn definitions() -> Vec<FunctionDefinition> {
    type H = *const c_void;
    type T = *const c_void;
    type A = *mut c_void;
    type P = *const u8;
    type O = *mut u8;
    vec![
        IntoFunctionDefinition::into(
            string_from_bytes as extern "C" fn(P, usize, T, A, O),
            "codira.string_from_bytes",
        ),
        IntoFunctionDefinition::into(
            string_append as extern "C" fn(H, usize, usize, P, usize, T, A, O),
            "codira.string_append",
        ),
        IntoFunctionDefinition::into(
            string_concat as extern "C" fn(P, usize, P, usize, T, A, O),
            "codira.string_concat",
        ),
        IntoFunctionDefinition::into(
            string_eq as extern "C" fn(P, usize, P, usize) -> u8,
            "codira.string_eq",
        ),
        IntoFunctionDefinition::into(
            string_cmp as extern "C" fn(P, usize, P, usize) -> i64,
            "codira.string_cmp",
        ),
        IntoFunctionDefinition::into(
            string_hash as extern "C" fn(P, usize) -> u64,
            "codira.string_hash",
        ),
        IntoFunctionDefinition::into(
            string_char_count as extern "C" fn(P, usize) -> usize,
            "codira.string_char_count",
        ),
        IntoFunctionDefinition::into(
            string_find as extern "C" fn(P, usize, P, usize) -> i64,
            "codira.string_find",
        ),
        IntoFunctionDefinition::into(
            string_rfind as extern "C" fn(P, usize, P, usize) -> i64,
            "codira.string_rfind",
        ),
        IntoFunctionDefinition::into(
            string_contains as extern "C" fn(P, usize, P, usize) -> u8,
            "codira.string_contains",
        ),
        IntoFunctionDefinition::into(
            string_starts_with as extern "C" fn(P, usize, P, usize) -> u8,
            "codira.string_starts_with",
        ),
        IntoFunctionDefinition::into(
            string_ends_with as extern "C" fn(P, usize, P, usize) -> u8,
            "codira.string_ends_with",
        ),
        IntoFunctionDefinition::into(
            string_split as extern "C" fn(H, usize, usize, P, usize, T, A, P, usize, u32, u32) -> H,
            "codira.string_split",
        ),
        IntoFunctionDefinition::into(
            string_lines as extern "C" fn(H, usize, usize, T, A) -> H,
            "codira.string_lines",
        ),
        IntoFunctionDefinition::into(
            string_trim as extern "C" fn(P, usize, u8, O) -> usize,
            "codira.string_trim",
        ),
        IntoFunctionDefinition::into(
            string_change_case as extern "C" fn(P, usize, u8, T, A, O),
            "codira.string_change_case",
        ),
        IntoFunctionDefinition::into(
            string_replace as extern "C" fn(P, usize, P, usize, P, usize, T, A, O),
            "codira.string_replace",
        ),
        IntoFunctionDefinition::into(
            string_repeat as extern "C" fn(P, usize, u64, u8, T, A, O),
            "codira.string_repeat",
        ),
        IntoFunctionDefinition::into(
            string_check_slice as extern "C" fn(P, usize, u64, u8, u64, u8, P, usize, u32, u32),
            "codira.string_check_slice",
        ),
        IntoFunctionDefinition::into(
            string_parse_i64 as extern "C" fn(P, usize, O) -> u8,
            "codira.string_parse_i64",
        ),
        IntoFunctionDefinition::into(
            string_parse_f64 as extern "C" fn(P, usize, O) -> u8,
            "codira.string_parse_f64",
        ),
        IntoFunctionDefinition::into(
            string_from_i64 as extern "C" fn(i64, T, A, O),
            "codira.string_from_i64",
        ),
        IntoFunctionDefinition::into(
            string_from_u64 as extern "C" fn(u64, T, A, O),
            "codira.string_from_u64",
        ),
        IntoFunctionDefinition::into(
            string_from_i128 as extern "C" fn(u64, u64, u8, T, A, O),
            "codira.string_from_i128",
        ),
        IntoFunctionDefinition::into(
            string_from_f64 as extern "C" fn(f64, T, A, O),
            "codira.string_from_f64",
        ),
        IntoFunctionDefinition::into(
            string_from_f32 as extern "C" fn(f32, T, A, O),
            "codira.string_from_f32",
        ),
        IntoFunctionDefinition::into(
            string_from_bool as extern "C" fn(u8, T, A, O),
            "codira.string_from_bool",
        ),
        IntoFunctionDefinition::into(
            string_chars as extern "C" fn(P, usize, T, A) -> H,
            "codira.string_chars",
        ),
        IntoFunctionDefinition::into(
            string_to_bytes as extern "C" fn(P, usize, T, A) -> H,
            "codira.string_to_bytes",
        ),
        IntoFunctionDefinition::into(
            string_from_utf8 as extern "C" fn(H, T, A, O) -> u8,
            "codira.string_from_utf8",
        ),
    ]
}
