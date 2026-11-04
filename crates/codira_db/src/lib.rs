//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - `codira_db`, the native storage engine intended to back `std.db`
//!   (experimental, Tier B candidate; see `lab/rfcs/0002-codira-db.md`).
//! - A key-value store over byte strings with a checksummed write-ahead
//!   log, explicit fsync policy and crash recovery ([`Db`]).
//! - Snapshot-isolation MVCC transactions with first-committer-wins
//!   conflict detection and ordered range scans ([`Txn`]).
//! - Atomic checkpoints and log truncation ([`Db::checkpoint`]).
//! - A storage abstraction with a fault-injecting implementation for crash
//!   tests ([`Storage`], [`fault::FaultStorage`]).
//!
//! # Guarantee
//!
//! Under [`FsyncPolicy::Always`] or [`FsyncPolicy::OnCommit`], and under
//! the crash model of [`Storage`] (unsynced bytes and unsynced directory
//! operations may be lost or torn, synced ones are not), the state after
//! recovery is exactly the effect of the transactions committed before the
//! crash, in commit order: every transaction whose `commit` returned `Ok`,
//! plus possibly the one transaction whose commit was in progress, and no
//! part of any other transaction. The crash tests check this at every
//! mutating storage operation of a workload; that is testing, not proof.
//!
//! Isolation is snapshot isolation, which is weaker than serializable:
//! write skew is possible.
//!
//! ```
//! use codira_db::{Db, FaultStorage, Options};
//! use std::sync::Arc;
//!
//! let db = Db::open(Arc::new(FaultStorage::new()), Options::default()).unwrap();
//! let mut t = db.begin();
//! t.put(b"greeting", b"hello").unwrap();
//! t.commit().unwrap();
//! assert_eq!(db.begin().get(b"greeting").as_deref(), Some(&b"hello"[..]));
//! ```

mod codec;
mod db;
mod error;
pub mod fault;
mod snapshot;
mod storage;
mod wal;

pub use codec::MAX_LEN;
pub use db::{Db, FsyncPolicy, Options, RecoveryReport, Txn};
pub use error::{Error, Result};
pub use fault::FaultStorage;
pub use storage::{FsStorage, Storage, SyncKind};
