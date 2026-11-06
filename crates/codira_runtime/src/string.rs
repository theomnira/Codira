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
//! through `a`. Appending (`s + t`, `s += t`) writes in place only past the
//! end of every existing value: when `s` ends exactly at the fill and the
//! buffer has room, the bytes go there and the fill rises. Any other append
//! copies into a new buffer, doubling the capacity when `s` was at the fill
//! (it is being built up) and exact otherwise. A loop of `s += piece` is
//! therefore amortized O(1) per byte, like Rust's `String::push_str`,
//! without a separate builder type.
//!
//! # Who may raise the fill: the owner word
//!
//! Two values on one buffer can both end at the fill (`let b = a`), and
//! appending to both must not let both write the same bytes. Within one
//! thread that cannot happen: appends run one after the other, the second
//! sees the fill the first raised and copies. Across threads it could, so
//! every buffer records, in its first [`OWNER_BYTES`] bytes, which thread
//! may raise its fill (the *owner word*; a string's bytes start after it):
//!
//! - the thread that allocated the buffer, identified by its thread
//!   pointer ([`thread_token`]: `fs:0` on x86-64 Linux, `TPIDR_EL0` on
//!   AArch64 Linux), when the platform has one. Only that thread appends in
//!   place, with plain (relaxed) loads and stores and no read-modify-write;
//!   any other thread appending to a value on the buffer copies, so it
//!   never writes the buffer at all;
//! - [`SHARED`] (zero) where no thread pointer is known. Every thread then
//!   raises the fill with a compare-and-swap, so two appends racing on one
//!   buffer cannot both win.
//!
//! Why the non-atomic path is sound: (1) the owner word is written before
//! the buffer is reachable by anything and never changes, so every thread
//! reads the same owner; (2) no two live threads have the same thread
//! pointer, so at most one thread at a time passes the owner check, and the
//! fill and the bytes above it are written by that thread alone; (3) a
//! thread pointer is reused only after its thread has exited, and a new
//! thread starts after the old one's exit (the C library's thread cache
//! hands the memory over under a lock), so the new owner sees every write
//! the old one made; (4) other threads read only the bytes of values they
//! were given, which are below the fill and never written again, and they
//! received those values through whatever synchronisation handed them over
//! (Codira tasks share nothing but channels of scalars, so that is the
//! host). A buffer whose owner word is stale -- restored from a heap
//! snapshot made by another process -- is still matched by at most one
//! thread, which is all the argument needs. A thread that cannot prove
//! ownership copies: the safe path.
//!
//! Compiled code makes the same check inline (`codira_codegen`'s
//! `ir::body::fabric`): when the owner word is the calling thread's thread
//! pointer, the value ends at the fill and the room suffices, an append is
//! a copy of the bytes and a store of the new fill, with no call. Anything
//! else calls [`string_append`] here.
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
    gc::{AllocError, Array as _, GcPtr, HasIndirectionPtr, RawGcPtr},
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

/// The bytes at the start of every string buffer that hold its owner word
/// (see the module docs). A string's bytes start at this offset or later.
pub const OWNER_BYTES: usize = std::mem::size_of::<usize>();

/// The owner word of a buffer any thread may append to in place, with a
/// compare-and-swap of the fill.
pub const SHARED: usize = 0;

/// The calling thread's identity as the owner of string buffers: its thread
/// pointer, which no other live thread has and which is never zero, or
/// [`SHARED`] on a platform where it is not known. Compiled code reads the
/// same register (`llvm.thread.pointer`), so the two agree; if they did not,
/// the inline check would fail and the call here decide, which costs time
/// and nothing else.
#[inline(always)]
pub fn thread_token() -> usize {
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        let token: usize;
        // Safety: on x86-64 Linux `fs:0` holds the thread control block's
        // pointer to itself (the TLS ABI requires it); reading it has no
        // side effect.
        unsafe {
            std::arch::asm!(
                "mov {}, qword ptr fs:[0]",
                out(reg) token,
                options(nostack, preserves_flags, readonly, pure)
            );
        }
        token
    }
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        let token: usize;
        // Safety: reading the thread pointer register has no side effect.
        unsafe {
            std::arch::asm!(
                "mrs {}, tpidr_el0",
                out(reg) token,
                options(nomem, nostack, preserves_flags, pure)
            );
        }
        token
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", target_os = "linux")
    )))]
    {
        SHARED
    }
}

