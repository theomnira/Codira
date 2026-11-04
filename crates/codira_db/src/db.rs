//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - `Db`: the key-value engine. An in-memory ordered multi-version index
//!   (`BTreeMap` of version chains) made durable by a write-ahead log and
//!   periodic checkpoint snapshots.
//! - `Txn`: snapshot-isolation transactions with buffered writes,
//!   first-committer-wins write-write conflict detection, and range scans
//!   over the transaction's snapshot.
//! - Recovery on open: load the snapshot, replay the logs that follow it,
//!   discard a torn tail, report corruption.
//! - Checkpoints: switch to a new log, write the snapshot atomically
//!   (temp file, fsync, rename, fsync directory), delete the old logs.
//!
//! Isolation level: snapshot isolation (Berenson et al., SIGMOD 1995). It
//! is NOT serializable: two transactions that read overlapping data and
//! write disjoint keys can both commit, so write skew is possible (the test
//! `write_skew_is_possible` in `tests/isolation.rs` demonstrates it).

use std::{
    collections::BTreeMap,
    ops::Bound,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
    },
};

use crate::{
    codec::{check_len, Reader, Writer},
    error::corruption,
    snapshot,
    storage::{FsStorage, Storage, SyncKind},
    wal, Error, Result,
};

/// When the log is flushed to stable storage.
///
/// A transaction writes exactly one log record, at commit, so the policy
/// decides whether `commit` returns before or after that record is durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsyncPolicy {
    /// `commit` returns after `fsync` (`File::sync_all`: data and all
    /// metadata) of the log. A commit that returned `Ok` survives a crash.
    Always,
    /// `commit` returns after `fdatasync` (`File::sync_data`: data and the
    /// metadata needed to read it, such as the file size, but not the
    /// modification time) of the log. Same crash guarantee as `Always`,
    /// usually cheaper.
    OnCommit,
    /// `commit` never flushes. Only checkpoints and `Db::close` flush. A
    /// crash may lose any suffix of the commits since the last flush. For
    /// tests and bulk loads only: on a real file system, unsynced pages can
    /// also be written back out of order, and recovery may then report
    /// corruption instead of a clean torn tail.
    Never,
}

/// Engine configuration.
#[derive(Clone, Debug)]
pub struct Options {
    /// See [`FsyncPolicy`].
    pub fsync: FsyncPolicy,
    /// Run a checkpoint after a commit leaves the active log at least this
    /// many bytes long. `None` checkpoints only on [`Db::checkpoint`].
    pub checkpoint_wal_bytes: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            fsync: FsyncPolicy::OnCommit,
            checkpoint_wal_bytes: Some(64 << 20),
        }
    }
}

/// What recovery did when the database was opened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// The commit timestamp the snapshot was taken at, if there was one.
    pub snapshot_ts: Option<u64>,
    /// Log files replayed.
    pub wal_files: usize,
    /// Commit records replayed from the logs.
    pub records_replayed: u64,
    /// Bytes of torn tail discarded from the newest log (and truncated).
    pub torn_bytes_discarded: u64,
    /// The newest commit timestamp after recovery.
    pub last_commit_ts: u64,
}

#[derive(Clone, Debug)]
struct Version {
    ts: u64,
    /// `None` is a tombstone (the key was deleted at `ts`).
    value: Option<Vec<u8>>,
}

struct Mvcc {
    /// Version chains, oldest first, never empty.
    versions: BTreeMap<Vec<u8>, Vec<Version>>,
    committed_ts: u64,
}

struct WalState {
    generation: u64,
    oldest_generation: u64,
    size: u64,
}

