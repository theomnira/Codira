//! Runs funis's shell tests in parallel for Cargo's test harness.
//!
//! The tests are shell scripts, in elf/ and macho/, so that they exercise
//! funis with the real compilers, binutils and loaders of each target. The
//! runner owns test discovery, target selection, scheduling, timeouts and
//! reporting. The ELF tests run for the host and, under QEMU, for each
//! cross target whose toolchain is installed (see elf.rs). The Mach-O
//! tests, which drive Apple's toolchain, run on macOS for the host, for
//! x86_64 under Rosetta and on simulator devices (see macho.rs).
// Vendored from mold (see NOTICE) and kept close to upstream, so that mold's changes keep
// applying. The workspace turns on extra lints (.cargo/config.toml) that mold's code does not
// follow, thousands of times over. They are allowed here by name, and nothing else is.
#![allow(
    elided_lifetimes_in_paths,
    clippy::bool_to_int_with_if,
    clippy::cast_lossless,
    clippy::checked_conversions,
    clippy::debug_assert_with_mut_call,
    clippy::default_trait_access,
    clippy::doc_markdown,
    clippy::enum_glob_use,
    clippy::if_not_else,
    clippy::items_after_statements,
    clippy::manual_assert,
    clippy::map_err_ignore,
    clippy::match_same_arms,
    clippy::match_wildcard_for_single_variants,
    clippy::mem_forget,
    clippy::mut_mut,
    clippy::redundant_closure_for_method_calls,
    clippy::rest_pat_in_fully_bound_structs,
    clippy::semicolon_if_nothing_returned,
    clippy::single_match_else,
    clippy::unnested_or_patterns,
    clippy::unused_self,
    clippy::wildcard_imports
)]

pub mod elf;
#[cfg(target_os = "macos")]
pub mod macho;
#[cfg(target_os = "macos")]
mod simulator;

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// A configuration the scripts run in.
#[derive(Clone)]
struct Target {
    /// The name its results go under.
    label: String,
    /// If set, of the scripts named arch-*, only arch-<arch>-* run.
    arch: Option<String>,
    /// The environment variables the scripts get, each set or removed.
    env: Vec<(&'static str, Option<String>)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    /// The targets the host runs directly, without QEMU or a simulator.
    Native,
    /// The usual targets.
    Default,
    /// Every target there is a toolchain for.
    All,
    /// One cross or simulator target.
    Triple(String),
}

struct Options {
    jobs: usize,
    mode: Mode,
    cpu: Option<String>,
    patterns: Vec<String>,
    timeout: Duration,
    list: bool,
    /// `--list --format terse`: list the tests one per line, as libtest
    /// does for cargo-nextest, instead of counting them.
    terse: bool,
    /// `--ignored`: list, or run, only the tests that are ignored.
    ignored: bool,
    /// `--include-ignored`: run the ignored tests too.
    include_ignored: bool,
    /// `--exact`: the patterns are whole test names, as listed.
    exact: bool,
}

struct TestJob {
    target: Arc<Target>,
    script: PathBuf,
    name: String,
    log: PathBuf,
    status_file: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Pass,
    Skip,
    Fail,
    Timeout,
}

impl Outcome {
    fn status(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Skip => "skip",
            Self::Fail | Self::Timeout => "fail",
        }
    }
}

struct TestResult {
    target: Arc<Target>,
    name: String,
    log: PathBuf,
    outcome: Outcome,
}

#[derive(Default)]
struct Counts {
    pass: usize,
    skip: usize,
    fail: usize,
}

impl Counts {
    fn add(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Pass => self.pass += 1,
            Outcome::Skip => self.skip += 1,
            Outcome::Fail | Outcome::Timeout => self.fail += 1,
        }
    }

    fn merge(&mut self, other: &Self) {
        self.pass += other.pass;
        self.skip += other.skip;
        self.fail += other.fail;
    }
}

fn usage() -> ! {
    eprintln!(
        "Usage: cargo test [pattern] [-- [--test-threads N] \
         [--native | --all | --triple TRIPLE] [--cpu CPU] [--timeout SECONDS] [--list]]\n\
         Test harness protocol (as cargo-nextest uses it): --list --format terse [--ignored] \
         lists the tests, one `<target>::<script>: test` per line, and `<name> --exact` runs \
         one of them.\n\
         By default the ELF tests run for every target whose cross compiler and QEMU are \
         installed, and the Mach-O tests for macOS (arm64, and x86_64 under Rosetta) and the \
         arm64 iOS simulator; --all adds the other simulators. --native runs the targets the \
         host runs directly, --triple one cross or simulator target (e.g. aarch64-linux-gnu \
         or arm64-apple-tvos-simulator)."
    );
    std::process::exit(2);
}