/// The owner word of the string buffer `buf`.
///
/// # Safety
///
/// `buf` must be a live string buffer.
unsafe fn owner(buf: *const *mut u8) -> usize {
    elements(buf).cast::<usize>().read()
}

/// The first byte of the string `s`.
///
/// # Safety
///
/// `s.buf` must be a live string buffer.
unsafe fn data_of(s: RawString) -> *mut u8 {
    elements(s.buf).add(s.start)
}

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
            Err(err) => crate::heap_exhausted(&self.gc, &self.ty, None, &err),
        };
        // Safety: the elements below `length` are written by the caller
        // before the array is reachable from Codira code.
        unsafe { handle.set_length(length) };
        let raw: codira_memory::gc::RawGcPtr = handle.as_raw().into();
        raw.cast()
    }

    /// A new string buffer owned by this thread, with room for `capacity`
    /// bytes of text, and the value of its first `len` bytes (which the
    /// caller writes before the value is used), ending at the fill.
    fn buffer(&self, capacity: usize, len: usize) -> RawString {
        let total = capacity
            .max(len)
            .checked_add(OWNER_BYTES)
            .unwrap_or_else(|| report_capacity_overflow());
        let buf = self.array(total, OWNER_BYTES + len);
        // Safety: the array was made with room for the owner word, and is
        // not reachable by anything else yet.
        unsafe { elements(buf).cast::<usize>().write(thread_token()) };
        RawString {
            buf,
            start: OWNER_BYTES,
            len,
        }
    }

    /// A new string buffer holding `parts` one after another, with room for
    /// `capacity` bytes (at least their total).
    fn string(&self, parts: &[&[u8]], capacity: usize) -> RawString {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        let value = self.buffer(capacity, len);
        let mut offset = 0;
        for part in parts {
            // Safety: the new buffer has room for `len` bytes, and no part
            // can overlap it (it did not exist when they were made).
            unsafe {
                std::ptr::copy_nonoverlapping(
                    part.as_ptr(),
                    data_of(value).add(offset),
                    part.len(),
                );
            }
            offset += part.len();
        }
        value
    }
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
    let value = reserve(current, extra.len(), ty, alloc);
    // The reserved bytes belong to no other value; `extra` is below some
    // value's end, so it cannot overlap them.
    std::ptr::copy_nonoverlapping(extra.as_ptr(), data_of(value).add(len), extra.len());
    value
}

/// Where an append can put its bytes.
enum Room {
    /// In place: the fill was raised over the bytes after the value.
    Claimed,
    /// Not in place; `bool` says whether the value ends at the fill (it is
    /// being built up, so a new buffer should double).
    Copy(bool),
}

/// Claims `extra` bytes right after `s` in its buffer, by the rule of the
/// module docs: as the buffer's owner thread with a plain store of the fill,
/// on a [`SHARED`] buffer with a compare-and-swap, and never on a buffer
/// another thread owns.
///
/// # Safety
///
/// `s` must be a live string with a buffer.
unsafe fn claim(s: RawString, extra: usize) -> Room {
    let header = header(s.buf);
    let end = s.start + s.len;
    let owner = owner(s.buf);
    if owner == SHARED {
        let fill = header.length.load(Ordering::Acquire);
        let at_fill = fill == end;
        if at_fill
            && header.capacity - end >= extra
            && header
                .length
                .compare_exchange(end, end + extra, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return Room::Claimed;
        }
        Room::Copy(at_fill)
    } else {
        // Only the owner ever stores the fill of an owned buffer; another
        // thread reads it only to choose a capacity.
        let fill = header.length.load(Ordering::Relaxed);
        let at_fill = fill == end;
        if owner == thread_token() && at_fill && header.capacity - end >= extra {
            header.length.store(end + extra, Ordering::Relaxed);
            return Room::Claimed;
        }
        Room::Copy(at_fill)
    }
}

/// `s` extended by `extra` bytes that belong to no other value and that the
/// caller writes (at `data_of(result) + s.len`) before the value is used:
/// in `s`'s buffer when [`claim`] allows, otherwise in a new buffer holding
/// a copy of `s`, of double the size when `s` was being built up.
///
/// # Safety
///
/// `s` must be a live string; `ty`/`alloc` the `[u8]` handles.
unsafe fn reserve(s: RawString, extra: usize, ty: *const c_void, alloc: *mut c_void) -> RawString {
    let at_fill = if s.buf.is_null() {
        false
    } else {
        match claim(s, extra) {
            Room::Claimed => {
                return RawString {
                    len: s.len + extra,
                    ..s
                }
            }
            Room::Copy(at_fill) => at_fill,
        }
    };
    let needed = s
        .len
        .checked_add(extra)
        .unwrap_or_else(|| report_capacity_overflow());
    let capacity = if at_fill {
        needed.max(s.len.saturating_mul(2)).max(32)
    } else {
        needed
    };
    let alloc = Alloc::new(ty, alloc);
    let value = alloc.buffer(capacity, needed);
    // Read after the allocation: the old buffer is kept alive by `s.buf`,
    // which is still in use here.
    let old = string_bytes(s.buf, s.start, s.len);
    std::ptr::copy_nonoverlapping(old.as_ptr(), data_of(value), old.len());
    value
}