/// The storage engine. Share it between threads by reference (it is
/// `Sync`); each thread runs its own transactions.
pub struct Db {
    storage: Arc<dyn Storage>,
    opts: Options,
    state: RwLock<Mvcc>,
    /// Snapshot timestamps of open transactions, with counts.
    active: Mutex<BTreeMap<u64, usize>>,
    /// The log, and the commit lock: commits are serialised by it.
    wal: Mutex<WalState>,
    poisoned: AtomicBool,
    report: RecoveryReport,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn read<T>(m: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn write<T>(m: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

const COMMIT_RECORD: u8 = 1;
const OP_PUT: u8 = 1;
const OP_DELETE: u8 = 2;

type WriteSet = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

fn encode_commit(ts: u64, writes: &WriteSet) -> Vec<u8> {
    let mut w = Writer::default();
    w.u8(COMMIT_RECORD);
    w.u64(ts);
    w.u32(writes.len() as u32);
    for (k, v) in writes {
        if let Some(v) = v {
            w.u8(OP_PUT);
            w.bytes(k);
            w.bytes(v);
        } else {
            w.u8(OP_DELETE);
            w.bytes(k);
        }
    }
    w.buf
}

type Ops = Vec<(Vec<u8>, Option<Vec<u8>>)>;

fn decode_commit(file: &str, offset: u64, rec: &[u8]) -> Result<(u64, Ops)> {
    let bad = || corruption(file, offset, "malformed commit record");
    let mut r = Reader::new(rec);
    if r.u8().ok_or_else(bad)? != COMMIT_RECORD {
        return Err(corruption(file, offset, "unknown record type"));
    }
    let ts = r.u64().ok_or_else(bad)?;
    let n = r.u32().ok_or_else(bad)?;
    let mut ops = Vec::new();
    for _ in 0..n {
        let op = r.u8().ok_or_else(bad)?;
        let key = r.bytes().ok_or_else(bad)?.to_vec();
        match op {
            OP_PUT => ops.push((key, Some(r.bytes().ok_or_else(bad)?.to_vec()))),
            OP_DELETE => ops.push((key, None)),
            _ => return Err(bad()),
        }
    }
    if !r.is_empty() {
        return Err(bad());
    }
    Ok((ts, ops))
}

/// The newest version visible at `snapshot`.
fn visible(chain: &[Version], snapshot: u64) -> Option<&Version> {
    chain.iter().rev().find(|v| v.ts <= snapshot)
}

/// Drops versions no open or future transaction can see. Returns `false`
/// if the whole chain can go (a lone tombstone visible to everyone).
fn prune(chain: &mut Vec<Version>, horizon: u64) -> bool {
    if let Some(i) = chain.iter().rposition(|v| v.ts <= horizon) {
        chain.drain(..i);
    }
    !(chain.len() == 1 && chain[0].value.is_none() && chain[0].ts <= horizon)
}

/// Whether `BTreeMap::range` would accept these bounds (it panics on an
/// inverted or empty-exclusive range).
fn valid_range(start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    match (start, end) {
        (Bound::Included(s), Bound::Included(e)) => s <= e,
        (Bound::Included(s), Bound::Excluded(e)) | (Bound::Excluded(s), Bound::Included(e)) => {
            s <= e
        }
        (Bound::Excluded(s), Bound::Excluded(e)) => s < e,
        _ => true,
    }
}

/// The smallest key greater than every key with this prefix, if any.
pub(crate) fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last != 0xFF {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

impl Db {
    /// Opens (or creates) the database in directory `dir`.
    pub fn open_dir(dir: impl AsRef<std::path::Path>, opts: Options) -> Result<Db> {
        Db::open(Arc::new(FsStorage::open(dir)?), opts)
    }

    /// Opens (or creates) the database stored in `storage`, running crash
    /// recovery. See [`Db::recovery_report`] for what it did.
    pub fn open(storage: Arc<dyn Storage>, opts: Options) -> Result<Db> {
        let names = storage.list()?;
        let mut dir_changed = false;
        if names.iter().any(|n| n == snapshot::TEMP_FILE) {
            // A checkpoint crashed before publishing its snapshot.
            storage.remove(snapshot::TEMP_FILE)?;
            dir_changed = true;
        }
        let mut report = RecoveryReport::default();
        let mut latest: BTreeMap<Vec<u8>, (u64, Vec<u8>)> = BTreeMap::new();
        let (first_gen, mut last_ts, have_snapshot) = match storage.read(snapshot::FILE)? {
            Some(data) => {
                let snap = snapshot::decode(&data)?;
                report.snapshot_ts = Some(snap.last_ts);
                for (k, v) in snap.entries {
                    latest.insert(k, (snap.last_ts, v));
                }
                (snap.wal_generation, snap.last_ts, true)
            }
            None => (1, 0, false),
        };

        let mut gens: Vec<u64> = names
            .iter()
            .filter_map(|n| wal::parse_file_name(n))
            .collect();
        gens.sort_unstable();
        let stale: Vec<u64> = gens.iter().copied().filter(|g| *g < first_gen).collect();
        let live: Vec<u64> = gens.iter().copied().filter(|g| *g >= first_gen).collect();

        let mut size = wal::FILE_HEADER_LEN as u64;
        let generation;
        if live.is_empty() {
            if have_snapshot {
                return Err(corruption(
                    &wal::file_name(first_gen),
                    0,
                    "the snapshot continues in this log, which is missing",
                ));
            }
            if !stale.is_empty() {
                return Err(corruption(
                    snapshot::FILE,
                    0,
                    "logs exist but the snapshot is missing",
                ));
            }
            // A new database.
            let name = wal::file_name(first_gen);
            storage.create(&name, &wal::file_header(first_gen))?;
            storage.sync(&name, SyncKind::All)?;
            storage.sync_dir()?;
            generation = first_gen;
        } else {
            for (i, g) in live.iter().enumerate() {
                if *g != first_gen + i as u64 {
                    return Err(corruption(
                        &wal::file_name(first_gen + i as u64),
                        0,
                        format!("log generation missing (found {g} instead)"),
                    ));
                }
            }
            for (i, &g) in live.iter().enumerate() {
                let newest = i + 1 == live.len();
                let name = wal::file_name(g);
                let data = storage
                    .read(&name)?
                    .ok_or_else(|| corruption(&name, 0, "log vanished during recovery"))?;
                let scan = wal::scan(&name, &data, g)?;
                report.wal_files += 1;
                if scan.torn_header {
                    if !newest {
                        return Err(corruption(
                            &name,
                            0,
                            "truncated header in a log that is not the newest",
                        ));
                    }
                    storage.create(&name, &wal::file_header(g))?;
                    storage.sync(&name, SyncKind::All)?;
                    storage.sync_dir()?;
                    report.torn_bytes_discarded += scan.torn_bytes;
                    continue;
                }
                if scan.torn_bytes > 0 && !newest {
                    return Err(corruption(
                        &name,
                        scan.valid_len,
                        "torn record in a log that is not the newest",
                    ));
                }
                for (offset, rec) in &scan.records {
                    let (ts, ops) = decode_commit(&name, *offset, rec)?;
                    if ts != last_ts + 1 {
                        return Err(corruption(
                            &name,
                            *offset,
                            format!("commit timestamp {ts} does not follow {last_ts}"),
                        ));
                    }
                    for (k, v) in ops {
                        match v {
                            Some(v) => {
                                latest.insert(k, (ts, v));
                            }
                            None => {
                                latest.remove(&k);
                            }
                        }
                    }
                    last_ts = ts;
                    report.records_replayed += 1;
                }
                if newest {
                    if scan.torn_bytes > 0 {
                        storage.truncate(&name, scan.valid_len)?;
                        storage.sync(&name, SyncKind::All)?;
                        report.torn_bytes_discarded += scan.torn_bytes;
                    }
                    size = scan.valid_len;
                }
            }
            generation = *live.last().expect("live is not empty");
        }
        for g in stale {
            storage.remove(&wal::file_name(g))?;
            dir_changed = true;
        }
        if dir_changed {
            storage.sync_dir()?;
        }
        report.last_commit_ts = last_ts;

        let versions = latest
            .into_iter()
            .map(|(k, (ts, v))| (k, vec![Version { ts, value: Some(v) }]))
            .collect();
        Ok(Db {
            storage,
            opts,
            state: RwLock::new(Mvcc {
                versions,
                committed_ts: last_ts,
            }),
            active: Mutex::new(BTreeMap::new()),
            wal: Mutex::new(WalState {
                generation,
                oldest_generation: first_gen,
                size,
            }),
            poisoned: AtomicBool::new(false),
            report,
        })
    }

    /// What recovery did when this database was opened.
    pub fn recovery_report(&self) -> &RecoveryReport {
        &self.report
    }

    /// The newest commit timestamp. Timestamps start at 1 and increase by
    /// exactly one per committed read-write transaction.
    pub fn last_commit_ts(&self) -> u64 {
        read(&self.state).committed_ts
    }

    /// Whether an I/O failure has poisoned the database.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    /// The options the database was opened with.
    pub fn options(&self) -> &Options {
        &self.opts
    }

    /// Starts a transaction reading the snapshot of every transaction
    /// committed so far.
    pub fn begin(&self) -> Txn<'_> {
        // Registering under the state lock makes the registration atomic
        // with respect to version pruning in `commit`.
        let st = read(&self.state);
        let snapshot = st.committed_ts;
        *lock(&self.active).entry(snapshot).or_insert(0) += 1;
        drop(st);
        Txn {
            db: self,
            snapshot,
            writes: BTreeMap::new(),
            finished: false,
        }
    }

    fn unregister(&self, snapshot: u64) {
        let mut active = lock(&self.active);
        if let Some(n) = active.get_mut(&snapshot) {
            *n -= 1;
            if *n == 0 {
                active.remove(&snapshot);
            }
        }
    }

    fn poison<T>(&self, e: impl Into<Error>) -> Result<T> {
        self.poisoned.store(true, Ordering::SeqCst);
        Err(e.into())
    }

    fn commit_writes(&self, snapshot: u64, writes: WriteSet) -> Result<u64> {
        let mut wal = lock(&self.wal);
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        let ts = {
            let st = read(&self.state);
            for k in writes.keys() {
                if let Some(chain) = st.versions.get(k) {
                    if chain.last().expect("chains are never empty").ts > snapshot {
                        return Err(Error::Conflict { key: k.clone() });
                    }
                }
            }
            st.committed_ts + 1
        };
        let payload = encode_commit(ts, &writes);
        check_len("commit record", payload.len())?;
        let record = wal::frame(&payload);
        let name = wal::file_name(wal.generation);
        if let Err(e) = self.storage.append(&name, &record) {
            return self.poison(e);
        }
        wal.size += record.len() as u64;
        let synced = match self.opts.fsync {
            FsyncPolicy::Always => self.storage.sync(&name, SyncKind::All),
            FsyncPolicy::OnCommit => self.storage.sync(&name, SyncKind::Data),
            FsyncPolicy::Never => Ok(()),
        };
        if let Err(e) = synced {
            return self.poison(e);
        }
        {
            let mut st = write(&self.state);
            // The oldest snapshot any other open transaction reads at; the
            // committing transaction's own snapshot no longer counts.
            let horizon = lock(&self.active)
                .iter()
                .find(|(s, n)| **n > usize::from(**s == snapshot))
                .map_or(ts, |(s, _)| *s);
            for (k, value) in writes {
                let chain = st.versions.entry(k.clone()).or_default();
                chain.push(Version { ts, value });
                if !prune(chain, horizon) {
                    st.versions.remove(&k);
                }
            }
            st.committed_ts = ts;
        }
        if let Some(limit) = self.opts.checkpoint_wal_bytes {
            if wal.size >= limit {
                // The commit is already durable (per policy); a failed
                // checkpoint poisons the database for later commits.
                let _ = self.checkpoint_locked(&mut wal);
            }
        }
        Ok(ts)
    }

    /// Writes a snapshot of the committed state and deletes the logs it
    /// replaces. Blocks commits (not readers) while it runs.
    pub fn checkpoint(&self) -> Result<()> {
        let mut wal = lock(&self.wal);
        self.checkpoint_locked(&mut wal)
    }

    fn checkpoint_locked(&self, wal: &mut WalState) -> Result<()> {
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        let result = self.checkpoint_steps(wal);
        if let Err(e) = result {
            return self.poison(e);
        }
        Ok(())
    }

    fn checkpoint_steps(&self, wal: &mut WalState) -> Result<()> {
        let s = &self.storage;
        // 1. Flush the current log, whatever the policy, so that a torn
        //    tail can only ever be in the newest log.
        s.sync(&wal::file_name(wal.generation), SyncKind::All)?;
        // 2. Switch to a new, durable, empty log.
        let next = wal.generation + 1;
        let next_name = wal::file_name(next);
        s.create(&next_name, &wal::file_header(next))?;
        s.sync(&next_name, SyncKind::All)?;
        s.sync_dir()?;
        wal.generation = next;
        wal.size = wal::FILE_HEADER_LEN as u64;
        // 3. Collect garbage versions, then encode the latest committed
        //    state. The commit lock is held, so nothing commits meanwhile.
        {
            let mut st = write(&self.state);
            let horizon = lock(&self.active)
                .keys()
                .next()
                .copied()
                .unwrap_or(st.committed_ts);
            st.versions.retain(|_, chain| prune(chain, horizon));
        }
        let bytes = {
            let st = read(&self.state);
            let entries: Vec<(&[u8], &[u8])> = st
                .versions
                .iter()
                .filter_map(|(k, chain)| {
                    let v = chain.last().expect("chains are never empty");
                    v.value.as_deref().map(|v| (k.as_slice(), v))
                })
                .collect();
            snapshot::encode(next, st.committed_ts, entries.into_iter())
        };
        // 4. Publish the snapshot atomically.
        s.create(snapshot::TEMP_FILE, &bytes)?;
        s.sync(snapshot::TEMP_FILE, SyncKind::All)?;
        s.rename(snapshot::TEMP_FILE, snapshot::FILE)?;
        s.sync_dir()?;
        // 5. The old logs are now redundant.
        for g in wal.oldest_generation..next {
            s.remove(&wal::file_name(g))?;
        }
        wal.oldest_generation = next;
        s.sync_dir()?;
        Ok(())
    }

    /// Flushes the log (needed under [`FsyncPolicy::Never`]) and closes the
    /// database. Dropping a `Db` without `close` does not flush.
    pub fn close(self) -> Result<()> {
        let wal = lock(&self.wal);
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        self.storage
            .sync(&wal::file_name(wal.generation), SyncKind::All)?;
        Ok(())
    }

    /// The number of keys with at least one stored version, and the total
    /// number of versions (live values plus retained old versions and
    /// tombstones). For tests and monitoring.
    pub fn version_counts(&self) -> (usize, usize) {
        let st = read(&self.state);
        (st.versions.len(), st.versions.values().map(Vec::len).sum())
    }
}

/// A transaction. Reads see the snapshot taken at [`Db::begin`] plus this
/// transaction's own writes; writes are buffered until [`Txn::commit`].
/// Dropping a transaction without committing aborts it.
pub struct Txn<'db> {
    db: &'db Db,
    snapshot: u64,
    writes: WriteSet,
    finished: bool,
}

impl<'db> Txn<'db> {
    /// The commit timestamp this transaction reads at.
    pub fn snapshot_ts(&self) -> u64 {
        self.snapshot
    }

    /// The value of `key` in this transaction's view.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        if let Some(w) = self.writes.get(key) {
            return w.clone();
        }
        let st = read(&self.db.state);
        st.versions
            .get(key)
            .and_then(|chain| visible(chain, self.snapshot))
            .and_then(|v| v.value.clone())
    }

