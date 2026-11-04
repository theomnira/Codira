//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Unit tests of the runtime's file services: the round trip through every
//!   operation, and an error path for each (bad and stale handles, flags,
//!   modes, lengths, paths that are too long, not UTF-8, empty or contain NUL,
//!   missing files, directories).

use super::*;

/// A path as the `(addr, len)` pair a Codira program passes.
fn raw(path: &str) -> (usize, usize) {
    (path.as_ptr() as usize, path.len())
}

fn open(path: &str, flags: u32) -> i64 {
    let (addr, len) = raw(path);
    codira_fs_open(addr, len, flags)
}

fn write_all(handle: i64, bytes: &[u8]) {
    let mut done = 0;
    while done < bytes.len() {
        let rest = &bytes[done..];
        let written = codira_fs_write(handle, rest.as_ptr() as usize, rest.len());
        assert!(written > 0, "write failed: {written}");
        done += written as usize;
    }
}

fn temp_path(dir: &tempfile::TempDir, name: &str) -> String {
    dir.path().join(name).to_str().unwrap().to_owned()
}

#[test]
fn a_file_round_trips_through_every_operation() {
    let dir = tempfile::tempdir().unwrap();
    let path = temp_path(&dir, "data.bin");
    let handle = open(&path, FS_READ | FS_WRITE | FS_CREATE | FS_TRUNCATE);
    assert!(handle > 0, "open failed: {handle}");

    write_all(handle, b"hello, file services");
    assert_eq!(codira_fs_fsync(handle), 0);
    assert_eq!(codira_fs_size(handle), 20);

    assert_eq!(codira_fs_seek(handle, 7, FS_SEEK_START), 7);
    let mut buf = [0u8; 64];
    let read = codira_fs_read(handle, buf.as_mut_ptr() as usize, buf.len());
    assert_eq!(read, 13);
    assert_eq!(&buf[..13], b"file services");
    // At the end of the file a read returns 0.
    assert_eq!(
        codira_fs_read(handle, buf.as_mut_ptr() as usize, buf.len()),
        0
    );
    // Zero-length reads and writes succeed without touching the buffer.
    assert_eq!(codira_fs_read(handle, 0, 0), 0);
    assert_eq!(codira_fs_write(handle, 0, 0), 0);

    assert_eq!(codira_fs_seek(handle, -8, FS_SEEK_END), 12);
    assert_eq!(codira_fs_seek(handle, 2, FS_SEEK_CURRENT), 14);
    assert_eq!(codira_fs_close(handle), 0);

    assert_eq!(std::fs::read(&path).unwrap(), b"hello, file services");

    let renamed = temp_path(&dir, "renamed.bin");
    let (from_addr, from_len) = raw(&path);
    let (to_addr, to_len) = raw(&renamed);
    assert_eq!(codira_fs_rename(from_addr, from_len, to_addr, to_len), 0);
    assert!(!Path::new(&path).exists());
    let dir_path = dir.path().to_str().unwrap();
    let (dir_addr, dir_len) = raw(dir_path);
    #[cfg(unix)]
    assert_eq!(codira_fs_sync_dir(dir_addr, dir_len), 0);
    #[cfg(not(unix))]
    assert_eq!(codira_fs_sync_dir(dir_addr, dir_len), FS_ERR_UNSUPPORTED);

    let (addr, len) = raw(&renamed);
    assert_eq!(codira_fs_remove(addr, len), 0);
    assert!(!Path::new(&renamed).exists());
}

#[test]
fn append_writes_at_the_end_and_create_new_refuses_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = temp_path(&dir, "log.txt");
    std::fs::write(&path, b"one ").unwrap();

    let handle = open(&path, FS_APPEND);
    assert!(handle > 0);
    write_all(handle, b"two");
    assert_eq!(codira_fs_close(handle), 0);
    assert_eq!(std::fs::read(&path).unwrap(), b"one two");

    assert_eq!(
        open(&path, FS_WRITE | FS_CREATE_NEW),
        i64::from(FS_ERR_ALREADY_EXISTS)
    );
    #[cfg(unix)]
    assert_eq!(codira_fs_last_os_error(), libc::EEXIST);

    let fresh = temp_path(&dir, "fresh.txt");
    let handle = open(&fresh, FS_WRITE | FS_CREATE_NEW);
    assert!(handle > 0);
    assert_eq!(codira_fs_close(handle), 0);
    assert_eq!(std::fs::metadata(&fresh).unwrap().len(), 0);
}

