//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Unit tests of the runtime's database services: a round trip through
//!   every operation under each fsync policy and a reopen, snapshot
//!   isolation and the write-write conflict code, and an error path for each
//!   service (bad, stale and wrong-kind handles, busy and already-open
//!   databases, arguments, too-large keys, I/O failure and poisoning,
//!   corruption found by recovery).

use super::*;

fn raw(text: &str) -> (usize, usize) {
    (text.as_ptr() as usize, text.len())
}

fn open(dir: &Path, fsync: u32) -> i64 {
    let path = dir.to_str().unwrap();
    let (addr, len) = raw(path);
    codira_db_open(addr, len, fsync, 0)
}

fn put(txn: i64, key: &[u8], value: &[u8]) -> i32 {
    codira_db_put(
        txn,
        key.as_ptr() as usize,
        key.len(),
        value.as_ptr() as usize,
        value.len(),
    )
}

fn delete(txn: i64, key: &[u8]) -> i32 {
    codira_db_delete(txn, key.as_ptr() as usize, key.len())
}

/// The value of `key`, or the error code.
fn get(txn: i64, key: &[u8]) -> Result<Vec<u8>, i64> {
    let mut buf = vec![0u8; 256];
    let got = codira_db_get(
        txn,
        key.as_ptr() as usize,
        key.len(),
        buf.as_mut_ptr() as usize,
        buf.len(),
    );
    if got < 0 {
        return Err(got);
    }
    buf.truncate(got as usize);
    Ok(buf)
}

fn value_len(txn: i64, key: &[u8]) -> i64 {
    codira_db_value_len(txn, key.as_ptr() as usize, key.len())
}

const NOT_FOUND: i64 = DB_ERR_NOT_FOUND as i64;
const BAD_HANDLE: i64 = DB_ERR_BAD_HANDLE as i64;

#[test]
fn a_database_round_trips_through_every_operation_under_every_policy() {
    for policy in [DB_FSYNC_ALWAYS, DB_FSYNC_ON_COMMIT, DB_FSYNC_NEVER] {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path(), policy);
        assert!(db > 0, "open failed: {db}");

        let txn = codira_db_begin(db);
        assert!(txn > 0, "begin failed: {txn}");
        assert_eq!(put(txn, b"alpha", b"1"), 0);
        assert_eq!(put(txn, b"beta", b"two"), 0);
        assert_eq!(put(txn, b"gamma", b""), 0);
        assert_eq!(put(txn, b"", b"empty key"), 0);
        assert_eq!(put(txn, b"doomed", b"x"), 0);
        assert_eq!(delete(txn, b"doomed"), 0);
        // A transaction reads its own writes.
        assert_eq!(get(txn, b"beta"), Ok(b"two".to_vec()));
        assert_eq!(get(txn, b"gamma"), Ok(Vec::new()));
        assert_eq!(get(txn, b"doomed"), Err(NOT_FOUND));
        assert_eq!(codira_db_commit(txn), 1);
        // The handle is gone after commit.
        assert_eq!(codira_db_commit(txn), BAD_HANDLE);

        // A second commit deletes one key and overwrites another.
        let txn = codira_db_begin(db);
        assert_eq!(delete(txn, b"alpha"), 0);
        assert_eq!(put(txn, b"beta", b"2"), 0);
        assert_eq!(codira_db_commit(txn), 2);
        assert_eq!(codira_db_checkpoint(db), 0);
        assert_eq!(codira_db_close(db), 0);
        assert_eq!(codira_db_close(db), DB_ERR_BAD_HANDLE);

        // Everything committed is there after reopening.
        let db = open(dir.path(), policy);
        assert!(db > 0, "reopen failed: {db}");
        let txn = codira_db_begin(db);
        assert_eq!(get(txn, b"alpha"), Err(NOT_FOUND));
        assert_eq!(get(txn, b"beta"), Ok(b"2".to_vec()));
        assert_eq!(get(txn, b"gamma"), Ok(Vec::new()));
        assert_eq!(get(txn, b""), Ok(b"empty key".to_vec()));
        assert_eq!(value_len(txn, b""), 9);
        assert_eq!(value_len(txn, b"gamma"), 0);
        assert_eq!(value_len(txn, b"alpha"), NOT_FOUND);
        // A transaction that wrote nothing commits at its snapshot.
        assert_eq!(codira_db_commit(txn), 2);
        assert_eq!(codira_db_close(db), 0);
    }
}

