//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - File services the runtime provides to Codira programs through
//!   `extern "C"` declarations (`std/fs/fs.code`): open, read, write, seek,
//!   fsync, close, size, remove, rename and directory sync. Every function is
//!   registered in each runtime's dispatch table.
//!
//! The conventions are those of `native_memory`: addresses and lengths cross
//! the boundary as `usize`, and every failure is a return value, never a
//! panic (these functions are called from generated code, where unwinding is
//! not defined; each body also runs under `catch_unwind`, so a bug here
//! becomes [`FS_ERR_OTHER`] instead of an abort).
//!
//! * Operations that produce a number (`open`, `read`, `write`, `seek`,
//!   `size`) return an `i64`: the value when it is zero or more, a negative
//!   `FS_ERR_*` code otherwise.
//! * Operations that only succeed or fail return an `i32`: `0`, or a negative
//!   `FS_ERR_*` code.
//! * [`codira_fs_last_os_error`] returns the raw OS error number (`errno`,
//!   `GetLastError`) of the calling thread's last failed call, for messages.
//!
//! File handles are not OS descriptors. They are an index into a
//! process-wide table plus a generation (as region handles are), so a closed
//! or forged handle is rejected with [`FS_ERR_BAD_HANDLE`] instead of
//! reaching some other file, and the same handles work on Windows, which has
//! no integer descriptors. The table holds each file behind an `Arc`: the lock
//! is held only to look the file up, never during the I/O, so one thread
//! blocked in a read does not stall another thread's I/O.
//!
//! Paths are UTF-8 bytes (`str_data`/`str_len` of a Codira string), at most
//! [`FS_MAX_PATH_BYTES`] long, non-empty and without NUL bytes. Anything else
//! is refused before the OS sees it.
//!
//! Buffers passed to `read`/`write` are raw memory like the `load_*`/`store_*`
//! intrinsics: the runtime checks that the length is representable and the
//! address is not null, and trusts the caller that `addr..addr + len` is
//! memory it may read (or write).

use std::{
    cell::Cell,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::Arc,
};

use parking_lot::Mutex;

use crate::{FunctionDefinition, IntoFunctionDefinition};

/// Open for reading.
pub const FS_READ: u32 = 1;
/// Open for writing.
pub const FS_WRITE: u32 = 2;
/// Every write goes to the end of the file (implies writing).
pub const FS_APPEND: u32 = 4;
/// Create the file if it does not exist (needs `FS_WRITE` or `FS_APPEND`).
pub const FS_CREATE: u32 = 8;
/// Truncate the file to length 0 when opening (needs `FS_WRITE`).
pub const FS_TRUNCATE: u32 = 16;
/// Create the file, failing with [`FS_ERR_ALREADY_EXISTS`] if it exists
/// (needs `FS_WRITE` or `FS_APPEND`; `FS_CREATE` and `FS_TRUNCATE` are
/// ignored with it).
pub const FS_CREATE_NEW: u32 = 32;
const FS_ALL_FLAGS: u32 = FS_READ | FS_WRITE | FS_APPEND | FS_CREATE | FS_TRUNCATE | FS_CREATE_NEW;

/// `whence` for [`codira_fs_seek`]: from the start of the file.
pub const FS_SEEK_START: u32 = 0;
/// `whence` for [`codira_fs_seek`]: from the current position.
pub const FS_SEEK_CURRENT: u32 = 1;
/// `whence` for [`codira_fs_seek`]: from the end of the file.
pub const FS_SEEK_END: u32 = 2;

/// The longest path, in bytes, a service accepts.
pub const FS_MAX_PATH_BYTES: usize = 4096;

