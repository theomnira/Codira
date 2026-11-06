//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The runtime half of format specs (`f"{x:>8.3}"`) and of the debug form
//!   (`f"{x:?}"`): the `codira.string_format_*` functions a hole with a spec
//!   calls, and the `codira.debug_*` functions the code generated for a
//!   type's debug form calls.
//!
//! # Format specs
//!
//! The compiler parses and checks a spec against the value's type, and a
//! hole becomes a call of the entry point for that type, with the spec as
//! four constant scalars: a fill character, a flags word, a width and a
//! precision (the layout is `codira_hir::FormatSpec::abi_flags`'s). Nothing
//! here parses a format string. The formatted text is appended to the
//! `String` being built -- in its buffer when it has room, as `+=` does --
//! so a hole makes no string of its own: the digits are made on the stack
//! ([`Small`]) and copied once, into the destination. What the spec means is what it
//! means in Rust's `std::fmt`, and the digits are Rust's own: a float's
//! `{:.3}` is `format!("{:.3}")` of its magnitude, so rounding is Rust's
//! (correctly rounded, ties to even on the exact binary value). The sign,
//! the `0x` prefix and the padding are applied here, by the rules of Rust's
//! `Formatter::pad_integral`, `pad_formatted_parts` and `pad`: numbers align
//! right and text left by default, `0` pads with zeros between the sign
//! (and prefix) and the digits, a centred value puts the odd fill
//! character on the right, and widths and precisions count characters.
//!
//! # The debug form
//!
//! The compiler generates the debug form of each type as code (no type is
//! inspected at run time); it calls in here to quote a string, to start an
//! indented line of the pretty form, and to notice a class instance that is
//! already being printed further up -- a reference cycle -- so that it
//! prints a marker instead of looping.

use std::{cell::RefCell, ffi::c_void, fmt::Write as _};

use super::{append, bytes_at, decimal_u128, text, write};

/// Where formatted text goes: a Rust `String` (the tests) or a [`Small`].
pub(super) trait Out: std::fmt::Write {
    fn put(&mut self, text: &str);

    fn put_char(&mut self, c: char) {
        let mut buffer = [0; 4];
        self.put(c.encode_utf8(&mut buffer));
    }
}

impl Out for String {
    fn put(&mut self, text: &str) {
        self.push_str(text);
    }

    fn put_char(&mut self, c: char) {
        self.push(c);
    }
}

/// Text being formatted, on the stack while it fits -- every number does --
/// and on the heap beyond (a long padded string, a float with a precision
/// of hundreds of digits).
pub(super) struct Small {
    /// The first `len` bytes are written (and are whole UTF-8 text); the
    /// rest is not initialised, so making one costs nothing.
    stack: [std::mem::MaybeUninit<u8>; 128],
    len: usize,
    heap: Option<String>,
}

impl Small {
    pub(super) fn new() -> Self {
        Small {
            stack: [std::mem::MaybeUninit::uninit(); 128],
            len: 0,
            heap: None,
        }
    }

    pub(super) fn as_bytes(&self) -> &[u8] {
        match &self.heap {
            Some(heap) => heap.as_bytes(),
            // Safety: the first `len` bytes were written.
            None => unsafe { std::slice::from_raw_parts(self.stack.as_ptr().cast(), self.len) },
        }
    }

    fn as_str(&self) -> &str {
        // Safety: only whole `str`s are ever written.
        unsafe { std::str::from_utf8_unchecked(self.as_bytes()) }
    }
}

impl std::fmt::Write for Small {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.put(text);
        Ok(())
    }
}

impl Out for Small {
    fn put(&mut self, text: &str) {
        if let Some(heap) = &mut self.heap {
            heap.push_str(text);
        } else if self.len + text.len() <= self.stack.len() {
            // Safety: within the stack buffer.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    text.as_ptr(),
                    self.stack.as_mut_ptr().add(self.len).cast(),
                    text.len(),
                );
            }
            self.len += text.len();
        } else {
            let mut heap = String::with_capacity(2 * (self.len + text.len()));
            heap.push_str(self.as_str());
            heap.push_str(text);
            self.heap = Some(heap);
        }
    }
}