#[test]
fn a_buffer_too_small_is_refused_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), DB_FSYNC_ALWAYS);
    let txn = codira_db_begin(db);
    assert_eq!(put(txn, b"k", b"0123456789"), 0);
    let mut buf = [0xAAu8; 9];
    let key = b"k";
    let got = codira_db_get(
        txn,
        key.as_ptr() as usize,
        1,
        buf.as_mut_ptr() as usize,
        buf.len(),
    );
    assert_eq!(got, i64::from(DB_ERR_BUFFER_TOO_SMALL));
    assert!(buf.iter().all(|&byte| byte == 0xAA));
    assert_eq!(value_len(txn, key), 10);
    // An empty value fits a null buffer of length 0.
    assert_eq!(put(txn, b"e", b""), 0);
    assert_eq!(codira_db_get(txn, b"e".as_ptr() as usize, 1, 0, 0), 0);
    assert_eq!(codira_db_abort(txn), 0);
    assert_eq!(codira_db_close(db), 0);
}

#[test]
fn snapshot_isolation_and_the_conflict_code() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), DB_FSYNC_ON_COMMIT);
    let setup = codira_db_begin(db);
    assert_eq!(put(setup, b"balance", b"100"), 0);
    assert_eq!(codira_db_commit(setup), 1);

    let first = codira_db_begin(db);
    let second = codira_db_begin(db);
    assert_eq!(put(first, b"balance", b"90"), 0);
    assert_eq!(put(second, b"balance", b"80"), 0);
    assert_eq!(codira_db_commit(first), 2);
    // `second` still reads its snapshot (plus its own write) ...
    assert_eq!(get(second, b"balance"), Ok(b"80".to_vec()));
    // ... and loses: first committer wins.
    assert_eq!(codira_db_commit(second), i64::from(DB_ERR_CONFLICT));
    assert_eq!(codira_db_last_os_error(), 0);
    // A conflict ends the transaction too.
    assert_eq!(codira_db_abort(second), DB_ERR_BAD_HANDLE);

    let reader = codira_db_begin(db);
    assert_eq!(get(reader, b"balance"), Ok(b"90".to_vec()));
    // A reader that started before a commit does not see it.
    let writer = codira_db_begin(db);
    assert_eq!(put(writer, b"balance", b"70"), 0);
    assert_eq!(codira_db_commit(writer), 3);
    assert_eq!(get(reader, b"balance"), Ok(b"90".to_vec()));
    assert_eq!(codira_db_abort(reader), 0);
    assert_eq!(codira_db_close(db), 0);
}

#[test]
fn bad_stale_and_wrong_kind_handles_are_refused_by_every_operation() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), DB_FSYNC_ALWAYS);
    let txn = codira_db_begin(db);
    let done = codira_db_begin(db);
    assert_eq!(codira_db_abort(done), 0);
    let key = b"k";
    let mut buf = [0u8; 4];
    let (k, b) = (key.as_ptr() as usize, buf.as_mut_ptr() as usize);

    // Not transactions: a database, an aborted transaction, forgeries, and a
    // `native_fs`-style handle.
    for bad in [db, done, 0, -1, i64::MIN, i64::MAX, txn | (1 << 50), 1] {
        assert_eq!(codira_db_get(bad, k, 1, b, 4), BAD_HANDLE, "{bad}");
        assert_eq!(codira_db_value_len(bad, k, 1), BAD_HANDLE, "{bad}");
        assert_eq!(codira_db_put(bad, k, 1, k, 1), DB_ERR_BAD_HANDLE, "{bad}");
        assert_eq!(codira_db_delete(bad, k, 1), DB_ERR_BAD_HANDLE, "{bad}");
        assert_eq!(codira_db_commit(bad), BAD_HANDLE, "{bad}");
        assert_eq!(codira_db_abort(bad), DB_ERR_BAD_HANDLE, "{bad}");
        assert_eq!(codira_db_last_os_error(), 0);
    }
    // Not databases: a transaction, forgeries.
    for bad in [txn, 0, -1, i64::MIN, i64::MAX, db | (1 << 50), 1] {
        assert_eq!(codira_db_begin(bad), BAD_HANDLE, "{bad}");
        assert_eq!(codira_db_checkpoint(bad), DB_ERR_BAD_HANDLE, "{bad}");
        assert_eq!(codira_db_close(bad), DB_ERR_BAD_HANDLE, "{bad}");
    }
    assert_eq!(codira_db_abort(txn), 0);
    assert_eq!(codira_db_close(db), 0);
    // A closed database's handle stays refused after its slot is reused.
    let again = open(dir.path(), DB_FSYNC_ALWAYS);
    assert!(again > 0);
    assert_ne!(again, db);
    assert_eq!(codira_db_begin(db), BAD_HANDLE);
    assert_eq!(codira_db_close(again), 0);
}

