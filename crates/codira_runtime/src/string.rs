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

use std::ptr::NonNull;

use codira_memory::{
    gc::{Array as _, GcPtr, HasIndirectionPtr, RawGcPtr},
    HasStaticType, Type,
};

use crate::{
    builtins::{embedded_str, report_panic},
    get_allocator, get_type_info, ArgumentReflection, FunctionDefinition, GarbageCollector,
    IntoFunctionDefinition, Marshal, ReturnTypeReflection, Runtime,
};

mod format;

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
    (
        piece.as_ptr() as usize - whole.as_ptr() as usize,
        piece.len(),
    )
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
        write(
            out,
            RawString {
                len: total,
                ..value
            },
        );
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

/// Why `start..end` is not a valid slice of `whole`, or `None` when it is:
/// both in range, `start <= end`, and both on character boundaries. Indices
/// travel as 64 bits plus their signedness, so `-1` reads as `-1`. The same
/// decision as `str::get(start..end).is_some()` (checked against it by the
/// property tests below).
pub fn slice_error(
    whole: &str,
    start: u64,
    start_signed: u8,
    end: u64,
    end_signed: u8,
) -> Option<String> {
    let negative = |value: u64, signed: u8| signed != 0 && (value as i64) < 0;
    let len = whole.len();
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
        return Some(format!(
            "string slice {} is out of range for a string of length {len}",
            range()
        ));
    }
    if start > end {
        return Some(format!("string slice {} starts after it ends", range()));
    }
    for index in [start as usize, end as usize] {
        if !whole.is_char_boundary(index) {
            let char_start = (0..index)
                .rev()
                .find(|&i| whole.is_char_boundary(i))
                .unwrap_or(0);
            let ch = whole[char_start..].chars().next().unwrap_or('\u{FFFD}');
            return Some(format!(
                "byte index {index} is not a char boundary; it is inside {ch:?} (bytes {char_start}..{})",
                char_start + ch.len_utf8()
            ));
        }
    }
    None
}

/// The check before `s.slice(start, end)`: a panic at the call's location
/// unless [`slice_error`] finds nothing wrong.
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
    // Safety: generated code passes a valid `String`.
    let whole = unsafe { text(bytes_at(data, len)) };
    if let Some(message) = slice_error(whole, start, start_signed, end, end_signed) {
        // Safety: generated code passes a constant file name.
        let file = unsafe { embedded_str(file, file_len) };
        report_panic(&message, &file, line, column)
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

/// A Codira `String` held by the host: returned by [`Runtime::invoke`], read
/// from a field with `StructRef::get`, or made with [`StringRef::new`] to
/// pass to Codira code.
///
/// Like [`crate::StructRef`] and [`crate::ArrayRef`] it does not keep the
/// string alive: [`StringRef::root`] it to hold it across calls. Its bytes
/// never change, so reading them needs no lock.
///
/// Across the host boundary (and between modules) a `String` travels boxed:
/// as one pointer to a GC object holding the three words, the convention of
/// value structs.
#[derive(Clone)]
pub struct StringRef<'r> {
    raw: RawString,
    runtime: &'r Runtime,
}

/// The boxed `String` of the host calling convention: a GC handle to an
/// object of type `core::String`.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct BoxedString(GcPtr);

impl<'r> StringRef<'r> {
    /// A new string holding a copy of `text`, allocated on `runtime`'s heap.
    pub fn new(runtime: &'r Runtime, text: &str) -> Self {
        let ty = <u8 as HasStaticType>::type_info().array_type();
        let mut handle = match runtime.gc.try_alloc_array(&ty, text.len()) {
            Ok(handle) => handle,
            Err(err) => out_of_memory(&ty, &err.to_string()),
        };
        // Safety: every element is written right below.
        unsafe { handle.set_length(text.len()) };
        let raw: RawGcPtr = handle.as_raw().into();
        let buf = raw.cast::<*mut u8>();
        // Safety: the array was made with room for `text.len()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), elements(buf), text.len());
        }
        Self {
            raw: RawString {
                buf,
                start: 0,
                len: text.len(),
            },
            runtime,
        }
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        // Safety: a `String` is valid UTF-8 and its bytes never change; the
        // buffer is alive while the caller keeps the string reachable.
        unsafe { text(string_bytes(self.raw.buf, self.raw.start, self.raw.len)) }
    }

    /// The length in bytes.
    pub fn len(&self) -> usize {
        self.raw.len
    }

    /// Whether the string is empty.
    pub fn is_empty(&self) -> bool {
        self.raw.len == 0
    }

    /// The three words of the value.
    pub fn raw(&self) -> RawString {
        self.raw
    }

    /// Roots the string's buffer so that it survives collections.
    pub fn root(self) -> RootedString {
        RootedString {
            handle: (!self.raw.buf.is_null()).then(|| {
                crate::garbage_collector::GcRootPtr::new(
                    &self.runtime.gc,
                    GcPtr::from(self.raw.buf.cast::<*mut c_void>()),
                )
            }),
            start: self.raw.start,
            len: self.raw.len,
        }
    }

    fn string_type(runtime: &Runtime) -> Type {
        runtime
            .get_type_info_by_name("core::String")
            .expect("the loaded program uses no `String`, so the runtime has no type for one")
    }
}

