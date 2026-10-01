//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 1, 2026
//!
//! Functionality:
//! - End-to-end tests of the build daemon: a real server on a real loopback
//!   port, driven through the real client, compiling real packages.

use std::{
    io::BufReader,
    net::{Ipv4Addr, TcpStream},
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use codira_compiler_daemon::{
    client::{connect_or_start, Client},
    protocol::{
        read_message, write_message, BuildRequest, BuildResponse, Envelope, Request, Response,
        PROTOCOL_VERSION,
    },
    server::{Server, ServerOptions, ShutdownHandle},
    state::{read_state, state_path, write_state, DaemonState},
};
use codira_runtime::Runtime;
use tempfile::TempDir;

/// A daemon running on a thread of the test process.
struct Daemon {
    state_dir: TempDir,
    handle: ShutdownHandle,
    thread: Option<JoinHandle<()>>,
}

impl Daemon {
    fn start() -> Self {
        Self::start_with_idle_timeout(None)
    }

    fn start_with_idle_timeout(idle_timeout: Option<Duration>) -> Self {
        let state_dir = tempfile::tempdir().unwrap();
        let (handle, thread) = spawn_server(state_dir.path(), idle_timeout);
        Self {
            state_dir,
            handle,
            thread: Some(thread),
        }
    }

    fn client(&self) -> Client {
        Client::connect(self.state_dir.path()).expect("the daemon is running")
    }

    fn state(&self) -> DaemonState {
        read_state(self.state_dir.path()).expect("the daemon published its state")
    }

    /// Waits for the daemon's thread to finish on its own.
    fn join(&mut self) {
        self.thread.take().unwrap().join().unwrap();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn spawn_server(
    state_dir: &Path,
    idle_timeout: Option<Duration>,
) -> (ShutdownHandle, JoinHandle<()>) {
    let server = Server::bind(ServerOptions {
        state_dir: state_dir.to_path_buf(),
        idle_timeout,
    })
    .unwrap();
    let handle = server.shutdown_handle();
    let thread = std::thread::spawn(move || server.run().unwrap());
    (handle, thread)
}

/// A package on disk.
struct Project {
    dir: TempDir,
}

impl Project {
    fn new(source: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("codira.toml"),
            "[package]\nname=\"sample\"\nauthors=[]\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let project = Self { dir };
        project.write("mod.code", source);
        project
    }

    fn manifest(&self) -> PathBuf {
        self.dir.path().join("codira.toml")
    }

    fn write(&self, file: &str, source: &str) {
        std::fs::write(self.dir.path().join("src").join(file), source).unwrap();
    }

    fn remove(&self, file: &str) {
        std::fs::remove_file(self.dir.path().join("src").join(file)).unwrap();
    }

    fn library(&self) -> PathBuf {
        self.dir.path().join("target").join("mod.codiralib")
    }

    fn request(&self) -> BuildRequest {
        BuildRequest {
            manifest_path: self.manifest(),
            opt_level: 2,
            target: None,
            emit_ir: false,
            color: false,
        }
    }

    /// Loads the built library and calls its `main`.
    fn run_main(&self) -> i64 {
        // Safety: the library was compiled by the daemon under test from
        // sources this test wrote.
        let runtime = unsafe { Runtime::builder(self.library()).finish() }.unwrap();
        runtime.invoke("main", ()).unwrap()
    }
}

fn main_returning(value: i64) -> String {
    format!("public func main() -> i64 {{\n    {value}\n}}\n")
}

fn build(client: &mut Client, project: &Project) -> BuildResponse {
    client.build(project.request()).unwrap()
}

/// Sends one envelope over a fresh connection, bypassing the client.
fn raw_request(port: u16, envelope: &Envelope) -> Response {
    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    let mut writer = stream.try_clone().unwrap();
    write_message(&mut writer, envelope).unwrap();
    read_message(&mut BufReader::new(stream), 1 << 20)
        .unwrap()
        .expect("the daemon answers before closing")
}

fn error_message(response: Response) -> String {
    match response {
        Response::Error { message } => message,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn builds_a_package_and_keeps_it_warm() {
    let daemon = Daemon::start();
    let project = Project::new(&main_returning(7));
    let mut client = daemon.client();

    let first = build(&mut client, &project);
    assert!(first.success, "{}", first.diagnostics);
    assert!(!first.session_reused);
    assert_eq!(project.run_main(), 7);

    // Nothing changed: the same session answers, and finds nothing to do.
    let second = build(&mut client, &project);
    assert!(second.success);
    assert!(second.session_reused);
    assert_eq!(second.files_changed, 0);
    assert_eq!(project.run_main(), 7);

    assert_eq!(client.ping().unwrap().sessions, 1);
}

#[test]
fn a_warm_build_sees_what_is_on_disk() {
    let daemon = Daemon::start();
    let project = Project::new(&main_returning(1));
    let mut client = daemon.client();

    assert!(build(&mut client, &project).success);
    assert_eq!(project.run_main(), 1);

    // The daemon is told nothing about this edit; it has to find it.
    project.write("mod.code", &main_returning(2));
    let response = build(&mut client, &project);
    assert!(response.success, "{}", response.diagnostics);
    assert!(response.session_reused);
    assert_eq!(response.files_changed, 1);
    assert_eq!(project.run_main(), 2);
}

#[test]
fn diagnostics_are_a_result_not_a_failure() {
    let daemon = Daemon::start();
    let project = Project::new("public func main() -> i64 {\n    1 +\n}\n");
    let mut client = daemon.client();

    let broken = build(&mut client, &project);
    assert!(!broken.success);
    assert!(
        broken.diagnostics.contains("error"),
        "{}",
        broken.diagnostics
    );
    assert!(
        broken.diagnostics.contains("mod.code"),
        "{}",
        broken.diagnostics
    );
    // Color was not asked for, so none was sent.
    assert!(!broken.diagnostics.contains('\u{1b}'));
    assert!(!project.library().exists());

    // The session survives a failed build and picks up the fix.
    project.write("mod.code", &main_returning(3));
    let fixed = build(&mut client, &project);
    assert!(fixed.success, "{}", fixed.diagnostics);
    assert!(fixed.session_reused);
    assert_eq!(fixed.diagnostics, "");
    assert_eq!(project.run_main(), 3);
}

#[test]
fn files_added_and_removed_between_builds_are_noticed() {
    let daemon = Daemon::start();
    let project = Project::new(&main_returning(4));
    let mut client = daemon.client();
    assert!(build(&mut client, &project).success);

    // A new file that does not compile must fail the build: if the daemon
    // missed it, the build would wrongly succeed.
    project.write("extra.code", "public func broken() -> i64 {\n    1 +\n}\n");
    let with_broken_file = build(&mut client, &project);
    assert!(!with_broken_file.success);
    assert_eq!(with_broken_file.files_changed, 1);
    assert!(
        with_broken_file.diagnostics.contains("extra.code"),
        "{}",
        with_broken_file.diagnostics
    );

    // And once it is deleted, the build must succeed again.
    project.remove("extra.code");
    let after_removal = build(&mut client, &project);
    assert!(after_removal.success, "{}", after_removal.diagnostics);
    assert_eq!(after_removal.files_changed, 1);
    assert_eq!(project.run_main(), 4);
}

#[test]
fn an_edited_manifest_starts_a_fresh_session() {
    let daemon = Daemon::start();
    let project = Project::new(&main_returning(5));
    let mut client = daemon.client();
    assert!(build(&mut client, &project).success);
    assert!(build(&mut client, &project).session_reused);

    std::fs::write(
        project.manifest(),
        "[package]\nname=\"renamed\"\nauthors=[]\nversion=\"0.2.0\"\n",
    )
    .unwrap();
    let response = build(&mut client, &project);
    assert!(response.success, "{}", response.diagnostics);
    assert!(!response.session_reused);
}

#[test]
fn different_settings_get_different_sessions() {
    let daemon = Daemon::start();
    let project = Project::new(&main_returning(6));
    let mut client = daemon.client();

    assert!(build(&mut client, &project).success);

    let unoptimized = client
        .build(BuildRequest {
            opt_level: 0,
            ..project.request()
        })
        .unwrap();
    assert!(unoptimized.success);
    assert!(!unoptimized.session_reused);

    let ir = client
        .build(BuildRequest {
            emit_ir: true,
            ..project.request()
        })
        .unwrap();
    assert!(ir.success);
    assert!(project.dir.path().join("target").join("mod.ll").is_file());

    assert_eq!(client.ping().unwrap().sessions, 3);
}

#[test]
fn requests_without_the_token_are_refused() {
    let daemon = Daemon::start();
    let project = Project::new(&main_returning(8));
    let state = daemon.state();

    for token in ["", "0000", &"f".repeat(64)] {
        let response = raw_request(
            state.port,
            &Envelope {
                protocol: PROTOCOL_VERSION,
                token: token.to_owned(),
                request: Request::Build(project.request()),
            },
        );
        assert_eq!(error_message(response), "not authorized");
    }
    // Nothing was built on behalf of the unauthenticated caller.
    assert!(!project.dir.path().join("target").exists());

    // Nor can a stranger stop the daemon.
    let response = raw_request(
        state.port,
        &Envelope {
            protocol: PROTOCOL_VERSION,
            token: "0".repeat(64),
            request: Request::Shutdown,
        },
    );
    assert_eq!(error_message(response), "not authorized");
    assert_eq!(daemon.client().ping().unwrap().pid, std::process::id());
}

#[test]
fn a_mismatched_protocol_is_refused() {
    let daemon = Daemon::start();
    let state = daemon.state();
    let response = raw_request(
        state.port,
        &Envelope {
            protocol: PROTOCOL_VERSION + 1,
            token: state.token,
            request: Request::Ping,
        },
    );
    assert!(error_message(response).contains("protocol"));
}

#[test]
fn only_manifests_can_be_built() {
    let daemon = Daemon::start();
    let project = Project::new(&main_returning(9));
    let mut client = daemon.client();

    let build_error =
        |client: &mut Client, request: BuildRequest| client.build(request).unwrap_err().to_string();

    let not_a_manifest = build_error(
        &mut client,
        BuildRequest {
            manifest_path: project.dir.path().join("src").join("mod.code"),
            ..project.request()
        },
    );
    assert!(
        not_a_manifest.contains("is not a codira.toml"),
        "{not_a_manifest}"
    );

    let relative = build_error(
        &mut client,
        BuildRequest {
            manifest_path: PathBuf::from("codira.toml"),
            ..project.request()
        },
    );
    assert!(relative.contains("not an absolute path"), "{relative}");

    let missing = build_error(
        &mut client,
        BuildRequest {
            manifest_path: project.dir.path().join("nowhere").join("codira.toml"),
            ..project.request()
        },
    );
    assert!(missing.contains("valid manifest path"), "{missing}");

    let bad_level = build_error(
        &mut client,
        BuildRequest {
            opt_level: 9,
            ..project.request()
        },
    );
    assert!(bad_level.contains("optimization levels 0-3"), "{bad_level}");

    let bad_target = build_error(
        &mut client,
        BuildRequest {
            target: Some("pdp11-unknown-none".to_owned()),
            ..project.request()
        },
    );
    assert!(bad_target.contains("could not find target"), "{bad_target}");

    // The connection, and the daemon, are fine after all of that.
    assert!(build(&mut client, &project).success);
}

#[test]
fn garbage_gets_an_answer_and_the_daemon_carries_on() {
    use std::io::Write;

    let daemon = Daemon::start();
    let state = daemon.state();

    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, state.port)).unwrap();
    let mut writer = stream.try_clone().unwrap();
    writer.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    let response: Response = read_message(&mut BufReader::new(stream), 1 << 20)
        .unwrap()
        .unwrap();
    assert!(error_message(response).contains("malformed request"));

    assert_eq!(daemon.client().ping().unwrap().pid, std::process::id());
}

#[test]
fn shutdown_stops_the_daemon_and_withdraws_its_state() {
    let mut daemon = Daemon::start();
    let state_file = state_path(daemon.state_dir.path());
    assert!(state_file.is_file());

    daemon.client().shutdown().unwrap();
    daemon.join();

    assert!(!state_file.exists());
    assert!(Client::connect(daemon.state_dir.path()).is_none());
}

#[test]
fn an_idle_daemon_exits_on_its_own() {
    let mut daemon = Daemon::start_with_idle_timeout(Some(Duration::from_millis(300)));
    let state_file = state_path(daemon.state_dir.path());

    // A request inside the window keeps it alive past the first deadline.
    std::thread::sleep(Duration::from_millis(150));
    daemon.client().ping().unwrap();

    let started = Instant::now();
    daemon.join();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!state_file.exists());
}

#[test]
fn a_stale_state_file_is_cleaned_up() {
    let state_dir = tempfile::tempdir().unwrap();

    // A port nothing is listening on: bind one, note it, and let it go.
    let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    write_state(
        state_dir.path(),
        &DaemonState {
            pid: 1,
            port,
            token: "a".repeat(64),
            build_id: "gone".to_owned(),
            protocol: PROTOCOL_VERSION,
        },
    )
    .unwrap();

    assert!(Client::connect(state_dir.path()).is_none());
    assert!(!state_path(state_dir.path()).exists());
}

#[test]
fn a_missing_daemon_is_started_on_demand() {
    let state_dir = tempfile::tempdir().unwrap();
    let mut started = None;

    let mut client = connect_or_start(
        state_dir.path(),
        || {
            started = Some(spawn_server(state_dir.path(), None));
            Ok(())
        },
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(client.ping().unwrap().pid, std::process::id());

    // With one running, a second call uses it instead of starting another.
    let mut again = connect_or_start(
        state_dir.path(),
        || panic!("a daemon is already running"),
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(again.state().token, client.state().token);
    again.ping().unwrap();

    let (handle, thread) = started.unwrap();
    handle.shutdown();
    thread.join().unwrap();
}

#[test]
fn a_daemon_from_another_compiler_build_is_replaced() {
    let state_dir = tempfile::tempdir().unwrap();
    let (old_handle, old_thread) = spawn_server(state_dir.path(), None);

    // Rewrite the advertised build id, as a daemon left running by an
    // older compiler binary would have published it.
    let mut state = read_state(state_dir.path()).unwrap();
    let old_token = state.token.clone();
    state.build_id = "0.0.0+some-other-build".to_owned();
    write_state(state_dir.path(), &state).unwrap();

    // The live daemon's ping no longer matches what is advertised, so it
    // is not trusted, and a replacement is started.
    let mut replacement = None;
    let client = connect_or_start(
        state_dir.path(),
        || {
            replacement = Some(spawn_server(state_dir.path(), None));
            Ok(())
        },
        Duration::from_secs(10),
    )
    .unwrap();
    assert_ne!(client.state().token, old_token);

    old_handle.shutdown();
    old_thread.join().unwrap();
    // The old daemon must not have withdrawn the new daemon's state.
    assert_eq!(
        read_state(state_dir.path()).unwrap().token,
        client.state().token
    );

    let (handle, thread) = replacement.unwrap();
    handle.shutdown();
    thread.join().unwrap();
}
