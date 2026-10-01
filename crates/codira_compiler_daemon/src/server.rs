//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 1, 2026
//!
//! Functionality:
//! - The build daemon: a compiler that stays alive between builds.
//!
//! # What staying alive buys
//!
//! A one-shot `codira build` starts a process, initializes LLVM, parses and
//! type-checks every file, generates code, links, and then throws all of it
//! away. The daemon keeps a `Driver` per package, and with it the salsa
//! database: the next build re-runs only the queries an edit invalidated,
//! in a process whose LLVM targets are already initialized and whose linker
//! is already paged in.
//!
//! # What it does not trust
//!
//! * **Its callers.** Every request must carry the token from the state file
//!   (`state.rs`), the listener is bound to loopback only, and requests are
//!   bounded in size and in how long they may take to arrive.
//! * **Its own cache.** Nothing is assumed about what changed on disk. Every
//!   build re-reads the package's sources and compares them with what the
//!   database holds (`Driver::sync_source_directory`), so a warm build sees
//!   exactly the bytes a cold one would. A file watcher would be cheaper and
//!   would also be a race: an event that has not arrived yet is a build of
//!   stale sources that reports success.
//! * **The compiler.** A panic while building is caught, reported to the client
//!   as an error, and the session it happened in is thrown away, so the next
//!   request starts from a fresh database rather than from whatever state the
//!   panic left behind. One bad build cannot take the daemon, or any other
//!   package's session, down with it.

use std::{
    collections::HashMap,
    io::BufReader,
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard, PoisonError,
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use codira_compiler::{Config, DisplayColor, Driver, OptimizationLevel, Target};
use codira_project::{Package, MANIFEST_FILENAME};

use crate::{
    protocol::{
        read_message, write_message, BuildRequest, BuildResponse, Envelope, Pong, Request,
        Response, MAX_REQUEST_BYTES, PROTOCOL_VERSION,
    },
    state::{remove_state_if_owned, tokens_match, write_state, DaemonState},
};

/// How long a daemon with nothing to do stays alive, by default.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How many packages a daemon keeps warm at once.
///
/// Each session holds a whole salsa database. Without a bound, a daemon
/// that is pointed at one project after another over a long day grows
/// until it is the problem it was meant to solve.
const MAX_SESSIONS: usize = 16;

/// How long a connection may sit idle between requests before the daemon
/// closes it. Clients that stay connected simply reconnect.
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the idle watchdog looks at the clock.
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(200);

/// Settings for a daemon.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Directory the state file is published in.
    pub state_dir: PathBuf,

    /// Exit after this long without a request. `None` never exits on its
    /// own.
    pub idle_timeout: Option<Duration>,
}

/// Identifies a session: the package, and every setting that changes what
/// building it means.
///
/// The settings are part of the key because they are salsa inputs. Reusing
/// one database across two optimization levels would work, but would
/// invalidate every generated assembly each time the level flipped -- two
/// sessions keep both warm.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    manifest_path: PathBuf,
    opt_level: u8,
    target: Option<String>,
    emit_ir: bool,
}

/// A package held warm: its driver, and the manifest it was created from.
struct Session {
    package: Package,
    driver: Driver,

    /// The manifest's bytes when the session was created. The manifest is
    /// read once, by `Driver::with_package_path`, so an edit to it is
    /// invisible to the driver; comparing the bytes on each build is what
    /// notices it.
    manifest_contents: Vec<u8>,
}

/// What one build did.
struct BuildOutcome {
    success: bool,
    diagnostics: String,
    files_changed: u32,
}

impl Session {
    fn create(key: &SessionKey) -> anyhow::Result<Self> {
        let manifest_contents = std::fs::read(&key.manifest_path)
            .with_context(|| format!("could not read '{}'", key.manifest_path.display()))?;

        let target = match &key.target {
            Some(triple) => Target::search(triple)
                .with_context(|| format!("could not find target for '{triple}'"))?,
            None => Target::host_target().context("unable to determine host target")?,
        };
        let config = Config {
            target,
            optimization_lvl: optimization_level(key.opt_level)?,
            out_dir: None,
            emit_ir: key.emit_ir,
        };

        let (package, driver) = Driver::with_package_path(&key.manifest_path, config)?;
        Ok(Self {
            package,
            driver,
            manifest_contents,
        })
    }

