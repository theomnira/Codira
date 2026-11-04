//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Isolation tests for the snapshot-isolation transactions: stable
//!   snapshots, no dirty reads, first committer wins, no lost updates under
//!   concurrent increments, consistent snapshots under concurrent transfers,
//!   and a demonstration that write skew IS possible (snapshot isolation is
//!   not serializable).

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use codira_db::{Db, Error, FaultStorage, FsyncPolicy, Options};

fn db() -> Db {
    Db::open(
        Arc::new(FaultStorage::new()),
        Options {
            fsync: FsyncPolicy::Never,
            checkpoint_wal_bytes: Some(1 << 16),
        },
    )
    .unwrap()
}

fn set(db: &Db, k: &[u8], v: &[u8]) {
    let mut t = db.begin();
    t.put(k, v).unwrap();
    t.commit().unwrap();
}

fn int(v: Option<Vec<u8>>) -> i64 {
    v.map_or(0, |v| String::from_utf8(v).unwrap().parse().unwrap())
}

#[test]
fn snapshot_reads_are_stable() {
    let db = db();
    set(&db, b"k", b"old");
    let reader = db.begin();
    set(&db, b"k", b"new");
    set(&db, b"other", b"x");
    assert_eq!(reader.get(b"k").as_deref(), Some(&b"old"[..]));
    assert_eq!(reader.get(b"other"), None);
    assert_eq!(reader.scan_all().len(), 1);
    assert_eq!(db.begin().get(b"k").as_deref(), Some(&b"new"[..]));
}

#[test]
fn no_dirty_reads() {
    let db = db();
    let mut writer = db.begin();
    writer.put(b"k", b"uncommitted").unwrap();
    assert_eq!(db.begin().get(b"k"), None);
    writer.commit().unwrap();
    assert_eq!(db.begin().get(b"k").as_deref(), Some(&b"uncommitted"[..]));
}

#[test]
fn first_committer_wins() {
    let db = db();
    set(&db, b"k", b"0");
    let mut t1 = db.begin();
    let mut t2 = db.begin();
    t1.put(b"k", b"t1").unwrap();
    t2.put(b"k", b"t2").unwrap();
    t1.commit().unwrap();
    match t2.commit() {
        Err(Error::Conflict { key }) => assert_eq!(key, b"k"),
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(db.begin().get(b"k").as_deref(), Some(&b"t1"[..]));
    // The loser wrote nothing: the next timestamp is 3, not 4.
    set(&db, b"z", b"1");
    assert_eq!(db.last_commit_ts(), 3);
}

#[test]
fn delete_and_insert_of_an_absent_key_conflict() {
    let db = db();
    let mut t1 = db.begin();
    let mut t2 = db.begin();
    t1.delete(b"k").unwrap();
    t2.put(b"k", b"v").unwrap();
    t2.commit().unwrap();
    assert!(matches!(t1.commit(), Err(Error::Conflict { .. })));
}

#[test]
fn disjoint_writers_both_commit_and_read_only_never_conflicts() {
    let db = db();
    let mut t1 = db.begin();
    let mut t2 = db.begin();
    let t3 = db.begin();
    t1.put(b"a", b"1").unwrap();
    t2.put(b"b", b"2").unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
    assert_eq!(t3.get(b"a"), None);
    t3.commit().unwrap();
}

/// The classic write-skew anomaly (Berenson et al. 1995, A5B): the
/// invariant "at least one doctor is on call" holds in every snapshot, each
/// transaction preserves it on its own, and both commit, breaking it.
/// Snapshot isolation allows this; a serializable engine would not. This
/// test pins the documented behaviour: if it ever fails, the isolation
/// level changed and the documentation must change with it.
#[test]
fn write_skew_is_possible() {
    let db = db();
    set(&db, b"alice_on_call", b"1");
    set(&db, b"bob_on_call", b"1");
    let on_call =
        |t: &codira_db::Txn<'_>| int(t.get(b"alice_on_call")) + int(t.get(b"bob_on_call"));
    let mut t1 = db.begin();
    let mut t2 = db.begin();
    assert_eq!(on_call(&t1), 2);
    assert_eq!(on_call(&t2), 2);
    t1.put(b"alice_on_call", b"0").unwrap();
    t2.put(b"bob_on_call", b"0").unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
    assert_eq!(on_call(&db.begin()), 0, "write skew happened");
}

#[test]
fn concurrent_increments_lose_no_update() {
    let db = db();
    let threads = 8;
    let per_thread = 200;
    let conflicts = AtomicU64::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                for _ in 0..per_thread {
                    loop {
                        let mut t = db.begin();
                        let n = int(t.get(b"counter"));
                        t.put(b"counter", (n + 1).to_string().as_bytes()).unwrap();
                        match t.commit() {
                            Ok(_) => break,
                            Err(Error::Conflict { .. }) => {
                                conflicts.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => panic!("{e}"),
                        }
                    }
                }
            });
        }
    });
    assert_eq!(int(db.begin().get(b"counter")), threads * per_thread);
    assert_eq!(db.last_commit_ts(), (threads * per_thread) as u64);
    eprintln!("{} conflicts retried", conflicts.load(Ordering::Relaxed));
}

#[test]
fn concurrent_snapshots_are_consistent_under_transfers() {
    let db = db();
    let accounts = 10;
    {
        let mut t = db.begin();
        for i in 0..accounts {
            t.put(format!("acct{i}").as_bytes(), b"100").unwrap();
        }
        t.commit().unwrap();
    }
    let stop = AtomicBool::new(false);
    let checks = AtomicU64::new(0);
    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| loop {
                // At least one check per reader, then until the writers stop.
                let done = stop.load(Ordering::SeqCst);
                let t = db.begin();
                let total: i64 = t
                    .scan_prefix(b"acct")
                    .into_iter()
                    .map(|(_, v)| int(Some(v)))
                    .sum();
                assert_eq!(total, 100 * accounts as i64, "inconsistent snapshot");
                let one_by_one: i64 = (0..accounts)
                    .map(|i| int(t.get(format!("acct{i}").as_bytes())))
                    .sum();
                assert_eq!(one_by_one, total, "point reads disagree with the scan");
                checks.fetch_add(1, Ordering::Relaxed);
                if done {
                    break;
                }
            });
        }
        let writers: Vec<_> = (0..4u64)
            .map(|w| {
                let db = &db;
                s.spawn(move || {
                    let mut rng = codira_db::fault::Rng::new(w);
                    for _ in 0..500 {
                        let a = rng.below(accounts);
                        let b = rng.below(accounts);
                        if a == b {
                            continue;
                        }
                        let mut t = db.begin();
                        let ka = format!("acct{a}");
                        let kb = format!("acct{b}");
                        let va = int(t.get(ka.as_bytes()));
                        let vb = int(t.get(kb.as_bytes()));
                        t.put(ka.as_bytes(), (va - 7).to_string().as_bytes())
                            .unwrap();
                        t.put(kb.as_bytes(), (vb + 7).to_string().as_bytes())
                            .unwrap();
                        match t.commit() {
                            Ok(_) | Err(Error::Conflict { .. }) => {}
                            Err(e) => panic!("{e}"),
                        }
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::SeqCst);
    });
    assert!(db.last_commit_ts() > 1);
    assert!(checks.load(Ordering::Relaxed) > 0);
}
