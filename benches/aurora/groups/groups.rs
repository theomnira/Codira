// AURORA: task groups, the Rust baseline of `src/mod.code`: a scoped
// thread per task (`std::thread::scope`), joined for its result.
#[path = "../common/bench.rs"]
mod bench;

fn fib(n: i64) -> i64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn main() {
    match bench::kernel().as_str() {
        "fib_64" => {
            let start = bench::now();
            let mut check = 0i64;
            for _ in 0..10 {
                let results: Vec<i64> = std::thread::scope(|s| {
                    let hs: Vec<_> = (0..64i64).map(|i| s.spawn(move || fib(24 + (i & 1)))).collect();
                    hs.into_iter().map(|h| h.join().unwrap()).collect()
                });
                check += results.iter().sum::<i64>();
            }
            bench::report(start, check as u64);
        }
        k => bench::unknown(k),
    }
}
