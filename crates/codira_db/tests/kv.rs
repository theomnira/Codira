//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Key-value, recovery and checkpoint tests, on a real directory
//!   (`FsStorage` in a temporary directory) and on `FaultStorage`: reopen,
//!   torn tails written to a real file, corruption in the middle of a real
//!   log and in a snapshot, automatic checkpoints, I/O failures poisoning
//!   the database, version garbage collection, ordered scans.

use std::{fs, ops::Bound, path::Path, sync::Arc};

use codira_db::{Db, Error, FaultStorage, FsyncPolicy, Options, Storage};

fn opts() -> Options {
    Options {
        fsync: FsyncPolicy::OnCommit,
        checkpoint_wal_bytes: None,
    }
}

fn put(db: &Db, k: &str, v: &str) -> u64 {
    let mut t = db.begin();
    t.put(k.as_bytes(), v.as_bytes()).unwrap();
    t.commit().unwrap()
}

fn get(db: &Db, k: &str) -> Option<String> {
    db.begin()
        .get(k.as_bytes())
        .map(|v| String::from_utf8(v).unwrap())
}

fn wal_files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("wal-"))
        .collect();
    v.sort();
    v
}

#[test]
fn put_get_delete_survive_reopen_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        assert_eq!(put(&db, "a", "1"), 1);
        assert_eq!(put(&db, "b", "2"), 2);
        let mut t = db.begin();
        t.delete(b"a").unwrap();
        t.put(b"c", b"3").unwrap();
        assert_eq!(t.commit().unwrap(), 3);
        db.close().unwrap();
    }
    let db = Db::open_dir(dir.path(), opts()).unwrap();
    let r = db.recovery_report();
    assert_eq!(r.records_replayed, 3);
    assert_eq!(r.last_commit_ts, 3);
    assert_eq!(r.torn_bytes_discarded, 0);
    assert_eq!(get(&db, "a"), None);
    assert_eq!(get(&db, "b").as_deref(), Some("2"));
    assert_eq!(get(&db, "c").as_deref(), Some("3"));
    assert_eq!(put(&db, "d", "4"), 4, "timestamps continue after reopen");
}

#[test]
fn abort_and_drop_discard_writes() {
    let db = Db::open(Arc::new(FaultStorage::new()), opts()).unwrap();
    let mut t = db.begin();
    t.put(b"x", b"1").unwrap();
    assert_eq!(
        t.get(b"x").as_deref(),
        Some(&b"1"[..]),
        "reads its own writes"
    );
    t.abort();
    {
        let mut t = db.begin();
        t.put(b"y", b"1").unwrap();
    }
    assert_eq!(get(&db, "x"), None);
    assert_eq!(get(&db, "y"), None);
    assert_eq!(db.last_commit_ts(), 0);
    let ro = db.begin();
    assert_eq!(ro.commit().unwrap(), 0, "a read-only commit writes nothing");
}

#[test]
fn torn_tail_in_a_real_file_is_discarded_and_truncated() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        for i in 0..3 {
            put(&db, &format!("k{i}"), "v");
        }
    }
    let wal = dir.path().join(&wal_files(dir.path())[0]);
    let full = fs::metadata(&wal).unwrap().len();
    // Cut the last record in half.
    fs::OpenOptions::new()
        .write(true)
        .open(&wal)
        .unwrap()
        .set_len(full - 5)
        .unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        let r = db.recovery_report().clone();
        assert_eq!(r.records_replayed, 2);
        assert!(r.torn_bytes_discarded > 0);
        assert_eq!(get(&db, "k2"), None);
        assert_eq!(put(&db, "k9", "after"), 3);
    }
    let db = Db::open_dir(dir.path(), opts()).unwrap();
    assert_eq!(db.recovery_report().torn_bytes_discarded, 0);
    assert_eq!(db.recovery_report().records_replayed, 3);
    assert_eq!(get(&db, "k9").as_deref(), Some("after"));
}

#[test]
fn zero_filled_tail_in_a_real_file_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        put(&db, "a", "1");
    }
    let wal = dir.path().join(&wal_files(dir.path())[0]);
    let len = fs::metadata(&wal).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&wal)
        .unwrap()
        .set_len(len + 4096)
        .unwrap();
    let db = Db::open_dir(dir.path(), opts()).unwrap();
    assert_eq!(db.recovery_report().torn_bytes_discarded, 4096);
    assert_eq!(get(&db, "a").as_deref(), Some("1"));
}

