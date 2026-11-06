// AURORA: `@region` functions, the Rust baseline of `src/mod.code`: `Box`
// cells and `Vec`s, freed as they go out of scope. Rust's standard library
// has no arena, so the `_region` kernels run the same code as the `_heap`
// ones (a crate such as bumpalo would be the counterpart).
#[path = "../common/bench.rs"]
mod bench;

use std::hint::black_box;

struct Cell {
    value: i64,
    next: Option<Box<Cell>>,
}

fn list_sum(n: i64) -> i64 {
    let mut head: Option<Box<Cell>> = None;
    for i in 0..n {
        head = Some(Box::new(Cell { value: i, next: head }));
    }
    let mut sum = 0;
    let mut at = head.as_deref();
    while let Some(c) = at {
        sum += c.value;
        at = c.next.as_deref();
    }
    // Drop iteratively (a long chain would recurse).
    let mut cur = head;
    while let Some(mut c) = cur {
        cur = c.next.take();
    }
    sum
}

fn scratch(n: usize, seed: i64) -> i64 {
    let xs = vec![seed; n];
    let ys: Vec<i64> = xs.iter().map(|x| x * 3 + 1).collect();
    ys.iter().sum::<i64>() + ys[0]
}

fn main() {
    let start = bench::now();
    let mut check = 0i64;
    match bench::kernel().as_str() {
        "list_heap" | "list_region" => {
            for i in 0..100_000i64 {
                check += list_sum(black_box(64 + (i & 1)));
            }
        }
        "scratch_arrays_heap" | "scratch_arrays_region" => {
            for i in 0..50_000i64 {
                check += scratch(black_box(1024), i & 7);
            }
        }
        k => bench::unknown(k),
    }
    bench::report(start, check as u64);
}
