// AURORA: a server-style workload, the Rust baseline of `src/mod.code`
// (`HashMap` with Codira's integer hash, `String`, `format!`). The same
// requests, the same work per request, the same check values.
#[path = "../common/bench.rs"]
mod bench;

use bench::Rng;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::time::Instant;

#[derive(Default)]
struct Mix64(u64);
impl Hasher for Mix64 {
    fn finish(&self) -> u64 {
        let mut x = self.0;
        x ^= x >> 33;
        x = x.wrapping_mul(0xFF51AFD7ED558CCD);
        x ^= x >> 33;
        x = x.wrapping_mul(0xC4CEB9FE1A85EC53);
        x ^ (x >> 33)
    }
    fn write(&mut self, _: &[u8]) {
        unreachable!()
    }
    fn write_i64(&mut self, v: i64) {
        self.0 = v as u64;
    }
}

struct Session {
    #[allow(dead_code)]
    uid: i64,
    hits: i64,
    items: Vec<i64>,
    last: String,
}

fn serve(requests: usize, live: usize, users: u64) {
    let mut r = Rng::new();
    let mut sessions: HashMap<i64, Session, BuildHasherDefault<Mix64>> = HashMap::default();
    let mut ring = vec![-1i64; live];
    let mut ring_at = 0;
    let mut lat = vec![0u64; requests];
    let mut check = 0u64;
    let start = Instant::now();
    for slot in lat.iter_mut() {
        let uid = (r.next() % users) as i64;
        let item = (r.next() % 1000) as i64;
        let line = format!("GET /users/{uid}/items/{item} HTTP/1.1");
        let t0 = Instant::now();

        let words: Vec<&str> = line.split(' ').collect();
        let parts: Vec<&str> = words[1].split('/').collect();
        let user: i64 = parts[2].parse().unwrap();
        let thing: i64 = parts[4].parse().unwrap();
        if !sessions.contains_key(&user) {
            let old = ring[ring_at];
            if old >= 0 {
                sessions.remove(&old);
            }
            ring[ring_at] = user;
            ring_at = (ring_at + 1) % live;
            sessions.insert(user, Session { uid: user, hits: 0, items: Vec::new(), last: String::new() });
        }
        let session = sessions.get_mut(&user).unwrap();
        session.hits += 1;
        if session.items.len() >= 16 {
            session.items = Vec::new();
        }
        session.items.push(thing);
        let body = format!(
            "{{\"user\":{user},\"item\":{thing},\"hits\":{},\"recent\":{}}}",
            session.hits,
            session.items.len()
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            body.len()
        ) + &body;
        check += response.len() as u64;
        session.last = response;

        *slot = t0.elapsed().as_nanos() as u64;
    }
    let ns = start.elapsed().as_nanos();
    println!("@@ ns={ns} check={}", check * 1000 + sessions.len() as u64);
    bench::report_latencies(&mut lat);
    bench::report_rss();
}

fn main() {
    match bench::kernel().as_str() {
        "requests" => serve(200_000, 20_000, 50_000),
        "long_run" => serve(1_000_000, 50_000, 200_000),
        k => bench::unknown(k),
    }
}
