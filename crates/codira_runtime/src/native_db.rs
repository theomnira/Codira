//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Database services the runtime provides to Codira programs through
//!   `extern "C"` declarations (`std/db/db.code`), backed by the `codira_db`
//!   storage engine (write-ahead log, snapshot-isolation MVCC, checkpoints;
//!   see its README): open a database directory with an fsync
//!   policy, begin a transaction, get, put, delete, commit, abort,
//!   checkpoint and close. Every function is registered in each runtime's
//!   dispatch table.
//!
//! The conventions are those of `native_fs`: addresses and lengths cross the
//! boundary as `usize`, every failure is a return value, never a panic, and
//! each body runs under `catch_unwind`, so a bug here becomes
//! [`DB_ERR_OTHER`] instead of an abort.
//!
//! * Operations that produce a number (`open`, `begin`, `get`, `value_len`,
//!   `commit`) return an `i64`: the value when it is zero or more, a negative
//!   `DB_ERR_*` code otherwise.
//! * Operations that only succeed or fail return an `i32`: `0`, or a negative
//!   `DB_ERR_*` code.
//! * [`codira_db_last_os_error`] returns the raw OS error number of the
//!   calling thread's last [`DB_ERR_IO`], for messages.
//!
//! Database and transaction handles are generation-checked entries of two
//! tables with distinct kind tags (`native_handles`), so a closed, stale or
//! forged handle, or a transaction handle passed where a database handle is
//! expected, is refused with [`DB_ERR_BAD_HANDLE`].
//!
//! A transaction keeps its database alive: [`codira_db_close`] refuses with
//! [`DB_ERR_BUSY`] (and the database stays open) while any transaction of it
//! is open or another thread is in the middle of a call on it. One directory
//! can be open once per process at a time ([`DB_ERR_ALREADY_OPEN`]); the
//! engine has no lock file, so nothing stops *another process* from opening
//! the same directory, which corrupts it (`crates/codira_db/README.md`).
//!
//! A transaction handle may be passed between threads; calls on one
//! transaction are serialised by a lock of its own, never by a global one.
//! Commits of one database are serialised by the engine.

use std::{
    cell::Cell,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::Arc,
};

use codira_db::{Db, Error, FsStorage, FsyncPolicy, Options, Txn, MAX_LEN};
use parking_lot::Mutex;

use crate::{native_handles::HandleTable, FunctionDefinition, IntoFunctionDefinition};

/// `fsync` policy for [`codira_db_open`]: `commit` returns after `fsync` of
/// the log (`FsyncPolicy::Always`).
pub const DB_FSYNC_ALWAYS: u32 = 0;
/// `fsync` policy: `commit` returns after `fdatasync` of the log
/// (`FsyncPolicy::OnCommit`); the same crash guarantee as `ALWAYS`.
pub const DB_FSYNC_ON_COMMIT: u32 = 1;
/// `fsync` policy: `commit` never flushes; only checkpoints and close do
/// (`FsyncPolicy::Never`). A crash may lose acknowledged commits.
pub const DB_FSYNC_NEVER: u32 = 2;

/// The longest directory path, in bytes, [`codira_db_open`] accepts.
pub const DB_MAX_PATH_BYTES: usize = 4096;

