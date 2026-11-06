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
//! - `stream`: the parse check value computed by a `serde` visitor driven by
//!   `serde_json`'s deserializer, which builds no tree (the counterpart of
//!   `JsonReader`'s events). `typed` (twitter): the statuses decoded into
//!   `#[derive(Deserialize)]` structs, unknown members ignored.
//! - With the `simd` feature, `simd-json` parses each input into its
//!   `OwnedValue` too (`simd-parse`), from a copy of the text it may change.

use std::{fmt, time::Instant};

use serde::{
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer,
};
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

/// Adds every value of the input to the sums, as `walk` does, without
/// building it.
struct Stream<'a>(&'a mut Sums);

impl<'de> DeserializeSeed<'de> for Stream<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Stream<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        self.0.nodes += 1;
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, b: bool) -> Result<(), E> {
        self.0.nodes += 1;
        if b {
            self.0.ints = self.0.ints.wrapping_add(1);
        }
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, n: i64) -> Result<(), E> {
        self.0.nodes += 1;
        self.0.ints = self.0.ints.wrapping_add(n as u64);
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, n: u64) -> Result<(), E> {
        self.0.nodes += 1;
        if n <= i64::MAX as u64 {
            self.0.ints = self.0.ints.wrapping_add(n);
        } else {
            self.0.floats = self.0.floats.wrapping_add((n as f64).to_bits());
        }
        Ok(())
    }

    fn visit_f64<E: de::Error>(self, x: f64) -> Result<(), E> {
        self.0.nodes += 1;
        self.0.floats = self.0.floats.wrapping_add(x.to_bits());
        Ok(())
    }

    fn visit_str<E: de::Error>(self, t: &str) -> Result<(), E> {
        self.0.nodes += 1;
        self.0.strings += t.len() as u64;
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        self.0.nodes += 1;
        while seq.next_element_seed(Stream(&mut *self.0))?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        self.0.nodes += 1;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            self.0.strings += key.len() as u64;
            map.next_value_seed(Stream(&mut *self.0))?;
        }
        Ok(())
    }
}

fn bench_stream(name: &str, text: &str) {
    let mut times = Vec::new();
    let mut s = Sums::default();
    for _ in 0..RUNS {
        s = Sums::default();
        let t0 = Instant::now();
        let mut de = serde_json::Deserializer::from_str(text);
        Stream(&mut s).deserialize(&mut de).unwrap();
        de.end().unwrap();
        times.push(t0.elapsed().as_nanos() as u64);
    }
    println!(
        "{name} stream {} {}/{}/{}/{}",
        median(times),
        s.nodes,
        s.strings,
        s.ints,
        s.floats
    );
}

#[derive(Deserialize)]
struct User {
    id: i64,
    name: String,
    screen_name: String,
    location: Option<String>,
    followers_count: i64,
    verified: bool,
}

#[derive(Deserialize)]
struct Hashtag {
    text: String,
    indices: Vec<i64>,
}

#[derive(Deserialize)]
struct Entities {
    hashtags: Vec<Hashtag>,
}

#[derive(Deserialize)]
struct Status {
    id: i64,
    text: String,
    truncated: bool,
    in_reply_to_status_id: Option<i64>,
    user: User,
    retweet_count: i64,
    favorite_count: i64,
    entities: Entities,
    lang: String,
    score: f64,
}

#[derive(Deserialize)]
struct Twitter {
    statuses: Vec<Status>,
}

/// The typed check value, as `typed_check` in `../src/mod.code`.
fn typed_check(statuses: &[Status]) -> String {
    let mut numbers: u64 = 0;
    let mut bytes: u64 = 0;
    for s in statuses {
        numbers = numbers
            .wrapping_add(s.id as u64)
            .wrapping_add(s.retweet_count as u64)
            .wrapping_add(s.favorite_count as u64)
            .wrapping_add(s.score.to_bits())
            .wrapping_add(u64::from(s.truncated));
        if let Some(n) = s.in_reply_to_status_id {
            numbers = numbers.wrapping_add(n as u64);
        }
        let u = &s.user;
        numbers = numbers
            .wrapping_add(u.id as u64)
            .wrapping_add(u.followers_count as u64)
            .wrapping_add(u64::from(u.verified));
        bytes += (s.text.len() + s.lang.len() + u.name.len() + u.screen_name.len()) as u64;
        if let Some(place) = &u.location {
            bytes += place.len() as u64;
        }
        for h in &s.entities.hashtags {
            bytes += h.text.len() as u64;
            for i in &h.indices {
                numbers = numbers.wrapping_add(*i as u64);
            }
        }
    }
    format!("{}/{numbers}/{bytes}", statuses.len())
}

fn bench_typed(name: &str, text: &str) {
    let mut times = Vec::new();
    let mut check = String::new();
    for _ in 0..RUNS {
        let t0 = Instant::now();
        let decoded: Twitter = serde_json::from_str(text).unwrap();
        times.push(t0.elapsed().as_nanos() as u64);
        check = typed_check(&decoded.statuses);
    }
    println!("{name} typed {} {check}", median(times));
}

#[cfg(feature = "simd")]
fn simd_walk(v: &simd_json::OwnedValue, s: &mut Sums) {
    use simd_json::{prelude::*, OwnedValue, StaticNode};
    s.nodes += 1;
    match v {
        OwnedValue::Static(StaticNode::Null) => {}
        OwnedValue::Static(StaticNode::Bool(b)) => {
            if *b {
                s.ints = s.ints.wrapping_add(1);
            }
        }
        OwnedValue::Static(StaticNode::I64(n)) => s.ints = s.ints.wrapping_add(*n as u64),
        OwnedValue::Static(StaticNode::U64(n)) => {
            if *n <= i64::MAX as u64 {
                s.ints = s.ints.wrapping_add(*n);
            } else {
                s.floats = s.floats.wrapping_add((*n as f64).to_bits());
            }
        }
        OwnedValue::Static(StaticNode::F64(x)) => s.floats = s.floats.wrapping_add(x.to_bits()),
        OwnedValue::String(t) => s.strings += t.len() as u64,
        OwnedValue::Array(items) => items.iter().for_each(|item| simd_walk(item, s)),
        OwnedValue::Object(o) => {
            for (k, item) in o.iter() {
                s.strings += k.len() as u64;
                simd_walk(item, s);
            }
        }
    }
    let _ = v.value_type();
}

#[cfg(feature = "simd")]
fn bench_simd(name: &str, text: &str) {
    let mut times = Vec::new();
    let mut s = Sums::default();
    for _ in 0..RUNS {
        let mut bytes = text.as_bytes().to_vec();
        let t0 = Instant::now();
        let value = simd_json::to_owned_value(&mut bytes).unwrap();
        times.push(t0.elapsed().as_nanos() as u64);
        s = Sums::default();
        simd_walk(&value, &mut s);
    }
    println!(
        "{name} simd-parse {} {}/{}/{}/{}",
        median(times),
        s.nodes,
        s.strings,
        s.ints,
        s.floats
    );
}

fn load(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn main() {
    let twitter = load("data/twitter.json");
    let canada = load("data/canada.json");
    let deep = load("data/deep.json");
    bench("twitter", &twitter);
    bench("canada", &canada);
    bench("deep", &deep);
    bench_stream("twitter", &twitter);
    bench_stream("canada", &canada);
    bench_stream("deep", &deep);
    bench_typed("twitter", &twitter);
    #[cfg(feature = "simd")]
    for (name, text) in [("twitter", &twitter), ("canada", &canada), ("deep", &deep)] {
        bench_simd(name, text);
    }
}
