//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 1, 2026
//!
//! Functionality:
//! - The wire protocol between `codira` clients and the build daemon.
//!
//! One JSON document per line, in both directions, over a loopback TCP
//! connection. A connection may carry any number of request/response pairs,
//! so a client that stays connected (an editor, say) pays for the connect
//! once.
//!
//! JSON rather than a hand-packed binary format because the messages are a
//! few hundred bytes and are exchanged once per build: encoding them is
//! microseconds against a build measured in milliseconds, and a protocol a
//! person can read with `nc` is one that can be debugged.

use std::{
    io::{self, BufRead, Read, Write},
    path::PathBuf,
};

use serde_derive::{Deserialize, Serialize};

/// Version of the message layout below. A daemon refuses any other.
///
/// This guards the *shape* of the messages. It does not have to be bumped
/// when the compiler changes: a daemon left over from an older compiler is
/// caught separately, by `DaemonState::build_id`.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest request a daemon will read.
///
/// A request is a path and a handful of flags. The bound exists so that a
/// connection that never sends a newline cannot make the daemon buffer
/// without limit.
pub const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// The largest response a client will read.
///
/// Responses carry rendered diagnostics, which for a badly broken project
/// run to megabytes, so this is far looser than the request bound -- but it
/// is still a bound.
pub const MAX_RESPONSE_BYTES: u64 = 256 * 1024 * 1024;

/// A request together with what authenticates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// The sender's `PROTOCOL_VERSION`.
    pub protocol: u32,

    /// The secret from the daemon's state file. Being able to read that
    /// file is what proves the sender is the user the daemon runs as.
    pub token: String,

    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Asks the daemon to identify itself.
    Ping,

    /// Asks the daemon to build a package.
    Build(BuildRequest),

    /// Asks the daemon to exit once it has answered.
    Shutdown,
}

/// Everything `codira build` needs to say to have a package built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildRequest {
    /// Absolute path of the package's `codira.toml`.
    pub manifest_path: PathBuf,

    /// Optimization level, 0 through 3.
    pub opt_level: u8,

    /// Target triple, or `None` for the daemon's host target.
    pub target: Option<String>,

    /// Emit LLVM IR instead of a `.codiralib`.
    pub emit_ir: bool,

    /// Whether diagnostics should carry ANSI color codes.
    ///
    /// The client decides, because the client is what is attached to the
    /// user's terminal. The daemon has no terminal to ask.
    pub color: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Pong(Pong),

    /// The build ran to completion. `BuildResponse::success` says whether
    /// the package compiled.
    Build(BuildResponse),

    /// The daemon accepted a `Shutdown` and is exiting.
    ShuttingDown,

    /// The request could not be carried out at all: it was malformed, not
    /// authorized, or the build failed for a reason that is not a
    /// diagnostic in the user's code.
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub pid: u32,
    pub build_id: String,

    /// Number of packages the daemon is holding warm.
    pub sessions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildResponse {
    /// `true` if the package compiled and its outputs were written.
    pub success: bool,

    /// Rendered diagnostics, ready to be written to the user's terminal.
    pub diagnostics: String,

    /// Time the daemon spent on this build, from receiving the request to
    /// having the outputs on disk. Excludes the client's own startup and
    /// the round trip, which the daemon cannot see.
    pub elapsed_micros: u64,

    /// `false` when this request created the session, so the build was a
    /// cold one; `true` when it reused a warm session.
    pub session_reused: bool,

    /// Source files added, changed or removed since the previous build.
    pub files_changed: u32,
}

/// Writes `message` as one line and flushes it.
pub fn write_message<T: serde::Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    // `serde_json` never emits a raw newline -- newlines inside strings are
    // escaped -- so one document is always exactly one line.
    let mut bytes =
        serde_json::to_vec(message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

/// Reads one line and decodes it, refusing a line longer than `limit`.
///
/// Returns `Ok(None)` when the peer closed the connection before sending
/// anything, which is how a well-behaved peer ends a conversation.
pub fn read_message<T: serde::de::DeserializeOwned>(
    reader: &mut impl BufRead,
    limit: u64,
) -> io::Result<Option<T>> {
    let mut line = Vec::new();

    // `+ 1` so that a line of exactly `limit` bytes followed by its newline
    // is accepted, while anything longer is distinguishable from it.
    let read = reader
        .by_ref()
        .take(limit + 1)
        .read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        let reason = if line.len() as u64 > limit {
            "message exceeds the size limit"
        } else {
            "connection closed in the middle of a message"
        };
        return Err(io::Error::new(io::ErrorKind::InvalidData, reason));
    }

    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        read_message, write_message, BuildRequest, BuildResponse, Envelope, Request, Response,
        PROTOCOL_VERSION,
    };

    fn round_trip<T>(message: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let mut wire = Vec::new();
        write_message(&mut wire, message).unwrap();
        read_message(&mut Cursor::new(wire), 1 << 20)
            .unwrap()
            .expect("a message was written")
    }

    #[test]
    fn requests_survive_the_wire() {
        let envelope = Envelope {
            protocol: PROTOCOL_VERSION,
            token: "secret".to_owned(),
            request: Request::Build(BuildRequest {
                manifest_path: "/work/project/codira.toml".into(),
                opt_level: 2,
                target: Some("x86_64-pc-windows-msvc".to_owned()),
                emit_ir: false,
                color: true,
            }),
        };
        assert_eq!(round_trip(&envelope), envelope);

        for request in [Request::Ping, Request::Shutdown] {
            assert_eq!(round_trip(&request), request);
        }
    }

    #[test]
    fn diagnostics_with_newlines_stay_on_one_line() {
        let response = Response::Build(BuildResponse {
            success: false,
            diagnostics: "error: first\n --> mod.code:1:1\nerror: second\n".to_owned(),
            elapsed_micros: 412,
            session_reused: true,
            files_changed: 1,
        });

        let mut wire = Vec::new();
        write_message(&mut wire, &response).unwrap();
        assert_eq!(wire.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(wire.last(), Some(&b'\n'));

        assert_eq!(round_trip(&response), response);
    }

    #[test]
    fn several_messages_share_a_connection() {
        let mut wire = Vec::new();
        write_message(&mut wire, &Request::Ping).unwrap();
        write_message(&mut wire, &Request::Shutdown).unwrap();

        let mut reader = Cursor::new(wire);
        assert_eq!(
            read_message::<Request>(&mut reader, 1024).unwrap(),
            Some(Request::Ping)
        );
        assert_eq!(
            read_message::<Request>(&mut reader, 1024).unwrap(),
            Some(Request::Shutdown)
        );
        assert_eq!(read_message::<Request>(&mut reader, 1024).unwrap(), None);
    }

    #[test]
    fn an_oversized_message_is_refused_not_buffered() {
        let mut wire = vec![b'"'; 5000];
        wire.push(b'\n');
        let error = read_message::<String>(&mut Cursor::new(wire), 100).unwrap_err();
        assert!(error.to_string().contains("size limit"), "{error}");
    }

    #[test]
    fn a_truncated_message_is_an_error_not_a_clean_close() {
        let error = read_message::<Request>(&mut Cursor::new(b"{\"kind\":\"pi".to_vec()), 1024)
            .unwrap_err();
        assert!(error.to_string().contains("middle of a message"), "{error}");
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(read_message::<Request>(&mut Cursor::new(b"not json\n".to_vec()), 1024).is_err());
        assert!(read_message::<Request>(
            &mut Cursor::new(b"{\"kind\":\"format_disk\"}\n".to_vec()),
            1024
        )
        .is_err());
    }
}