/// An unexpected internal failure (a caught panic).
pub const DB_ERR_OTHER: i32 = -1;
/// The key is not present in the transaction's view.
pub const DB_ERR_NOT_FOUND: i32 = -2;
/// A write-write conflict at commit: another transaction committed a write
/// to one of this transaction's keys after its snapshot. Nothing was
/// written, the transaction is gone; retry from `begin`.
pub const DB_ERR_CONFLICT: i32 = -3;
/// The storage failed. From `commit` or `checkpoint` this poisons the
/// database and the commit's outcome is unknown until it is reopened.
/// [`codira_db_last_os_error`] gives the OS error number.
pub const DB_ERR_IO: i32 = -4;
/// Recovery found a corrupt log or snapshot (a checksum failure that is not
/// a torn tail, a bad header, a gap in commit timestamps, ...).
pub const DB_ERR_CORRUPTION: i32 = -5;
/// An earlier I/O failure poisoned the database: every commit, checkpoint
/// and close fails with this. Reopen it to recover from the log.
pub const DB_ERR_POISONED: i32 = -6;
/// A key or value longer than the engine's limit (`codira_db::MAX_LEN`,
/// 1 GiB), or a transaction too large to encode.
pub const DB_ERR_TOO_LARGE: i32 = -7;
/// An invalid fsync policy, checkpoint threshold, length or address, or an
/// empty path or one with a NUL byte.
pub const DB_ERR_INVALID_ARGUMENT: i32 = -8;
/// The handle is not an open database or transaction of the expected kind
/// (never opened, closed, committed, aborted, or forged).
pub const DB_ERR_BAD_HANDLE: i32 = -9;
/// The value is longer than the caller's buffer; nothing was written.
/// [`codira_db_value_len`] gives the length needed.
pub const DB_ERR_BUFFER_TOO_SMALL: i32 = -10;
/// The database has open transactions, or another thread is in a call on
/// it; it stays open.
pub const DB_ERR_BUSY: i32 = -11;
/// The directory is already open as a database in this process.
pub const DB_ERR_ALREADY_OPEN: i32 = -12;
/// The runtime's table of databases or transactions is full.
pub const DB_ERR_TOO_MANY_OPEN: i32 = -13;
/// The path is longer than [`DB_MAX_PATH_BYTES`].
pub const DB_ERR_PATH_TOO_LONG: i32 = -14;
/// The path is not valid UTF-8.
pub const DB_ERR_PATH_NOT_UTF8: i32 = -15;
/// An error of the engine's typed-table layer. Reserved: the key-value
/// services of this module never return it.
pub const DB_ERR_SCHEMA: i32 = -16;

/// An open database and the claim on its directory.
struct OpenDb {
    db: Db,
    _dir: DirClaim,
}

/// The canonical path of an open database directory, released when the
/// database is dropped, however that happens.
struct DirClaim(PathBuf);

impl Drop for DirClaim {
    fn drop(&mut self) {
        OPEN_DIRS.lock().retain(|dir| *dir != self.0);
    }
}

/// An open transaction.
///
/// `txn` borrows `_db.db`; the borrow is given the `'static` lifetime
/// because `_db` keeps the `Db` alive and at a fixed address (inside an
/// `Arc`) for as long as this value exists. The transaction is declared first
/// so that it is dropped before `_db` (fields drop in declaration order), and
/// it never
/// leaves this struct except into `Txn::commit`/`Txn::abort`, which finish
/// before the struct is dropped.
struct OpenTxn {
    txn: Option<Txn<'static>>,
    /// Only held, never read: it keeps the borrowed `Db` alive.
    _db: Arc<OpenDb>,
}

type SharedTxn = Arc<Mutex<OpenTxn>>;

static DBS: Mutex<HandleTable<Arc<OpenDb>>> = Mutex::new(HandleTable::new(1));
static TXNS: Mutex<HandleTable<SharedTxn>> = Mutex::new(HandleTable::new(2));
static OPEN_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

thread_local! {
    static LAST_OS_ERROR: Cell<i32> = const { Cell::new(0) };
}

/// Maps an engine error to a `DB_ERR_*` code and records its OS error
/// number.
fn error_code(error: &Error) -> i32 {
    let os = match error {
        Error::Io(io) => io.raw_os_error().unwrap_or(0),
        _ => 0,
    };
    LAST_OS_ERROR.with(|last| last.set(os));
    match error {
        Error::Io(_) => DB_ERR_IO,
        Error::Corruption { .. } => DB_ERR_CORRUPTION,
        Error::Conflict { .. } => DB_ERR_CONFLICT,
        Error::Poisoned => DB_ERR_POISONED,
        Error::TooLarge(_) => DB_ERR_TOO_LARGE,
        Error::Schema(_) | Error::DuplicateKey | Error::SchemaChanged { .. } => DB_ERR_SCHEMA,
    }
}

/// Records a failure the runtime detected itself (no OS error number).
fn refuse(code: i32) -> i32 {
    LAST_OS_ERROR.with(|last| last.set(0));
    code
}

