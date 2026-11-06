//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The float formatting `std/json/json.code` calls through an
//!   `extern "C"` declaration: `codira_json_format_f64` writes the shortest
//!   decimal text that reads back as the same `f64` (the `ryu` crate) into
//!   a buffer the program passes by address, so that printing a float
//!   allocates nothing. Everything else of `std.json` is Codira.
//!
//! The conventions are those of `native_hash`: addresses are `usize`, and
//! the runtime trusts the caller that the bytes it names are writable (the
//! program passes a `stack_alloc` slot of [`FORMAT_F64_CAPACITY`] bytes). A
//! null address writes nothing and returns 0. The call allocates nothing,
//! takes no lock, makes no system call and has no panic path.

use crate::{FunctionDefinition, IntoFunctionDefinition};

/// The bytes [`codira_json_format_f64`] may write: `ryu` writes at most 24.
pub const FORMAT_F64_CAPACITY: usize = 32;

/// The JSON text of `value` at `out_addr`, returning its length in bytes.
///
/// A finite value is written in the shortest form that parses back to the
/// same `f64`, always with a `.` or an exponent (`1.0`, `0.1`, `1e300`,
/// `-0.0`, `5e-324`), so a reader that keeps integers apart reads a float.
/// JSON has no NaN or infinity: those are written as `null`, as
/// JavaScript's `JSON.stringify` and `serde_json` do.
pub extern "C" fn codira_json_format_f64(value: f64, out_addr: usize) -> usize {
    if out_addr == 0 {
        return 0;
    }
    let mut buffer = ryu::Buffer::new();
    let text = if value.is_finite() {
        buffer.format_finite(value)
    } else {
        "null"
    };
    debug_assert!(text.len() <= FORMAT_F64_CAPACITY);
    // Safety: the program passes `FORMAT_F64_CAPACITY` writable bytes (see
    // the module docs), and `text` is at most 24 bytes long.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), out_addr as *mut u8, text.len());
    }
    text.len()
}

/// Returns the definitions of every function in this module, for the
/// runtime's dispatch table.
pub(crate) fn functions() -> Vec<FunctionDefinition> {
    vec![IntoFunctionDefinition::into(
        codira_json_format_f64 as extern "C" fn(f64, usize) -> usize,
        "codira_json_format_f64",
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(value: f64) -> String {
        let mut out = [0u8; FORMAT_F64_CAPACITY];
        let len = codira_json_format_f64(value, out.as_mut_ptr() as usize);
        String::from_utf8(out[..len].to_vec()).unwrap()
    }

    /// xorshift64*: deterministic bit patterns for the property test.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
    }

    #[test]
    fn known_values_are_written_as_json() {
        assert_eq!(format(1.0), "1.0");
        assert_eq!(format(0.1), "0.1");
        assert_eq!(format(-0.0), "-0.0");
        assert_eq!(format(1e300), "1e300");
        assert_eq!(format(5e-324), "5e-324");
        assert_eq!(format(f64::MAX), "1.7976931348623157e308");
        assert_eq!(format(-65.613616999999977), "-65.61361699999998");
        assert_eq!(format(f64::NAN), "null");
        assert_eq!(format(f64::INFINITY), "null");
        assert_eq!(format(f64::NEG_INFINITY), "null");
        assert_eq!(codira_json_format_f64(1.5, 0), 0);
    }

    /// Every finite value reads back as itself, is the shortest such text
    /// Rust finds (`{:?}` has the same digits), and has a `.` or an
    /// exponent.
    #[test]
    fn random_values_round_trip_and_look_like_floats() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..200_000 {
            let value = f64::from_bits(rng.next());
            if !value.is_finite() {
                assert_eq!(format(value), "null");
                continue;
            }
            let text = format(value);
            assert_eq!(
                text.parse::<f64>().unwrap().to_bits(),
                value.to_bits(),
                "{text}"
            );
            assert!(text.contains('.') || text.contains('e'), "{text}");
            let digits = |s: &str| {
                let mantissa = s.split(['e', 'E']).next().unwrap();
                mantissa
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect::<String>()
                    .trim_matches('0')
                    .len()
            };
            assert_eq!(
                digits(&text),
                digits(&format!("{value:e}")),
                "{text} vs {value:e}"
            );
        }
    }
}