    /// Whether the manifest on disk is still the one this session was
    /// created from. An unreadable manifest counts as changed, so the
    /// rebuild that follows reports why it cannot be read.
    fn manifest_is_current(&self, key: &SessionKey) -> bool {
        std::fs::read(&key.manifest_path).is_ok_and(|contents| contents == self.manifest_contents)
    }

    fn build(&mut self, color: bool) -> anyhow::Result<BuildOutcome> {
        let summary = self
            .driver
            .sync_source_directory(&self.package.source_directory())?;

        let display_color = if color {
            DisplayColor::Enable
        } else {
            DisplayColor::Disable
        };

        let mut diagnostics = Vec::new();
        let has_errors = self
            .driver
            .emit_diagnostics(&mut diagnostics, display_color)?;
        if !has_errors {
            self.driver.write_all_assemblies(false)?;
        }

        Ok(BuildOutcome {
            success: !has_errors,
            diagnostics: String::from_utf8_lossy(&diagnostics).into_owned(),
            files_changed: u32::try_from(summary.total()).unwrap_or(u32::MAX),
        })
    }
}

fn optimization_level(level: u8) -> anyhow::Result<OptimizationLevel> {
    match level {
        0 => Ok(OptimizationLevel::None),
        1 => Ok(OptimizationLevel::Less),
        2 => Ok(OptimizationLevel::Default),
        3 => Ok(OptimizationLevel::Aggressive),
        _ => Err(anyhow::anyhow!(
            "Only optimization levels 0-3 are supported"
        )),
    }
}

/// A slot in the session table.
///
/// The slot is created under the table's lock and filled under its own, so
/// that creating a session -- which reads a whole package from disk -- never
/// blocks requests for other packages. `None` means "not created yet" or
/// "discarded after a failure".
struct Slot {
    session: Mutex<Option<Session>>,
    last_used: Mutex<Instant>,
}

/// State shared by every connection.
struct Shared {
    state: DaemonState,
    local_addr: SocketAddr,
    sessions: Mutex<HashMap<SessionKey, Arc<Slot>>>,
    shutting_down: AtomicBool,
    last_activity: Mutex<Instant>,
    active_requests: AtomicUsize,
}