/// `String.with_capacity`: the empty string at the fill of a new buffer
/// with room for `capacity` bytes, which the appends after it fill in place.
/// A compiled format plan starts with one, sized for the whole result.
extern "C" fn string_with_capacity(
    capacity: usize,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    // Safety: generated code passes its handles and a `String` slot.
    unsafe {
        let alloc = Alloc::new(ty, alloc);
        write(out, alloc.buffer(capacity, 0));
    }
}

/// The decimal digits of `value` at the end of `buffer`; returns where they
/// start. Two digits at a time, as Rust's own formatter does.
pub(crate) fn decimal_u128(buffer: &mut [u8; 40], mut value: u128) -> usize {
    const PAIRS: &[u8; 200] = b"0001020304050607080910111213141516171819\
        2021222324252627282930313233343536373839\
        4041424344454647484950515253545556575859\
        6061626364656667686970717273747576777879\
        8081828384858687888990919293949596979899";
    let mut at = buffer.len();
    while value > u128::from(u64::MAX) {
        let rest = value % 10_000_000_000_000_000_000;
        value /= 10_000_000_000_000_000_000;
        let mut low = rest as u64;
        for _ in 0..19 {
            at -= 1;
            buffer[at] = b'0' + (low % 10) as u8;
            low /= 10;
        }
    }
    let mut value = value as u64;
    while value >= 100 {
        let pair = (value % 100) as usize * 2;
        value /= 100;
        at -= 2;
        buffer[at..at + 2].copy_from_slice(&PAIRS[pair..pair + 2]);
    }
    if value >= 10 {
        let pair = value as usize * 2;
        at -= 2;
        buffer[at..at + 2].copy_from_slice(&PAIRS[pair..pair + 2]);
    } else {
        at -= 1;
        buffer[at] = b'0' + value as u8;
    }
    at
}

/// An integer as its two 64-bit halves (sign- or zero-extended to 128 bits)
/// and its signedness, written in decimal at the end of `buffer`.
fn integer_text(buffer: &mut [u8; 40], low: u64, high: u64, signed: u8) -> &[u8] {
    let bits = (u128::from(high) << 64) | u128::from(low);
    let negative = signed != 0 && (bits as i128) < 0;
    let magnitude = if negative {
        (bits as i128).unsigned_abs()
    } else {
        bits
    };
    let mut at = decimal_u128(buffer, magnitude);
    if negative {
        at -= 1;
        buffer[at] = b'-';
    }
    &buffer[at..]
}

/// `s += n.to_string()` for an integer `n`, without the temporary string:
/// the digits are written into `s`'s buffer (or the new one it grows into).
/// Compiled code writes them inline when the room is there and calls this
/// otherwise.
#[allow(clippy::too_many_arguments)]
extern "C" fn string_append_int(
    buf: *const c_void,
    start: usize,
    len: usize,
    low: u64,
    high: u64,
    signed: u8,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    let mut digits = [0u8; 40];
    let text = integer_text(&mut digits, low, high, signed);
    // Safety: generated code passes a live `String` and its handles.
    unsafe { write(out, append(buf.cast(), start, len, text, ty, alloc)) }
}

/// `s += x.to_string()` for a float `x` (an `f32` widened when `is_f32` is
/// set): the shortest digits that read back, Rust's `{:?}`, written into
/// `s`'s buffer with no temporary string.
#[allow(clippy::too_many_arguments)]
extern "C" fn string_append_float(
    buf: *const c_void,
    start: usize,
    len: usize,
    value: f64,
    is_f32: u8,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    let mut text = format::Small::new();
    if is_f32 != 0 {
        std::fmt::Write::write_fmt(&mut text, format_args!("{:?}", value as f32))
    } else {
        std::fmt::Write::write_fmt(&mut text, format_args!("{value:?}"))
    }
    .expect("writing to memory cannot fail");
    // Safety: as for `string_append_int`.
    unsafe {
        write(
            out,
            append(buf.cast(), start, len, text.as_bytes(), ty, alloc),
        );
    }
}

