//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 1, 2026
//!
//! Functionality:
//! - `codira daemon`: run, start, stop and inspect the build daemon, and the
//!   client half of `codira build --daemon`.

use std::{
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::Context;
use codira_compiler_daemon::{
    client::{connect_or_start, Client},
    protocol::BuildRequest,
    server::{run_daemon, ServerOptions, DEFAULT_IDLE_TIMEOUT},
    state::default_state_dir,
};

use crate::ExitStatus;

/// How long to wait for a freshly started daemon to begin answering.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(clap::Args)]
pub struct Args {
    #[clap(subcommand)]
    command: DaemonCommand,
}

#[derive(clap::Subcommand)]
enum DaemonCommand {
    /// Run the build daemon in this terminal until it is stopped
    Run {
        /// Exit after this many seconds without a request; 0 never exits
        #[clap(long, default_value_t = DEFAULT_IDLE_TIMEOUT.as_secs())]
        idle_timeout: u64,
    },

    /// Start the build daemon in the background, if it is not running
    Start,

    /// Stop the build daemon
    Stop,

    /// Report whether the build daemon is running
    Status,
}

/// This function is invoked when the executable is run with the `daemon`
/// argument.
pub fn daemon(args: Args) -> Result<ExitStatus, anyhow::Error> {
    let state_dir = default_state_dir()?;

    match args.command {
        DaemonCommand::Run { idle_timeout } => {
            run_daemon(ServerOptions {
                state_dir,
                idle_timeout: (idle_timeout > 0).then(|| Duration::from_secs(idle_timeout)),
            })?;
            Ok(ExitStatus::Success)
        }
        DaemonCommand::Start => {
            let client = connect_or_start(&state_dir, spawn_daemon, STARTUP_TIMEOUT)?;
            println!(
                "build daemon running (pid {}, port {})",
                client.state().pid,
                client.state().port
            );
            Ok(ExitStatus::Success)
        }
        DaemonCommand::Stop => {
            if let Some(mut client) = Client::connect(&state_dir) {
                client.shutdown()?;
                println!("build daemon stopped (pid {})", client.state().pid);
            } else {
                println!("build daemon is not running");
            }
            Ok(ExitStatus::Success)
        }
        DaemonCommand::Status => {
            if let Some(mut client) = Client::connect(&state_dir) {
                let pong = client.ping()?;
                println!(
                    "build daemon running (pid {}, port {}, {} warm package{})",
                    pong.pid,
                    client.state().port,
                    pong.sessions,
                    if pong.sessions == 1 { "" } else { "s" }
                );
                Ok(ExitStatus::Success)
            } else {
                println!("build daemon is not running");
                // Not running is an answer, not a failure of this command,
                // but scripts need to be able to tell the two states apart.
                Ok(ExitStatus::Error)
            }
        }
    }
}

/// Launches `codira daemon run` as a background process that outlives this
/// one.
fn spawn_daemon() -> anyhow::Result<()> {
    let executable = std::env::current_exe().context("could not locate the codira executable")?;
    let state_dir = default_state_dir()?;
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("could not create '{}'", state_dir.display()))?;

    let mut command = Command::new(executable);
    command
        .args(["daemon", "run"])
        // The daemon outlives the build that started it. Left in the
        // caller's directory it would pin that directory for as long as
        // it runs -- on Windows, nobody could delete or rename it.
        .current_dir(&state_dir)
        // Nothing reads the daemon's output, and a pipe nobody drains
        // would eventually block it; its results travel over the socket.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);

    // Held across the spawn only; see `StdHandlesNotInherited`.
    let _not_inherited = StdHandlesNotInherited::new();
    command.spawn().context("could not launch the daemon")?;
    Ok(())
}

