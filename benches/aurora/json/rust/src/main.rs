// AURORA: JSON, the Rust baseline of `../src/mod.code`: the same kernels
// on `serde_json::Value` (insertion order kept, floats correctly rounded),
// the same check values. `stream_*`: a `serde` visitor driven by
// `serde_json`'s deserializer, no tree; `typed_twitter`:
// `#[derive(Deserialize)]` structs, unknown members ignored.
#[path = "../../../common/bench.rs"]
mod bench;

use std::fmt;

use serde::{
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer,
};
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

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
fn typed_check(statuses: &[Status]) -> u64 {
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
    (statuses.len() as u64)
        .wrapping_mul(1000003)
        .wrapping_add(bytes.wrapping_mul(10007))
        .wrapping_add(numbers)
}

fn fold(s: &Sums) -> u64 {
    s.nodes
        .wrapping_mul(1000003)
        .wrapping_add(s.strings.wrapping_mul(10007))
        .wrapping_add(s.ints.wrapping_mul(101))
        .wrapping_add(s.floats)
}

fn run_stream(name: &str) {
    let text = load(name);
    let start = bench::now();
    let mut check = 0;
    for _ in 0..REPS {
        let mut s = Sums::default();
        let mut de = serde_json::Deserializer::from_str(&text);
        Stream(&mut s).deserialize(&mut de).expect("parse error");
        de.end().expect("parse error");
        check = fold(&s);
    }
    bench::report(start, check);
}

fn run_typed(name: &str) {
    let text = load(name);
    let start = bench::now();
    let mut check = 0;
    for _ in 0..REPS {
        let decoded: Twitter = serde_json::from_str(&text).expect("decode error");
        check = typed_check(&decoded.statuses);
    }
    bench::report(start, check);
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
        "stream" => run_stream(input),
        "typed" if input == "twitter" => run_typed(input),
        _ => bench::unknown(&kernel),
    }
}