    /// Sets `key` to `value` when the transaction commits.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        check_len("key", key.len())?;
        check_len("value", value.len())?;
        self.writes.insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    }

    /// Deletes `key` when the transaction commits. Deleting a missing key is
    /// not an error, and still counts as a write for conflict detection.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        check_len("key", key.len())?;
        self.writes.insert(key.to_vec(), None);
        Ok(())
    }

    /// Every key-value pair with a key between `start` and `end`, in key
    /// order, in this transaction's view. The result is materialised: it is
    /// a consistent copy, and holds no lock after it returns. An empty or
    /// inverted range yields nothing.
    pub fn scan(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
        if !valid_range(start, end) {
            return Vec::new();
        }
        let mut out: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        {
            let st = read(&self.db.state);
            for (k, chain) in st.versions.range::<[u8], _>((start, end)) {
                if let Some(v) = visible(chain, self.snapshot).and_then(|v| v.value.as_ref()) {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        for (k, w) in self.writes.range::<[u8], _>((start, end)) {
            match w {
                Some(v) => {
                    out.insert(k.clone(), v.clone());
                }
                None => {
                    out.remove(k);
                }
            }
        }
        out.into_iter().collect()
    }

    /// Every key-value pair with `from <= key < to`, in key order.
    pub fn range(&self, from: &[u8], to: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.scan(Bound::Included(from), Bound::Excluded(to))
    }

    /// Every key-value pair, in key order.
    pub fn scan_all(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.scan(Bound::Unbounded, Bound::Unbounded)
    }

    /// Every key-value pair whose key starts with `prefix`, in key order.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        match prefix_end(prefix) {
            Some(end) => self.scan(Bound::Included(prefix), Bound::Excluded(&end)),
            None => self.scan(Bound::Included(prefix), Bound::Unbounded),
        }
    }

    /// Commits. Returns the commit timestamp, or, for a transaction that
    /// wrote nothing, its snapshot timestamp.
    ///
    /// Errors: [`Error::Conflict`] if another transaction committed a write
    /// to one of this transaction's keys after its snapshot (nothing was
    /// written; retry from `begin`); [`Error::Io`] if the log write or flush
    /// failed (the outcome is unknown and the database is poisoned);
    /// [`Error::Poisoned`] after an earlier I/O failure.
    pub fn commit(mut self) -> Result<u64> {
        self.finished = true;
        let result = if self.writes.is_empty() {
            Ok(self.snapshot)
        } else {
            let writes = std::mem::take(&mut self.writes);
            self.db.commit_writes(self.snapshot, writes)
        };
        self.db.unregister(self.snapshot);
        result
    }

    /// Discards the transaction's writes.
    pub fn abort(mut self) {
        self.finished = true;
        self.db.unregister(self.snapshot);
    }
}