#[test]
fn corruption_in_the_middle_of_a_real_log_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        for i in 0..3 {
            put(&db, &format!("key{i}"), "value");
        }
    }
    let wal = dir.path().join(&wal_files(dir.path())[0]);
    let mut data = fs::read(&wal).unwrap();
    // Byte 40 is inside the first record's payload (20-byte file header,
    // 12-byte record header).
    data[40] ^= 0x01;
    fs::write(&wal, &data).unwrap();
    match Db::open_dir(dir.path(), opts()) {
        Err(Error::Corruption { offset, file, .. }) => {
            assert_eq!(offset, 20);
            assert!(file.starts_with("wal-"));
        }
        Err(e) => panic!("expected corruption, got {e}"),
        Ok(_) => panic!("expected corruption, got a database"),
    }
}

#[test]
fn checkpoint_truncates_the_log_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        for i in 0..50 {
            put(&db, &format!("k{i:02}"), &format!("v{i}"));
        }
        let mut t = db.begin();
        t.delete(b"k00").unwrap();
        t.commit().unwrap();
        db.checkpoint().unwrap();
        let wals = wal_files(dir.path());
        assert_eq!(wals, vec!["wal-00000000000000000002.log"]);
        assert_eq!(fs::metadata(dir.path().join(&wals[0])).unwrap().len(), 20);
        assert!(dir.path().join("snapshot").exists());
        assert!(!dir.path().join("snapshot.tmp").exists());
        put(&db, "late", "x");
    }
    let db = Db::open_dir(dir.path(), opts()).unwrap();
    let r = db.recovery_report();
    assert_eq!(r.snapshot_ts, Some(51));
    assert_eq!(r.records_replayed, 1);
    assert_eq!(r.last_commit_ts, 52);
    assert_eq!(get(&db, "k00"), None);
    assert_eq!(get(&db, "k49").as_deref(), Some("v49"));
    assert_eq!(get(&db, "late").as_deref(), Some("x"));
}

#[test]
fn corrupt_snapshot_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        put(&db, "a", "1");
        db.checkpoint().unwrap();
    }
    let snap = dir.path().join("snapshot");
    let mut data = fs::read(&snap).unwrap();
    data[30] ^= 0x80;
    fs::write(&snap, &data).unwrap();
    assert!(matches!(
        Db::open_dir(dir.path(), opts()),
        Err(Error::Corruption { .. })
    ));
}

#[test]
fn missing_log_after_snapshot_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        put(&db, "a", "1");
        db.checkpoint().unwrap();
    }
    for w in wal_files(dir.path()) {
        fs::remove_file(dir.path().join(w)).unwrap();
    }
    assert!(matches!(
        Db::open_dir(dir.path(), opts()),
        Err(Error::Corruption { .. })
    ));
}

#[test]
fn automatic_checkpoint_bounds_the_log() {
    let storage = Arc::new(FaultStorage::new());
    let o = Options {
        fsync: FsyncPolicy::OnCommit,
        checkpoint_wal_bytes: Some(4096),
    };
    let db = Db::open(storage.clone(), o.clone()).unwrap();
    for i in 0..1000 {
        put(&db, &format!("k{}", i % 37), &format!("value {i}"));
    }
    let names = storage.list().unwrap();
    let wals: Vec<_> = names.iter().filter(|n| n.starts_with("wal-")).collect();
    assert_eq!(wals.len(), 1, "{names:?}");
    assert!(names.contains(&"snapshot".to_string()));
    let wal_len = storage.read(wals[0]).unwrap().unwrap().len();
    assert!(wal_len < 4096 + 200, "log is {wal_len} bytes");
    drop(db);
    let db = Db::open(storage, o).unwrap();
    assert_eq!(db.last_commit_ts(), 1000);
    assert_eq!(get(&db, "k0").as_deref(), Some("value 999"));
}

#[test]
fn failed_append_poisons_the_database() {
    let storage = Arc::new(FaultStorage::new());
    let db = Db::open(storage.clone(), opts()).unwrap();
    put(&db, "a", "1");
    storage.set_fail_at(Some(storage.ops()));
    let mut t = db.begin();
    t.put(b"b", b"2").unwrap();
    assert!(matches!(t.commit(), Err(Error::Io(_))));
    assert!(db.is_poisoned());
    let mut t = db.begin();
    t.put(b"c", b"3").unwrap();
    assert!(matches!(t.commit(), Err(Error::Poisoned)));
    assert!(matches!(db.checkpoint(), Err(Error::Poisoned)));
    // Reads still work, and reopening recovers from the log.
    assert_eq!(get(&db, "a").as_deref(), Some("1"));
    drop(db);
    let db = Db::open(storage, opts()).unwrap();
    assert_eq!(get(&db, "a").as_deref(), Some("1"));
    assert_eq!(get(&db, "b"), None);
    assert_eq!(put(&db, "c", "3"), 2);
}