#[test]
fn missing_files_are_reported_as_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let missing = temp_path(&dir, "missing.txt");
    assert_eq!(open(&missing, FS_READ), i64::from(FS_ERR_NOT_FOUND));
    #[cfg(unix)]
    assert_eq!(codira_fs_last_os_error(), libc::ENOENT);

    let (addr, len) = raw(&missing);
    assert_eq!(codira_fs_remove(addr, len), FS_ERR_NOT_FOUND);
    let other = temp_path(&dir, "other.txt");
    let (to_addr, to_len) = raw(&other);
    assert_eq!(
        codira_fs_rename(addr, len, to_addr, to_len),
        FS_ERR_NOT_FOUND
    );
    assert_eq!(codira_fs_sync_dir(addr, len), {
        if cfg!(unix) {
            FS_ERR_NOT_FOUND
        } else {
            FS_ERR_UNSUPPORTED
        }
    });

    // A missing parent directory, when creating.
    let nested = temp_path(&dir, "no/such/dir/file.txt");
    assert_eq!(
        open(&nested, FS_WRITE | FS_CREATE),
        i64::from(FS_ERR_NOT_FOUND)
    );
}

#[test]
fn directories_are_not_files() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_str().unwrap();
    let result = open(dir_path, FS_READ);
    // Unix opens the directory and the runtime refuses it; Windows refuses
    // to open it at all.
    assert!(
        result == i64::from(FS_ERR_IS_A_DIRECTORY) || result == i64::from(FS_ERR_PERMISSION_DENIED),
        "{result}"
    );
    let (addr, len) = raw(dir_path);
    assert!(codira_fs_remove(addr, len) < 0);

    #[cfg(unix)]
    {
        let file = temp_path(&dir, "plain.txt");
        std::fs::write(&file, b"x").unwrap();
        let (addr, len) = raw(&file);
        assert_eq!(codira_fs_sync_dir(addr, len), FS_ERR_NOT_A_DIRECTORY);
        // A file used as a directory component.
        let below = format!("{file}/child");
        assert_eq!(open(&below, FS_READ), i64::from(FS_ERR_NOT_A_DIRECTORY));
    }
}

#[test]
fn bad_closed_and_stale_handles_are_refused_by_every_operation() {
    let dir = tempfile::tempdir().unwrap();
    let path = temp_path(&dir, "h.txt");
    let first = open(&path, FS_READ | FS_WRITE | FS_CREATE);
    assert!(first > 0);
    assert_eq!(codira_fs_close(first), 0);
    // Reopening reuses the slot with a new generation: the old handle must
    // not reach the new file.
    let second = open(&path, FS_READ | FS_WRITE);
    assert!(second > 0);
    assert_ne!(first, second);

    let mut buf = [0u8; 4];
    let addr = buf.as_mut_ptr() as usize;
    // `second` with another generation: the same slot, never handed out.
    for handle in [first, 0, -1, i64::MIN, i64::MAX, second + (1 << 40)] {
        let bad = i64::from(FS_ERR_BAD_HANDLE);
        assert_eq!(codira_fs_read(handle, addr, 4), bad, "{handle}");
        assert_eq!(codira_fs_write(handle, addr, 4), bad, "{handle}");
        assert_eq!(codira_fs_seek(handle, 0, FS_SEEK_START), bad, "{handle}");
        assert_eq!(codira_fs_size(handle), bad, "{handle}");
        assert_eq!(codira_fs_fsync(handle), FS_ERR_BAD_HANDLE, "{handle}");
        assert_eq!(codira_fs_close(handle), FS_ERR_BAD_HANDLE, "{handle}");
        assert_eq!(codira_fs_last_os_error(), 0);
    }
    assert_eq!(codira_fs_close(second), 0);
    assert_eq!(codira_fs_close(second), FS_ERR_BAD_HANDLE);
}

#[test]
fn flags_modes_and_lengths_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let path = temp_path(&dir, "f.txt");
    std::fs::write(&path, b"abc").unwrap();
    let invalid = i64::from(FS_ERR_INVALID_ARGUMENT);

    // No access mode, unknown bits, creation or truncation without writing,
    // truncation with appending.
    for flags in [
        0,
        FS_CREATE,
        64,
        FS_READ | 1 << 31,
        FS_READ | FS_CREATE,
        FS_READ | FS_TRUNCATE,
        FS_READ | FS_CREATE_NEW,
        FS_APPEND | FS_TRUNCATE,
    ] {
        assert_eq!(open(&path, flags), invalid, "flags {flags:#b}");
    }

    let reader = open(&path, FS_READ);
    let writer = open(&path, FS_WRITE);
    assert!(reader > 0 && writer > 0);
    let mut buf = [0u8; 4];
    let addr = buf.as_mut_ptr() as usize;
    let wrong_mode = i64::from(FS_ERR_WRONG_MODE);
    assert_eq!(codira_fs_write(reader, addr, 4), wrong_mode);
    assert_eq!(codira_fs_read(writer, addr, 4), wrong_mode);

    // Lengths an `i64` result cannot express, and a null buffer.
    assert_eq!(codira_fs_read(reader, addr, usize::MAX), invalid);
    assert_eq!(codira_fs_write(writer, addr, usize::MAX), invalid);
    assert_eq!(
        codira_fs_read(reader, addr, isize::MAX as usize + 1),
        invalid
    );
    assert_eq!(codira_fs_read(reader, 0, 4), invalid);
    assert_eq!(codira_fs_write(writer, 0, 4), invalid);

    // Seeking: unknown `whence`, a negative absolute offset, before the start.
    assert_eq!(codira_fs_seek(reader, 0, 3), invalid);
    assert_eq!(codira_fs_seek(reader, -1, FS_SEEK_START), invalid);
    assert!(codira_fs_seek(reader, -10, FS_SEEK_END) < 0);
    // Past the end is allowed.
    assert_eq!(codira_fs_seek(reader, 100, FS_SEEK_START), 100);
    assert_eq!(codira_fs_read(reader, addr, 4), 0);

    assert_eq!(codira_fs_close(reader), 0);
    assert_eq!(codira_fs_close(writer), 0);
    assert_eq!(std::fs::read(&path).unwrap(), b"abc");
}

