// AURORA: JSON, the Rust baseline of `../src/mod.code`: the same kernels
// on `serde_json::Value` (insertion order kept, floats correctly rounded),
// the same check values.
#[path = "../../../common/bench.rs"]
mod bench;

use serde_json::Value;

const REPS: usize = 5;

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

fn load(name: &str) -> String {
    std::fs::read_to_string(format!("data/{name}.json")).expect("run from the directory holding data/")
}

fn run_parse(name: &str) {
    let text = load(name);
    let start = bench::now();
    let mut value = Value::Null;
    for _ in 0..REPS {
        value = serde_json::from_str(&text).expect("parse error");
    }
    let ns = start.elapsed().as_nanos();
    let mut s = Sums::default();
    walk(&value, &mut s);
    let check = s
        .nodes
        .wrapping_mul(1000003)
        .wrapping_add(s.strings.wrapping_mul(10007))
        .wrapping_add(s.ints.wrapping_mul(101))
        .wrapping_add(s.floats);
    println!("@@ ns={ns} check={check}");
    bench::report_rss();
}

fn run_print(name: &str, pretty: bool) {
    let value: Value = serde_json::from_str(&load(name)).expect("parse error");
    let start = bench::now();
    let mut len = 0;
    for _ in 0..REPS {
        len = if pretty {
            serde_json::to_string_pretty(&value).unwrap().len()
        } else {
            serde_json::to_string(&value).unwrap().len()
        };
    }
    bench::report(start, len as u64);
}

fn main() {
    let kernel = bench::kernel();
    let (phase, input) = kernel.split_once('_').unwrap_or_else(|| bench::unknown(&kernel));
    if !["twitter", "canada", "deep"].contains(&input) {
        bench::unknown(&kernel);
    }
    match phase {
        "parse" => run_parse(input),
        "print" => run_print(input, false),
        "pretty" => run_print(input, true),
        _ => bench::unknown(&kernel),
    }
}
