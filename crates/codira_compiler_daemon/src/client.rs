//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 1, 2026
//!
//! Functionality:
//! - The client side of the build daemon: finding a running daemon, starting
//!   one when there is none, and exchanging requests with it.

use std::{
    io::BufReader,
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::Context;

use crate::{
    protocol::{
        read_message, write_message, BuildRequest, BuildResponse, Envelope, Pong, Request,
        Response, MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
    },
    state::{build_id, read_state, remove_state_if_owned, DaemonState},
};

/// How long to wait for a daemon on this machine to accept a connection. A
/// live daemon answers a loopback connect in microseconds; one that has not
/// answered in this long is not there.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// How long a daemon may take to answer a `Ping` or a `Shutdown`. Neither
/// does any work, so this only has to cover scheduling.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// How often to look for the state file of a daemon that is starting up.
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// A connection to a running daemon.
pub struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    state: DaemonState,
}

impl Client {
    /// Connects to the daemon advertised in `state_dir` and checks that it
    /// is alive and is the daemon the state file describes.
    ///
    /// `None` means there is no usable daemon: nothing is advertised, or
    /// what is advertised no longer answers. A state file that points at
    /// nothing is removed, so the next client does not repeat the attempt.
    pub fn connect(state_dir: &Path) -> Option<Self> {
        let state = read_state(state_dir)?;

        let client = Self::open(state.clone()).and_then(|mut client| {
            // A stale state file can name a port that some unrelated
            // program has since been given. Only a peer that answers a
            // ping with the advertised identity is the daemon.
            let pong = client.ping().ok()?;
            (pong.pid == state.pid && pong.build_id == state.build_id).then_some(client)
        });

        if client.is_none() {
            remove_state_if_owned(state_dir, &state.token);
        }
        client
    }

    fn open(state: DaemonState) -> Option<Self> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, state.port));
        let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).ok()?;
        // Requests are small and latency is the point; see the matching
        // call on the daemon's side.
        let _ = stream.set_nodelay(true);
        let writer = stream.try_clone().ok()?;
        Some(Self {
            reader: BufReader::new(stream),
            writer,
            state,
        })
    }

    /// What the daemon this client is connected to published about itself.
    pub fn state(&self) -> &DaemonState {
        &self.state
    }

    /// Sends one request and waits for its response.
    ///
    /// `timeout` bounds the wait; `None` waits as long as it takes, which
    /// is what a build needs -- a cold build of a large package is slow,
    /// not stuck.
    fn request(&mut self, request: Request, timeout: Option<Duration>) -> anyhow::Result<Response> {
        self.reader
            .get_ref()
            .set_read_timeout(timeout)
            .context("could not configure the connection to the build daemon")?;

        write_message(
            &mut self.writer,
            &Envelope {
                protocol: PROTOCOL_VERSION,
                token: self.state.token.clone(),
                request,
            },
        )
        .context("could not send the request to the build daemon")?;

        read_message(&mut self.reader, MAX_RESPONSE_BYTES)
            .context("could not read the build daemon's response")?
            .context("the build daemon closed the connection without answering")
    }

    /// Asks the daemon to identify itself.
    pub fn ping(&mut self) -> anyhow::Result<Pong> {
        match self.request(Request::Ping, Some(CONTROL_TIMEOUT))? {
            Response::Pong(pong) => Ok(pong),
            other => Err(unexpected(&other)),
        }
    }

    /// Asks the daemon to build a package.
    pub fn build(&mut self, request: BuildRequest) -> anyhow::Result<BuildResponse> {
        match self.request(Request::Build(request), None)? {
            Response::Build(response) => Ok(response),
            other => Err(unexpected(&other)),
        }
    }

    /// Asks the daemon to exit.
    pub fn shutdown(&mut self) -> anyhow::Result<()> {
        match self.request(Request::Shutdown, Some(CONTROL_TIMEOUT))? {
            Response::ShuttingDown => Ok(()),
            other => Err(unexpected(&other)),
        }
    }
}

/// Turns a response of the wrong kind into an error that says what the
/// daemon actually said.
fn unexpected(response: &Response) -> anyhow::Error {
    match response {
        Response::Error { message } => anyhow::anyhow!("{message}"),
        _ => anyhow::anyhow!("the build daemon sent an unexpected response"),
    }
}

/// Connects to the daemon in `state_dir`, starting one with `start` if none
/// is running -- or if the one that is running is a different compiler.
///
/// `start` must launch a daemon that publishes its state in `state_dir`; it
/// is given no arguments because how to launch one (which executable, how
/// to detach it) is the caller's business.
pub fn connect_or_start(
    state_dir: &Path,
    start: impl FnOnce() -> anyhow::Result<()>,
    startup_timeout: Duration,
) -> anyhow::Result<Client> {
    if let Some(mut client) = Client::connect(state_dir) {
        if client.state().build_id == build_id() && client.state().protocol == PROTOCOL_VERSION {
            return Ok(client);
        }

        // The daemon belongs to another build of the compiler. Using it
        // would silently compile with that compiler instead of this one,
        // so it is retired and replaced.
        let retired_token = client.state().token.clone();
        let _ = client.shutdown();
        drop(client);
        wait_until(startup_timeout, || {
            read_state(state_dir).is_none_or(|state| state.token != retired_token)
        });
        // If it did not get as far as withdrawing its state, do it for it.
        remove_state_if_owned(state_dir, &retired_token);
    }

    start().context("could not start the build daemon")?;

    let deadline = Instant::now() + startup_timeout;
    loop {
        if let Some(client) = Client::connect(state_dir) {
            if client.state().build_id == build_id() {
                return Ok(client);
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "the build daemon did not come up within {} seconds",
                startup_timeout.as_secs()
            );
        }
        std::thread::sleep(STARTUP_POLL_INTERVAL);
    }
}

/// Polls `condition` until it holds or `timeout` passes.
fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(STARTUP_POLL_INTERVAL);
    }
}