/// Where a padded value sits within its width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// What a value is written as (the type character of the spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Display,
    Debug,
    LowerHex,
    UpperHex,
    Octal,
    Binary,
    LowerExp,
    UpperExp,
}

/// A decoded format spec. See `codira_hir::FormatSpec::abi_flags` for the
/// encoding.
#[derive(Debug, Clone, Copy)]
struct Spec {
    fill: char,
    align: Option<Align>,
    plus: bool,
    alternate: bool,
    zero: bool,
    width: Option<usize>,
    precision: Option<usize>,
    kind: Kind,
}

impl Spec {
    /// The spec generated code passes. A negative width or precision (read
    /// from a variable) counts as zero.
    fn decode(fill: u32, flags: u32, width: i64, precision: i64) -> Spec {
        let count = |present: bool, value: i64| present.then(|| value.max(0) as usize);
        Spec {
            fill: char::from_u32(fill).unwrap_or(' '),
            align: match flags & 3 {
                1 => Some(Align::Left),
                2 => Some(Align::Center),
                3 => Some(Align::Right),
                _ => None,
            },
            plus: flags & (1 << 2) != 0,
            alternate: flags & (1 << 3) != 0,
            zero: flags & (1 << 4) != 0,
            width: count(flags & (1 << 5) != 0, width),
            precision: count(flags & (1 << 6) != 0, precision),
            kind: match (flags >> 8) & 0xff {
                1 => Kind::Debug,
                2 => Kind::LowerHex,
                3 => Kind::UpperHex,
                4 => Kind::Octal,
                5 => Kind::Binary,
                6 => Kind::LowerExp,
                7 => Kind::UpperExp,
                _ => Kind::Display,
            },
        }
    }
}

/// Writes `count` fill characters.
fn fill(out: &mut impl Out, fill: char, count: usize) {
    if fill.is_ascii() {
        const SPACES: &str = "                                ";
        const ZEROS: &str = "00000000000000000000000000000000";
        let run = match fill {
            ' ' => Some(SPACES),
            '0' => Some(ZEROS),
            _ => None,
        };
        if let Some(run) = run {
            let mut left = count;
            while left > 0 {
                let n = left.min(run.len());
                out.put(&run[..n]);
                left -= n;
            }
            return;
        }
    }
    for _ in 0..count {
        out.put_char(fill);
    }
}

/// Writes `body` (of `len` characters) padded to the spec's width, aligned
/// as the spec says or as `default` when it says nothing: Rust's
/// `Formatter::padding`.
fn padded(out: &mut impl Out, body: &str, len: usize, spec: &Spec, default: Align) {
    let width = spec.width.unwrap_or(0);
    if len >= width {
        out.put(body);
        return;
    }
    let padding = width - len;
    let (before, after) = match spec.align.unwrap_or(default) {
        Align::Left => (0, padding),
        Align::Right => (padding, 0),
        Align::Center => (padding / 2, padding.div_ceil(2)),
    };
    fill(out, spec.fill, before);
    out.put(body);
    fill(out, spec.fill, after);
}

/// A number: `sign`, then `prefix`, then `digits` (all ASCII), padded as
/// Rust's `pad_integral` and `pad_formatted_parts` pad: with `0`, zeros go
/// between the sign and prefix and the digits, whatever the fill and the
/// alignment say.
fn number(out: &mut impl Out, sign: &str, prefix: &str, digits: &str, spec: &Spec) {
    let len = sign.len() + prefix.len() + digits.len();
    match spec.width {
        Some(width) if spec.zero && len < width => {
            out.put(sign);
            out.put(prefix);
            fill(out, '0', width - len);
            out.put(digits);
        }
        Some(width) if len < width => {
            let padding = width - len;
            let (before, after) = match spec.align.unwrap_or(Align::Right) {
                Align::Left => (0, padding),
                Align::Right => (padding, 0),
                Align::Center => (padding / 2, padding.div_ceil(2)),
            };
            fill(out, spec.fill, before);
            out.put(sign);
            out.put(prefix);
            out.put(digits);
            fill(out, spec.fill, after);
        }
        _ => {
            out.put(sign);
            out.put(prefix);
            out.put(digits);
        }
    }
}

