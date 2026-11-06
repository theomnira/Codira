// The Rust half of the String benchmarks: the same algorithms as
// `src/mod.code`, on `String` and `&str`. Build with
// `rustc -C opt-level=3 strings.rs` and run `./strings`; each line printed is
// `<name> <nanoseconds> <check value>`.

use std::hint::black_box;
use std::time::Instant;

fn report(name: &str, start: Instant, check: u64) {
    println!("{name} {} {check}", start.elapsed().as_nanos());
}

fn main() {
    // 1. Concatenating 1M short strings.
    let start = Instant::now();
    let mut words = String::new();
    for _ in 0..1_000_000 {
        words.push_str(black_box("word "));
    }
    report("concat_1m_short", start, words.len() as u64);

    let start = Instant::now();
    let mut numbers = String::new();
    for i in 0..1_000_000i64 {
        numbers.push_str(&black_box(i).to_string());
        numbers.push('\n');
    }
    report("concat_1m_numbers", start, numbers.len() as u64);

    // 2. Splitting a 10 MB text into lines (views, like Codira's).
    let text = "the quick brown fox jumps over the lazy dog 012345\n".repeat(205_600);
    let start = Instant::now();
    let lines: Vec<&str> = black_box(&text).lines().collect();
    report("split_10mb_lines", start, lines.len() as u64);

    // 2b. The same lines read lazily.
    let start = Instant::now();
    let mut line_bytes = 0;
    for line in black_box(&text).lines() {
        line_bytes += line.len();
    }
    report("lines_10mb_lazy", start, line_bytes as u64);

    // 2c. Split at commas lazily, the long fields counted.
    let fields_text = "the,quick,brown,fox,jumps,over,the,lazy,dog,012345\n".repeat(205_600);
    let start = Instant::now();
    let long_fields = black_box(&fields_text)
        .split(',')
        .filter(|w| w.len() > 3)
        .count();
    report("split_10mb_lazy_count", start, long_fields as u64);

    // 3. `find` on a long haystack.
    let hay = "ab".repeat(5_000_000) + "needle";
    let start = Instant::now();
    let at = black_box(&hay).find(black_box("needle")).unwrap();
    report("find_10mb", start, at as u64);

    // 4. Parsing 1M integers.
    let fields: Vec<&str> = numbers.lines().collect();
    let start = Instant::now();
    let mut sum = 0i64;
    for field in &fields {
        sum += black_box(field).parse::<i64>().unwrap();
    }
    report("parse_1m_i64", start, sum as u64);
}