/// Runs `body`, turning a panic into [`DB_ERR_OTHER`].
fn guarded<R: From<i32>>(body: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|_| R::from(refuse(DB_ERR_OTHER)))
}

/// Validates a directory path passed as an address and a length.
///
/// # Safety
///
/// `addr..addr + len` must be readable when `len` is at most
/// [`DB_MAX_PATH_BYTES`] and `addr` is not null.
unsafe fn path_from<'a>(addr: usize, len: usize) -> Result<&'a Path, i32> {
    if len == 0 || addr == 0 {
        return Err(refuse(DB_ERR_INVALID_ARGUMENT));
    }
    if len > DB_MAX_PATH_BYTES {
        return Err(refuse(DB_ERR_PATH_TOO_LONG));
    }
    // Safety: the caller's contract above.
    let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err(refuse(DB_ERR_PATH_NOT_UTF8));
    };
    if text.contains('\0') {
        return Err(refuse(DB_ERR_INVALID_ARGUMENT));
    }
    Ok(Path::new(text))
}

/// A key or value passed as an address and a length: at most
/// `codira_db::MAX_LEN` bytes, and not null unless empty. The length is
/// checked before the memory is touched.
///
/// # Safety
///
/// When the checks pass, `addr..addr + len` must be readable.
unsafe fn bytes_from<'a>(addr: usize, len: usize) -> Result<&'a [u8], i32> {
    if len > MAX_LEN {
        return Err(refuse(DB_ERR_TOO_LARGE));
    }
    if len == 0 {
        return Ok(&[]);
    }
    if addr == 0 {
        return Err(refuse(DB_ERR_INVALID_ARGUMENT));
    }
    // Safety: the caller's contract; `len <= MAX_LEN < isize::MAX`.
    Ok(unsafe { std::slice::from_raw_parts(addr as *const u8, len) })
}

fn db_for(handle: i64) -> Result<Arc<OpenDb>, i32> {
    DBS.lock()
        .get(handle)
        .cloned()
        .ok_or_else(|| refuse(DB_ERR_BAD_HANDLE))
}

fn txn_for(handle: i64) -> Result<SharedTxn, i32> {
    TXNS.lock()
        .get(handle)
        .cloned()
        .ok_or_else(|| refuse(DB_ERR_BAD_HANDLE))
}