/// The offset of the first `byte` in `data[..len]`, or -1: the scan behind
/// `lines()` and a `split` on one ASCII byte, used lazily (a pipeline or a
/// `for` loop over the pieces) as well as by the array-making versions.
extern "C" fn string_find_byte(data: *const u8, len: usize, byte: u8) -> i64 {
    // Safety: generated code passes a string's bytes.
    let hay = unsafe { bytes_at(data, len) };
    memchr::memchr(byte, hay).map_or(-1, |at| at as i64)
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
    string_array_with(
        s,
        pieces.len(),
        |piece| pieces.iter().copied().for_each(piece),
        ty,
        alloc,
    )
}

/// Makes a `[String]` of `count` pieces of `s`, whose byte ranges `pieces`
/// gives one after another (to the callback it is passed), all sharing `s`'s
/// buffer. `pieces` runs after the allocation, so it must read `s` afresh.
///
/// # Safety
///
/// As for [`string_array`]; `pieces` must give exactly `count` ranges.
unsafe fn string_array_with(
    s: RawString,
    count: usize,
    pieces: impl FnOnce(&mut dyn FnMut((usize, usize))),
    ty: *const c_void,
    alloc: *mut c_void,
) -> *const *mut u8 {
    let alloc = Alloc::new(ty, alloc);
    let array = alloc.array(count, count);
    let slots = elements(array).cast::<RawString>();
    let mut index = 0;
    pieces(&mut |(offset, len)| {
        assert!(index < count, "more pieces than were counted");
        slots.add(index).write(RawString {
            buf: s.buf,
            start: s.start + offset,
            len,
        });
        index += 1;
    });
    assert_eq!(index, count, "fewer pieces than were counted");
    array
}

/// The byte ranges of the pieces of `bytes.split(sep)` for a one-byte
/// separator, as Rust's `str::split` gives them, found with `memchr`. (A
/// one-byte UTF-8 string is ASCII, so a match is always a whole character.)
fn split_on_byte(bytes: &[u8], sep: u8, piece: &mut dyn FnMut((usize, usize))) {
    let mut start = 0;
    for at in memchr::memchr_iter(sep, bytes) {
        piece((start, at - start));
        start = at + 1;
    }
    piece((start, bytes.len() - start));
}

