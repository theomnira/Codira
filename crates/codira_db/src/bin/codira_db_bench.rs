//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - A throughput benchmark for `codira_db` on the local file system:
//!   bulk puts, point gets, range scans, small-transaction commits under
//!   each fsync policy, checkpoint, and recovery (from a snapshot and from
//!   a log). Prints one line per measurement.
//!
//! Usage: `cargo run --release -p codira_db --bin codira_db_bench [DIR]`.
//! DIR defaults to a fresh directory under the system temporary directory
//! and is deleted afterwards. The numbers depend on the device and file
//! system that DIR is on; record them together with that.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use codira_db::{fault::Rng, Db, FsyncPolicy, Options};

const BULK_KEYS: u64 = 100_000;
const VALUE_LEN: usize = 100;

fn key(i: u64) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..4].copy_from_slice(b"key:");
    k[8..].copy_from_slice(&i.to_be_bytes());
    k
}

fn report(what: &str, n: u64, d: Duration) {
    let secs = d.as_secs_f64();
    println!(
        "{what:<44} {n:>8} ops {:>10.1} ms {:>12.0} ops/s {:>9.2} us/op",
        secs * 1e3,
        n as f64 / secs,
        secs * 1e6 / n as f64
    );
}

fn opts(fsync: FsyncPolicy) -> Options {
    Options {
        fsync,
        checkpoint_wal_bytes: None,
    }
}

fn fresh(base: &Path, name: &str) -> PathBuf {
    let d = base.join(name);
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn small_commits(base: &Path, policy: FsyncPolicy, n: u64) {
    let dir = fresh(base, &format!("commits-{policy:?}"));
    let db = Db::open_dir(&dir, opts(policy)).unwrap();
    let value = [7u8; VALUE_LEN];
    let start = Instant::now();
    for i in 0..n {
        let mut t = db.begin();
        t.put(&key(i), &value).unwrap();
        t.commit().unwrap();
    }
    report(
        &format!("commit, 1 put/txn, fsync={policy:?}"),
        n,
        start.elapsed(),
    );
    db.close().unwrap();
}

fn main() {
    let (base, owned) = match std::env::args().nth(1) {
        Some(d) => (PathBuf::from(d), false),
        None => (
            std::env::temp_dir().join(format!("codira_db_bench_{}", std::process::id())),
            true,
        ),
    };
    std::fs::create_dir_all(&base).unwrap();
    println!("directory: {}", base.display());
    println!(
        "keys: 16 bytes, values: {VALUE_LEN} bytes, bulk set: {BULK_KEYS} keys, auto-checkpoint off"
    );

    // Bulk load in one transaction.
    let dir = fresh(&base, "bulk");
    let db = Db::open_dir(&dir, opts(FsyncPolicy::OnCommit)).unwrap();
    let value = [42u8; VALUE_LEN];
    let start = Instant::now();
    let mut t = db.begin();
    for i in 0..BULK_KEYS {
        t.put(&key(i), &value).unwrap();
    }
    report("put (buffered in one txn)", BULK_KEYS, start.elapsed());
    let start = Instant::now();
    t.commit().unwrap();
    report(
        "commit of that txn (1 record, fdatasync)",
        BULK_KEYS,
        start.elapsed(),
    );

    // Point reads.
    let mut rng = Rng::new(1);
    let t = db.begin();
    let start = Instant::now();
    let mut found = 0u64;
    for _ in 0..BULK_KEYS {
        if t.get(&key(rng.below(BULK_KEYS))).is_some() {
            found += 1;
        }
    }
    report("get (random, hit)", BULK_KEYS, start.elapsed());
    assert_eq!(found, BULK_KEYS);
    let start = Instant::now();
    let rows = t.scan_all();
    report(
        "full scan (rows returned)",
        rows.len() as u64,
        start.elapsed(),
    );
    drop(t);

    // Checkpoint and recovery.
    let start = Instant::now();
    db.checkpoint().unwrap();
    report(
        "checkpoint (snapshot of all keys)",
        BULK_KEYS,
        start.elapsed(),
    );
    db.close().unwrap();
    let start = Instant::now();
    let db = Db::open_dir(&dir, opts(FsyncPolicy::OnCommit)).unwrap();
    report(
        "open: recover from snapshot (keys)",
        BULK_KEYS,
        start.elapsed(),
    );
    assert_eq!(db.begin().scan_all().len() as u64, BULK_KEYS);
    drop(db);

    // Small transactions under each policy.
    small_commits(&base, FsyncPolicy::Never, 50_000);
    small_commits(&base, FsyncPolicy::OnCommit, 2_000);
    small_commits(&base, FsyncPolicy::Always, 2_000);

    // Recovery by log replay.
    let dir = base.join(format!("commits-{:?}", FsyncPolicy::Never));
    let start = Instant::now();
    let db = Db::open_dir(&dir, opts(FsyncPolicy::OnCommit)).unwrap();
    let n = db.recovery_report().records_replayed;
    report("open: replay log (commit records)", n, start.elapsed());
    drop(db);

    if owned {
        std::fs::remove_dir_all(&base).unwrap();
    }
}