/// Locks a mutex whether or not a thread panicked while holding it.
///
/// Every mutex here guards data that stays valid across a panic (a
/// timestamp, a table of slots, a session that is discarded on failure
/// anyway), so a poisoned lock carries no information worth dying for.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn touch(&self) {
        *lock(&self.last_activity) = Instant::now();
    }

    /// Stops the accept loop. `TcpListener::accept` cannot be interrupted,
    /// so after raising the flag this connects to the listener itself to
    /// make the blocked `accept` return and see it.
    fn begin_shutdown(&self) {
        if !self.shutting_down.swap(true, Ordering::SeqCst) {
            let _ = TcpStream::connect_timeout(&self.local_addr, Duration::from_secs(1));
        }
    }

    /// Finds the slot for `key`, creating it -- and evicting the least
    /// recently used session if the table is full -- when there is none.
    fn slot(&self, key: &SessionKey) -> Arc<Slot> {
        let mut sessions = lock(&self.sessions);
        if let Some(slot) = sessions.get(key) {
            return slot.clone();
        }

        if sessions.len() >= MAX_SESSIONS {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, slot)| *lock(&slot.last_used))
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                // A build still running in the evicted session keeps its
                // slot alive through its own `Arc` and finishes normally;
                // the session is freed when that build lets go of it.
                sessions.remove(&oldest);
            }
        }

        let slot = Arc::new(Slot {
            session: Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
        });
        sessions.insert(key.clone(), slot.clone());
        slot
    }

    fn handle(&self, envelope: Envelope) -> Response {
        if envelope.protocol != PROTOCOL_VERSION {
            return Response::Error {
                message: format!(
                    "the daemon speaks protocol {PROTOCOL_VERSION}, the client protocol {}",
                    envelope.protocol
                ),
            };
        }
        if !tokens_match(&self.state.token, &envelope.token) {
            return Response::Error {
                message: "not authorized".to_owned(),
            };
        }

        match envelope.request {
            Request::Ping => Response::Pong(Pong {
                pid: self.state.pid,
                build_id: self.state.build_id.clone(),
                sessions: u32::try_from(lock(&self.sessions).len()).unwrap_or(u32::MAX),
            }),
            Request::Shutdown => Response::ShuttingDown,
            Request::Build(request) => self.build(&request),
        }
    }

    fn build(&self, request: &BuildRequest) -> Response {
        let started = Instant::now();

        let key = match session_key(request) {
            Ok(key) => key,
            Err(error) => {
                return Response::Error {
                    message: format!("{error:#}"),
                }
            }
        };

        let slot = self.slot(&key);
        *lock(&slot.last_used) = Instant::now();

        // Builds of one package are serialized by this lock. Two clients
        // asking for the same package at once would otherwise race each
        // other to write the same output files.
        let mut session = lock(&slot.session);

        let result = catch_unwind(AssertUnwindSafe(
            || -> anyhow::Result<(BuildOutcome, bool)> {
                let reused = match session.as_ref() {
                    Some(existing) if existing.manifest_is_current(&key) => true,
                    _ => {
                        *session = None;
                        *session = Some(Session::create(&key)?);
                        false
                    }
                };
                let outcome = session
                    .as_mut()
                    .expect("the session was created above")
                    .build(request.color)?;
                Ok((outcome, reused))
            },
        ));

        match result {
            Ok(Ok((outcome, session_reused))) => Response::Build(BuildResponse {
                success: outcome.success,
                diagnostics: outcome.diagnostics,
                elapsed_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                session_reused,
                files_changed: outcome.files_changed,
            }),
            Ok(Err(error)) => {
                // The build stopped half way; whether the database is still
                // consistent is not something worth reasoning about.
                *session = None;
                Response::Error {
                    message: format!("{error:#}"),
                }
            }
            Err(panic) => {
                *session = None;
                Response::Error {
                    message: format!(
                        "the compiler panicked while building '{}': {}\nThe daemon discarded \
                         that package's cached state and is still running; the next build \
                         starts from scratch.",
                        key.manifest_path.display(),
                        panic_message(panic.as_ref())
                    ),
                }
            }
        }
    }
}

/// Validates a build request and turns it into the key of its session.
///
/// The path is canonicalized so that two spellings of one manifest share a
/// session, and it must name a `codira.toml`: the daemon reads whatever it
/// is pointed at and writes a `target` directory next to it, so "any file
/// at all" is not a request it should accept.
fn session_key(request: &BuildRequest) -> anyhow::Result<SessionKey> {
    if !request.manifest_path.is_absolute() {
        anyhow::bail!(
            "'{}' is not an absolute path",
            request.manifest_path.display()
        );
    }
    if request.manifest_path.file_name() != Some(MANIFEST_FILENAME.as_ref()) {
        anyhow::bail!(
            "'{}' is not a {MANIFEST_FILENAME}",
            request.manifest_path.display()
        );
    }
    // Checked here, not only when the session is created, so that a bad
    // level is rejected even when a session for it could never exist.
    optimization_level(request.opt_level)?;

    let manifest_path = std::fs::canonicalize(&request.manifest_path).map_err(|_error| {
        anyhow::anyhow!(
            "'{}' does not refer to a valid manifest path",
            request.manifest_path.display()
        )
    })?;

    Ok(SessionKey {
        manifest_path,
        opt_level: request.opt_level,
        target: request.target.clone(),
        emit_ir: request.emit_ir,
    })
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

/// A build daemon that has claimed its port and published its state, and is
/// ready to serve.
pub struct Server {
    listener: TcpListener,
    shared: Arc<Shared>,
    options: ServerOptions,
}

impl Server {
    /// Binds a loopback port and publishes it, with a fresh token, in the
    /// state file.
    pub fn bind(options: ServerOptions) -> anyhow::Result<Self> {
        // Loopback only, and a port chosen by the operating system: the
        // daemon is never reachable from another machine, and never fights
        // anything else for a well-known port.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("could not bind a loopback port for the build daemon")?;
        let local_addr = listener.local_addr()?;

        let state = DaemonState::for_this_process(local_addr.port())?;
        write_state(&options.state_dir, &state)?;

        Ok(Self {
            listener,
            shared: Arc::new(Shared {
                state,
                local_addr,
                sessions: Mutex::new(HashMap::new()),
                shutting_down: AtomicBool::new(false),
                last_activity: Mutex::new(Instant::now()),
                active_requests: AtomicUsize::new(0),
            }),
            options,
        })
    }

    /// What this daemon published about itself.
    pub fn state(&self) -> &DaemonState {
        &self.shared.state
    }

    /// Returns a handle that can stop the daemon from another thread.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            shared: self.shared.clone(),
        }
    }

    /// Serves requests until told to shut down or idle for too long, then
    /// withdraws the state file.
    pub fn run(self) -> anyhow::Result<()> {
        if let Some(idle_timeout) = self.options.idle_timeout {
            let shared = self.shared.clone();
            std::thread::spawn(move || idle_watchdog(&shared, idle_timeout));
        }

        for connection in self.listener.incoming() {
            if self.shared.shutting_down.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = connection else {
                // A failed accept (the peer vanished mid-handshake, the
                // process ran out of descriptors for a moment) is that
                // connection's problem, not a reason to stop serving.
                continue;
            };

            let shared = self.shared.clone();
            std::thread::spawn(move || serve_connection(&shared, stream));
        }

        remove_state_if_owned(&self.options.state_dir, &self.shared.state.token);
        Ok(())
    }
}