impl std::fmt::Debug for StringRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for StringRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A [`StringRef`] whose buffer is rooted. Obtain a `StringRef` with
/// [`RootedString::as_ref`] to read it.
pub struct RootedString {
    handle: Option<crate::garbage_collector::GcRootPtr>,
    start: usize,
    len: usize,
}

impl RootedString {
    /// The string, for reading or passing to Codira code.
    pub fn as_ref<'r>(&self, runtime: &'r Runtime) -> StringRef<'r> {
        let buf = self.handle.as_ref().map_or(std::ptr::null(), |handle| {
            let raw: RawGcPtr = handle.handle().into();
            raw.cast::<*mut u8>()
        });
        StringRef {
            raw: RawString {
                buf,
                start: self.start,
                len: self.len,
            },
            runtime,
        }
    }
}

impl ReturnTypeReflection for StringRef<'_> {
    fn accepts_type(ty: &Type) -> bool {
        ty.name() == "core::String"
    }

    fn type_hint() -> &'static str {
        "String"
    }
}

impl ArgumentReflection for StringRef<'_> {
    fn type_info(&self, runtime: &Runtime) -> Type {
        Self::string_type(runtime)
    }
}

impl<'r> Marshal<'r> for StringRef<'r> {
    type CodiraType = BoxedString;

    fn marshal_from<'rt>(value: BoxedString, runtime: &'rt Runtime) -> Self
    where
        Self: 'r,
        'rt: 'r,
    {
        // Safety: a boxed `String` holds the three words of one.
        let raw = unsafe { value.0.deref::<RawString>().read_unaligned() };
        StringRef { raw, runtime }
    }

    fn marshal_into(self) -> BoxedString {
        let ty = Self::string_type(self.runtime);
        let mut handle = match self.runtime.gc.try_alloc(&ty) {
            Ok(handle) => handle,
            Err(err) => out_of_memory(&ty, &err.to_string()),
        };
        // Safety: the object was allocated with the layout of a `String`.
        unsafe { handle.deref_mut::<RawString>().write_unaligned(self.raw) };
        BoxedString(handle)
    }

    fn marshal_from_ptr<'rt>(ptr: NonNull<BoxedString>, runtime: &'rt Runtime, _ty: &Type) -> Self
    where
        Self: 'r,
        'rt: 'r,
    {
        // A field or an element holds the three words inline.
        // Safety: the caller passes the address of a `String` value.
        let raw = unsafe { ptr.cast::<RawString>().as_ptr().read_unaligned() };
        StringRef { raw, runtime }
    }

    fn marshal_to_ptr(value: Self, ptr: NonNull<BoxedString>, _ty: &Type) {
        // Safety: the caller passes the address of a `String` slot.
        unsafe { ptr.cast::<RawString>().as_ptr().write_unaligned(value.raw) };
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
    .into_iter()
    .chain(format::definitions())
    .collect()
}

#[cfg(test)]
mod tests {
    //! Unit and property tests of the string functions, called the way
    //! generated code calls them, against Rust's `str` as the oracle. The
    //! inputs come from a seeded generator, so every run checks the same
    //! cases and a failure names the case that broke.
    use std::sync::Arc;

    use codira_abi::StructMemoryKind;
    use codira_memory::StructTypeBuilder;

    use super::*;

