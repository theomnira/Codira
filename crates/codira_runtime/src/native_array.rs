//! Copyright (c) 2026 Omnira CJSC
//! Date: November 5, 2026
//!
//! Functionality:
//! - The runtime half of growable arrays: `codira.array_grow`, which moves an
//!   array's elements to a larger buffer, and the element-wise operations
//!   generated code hands to the runtime instead of emitting them inline
//!   (`sort`, `binary_search`, `reverse`).
//! - The number formatting of the byte-buffer methods of `[u8]`:
//!   `codira.bytes_format_int` (`push_int`) and
//!   `codira.bytes_format_json_f64` (`push_json_float`) write the text into
//!   the spare capacity generated code has made room in, and return its
//!   length.
//!
//! The compiler side is `codira_codegen::intrinsics::array_functions` and
//! `codira_codegen::ir::body::arrays`; names and signatures must match the
//! prototypes declared there. Nothing here panics: these are called from
//! generated code, where unwinding is not defined. A request the heap cannot
//! satisfy ends the process with a message, as an allocation does.

use std::ffi;

use codira_memory::gc::{GcPtr, GcRuntime};

use crate::{FunctionDefinition, IntoFunctionDefinition};

/// Makes room in the array `array` for at least `min_capacity` elements.
/// The array keeps its handle; its elements may move.
extern "C" fn array_grow(
    array: *const *mut ffi::c_void,
    min_capacity: usize,
    alloc_handle: *mut ffi::c_void,
) {
    // Safety: generated code passes the allocator handle the runtime set.
    let allocator = std::mem::ManuallyDrop::new(unsafe { crate::get_allocator(alloc_handle) });
    let handle = GcPtr::from(array);
    // Safety: `array` is a live array; arrays are never shared between
    // threads (only scalars and channels cross between tasks).
    if let Err(err) = unsafe { allocator.try_grow_array(handle, min_capacity) } {
        let ty = allocator.ptr_type(handle);
        crate::heap_exhausted(&allocator, &ty, None, &err);
    }
}

/// The element types `sort` and `binary_search` know how to order, as
/// generated code numbers them (`codira_codegen::ir::body::arrays`).
#[derive(Clone, Copy)]
enum Kind {
    I8,
    I16,
    I32,
    I64,
    I128,
    U8,
    U16,
    U32,
    U64,
    U128,
    F32,
    F64,
    Str,
}

impl Kind {
    fn from_code(code: u32) -> Option<Kind> {
        Some(match code {
            0 => Kind::I8,
            1 => Kind::I16,
            2 => Kind::I32,
            3 => Kind::I64,
            4 => Kind::I128,
            5 => Kind::U8,
            6 => Kind::U16,
            7 => Kind::U32,
            8 => Kind::U64,
            9 => Kind::U128,
            10 => Kind::F32,
            11 => Kind::F64,
            12 => Kind::Str,
            _ => return None,
        })
    }
}

/// A `str` as generated code lays it out in an array.
#[repr(C)]
#[derive(Clone, Copy)]
struct StrSlot {
    data: *const u8,
    len: usize,
}

impl StrSlot {
    fn bytes(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            // Safety: a `str` describes `len` readable bytes.
            unsafe { std::slice::from_raw_parts(self.data, self.len) }
        }
    }
}

/// Runs `f` on the `len` elements at `data`, viewed as a slice of `T`.
///
/// # Safety
///
/// `data` must hold `len` initialized, aligned values of `T` (a pointer
/// that may be dangling when `len` is 0).
unsafe fn with_slice<T, R>(data: *mut u8, len: usize, f: impl FnOnce(&mut [T]) -> R) -> R {
    if len == 0 {
        f(&mut [])
    } else {
        f(std::slice::from_raw_parts_mut(data.cast::<T>(), len))
    }
}

