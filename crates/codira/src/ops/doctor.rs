//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - `codira doctor`: checks the toolchain this compiler runs with and reports,
//!   for each problem, the exact fix.
//!
//! What it checks, and how:
//!
//! - LLVM: the version of the LLVM library linked into this compiler, asked of
//!   the library itself (`LLVMGetVersion`). 22 is required;
//! - the target: that it is a supported triple and that the linked LLVM has a
//!   backend for it (`LLVMGetTargetFromTriple`);
//! - Z3: the version of the linked library (`Z3_get_full_version`);
//! - LLD: linked into the compiler from the same LLVM, so present whenever the
//!   compiler runs; reported with the LLVM version;
//! - the C compiler `[native]` sources are built with: runs it with
//!   `--version`. A problem only for a package that has `[native]` sources;
//! - the standard library: the directory the compiler would use, and why it
//!   finds none;
//! - in a checkout of the compiler's own sources, what a contributor needs:
//!   `llvm-config` for LLVM 22 where the build looks for it, and nightly
//!   rustfmt (CI checks formatting with it);
//! - free disk space where builds are written.

use std::{
    ffi::CStr,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{ops::build::find_manifest, ExitStatus};

/// The LLVM major version this compiler is built for.
const LLVM_MAJOR: u32 = 22;

#[derive(clap::Args)]
pub struct Args {
    /// Check this target triple instead of the host's
    #[clap(long)]
    target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub(crate) struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    /// What to do about it; empty when the check passed.
    pub fix: Vec<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Check {
        Check {
            name,
            status: Status::Ok,
            detail: detail.into(),
            fix: Vec::new(),
        }
    }

    fn problem(
        name: &'static str,
        status: Status,
        detail: impl Into<String>,
        fix: &[String],
    ) -> Check {
        Check {
            name,
            status,
            detail: detail.into(),
            fix: fix.to_vec(),
        }
    }
}

pub fn doctor(args: Args) -> anyhow::Result<ExitStatus> {
    let cwd = std::env::current_dir()?;
    let manifest = find_manifest(&cwd);
    let has_native = manifest
        .as_ref()
        .and_then(|m| codira_project::Package::from_file(m).ok())
        .is_some_and(|p| !p.manifest().native().is_empty());
    let checkout = find_compiler_checkout(&cwd);

    let mut checks = vec![
        check_llvm(linked_llvm_version()),
        check_target(args.target.as_deref()),
        check_z3(&codira_smt::z3_version()),
        Check::ok(
            "LLD",
            format!(
                "linked into the compiler (from LLVM {})",
                version_text(linked_llvm_version())
            ),
        ),
        check_c_compiler(&codira_compiler::find_clang(), has_native),
        check_std(codira_compiler::std_library_dir()),
    ];
    if checkout.is_some() {
        checks.push(check_llvm_config());
        checks.push(check_rustfmt());
    }
    let build_dir = manifest
        .as_ref()
        .and_then(|m| m.parent())
        .map(Path::to_path_buf)
        .or_else(|| checkout.clone())
        .unwrap_or_else(|| cwd.clone());
    checks.push(check_disk(&build_dir, checkout.is_some()));

    print_checks(&checks);
    Ok(ExitStatus::from(
        checks.iter().all(|c| c.status != Status::Fail),
    ))
}

fn print_checks(checks: &[Check]) {
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for check in checks {
        let tag = match check.status {
            Status::Ok => "ok  ",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        };
        println!("[{tag}] {:width$}  {}", check.name, check.detail);
        for line in &check.fix {
            println!("       {:width$}  fix: {line}", "");
        }
    }
    let fails = checks.iter().filter(|c| c.status == Status::Fail).count();
    let warns = checks.iter().filter(|c| c.status == Status::Warn).count();
    println!(
        "\n{} check(s): {fails} failed, {warns} warning(s)",
        checks.len()
    );
}

fn version_text((major, minor, patch): (u32, u32, u32)) -> String {
    format!("{major}.{minor}.{patch}")
}

/// The version of the LLVM library linked into this binary.
fn linked_llvm_version() -> (u32, u32, u32) {
    let (mut major, mut minor, mut patch) = (0, 0, 0);
    // Safety: writes three integers.
    unsafe { llvm_sys::core::LLVMGetVersion(&mut major, &mut minor, &mut patch) };
    (major, minor, patch)
}

fn install_llvm_fix() -> Vec<String> {
    match std::env::consts::OS {
        "linux" => vec![
            "add apt.llvm.org's llvm-toolchain-<release>-22 repository, then: \
             apt-get install llvm-22-dev libpolly-22-dev clang-22 lld-22 liblld-22-dev libzstd-dev"
                .to_string(),
            "export LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 and rebuild the compiler".to_string(),
        ],
        "macos" => vec![
            "brew install llvm@22 lld@22".to_string(),
            "export LLVM_SYS_221_PREFIX=$(brew --prefix llvm@22) and rebuild the compiler"
                .to_string(),
        ],
        _ => vec![
            "install an LLVM 22 build with its development libraries and lld".to_string(),
            "set LLVM_SYS_221_PREFIX to its directory and rebuild the compiler".to_string(),
        ],
    }
}

pub(crate) fn check_llvm(version: (u32, u32, u32)) -> Check {
    if version.0 == LLVM_MAJOR {
        Check::ok("LLVM", format!("{} (linked)", version_text(version)))
    } else {
        Check::problem(
            "LLVM",
            Status::Fail,
            format!(
                "the compiler is linked against LLVM {}, but LLVM {LLVM_MAJOR} is required",
                version_text(version)
            ),
            &install_llvm_fix(),
        )
    }
}

fn check_target(triple: Option<&str>) -> Check {
    let host = codira_target::spec::Target::host_target();
    let (triple, target) = match triple {
        Some(t) => (t.to_string(), codira_target::spec::Target::search(t)),
        None => (
            host.as_ref()
                .map(|t| t.llvm_target.to_string())
                .unwrap_or_else(|| "<unknown host>".to_string()),
            host,
        ),
    };
    let supported: Vec<&str> = codira_target::spec::get_targets().collect();
    let Some(target) = target else {
        return Check::problem(
            "target",
            Status::Fail,
            format!("'{triple}' is not a supported target"),
            &[format!(
                "use one of: {} (codira build --target <triple>)",
                supported.join(", ")
            )],
        );
    };
    match llvm_has_backend(&target.llvm_target) {
        Ok(()) => Check::ok(
            "target",
            format!("{} (LLVM backend present)", target.llvm_target),
        ),
        Err(reason) => Check::problem(
            "target",
            Status::Fail,
            format!(
                "the linked LLVM has no backend for {}: {reason}",
                target.llvm_target
            ),
            &[format!(
                "rebuild LLVM {LLVM_MAJOR} with this target in LLVM_TARGETS_TO_BUILD, or use a \
                 distribution build of LLVM {LLVM_MAJOR} (they include every stable backend)"
            )],
        ),
    }
}

fn llvm_has_backend(triple: &str) -> Result<(), String> {
    let triple = std::ffi::CString::new(triple).map_err(|e| e.to_string())?;
    // Safety: plain LLVM C API calls; the error message is freed with
    // LLVMDisposeMessage.
    unsafe {
        llvm_sys::target::LLVM_InitializeAllTargetInfos();
        let mut target = std::ptr::null_mut();
        let mut error = std::ptr::null_mut();
        if llvm_sys::target_machine::LLVMGetTargetFromTriple(
            triple.as_ptr(),
            &mut target,
            &mut error,
        ) == 0
        {
            return Ok(());
        }
        let message = if error.is_null() {
            "unknown target".to_string()
        } else {
            let text = CStr::from_ptr(error).to_string_lossy().into_owned();
            llvm_sys::core::LLVMDisposeMessage(error);
            text
        };
        Err(message)
    }
}

/// `Z3 4.13.0.0` -> (4, 13).
pub(crate) fn parse_z3_version(text: &str) -> Option<(u32, u32)> {
    let version = text
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut parts = version.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

pub(crate) fn check_z3(version: &str) -> Check {
    match parse_z3_version(version) {
        // Contracts, `@rewrite` proofs and the superoptimiser use Z3 4.8+
        // features (the incremental solver API and bit-vector tactics).
        Some((major, minor)) if (major, minor) >= (4, 8) => {
            Check::ok("Z3", format!("{version} (linked)"))
        }
        _ => Check::problem(
            "Z3",
            Status::Fail,
            format!("the linked Z3 reports '{version}'; 4.8 or later is required"),
            &[match std::env::consts::OS {
                "linux" => "apt-get install libz3-dev, then rebuild the compiler".to_string(),
                "macos" => "brew install z3, then rebuild the compiler".to_string(),
                "windows" => "choco install z3, then rebuild the compiler".to_string(),
                _ => "install Z3 4.8 or later with its library, then rebuild the compiler"
                    .to_string(),
            }],
        ),
    }
}

/// `clang version 22.1.8 (...)` / `Apple clang version 17.0.0` -> 22 / 17.
pub(crate) fn parse_clang_major(text: &str) -> Option<u32> {
    let after = text.split("clang version ").nth(1)?;
    after.split('.').next()?.trim().parse().ok()
}

fn check_c_compiler(clang: &Path, needed: bool) -> Check {
    let output = Command::new(clang).arg("--version").output();
    let fix = |what: &str| -> Vec<String> {
        let mut fix = vec![match std::env::consts::OS {
            "linux" => format!("{what}: apt-get install clang-{LLVM_MAJOR}"),
            "macos" => {
                format!("{what}: xcode-select --install (or brew install llvm@{LLVM_MAJOR})")
            }
            "windows" => format!("{what}: winget install LLVM.LLVM"),
            _ => format!("{what}: install clang"),
        }];
        fix.push("or point CODIRA_CLANG at a clang binary".to_string());
        fix
    };
    let status = if needed { Status::Fail } else { Status::Warn };
    let scope = if needed {
        "this package has [native] sources and needs it"
    } else {
        "only packages with [native] sources need it"
    };
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let first = text.lines().next().unwrap_or("").trim().to_string();
            match parse_clang_major(&text) {
                // `[native]` C and C++ are compiled to LLVM bitcode and
                // linked into the module: LLVM reads bitcode from older
                // releases, not from newer ones.
                Some(major) if major > LLVM_MAJOR => Check::problem(
                    "C compiler",
                    status,
                    format!(
                        "{} is clang {major}, newer than this compiler's LLVM {LLVM_MAJOR}: its \
                         bitcode cannot be read; {scope}",
                        clang.display()
                    ),
                    &fix(&format!("install clang {LLVM_MAJOR}")),
                ),
                Some(_) => Check::ok("C compiler", format!("{} ({first})", clang.display())),
                None => Check::problem(
                    "C compiler",
                    status,
                    format!("{} is not clang ({first}); {scope}", clang.display()),
                    &fix("install clang"),
                ),
            }
        }
        Ok(out) => Check::problem(
            "C compiler",
            status,
            format!(
                "{} --version failed ({}); {scope}",
                clang.display(),
                out.status
            ),
            &fix("install clang"),
        ),
        Err(e) => Check::problem(
            "C compiler",
            status,
            format!("{} could not be run ({e}); {scope}", clang.display()),
            &fix("install clang"),
        ),
    }
}