/// An integer: `bits` is its value as 128 bits (sign- or zero-extended),
/// `width` its type's size in bits, which the radix forms of a negative
/// value print the two's complement of, as Rust does.
fn format_int(out: &mut impl Out, bits: u128, signed: bool, width: u32, spec: &Spec) {
    let negative = signed && (bits as i128) < 0;
    let magnitude = if negative {
        (bits as i128).unsigned_abs()
    } else {
        bits
    };
    let mask = if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    };
    let raw = bits & mask;
    let mut digits = Small::new();
    let mut decimal = [0u8; 40];
    let (radix, prefix) = match spec.kind {
        Kind::LowerHex => (write!(digits, "{raw:x}"), "0x"),
        Kind::UpperHex => (write!(digits, "{raw:X}"), "0x"),
        Kind::Octal => (write!(digits, "{raw:o}"), "0o"),
        Kind::Binary => (write!(digits, "{raw:b}"), "0b"),
        Kind::LowerExp => (exponent(&mut digits, magnitude, spec, false), ""),
        Kind::UpperExp => (exponent(&mut digits, magnitude, spec, true), ""),
        Kind::Display | Kind::Debug => {
            let at = decimal_u128(&mut decimal, magnitude);
            let text = std::str::from_utf8(&decimal[at..]).expect("digits are ASCII");
            digits.put(text);
            (Ok(()), "")
        }
    };
    radix.expect("writing to a String cannot fail");
    let is_radix = !prefix.is_empty();
    // The radix forms print the bits, which have no sign.
    let sign = if negative && !is_radix {
        "-"
    } else if spec.plus {
        "+"
    } else {
        ""
    };
    let prefix = if is_radix && spec.alternate {
        prefix
    } else {
        ""
    };
    number(out, sign, prefix, digits.as_str(), spec);
}

/// The exponent form of an integer's magnitude.
fn exponent(digits: &mut Small, magnitude: u128, spec: &Spec, upper: bool) -> std::fmt::Result {
    match (spec.precision, upper) {
        (Some(p), false) => write!(digits, "{magnitude:.p$e}"),
        (Some(p), true) => write!(digits, "{magnitude:.p$E}"),
        (None, false) => write!(digits, "{magnitude:e}"),
        (None, true) => write!(digits, "{magnitude:E}"),
    }
}

/// A float. Without a precision its digits are the ones `to_string()`
/// writes (Rust's `{:?}`: the shortest that reads back, with a `.0` on a
/// whole number); with one, Rust's `{:.p}`. `f32` values are written as
/// `f32`s, which have fewer digits.
fn format_float(out: &mut impl Out, value: f64, is_f32: bool, spec: &Spec) {
    // NaN has no sign, as in Rust; negative zero has one.
    let negative = value.is_sign_negative() && !value.is_nan();
    let magnitude = value.abs();
    let mut digits = Small::new();
    macro_rules! digits {
        ($v:expr) => {{
            let v = $v;
            match (spec.kind, spec.precision) {
                (Kind::LowerExp, Some(p)) => write!(digits, "{v:.p$e}"),
                (Kind::LowerExp, None) => write!(digits, "{v:e}"),
                (Kind::UpperExp, Some(p)) => write!(digits, "{v:.p$E}"),
                (Kind::UpperExp, None) => write!(digits, "{v:E}"),
                (_, Some(p)) => write!(digits, "{v:.p$}"),
                (_, None) => write!(digits, "{v:?}"),
            }
        }};
    }
    if is_f32 {
        digits!(magnitude as f32)
    } else {
        digits!(magnitude)
    }
    .expect("writing to a String cannot fail");
    let sign = if negative {
        "-"
    } else if spec.plus && !value.is_nan() {
        "+"
    } else {
        ""
    };
    number(out, sign, "", digits.as_str(), spec);
}

