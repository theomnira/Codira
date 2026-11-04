//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - `FaultStorage`, an in-memory `Storage` that models what a power loss
//!   keeps and what it drops, and can crash or fail at the N-th mutating
//!   operation (create, append, truncate, sync, rename, remove, `sync_dir`).
//! - `Rng`, a small deterministic PRNG (`SplitMix64`) so that every crash
//!   test is reproducible from its seed.
//!
//! The crash model (see also `storage::Storage`):
//! - every file has its current bytes and its durable bytes (as of the last
//!   successful `sync`). After a crash, a file that was only appended to
//!   since its last sync keeps its durable bytes plus *any prefix* of the
//!   unsynced bytes, possibly cutting a record in half (a torn write). A
//!   file that was truncated since its last sync reverts to its durable
//!   bytes;
//! - directory operations since the last `sync_dir` are kept as *any
//!   subset* of them (applied in their original order; half of the crash
//!   images use a prefix, half an arbitrary subset): a crash may lose any
//!   `create`, `rename` or `remove` that was not followed by `sync_dir`,
//!   and a later one may survive while an earlier one is lost;
//! - the operation the crash happens in does not complete: an `append` or
//!   `create` may leave a prefix of its bytes, every other operation is lost.
//!
//! This is a model, not a disk. It does not model reordered page writeback
//! within one unsynced file, sector-level garbage, lying fsyncs, or bit rot;
//! the engine's limits for those are in the crate README.

use std::{collections::BTreeMap, io, sync::Mutex};

use crate::storage::{Storage, SyncKind};

/// `SplitMix64`: a tiny, fast, deterministic generator. Not cryptographic.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`. `n` must be positive.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "Rng::below(0)");
        self.next_u64() % n
    }
}

#[derive(Clone, Debug, Default)]
struct Inode {
    data: Vec<u8>,
    durable: Vec<u8>,
}

#[derive(Clone, Debug)]
enum DirOp {
    Link(String, usize),
    Unlink(String),
    Rename(String, String),
}

fn apply_dir_op(names: &mut BTreeMap<String, usize>, op: &DirOp) {
    match op {
        DirOp::Link(name, id) => {
            names.insert(name.clone(), *id);
        }
        DirOp::Unlink(name) => {
            names.remove(name);
        }
        DirOp::Rename(from, to) => {
            if let Some(id) = names.remove(from) {
                names.insert(to.clone(), id);
            }
        }
    }
}

#[derive(Debug)]
struct State {
    inodes: Vec<Inode>,
    names: BTreeMap<String, usize>,
    durable_names: BTreeMap<String, usize>,
    pending: Vec<DirOp>,
    ops: u64,
    crash_at: Option<u64>,
    fail_at: Option<u64>,
    crashed: bool,
    rng: Rng,
}

enum Gate {
    Normal,
    CrashDuring,
}

fn crashed_error() -> io::Error {
    io::Error::other("simulated crash: storage is gone")
}

fn not_found(name: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no file `{name}`"))
}

/// Picks how many of `extra` unsynced bytes survive: none, all, or a random
/// count, each a third of the time.
fn surviving(rng: &mut Rng, extra: usize) -> usize {
    if extra == 0 {
        return 0;
    }
    match rng.below(3) {
        0 => 0,
        1 => extra,
        _ => rng.below(extra as u64 + 1) as usize,
    }
}

impl State {
    fn gate(&mut self) -> io::Result<Gate> {
        if self.crashed {
            return Err(crashed_error());
        }
        let n = self.ops;
        self.ops += 1;
        if self.fail_at == Some(n) {
            return Err(io::Error::other(format!(
                "injected I/O failure at operation {n}"
            )));
        }
        if self.crash_at == Some(n) {
            self.crashed = true;
            return Ok(Gate::CrashDuring);
        }
        Ok(Gate::Normal)
    }

    fn inode(&mut self, name: &str) -> io::Result<&mut Inode> {
        let id = *self.names.get(name).ok_or_else(|| not_found(name))?;
        Ok(&mut self.inodes[id])
    }
}

/// An in-memory `Storage` with crash and failure injection.
#[derive(Debug)]
pub struct FaultStorage {
    state: Mutex<State>,
}

impl Default for FaultStorage {
    fn default() -> Self {
        FaultStorage::new()
    }
}