/// An OS error without a more specific code below.
pub const FS_ERR_OTHER: i32 = -1;
/// No such file or directory.
pub const FS_ERR_NOT_FOUND: i32 = -2;
/// The OS refused access.
pub const FS_ERR_PERMISSION_DENIED: i32 = -3;
/// The file exists (`FS_CREATE_NEW`, or a rename target the OS will not
/// replace).
pub const FS_ERR_ALREADY_EXISTS: i32 = -4;
/// Invalid flags, `whence`, length, address, or an empty path or one with a
/// NUL byte.
pub const FS_ERR_INVALID_ARGUMENT: i32 = -5;
/// The handle is not an open file (never opened, closed, or forged).
pub const FS_ERR_BAD_HANDLE: i32 = -6;
/// The path is longer than [`FS_MAX_PATH_BYTES`].
pub const FS_ERR_PATH_TOO_LONG: i32 = -7;
/// The path is not valid UTF-8.
pub const FS_ERR_PATH_NOT_UTF8: i32 = -8;
/// Too many open files (the runtime's table, the process or the system).
pub const FS_ERR_TOO_MANY_OPEN: i32 = -9;
/// The path is a directory where a file was needed.
pub const FS_ERR_IS_A_DIRECTORY: i32 = -10;
/// A component of the path is not a directory.
pub const FS_ERR_NOT_A_DIRECTORY: i32 = -11;
/// The file system is full (or a quota is exhausted).
pub const FS_ERR_STORAGE_FULL: i32 = -12;
/// The operation is not supported on this platform or file system.
pub const FS_ERR_UNSUPPORTED: i32 = -13;
/// A directory to be replaced or removed is not empty.
pub const FS_ERR_DIRECTORY_NOT_EMPTY: i32 = -14;
/// A rename across file systems.
pub const FS_ERR_CROSSES_DEVICES: i32 = -15;
/// The file system is read-only.
pub const FS_ERR_READ_ONLY_FILESYSTEM: i32 = -16;
/// The file would exceed the largest size the file system allows.
pub const FS_ERR_FILE_TOO_LARGE: i32 = -17;
/// Reading a file not opened for reading, or writing one not opened for
/// writing.
pub const FS_ERR_WRONG_MODE: i32 = -18;

/// Bits of a file handle that hold the slot index (plus one, so that `0` is
/// never a valid handle).
const INDEX_BITS: u32 = 20;
const INDEX_MASK: u64 = (1 << INDEX_BITS) - 1;
/// Generations stay below this, so a handle is always a positive `i64`.
const GENERATION_MASK: u64 = (1 << (63 - INDEX_BITS)) - 1;

struct OpenFile {
    file: Arc<File>,
    readable: bool,
    writable: bool,
}

struct Slot {
    generation: u64,
    file: Option<OpenFile>,
}

struct Files {
    slots: Vec<Slot>,
    free: Vec<usize>,
}

static FILES: Mutex<Files> = Mutex::new(Files {
    slots: Vec::new(),
    free: Vec::new(),
});

impl Files {
    fn handle(index: usize, generation: u64) -> i64 {
        // `index < INDEX_MASK` and `generation <= GENERATION_MASK`, so this is
        // positive and fits.
        ((generation << INDEX_BITS) | (index as u64 + 1)) as i64
    }

    /// The slot index of a live handle.
    fn index(&self, handle: i64) -> Option<usize> {
        let handle = u64::try_from(handle).ok()?;
        let index = usize::try_from(handle & INDEX_MASK).ok()?.checked_sub(1)?;
        let slot = self.slots.get(index)?;
        (slot.generation == handle >> INDEX_BITS && slot.file.is_some()).then_some(index)
    }

    fn get(&self, handle: i64) -> Option<&OpenFile> {
        let index = self.index(handle)?;
        self.slots[index].file.as_ref()
    }