impl Drop for Txn<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.db.unregister(self.snapshot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_end_handles_ff() {
        assert_eq!(prefix_end(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(prefix_end(b"a\xFF"), Some(b"b".to_vec()));
        assert_eq!(prefix_end(b"\xFF\xFF"), None);
        assert_eq!(prefix_end(b""), None);
    }

    #[test]
    fn prune_keeps_what_the_horizon_can_see() {
        let v = |ts, value: Option<&[u8]>| Version {
            ts,
            value: value.map(<[u8]>::to_vec),
        };
        let mut chain = vec![v(1, Some(b"a")), v(3, Some(b"b")), v(5, Some(b"c"))];
        assert!(prune(&mut chain, 4));
        assert_eq!(chain.iter().map(|v| v.ts).collect::<Vec<_>>(), vec![3, 5]);
        let mut chain = vec![v(1, Some(b"a")), v(2, None)];
        assert!(!prune(&mut chain, 2));
        let mut chain = vec![v(1, Some(b"a")), v(2, None)];
        assert!(prune(&mut chain, 1));
        assert_eq!(chain.len(), 2);
    }

    #[test]
    fn commit_record_round_trips() {
        let mut ws = WriteSet::new();
        ws.insert(b"k".to_vec(), Some(b"v".to_vec()));
        ws.insert(b"d".to_vec(), None);
        let rec = encode_commit(7, &ws);
        let (ts, ops) = decode_commit("w", 0, &rec).unwrap();
        assert_eq!(ts, 7);
        assert_eq!(
            ops,
            vec![(b"d".to_vec(), None), (b"k".to_vec(), Some(b"v".to_vec()))]
        );
        assert!(decode_commit("w", 0, &rec[..rec.len() - 1]).is_err());
    }
}