fn check_std(dir: Result<PathBuf, String>) -> Check {
    match dir {
        Ok(dir) => {
            let source = if std::env::var_os(codira_compiler::STD_PATH_ENV).is_some() {
                format!(" (from {})", codira_compiler::STD_PATH_ENV)
            } else {
                String::new()
            };
            Check::ok("std library", format!("{}{source}", dir.display()))
        }
        Err(reason) => Check::problem(
            "std library",
            Status::Fail,
            reason,
            &[format!(
                "set {}=<dir>, where <dir> is the std/ directory of a Codira installation or \
                 source checkout (it contains prelude.code)",
                codira_compiler::STD_PATH_ENV
            )],
        ),
    }
}

/// The root of a checkout of the compiler's sources containing `dir`.
fn find_compiler_checkout(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|d| {
            d.join("crates/codira_compiler/Cargo.toml").is_file() && d.join("Cargo.toml").is_file()
        })
        .map(Path::to_path_buf)
}

/// `LLVM version 22.1.8` or `22.1.8` -> 22.
pub(crate) fn parse_llvm_config_major(text: &str) -> Option<u32> {
    text.split_whitespace()
        .last()?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn check_llvm_config() -> Check {
    let mut candidates = Vec::new();
    if let Some(prefix) = std::env::var_os("LLVM_SYS_221_PREFIX") {
        candidates.push(
            PathBuf::from(prefix)
                .join("bin")
                .join(format!("llvm-config{}", std::env::consts::EXE_SUFFIX)),
        );
    }
    candidates.push(PathBuf::from(format!("llvm-config-{LLVM_MAJOR}")));
    candidates.push(PathBuf::from("llvm-config"));
    let mut seen = Vec::new();
    for candidate in &candidates {
        let Ok(out) = Command::new(candidate).arg("--version").output() else {
            continue;
        };
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        match parse_llvm_config_major(&text) {
            Some(LLVM_MAJOR) => {
                let note = if std::env::var_os("LLVM_SYS_221_PREFIX").is_some() {
                    ""
                } else {
                    "; LLVM_SYS_221_PREFIX is not set, so the build searches PATH"
                };
                return Check::ok(
                    "build: llvm-config",
                    format!("{} reports {text}{note}", candidate.display()),
                );
            }
            _ => seen.push(format!("{} reports {text}", candidate.display())),
        }
    }
    let detail = if seen.is_empty() {
        format!("no llvm-config for LLVM {LLVM_MAJOR}: building the compiler will fail")
    } else {
        format!(
            "no llvm-config for LLVM {LLVM_MAJOR} ({}): building the compiler will fail",
            seen.join("; ")
        )
    };
    Check::problem(
        "build: llvm-config",
        Status::Fail,
        detail,
        &install_llvm_fix(),
    )
}

fn check_rustfmt() -> Check {
    let out = Command::new("rustup")
        .args(["run", "nightly", "rustfmt", "--version"])
        .output();
    match out {
        Ok(out) if out.status.success() => Check::ok(
            "build: nightly rustfmt",
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ),
        _ => Check::problem(
            "build: nightly rustfmt",
            Status::Warn,
            "not installed; CI checks formatting with nightly rustfmt (rustfmt.toml uses \
             unstable options)",
            &[
                "rustup toolchain install nightly --profile minimal --component rustfmt"
                    .to_string(),
            ],
        ),
    }
}

/// Free bytes on the file system holding `path`.
fn free_bytes(path: &Path) -> Result<u64, String> {
    if cfg!(windows) {
        let script = format!(
            "(Get-Item -LiteralPath '{}').PSDrive.Free",
            path.display().to_string().replace('\'', "''")
        );
        let out = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .map_err(|e| e.to_string())?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .map_err(|_e| "powershell did not report the free space".to_string())
    } else {
        let out = Command::new("df")
            .arg("-Pk")
            .arg(path)
            .output()
            .map_err(|e| e.to_string())?;
        parse_df(&String::from_utf8_lossy(&out.stdout))
            .ok_or_else(|| "df did not report the free space".to_string())
    }
}

/// The available bytes from `df -Pk` output.
pub(crate) fn parse_df(text: &str) -> Option<u64> {
    let line = text.lines().nth(1)?;
    let available: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(available * 1024)
}

pub(crate) fn disk_check(free: u64, path: &Path, contributor: bool) -> Check {
    const GB: u64 = 1 << 30;
    // A debug build of the compiler takes 10-20 GB; building a package
    // needs far less.
    let (warn_below, fail_below) = if contributor {
        (20 * GB, 5 * GB)
    } else {
        (2 * GB, GB / 2)
    };
    let gb = free as f64 / GB as f64;
    let detail = format!("{gb:.1} GB free at {}", path.display());
    let status = if free < fail_below {
        Status::Fail
    } else if free < warn_below {
        Status::Warn
    } else {
        return Check::ok("disk space", detail);
    };
    let mut fix = vec![format!(
        "free at least {:.0} GB: remove build output you do not need (target/ directories)",
        (warn_below - free.min(warn_below)) as f64 / GB as f64 + 0.5
    )];
    if contributor {
        fix.push(
            "build with CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 to roughly halve a debug \
             build"
                .to_string(),
        );
    }
    Check::problem("disk space", status, detail, &fix)
}

fn check_disk(path: &Path, contributor: bool) -> Check {
    match free_bytes(path) {
        Ok(free) => disk_check(free, path, contributor),
        Err(reason) => Check::problem(
            "disk space",
            Status::Warn,
            format!(
                "could not measure the free space at {}: {reason}",
                path.display()
            ),
            &["check it by hand: builds need 2 GB, a compiler build 20 GB".to_string()],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llvm_version_must_be_22() {
        assert_eq!(check_llvm((22, 1, 8)).status, Status::Ok);
        let old = check_llvm((21, 1, 0));
        assert_eq!(old.status, Status::Fail);
        assert!(old.fix.iter().any(|f| f.contains("LLVM_SYS_221_PREFIX")));
    }

    #[test]
    fn z3_versions() {
        assert_eq!(parse_z3_version("Z3 4.13.0.0"), Some((4, 13)));
        assert_eq!(parse_z3_version("4.8.12"), Some((4, 8)));
        assert_eq!(parse_z3_version(""), None);
        assert_eq!(check_z3("Z3 4.13.0.0").status, Status::Ok);
        assert_eq!(check_z3("Z3 4.4.1").status, Status::Fail);
        assert_eq!(check_z3("").status, Status::Fail);
    }

    #[test]
    fn clang_and_llvm_config_versions() {
        assert_eq!(
            parse_clang_major("Ubuntu clang version 22.1.8 (++2026)\nTarget: x"),
            Some(22)
        );
        assert_eq!(
            parse_clang_major("Apple clang version 17.0.0 (clang-1700)"),
            Some(17)
        );
        assert_eq!(parse_clang_major("gcc (GCC) 13.2.0"), None);
        assert_eq!(parse_llvm_config_major("22.1.8"), Some(22));
        assert_eq!(parse_llvm_config_major("LLVM version 21.0.0git"), Some(21));
    }

    #[test]
    fn df_output() {
        let text = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda 263174212 9600000 31457280 24% /\n";
        assert_eq!(parse_df(text), Some(31457280 * 1024));
        assert_eq!(parse_df("garbage"), None);
    }

    #[test]
    fn disk_thresholds() {
        const GB: u64 = 1 << 30;
        let p = Path::new("/x");
        assert_eq!(disk_check(30 * GB, p, true).status, Status::Ok);
        assert_eq!(disk_check(10 * GB, p, true).status, Status::Warn);
        assert_eq!(disk_check(GB, p, true).status, Status::Fail);
        assert_eq!(disk_check(10 * GB, p, false).status, Status::Ok);
        assert_eq!(disk_check(GB, p, false).status, Status::Warn);
        let low = disk_check(GB / 4, p, false);
        assert_eq!(low.status, Status::Fail);
        assert!(!low.fix.is_empty());
    }

    #[test]
    fn the_linked_toolchain_passes() {
        // This test binary links the same LLVM and Z3 as the compiler.
        assert_eq!(check_llvm(linked_llvm_version()).status, Status::Ok);
        assert_eq!(check_z3(&codira_smt::z3_version()).status, Status::Ok);
        assert_eq!(check_target(None).status, Status::Ok);
        let bad = check_target(Some("mips-unknown-nowhere"));
        assert_eq!(bad.status, Status::Fail);
        assert!(bad.fix[0].contains("x86_64-unknown-linux-gnu"));
    }
}