/// While alive, keeps this process's standard handles from being inherited
/// by a child it spawns.
///
/// Redirecting the daemon's own stdio to the null device is not enough on
/// Windows. A child there inherits *every* inheritable handle its parent
/// holds, whether or not it is wired up as the child's stdin, stdout or
/// stderr -- and a parent's standard handles are inheritable. So when
/// `codira build --daemon` runs with its output captured (an editor, a CI
/// step, `$(codira build --daemon)` in a shell), the daemon would end up
/// holding the write end of that capture pipe. The build finishes, the
/// client exits, and whoever is reading the pipe waits for an end-of-file
/// that cannot come until the daemon exits, half an hour later. From the
/// outside that is a build that hangs.
///
/// Rust marks the handles it opens itself as non-inheritable, so the three
/// standard handles are the ones that need this.
#[cfg(windows)]
struct StdHandlesNotInherited {
    /// The handles whose inherit flag was cleared, to be set again on drop.
    cleared: Vec<std::os::windows::io::RawHandle>,
}

#[cfg(windows)]
mod handle_api {
    use std::os::windows::io::RawHandle;

    pub const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetHandleInformation(handle: RawHandle, flags: *mut u32) -> i32;
        pub fn SetHandleInformation(handle: RawHandle, mask: u32, flags: u32) -> i32;
    }
}

#[cfg(windows)]
impl StdHandlesNotInherited {
    fn new() -> Self {
        use std::os::windows::io::AsRawHandle;

        use handle_api::{GetHandleInformation, SetHandleInformation, HANDLE_FLAG_INHERIT};

        let handles = [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ];

        let mut cleared = Vec::new();
        for handle in handles {
            // A process started without a console has no standard handles.
            if handle.is_null() {
                continue;
            }
            let mut flags = 0u32;
            // Safety: `handle` is one of this process's own standard
            // handles, which stay open for its whole life, and `flags`
            // outlives the call. Both functions only fail -- returning 0
            // -- on a handle that is not valid, in which case there is
            // nothing to inherit and nothing to restore.
            let was_inheritable = unsafe { GetHandleInformation(handle, &mut flags) } != 0
                && flags & HANDLE_FLAG_INHERIT != 0;
            if was_inheritable
                && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0
            {
                cleared.push(handle);
            }
        }
        Self { cleared }
    }
}

#[cfg(windows)]
impl Drop for StdHandlesNotInherited {
    fn drop(&mut self) {
        use handle_api::{SetHandleInformation, HANDLE_FLAG_INHERIT};
        for handle in &self.cleared {
            // Safety: as in `new`. The flag is put back so that anything
            // this process spawns afterwards inherits its terminal as
            // usual.
            unsafe {
                SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
        }
    }
}

/// Elsewhere there is nothing to do: the standard descriptors are replaced
/// by the null device in the child, and every other descriptor Rust opens
/// is close-on-exec.
#[cfg(not(windows))]
struct StdHandlesNotInherited;

#[cfg(not(windows))]
impl StdHandlesNotInherited {
    fn new() -> Self {
        Self
    }
}

/// Cuts the daemon loose from the terminal that started it, so closing the
/// terminal -- or pressing Ctrl-C in it -- does not take the daemon down.
#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // Its own (hidden) console, and its own process group so that a Ctrl-C
    // aimed at the client is not delivered to it as well.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Its own process group, so terminal signals sent to the client's
    // group do not reach it.
    command.process_group(0);
}

#[cfg(not(any(windows, unix)))]
fn detach(_command: &mut Command) {}

/// Builds a package by asking the daemon, starting one if need be.
///
/// Returns the build's outcome. An `Err` means the daemon could not be
/// used at all -- the caller then builds in-process, so a broken daemon
/// costs time, never a build.
pub fn build_via_daemon(request: BuildRequest) -> anyhow::Result<ExitStatus> {
    debug_assert!(request.manifest_path.is_absolute());

    let state_dir = default_state_dir()?;
    let mut client = connect_or_start(&state_dir, spawn_daemon, STARTUP_TIMEOUT)?;
    let response = client.build(request)?;

    // Diagnostics go where a local build would have put them.
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(response.diagnostics.as_bytes())?;
    stderr.flush()?;

    log::info!(
        "daemon built in {} us ({} file(s) changed, {} session)",
        response.elapsed_micros,
        response.files_changed,
        if response.session_reused {
            "warm"
        } else {
            "new"
        }
    );

    Ok(response.success.into())
}