    /// xorshift64*: small, fast and deterministic.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        /// A character from a mix that stresses UTF-8: ASCII, white space
        /// (ASCII and not), and two-, three- and four-byte characters.
        fn char(&mut self) -> char {
            const SPECIAL: &[char] = &[
                ' ',
                '\t',
                '\n',
                '\r',
                '\u{a0}',
                '\u{2003}',
                '\u{3000}',
                'é',
                'ß',
                'Ω',
                '世',
                '界',
                '\u{1F600}',
                '\u{10FFFF}',
                '\u{FFFD}',
                ',',
                'a',
                'b',
            ];
            match self.below(4) {
                0 => SPECIAL[self.below(SPECIAL.len() as u64) as usize],
                1 => char::from(b'a' + self.below(26) as u8),
                2 => char::from(b' ' + self.below(95) as u8),
                _ => loop {
                    if let Some(c) = char::from_u32(self.below(0x11_0000) as u32) {
                        break c;
                    }
                },
            }
        }

        fn string(&mut self, max: u64) -> String {
            (0..self.below(max + 1)).map(|_| self.char()).collect()
        }

        /// Bytes that are often but not always UTF-8: valid characters,
        /// lone continuation bytes, truncated sequences, overlong forms,
        /// surrogates and bytes no UTF-8 has.
        fn bytes(&mut self) -> Vec<u8> {
            let mut out = Vec::new();
            for _ in 0..self.below(12) {
                match self.below(8) {
                    0..=2 => {
                        let mut buffer = [0; 4];
                        out.extend_from_slice(self.char().encode_utf8(&mut buffer).as_bytes());
                    }
                    3 => out.push(0x80 + self.below(64) as u8),
                    4 => {
                        let mut buffer = [0; 4];
                        let encoded = self.char().encode_utf8(&mut buffer).as_bytes().to_vec();
                        let keep = 1 + self.below(encoded.len() as u64) as usize;
                        out.extend_from_slice(&encoded[..keep.min(encoded.len())]);
                    }
                    5 => out.extend_from_slice(&[0xC0 + self.below(2) as u8, 0x80]),
                    6 => out.extend_from_slice(&[0xED, 0xA0 + self.below(32) as u8, 0x80]),
                    _ => out.push(self.next() as u8),
                }
            }
            out
        }
    }

    /// A collector and the array types the string functions allocate, with
    /// the raw handles generated code would pass.
    struct Heap {
        gc: Arc<GarbageCollector>,
        bytes: *const c_void,
        strings: *const c_void,
        code_points: *const c_void,
    }

    impl Heap {
        fn new() -> Self {
            let bytes = <u8 as HasStaticType>::type_info().array_type();
            let string = StructTypeBuilder::new("core::String")
                .set_memory_kind(StructMemoryKind::Value)
                .add_field("buf", bytes.clone())
                .add_field("start", <usize as HasStaticType>::type_info().clone())
                .add_field("len", <usize as HasStaticType>::type_info().clone())
                .finish();
            Self {
                gc: Arc::new(GarbageCollector::default()),
                // The handles stay valid for the test: `into_raw` keeps a
                // reference that is never given back.
                bytes: Type::into_raw(bytes),
                strings: Type::into_raw(string.array_type()),
                code_points: Type::into_raw(<u32 as HasStaticType>::type_info().array_type()),
            }
        }

        fn alloc(&self) -> *mut c_void {
            Arc::as_ptr(&self.gc) as *mut c_void
        }

        fn string(&self, text: &str) -> RawString {
            let mut out = std::mem::MaybeUninit::<RawString>::uninit();
            string_from_bytes(
                text.as_ptr(),
                text.len(),
                self.bytes,
                self.alloc(),
                out.as_mut_ptr().cast(),
            );
            // Safety: the function wrote the slot.
            unsafe { out.assume_init() }
        }

        fn read(&self, value: RawString) -> &str {
            // Safety: every value the tests make is a live, valid string.
            unsafe { text(string_bytes(value.buf, value.start, value.len)) }
        }

        fn data(&self, value: RawString) -> *const u8 {
            // Safety: as for `read`.
            unsafe { string_bytes(value.buf, value.start, value.len).as_ptr() }
        }

        fn append(&self, value: RawString, extra: &str) -> RawString {
            let mut out = std::mem::MaybeUninit::<RawString>::uninit();
            string_append(
                value.buf.cast(),
                value.start,
                value.len,
                extra.as_ptr(),
                extra.len(),
                self.bytes,
                self.alloc(),
                out.as_mut_ptr().cast(),
            );
            // Safety: the function wrote the slot.
            unsafe { out.assume_init() }
        }

        fn byte_array(&self, bytes: &[u8]) -> *const c_void {
            // A string buffer is a `[u8]`; fill one with arbitrary bytes.
            // Safety: the type handle is `[u8]`.
            let alloc = unsafe { Alloc::new(self.bytes, self.alloc()) };
            alloc.string(&[bytes], bytes.len()).buf.cast()
        }

        /// The strings of a `[String]` array the runtime made.
        fn strings(&self, array: *const c_void) -> Vec<String> {
            let array = array.cast::<*mut u8>();
            // Safety: `array` is a `[String]` the runtime just made.
            unsafe {
                let len = header(array).length.load(Ordering::Acquire);
                let slots = elements(array).cast::<RawString>();
                (0..len)
                    .map(|i| self.read(slots.add(i).read()).to_owned())
                    .collect()
            }
        }
    }

    #[test]
    fn utf8_validation_agrees_with_rust() {
        let heap = Heap::new();
        let mut rng = Rng(0x5eed_0001);
        let mut valid = 0;
        for case in 0..20_000 {
            let bytes = rng.bytes();
            let array = heap.byte_array(&bytes);
            let mut out = std::mem::MaybeUninit::<RawString>::uninit();
            let ok = string_from_utf8(array, heap.bytes, heap.alloc(), out.as_mut_ptr().cast());
            match std::str::from_utf8(&bytes) {
                Ok(expected) => {
                    assert_eq!(ok, 1, "case {case}: {bytes:x?} is UTF-8");
                    // Safety: the function wrote the slot.
                    let value = unsafe { out.assume_init() };
                    assert_eq!(heap.read(value), expected, "case {case}");
                    assert_ne!(value.buf, array.cast(), "case {case}: the bytes are copied");
                    valid += 1;
                }
                Err(_) => assert_eq!(ok, 0, "case {case}: {bytes:x?} is not UTF-8"),
            }
        }
        // Both outcomes are well represented.
        assert!(valid > 2_000 && valid < 18_000, "{valid} valid of 20000");
    }

    #[test]
    fn slicing_agrees_with_rust() {
        let mut rng = Rng(0x5eed_0002);
        let mut accepted = 0;
        for case in 0..50_000 {
            let s = rng.string(8);
            let span = s.len() as i64 + 3;
            let a = rng.below(span as u64 + 2) as i64 - 2;
            let b = rng.below(span as u64 + 2) as i64 - 2;
            let signed = u8::from(rng.below(2) == 0 || a < 0 || b < 0);
            let error = slice_error(&s, a as u64, signed, b as u64, signed);
            let oracle = (a >= 0 && b >= 0)
                .then(|| s.get(a as usize..b as usize))
                .flatten();
            assert_eq!(
                error.is_none(),
                oracle.is_some(),
                "case {case}: {s:?}.slice({a}, {b}): {error:?}"
            );
            if oracle.is_some() {
                accepted += 1;
            }
        }
        assert!(accepted > 3_000, "{accepted} in-range slices of 50000");
        // The messages name what is wrong.
        assert_eq!(
            slice_error("héllo", 0, 0, 2, 0).unwrap(),
            "byte index 2 is not a char boundary; it is inside 'é' (bytes 1..3)"
        );
        assert_eq!(
            slice_error("abc", (-1i64) as u64, 1, 2, 1).unwrap(),
            "string slice -1..2 is out of range for a string of length 3"
        );
        assert_eq!(
            slice_error("abc", 2, 0, 1, 0).unwrap(),
            "string slice 2..1 starts after it ends"
        );
    }

    /// Appending never changes a value that exists: a random tree of
    /// appends, each from a random earlier value, is checked against Rust
    /// strings built the same way. Values that end at their buffer's fill
    /// are extended in place, so this is what makes sharing safe.
    #[test]
    fn appending_never_changes_an_existing_value() {
        let heap = Heap::new();
        let mut rng = Rng(0x5eed_0003);
        let mut values = vec![(heap.string(""), String::new())];
        let mut in_place = 0;
        for case in 0..20_000 {
            let from = rng.below(values.len() as u64) as usize;
            let (value, model) = values[from].clone();
            let extra = rng.string(3);
            let appended = heap.append(value, &extra);
            if appended.buf == value.buf && !extra.is_empty() {
                in_place += 1;
            }
            let mut expected = model;
            expected.push_str(&extra);
            assert_eq!(heap.read(appended), expected, "case {case}");
            values.push((appended, expected));
            if case % 1000 == 0 {
                for (index, (value, model)) in values.iter().enumerate() {
                    assert_eq!(heap.read(*value), model, "value {index} after case {case}");
                }
            }
        }
        for (value, model) in &values {
            assert_eq!(heap.read(*value), model);
        }
        assert!(in_place > 1_000, "only {in_place} appends were in place");
    }

    #[test]
    fn searching_splitting_and_trimming_agree_with_rust() {
        let heap = Heap::new();
        let mut rng = Rng(0x5eed_0004);
        for case in 0..5_000 {
            let s = rng.string(12);
            // A needle that often occurs: a piece of `s`, or a character.
            let needle = match rng.below(3) {
                0 if !s.is_empty() => {
                    let chars: Vec<char> = s.chars().collect();
                    let a = rng.below(chars.len() as u64) as usize;
                    let b = a + rng.below((chars.len() - a) as u64 + 1) as usize;
                    chars[a..b].iter().collect()
                }
                1 => rng.char().to_string(),
                _ => rng.string(2),
            };
            let value = heap.string(&s);
            let (data, len) = (heap.data(value), value.len);
            let (n, n_len) = (needle.as_ptr(), needle.len());
            let found = |at: i64| (at >= 0).then_some(at as usize);
            assert_eq!(
                found(string_find(data, len, n, n_len)),
                s.find(&*needle),
                "{case}"
            );
            assert_eq!(
                found(string_rfind(data, len, n, n_len)),
                s.rfind(&*needle),
                "{case}"
            );
            assert_eq!(
                string_contains(data, len, n, n_len) != 0,
                s.contains(&*needle)
            );
            assert_eq!(
                string_starts_with(data, len, n, n_len) != 0,
                s.starts_with(&*needle)
            );
            assert_eq!(
                string_ends_with(data, len, n, n_len) != 0,
                s.ends_with(&*needle)
            );
            assert_eq!(
                string_cmp(data, len, n, n_len),
                s.cmp(&needle) as i64,
                "{case}: {s:?} vs {needle:?}"
            );
            assert_eq!(string_eq(data, len, n, n_len) != 0, s == needle);
            assert_eq!(string_hash(data, len), fnv1a(s.as_bytes()));
            assert_eq!(string_char_count(data, len), s.chars().count());

            if !needle.is_empty() {
                let pieces = string_split(
                    value.buf.cast(),
                    value.start,
                    value.len,
                    n,
                    n_len,
                    heap.strings,
                    heap.alloc(),
                    std::ptr::null(),
                    0,
                    0,
                    0,
                );
                let expected: Vec<&str> = s.split(&*needle).collect();
                assert_eq!(
                    heap.strings(pieces),
                    expected,
                    "{case}: {s:?} split {needle:?}"
                );
            }
            let lines = string_lines(
                value.buf.cast(),
                value.start,
                value.len,
                heap.strings,
                heap.alloc(),
            );
            assert_eq!(heap.strings(lines), s.lines().collect::<Vec<_>>(), "{case}");

            for (mode, expected) in [(0, s.trim()), (1, s.trim_start()), (2, s.trim_end())] {
                let mut offset = 0usize;
                let trimmed_len =
                    string_trim(data, len, mode, std::ptr::addr_of_mut!(offset).cast());
                let view = RawString {
                    buf: value.buf,
                    start: value.start + offset,
                    len: trimmed_len,
                };
                assert_eq!(heap.read(view), expected, "{case}: mode {mode} of {s:?}");
            }

            let chars = string_chars(data, len, heap.code_points, heap.alloc()).cast::<*mut u8>();
            // Safety: a `[u32]` the runtime just made.
            let code_points: Vec<u32> = unsafe {
                let count = header(chars).length.load(Ordering::Acquire);
                std::slice::from_raw_parts(elements(chars).cast::<u32>(), count).to_vec()
            };
            assert_eq!(code_points, s.chars().map(u32::from).collect::<Vec<_>>());
        }
    }

    #[test]
    fn transformations_agree_with_rust() {
        let heap = Heap::new();
        let mut rng = Rng(0x5eed_0005);
        for case in 0..3_000 {
            let s = rng.string(10);
            let from = rng.string(1);
            let to = rng.string(2);
            let value = heap.string(&s);
            let (data, len) = (heap.data(value), value.len);
            let mut out = std::mem::MaybeUninit::<RawString>::uninit();
            for upper in [0u8, 1] {
                string_change_case(
                    data,
                    len,
                    upper,
                    heap.bytes,
                    heap.alloc(),
                    out.as_mut_ptr().cast(),
                );
                // Safety: the function wrote the slot.
                let changed = heap.read(unsafe { out.assume_init() });
                let expected = if upper != 0 {
                    s.to_ascii_uppercase()
                } else {
                    s.to_ascii_lowercase()
                };
                assert_eq!(changed, expected, "{case}");
            }
            string_replace(
                data,
                len,
                from.as_ptr(),
                from.len(),
                to.as_ptr(),
                to.len(),
                heap.bytes,
                heap.alloc(),
                out.as_mut_ptr().cast(),
            );
            // Safety: the function wrote the slot.
            assert_eq!(
                heap.read(unsafe { out.assume_init() }),
                s.replace(&*from, &to),
                "{case}"
            );
            let times = rng.below(4);
            string_repeat(
                data,
                len,
                times,
                0,
                heap.bytes,
                heap.alloc(),
                out.as_mut_ptr().cast(),
            );
            // Safety: the function wrote the slot.
            assert_eq!(
                heap.read(unsafe { out.assume_init() }),
                s.repeat(times as usize)
            );
        }
    }

    #[test]
    fn parsing_and_formatting_agree_with_rust() {
        let heap = Heap::new();
        let mut rng = Rng(0x5eed_0006);
        let mut samples: Vec<String> = [
            "0",
            "-0",
            "+1",
            "42",
            "-9223372036854775808",
            "9223372036854775807",
            "9223372036854775808",
            "",
            " 1",
            "1 ",
            "1e3",
            "2.5",
            "-.5",
            "inf",
            "-inf",
            "NaN",
            "nan",
            "1_000",
            "0x10",
            "١٢",
            "+",
            "-",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for _ in 0..2_000 {
            samples.push(match rng.below(3) {
                0 => (rng.next() as i64 >> rng.below(64)).to_string(),
                1 => f64::from_bits(rng.next()).to_string(),
                _ => rng.string(4),
            });
        }
        for s in &samples {
            let value = heap.string(s);
            let mut int = 0i64;
            let ok = string_parse_i64(
                heap.data(value),
                value.len,
                std::ptr::addr_of_mut!(int).cast(),
            );
            assert_eq!((ok != 0).then_some(int), s.parse::<i64>().ok(), "{s:?}");
            let mut float = 0f64;
            let ok = string_parse_f64(
                heap.data(value),
                value.len,
                std::ptr::addr_of_mut!(float).cast(),
            );
            let expected = s.parse::<f64>().ok();
            assert_eq!(ok != 0, expected.is_some(), "{s:?}");
            if let Some(expected) = expected {
                assert!(
                    expected.to_bits() == float.to_bits() || (expected.is_nan() && float.is_nan())
                );
            }
        }
        let mut out = std::mem::MaybeUninit::<RawString>::uninit();
        for value in [0i64, -1, i64::MIN, i64::MAX, 1234567890] {
            string_from_i64(value, heap.bytes, heap.alloc(), out.as_mut_ptr().cast());
            // Safety: the function wrote the slot.
            assert_eq!(heap.read(unsafe { out.assume_init() }), value.to_string());
        }
        for value in [0.1f64, 1.0, -0.0, 1e300, f64::NAN, f64::INFINITY] {
            string_from_f64(value, heap.bytes, heap.alloc(), out.as_mut_ptr().cast());
            // Safety: the function wrote the slot.
            assert_eq!(
                heap.read(unsafe { out.assume_init() }),
                format!("{value:?}")
            );
        }
        string_from_i128(
            u64::MAX,
            u64::MAX,
            1,
            heap.bytes,
            heap.alloc(),
            out.as_mut_ptr().cast(),
        );
        // Safety: the function wrote the slot.
        assert_eq!(heap.read(unsafe { out.assume_init() }), "-1");
        string_from_bool(1, heap.bytes, heap.alloc(), out.as_mut_ptr().cast());
        // Safety: the function wrote the slot.
        assert_eq!(heap.read(unsafe { out.assume_init() }), "true");
    }

    #[test]
    fn the_empty_string_and_empty_arguments() {
        let heap = Heap::new();
        let empty = heap.string("");
        assert_eq!(heap.read(empty), "");
        let grown = heap.append(empty, "x");
        assert_eq!(heap.read(grown), "x");
        assert_eq!(heap.read(empty), "");
        // An empty `str` may come with a null data pointer.
        assert_eq!(string_eq(std::ptr::null(), 0, std::ptr::null(), 0), 1);
        assert_eq!(string_find(heap.data(grown), 1, std::ptr::null(), 0), 0);
        let unchanged = heap.append(grown, "");
        assert_eq!(unchanged, grown);
    }
}