/// Dispatches `$body` over the element type of `$kind`, binding the slice
/// at `$data` to `$slice` and the comparison of two elements to `$cmp`.
macro_rules! by_kind {
    ($kind:expr, $data:expr, $len:expr, |$slice:ident, $cmp:ident, $t:ident| $body:expr) => {
        match $kind {
            Kind::I8 => by_kind!(@int i8, $data, $len, $slice, $cmp, $t, $body),
            Kind::I16 => by_kind!(@int i16, $data, $len, $slice, $cmp, $t, $body),
            Kind::I32 => by_kind!(@int i32, $data, $len, $slice, $cmp, $t, $body),
            Kind::I64 => by_kind!(@int i64, $data, $len, $slice, $cmp, $t, $body),
            Kind::I128 => by_kind!(@int i128, $data, $len, $slice, $cmp, $t, $body),
            Kind::U8 => by_kind!(@int u8, $data, $len, $slice, $cmp, $t, $body),
            Kind::U16 => by_kind!(@int u16, $data, $len, $slice, $cmp, $t, $body),
            Kind::U32 => by_kind!(@int u32, $data, $len, $slice, $cmp, $t, $body),
            Kind::U64 => by_kind!(@int u64, $data, $len, $slice, $cmp, $t, $body),
            Kind::U128 => by_kind!(@int u128, $data, $len, $slice, $cmp, $t, $body),
            Kind::F32 => {
                #[allow(dead_code)]
                type $t = f32;
                let $cmp = |a: &f32, b: &f32| a.total_cmp(b);
                with_slice::<f32, _>($data, $len, |$slice| $body)
            }
            Kind::F64 => {
                #[allow(dead_code)]
                type $t = f64;
                let $cmp = |a: &f64, b: &f64| a.total_cmp(b);
                with_slice::<f64, _>($data, $len, |$slice| $body)
            }
            Kind::Str => {
                #[allow(dead_code)]
                type $t = StrSlot;
                let $cmp = |a: &StrSlot, b: &StrSlot| a.bytes().cmp(b.bytes());
                with_slice::<StrSlot, _>($data, $len, |$slice| $body)
            }
        }
    };
    (@int $ty:ty, $data:expr, $len:expr, $slice:ident, $cmp:ident, $t:ident, $body:expr) => {{
        #[allow(dead_code)]
                type $t = $ty;
        let $cmp = |a: &$ty, b: &$ty| a.cmp(b);
        with_slice::<$ty, _>($data, $len, |$slice| $body)
    }};
}

/// Sorts the `len` elements at `data` in ascending order. Not stable
/// (Rust's `sort_unstable_by`, a pattern-defeating quicksort: O(n log n)
/// worst case, in place, no allocation). Floats are ordered by IEEE 754
/// `totalOrder`: `-NaN < -inf < .. < -0.0 < 0.0 < .. < inf < NaN`.
extern "C" fn array_sort(data: *mut u8, len: usize, kind: u32) {
    let Some(kind) = Kind::from_code(kind) else {
        return;
    };
    // Safety: generated code passes the elements and length of a live array
    // whose element type `kind` describes.
    unsafe {
        by_kind!(kind, data, len, |slice, cmp, T| slice.sort_unstable_by(cmp));
    }
}

/// Searches the sorted `len` elements at `data` for the value at `key`;
/// returns an index of an equal element, or -1. On an unsorted array the
/// result is some index or -1, never out of range.
extern "C" fn array_binary_search(data: *mut u8, len: usize, kind: u32, key: *const u8) -> i64 {
    let Some(kind) = Kind::from_code(kind) else {
        return -1;
    };
    // Safety: as for `array_sort`; `key` points at one value of the element
    // type.
    let found: Option<usize> = unsafe {
        by_kind!(kind, data, len, |slice, cmp, T| {
            let key = &*key.cast::<T>();
            slice.binary_search_by(|probe| cmp(probe, key)).ok()
        })
    };
    found.map_or(-1, |index| index as i64)
}

/// Reverses the `len` elements of `stride` bytes each at `data`, in place.
extern "C" fn array_reverse(data: *mut u8, len: usize, stride: usize) {
    if len < 2 || stride == 0 {
        return;
    }
    // Safety: generated code passes the elements of a live array.
    unsafe {
        match stride {
            1 => with_slice::<u8, _>(data, len, <[u8]>::reverse),
            2 => with_slice::<u16, _>(data, len, <[u16]>::reverse),
            4 => with_slice::<u32, _>(data, len, <[u32]>::reverse),
            8 => with_slice::<u64, _>(data, len, <[u64]>::reverse),
            _ => {
                let (mut i, mut j) = (0, len - 1);
                while i < j {
                    std::ptr::swap_nonoverlapping(
                        data.add(i * stride),
                        data.add(j * stride),
                        stride,
                    );
                    i += 1;
                    j -= 1;
                }
            }
        }
    }
}

/// The bytes [`bytes_format_int`] may write: `-9223372036854775808` and
/// `18446744073709551615` are 20 long.
pub const FORMAT_INT_CAPACITY: usize = 20;

/// Two decimal digits for every value below 100, for [`format_u64`].
const DIGIT_PAIRS: &[u8; 200] = b"\
0001020304050607080910111213141516171819\
2021222324252627282930313233343536373839\
4041424344454647484950515253545556575859\
6061626364656667686970717273747576777879\
8081828384858687888990919293949596979899";