/// Reports an error of the runner itself and exits.
fn fail(err: impl std::fmt::Display) -> ! {
    eprintln!("funis-tests: {err}");
    std::process::exit(1);
}

fn parse_usize(value: Option<String>) -> usize {
    value.and_then(|s| s.parse().ok()).filter(|&n| n != 0).unwrap_or_else(|| usage())
}

fn parse_options() -> Options {
    let mut jobs = thread::available_parallelism().map_or(1, usize::from);
    let mut mode = Mode::Default;
    let mut mode_was_set = false;
    let mut cpu = None;
    let mut patterns = Vec::new();
    let mut timeout = DEFAULT_TIMEOUT;
    let mut list = false;
    let mut terse = false;
    let mut ignored = false;
    let mut include_ignored = false;
    let mut exact = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-j" | "--jobs" | "--test-threads" => jobs = parse_usize(args.next()),
            "--native" => {
                mode = Mode::Native;
                mode_was_set = true;
            }
            "--all" => {
                mode = Mode::All;
                mode_was_set = true;
            }
            "--triple" => {
                mode = Mode::Triple(args.next().unwrap_or_else(|| usage()));
                mode_was_set = true;
            }
            "--cpu" => cpu = Some(args.next().unwrap_or_else(|| usage())),
            "--timeout" => timeout = Duration::from_secs(parse_usize(args.next()) as u64),
            "--list" => list = true,
            "--format" => match args.next().as_deref() {
                Some("terse") => terse = true,
                _ => usage(),
            },
            "--format=terse" => terse = true,
            "--ignored" => ignored = true,
            "--include-ignored" => include_ignored = true,
            "--exact" => exact = true,
            "--nocapture" | "--show-output" | "-q" | "--quiet" => {}
            "-h" | "--help" => usage(),
            _ if arg.starts_with("-j") && arg.len() > 2 => {
                jobs = parse_usize(Some(arg[2..].to_owned()))
            }
            _ if arg.starts_with("--test-threads=") => {
                jobs = parse_usize(arg.split_once('=').map(|(_, value)| value.to_owned()))
            }
            _ if arg.starts_with('-') => usage(),
            _ => patterns.push(arg),
        }
    }

    // TRIPLE and CPU in the environment select a target too.
    let var = |name| env::var_os(name).filter(|s| !s.is_empty());
    if !mode_was_set && let Some(triple) = var("TRIPLE") {
        mode = Mode::Triple(triple.to_string_lossy().into_owned());
    }
    if cpu.is_none() {
        cpu = var("CPU").map(|s| s.to_string_lossy().into_owned());
    }

    Options {
        jobs,
        mode,
        cpu,
        patterns,
        timeout,
        list,
        terse,
        ignored,
        include_ignored,
        exact,
    }
}

fn matches_target(name: &str, target: &Target) -> bool {
    match &target.arch {
        Some(arch) => !name.starts_with("arch-") || name.starts_with(&format!("arch-{arch}-")),
        None => true,
    }
}

fn matches_patterns(name: &str, patterns: &[String]) -> bool {
    patterns.is_empty() || patterns.iter().any(|pattern| name.contains(pattern))
}

/// The scripts in `dir` that the patterns select, by name.
fn selected_scripts(dir: &Path, patterns: &[String]) -> io::Result<Vec<(String, PathBuf)>> {
    let mut scripts = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension() != Some(OsStr::new("sh")) {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        if matches_patterns(&name, patterns) {
            scripts.push((name, path));
        }
    }
    scripts.sort();
    Ok(scripts)
}