#[test]
fn failed_fsync_poisons_and_the_outcome_is_decided_by_the_log() {
    let storage = Arc::new(FaultStorage::new());
    let db = Db::open(storage.clone(), opts()).unwrap();
    // Operation `ops()` is the append, `ops() + 1` the fsync.
    storage.set_fail_at(Some(storage.ops() + 1));
    let mut t = db.begin();
    t.put(b"b", b"2").unwrap();
    assert!(matches!(t.commit(), Err(Error::Io(_))));
    assert!(db.is_poisoned());
    assert_eq!(get(&db, "b"), None, "not visible in this process");
    drop(db);
    // The record reached the (simulated) file, so recovery replays it: the
    // caller was told the outcome is unknown, and it turned out committed.
    let db = Db::open(storage, opts()).unwrap();
    assert_eq!(get(&db, "b").as_deref(), Some("2"));
}

#[test]
fn old_versions_are_kept_for_open_snapshots_and_then_collected() {
    let db = Db::open(Arc::new(FaultStorage::new()), opts()).unwrap();
    put(&db, "k", "v0");
    for i in 1..100 {
        put(&db, "k", &format!("v{i}"));
    }
    assert_eq!(db.version_counts(), (1, 1), "no reader: one version");
    let reader = db.begin();
    for i in 100..110 {
        put(&db, "k", &format!("v{i}"));
    }
    assert_eq!(reader.get(b"k").as_deref(), Some(&b"v99"[..]));
    assert!(db.version_counts().1 >= 2);
    drop(reader);
    put(&db, "k", "last");
    assert_eq!(db.version_counts(), (1, 1));
    let mut t = db.begin();
    t.delete(b"k").unwrap();
    t.commit().unwrap();
    db.checkpoint().unwrap();
    assert_eq!(db.version_counts(), (0, 0), "tombstones are collected");
}

#[test]
fn scans_are_ordered_and_see_own_writes() {
    let db = Db::open(Arc::new(FaultStorage::new()), opts()).unwrap();
    let mut t = db.begin();
    for k in ["b", "d", "a", "c", "e", "ab"] {
        t.put(k.as_bytes(), k.as_bytes()).unwrap();
    }
    t.commit().unwrap();
    let mut t = db.begin();
    t.delete(b"c").unwrap();
    t.put(b"bb", b"new").unwrap();
    let keys = |v: Vec<(Vec<u8>, Vec<u8>)>| {
        v.into_iter()
            .map(|(k, _)| String::from_utf8(k).unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(t.range(b"b", b"e")), vec!["b", "bb", "d"]);
    assert_eq!(keys(t.scan_all()), vec!["a", "ab", "b", "bb", "d", "e"]);
    assert_eq!(keys(t.scan_prefix(b"a")), vec!["a", "ab"]);
    assert_eq!(
        keys(t.scan(Bound::Excluded(b"d"), Bound::Unbounded)),
        vec!["e"]
    );
    assert!(t.range(b"e", b"a").is_empty(), "inverted range");
    // Another transaction does not see the uncommitted changes.
    assert_eq!(
        keys(db.begin().scan_all()),
        vec!["a", "ab", "b", "c", "d", "e"]
    );
}

#[test]
fn binary_keys_and_empty_values() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        let mut t = db.begin();
        t.put(&[0, 0xFF, 7], b"").unwrap();
        t.put(b"", b"empty key").unwrap();
        t.commit().unwrap();
    }
    let db = Db::open_dir(dir.path(), opts()).unwrap();
    let t = db.begin();
    assert_eq!(t.get(&[0, 0xFF, 7]).as_deref(), Some(&b""[..]));
    assert_eq!(t.get(b"").as_deref(), Some(&b"empty key"[..]));
}

#[test]
fn never_policy_is_flushed_by_close() {
    let dir = tempfile::tempdir().unwrap();
    let o = Options {
        fsync: FsyncPolicy::Never,
        checkpoint_wal_bytes: None,
    };
    {
        let db = Db::open_dir(dir.path(), o.clone()).unwrap();
        for i in 0..10 {
            put(&db, &format!("k{i}"), "v");
        }
        db.close().unwrap();
    }
    let db = Db::open_dir(dir.path(), o).unwrap();
    assert_eq!(db.last_commit_ts(), 10);
}