/// The decimal digits of `value`, right-aligned in `buf`; returns where they
/// start.
fn format_u64(mut value: u64, buf: &mut [u8; FORMAT_INT_CAPACITY]) -> usize {
    let mut at = FORMAT_INT_CAPACITY;
    while value >= 100 {
        let pair = (value % 100) as usize * 2;
        value /= 100;
        at -= 2;
        buf[at..at + 2].copy_from_slice(&DIGIT_PAIRS[pair..pair + 2]);
    }
    if value >= 10 {
        let pair = value as usize * 2;
        at -= 2;
        buf[at..at + 2].copy_from_slice(&DIGIT_PAIRS[pair..pair + 2]);
    } else {
        at -= 1;
        buf[at] = b'0' + value as u8;
    }
    at
}

/// `bytes.push_int(n)`: the decimal text of `value` -- an `i64` when
/// `signed` is non-zero, a `u64` otherwise -- at `out`, returning its
/// length. Generated code has made room for [`FORMAT_INT_CAPACITY`] bytes
/// there. The text is what `to_string()` writes for the same integer.
extern "C" fn bytes_format_int(out: *mut u8, value: u64, signed: u8) -> usize {
    let mut buf = [0u8; FORMAT_INT_CAPACITY];
    let negative = signed != 0 && (value as i64) < 0;
    let magnitude = if negative {
        (value as i64).unsigned_abs()
    } else {
        value
    };
    let start = format_u64(magnitude, &mut buf);
    let digits = &buf[start..];
    let len = digits.len() + usize::from(negative);
    debug_assert!(len <= FORMAT_INT_CAPACITY);
    // Safety: generated code passes `FORMAT_INT_CAPACITY` writable bytes of
    // the array's spare capacity, and `len` is at most that.
    unsafe {
        let mut dest = out;
        if negative {
            *dest = b'-';
            dest = dest.add(1);
        }
        std::ptr::copy_nonoverlapping(digits.as_ptr(), dest, digits.len());
    }
    len
}

/// `bytes.push_json_float(x)`: the JSON text of `value` at `out` (room for
/// [`crate::native_json::FORMAT_F64_CAPACITY`] bytes), as
/// `codira_json_format_f64` writes it; returns its length.
extern "C" fn bytes_format_json_f64(value: f64, out: *mut u8) -> usize {
    crate::native_json::codira_json_format_f64(value, out as usize)
}

/// Returns the definitions of every function in this module, for the
/// runtime's dispatch table.
pub(crate) fn functions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(
            array_grow as extern "C" fn(*const *mut ffi::c_void, usize, *mut ffi::c_void),
            "codira.array_grow",
        ),
        IntoFunctionDefinition::into(
            array_sort as extern "C" fn(*mut u8, usize, u32),
            "codira.array_sort",
        ),
        IntoFunctionDefinition::into(
            array_binary_search as extern "C" fn(*mut u8, usize, u32, *const u8) -> i64,
            "codira.array_binary_search",
        ),
        IntoFunctionDefinition::into(
            array_reverse as extern "C" fn(*mut u8, usize, usize),
            "codira.array_reverse",
        ),
        IntoFunctionDefinition::into(
            bytes_format_int as extern "C" fn(*mut u8, u64, u8) -> usize,
            "codira.bytes_format_int",
        ),
        IntoFunctionDefinition::into(
            bytes_format_json_f64 as extern "C" fn(f64, *mut u8) -> usize,
            "codira.bytes_format_json_f64",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_text(value: u64, signed: bool) -> String {
        let mut out = [0u8; FORMAT_INT_CAPACITY];
        let len = bytes_format_int(out.as_mut_ptr(), value, u8::from(signed));
        String::from_utf8(out[..len].to_vec()).unwrap()
    }

    /// Every boundary, and a seeded spread of values, are written as Rust's
    /// `to_string` writes them.
    #[test]
    fn integers_are_written_as_to_string_writes_them() {
        for value in [0i64, 1, -1, 9, 10, 99, 100, -100, i64::MAX, i64::MIN] {
            assert_eq!(int_text(value as u64, true), value.to_string());
        }
        for value in [0u64, 9, 10, 1 << 63, u64::MAX] {
            assert_eq!(int_text(value, false), value.to_string());
        }
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..100_000 {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let bits = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            let value = bits >> (bits % 64);
            assert_eq!(int_text(value, false), value.to_string());
            assert_eq!(int_text(value, true), (value as i64).to_string());
        }
    }
}
