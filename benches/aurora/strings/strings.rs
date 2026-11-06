// AURORA: strings, the Rust baseline of `src/mod.code` (same work, same
// check values). Built by `scripts/bench.py` with rustc -C opt-level=3.
#[path = "../common/bench.rs"]
mod bench;

use bench::{now, report};
use std::hint::black_box;

fn main() {
    let kernel = bench::kernel();
    match kernel.as_str() {
        "append_short" => {
            let start = now();
            let mut words = String::new();
            for _ in 0..1_000_000 {
                words.push_str(black_box("word "));
            }
            report(start, words.len() as u64);
        }
        "append_numbers" => {
            let start = now();
            let mut numbers = String::new();
            for i in 0..1_000_000i64 {
                numbers.push_str(&black_box(i).to_string());
                numbers.push('\n');
            }
            report(start, numbers.len() as u64);
        }
        "concat" => {
            let names: Vec<String> = (0..64).map(|k| format!("name{k}")).collect();
            let start = now();
            let mut check = 0u64;
            for i in 0..1_000_000usize {
                let i = black_box(i);
                let t = names[i & 63].clone() + "/" + &names[(i * 7) & 63];
                check += t.len() as u64 + u64::from(t.as_bytes()[t.len() - 1]);
            }
            report(start, check);
        }
        "split_lines" => {
            let text = "the quick brown fox jumps over the lazy dog 012345\n".repeat(205_600);
            let start = now();
            let lines: Vec<&str> = black_box(&text).lines().collect();
            report(start, lines.len() as u64);
        }
        "lines_lazy" => {
            let text = "the quick brown fox jumps over the lazy dog 012345\n".repeat(205_600);
            let start = now();
            let mut line_bytes = 0;
            for line in black_box(&text).lines() {
                line_bytes += line.len();
            }
            report(start, line_bytes as u64);
        }
        "split_lazy_count" => {
            let text = "the,quick,brown,fox,jumps,over,the,lazy,dog,012345\n".repeat(205_600);
            let start = now();
            let long_fields = black_box(&text).split(',').filter(|w| w.len() > 3).count();
            report(start, long_fields as u64);
        }
        "split_fields" => {
            let text = "alpha,beta,gamma,delta,".repeat(250_000);
            let start = now();
            let parts: Vec<&str> = black_box(&text).split(',').collect();
            let total: u64 = parts.iter().map(|p| p.len() as u64).sum();
            report(start, parts.len() as u64 * 1_000_000 + total);
        }
        "find_10mb" => {
            let hay = "ab".repeat(5_000_000) + "needle";
            let start = now();
            let at = black_box(&hay).find(black_box("needle")).unwrap();
            report(start, at as u64);
        }
        "parse_i64" => {
            let mut numbers = String::new();
            for i in 0..1_000_000i64 {
                numbers.push_str(&i.to_string());
                numbers.push('\n');
            }
            let fields: Vec<&str> = numbers.lines().collect();
            let start = now();
            let mut sum = 0i64;
            for field in &fields {
                sum += black_box(field).parse::<i64>().unwrap();
            }
            report(start, sum as u64);
        }
        k => bench::unknown(k),
    }
}
