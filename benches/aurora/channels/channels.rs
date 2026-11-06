// AURORA: producers and consumers, the Rust baseline of `src/mod.code`:
// `std::sync::mpsc::sync_channel(1024)` and threads (spawned inside the
// timing, as Codira's tasks are). The same messages, the same check values.
#[path = "../common/bench.rs"]
mod bench;

use bench::report;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread;

fn produce(out: SyncSender<i64>, first: i64, count: i64) {
    for i in first..first + count {
        out.send(i).unwrap();
    }
}

fn square_each(jobs: Receiver<i64>, results: SyncSender<i64>, count: i64) {
    for _ in 0..count {
        let x = jobs.recv().unwrap();
        results.send(x.wrapping_mul(x) & 0xFFFF).unwrap();
    }
}

fn main() {
    match bench::kernel().as_str() {
        "spsc" => {
            let start = bench::now();
            let (tx, rx) = sync_channel(1024);
            let t = thread::spawn(move || produce(tx, 0, 2_000_000));
            let mut sum = 0i64;
            for _ in 0..2_000_000 {
                sum += rx.recv().unwrap();
            }
            t.join().unwrap();
            report(start, sum as u64);
        }
        "mpsc_4" => {
            let start = bench::now();
            let (tx, rx) = sync_channel(1024);
            let ts: Vec<_> = (0..4)
                .map(|p| {
                    let tx = tx.clone();
                    thread::spawn(move || produce(tx, p * 500_000, 500_000))
                })
                .collect();
            let mut sum = 0i64;
            for _ in 0..2_000_000 {
                sum += rx.recv().unwrap();
            }
            ts.into_iter().for_each(|t| t.join().unwrap());
            report(start, sum as u64);
        }
        "fan_out_in" => {
            let start = bench::now();
            let (rtx, rrx) = sync_channel(1024);
            let mut jobs = Vec::new();
            let mut ts = Vec::new();
            for _ in 0..4 {
                let (jtx, jrx) = sync_channel(1024);
                jobs.push(jtx);
                let rtx = rtx.clone();
                ts.push(thread::spawn(move || square_each(jrx, rtx, 50_000)));
            }
            let (mut sum, mut sent, mut got) = (0i64, 0i64, 0i64);
            while got < 200_000 {
                let mut b = 0;
                while b < 256 && sent < 200_000 {
                    jobs[(sent & 3) as usize].send(sent).unwrap();
                    sent += 1;
                    b += 1;
                }
                while got < sent {
                    sum += rrx.recv().unwrap();
                    got += 1;
                }
            }
            ts.into_iter().for_each(|t| t.join().unwrap());
            report(start, sum as u64);
        }
        k => bench::unknown(k),
    }
}
