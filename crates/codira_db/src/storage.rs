//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The `Storage` trait: the only way the engine touches persistent
//!   state. It is a flat directory of named files with explicit `sync`
//!   (file data) and `sync_dir` (directory entries) barriers.
//! - `FsStorage`, the implementation over a real directory with `std::fs`.
//!
//! The trait is deliberately small so that `fault::FaultStorage` can model
//! exactly what a crash may lose: unsynced file bytes and unsynced directory
//! operations.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// How strongly `Storage::sync` flushes a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncKind {
    /// Flush the file's data and the metadata needed to read it back
    /// (`fdatasync`, `File::sync_data`).
    Data,
    /// Flush the data and all metadata (`fsync`, `File::sync_all`).
    All,
}

/// A flat directory of named files.
///
/// Crash semantics the engine relies on, and that `FaultStorage` enforces:
/// - bytes written by `create` or `append` are durable only after `sync` on
///   that file returns `Ok`; before that a crash may keep any prefix of the
///   unsynced bytes (a torn write);
/// - `create`, `rename` and `remove` change the directory; those changes are
///   durable only after `sync_dir` returns `Ok`; before that a crash may keep
///   any subset of them (each `rename` itself is atomic);
/// - `sync` on a file does not make its directory entry durable.
pub trait Storage: Send + Sync {
    /// Names of the files in the directory, in no particular order.
    fn list(&self) -> io::Result<Vec<String>>;
    /// The whole contents of a file, or `None` if it does not exist.
    fn read(&self, name: &str) -> io::Result<Option<Vec<u8>>>;
    /// Creates `name` (replacing any existing file) with `data` as contents.
    fn create(&self, name: &str, data: &[u8]) -> io::Result<()>;
    /// Appends `data` to an existing file.
    fn append(&self, name: &str, data: &[u8]) -> io::Result<()>;
    /// Shortens an existing file to `len` bytes.
    fn truncate(&self, name: &str, len: u64) -> io::Result<()>;
    /// Makes the file's written bytes durable.
    fn sync(&self, name: &str, kind: SyncKind) -> io::Result<()>;
    /// Atomically renames `from` to `to`, replacing `to` if it exists.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    /// Removes a file. Removing a missing file is an error.
    fn remove(&self, name: &str) -> io::Result<()>;
    /// Makes earlier `create`, `rename` and `remove` calls durable.
    fn sync_dir(&self) -> io::Result<()>;
}

/// `Storage` over a real directory.
///
/// Open append handles are cached so that a commit costs one `write` (and,
/// depending on the fsync policy, one `fsync`), not an `open` as well.
pub struct FsStorage {
    dir: PathBuf,
    handles: Mutex<HashMap<String, File>>,
}

impl FsStorage {
    /// Uses `dir` as the database directory, creating it if needed.
    pub fn open(dir: impl AsRef<Path>) -> io::Result<FsStorage> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        Ok(FsStorage {
            dir,
            handles: Mutex::new(HashMap::new()),
        })
    }

    /// The directory this storage lives in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, name: &str) -> io::Result<PathBuf> {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid file name `{name}`"),
            ));
        }
        Ok(self.dir.join(name))
    }

    fn with_handle<T>(
        &self,
        name: &str,
        f: impl FnOnce(&mut File) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !handles.contains_key(name) {
            let file = OpenOptions::new().append(true).open(self.path(name)?)?;
            handles.insert(name.to_string(), file);
        }
        f(handles.get_mut(name).expect("handle inserted above"))
    }

    fn forget(&self, name: &str) {
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
    }
}

impl Storage for FsStorage {
    fn list(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    out.push(name.to_string());
                }
            }
        }
        Ok(out)
    }

    fn read(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.path(name)?) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn create(&self, name: &str, data: &[u8]) -> io::Result<()> {
        self.forget(name);
        let mut file = File::create(self.path(name)?)?;
        file.write_all(data)?;
        Ok(())
    }

    fn append(&self, name: &str, data: &[u8]) -> io::Result<()> {
        self.with_handle(name, |f| f.write_all(data))
    }

    fn truncate(&self, name: &str, len: u64) -> io::Result<()> {
        self.forget(name);
        let file = OpenOptions::new().write(true).open(self.path(name)?)?;
        file.set_len(len)
    }

    fn sync(&self, name: &str, kind: SyncKind) -> io::Result<()> {
        self.with_handle(name, |f| match kind {
            SyncKind::Data => f.sync_data(),
            SyncKind::All => f.sync_all(),
        })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.forget(from);
        self.forget(to);
        fs::rename(self.path(from)?, self.path(to)?)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.forget(name);
        fs::remove_file(self.path(name)?)
    }

    fn sync_dir(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            File::open(&self.dir)?.sync_all()
        }
        // Windows cannot open a directory for flushing with `std`; NTFS
        // journals metadata. This is a documented limit (README, "Limits").
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }
}