    fn insert(&mut self, file: OpenFile) -> Option<i64> {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index];
            slot.file = Some(file);
            Some(Self::handle(index, slot.generation))
        } else if (self.slots.len() as u64) < INDEX_MASK {
            self.slots.push(Slot {
                generation: 0,
                file: Some(file),
            });
            Some(Self::handle(self.slots.len() - 1, 0))
        } else {
            None
        }
    }

    fn remove(&mut self, handle: i64) -> Option<OpenFile> {
        let index = self.index(handle)?;
        let slot = &mut self.slots[index];
        let file = slot.file.take();
        slot.generation = slot.generation.wrapping_add(1) & GENERATION_MASK;
        self.free.push(index);
        file
    }
}

thread_local! {
    static LAST_OS_ERROR: Cell<i32> = const { Cell::new(0) };
}

/// Maps an I/O error to an `FS_ERR_*` code and records its OS error number.
fn error_code(error: &io::Error) -> i32 {
    LAST_OS_ERROR.with(|last| last.set(error.raw_os_error().unwrap_or(0)));
    #[cfg(unix)]
    if let Some(libc::EMFILE | libc::ENFILE) = error.raw_os_error() {
        return FS_ERR_TOO_MANY_OPEN;
    }
    #[cfg(unix)]
    if let Some(libc::ENAMETOOLONG) = error.raw_os_error() {
        return FS_ERR_PATH_TOO_LONG;
    }
    #[cfg(windows)]
    // ERROR_TOO_MANY_OPEN_FILES
    if let Some(4) = error.raw_os_error() {
        return FS_ERR_TOO_MANY_OPEN;
    }
    match error.kind() {
        io::ErrorKind::NotFound => FS_ERR_NOT_FOUND,
        io::ErrorKind::PermissionDenied => FS_ERR_PERMISSION_DENIED,
        io::ErrorKind::AlreadyExists => FS_ERR_ALREADY_EXISTS,
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidFilename => FS_ERR_INVALID_ARGUMENT,
        io::ErrorKind::IsADirectory => FS_ERR_IS_A_DIRECTORY,
        io::ErrorKind::NotADirectory => FS_ERR_NOT_A_DIRECTORY,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => FS_ERR_STORAGE_FULL,
        io::ErrorKind::Unsupported => FS_ERR_UNSUPPORTED,
        io::ErrorKind::DirectoryNotEmpty => FS_ERR_DIRECTORY_NOT_EMPTY,
        io::ErrorKind::CrossesDevices => FS_ERR_CROSSES_DEVICES,
        io::ErrorKind::ReadOnlyFilesystem => FS_ERR_READ_ONLY_FILESYSTEM,
        io::ErrorKind::FileTooLarge => FS_ERR_FILE_TOO_LARGE,
        _ => FS_ERR_OTHER,
    }
}

/// Records a failure the runtime detected itself (no OS error number).
fn refuse(code: i32) -> i32 {
    LAST_OS_ERROR.with(|last| last.set(0));
    code
}

/// Runs `body`, turning a panic into [`FS_ERR_OTHER`].
fn guarded<R: From<i32>>(body: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|_| R::from(refuse(FS_ERR_OTHER)))
}

/// Validates a path passed as an address and a length.
///
/// # Safety
///
/// `addr..addr + len` must be readable when `len` is at most
/// [`FS_MAX_PATH_BYTES`] and `addr` is not null.
unsafe fn path_from<'a>(addr: usize, len: usize) -> Result<&'a Path, i32> {
    if len == 0 || addr == 0 {
        return Err(refuse(FS_ERR_INVALID_ARGUMENT));
    }
    if len > FS_MAX_PATH_BYTES {
        return Err(refuse(FS_ERR_PATH_TOO_LONG));
    }
    // Safety: the caller's contract above.
    let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err(refuse(FS_ERR_PATH_NOT_UTF8));
    };
    if text.contains('\0') {
        return Err(refuse(FS_ERR_INVALID_ARGUMENT));
    }
    Ok(Path::new(text))
}