fn clear_results(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if matches!(path.extension().and_then(OsStr::to_str), Some("log" | "status")) {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn replace_file_link(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} is a directory", destination.display()),
            ));
        }
        // Already in place. cargo-nextest runs one process per test, many at
        // once, and the scripts of the others run the linker by this name:
        // it must not be taken away under them, even for a moment.
        #[cfg(unix)]
        Ok(metadata)
            if metadata.file_type().is_symlink()
                && fs::read_link(destination).is_ok_and(|target| target == source) =>
        {
            return Ok(());
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    // Made beside the destination under a name of this process's own, then
    // renamed over it, which replaces it in one step.
    let mut temporary = destination.as_os_str().to_owned();
    temporary.push(format!(".{}.tmp", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let _ = fs::remove_file(&temporary);
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, &temporary)?;
    #[cfg(not(unix))]
    fs::copy(source, &temporary)?;
    fs::rename(&temporary, destination)
}

/// Tests run from a directory beside the linker under test and write
/// their outputs under out/test there, so nothing generated lands in the
/// source tree. The directory holds the linker under the names the
/// scripts invoke it by: funis, ld and ld.lld, which the ELF scripts find
/// with ./funis and the compiler's -B. option (upstream LLVM defaults to
/// ld.lld on FreeBSD, so overriding only ld is not enough), and
/// ld64.funis, the name under which funis acts as the Mach-O linker.
fn prepare_work_dir(funis: &Path) -> io::Result<PathBuf> {
    let funis = funis.canonicalize()?;
    let profile_dir = funis.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", funis.display()),
        )
    })?;
    let work_dir = profile_dir.join("funis-test");
    fs::create_dir_all(&work_dir)?;
    for name in ["funis", "ld", "ld.lld", "ld64.funis"] {
        replace_file_link(&funis, &work_dir.join(name))?;
    }
    Ok(work_dir)
}

fn make_jobs(
    scripts: &[(String, PathBuf)],
    work_dir: &Path,
    targets: Vec<Target>,
    clean: bool,
) -> io::Result<Vec<TestJob>> {
    let mut jobs = Vec::new();
    for target in targets {
        let target = Arc::new(target);
        let result_dir = work_dir.join("out/test/results").join(&target.label);
        if clean {
            clear_results(&result_dir)?;
        } else {
            fs::create_dir_all(&result_dir)?;
        }
        for (name, script) in scripts {
            if matches_target(name, &target) {
                jobs.push(TestJob {
                    target: Arc::clone(&target),
                    script: script.clone(),
                    name: name.clone(),
                    log: result_dir.join(format!("{name}.log")),
                    status_file: result_dir.join(format!("{name}.status")),
                });
            }
        }
    }
    Ok(jobs)
}

/// A script that skips itself ends a line of its log with "skipped".
fn log_says_skipped(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| {
        bytes
            .split(|&byte| byte == b'\n')
            .any(|line| line.strip_suffix(b"\r").unwrap_or(line).ends_with(b"skipped"))
    })
}

fn run_process(root: &Path, job: &TestJob, timeout: Duration) -> Result<Outcome, String> {
    let log = File::create(&job.log)
        .map_err(|err| format!("cannot create {}: {err}", job.log.display()))?;
    let stderr =
        log.try_clone().map_err(|err| format!("cannot clone {}: {err}", job.log.display()))?;
    let mut command = Command::new(&job.script);
    command.current_dir(root).stdout(Stdio::from(log)).stderr(Stdio::from(stderr));
    for (name, value) in &job.target.env {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }

    // A timeout must also kill compiler and QEMU children, so a test runs
    // in its own process group. That is a background group of the
    // terminal cargo was started from, so the test must not inherit the
    // terminal as stdin: a program that reads it or restores its settings
    // on exit (lldb does, even in batch mode) is stopped by SIGTTIN or
    // SIGTTOU and hangs until the timeout.
    command.stdin(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    let mut child =
        command.spawn().map_err(|err| format!("cannot run {}: {err}", job.script.display()))?;
    let start = Instant::now();
    loop {
        match child.try_wait().map_err(|err| format!("cannot wait for test: {err}"))? {
            Some(status) => {
                return Ok(if !status.success() {
                    Outcome::Fail
                } else if log_says_skipped(&job.log) {
                    Outcome::Skip
                } else {
                    Outcome::Pass
                });
            }
            None if start.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            None => {
                // SAFETY: kill only signals the test's own process group.
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                #[cfg(not(unix))]
                let _ = child.kill();
                let _ = child.wait();
                return Ok(Outcome::Timeout);
            }
        }
    }
}

fn run_job(root: &Path, job: &TestJob, timeout: Duration) -> TestResult {
    let mut outcome = run_process(root, job, timeout).unwrap_or_else(|err| {
        eprintln!("{}: {err}", job.name);
        Outcome::Fail
    });

    // Keep failed test directories for diagnosis, but do not retain the
    // successful tests' potentially large temporary files.
    if matches!(outcome, Outcome::Pass | Outcome::Skip) {
        let dir = root.join("out/test").join(&job.target.label).join(&job.name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!("{}: cannot remove {}: {err}", job.name, dir.display());
                outcome = Outcome::Fail;
            }
        }
    }

    if let Err(err) = fs::write(&job.status_file, format!("{}\n", outcome.status())) {
        eprintln!("{}: cannot write {}: {err}", job.name, job.status_file.display());
    }
    TestResult {
        target: Arc::clone(&job.target),
        name: job.name.clone(),
        log: job.log.clone(),
        outcome,
    }
}