#[test]
fn close_waits_for_transactions_and_a_directory_opens_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), DB_FSYNC_ALWAYS);
    // The same directory under another spelling is the same database.
    let other_spelling = dir.path().join(".");
    assert_eq!(
        open(&other_spelling, DB_FSYNC_ALWAYS),
        i64::from(DB_ERR_ALREADY_OPEN)
    );
    let txn = codira_db_begin(db);
    assert_eq!(put(txn, b"k", b"v"), 0);
    assert_eq!(codira_db_close(db), DB_ERR_BUSY);
    // Still open and usable.
    assert_eq!(codira_db_commit(txn), 1);
    assert_eq!(codira_db_close(db), 0);
    // Closing released the directory.
    let db = open(&other_spelling, DB_FSYNC_ALWAYS);
    assert!(db > 0, "{db}");
    assert_eq!(codira_db_close(db), 0);
}

#[test]
fn arguments_are_validated_before_anything_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let (addr, len) = raw(path);
    let invalid = i64::from(DB_ERR_INVALID_ARGUMENT);
    assert_eq!(codira_db_open(addr, len, 3, 0), invalid);
    assert_eq!(codira_db_open(addr, len, u32::MAX, 0), invalid);
    assert_eq!(codira_db_open(addr, len, DB_FSYNC_ALWAYS, -1), invalid);
    assert_eq!(codira_db_open(0, len, DB_FSYNC_ALWAYS, 0), invalid);
    assert_eq!(codira_db_open(addr, 0, DB_FSYNC_ALWAYS, 0), invalid);
    let long = "a".repeat(DB_MAX_PATH_BYTES + 1);
    let (long_addr, long_len) = raw(&long);
    assert_eq!(
        codira_db_open(long_addr, long_len, DB_FSYNC_ALWAYS, 0),
        i64::from(DB_ERR_PATH_TOO_LONG)
    );
    let not_utf8 = [0x66u8, 0xFF, 0x66];
    assert_eq!(
        codira_db_open(not_utf8.as_ptr() as usize, 3, DB_FSYNC_ALWAYS, 0),
        i64::from(DB_ERR_PATH_NOT_UTF8)
    );
    let (nul_addr, nul_len) = raw("db\0dir");
    assert_eq!(
        codira_db_open(nul_addr, nul_len, DB_FSYNC_ALWAYS, 0),
        invalid
    );
    assert_eq!(codira_db_last_os_error(), 0);

    let db = open(dir.path(), DB_FSYNC_ALWAYS);
    let txn = codira_db_begin(db);
    let key = b"k".as_ptr() as usize;
    // Lengths above the engine's limit are refused before the address is
    // read, so a bogus address is never dereferenced.
    let too_large = i64::from(DB_ERR_TOO_LARGE);
    assert_eq!(codira_db_put(txn, 8, MAX_LEN + 1, key, 1), DB_ERR_TOO_LARGE);
    assert_eq!(codira_db_put(txn, key, 1, 8, MAX_LEN + 1), DB_ERR_TOO_LARGE);
    assert_eq!(codira_db_delete(txn, 8, usize::MAX), DB_ERR_TOO_LARGE);
    assert_eq!(codira_db_value_len(txn, 8, MAX_LEN + 1), too_large);
    // A null key or value with a length, a null or oversized buffer.
    assert_eq!(codira_db_put(txn, 0, 1, key, 1), DB_ERR_INVALID_ARGUMENT);
    assert_eq!(codira_db_put(txn, key, 1, 0, 1), DB_ERR_INVALID_ARGUMENT);
    assert_eq!(codira_db_delete(txn, 0, 1), DB_ERR_INVALID_ARGUMENT);
    assert_eq!(codira_db_get(txn, key, 1, 0, 4), invalid);
    assert_eq!(codira_db_get(txn, key, 1, 8, usize::MAX), invalid);
    // Nothing was written by the refused calls.
    assert_eq!(codira_db_commit(txn), 0);
    assert_eq!(codira_db_close(db), 0);
}