/// Looks up an open file, checking the mode it was opened with.
fn file_for(handle: i64, need_read: bool, need_write: bool) -> Result<Arc<File>, i32> {
    let files = FILES.lock();
    let Some(open) = files.get(handle) else {
        return Err(refuse(FS_ERR_BAD_HANDLE));
    };
    if (need_read && !open.readable) || (need_write && !open.writable) {
        return Err(refuse(FS_ERR_WRONG_MODE));
    }
    Ok(Arc::clone(&open.file))
}

/// Runs a system call, retrying it when the OS interrupted it with a
/// signal. The call may block, so it runs parked for the collector and, in a
/// task, releases its worker (`tasks::blocking_io`): a task reading a slow
/// file holds up neither a collection nor the other tasks. The buffers it
/// reads or writes are byte arrays the caller's frame keeps alive; the
/// collector does not move objects or read byte arrays' contents.
fn retry<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    crate::tasks::blocking_io(|| loop {
        match op() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    })
}

/// Checks a buffer passed to `read`/`write`: the length must fit an `i64`
/// result and the address must not be null unless the length is 0.
fn check_buffer(addr: usize, len: usize) -> Result<(), i32> {
    if i64::try_from(len).is_err() || isize::try_from(len).is_err() || (addr == 0 && len != 0) {
        return Err(refuse(FS_ERR_INVALID_ARGUMENT));
    }
    Ok(())
}

/// Opens the file at the path `path_addr..path_addr + path_len` (UTF-8) with
/// `flags` (`FS_READ`, `FS_WRITE`, `FS_APPEND`, `FS_CREATE`, `FS_TRUNCATE`,
/// `FS_CREATE_NEW`; at least one of read, write or append). Returns a file
/// handle (positive) or a negative `FS_ERR_*` code.
///
/// The file is not inherited by child processes (`O_CLOEXEC` on Unix, as
/// every Rust `File`). New files get mode `0o666` minus the umask on Unix.
pub extern "C" fn codira_fs_open(path_addr: usize, path_len: usize, flags: u32) -> i64 {
    guarded(|| {
        if flags & !FS_ALL_FLAGS != 0 || flags & (FS_READ | FS_WRITE | FS_APPEND) == 0 {
            return i64::from(refuse(FS_ERR_INVALID_ARGUMENT));
        }
        let writable = flags & (FS_WRITE | FS_APPEND) != 0;
        if !writable && flags & (FS_CREATE | FS_CREATE_NEW | FS_TRUNCATE) != 0 {
            return i64::from(refuse(FS_ERR_INVALID_ARGUMENT));
        }
        if flags & FS_TRUNCATE != 0 && flags & FS_APPEND != 0 {
            return i64::from(refuse(FS_ERR_INVALID_ARGUMENT));
        }
        // Safety: the program passes a path it owns (`str_data`/`str_len`).
        let path = match unsafe { path_from(path_addr, path_len) } {
            Ok(path) => path,
            Err(code) => return i64::from(code),
        };
        let mut options = OpenOptions::new();
        options
            .read(flags & FS_READ != 0)
            .write(flags & FS_WRITE != 0)
            .append(flags & FS_APPEND != 0)
            .truncate(flags & FS_TRUNCATE != 0);
        if flags & FS_CREATE_NEW != 0 {
            options.create_new(true);
        } else {
            options.create(flags & FS_CREATE != 0);
        }
        let file = match retry(|| options.open(path)) {
            Ok(file) => file,
            Err(error) => return i64::from(error_code(&error)),
        };
        // Opening a directory read-only succeeds on Unix; nothing a file
        // handle offers works on one, so refuse it here.
        if file.metadata().is_ok_and(|metadata| metadata.is_dir()) {
            return i64::from(refuse(FS_ERR_IS_A_DIRECTORY));
        }
        let open = OpenFile {
            file: Arc::new(file),
            readable: flags & FS_READ != 0,
            writable,
        };
        FILES
            .lock()
            .insert(open)
            .unwrap_or_else(|| i64::from(refuse(FS_ERR_TOO_MANY_OPEN)))
    })
}