fn run_jobs(root: &Path, jobs: Vec<TestJob>, options: &Options) -> Vec<TestResult> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    let workers = options.jobs.min(jobs.len());

    thread::scope(|scope| {
        for _ in 0..workers {
            let jobs = &jobs;
            let next = &next;
            let sender = sender.clone();
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        break;
                    };
                    if sender.send(run_job(root, job, options.timeout)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);

        let mut results = Vec::with_capacity(jobs.len());
        while let Ok(result) = receiver.recv() {
            if matches!(result.outcome, Outcome::Fail | Outcome::Timeout) {
                eprintln!(
                    "FAIL {}:{}{} ({})",
                    result.target.label,
                    result.name,
                    if result.outcome == Outcome::Timeout { " [timeout]" } else { "" },
                    result.log.display()
                );
            }
            results.push(result);
            if results.len().is_multiple_of(100) || results.len() == jobs.len() {
                eprintln!("completed {}/{}", results.len(), jobs.len());
            }
        }
        results
    })
}

/// Lists the tests the options select, by target, instead of running
/// them, and the targets left out for want of a toolchain.
fn print_inventory(
    scripts: &[(String, PathBuf)],
    work_dir: &Path,
    targets: Vec<Target>,
    unavailable: &[String],
) -> ExitCode {
    let jobs = make_jobs(scripts, work_dir, targets, false).unwrap_or_else(|err| fail(err));
    let mut counts = BTreeMap::new();
    for job in &jobs {
        *counts.entry(job.target.label.as_str()).or_insert(0usize) += 1;
    }
    for (target, count) in counts {
        println!("{target}: tests={count}");
    }
    println!("total: tests={}", jobs.len());
    if !unavailable.is_empty() {
        println!("unavailable: {}", unavailable.join(", "));
    }
    ExitCode::SUCCESS
}