/// Runs `body` on a live transaction. A transaction another thread just
/// committed or aborted (its slot is empty) is a bad handle.
fn with_txn<R: From<i32>>(handle: i64, body: impl FnOnce(&mut Txn<'static>) -> R) -> R {
    let shared = match txn_for(handle) {
        Ok(shared) => shared,
        Err(code) => return R::from(code),
    };
    let mut open = shared.lock();
    match open.txn.as_mut() {
        Some(txn) => body(txn),
        None => R::from(refuse(DB_ERR_BAD_HANDLE)),
    }
}

/// Opens (creating it if needed) the database in the directory at
/// `path_addr..path_addr + path_len` (UTF-8), running crash recovery.
/// `fsync` is `DB_FSYNC_ALWAYS`, `DB_FSYNC_ON_COMMIT` or `DB_FSYNC_NEVER`.
/// `checkpoint_wal_bytes > 0` checkpoints automatically after a commit leaves
/// the log at least that long; `0` checkpoints only on
/// [`codira_db_checkpoint`]. Returns a database handle (positive) or a
/// negative `DB_ERR_*` code.
pub extern "C" fn codira_db_open(
    path_addr: usize,
    path_len: usize,
    fsync: u32,
    checkpoint_wal_bytes: i64,
) -> i64 {
    guarded(|| {
        let fsync = match fsync {
            DB_FSYNC_ALWAYS => FsyncPolicy::Always,
            DB_FSYNC_ON_COMMIT => FsyncPolicy::OnCommit,
            DB_FSYNC_NEVER => FsyncPolicy::Never,
            _ => return i64::from(refuse(DB_ERR_INVALID_ARGUMENT)),
        };
        let checkpoint_wal_bytes = match checkpoint_wal_bytes {
            0 => None,
            bytes if bytes > 0 => Some(bytes as u64),
            _ => return i64::from(refuse(DB_ERR_INVALID_ARGUMENT)),
        };
        // Safety: the program passes a path it owns (`str_data`/`str_len`).
        let path = match unsafe { path_from(path_addr, path_len) } {
            Ok(path) => path,
            Err(code) => return i64::from(code),
        };
        // Creates the directory if it is missing.
        let storage = match FsStorage::open(path) {
            Ok(storage) => storage,
            Err(error) => return i64::from(error_code(&Error::Io(error))),
        };
        let canonical = match std::fs::canonicalize(path) {
            Ok(canonical) => canonical,
            Err(error) => return i64::from(error_code(&Error::Io(error))),
        };
        let claim = {
            let mut dirs = OPEN_DIRS.lock();
            if dirs.contains(&canonical) {
                return i64::from(refuse(DB_ERR_ALREADY_OPEN));
            }
            dirs.push(canonical.clone());
            DirClaim(canonical)
        };
        let options = Options {
            fsync,
            checkpoint_wal_bytes,
        };
        // On failure `claim` is dropped here, releasing the directory.
        let db = match Db::open(Arc::new(storage), options) {
            Ok(db) => db,
            Err(error) => return i64::from(error_code(&error)),
        };
        let open = Arc::new(OpenDb { db, _dir: claim });
        DBS.lock()
            .insert(open)
            .unwrap_or_else(|_| i64::from(refuse(DB_ERR_TOO_MANY_OPEN)))
    })
}

/// Starts a transaction that reads the snapshot of every transaction
/// committed so far plus its own writes. Returns a transaction handle
/// (positive) or a negative `DB_ERR_*` code. Finish it with
/// [`codira_db_commit`] or [`codira_db_abort`]; until then the database
/// cannot be closed.
pub extern "C" fn codira_db_begin(db: i64) -> i64 {
    guarded(|| {
        let open = match db_for(db) {
            Ok(open) => open,
            Err(code) => return i64::from(code),
        };
        let txn = open.db.begin();
        // Safety: see `OpenTxn`. `open` (an `Arc`) is stored next to the
        // transaction and outlives it, and the `Db` inside never moves.
        let txn: Txn<'static> = unsafe { std::mem::transmute::<Txn<'_>, Txn<'static>>(txn) };
        let shared = Arc::new(Mutex::new(OpenTxn {
            txn: Some(txn),
            _db: open,
        }));
        TXNS.lock()
            .insert(shared)
            .unwrap_or_else(|_| i64::from(refuse(DB_ERR_TOO_MANY_OPEN)))
    })
}

/// The length of the value of the key at `key_addr..key_addr + key_len` in
/// the transaction's view, or a negative `DB_ERR_*` code
/// ([`DB_ERR_NOT_FOUND`] if it has none).
pub extern "C" fn codira_db_value_len(txn: i64, key_addr: usize, key_len: usize) -> i64 {
    guarded(|| {
        // Safety: the program passes a key it owns.
        let key = match unsafe { bytes_from(key_addr, key_len) } {
            Ok(key) => key,
            Err(code) => return i64::from(code),
        };
        with_txn(txn, |txn| match txn.get(key) {
            Some(value) => value.len() as i64,
            None => i64::from(refuse(DB_ERR_NOT_FOUND)),
        })
    })
}

/// Copies the value of the key at `key_addr..key_addr + key_len`, in the
/// transaction's view, to `buf_addr..buf_addr + buf_len`. Returns the
/// value's length, or a negative `DB_ERR_*` code: [`DB_ERR_NOT_FOUND`] if
/// the key has no value, [`DB_ERR_BUFFER_TOO_SMALL`] (writing nothing) if
/// the value is longer than `buf_len`; [`codira_db_value_len`] gives the
/// length needed.
pub extern "C" fn codira_db_get(
    txn: i64,
    key_addr: usize,
    key_len: usize,
    buf_addr: usize,
    buf_len: usize,
) -> i64 {
    guarded(|| {
        if isize::try_from(buf_len).is_err() || (buf_addr == 0 && buf_len != 0) {
            return i64::from(refuse(DB_ERR_INVALID_ARGUMENT));
        }
        // Safety: the program passes a key it owns.
        let key = match unsafe { bytes_from(key_addr, key_len) } {
            Ok(key) => key,
            Err(code) => return i64::from(code),
        };
        with_txn(txn, |txn| {
            let Some(value) = txn.get(key) else {
                return i64::from(refuse(DB_ERR_NOT_FOUND));
            };
            if value.len() > buf_len {
                return i64::from(refuse(DB_ERR_BUFFER_TOO_SMALL));
            }
            if !value.is_empty() {
                // Safety: the program passes `buf_len >= value.len()`
                // writable bytes (module docs); `value` is our own copy, so
                // the ranges do not overlap.
                unsafe {
                    std::ptr::copy_nonoverlapping(value.as_ptr(), buf_addr as *mut u8, value.len());
                }
            }
            value.len() as i64
        })
    })
}

/// Sets the key at `key_addr..key_addr + key_len` to the value at
/// `val_addr..val_addr + val_len` when the transaction commits (both are
/// copied now). Returns `0` or a negative `DB_ERR_*` code.
pub extern "C" fn codira_db_put(
    txn: i64,
    key_addr: usize,
    key_len: usize,
    val_addr: usize,
    val_len: usize,
) -> i32 {
    guarded(|| {
        // Safety: the program passes a key and a value it owns.
        let (key, value) =
            match unsafe { (bytes_from(key_addr, key_len), bytes_from(val_addr, val_len)) } {
                (Ok(key), Ok(value)) => (key, value),
                (Err(code), _) | (_, Err(code)) => return code,
            };
        with_txn(txn, |txn| match txn.put(key, value) {
            Ok(()) => 0,
            Err(error) => error_code(&error),
        })
    })
}

/// Deletes the key at `key_addr..key_addr + key_len` when the transaction
/// commits. Deleting a missing key is not an error and still counts as a
/// write for conflict detection. Returns `0` or a negative `DB_ERR_*` code.
pub extern "C" fn codira_db_delete(txn: i64, key_addr: usize, key_len: usize) -> i32 {
    guarded(|| {
        // Safety: the program passes a key it owns.
        let key = match unsafe { bytes_from(key_addr, key_len) } {
            Ok(key) => key,
            Err(code) => return code,
        };
        with_txn(txn, |txn| match txn.delete(key) {
            Ok(()) => 0,
            Err(error) => error_code(&error),
        })
    })
}

/// Commits the transaction. Returns its commit timestamp (`>= 1`; for a
/// transaction that wrote nothing, the timestamp of its snapshot, `>= 0`),
/// or a negative `DB_ERR_*` code: [`DB_ERR_CONFLICT`] (nothing was written;
/// retry from `begin`), [`DB_ERR_IO`] (the outcome is unknown and the
/// database is poisoned), [`DB_ERR_POISONED`]. The handle is invalid
/// afterwards in every case. With `DB_FSYNC_ALWAYS` or `DB_FSYNC_ON_COMMIT`
/// a commit that returned a timestamp survives a crash.
pub extern "C" fn codira_db_commit(txn: i64) -> i64 {
    guarded(|| {
        let Some(shared) = TXNS.lock().remove(txn) else {
            return i64::from(refuse(DB_ERR_BAD_HANDLE));
        };
        let mut open = shared.lock();
        let Some(txn) = open.txn.take() else {
            return i64::from(refuse(DB_ERR_BAD_HANDLE));
        };
        // `open._db` is still held, so the borrowed `Db` is alive.
        match txn.commit() {
            Ok(ts) => i64::try_from(ts).unwrap_or_else(|_| i64::from(refuse(DB_ERR_OTHER))),
            Err(error) => i64::from(error_code(&error)),
        }
    })
}

/// Discards the transaction's writes. Returns `0` or [`DB_ERR_BAD_HANDLE`];
/// the handle is invalid afterwards.
pub extern "C" fn codira_db_abort(txn: i64) -> i32 {
    guarded(|| {
        let Some(shared) = TXNS.lock().remove(txn) else {
            return refuse(DB_ERR_BAD_HANDLE);
        };
        let mut open = shared.lock();
        match open.txn.take() {
            Some(txn) => {
                txn.abort();
                0
            }
            None => refuse(DB_ERR_BAD_HANDLE),
        }
    })
}

/// Writes a snapshot of the committed state and deletes the log files it
/// replaces; commits wait while it runs, readers do not. Returns `0` or a
/// negative `DB_ERR_*` code ([`DB_ERR_IO`] poisons the database).
pub extern "C" fn codira_db_checkpoint(db: i64) -> i32 {
    guarded(|| {
        let open = match db_for(db) {
            Ok(open) => open,
            Err(code) => return code,
        };
        match open.db.checkpoint() {
            Ok(()) => 0,
            Err(error) => error_code(&error),
        }
    })
}

/// Flushes the log (which matters for `DB_FSYNC_NEVER`) and closes the
/// database. Returns `0` or a negative `DB_ERR_*` code. [`DB_ERR_BUSY`]
/// means transactions of it are still open (or another thread is in a call
/// on it) and it stays open; after any other result the handle is invalid.
pub extern "C" fn codira_db_close(db: i64) -> i32 {
    guarded(|| {
        let open = {
            let mut dbs = DBS.lock();
            let Some(open) = dbs.get(db) else {
                return refuse(DB_ERR_BAD_HANDLE);
            };
            // Every other reference was cloned under this lock (by a lookup
            // or a `begin`), so none can appear once the entry is removed.
            if Arc::strong_count(open) > 1 {
                return refuse(DB_ERR_BUSY);
            }
            dbs.remove(db).expect("looked up above")
        };
        let Ok(OpenDb { db, _dir }) = Arc::try_unwrap(open) else {
            return refuse(DB_ERR_OTHER);
        };
        let result = db.close();
        // The directory may be opened again from here on.
        drop(_dir);
        match result {
            Ok(()) => 0,
            Err(error) => error_code(&error),
        }
    })
}

/// The raw OS error number (`errno` on Unix, `GetLastError` on Windows) of
/// this thread's last [`DB_ERR_IO`], or `0` if its last failed database call
/// failed for another reason.
pub extern "C" fn codira_db_last_os_error() -> i32 {
    LAST_OS_ERROR.with(Cell::get)
}

/// Returns the definitions of every function in this module, for the
/// runtime's dispatch table.
pub(crate) fn functions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(
            codira_db_open as extern "C" fn(usize, usize, u32, i64) -> i64,
            "codira_db_open",
        ),
        IntoFunctionDefinition::into(
            codira_db_begin as extern "C" fn(i64) -> i64,
            "codira_db_begin",
        ),
        IntoFunctionDefinition::into(
            codira_db_value_len as extern "C" fn(i64, usize, usize) -> i64,
            "codira_db_value_len",
        ),
        IntoFunctionDefinition::into(
            codira_db_get as extern "C" fn(i64, usize, usize, usize, usize) -> i64,
            "codira_db_get",
        ),
        IntoFunctionDefinition::into(
            codira_db_put as extern "C" fn(i64, usize, usize, usize, usize) -> i32,
            "codira_db_put",
        ),
        IntoFunctionDefinition::into(
            codira_db_delete as extern "C" fn(i64, usize, usize) -> i32,
            "codira_db_delete",
        ),
        IntoFunctionDefinition::into(
            codira_db_commit as extern "C" fn(i64) -> i64,
            "codira_db_commit",
        ),
        IntoFunctionDefinition::into(
            codira_db_abort as extern "C" fn(i64) -> i32,
            "codira_db_abort",
        ),
        IntoFunctionDefinition::into(
            codira_db_checkpoint as extern "C" fn(i64) -> i32,
            "codira_db_checkpoint",
        ),
        IntoFunctionDefinition::into(
            codira_db_close as extern "C" fn(i64) -> i32,
            "codira_db_close",
        ),
        IntoFunctionDefinition::into(
            codira_db_last_os_error as extern "C" fn() -> i32,
            "codira_db_last_os_error",
        ),
    ]
}

#[cfg(test)]
mod tests;