/// Text: at most `precision` characters of it, padded (left by default):
/// Rust's `Formatter::pad`.
fn format_text(out: &mut impl Out, text: &str, spec: &Spec) {
    let text = match spec.precision {
        Some(max) => match text.char_indices().nth(max) {
            Some((end, _)) => &text[..end],
            None => text,
        },
        None => text,
    };
    padded(out, text, text.chars().count(), spec, Align::Left);
}

/// `text`, quoted as a Codira string literal: `"` and `\` escaped, `\n`,
/// `\r`, `\t` and `\0` by name, any other ASCII control character as
/// `\xNN`. Every other character is written as it is (Codira literals have
/// no `\u{..}` escape), so the result reads back as the same string.
fn quote(out: &mut impl Out, text: &str) {
    out.put_char('"');
    for c in text.chars() {
        match c {
            '"' => out.put("\\\""),
            '\\' => out.put("\\\\"),
            '\n' => out.put("\\n"),
            '\r' => out.put("\\r"),
            '\t' => out.put("\\t"),
            '\0' => out.put("\\0"),
            c if c.is_ascii_control() => {
                write!(out, "\\x{:02x}", c as u32).expect("writing to memory cannot fail");
            }
            c => out.put_char(c),
        }
    }
    out.put_char('"');
}