/// Reads at most `len` bytes at the file's position into `buf_addr`.
/// Returns the number of bytes read (`0` at the end of the file; fewer than
/// `len` is not an error) or a negative `FS_ERR_*` code.
pub extern "C" fn codira_fs_read(handle: i64, buf_addr: usize, len: usize) -> i64 {
    guarded(|| {
        if let Err(code) = check_buffer(buf_addr, len) {
            return i64::from(code);
        }
        let file = match file_for(handle, true, false) {
            Ok(file) => file,
            Err(code) => return i64::from(code),
        };
        if len == 0 {
            return 0;
        }
        // Safety: the program passes memory it may write (module docs); the
        // length was checked against `isize::MAX`.
        let buf = unsafe { std::slice::from_raw_parts_mut(buf_addr as *mut u8, len) };
        match retry(|| (&*file).read(buf)) {
            Ok(read) => read as i64,
            Err(error) => i64::from(error_code(&error)),
        }
    })
}

/// Writes at most `len` bytes from `buf_addr` at the file's position (at its
/// end with `FS_APPEND`). Returns the number of bytes written (fewer than
/// `len` is not an error; call again with the rest) or a negative `FS_ERR_*`
/// code. Written bytes may still be in OS caches: [`codira_fs_fsync`] makes
/// them durable.
pub extern "C" fn codira_fs_write(handle: i64, buf_addr: usize, len: usize) -> i64 {
    guarded(|| {
        if let Err(code) = check_buffer(buf_addr, len) {
            return i64::from(code);
        }
        let file = match file_for(handle, false, true) {
            Ok(file) => file,
            Err(code) => return i64::from(code),
        };
        if len == 0 {
            return 0;
        }
        // Safety: the program passes memory it may read (module docs); the
        // length was checked against `isize::MAX`.
        let buf = unsafe { std::slice::from_raw_parts(buf_addr as *const u8, len) };
        match retry(|| (&*file).write(buf)) {
            Ok(written) => written as i64,
            Err(error) => i64::from(error_code(&error)),
        }
    })
}

/// Moves the file's position to `offset` bytes from the start
/// (`FS_SEEK_START`, `offset` must not be negative), the current position
/// (`FS_SEEK_CURRENT`) or the end (`FS_SEEK_END`). Returns the new position
/// or a negative `FS_ERR_*` code. Seeking past the end is allowed; a write
/// there leaves a hole that reads as zeros.
pub extern "C" fn codira_fs_seek(handle: i64, offset: i64, whence: u32) -> i64 {
    guarded(|| {
        let target = match whence {
            FS_SEEK_START => match u64::try_from(offset) {
                Ok(offset) => SeekFrom::Start(offset),
                Err(_) => return i64::from(refuse(FS_ERR_INVALID_ARGUMENT)),
            },
            FS_SEEK_CURRENT => SeekFrom::Current(offset),
            FS_SEEK_END => SeekFrom::End(offset),
            _ => return i64::from(refuse(FS_ERR_INVALID_ARGUMENT)),
        };
        let file = match file_for(handle, false, false) {
            Ok(file) => file,
            Err(code) => return i64::from(code),
        };
        match retry(|| (&*file).seek(target)) {
            Ok(position) => {
                i64::try_from(position).unwrap_or_else(|_| i64::from(refuse(FS_ERR_FILE_TOO_LARGE)))
            }
            Err(error) => i64::from(error_code(&error)),
        }
    })
}

