//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The Rust half of the JSON benchmarks: parse, print and pretty-print the
//!   three inputs of `gen_inputs.py` with `serde_json::Value`, as
//!   `../src/mod.code` does with `std.json.json`, printing the same lines
//!   (`<input> <phase> <median nanoseconds> <check value>`) and the same
//!   check values. Run from the directory holding `data/`.

use std::time::Instant;

use serde_json::Value;

const RUNS: usize = 11;

#[derive(Default)]
struct Sums {
    nodes: u64,
    strings: u64,
    ints: u64,
    floats: u64,
}

fn walk(v: &Value, s: &mut Sums) {
    s.nodes += 1;
    match v {
        Value::Null => {}
        Value::Bool(b) => {
            if *b {
                s.ints = s.ints.wrapping_add(1);
            }
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                s.ints = s.ints.wrapping_add(i as u64);
            } else if let Some(u) = n.as_u64() {
                // Beyond `i64`: `std.json.json` holds these as floats.
                s.floats = s.floats.wrapping_add((u as f64).to_bits());
            } else {
                s.floats = s.floats.wrapping_add(n.as_f64().unwrap().to_bits());
            }
        }
        Value::String(t) => s.strings += t.len() as u64,
        Value::Array(items) => items.iter().for_each(|item| walk(item, s)),
        Value::Object(o) => {
            for (k, item) in o {
                s.strings += k.len() as u64;
                walk(item, s);
            }
        }
    }
}

fn median(mut times: Vec<u64>) -> u64 {
    times.sort_unstable();
    times[times.len() / 2]
}

fn bench(name: &str, text: &str) {
    let mut parse_times = Vec::new();
    let mut print_times = Vec::new();
    let mut pretty_times = Vec::new();
    let mut value = Value::Null;
    let mut compact = 0;
    let mut pretty = 0;
    for _ in 0..RUNS {
        let t0 = Instant::now();
        let parsed: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                println!("{name} parse error: {e}");
                return;
            }
        };
        parse_times.push(t0.elapsed().as_nanos() as u64);
        // The previous run's value is dropped outside the timed region, as
        // the collector frees Codira's whenever it runs.
        drop(std::mem::replace(&mut value, parsed));
        let t2 = Instant::now();
        compact = serde_json::to_string(&value).unwrap().len();
        let t3 = Instant::now();
        pretty = serde_json::to_string_pretty(&value).unwrap().len();
        let t4 = Instant::now();
        print_times.push((t3 - t2).as_nanos() as u64);
        pretty_times.push((t4 - t3).as_nanos() as u64);
    }
    let mut s = Sums::default();
    walk(&value, &mut s);
    println!(
        "{name} parse {} {}/{}/{}/{}",
        median(parse_times),
        s.nodes,
        s.strings,
        s.ints,
        s.floats
    );
    println!("{name} print {} {compact}", median(print_times));
    println!("{name} pretty {} {pretty}", median(pretty_times));
}

fn load(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn main() {
    bench("twitter", &load("data/twitter.json"));
    bench("canada", &load("data/canada.json"));
    bench("deep", &load("data/deep.json"));
}
