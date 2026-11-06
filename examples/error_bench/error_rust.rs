// The Rust side of examples/error_bench (src/mod.code is the Codira side;
// both print the same check values): rustc -C opt-level=3 error_rust.rs
//
// The same functions, written as a Rust program would: `next_word` and
// `parse_i64` are std.text.ascii's algorithms, `Problem` gets its variant
// from a `From` implementation, and `AnyError` is std.error.error's (the
// root cause's message, the contexts, the type's name), built the same way
// on the error path.
use std::fmt;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
enum ParseError {
    Empty,
    InvalidDigit(usize),
    Overflow,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => write!(f, "no digits"),
            ParseError::InvalidDigit(at) => write!(f, "invalid digit at byte {at}"),
            ParseError::Overflow => write!(f, "number too large"),
        }
    }
}

#[derive(Debug)]
enum Problem {
    #[allow(dead_code)]
    Parse(ParseError),
}

impl From<ParseError> for Problem {
    fn from(e: ParseError) -> Self {
        Problem::Parse(e)
    }
}

/// std.error.error's `AnyError`: the root cause's message, the contexts
/// added on the way out, and the error's type name.
#[allow(dead_code)]
struct AnyError {
    message: String,
    contexts: Vec<String>,
    type_name: Option<&'static str>,
}

impl AnyError {
    fn root_cause(&self) -> &str {
        &self.message
    }
}

impl From<ParseError> for AnyError {
    fn from(e: ParseError) -> Self {
        AnyError {
            message: e.to_string(),
            contexts: Vec::new(),
            type_name: Some("ParseError"),
        }
    }
}

fn is_space(b: u8) -> bool {
    b == 32 || (9..=13).contains(&b)
}

fn next_word(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let n = bytes.len();
    let mut start = from;
    while start < n && is_space(bytes[start]) {
        start += 1;
    }
    if start >= n {
        return None;
    }
    let mut end = start;
    while end < n && !is_space(bytes[end]) {
        end += 1;
    }
    Some((start, end))
}

fn parse_i64(bytes: &[u8], start: usize, end: usize) -> Result<i64, ParseError> {
    let mut i = start;
    let mut negative = false;
    if i < end && (bytes[i] == 43 || bytes[i] == 45) {
        negative = bytes[i] == 45;
        i += 1;
    }
    if i >= end {
        return Err(ParseError::Empty);
    }
    let min = i64::MIN;
    let mut value: i64 = 0;
    while i < end {
        let b = bytes[i];
        if !b.is_ascii_digit() {
            return Err(ParseError::InvalidDigit(i));
        }
        let digit = i64::from(b - 48);
        if value < (min + digit) / 10 {
            return Err(ParseError::Overflow);
        }
        value = value * 10 - digit;
        i += 1;
    }
    if negative {
        Ok(value)
    } else if value == min {
        Err(ParseError::Overflow)
    } else {
        Ok(-value)
    }
}

fn wrapped(bytes: &[u8], end: usize) -> Result<i64, Problem> {
    let v = parse_i64(bytes, 0, end)?;
    Ok(v + 1)
}

fn converted(bytes: &[u8], end: usize) -> Result<i64, AnyError> {
    let v = parse_i64(bytes, 0, end)?;
    Ok(v + 1)
}

fn with_context(bytes: &[u8], end: usize) -> Result<i64, AnyError> {
    let v = parse_i64(bytes, 0, end).map_err(|e| {
        let mut any = AnyError::from(e);
        any.contexts.push("parsing the answer".to_string());
        any
    })?;
    Ok(v + 1)
}

fn sum_words(bytes: &[u8]) -> Result<i64, Problem> {
    let mut total: i64 = 0;
    let mut at = 0;
    while let Some((start, end)) = next_word(bytes, at) {
        total = total.wrapping_add(parse_i64(bytes, start, end)?);
        at = end;
    }
    Ok(total)
}

fn words(count: usize) -> Vec<u8> {
    let mut text = Vec::new();
    let mut x: u64 = 88172645463325252;
    for _ in 0..count {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let n = (x % 1_000_000) as i64 - 500_000;
        text.extend_from_slice(format!("{n} ").as_bytes());
    }
    text
}

fn median(mut times: Vec<u128>) -> u128 {
    times.sort_unstable();
    times[3]
}

fn main() {
    let bytes = b"42x".to_vec();
    let count = 1_000_000;
    for end in [2usize, 3] {
        let outcome = if end == 2 { "ok" } else { "err" };
        let ends = vec![end; 1024];
        let mut times = Vec::new();
        let mut check: u64 = 0;
        for _ in 0..7 {
            check = 0;
            let start = Instant::now();
            for i in 0..count {
                match wrapped(&bytes, ends[i & 1023]) {
                    Ok(v) => check = check.wrapping_add(v as u64),
                    Err(_) => check += 1,
                }
            }
            times.push(start.elapsed().as_nanos());
        }
        println!("wrapped_{outcome} {} {check}", median(times));

        let mut times = Vec::new();
        for _ in 0..7 {
            check = 0;
            let start = Instant::now();
            for i in 0..count {
                match converted(&bytes, ends[i & 1023]) {
                    Ok(v) => check = check.wrapping_add(v as u64),
                    Err(e) => check += e.root_cause().len() as u64,
                }
            }
            times.push(start.elapsed().as_nanos());
        }
        println!("converted_{outcome} {} {check}", median(times));

        let mut times = Vec::new();
        for _ in 0..7 {
            check = 0;
            let start = Instant::now();
            for i in 0..count {
                match with_context(&bytes, ends[i & 1023]) {
                    Ok(v) => check = check.wrapping_add(v as u64),
                    Err(e) => check += e.root_cause().len() as u64,
                }
            }
            times.push(start.elapsed().as_nanos());
        }
        println!("context_{outcome} {} {check}", median(times));
    }

    let text = words(1_000_000);
    let mut times = Vec::new();
    let mut check = 0i64;
    for _ in 0..7 {
        let start = Instant::now();
        check = match sum_words(std::hint::black_box(&text)) {
            Ok(v) => v,
            Err(_) => -1,
        };
        times.push(start.elapsed().as_nanos());
    }
    println!("sum_words {} {check}", median(times));
}