#[test]
fn io_failure_poisons_the_database_and_is_reported() {
    let parent = tempfile::tempdir().unwrap();
    // A regular file where the directory should be.
    let file = parent.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    assert_eq!(open(&file, DB_FSYNC_ALWAYS), i64::from(DB_ERR_IO));
    assert_ne!(codira_db_last_os_error(), 0);

    let dir = parent.path().join("db");
    let db = open(&dir, DB_FSYNC_ALWAYS);
    assert!(db > 0);
    let txn = codira_db_begin(db);
    assert_eq!(put(txn, b"k", b"v"), 0);
    assert_eq!(codira_db_commit(txn), 1);
    // Pull the directory out from under the engine: the checkpoint cannot
    // create its new log.
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(codira_db_checkpoint(db), DB_ERR_IO);
    #[cfg(unix)]
    assert_eq!(codira_db_last_os_error(), libc::ENOENT);
    // Poisoned from now on.
    let txn = codira_db_begin(db);
    assert_eq!(put(txn, b"k", b"w"), 0);
    assert_eq!(codira_db_commit(txn), i64::from(DB_ERR_POISONED));
    assert_eq!(codira_db_last_os_error(), 0);
    assert_eq!(codira_db_checkpoint(db), DB_ERR_POISONED);
    assert_eq!(codira_db_close(db), DB_ERR_POISONED);
    // The handle is gone, and so is the claim on the directory.
    assert_eq!(codira_db_close(db), DB_ERR_BAD_HANDLE);
    let db = open(&dir, DB_FSYNC_ALWAYS);
    assert!(db > 0);
    assert_eq!(codira_db_close(db), 0);
}

#[test]
fn corruption_found_by_recovery_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), DB_FSYNC_ALWAYS);
    for value in [[b'A'; 64], [b'B'; 64]] {
        let txn = codira_db_begin(db);
        assert_eq!(put(txn, b"k", &value), 0);
        assert!(codira_db_commit(txn) > 0);
    }
    assert_eq!(codira_db_close(db), 0);
    // Flip a byte inside the first record's payload; the second record after
    // it makes this corruption, not a torn tail.
    let log = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "log"))
        .expect("a log file");
    let mut bytes = std::fs::read(&log).unwrap();
    let at = bytes
        .windows(64)
        .position(|window| window == [b'A'; 64])
        .unwrap();
    bytes[at + 10] ^= 1;
    std::fs::write(&log, bytes).unwrap();
    assert_eq!(
        open(dir.path(), DB_FSYNC_ALWAYS),
        i64::from(DB_ERR_CORRUPTION)
    );
    // A failed open releases the directory claim: the same error again, not
    // "already open".
    assert_eq!(
        open(dir.path(), DB_FSYNC_ALWAYS),
        i64::from(DB_ERR_CORRUPTION)
    );
}

#[test]
fn automatic_checkpoints_follow_the_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let (addr, len) = raw(path);
    let db = codira_db_open(addr, len, DB_FSYNC_ALWAYS, 1);
    assert!(db > 0);
    let txn = codira_db_begin(db);
    assert_eq!(put(txn, b"k", b"v"), 0);
    assert_eq!(codira_db_commit(txn), 1);
    assert!(dir.path().join("snapshot").exists());
    assert_eq!(codira_db_close(db), 0);
}

#[test]
fn transactions_work_across_threads() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), DB_FSYNC_NEVER);
    std::thread::scope(|scope| {
        for thread in 0..4u8 {
            scope.spawn(move || {
                for i in 0..25u8 {
                    let txn = codira_db_begin(db);
                    assert!(txn > 0);
                    assert_eq!(put(txn, &[thread, i], &[i]), 0);
                    assert!(codira_db_commit(txn) > 0);
                }
            });
        }
    });
    // One transaction handle used from several threads in turn.
    let txn = codira_db_begin(db);
    std::thread::scope(|scope| {
        for thread in 0..4u8 {
            scope.spawn(move || {
                assert_eq!(get(txn, &[thread, 24]), Ok(vec![24]));
            });
        }
    });
    assert_eq!(codira_db_commit(txn), 100);
    assert_eq!(codira_db_close(db), 0);
}

#[test]
fn every_service_is_in_the_dispatch_table() {
    let names: Vec<String> = functions()
        .into_iter()
        .map(|definition| definition.prototype.name)
        .collect();
    assert_eq!(
        names,
        [
            "codira_db_open",
            "codira_db_begin",
            "codira_db_value_len",
            "codira_db_get",
            "codira_db_put",
            "codira_db_delete",
            "codira_db_commit",
            "codira_db_abort",
            "codira_db_checkpoint",
            "codira_db_close",
            "codira_db_last_os_error",
        ]
    );
}