/// Flushes the file's data and metadata to the storage device. Returns `0`
/// or a negative `FS_ERR_*` code.
///
/// What "flushed" means depends on the OS: `fsync(2)` on Linux
/// and the BSDs; `fcntl(F_FULLFSYNC)` on Apple platforms, which also asks the
/// drive to empty its write cache (falling back to `fsync`, which does not,
/// where the file system refuses `F_FULLFSYNC`); `FlushFileBuffers` on
/// Windows. A new file's directory entry is durable only after
/// [`codira_fs_sync_dir`] on its directory.
pub extern "C" fn codira_fs_fsync(handle: i64) -> i32 {
    guarded(|| {
        let file = match file_for(handle, false, false) {
            Ok(file) => file,
            Err(code) => return code,
        };
        match retry(|| full_sync(&file)) {
            Ok(()) => 0,
            Err(error) => error_code(&error),
        }
    })
}

#[cfg(target_vendor = "apple")]
fn full_sync(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // Safety: a valid descriptor and a command that takes no argument.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } != -1 {
        return Ok(());
    }
    file.sync_all()
}

#[cfg(not(target_vendor = "apple"))]
fn full_sync(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Closes a file handle. Returns `0` or a negative `FS_ERR_*` code; the
/// handle is invalid afterwards either way.
///
/// On Unix the error `close(2)` reports is returned (some network file
/// systems report write errors only there) when no other thread is in the
/// middle of an operation on the same handle; otherwise the file is closed
/// when that operation finishes and its close error is lost. A program that
/// needs to know its data is safe calls [`codira_fs_fsync`] first.
pub extern "C" fn codira_fs_close(handle: i64) -> i32 {
    guarded(|| {
        let Some(open) = FILES.lock().remove(handle) else {
            return refuse(FS_ERR_BAD_HANDLE);
        };
        match Arc::try_unwrap(open.file) {
            Ok(file) => close_reporting(file),
            // Another thread holds it during an operation; it closes when that
            // finishes.
            Err(_shared) => 0,
        }
    })
}

#[cfg(unix)]
fn close_reporting(file: File) -> i32 {
    use std::os::fd::IntoRawFd;
    let fd = file.into_raw_fd();
    // Safety: `fd` was just released by its only owner. `close` is not
    // retried on `EINTR`: on Linux the descriptor is closed regardless.
    if unsafe { libc::close(fd) } == 0 {
        0
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            0
        } else {
            error_code(&error)
        }
    }
}

#[cfg(not(unix))]
fn close_reporting(file: File) -> i32 {
    drop(file);
    0
}

/// Returns the file's size in bytes or a negative `FS_ERR_*` code.
pub extern "C" fn codira_fs_size(handle: i64) -> i64 {
    guarded(|| {
        let file = match file_for(handle, false, false) {
            Ok(file) => file,
            Err(code) => return i64::from(code),
        };
        match file.metadata() {
            Ok(metadata) => i64::try_from(metadata.len())
                .unwrap_or_else(|_| i64::from(refuse(FS_ERR_FILE_TOO_LARGE))),
            Err(error) => i64::from(error_code(&error)),
        }
    })
}

/// Removes the file at a path (not a directory). Returns `0` or a negative
/// `FS_ERR_*` code. Open handles to it stay usable (on Windows the removal
/// may fail while it is open).
pub extern "C" fn codira_fs_remove(path_addr: usize, path_len: usize) -> i32 {
    guarded(|| {
        // Safety: the program passes a path it owns.
        let path = match unsafe { path_from(path_addr, path_len) } {
            Ok(path) => path,
            Err(code) => return code,
        };
        match std::fs::remove_file(path) {
            Ok(()) => 0,
            Err(error) => error_code(&error),
        }
    })
}