#[test]
fn paths_must_be_short_utf8_without_nul_and_not_empty() {
    let dir = tempfile::tempdir().unwrap();

    let long = format!(
        "{}/{}",
        dir.path().to_str().unwrap(),
        "x".repeat(FS_MAX_PATH_BYTES)
    );
    let (addr, len) = raw(&long);
    let too_long = FS_ERR_PATH_TOO_LONG;
    assert_eq!(codira_fs_open(addr, len, FS_READ), i64::from(too_long));
    assert_eq!(codira_fs_remove(addr, len), too_long);
    assert_eq!(codira_fs_sync_dir(addr, len), too_long);
    let ok = temp_path(&dir, "ok");
    let (ok_addr, ok_len) = raw(&ok);
    assert_eq!(codira_fs_rename(addr, len, ok_addr, ok_len), too_long);
    assert_eq!(codira_fs_rename(ok_addr, ok_len, addr, len), too_long);
    // Under the runtime's limit but over the OS's limit for one component.
    let component = temp_path(&dir, &"y".repeat(1000));
    #[cfg(unix)]
    assert_eq!(open(&component, FS_READ), i64::from(FS_ERR_PATH_TOO_LONG));
    #[cfg(not(unix))]
    assert!(open(&component, FS_READ) < 0);

    let not_utf8: &[u8] = b"/tmp/\xff\xfe.txt";
    let (addr, len) = (not_utf8.as_ptr() as usize, not_utf8.len());
    let bad_utf8 = FS_ERR_PATH_NOT_UTF8;
    assert_eq!(
        codira_fs_open(addr, len, FS_WRITE | FS_CREATE),
        i64::from(bad_utf8)
    );
    assert_eq!(codira_fs_remove(addr, len), bad_utf8);
    assert_eq!(codira_fs_rename(addr, len, ok_addr, ok_len), bad_utf8);
    assert_eq!(codira_fs_sync_dir(addr, len), bad_utf8);

    let invalid = FS_ERR_INVALID_ARGUMENT;
    let with_nul = temp_path(&dir, "a\0b");
    assert_eq!(open(&with_nul, FS_WRITE | FS_CREATE), i64::from(invalid));
    assert_eq!(codira_fs_open(ok_addr, 0, FS_READ), i64::from(invalid));
    assert_eq!(codira_fs_open(0, 5, FS_READ), i64::from(invalid));
    assert_eq!(codira_fs_remove(0, 0), invalid);
    assert_eq!(codira_fs_rename(0, 0, ok_addr, ok_len), invalid);
    assert_eq!(codira_fs_last_os_error(), 0);
    // Nothing was created by any of the refused calls.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn rename_replaces_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let from = temp_path(&dir, "new");
    let to = temp_path(&dir, "old");
    std::fs::write(&from, b"new contents").unwrap();
    std::fs::write(&to, b"old").unwrap();
    let (from_addr, from_len) = raw(&from);
    let (to_addr, to_len) = raw(&to);
    assert_eq!(codira_fs_rename(from_addr, from_len, to_addr, to_len), 0);
    assert_eq!(std::fs::read(&to).unwrap(), b"new contents");
    assert!(!Path::new(&from).exists());
}

#[test]
fn handles_work_across_threads() {
    let dir = tempfile::tempdir().unwrap();
    let path = temp_path(&dir, "threads.bin");
    let handle = open(&path, FS_READ | FS_WRITE | FS_CREATE);
    assert!(handle > 0);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..100 {
                    write_all(handle, b"0123456789");
                }
            });
        }
    });
    assert_eq!(codira_fs_size(handle), 4000);
    assert_eq!(codira_fs_close(handle), 0);
}

#[test]
fn every_service_is_in_the_dispatch_table() {
    let names: Vec<String> = functions()
        .into_iter()
        .map(|definition| definition.prototype.name)
        .collect();
    for name in [
        "codira_fs_open",
        "codira_fs_read",
        "codira_fs_write",
        "codira_fs_seek",
        "codira_fs_fsync",
        "codira_fs_close",
        "codira_fs_size",
        "codira_fs_remove",
        "codira_fs_rename",
        "codira_fs_sync_dir",
        "codira_fs_last_os_error",
    ] {
        assert!(names.iter().any(|n| n == name), "{name} missing");
    }
}