/// The byte ranges of the lines of `bytes`, as Rust's `str::lines` gives
/// them: split at `\n`, a `\r` right before it dropped, and no empty piece
/// after a final line break.
fn lines_of(bytes: &[u8], piece: &mut dyn FnMut((usize, usize))) {
    let mut start = 0;
    for at in memchr::memchr_iter(b'\n', bytes) {
        let end = if at > start && bytes[at - 1] == b'\r' {
            at - 1
        } else {
            at
        };
        piece((start, end - start));
        start = at + 1;
    }
    if start < bytes.len() {
        piece((start, bytes.len() - start));
    }
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
        let value = RawString { buf, start, len };
        if sep_len == 1 {
            // One ASCII byte: count the pieces, then make them, with no
            // list in between.
            let byte = *sep;
            let count = memchr::memchr_iter(byte, string_bytes(buf, start, len)).count() + 1;
            let pieces = |piece: &mut dyn FnMut((usize, usize))| {
                split_on_byte(string_bytes(buf, start, len), byte, piece);
            };
            return string_array_with(value, count, pieces, ty, alloc).cast();
        }
        let whole = text(string_bytes(buf, start, len));
        let sep = text(bytes_at(sep, sep_len));
        let pieces: Vec<(usize, usize)> = whole.split(sep).map(|p| range_in(whole, p)).collect();
        string_array(value, &pieces, ty, alloc).cast()
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
        // Counted first, then made, with no list in between.
        let mut count = 0;
        lines_of(string_bytes(buf, start, len), &mut |_| count += 1);
        let pieces = |piece: &mut dyn FnMut((usize, usize))| {
            lines_of(string_bytes(buf, start, len), piece);
        };
        string_array_with(RawString { buf, start, len }, count, pieces, ty, alloc).cast()
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
        let bytes = std::slice::from_raw_parts_mut(data_of(value), len);
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
        let total = len
            .checked_mul(count)
            .unwrap_or_else(|| report_capacity_overflow());
        let alloc = Alloc::new(ty, alloc);
        // The source is read after the allocation: `data` points into a
        // buffer the caller's `String` keeps alive.
        let value = alloc.buffer(total, total);
        let bytes = bytes_at(data, len);
        let target = data_of(value);
        for index in 0..count {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.add(index * len), len);
        }
        write(out, value);
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
    ///
    /// Ends the process like an allocation by Codira code if the heap cannot
    /// provide the memory (see [`crate::HEAP_EXHAUSTED_EXIT_STATUS`]);
    /// [`StringRef::try_new`] returns the error instead.
    pub fn new(runtime: &'r Runtime, text: &str) -> Self {
        Self::try_new(runtime, text).unwrap_or_else(|err| {
            let ty = <u8 as HasStaticType>::type_info().array_type();
            crate::heap_exhausted(&runtime.gc, &ty, Some(text.len()), &err)
        })
    }

    /// A new string holding a copy of `text`, allocated on `runtime`'s heap,
    /// or why the heap could not provide the memory.
    pub fn try_new(runtime: &'r Runtime, text: &str) -> Result<Self, AllocError> {
        let ty = <u8 as HasStaticType>::type_info().array_type();
        let total = text
            .len()
            .checked_add(OWNER_BYTES)
            .unwrap_or_else(|| report_capacity_overflow());
        let mut handle = runtime.gc.try_alloc_array(&ty, total)?;
        // Safety: every element is written right below.
        unsafe { handle.set_length(total) };
        let raw: RawGcPtr = handle.as_raw().into();
        let buf = raw.cast::<*mut u8>();
        let value = RawString {
            buf,
            start: OWNER_BYTES,
            len: text.len(),
        };
        // Safety: the array was made with room for the owner word and
        // `text.len()` bytes. The host thread that makes the string owns
        // its buffer, as Codira code's thread owns the ones it makes.
        unsafe {
            elements(buf).cast::<usize>().write(thread_token());
            std::ptr::copy_nonoverlapping(text.as_ptr(), data_of(value), text.len());
        }
        Ok(Self {
            raw: value,
            runtime,
        })
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
            Err(err) => crate::heap_exhausted(&self.runtime.gc, &ty, None, &err),
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
            string_with_capacity as extern "C" fn(usize, T, A, O),
            "codira.string_with_capacity",
        ),
        IntoFunctionDefinition::into(
            string_append_int as extern "C" fn(H, usize, usize, u64, u64, u8, T, A, O),
            "codira.string_append_int",
        ),
        IntoFunctionDefinition::into(
            string_append_float as extern "C" fn(H, usize, usize, f64, u8, T, A, O),
            "codira.string_append_float",
        ),
        IntoFunctionDefinition::into(
            string_find_byte as extern "C" fn(P, usize, u8) -> i64,
            "codira.string_find_byte",
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
            // A program's `[u8]`, of exactly these bytes.
            // Safety: the type handle is `[u8]`.
            let alloc = unsafe { Alloc::new(self.bytes, self.alloc()) };
            let array = alloc.array(bytes.len(), bytes.len());
            // Safety: the array has room for the bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), elements(array), bytes.len());
            }
            array.cast()
        }

        /// `s` with `extra` appended the way compiled code's slow path
        /// appends, through the given runtime function.
        fn append_int(&self, value: RawString, n: i128) -> RawString {
            let bits = n as u128;
            let mut out = std::mem::MaybeUninit::<RawString>::uninit();
            string_append_int(
                value.buf.cast(),
                value.start,
                value.len,
                bits as u64,
                (bits >> 64) as u64,
                1,
                self.bytes,
                self.alloc(),
                out.as_mut_ptr().cast(),
            );
            // Safety: the function wrote the slot.
            unsafe { out.assume_init() }
        }

        fn append_float(&self, value: RawString, x: f64) -> RawString {
            let mut out = std::mem::MaybeUninit::<RawString>::uninit();
            string_append_float(
                value.buf.cast(),
                value.start,
                value.len,
                x,
                0,
                self.bytes,
                self.alloc(),
                out.as_mut_ptr().cast(),
            );
            // Safety: the function wrote the slot.
            unsafe { out.assume_init() }
        }

        fn with_capacity(&self, capacity: usize) -> RawString {
            let mut out = std::mem::MaybeUninit::<RawString>::uninit();
            string_with_capacity(capacity, self.bytes, self.alloc(), out.as_mut_ptr().cast());
            // Safety: the function wrote the slot.
            unsafe { out.assume_init() }
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

    /// Marks `value`'s buffer as one any thread may append to (with a
    /// compare-and-swap), as a platform without a known thread pointer
    /// makes every buffer.
    fn make_shared(value: RawString) {
        // Safety: a live buffer the test made; nothing else uses it yet.
        unsafe { elements(value.buf).cast::<usize>().write(SHARED) };
    }

    /// The fill of `value`'s buffer.
    fn fill_of(value: RawString) -> usize {
        // Safety: a live buffer the test made.
        unsafe { header(value.buf).length.load(Ordering::Acquire) }
    }

    /// No existing value ever changes, under random sequences of every way
    /// to make a value from another: appends of text, integers and floats,
    /// formatted holes, aliases (a copy of the three words), views in the
    /// middle or at the end of a buffer, buffers pre-sized for a format
    /// plan, and buffers marked shared. Each value is checked against a Rust
    /// `String` built the same way, after every step.
    #[test]
    fn no_value_changes_under_random_appends_and_aliases() {
        let heap = Heap::new();
        let mut rng = Rng(0x5eed_0007);
        for round in 0..40 {
            let mut values = vec![(heap.string(""), String::new())];
            let mut in_place = 0;
            for step in 0..600 {
                let from = rng.below(values.len() as u64) as usize;
                let (value, model) = values[from].clone();
                let (made, expected) = match rng.below(9) {
                    0 | 1 => {
                        let extra = rng.string(4);
                        (heap.append(value, &extra), model.clone() + &extra)
                    }
                    2 => {
                        let n = (rng.next() as i64 >> rng.below(64)) as i128;
                        (heap.append_int(value, n), format!("{model}{n}"))
                    }
                    3 => {
                        let x = f64::from_bits(rng.next());
                        (heap.append_float(value, x), format!("{model}{x:?}"))
                    }
                    4 => {
                        // An alias: the same three words.
                        (value, model.clone())
                    }
                    5 => {
                        // A view: a char-aligned range of the value.
                        let bounds: Vec<usize> = model
                            .char_indices()
                            .map(|(at, _)| at)
                            .chain([model.len()])
                            .collect();
                        let a = bounds[rng.below(bounds.len() as u64) as usize];
                        let b = bounds[rng.below(bounds.len() as u64) as usize];
                        let (a, b) = (a.min(b), a.max(b));
                        let view = RawString {
                            start: value.start + a,
                            len: b - a,
                            ..value
                        };
                        (view, model[a..b].to_owned())
                    }
                    6 => {
                        // A format plan's buffer, then the value appended.
                        let fresh = heap.with_capacity(rng.below(48) as usize);
                        (heap.append(fresh, &model), model.clone())
                    }
                    7 => {
                        // A formatted hole appended.
                        let n = rng.next() as i64;
                        let w = rng.below(24) as i64;
                        let mut out = std::mem::MaybeUninit::<RawString>::uninit();
                        format::tests_support::append_format_int(
                            value,
                            n,
                            w,
                            heap.bytes,
                            heap.alloc(),
                            out.as_mut_ptr().cast(),
                        );
                        // Safety: the function wrote the slot.
                        let made = unsafe { out.assume_init() };
                        (made, format!("{model}{n:0w$}", w = w as usize))
                    }
                    _ => {
                        // A fresh buffer any thread may append to.
                        let extra = rng.string(3);
                        let fresh = heap.string(&extra);
                        make_shared(fresh);
                        (fresh, extra)
                    }
                };
                if !value.buf.is_null() && made.buf == value.buf && made.len > value.len {
                    in_place += 1;
                }
                assert_eq!(heap.read(made), expected, "round {round} step {step}");
                values.push((made, expected));
                for (index, (value, model)) in values.iter().enumerate() {
                    assert_eq!(
                        heap.read(*value),
                        model,
                        "value {index} changed at round {round} step {step}"
                    );
                }
            }
            assert!(in_place > 50, "only {in_place} appends were in place");
        }
    }

    /// A value that can be sent to another thread in a test.
    #[derive(Clone, Copy)]
    struct Sent(RawString);
    // Safety: the test hands values to threads the way a host would; the
    // runtime's rules make that safe, which is what the test checks.
    unsafe impl Send for Sent {}

    /// Two threads append at the same time to values that end at their
    /// buffer's fill and have room, each value raced by both: values the
    /// main thread made (neither of the two owns the buffer), values one of
    /// the two made and sent to the other (it owns the buffer, the other
    /// does not), and values on shared buffers (both compare-and-swap).
    /// Each thread checks every value it made right away; at the end every
    /// value of both, and every base, is checked again. No append changed a
    /// value, and no two appends claimed the same bytes.
    #[test]
    fn two_threads_appending_on_one_buffer_change_no_value() {
        const BASES: usize = 1500;
        let heap = Heap::new();
        let (bytes, alloc) = (heap.bytes as usize, heap.alloc() as usize);
        let append = move |value: RawString, extra: &str| -> RawString {
            let mut out = std::mem::MaybeUninit::<RawString>::uninit();
            string_append(
                value.buf.cast(),
                value.start,
                value.len,
                extra.as_ptr(),
                extra.len(),
                bytes as *const c_void,
                alloc as *mut c_void,
                out.as_mut_ptr().cast(),
            );
            // Safety: the function wrote the slot.
            unsafe { out.assume_init() }
        };
        let read = |value: RawString| -> String {
            // Safety: every value the test makes is live and valid.
            unsafe { text(string_bytes(value.buf, value.start, value.len)).to_owned() }
        };
        // Bases at their buffer's fill with room for both threads' appends.
        let fresh_base = |shared: bool| {
            let value = heap.append(heap.with_capacity(64), "base");
            if shared {
                make_shared(value);
            }
            Sent(value)
        };
        let main_bases: Vec<Sent> = (0..BASES).map(|_| fresh_base(false)).collect();
        let shared_bases: Vec<Sent> = (0..BASES).map(|_| fresh_base(true)).collect();
        let fills: Vec<usize> = main_bases.iter().map(|b| fill_of(b.0)).collect();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let (send_own, receive_own) = std::sync::mpsc::channel::<Vec<Sent>>();
        let worker = |name: char,
                      own: Option<std::sync::mpsc::Sender<Vec<Sent>>>,
                      theirs: Option<std::sync::mpsc::Receiver<Vec<Sent>>>,
                      barrier: Arc<std::sync::Barrier>,
                      main_bases: Vec<Sent>,
                      shared_bases: Vec<Sent>| {
            move || {
                // Thread `a` makes bases on buffers it owns and hands them
                // to `b`, as a host would.
                let owned: Vec<Sent> = match (own, theirs) {
                    (Some(own), _) => {
                        let owned: Vec<Sent> = (0..BASES)
                            .map(|_| {
                                let empty = RawString {
                                    buf: std::ptr::null(),
                                    start: 0,
                                    len: 0,
                                };
                                Sent(append(empty, "base"))
                            })
                            .collect();
                        own.send(owned.clone()).unwrap();
                        owned
                    }
                    (None, Some(theirs)) => theirs.recv().unwrap(),
                    (None, None) => Vec::new(),
                };
                barrier.wait();
                let mut made = Vec::new();
                for index in 0..BASES {
                    for base in [main_bases[index], shared_bases[index], owned[index]] {
                        let extra = format!("{name}{index},");
                        let expected = format!("base{extra}");
                        let value = append(base.0, &extra);
                        assert_eq!(read(value), expected);
                        // And a chain built up from it.
                        let longer = append(value, &extra);
                        assert_eq!(read(longer), format!("{expected}{extra}"));
                        made.push((Sent(value), expected.clone()));
                        made.push((Sent(longer), format!("{expected}{extra}")));
                    }
                }
                for (value, expected) in &made {
                    assert_eq!(read(value.0), *expected);
                }
                (made, owned)
            }
        };
        let a = std::thread::spawn(worker(
            'a',
            Some(send_own),
            None,
            barrier.clone(),
            main_bases.clone(),
            shared_bases.clone(),
        ));
        let b = std::thread::spawn(worker(
            'b',
            None,
            Some(receive_own),
            barrier,
            main_bases.clone(),
            shared_bases.clone(),
        ));
        let (mut all, owned) = a.join().unwrap();
        all.extend(b.join().unwrap().0);
        for (value, expected) in &all {
            assert_eq!(read(value.0), *expected);
        }
        let mut shared_in_place = 0;
        for index in 0..BASES {
            assert_eq!(read(main_bases[index].0), "base");
            assert_eq!(read(shared_bases[index].0), "base");
            assert_eq!(read(owned[index].0), "base");
            // Neither thread owns the main thread's buffers, so neither
            // wrote them.
            assert_eq!(fill_of(main_bases[index].0), fills[index]);
            // One append to each shared base won the swap.
            shared_in_place += all
                .iter()
                .filter(|(value, _)| {
                    value.0.buf == shared_bases[index].0.buf
                        && value.0.start == shared_bases[index].0.start
                        && value.0.len == 4 + 2 + index.to_string().len()
                })
                .count();
        }
        assert_eq!(
            shared_in_place, BASES,
            "every shared base is appended to in place once"
        );
    }

    #[test]
    fn a_buffer_is_owned_by_the_thread_that_made_it() {
        let heap = Heap::new();
        let here = thread_token();
        let there = std::thread::spawn(thread_token).join().unwrap();
        if cfg!(all(
            any(target_arch = "x86_64", target_arch = "aarch64"),
            target_os = "linux"
        )) {
            assert_ne!(here, SHARED);
            assert_ne!(there, SHARED);
            assert_ne!(here, there, "two live threads share a thread pointer");
        }
        let value = heap.append(heap.with_capacity(64), "abc");
        // Safety: a live buffer.
        assert_eq!(unsafe { owner(value.buf) }, here);
        // The owner appends in place; a copy made elsewhere is a new buffer
        // owned by the thread that made it.
        let more = heap.append(value, "def");
        assert_eq!(more.buf, value.buf);
        let sent = Sent(more);
        let (bytes, alloc) = (heap.bytes as usize, heap.alloc() as usize);
        let elsewhere = std::thread::spawn(move || {
            let sent = sent;
            let value = sent.0;
            let mut out = std::mem::MaybeUninit::<RawString>::uninit();
            string_append(
                value.buf.cast(),
                value.start,
                value.len,
                "ghi".as_ptr(),
                3,
                bytes as *const c_void,
                alloc as *mut c_void,
                out.as_mut_ptr().cast(),
            );
            // Safety: the function wrote the slot.
            let made = unsafe { out.assume_init() };
            // Safety: a live buffer.
            (Sent(made), unsafe { owner(made.buf) }, thread_token())
        });
        let (made, made_owner, that_thread) = elsewhere.join().unwrap();
        assert_ne!(made.0.buf, more.buf, "another thread's buffer was written");
        assert_eq!(made_owner, that_thread);
        assert_eq!(heap.read(made.0), "abcdefghi");
        // The owner still appends in place.
        assert_eq!(heap.append(more, "!").buf, more.buf);
    }

    #[test]
    fn numbers_append_as_to_string_writes_them() {
        let heap = Heap::new();
        let mut rng = Rng(0x5eed_0008);
        let mut value = heap.string("");
        let mut model = String::new();
        for _ in 0..5_000 {
            let n: i128 = match rng.below(4) {
                0 => (rng.next() as i64) as i128,
                1 => rng.next() as i128,
                2 => ((rng.next() as u128) << 64 | rng.next() as u128) as i128,
                _ => [0, -1, i64::MIN as i128, i128::MIN, i128::MAX][rng.below(5) as usize],
            };
            value = heap.append_int(value, n);
            model.push_str(&n.to_string());
            let x = f64::from_bits(rng.next());
            value = heap.append_float(value, x);
            model.push_str(&format!("{x:?}"));
        }
        assert_eq!(heap.read(value), model);
        let mut buffer = [0u8; 40];
        for n in [
            0u128,
            9,
            10,
            99,
            100,
            u64::MAX as u128,
            u64::MAX as u128 + 1,
            u128::MAX,
        ] {
            let at = decimal_u128(&mut buffer, n);
            assert_eq!(std::str::from_utf8(&buffer[at..]).unwrap(), n.to_string());
        }
    }

    #[test]
    fn the_byte_scan_agrees_with_rust() {
        let mut rng = Rng(0x5eed_0009);
        for _ in 0..5_000 {
            let s = rng.string(40);
            let byte = [b'\n', b',', b'a', b' '][rng.below(4) as usize];
            let found = string_find_byte(s.as_ptr(), s.len(), byte);
            let expected = s.bytes().position(|b| b == byte);
            assert_eq!((found >= 0).then_some(found as usize), expected);
            let mut lines = Vec::new();
            lines_of(s.as_bytes(), &mut |(at, len)| lines.push(&s[at..at + len]));
            assert_eq!(lines, s.lines().collect::<Vec<_>>(), "{s:?}");
            let mut pieces = Vec::new();
            split_on_byte(s.as_bytes(), byte, &mut |(at, len)| {
                pieces.push(&s[at..at + len]);
            });
            let sep = char::from(byte).to_string();
            assert_eq!(pieces, s.split(&*sep).collect::<Vec<_>>(), "{s:?}");
        }
    }
}
