//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 1, 2026
//!
//! Functionality:
//! - Where a running build daemon advertises itself, and the secret that lets a
//!   client talk to it.
//!
//! # Why a file, and why a token
//!
//! The daemon listens on a loopback TCP port, because that is the one local
//! transport the standard library offers on every platform Codira supports.
//! But a loopback port is reachable by *every* user and every process on the
//! machine, and a daemon that compiles whatever manifest it is handed --
//! writing files as the user who started it -- must not take orders from
//! all of them.
//!
//! So the daemon generates a random token at startup and writes it, with its
//! port, into a state file only its own user can read. A request that does
//! not carry the token is refused. Reading the file is the proof of
//! identity: the operating system's file permissions do the authentication,
//! and the daemon never has to.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde_derive::{Deserialize, Serialize};

use crate::protocol::PROTOCOL_VERSION;

/// Overrides where the state file lives. Tests use it to keep their daemons
/// apart from each other and from a real one.
pub const STATE_DIR_ENV: &str = "CODIRA_DAEMON_DIR";

const STATE_FILE_NAME: &str = "daemon.json";

/// What a running daemon publishes about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: u32,

    /// Loopback port the daemon accepts connections on.
    pub port: u16,

    /// Secret that every request must carry.
    pub token: String,

    /// Identifies the compiler binary the daemon is running; see `build_id`.
    pub build_id: String,

    pub protocol: u32,
}

impl DaemonState {
    /// Describes a daemon listening on `port` in this process.
    pub fn for_this_process(port: u16) -> anyhow::Result<Self> {
        Ok(Self {
            pid: std::process::id(),
            port,
            token: generate_token()?,
            build_id: build_id(),
            protocol: PROTOCOL_VERSION,
        })
    }
}

/// The directory the state file lives in.
///
/// Per user on every platform, so two users on one machine each get their
/// own daemon and cannot read each other's token.
pub fn default_state_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = env::var_os(STATE_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }

    // `%LOCALAPPDATA%` is the per-user, non-roaming data directory; its
    // default ACL already denies other users.
    #[cfg(windows)]
    let base = env::var_os("LOCALAPPDATA").filter(|dir| !dir.is_empty());

    // `$XDG_RUNTIME_DIR` is the right home for a socket-like file: per
    // user, mode 0700, and cleared at logout. Not every system sets it.
    #[cfg(not(windows))]
    let base = env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .or_else(|| {
            env::var_os("HOME")
                .filter(|dir| !dir.is_empty())
                .map(|home| PathBuf::from(home).join(".cache").into_os_string())
        });

    let base = base.context(
        "could not determine a per-user directory for the build daemon; set CODIRA_DAEMON_DIR",
    )?;
    Ok(PathBuf::from(base).join("codira"))
}

/// Path of the state file inside `state_dir`.
pub fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join(STATE_FILE_NAME)
}

/// Publishes `state`, replacing whatever was there.
///
/// The file is written under a temporary name and renamed into place, so a
/// client never observes it half-written, and it is created readable by its
/// owner alone *before* the token goes into it.
pub fn write_state(state_dir: &Path, state: &DaemonState) -> anyhow::Result<()> {
    create_private_dir(state_dir)
        .with_context(|| format!("could not create '{}'", state_dir.display()))?;

    let final_path = state_path(state_dir);
    let temp_path = state_dir.join(format!("{STATE_FILE_NAME}.{}.tmp", std::process::id()));

    let contents = serde_json::to_vec(state).context("could not encode the daemon state")?;
    write_private_file(&temp_path, &contents)
        .with_context(|| format!("could not write '{}'", temp_path.display()))?;

    fs::rename(&temp_path, &final_path).map_err(|e| {
        let _ = fs::remove_file(&temp_path);
        anyhow::anyhow!("could not publish '{}': {e}", final_path.display())
    })
}

/// Reads the state file. `None` means no daemon is advertised -- the file is
/// missing, or it is not something this version understands.
pub fn read_state(state_dir: &Path) -> Option<DaemonState> {
    let contents = fs::read(state_path(state_dir)).ok()?;
    serde_json::from_slice(&contents).ok()
}

