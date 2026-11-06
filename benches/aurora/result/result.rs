// AURORA: `Result` and `?`, the Rust baseline of `src/mod.code`: the same
// parser as `std.text.ascii.parse_i64`, the same error types (an
// `AnyError` with a message, a context list and a type name, made by
// `From`), the same check values.
#[path = "../common/bench.rs"]
mod bench;

use std::hint::black_box;

#[derive(Debug, Clone, Copy)]
enum ParseError {
    Empty,
    InvalidDigit(usize),
    Overflow,
}

impl ParseError {
    fn message(&self) -> String {
        match self {
            ParseError::Empty => "no digits".to_string(),
            ParseError::InvalidDigit(i) => format!("invalid digit at byte {i}"),
            ParseError::Overflow => "number too large".to_string(),
        }
    }
}

fn parse_i64(bytes: &[u8], start: usize, end: usize) -> Result<i64, ParseError> {
    let mut i = start;
    let mut negative = false;
    if i < end && (bytes[i] == b'+' || bytes[i] == b'-') {
        negative = bytes[i] == b'-';
        i += 1;
    }
    if i >= end {
        return Err(ParseError::Empty);
    }
    let mut value: i64 = 0;
    while i < end {
        let b = bytes[i];
        if !b.is_ascii_digit() {
            return Err(ParseError::InvalidDigit(i));
        }
        let digit = (b - b'0') as i64;
        if value < (i64::MIN + digit) / 10 {
            return Err(ParseError::Overflow);
        }
        value = value * 10 - digit;
        i += 1;
    }
    if negative {
        Ok(value)
    } else if value == i64::MIN {
        Err(ParseError::Overflow)
    } else {
        Ok(-value)
    }
}

#[allow(dead_code)]
enum Problem {
    Parse(ParseError),
}
impl From<ParseError> for Problem {
    fn from(e: ParseError) -> Self {
        Problem::Parse(e)
    }
}

#[allow(dead_code)]
struct AnyError {
    cause: String,
    contexts: Vec<String>,
    kind: Option<&'static str>,
}
impl From<ParseError> for AnyError {
    fn from(e: ParseError) -> Self {
        AnyError { cause: e.message(), contexts: Vec::new(), kind: Some("ParseError") }
    }
}
impl AnyError {
    fn context(mut self, what: &str) -> Self {
        self.contexts.push(what.to_string());
        self
    }
    fn chain_len(&self) -> usize {
        self.contexts.len() + 1
    }
}

#[inline(never)]
fn wrapped(bytes: &[u8], end: usize) -> Result<i64, Problem> {
    let v = parse_i64(bytes, 0, end)?;
    Ok(v + 1)
}

#[inline(never)]
fn converted(bytes: &[u8], end: usize) -> Result<i64, AnyError> {
    let v = parse_i64(bytes, 0, end)?;
    Ok(v + 1)
}

#[inline(never)]
fn with_context(bytes: &[u8], end: usize) -> Result<i64, AnyError> {
    let v = parse_i64(bytes, 0, end).map_err(|e| AnyError::from(e).context("parsing the answer"))?;
    Ok(v + 1)
}

#[allow(dead_code)]
enum Bad {
    Odd(i64),
}
fn level0(x: i64) -> Result<i64, Bad> {
    if x & 15 == 0 { Err(Bad::Odd(x)) } else { Ok(x.wrapping_mul(3)) }
}
fn level1(x: i64) -> Result<i64, Bad> { let v = level0(x)?; Ok(v.wrapping_add(1)) }
fn level2(x: i64) -> Result<i64, Bad> { let v = level1(x)?; Ok(v ^ 5) }
fn level3(x: i64) -> Result<i64, Bad> { let v = level2(x)?; Ok(v.wrapping_add(7)) }
fn level4(x: i64) -> Result<i64, Bad> { let v = level3(x)?; Ok(v ^ 9) }
fn level5(x: i64) -> Result<i64, Bad> { let v = level4(x)?; Ok(v.wrapping_add(11)) }
fn level6(x: i64) -> Result<i64, Bad> { let v = level5(x)?; Ok(v ^ 13) }
fn level7(x: i64) -> Result<i64, Bad> { let v = level6(x)?; Ok(v.wrapping_add(15)) }

fn run_wrapped(end: usize) {
    let bytes = b"42x".to_vec();
    let start = bench::now();
    let mut check = 0u64;
    for _ in 0..10_000_000 {
        match wrapped(black_box(&bytes), black_box(end)) {
            Ok(v) => check = check.wrapping_add(v as u64),
            Err(_) => check += 1,
        }
    }
    bench::report(start, check);
}

fn main() {
    match bench::kernel().as_str() {
        "wrapped_ok" => run_wrapped(2),
        "wrapped_err" => run_wrapped(3),
        "converted_err" => {
            let bytes = b"42x".to_vec();
            let start = bench::now();
            let mut check = 0u64;
            for _ in 0..1_000_000 {
                match converted(black_box(&bytes), 3) {
                    Ok(v) => check = check.wrapping_add(v as u64),
                    Err(e) => check += e.cause.len() as u64,
                }
            }
            bench::report(start, check);
        }
        "context_err" => {
            let bytes = b"42x".to_vec();
            let start = bench::now();
            let mut check = 0u64;
            for _ in 0..1_000_000 {
                match with_context(black_box(&bytes), 3) {
                    Ok(v) => check = check.wrapping_add(v as u64),
                    Err(e) => check += e.cause.len() as u64 + e.chain_len() as u64,
                }
            }
            bench::report(start, check);
        }
        "propagate_depth8" => {
            let start = bench::now();
            let mut check = 0u64;
            for i in 0..10_000_000i64 {
                match level7(black_box(i)) {
                    Ok(v) => check = check.wrapping_add(v as u64),
                    Err(Bad::Odd(x)) => check = check.wrapping_add((x as u64) >> 4),
                }
            }
            bench::report(start, check);
        }
        k => bench::unknown(k),
    }
}