/// Stops a running `Server`.
#[derive(Clone)]
pub struct ShutdownHandle {
    shared: Arc<Shared>,
}

impl ShutdownHandle {
    pub fn shutdown(&self) {
        self.shared.begin_shutdown();
    }
}

/// Shuts the daemon down once it has gone `idle_timeout` without a request.
///
/// A daemon nobody is using is a compiler's worth of memory held for
/// nothing; the next build starts another one.
fn idle_watchdog(shared: &Shared, idle_timeout: Duration) {
    while !shared.shutting_down.load(Ordering::SeqCst) {
        std::thread::sleep(WATCHDOG_INTERVAL.min(idle_timeout));

        // A build that outlasts the timeout is not idleness.
        if shared.active_requests.load(Ordering::SeqCst) > 0 {
            shared.touch();
            continue;
        }
        if lock(&shared.last_activity).elapsed() >= idle_timeout {
            shared.begin_shutdown();
        }
    }
}

/// Serves one connection until the peer closes it, goes quiet, or says
/// something that cannot be understood.
fn serve_connection(shared: &Shared, stream: TcpStream) {
    // Answers are small and latency is the whole point, so do not let the
    // kernel hold a response back waiting for more bytes to batch with it.
    let _ = stream.set_nodelay(true);
    // Bounds how long a connected peer can hold a thread without speaking.
    let _ = stream.set_read_timeout(Some(CONNECTION_IDLE_TIMEOUT));

    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);

    loop {
        let envelope = match read_message::<Envelope>(&mut reader, MAX_REQUEST_BYTES) {
            Ok(Some(envelope)) => envelope,
            Ok(None) => break,
            Err(error) => {
                // Say why before hanging up; a peer that sent garbage gets
                // one answer and no second chance on this connection.
                let _ = write_message(
                    &mut writer,
                    &Response::Error {
                        message: format!("malformed request: {error}"),
                    },
                );
                break;
            }
        };

        shared.touch();
        shared.active_requests.fetch_add(1, Ordering::SeqCst);
        let response = shared.handle(envelope);
        shared.active_requests.fetch_sub(1, Ordering::SeqCst);
        shared.touch();

        let shutting_down = response == Response::ShuttingDown;
        let written = write_message(&mut writer, &response);

        if shutting_down {
            // Only after the answer is on the wire, so the client that
            // asked hears that it worked.
            shared.begin_shutdown();
            break;
        }
        if written.is_err() {
            break;
        }
    }

    let _ = writer.shutdown(Shutdown::Both);
}

/// Runs a daemon in this process until it shuts down.
pub fn run_daemon(options: ServerOptions) -> anyhow::Result<()> {
    let server = Server::bind(options)?;
    log::info!(
        "build daemon listening on 127.0.0.1:{} (pid {})",
        server.state().port,
        server.state().pid
    );

    // Ctrl-C withdraws the state file on the way out instead of leaving a
    // stale one for the next client to trip over.
    let handle = server.shutdown_handle();
    let _ = ctrlc::set_handler(move || handle.shutdown());

    server.run()
}