fn print_summary(results: &[TestResult]) -> ExitCode {
    let mut by_target: BTreeMap<&str, Counts> = BTreeMap::new();
    for result in results {
        by_target.entry(&result.target.label).or_default().add(result.outcome);
    }

    let mut total = Counts::default();
    for (target, counts) in &by_target {
        println!("{target}: pass={} skip={} fail={}", counts.pass, counts.skip, counts.fail);
        total.merge(counts);
    }
    if by_target.len() > 1 {
        println!("total: pass={} skip={} fail={}", total.pass, total.skip, total.fail);
    } else {
        println!("pass={} skip={} fail={}", total.pass, total.skip, total.fail);
    }
    if total.fail == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// A target as the test harness protocol lists it, for cargo-nextest.
struct Listed {
    target: Target,
    /// Its scripts are one test, `<label>::all`, not one test each: a
    /// simulator's scripts run on a device booted once for all of them.
    whole: bool,
    /// Why its tests cannot run on this machine, if they cannot: they are
    /// listed as ignored, and running one says why it cannot run.
    unavailable: Option<String>,
}

impl Listed {
    fn available(target: Target) -> Self {
        Self { target, whole: false, unavailable: None }
    }

    /// The names of its tests, `<target>::<script>`, each with its script.
    fn test_names<'a>(
        &'a self,
        scripts: &'a [(String, PathBuf)],
    ) -> impl Iterator<Item = (String, Option<&'a (String, PathBuf)>)> + 'a {
        let label = &self.target.label;
        let whole = self.whole.then(|| (format!("{label}::all"), None));
        let each = scripts
            .iter()
            .filter(move |(name, _)| !self.whole && matches_target(name, &self.target))
            .map(move |script| (format!("{label}::{}", script.0), Some(script)));
        whole.into_iter().chain(each)
    }
}

/// Lists the tests, or with `--ignored` the ignored ones, one
/// `<name>: test` line each on stdout and nothing else there, as
/// cargo-nextest reads a test harness's list. As with libtest, the full
/// list includes the ignored tests.
fn print_test_list(
    scripts: &[(String, PathBuf)],
    listed: &[Listed],
    ignored_only: bool,
) -> ExitCode {
    for entry in listed {
        if ignored_only && entry.unavailable.is_none() {
            continue;
        }
        for (name, _) in entry.test_names(scripts) {
            println!("{name}: test");
        }
        if ignored_only && let Some(reason) = &entry.unavailable {
            eprintln!("{}: ignored: {reason}", entry.target.label);
        }
    }
    ExitCode::SUCCESS
}

/// Runs the tests named exactly, as cargo-nextest runs each test, with
/// `<name> --exact`. `run_whole` runs the scripts of a target listed as
/// one test, given its index in `listed`.
fn run_exact(
    options: &Options,
    scripts: &[(String, PathBuf)],
    work_dir: &Path,
    listed: &[Listed],
    mut run_whole: impl FnMut(usize) -> ExitCode,
) -> ExitCode {
    let mut status = ExitCode::SUCCESS;
    for wanted in &options.patterns {
        let found = listed.iter().enumerate().find_map(|(index, entry)| {
            entry.test_names(scripts).find(|(name, _)| name == wanted).map(|(_, s)| (index, s))
        });
        let Some((index, script)) = found else {
            eprintln!("funis-tests: no test is named {wanted}");
            status = ExitCode::FAILURE;
            continue;
        };
        let entry = &listed[index];
        if let Some(reason) = &entry.unavailable {
            if options.ignored || options.include_ignored {
                eprintln!("{wanted}: cannot run on this machine: {reason}");
                status = ExitCode::FAILURE;
            } else {
                println!("{wanted}: ignored: {reason}");
            }
            continue;
        }
        if options.ignored {
            // Only the ignored tests were asked for, as with libtest.
            continue;
        }
        let Some(script) = script else {
            if run_whole(index) != ExitCode::SUCCESS {
                status = ExitCode::FAILURE;
            }
            continue;
        };
        let jobs =
            make_jobs(std::slice::from_ref(script), work_dir, vec![entry.target.clone()], false)
                .unwrap_or_else(|err| fail(err));
        for result in run_jobs(work_dir, jobs, options) {
            match result.outcome {
                Outcome::Pass => println!("{wanted}: pass"),
                Outcome::Skip => {
                    // The script found nothing to test on this machine (a
                    // tool it needs is missing); its log says so.
                    let log = fs::read_to_string(&result.log).unwrap_or_default();
                    let why = log.lines().find(|line| {
                        !line.starts_with('+') && line.trim_end().ends_with("skipped")
                    });
                    println!("{wanted}: skipped by the script: {}", why.unwrap_or("").trim());
                }
                Outcome::Fail | Outcome::Timeout => {
                    status = ExitCode::FAILURE;
                    match fs::read_to_string(&result.log) {
                        Ok(log) => println!("{log}"),
                        Err(err) => println!("cannot read {}: {err}", result.log.display()),
                    }
                }
            }
        }
    }
    status
}

/// The harness of a host the scripts cannot run on at all. Its one test,
/// `name`, is listed as ignored, and says why when it is run.
pub fn run_unsupported(name: &str, reason: &str) -> ExitCode {
    let options = parse_options();
    if options.list && options.terse {
        println!("{name}: test");
        return ExitCode::SUCCESS;
    }
    if options.ignored || options.include_ignored {
        eprintln!("{name}: cannot run on this machine: {reason}");
        return ExitCode::FAILURE;
    }
    println!("{name}: ignored: {reason}");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_tests_by_substring() {
        assert!(matches_patterns("dead-strip", &[]));
        assert!(matches_patterns("dead-strip", &["strip".to_owned()]));
        assert!(!matches_patterns("hello", &["strip".to_owned()]));
    }

    #[test]
    fn selects_generic_and_target_tests() {
        let target = Target { label: String::new(), arch: Some("aarch64".to_owned()), env: vec![] };
        assert!(matches_target("gc-sections", &target));
        assert!(matches_target("arch-aarch64-reloc", &target));
        assert!(!matches_target("arch-x86_64-reloc", &target));
        let target = Target { arch: None, ..target };
        assert!(matches_target("arch-x86_64-reloc", &target));
    }
}