/// Renames `from` to `to`, replacing `to` if it is a file. Returns `0` or a
/// negative `FS_ERR_*` code. Within one file system the replacement is atomic
/// on POSIX systems (`rename(2)`); on Windows it is `MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING`. The new name is durable only after
/// [`codira_fs_sync_dir`] on the directory (both directories if they differ).
pub extern "C" fn codira_fs_rename(
    from_addr: usize,
    from_len: usize,
    to_addr: usize,
    to_len: usize,
) -> i32 {
    guarded(|| {
        // Safety: the program passes paths it owns.
        let from = match unsafe { path_from(from_addr, from_len) } {
            Ok(path) => path,
            Err(code) => return code,
        };
        // Safety: as above.
        let to = match unsafe { path_from(to_addr, to_len) } {
            Ok(path) => path,
            Err(code) => return code,
        };
        match std::fs::rename(from, to) {
            Ok(()) => 0,
            Err(error) => error_code(&error),
        }
    })
}

/// Makes the entries of the directory at a path (created, removed and
/// renamed files) durable: it opens the directory and `fsync`s it. Returns
/// `0` or a negative `FS_ERR_*` code; on Windows, which has no equivalent
/// for directories, [`FS_ERR_UNSUPPORTED`].
pub extern "C" fn codira_fs_sync_dir(path_addr: usize, path_len: usize) -> i32 {
    guarded(|| {
        // Safety: the program passes a path it owns.
        let path = match unsafe { path_from(path_addr, path_len) } {
            Ok(path) => path,
            Err(code) => return code,
        };
        sync_dir(path)
    })
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> i32 {
    let dir = match retry(|| File::open(path)) {
        Ok(dir) => dir,
        Err(error) => return error_code(&error),
    };
    match dir.metadata() {
        Ok(metadata) if !metadata.is_dir() => return refuse(FS_ERR_NOT_A_DIRECTORY),
        Ok(_) => {}
        Err(error) => return error_code(&error),
    }
    match retry(|| full_sync(&dir)) {
        Ok(()) => 0,
        Err(error) => error_code(&error),
    }
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> i32 {
    refuse(FS_ERR_UNSUPPORTED)
}

/// The raw OS error number (`errno` on Unix, `GetLastError` on Windows) of
/// this thread's last failed file service call, or `0` if the runtime
/// refused that call itself (bad handle, flags or path).
pub extern "C" fn codira_fs_last_os_error() -> i32 {
    LAST_OS_ERROR.with(Cell::get)
}

/// Returns the definitions of every function in this module, for the
/// runtime's dispatch table.
pub(crate) fn functions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(
            codira_fs_open as extern "C" fn(usize, usize, u32) -> i64,
            "codira_fs_open",
        ),
        IntoFunctionDefinition::into(
            codira_fs_read as extern "C" fn(i64, usize, usize) -> i64,
            "codira_fs_read",
        ),
        IntoFunctionDefinition::into(
            codira_fs_write as extern "C" fn(i64, usize, usize) -> i64,
            "codira_fs_write",
        ),
        IntoFunctionDefinition::into(
            codira_fs_seek as extern "C" fn(i64, i64, u32) -> i64,
            "codira_fs_seek",
        ),
        IntoFunctionDefinition::into(
            codira_fs_fsync as extern "C" fn(i64) -> i32,
            "codira_fs_fsync",
        ),
        IntoFunctionDefinition::into(
            codira_fs_close as extern "C" fn(i64) -> i32,
            "codira_fs_close",
        ),
        IntoFunctionDefinition::into(
            codira_fs_size as extern "C" fn(i64) -> i64,
            "codira_fs_size",
        ),
        IntoFunctionDefinition::into(
            codira_fs_remove as extern "C" fn(usize, usize) -> i32,
            "codira_fs_remove",
        ),
        IntoFunctionDefinition::into(
            codira_fs_rename as extern "C" fn(usize, usize, usize, usize) -> i32,
            "codira_fs_rename",
        ),
        IntoFunctionDefinition::into(
            codira_fs_sync_dir as extern "C" fn(usize, usize) -> i32,
            "codira_fs_sync_dir",
        ),
        IntoFunctionDefinition::into(
            codira_fs_last_os_error as extern "C" fn() -> i32,
            "codira_fs_last_os_error",
        ),
    ]
}

#[cfg(test)]
mod tests;
