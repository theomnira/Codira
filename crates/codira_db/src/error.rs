//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The error type of the storage engine: I/O failures, detected
//!   corruption, write-write conflicts, a poisoned database, and schema
//!   errors from the table layer.

use std::{fmt, io};

/// Every fallible operation in this crate returns this error.
#[derive(Debug)]
pub enum Error {
    /// The underlying storage failed. If this came out of `commit` or
    /// `checkpoint`, the database is now poisoned (see [`Error::Poisoned`])
    /// and the outcome of the failed commit is unknown until the database is
    /// reopened: the commit record may or may not have reached the disk.
    Io(io::Error),
    /// A file failed validation during recovery: a checksum mismatch in the
    /// middle of a log, a bad magic number, a gap in the commit sequence, a
    /// snapshot that refers to a missing log. Recovery never skips such a
    /// record silently.
    Corruption {
        /// The file name inside the database directory.
        file: String,
        /// The byte offset of the bad record or field.
        offset: u64,
        /// What was wrong.
        reason: String,
    },
    /// A write-write conflict: another transaction committed a write to a key
    /// this transaction also wrote, after this transaction's snapshot was
    /// taken. The first committer wins; this transaction was aborted and may
    /// be retried from the start.
    Conflict {
        /// One of the conflicting keys.
        key: Vec<u8>,
    },
    /// An earlier write or fsync failed, so the in-memory state may no longer
    /// match the disk. Every later commit fails with this error; reopen the
    /// database to recover from the log. (Retrying an fsync after a failure
    /// is unsafe on Linux: the kernel may have dropped the dirty pages.)
    Poisoned,
    /// A key, value or transaction exceeds an encoding limit.
    TooLarge(String),
    /// A table-layer error: unknown table, type mismatch, bad migration, ...
    Schema(String),
    /// The table already holds a row with this primary key.
    DuplicateKey,
    /// The table's schema changed after this `Table` handle was opened; open
    /// it again inside the current transaction.
    SchemaChanged {
        /// The table name.
        table: String,
    },
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Corruption {
                file,
                offset,
                reason,
            } => write!(f, "corruption in `{file}` at byte {offset}: {reason}"),
            Error::Conflict { key } => {
                write!(
                    f,
                    "write-write conflict on key {key:?}; transaction aborted"
                )
            }
            Error::Poisoned => write!(
                f,
                "database is poisoned by an earlier I/O failure; reopen it to recover"
            ),
            Error::TooLarge(s) => write!(f, "too large: {s}"),
            Error::Schema(s) => write!(f, "schema error: {s}"),
            Error::DuplicateKey => write!(f, "a row with this primary key already exists"),
            Error::SchemaChanged { table } => write!(
                f,
                "the schema of table `{table}` changed; open the table again"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub(crate) fn corruption(file: &str, offset: u64, reason: impl Into<String>) -> Error {
    Error::Corruption {
        file: file.to_string(),
        offset,
        reason: reason.into(),
    }
}