impl FaultStorage {
    /// An empty directory with no faults scheduled.
    pub fn new() -> FaultStorage {
        FaultStorage {
            state: Mutex::new(State {
                inodes: Vec::new(),
                names: BTreeMap::new(),
                durable_names: BTreeMap::new(),
                pending: Vec::new(),
                ops: 0,
                crash_at: None,
                fail_at: None,
                crashed: false,
                rng: Rng::new(0),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Crash during mutating operation number `n` (counting from 0, from now
    /// on as counted by [`FaultStorage::ops`]). `seed` decides how much of a
    /// torn `append` lands.
    pub fn set_crash_at(&self, n: Option<u64>, seed: u64) {
        let mut st = self.lock();
        st.crash_at = n;
        st.rng = Rng::new(seed);
    }

    /// Make mutating operation number `n` fail with an I/O error, without
    /// crashing and without applying it.
    pub fn set_fail_at(&self, n: Option<u64>) {
        self.lock().fail_at = n;
    }

    /// The number of mutating operations performed so far.
    pub fn ops(&self) -> u64 {
        self.lock().ops
    }

    /// Whether a scheduled crash has happened.
    pub fn crashed(&self) -> bool {
        self.lock().crashed
    }

    /// The directory as a power loss *now* would leave it, as a fresh
    /// storage with no faults scheduled. `seed` decides which unsynced bytes
    /// and directory operations survive. Works whether or not a scheduled
    /// crash has happened.
    pub fn crash_image(&self, seed: u64) -> FaultStorage {
        let st = self.lock();
        let mut rng = Rng::new(seed);
        let mut names = st.durable_names.clone();
        // Half the time a prefix of the pending directory operations
        // survives, half the time an arbitrary subset (in their original
        // order): without `sync_dir`, nothing orders them.
        if rng.below(2) == 0 {
            let keep_ops = rng.below(st.pending.len() as u64 + 1) as usize;
            for op in &st.pending[..keep_ops] {
                apply_dir_op(&mut names, op);
            }
        } else {
            for op in &st.pending {
                if rng.below(2) == 0 {
                    apply_dir_op(&mut names, op);
                }
            }
        }
        let mut new_ids: BTreeMap<usize, usize> = BTreeMap::new();
        let mut inodes = Vec::new();
        let mut new_names = BTreeMap::new();
        for (name, id) in names {
            let new_id = *new_ids.entry(id).or_insert_with(|| {
                let old = &st.inodes[id];
                let content = if old.data.starts_with(&old.durable) {
                    let extra = old.data.len() - old.durable.len();
                    let keep = surviving(&mut rng, extra);
                    old.data[..old.durable.len() + keep].to_vec()
                } else {
                    old.durable.clone()
                };
                inodes.push(Inode {
                    data: content.clone(),
                    durable: content,
                });
                inodes.len() - 1
            });
            new_names.insert(name, new_id);
        }
        FaultStorage {
            state: Mutex::new(State {
                inodes,
                durable_names: new_names.clone(),
                names: new_names,
                pending: Vec::new(),
                ops: 0,
                crash_at: None,
                fail_at: None,
                crashed: false,
                rng: Rng::new(0),
            }),
        }
    }

    /// XORs `mask` into byte `offset` of a file, in both its current and
    /// durable contents: models bit rot, for corruption tests.
    pub fn corrupt(&self, name: &str, offset: usize, mask: u8) -> io::Result<()> {
        let mut st = self.lock();
        let inode = st.inode(name)?;
        if offset >= inode.data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "offset past end",
            ));
        }
        inode.data[offset] ^= mask;
        if offset < inode.durable.len() {
            inode.durable[offset] ^= mask;
        }
        Ok(())
    }
}

impl Storage for FaultStorage {
    fn list(&self) -> io::Result<Vec<String>> {
        let st = self.lock();
        if st.crashed {
            return Err(crashed_error());
        }
        Ok(st.names.keys().cloned().collect())
    }

    fn read(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let st = self.lock();
        if st.crashed {
            return Err(crashed_error());
        }
        Ok(st.names.get(name).map(|id| st.inodes[*id].data.clone()))
    }

    fn create(&self, name: &str, data: &[u8]) -> io::Result<()> {
        let mut st = self.lock();
        let gate = st.gate()?;
        let id = st.inodes.len();
        let bytes = match gate {
            Gate::Normal => data.to_vec(),
            Gate::CrashDuring => {
                if st.rng.below(2) == 0 {
                    return Err(crashed_error());
                }
                let keep = st.rng.below(data.len() as u64 + 1) as usize;
                data[..keep].to_vec()
            }
        };
        st.inodes.push(Inode {
            data: bytes,
            durable: Vec::new(),
        });
        st.names.insert(name.to_string(), id);
        st.pending.push(DirOp::Link(name.to_string(), id));
        match gate {
            Gate::Normal => Ok(()),
            Gate::CrashDuring => Err(crashed_error()),
        }
    }

    fn append(&self, name: &str, data: &[u8]) -> io::Result<()> {
        let mut st = self.lock();
        if !st.names.contains_key(name) && !st.crashed {
            return Err(not_found(name));
        }
        let gate = st.gate()?;
        let keep = match gate {
            Gate::Normal => data.len(),
            Gate::CrashDuring => st.rng.below(data.len() as u64 + 1) as usize,
        };
        st.inode(name)?.data.extend_from_slice(&data[..keep]);
        match gate {
            Gate::Normal => Ok(()),
            Gate::CrashDuring => Err(crashed_error()),
        }
    }

    fn truncate(&self, name: &str, len: u64) -> io::Result<()> {
        let mut st = self.lock();
        if !st.names.contains_key(name) && !st.crashed {
            return Err(not_found(name));
        }
        if let Gate::CrashDuring = st.gate()? {
            return Err(crashed_error());
        }
        let inode = st.inode(name)?;
        let len = usize::try_from(len).map_err(io::Error::other)?;
        if len > inode.data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "truncate cannot extend a file",
            ));
        }
        inode.data.truncate(len);
        Ok(())
    }

    fn sync(&self, name: &str, _kind: SyncKind) -> io::Result<()> {
        let mut st = self.lock();
        if !st.names.contains_key(name) && !st.crashed {
            return Err(not_found(name));
        }
        if let Gate::CrashDuring = st.gate()? {
            return Err(crashed_error());
        }
        let inode = st.inode(name)?;
        inode.durable = inode.data.clone();
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut st = self.lock();
        if !st.names.contains_key(from) && !st.crashed {
            return Err(not_found(from));
        }
        if let Gate::CrashDuring = st.gate()? {
            return Err(crashed_error());
        }
        let op = DirOp::Rename(from.to_string(), to.to_string());
        apply_dir_op(&mut st.names, &op);
        st.pending.push(op);
        Ok(())
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let mut st = self.lock();
        if !st.names.contains_key(name) && !st.crashed {
            return Err(not_found(name));
        }
        if let Gate::CrashDuring = st.gate()? {
            return Err(crashed_error());
        }
        let op = DirOp::Unlink(name.to_string());
        apply_dir_op(&mut st.names, &op);
        st.pending.push(op);
        Ok(())
    }

    fn sync_dir(&self) -> io::Result<()> {
        let mut st = self.lock();
        if let Gate::CrashDuring = st.gate()? {
            return Err(crashed_error());
        }
        st.durable_names = st.names.clone();
        st.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn synced_data_survives_and_unsynced_is_a_prefix() {
        let s = FaultStorage::new();
        s.create("f", b"abc").unwrap();
        s.sync("f", SyncKind::Data).unwrap();
        s.sync_dir().unwrap();
        s.append("f", b"defgh").unwrap();
        for seed in 0..50 {
            let img = s.crash_image(seed);
            let data = img.read("f").unwrap().unwrap();
            assert!(data.starts_with(b"abc"));
            assert!(b"abcdefgh".starts_with(&data));
        }
    }

    #[test]
    fn unsynced_create_may_vanish_and_synced_dir_keeps_it() {
        let s = FaultStorage::new();
        s.create("f", b"x").unwrap();
        s.sync("f", SyncKind::Data).unwrap();
        let vanished = (0..50).any(|seed| s.crash_image(seed).read("f").unwrap().is_none());
        assert!(
            vanished,
            "an unsynced directory entry must be able to vanish"
        );
        s.sync_dir().unwrap();
        for seed in 0..50 {
            assert_eq!(s.crash_image(seed).read("f").unwrap().unwrap(), b"x");
        }
    }

    #[test]
    fn rename_without_dir_sync_may_be_lost() {
        let s = FaultStorage::new();
        s.create("a", b"1").unwrap();
        s.sync("a", SyncKind::All).unwrap();
        s.sync_dir().unwrap();
        s.rename("a", "b").unwrap();
        let mut seen_old = false;
        let mut seen_new = false;
        for seed in 0..50 {
            let img = s.crash_image(seed);
            match (img.read("a").unwrap(), img.read("b").unwrap()) {
                (Some(_), None) => seen_old = true,
                (None, Some(_)) => seen_new = true,
                other => panic!("rename must be atomic, got {other:?}"),
            }
        }
        assert!(seen_old && seen_new);
    }

    #[test]
    fn crash_at_stops_everything_after() {
        let s = FaultStorage::new();
        s.set_crash_at(Some(1), 3);
        s.create("f", b"x").unwrap();
        assert!(s.append("f", b"yy").is_err());
        assert!(s.crashed());
        assert!(s.sync("f", SyncKind::Data).is_err());
        assert!(s.read("f").is_err());
    }

    #[test]
    fn injected_failure_does_not_apply_the_operation() {
        let s = FaultStorage::new();
        s.create("f", b"x").unwrap();
        s.set_fail_at(Some(1));
        assert!(s.append("f", b"y").is_err());
        assert!(!s.crashed());
        s.append("f", b"z").unwrap();
        assert_eq!(s.read("f").unwrap().unwrap(), b"xz");
    }
}