/// Removes the state file, but only if it still advertises the daemon that
/// owns `token`.
///
/// A daemon that is shutting down must not delete the file out from under a
/// newer daemon that has since replaced it.
pub fn remove_state_if_owned(state_dir: &Path, token: &str) {
    if read_state(state_dir).is_some_and(|state| tokens_match(&state.token, token)) {
        let _ = fs::remove_file(state_path(state_dir));
    }
}

/// Identifies the compiler binary this process is running.
///
/// The crate version alone is not enough: during development the compiler is
/// rebuilt many times under one version, and a daemon left over from the
/// previous build would go on compiling with the previous compiler. The
/// executable's size and modification time change with every rebuild, so a
/// client can tell that the daemon it found is not the compiler it is.
pub fn build_id() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let stamp = env::current_exe().and_then(fs::metadata).ok().map_or_else(
        || "unknown".to_owned(),
        |metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since_epoch| since_epoch.as_nanos());
            format!("{}-{modified}", metadata.len())
        },
    );
    format!("{version}+{stamp}")
}

/// Generates the secret a daemon hands to its clients: 256 bits from the
/// operating system's random number generator, as hex.
pub fn generate_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("could not generate a daemon token: {e}"))?;

    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

/// Compares two tokens without stopping at the first difference.
///
/// A comparison that returns early leaks, through how long it takes, how
/// many leading characters of a guess were right -- enough to recover a
/// secret one character at a time. The length is not secret (every token is
/// 64 characters), so differing lengths may return at once.
pub fn tokens_match(expected: &str, provided: &str) -> bool {
    let (expected, provided) = (expected.as_bytes(), provided.as_bytes());
    if expected.len() != provided.len() {
        return false;
    }

    let mut difference = 0u8;
    for (a, b) in expected.iter().zip(provided) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::{
        build_id, generate_token, read_state, remove_state_if_owned, state_path, tokens_match,
        write_state, DaemonState,
    };

    fn sample(token: &str) -> DaemonState {
        DaemonState {
            pid: 42,
            port: 5000,
            token: token.to_owned(),
            build_id: build_id(),
            protocol: 1,
        }
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        // A nested path, because the directory may not exist yet.
        let state_dir = dir.path().join("nested").join("codira");

        assert_eq!(read_state(&state_dir), None);

        let state = sample("abc");
        write_state(&state_dir, &state).unwrap();
        assert_eq!(read_state(&state_dir), Some(state));

        // No temporary file is left behind next to it.
        let leftovers: Vec<_> = std::fs::read_dir(&state_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    }

    #[test]
    fn a_newer_daemon_replaces_the_state() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &sample("old")).unwrap();
        write_state(dir.path(), &sample("new")).unwrap();
        assert_eq!(read_state(dir.path()).unwrap().token, "new");
    }

    #[test]
    fn a_daemon_only_removes_its_own_state() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &sample("new")).unwrap();

        remove_state_if_owned(dir.path(), "old");
        assert!(state_path(dir.path()).exists());

        remove_state_if_owned(dir.path(), "new");
        assert!(!state_path(dir.path()).exists());
    }

    #[test]
    fn an_unreadable_state_file_means_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(state_path(dir.path()), b"{ not json").unwrap();
        assert_eq!(read_state(dir.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_state_file_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("codira");
        write_state(&state_dir, &sample("abc")).unwrap();

        let file_mode = std::fs::metadata(state_path(&state_dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600);

        let dir_mode = std::fs::metadata(&state_dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
    }

    #[test]
    fn tokens_are_long_random_and_distinct() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn token_comparison_is_exact() {
        assert!(tokens_match("abcdef", "abcdef"));
        assert!(!tokens_match("abcdef", "abcdeg"));
        assert!(!tokens_match("abcdef", "bbcdef"));
        assert!(!tokens_match("abcdef", "abcde"));
        assert!(!tokens_match("abcdef", ""));
        assert!(tokens_match("", ""));
    }

    #[test]
    fn build_id_is_stable_within_a_process() {
        assert_eq!(build_id(), build_id());
        assert!(build_id().starts_with(env!("CARGO_PKG_VERSION")));
    }
}