thread_local! {
    /// The class instances whose debug form this thread is in the middle of
    /// writing (their GC handles), innermost last.
    static IN_PROGRESS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Appends `formatted` to the `String` `buf/start/len` (in its buffer when
/// it has room) and writes the result to `out`.
///
/// # Safety
///
/// Generated code's `String`, handles and slot.
unsafe fn append_out(
    buf: *const c_void,
    start: usize,
    len: usize,
    formatted: &[u8],
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    write(out, append(buf.cast(), start, len, formatted, ty, alloc));
}

/// `{x:spec}` for an integer, passed as its two 64-bit halves (sign- or
/// zero-extended to 128 bits), its signedness and its size in bits,
/// appended to the `String` `buf/start/len`.
#[allow(clippy::too_many_arguments)]
extern "C" fn string_append_format_int(
    buf: *const c_void,
    start: usize,
    len: usize,
    low: u64,
    high: u64,
    signed: u8,
    bits: u32,
    fill: u32,
    flags: u32,
    width: i64,
    precision: i64,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    let spec = Spec::decode(fill, flags, width, precision);
    let value = (u128::from(high) << 64) | u128::from(low);
    let mut text = Small::new();
    format_int(&mut text, value, signed != 0, bits, &spec);
    // Safety: generated code passes a live `String`, its handles and a slot.
    unsafe { append_out(buf, start, len, text.as_bytes(), ty, alloc, out) }
}

/// `{x:spec}` for a float (an `f32` widened to `f64` when `is_f32` is set,
/// which narrows back exactly), appended to the `String` `buf/start/len`.
#[allow(clippy::too_many_arguments)]
extern "C" fn string_append_format_float(
    buf: *const c_void,
    start: usize,
    len: usize,
    value: f64,
    is_f32: u8,
    fill: u32,
    flags: u32,
    width: i64,
    precision: i64,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    let spec = Spec::decode(fill, flags, width, precision);
    let mut text = Small::new();
    format_float(&mut text, value, is_f32 != 0, &spec);
    // Safety: as for `string_append_format_int`.
    unsafe { append_out(buf, start, len, text.as_bytes(), ty, alloc, out) }
}

/// `{x:spec}` for text -- a `String`, a `str`, a `bool`'s `true`/`false`,
/// or a value's debug form -- appended to the `String` `buf/start/len`.
#[allow(clippy::too_many_arguments)]
extern "C" fn string_append_format_text(
    buf: *const c_void,
    start: usize,
    len: usize,
    data: *const u8,
    data_len: usize,
    fill: u32,
    flags: u32,
    width: i64,
    precision: i64,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    let spec = Spec::decode(fill, flags, width, precision);
    // Safety: generated code passes valid UTF-8, a live `String`, its
    // handles and a slot.
    unsafe {
        let source = text(bytes_at(data, data_len));
        // Text that needs no padding or truncating is appended as it is.
        if spec.width.is_none_or(|w| w == 0) && spec.precision.is_none() {
            append_out(buf, start, len, source.as_bytes(), ty, alloc, out);
            return;
        }
        // `source` may point into a GC buffer: it is read into `text`
        // before anything is allocated.
        let mut formatted = Small::new();
        format_text(&mut formatted, source, &spec);
        append_out(buf, start, len, formatted.as_bytes(), ty, alloc, out);
    }
}

/// Appends `text`, quoted (see [`quote`]), to the `String` `buf/start/len`,
/// in place when it can be (as `+=` does), and writes the result to `out`.
#[allow(clippy::too_many_arguments)]
extern "C" fn debug_quote(
    buf: *const c_void,
    start: usize,
    len: usize,
    data: *const u8,
    data_len: usize,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    let mut quoted = Small::new();
    // Safety: generated code passes a live `String`, valid UTF-8 and its
    // handles; the quoted copy is made before anything is allocated.
    unsafe {
        quote(&mut quoted, text(bytes_at(data, data_len)));
        append_out(buf, start, len, quoted.as_bytes(), ty, alloc, out);
    }
}

/// Appends a line break and `depth` levels of four-space indentation (the
/// pretty debug form) to the `String` `buf/start/len`, written to `out`.
extern "C" fn debug_newline(
    buf: *const c_void,
    start: usize,
    len: usize,
    depth: i64,
    ty: *const c_void,
    alloc: *mut c_void,
    out: *mut u8,
) {
    let depth = usize::try_from(depth).unwrap_or(0);
    let mut line = Small::new();
    line.put("\n");
    fill(&mut line, ' ', 4 * depth);
    // Safety: generated code passes a live `String` and its handles.
    unsafe {
        append_out(buf, start, len, line.as_bytes(), ty, alloc, out);
    }
}

/// Starts the debug form of the class instance `object`: 1 when it is not
/// already being written further up this thread's stack (it is then
/// recorded, and [`debug_leave`] must follow), 0 when it is -- a reference
/// cycle -- and the caller prints a marker instead.
extern "C" fn debug_enter(object: *const c_void) -> u8 {
    IN_PROGRESS.with(|stack| {
        let mut stack = stack.borrow_mut();
        let object = object as usize;
        if stack.contains(&object) {
            0
        } else {
            stack.push(object);
            1
        }
    })
}

/// Ends the debug form of the innermost class instance [`debug_enter`]
/// started.
extern "C" fn debug_leave() {
    IN_PROGRESS.with(|stack| {
        stack.borrow_mut().pop();
    });
}

/// Ends the program after `main` returned an `Err`: writes `Error: ` and
/// `text` (the error, as the compiler wrote it) and a line break to standard
/// error, and exits with status 1. Standard output is flushed first, so what
/// the program printed before is not lost.
extern "C" fn exit_with_error(data: *const u8, len: usize) {
    use std::io::Write as _;
    // Safety: generated code passes valid UTF-8.
    let message = unsafe { text(bytes_at(data, len)) };
    let _ = std::io::stdout().flush();
    let _ = writeln!(std::io::stderr(), "Error: {message}");
    std::process::exit(1);
}

/// The functions above, for the dispatch table (see `string::definitions`).
pub(super) fn definitions() -> Vec<crate::FunctionDefinition> {
    use crate::IntoFunctionDefinition;
    type H = *const c_void;
    type T = *const c_void;
    type A = *mut c_void;
    type P = *const u8;
    type O = *mut u8;
    vec![
        IntoFunctionDefinition::into(
            string_append_format_int
                as extern "C" fn(H, usize, usize, u64, u64, u8, u32, u32, u32, i64, i64, T, A, O),
            "codira.string_append_format_int",
        ),
        IntoFunctionDefinition::into(
            string_append_format_float
                as extern "C" fn(H, usize, usize, f64, u8, u32, u32, i64, i64, T, A, O),
            "codira.string_append_format_float",
        ),
        IntoFunctionDefinition::into(
            string_append_format_text
                as extern "C" fn(H, usize, usize, P, usize, u32, u32, i64, i64, T, A, O),
            "codira.string_append_format_text",
        ),
        IntoFunctionDefinition::into(
            debug_quote as extern "C" fn(H, usize, usize, P, usize, T, A, O),
            "codira.debug_quote",
        ),
        IntoFunctionDefinition::into(
            debug_newline as extern "C" fn(H, usize, usize, i64, T, A, O),
            "codira.debug_newline",
        ),
        IntoFunctionDefinition::into(debug_enter as extern "C" fn(H) -> u8, "codira.debug_enter"),
        IntoFunctionDefinition::into(debug_leave as extern "C" fn(), "codira.debug_leave"),
        IntoFunctionDefinition::into(
            exit_with_error as extern "C" fn(P, usize),
            "codira.exit_with_error",
        ),
    ]
}

#[cfg(test)]
pub(super) mod tests_support {
    //! Entry points of this module for the tests of `string`.
    use std::ffi::c_void;

    use super::super::RawString;

    /// Appends `{n:0w}` (zero-padded to `width`) to `value`, as compiled
    /// code does.
    pub(in crate::string) fn append_format_int(
        value: RawString,
        n: i64,
        width: i64,
        ty: *const c_void,
        alloc: *mut c_void,
        out: *mut u8,
    ) {
        let zero_and_width = (1 << 4) | (1 << 5);
        super::string_append_format_int(
            value.buf.cast(),
            value.start,
            value.len,
            n as u64,
            (n >> 63) as u64,
            1,
            64,
            u32::from(' '),
            zero_and_width,
            width,
            0,
            ty,
            alloc,
            out,
        );
    }
}

#[cfg(test)]
mod tests {
    //! The formatting rules against Rust's `format!` as the oracle, on
    //! seeded random values, widths and precisions: the same spec is
    //! written once for Rust (where fill, alignment and flags are part of
    //! the macro's literal) and once encoded the way the compiler encodes
    //! it.
    use super::*;

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

        /// Floats that stress rounding: halfway cases, tiny and huge
        /// magnitudes, negative zero, infinities, NaN, and random bits.
        fn float(&mut self) -> f64 {
            match self.below(8) {
                0 => [0.5, 1.5, 2.5, 0.125, 0.375, 1e21, 1e-7, -0.0, 0.0][self.below(9) as usize],
                1 => [
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NAN,
                    f64::MAX,
                    f64::MIN_POSITIVE,
                ][self.below(5) as usize],
                2 => (self.below(2_000_001) as f64 - 1_000_000.0) / 1000.0,
                3 => (self.below(20_001) as f64 - 10_000.0) / 8.0,
                4 => f64::from_bits(self.next()),
                _ => (self.next() as i64 as f64) / (1u64 << self.below(60)) as f64,
            }
        }
    }

    fn spec(fill: char, flags: u32, width: Option<usize>, precision: Option<usize>) -> Spec {
        let flags =
            flags | (u32::from(width.is_some()) << 5) | (u32::from(precision.is_some()) << 6);
        Spec::decode(
            fill as u32,
            flags,
            width.map_or(0, |w| w as i64),
            precision.map_or(0, |p| p as i64),
        )
    }

    const RIGHT: u32 = 3;
    const LEFT: u32 = 1;
    const CENTER: u32 = 2;
    const PLUS: u32 = 1 << 2;
    const ALT: u32 = 1 << 3;
    const ZERO: u32 = 1 << 4;
    const fn kind(k: u32) -> u32 {
        k << 8
    }

    fn int(value: i64, flags: u32, fill: char, width: usize) -> String {
        let mut out = String::new();
        format_int(
            &mut out,
            value as i128 as u128,
            true,
            64,
            &spec(fill, flags, Some(width), None),
        );
        out
    }

    #[test]
    fn integers_are_padded_signed_and_prefixed_as_rust_does() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for _ in 0..4000 {
            let v = match rng.below(4) {
                0 => rng.next() as i64,
                1 => rng.below(2001) as i64 - 1000,
                2 => [0, -1, i64::MIN, i64::MAX][rng.below(4) as usize],
                _ => (rng.next() >> rng.below(64)) as i64,
            };
            let w = rng.below(30) as usize;
            assert_eq!(int(v, 0, ' ', w), format!("{v:w$}"), "{v} {w}");
            assert_eq!(int(v, RIGHT, ' ', w), format!("{v:>w$}"));
            assert_eq!(int(v, LEFT, ' ', w), format!("{v:<w$}"));
            assert_eq!(int(v, CENTER, '*', w), format!("{v:*^w$}"));
            assert_eq!(int(v, ZERO, ' ', w), format!("{v:0w$}"));
            assert_eq!(int(v, PLUS | ZERO, ' ', w), format!("{v:+0w$}"));
            assert_eq!(int(v, PLUS | LEFT, '-', w), format!("{v:-<+w$}"));
            assert_eq!(int(v, kind(2), ' ', w), format!("{v:w$x}"));
            assert_eq!(int(v, kind(3) | ALT, ' ', w), format!("{v:#w$X}"));
            assert_eq!(int(v, kind(4) | ALT | ZERO, ' ', w), format!("{v:#0w$o}"));
            assert_eq!(
                int(v, kind(5) | ALT | CENTER, '.', w),
                format!("{v:.^#w$b}")
            );
            assert_eq!(int(v, kind(2) | PLUS, ' ', w), format!("{v:+w$x}"));
            assert_eq!(int(v, kind(6), ' ', w), format!("{v:w$e}"));
            assert_eq!(int(v, kind(7) | PLUS, ' ', w), format!("{v:+w$E}"));
            let p = rng.below(12) as usize;
            let mut out = String::new();
            format_int(
                &mut out,
                v as i128 as u128,
                true,
                64,
                &spec(' ', kind(6), Some(w), Some(p)),
            );
            assert_eq!(out, format!("{v:w$.p$e}"), "{v} {w} {p}");
        }
        // The radix forms of a narrow negative value are its type's bits.
        let mut out = String::new();
        format_int(
            &mut out,
            -1i8 as i128 as u128,
            true,
            8,
            &spec(' ', kind(2), None, None),
        );
        assert_eq!(out, format!("{:x}", -1i8));
        let mut out = String::new();
        format_int(
            &mut out,
            u128::MAX,
            false,
            128,
            &spec(' ', kind(4), None, None),
        );
        assert_eq!(out, format!("{:o}", u128::MAX));
        let mut out = String::new();
        format_int(
            &mut out,
            i128::MIN as u128,
            true,
            128,
            &spec(' ', 0, Some(45), None),
        );
        assert_eq!(out, format!("{:45}", i128::MIN));
    }

    #[test]
    fn floats_round_and_pad_as_rust_does() {
        let mut rng = Rng(0x0dd_ba11_cafe_f00d);
        for _ in 0..6000 {
            let v = rng.float();
            let w = rng.below(30) as usize;
            let p = rng.below(18) as usize;
            let f = |flags: u32, fill: char, width: Option<usize>, precision: Option<usize>| {
                let mut out = String::new();
                format_float(&mut out, v, false, &spec(fill, flags, width, precision));
                out
            };
            assert_eq!(f(0, ' ', None, None), format!("{v:?}"), "{v:?}");
            assert_eq!(
                f(RIGHT, ' ', Some(w), None),
                format!("{v:>w$?}"),
                "{v:?} {w}"
            );
            assert_eq!(
                f(0, ' ', Some(w), Some(p)),
                format!("{v:w$.p$}"),
                "{v:?} {w} {p}"
            );
            assert_eq!(f(LEFT, '_', Some(w), Some(p)), format!("{v:_<w$.p$}"));
            assert_eq!(f(CENTER, '*', Some(w), Some(p)), format!("{v:*^w$.p$}"));
            assert_eq!(f(PLUS, ' ', Some(w), Some(p)), format!("{v:+w$.p$}"));
            assert_eq!(f(ZERO, ' ', Some(w), Some(p)), format!("{v:0w$.p$}"));
            assert_eq!(f(PLUS | ZERO, ' ', Some(w), None), format!("{v:+0w$?}"));
            assert_eq!(f(kind(6), ' ', Some(w), None), format!("{v:w$e}"));
            assert_eq!(f(kind(6), ' ', Some(w), Some(p)), format!("{v:w$.p$e}"));
            assert_eq!(
                f(kind(7) | PLUS | ZERO, ' ', Some(w), Some(p)),
                format!("{v:+0w$.p$E}")
            );
            let narrow = v as f32;
            let mut out = String::new();
            format_float(
                &mut out,
                f64::from(narrow),
                true,
                &spec(' ', 0, Some(w), Some(p)),
            );
            assert_eq!(out, format!("{narrow:w$.p$}"));
            let mut out = String::new();
            format_float(&mut out, f64::from(narrow), true, &spec(' ', 0, None, None));
            assert_eq!(out, format!("{narrow:?}"));
        }
    }

    #[test]
    fn text_is_truncated_and_padded_as_rust_does() {
        let mut rng = Rng(42);
        let words = [
            "",
            "a",
            "héllo",
            "世界!",
            "\u{1F600}x",
            "tab\there",
            "long words here",
        ];
        for _ in 0..2000 {
            let s = words[rng.below(words.len() as u64) as usize];
            let w = rng.below(20) as usize;
            let p = rng.below(8) as usize;
            let t = |flags: u32, fill: char, precision: Option<usize>| {
                let mut out = String::new();
                format_text(&mut out, s, &spec(fill, flags, Some(w), precision));
                out
            };
            assert_eq!(t(0, ' ', None), format!("{s:w$}"));
            assert_eq!(t(RIGHT, ' ', None), format!("{s:>w$}"));
            assert_eq!(t(CENTER, 'é', Some(p)), format!("{s:é^w$.p$}"));
            assert_eq!(t(LEFT, '-', Some(p)), format!("{s:-<w$.p$}"));
        }
    }

    #[test]
    fn quoting_reads_back_as_a_codira_literal() {
        let mut out = String::new();
        quote(&mut out, "a\"b\\c\nd\re\tf\0g\u{1}h\u{7f}é\u{1F600}");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\re\\tf\\0g\\x01h\\x7fé\u{1F600}\"");
    }

    #[test]
    fn a_cycle_is_noticed_and_left_cleanly() {
        let a = 0x1000 as *const c_void;
        let b = 0x2000 as *const c_void;
        assert_eq!(debug_enter(a), 1);
        assert_eq!(debug_enter(b), 1);
        // Already in progress: nothing is recorded, so nothing is left.
        assert_eq!(debug_enter(a), 0);
        assert_eq!(debug_enter(b), 0);
        debug_leave();
        // `b` is done; `a` is still in progress.
        assert_eq!(debug_enter(b), 1);
        assert_eq!(debug_enter(a), 0);
        debug_leave();
        debug_leave();
        assert_eq!(debug_enter(a), 1);
        debug_leave();
    }
}
