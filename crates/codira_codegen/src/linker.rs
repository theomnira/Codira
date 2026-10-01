//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#[cfg(feature = "in-process-lld")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    borrow::Cow,
    env, fmt,
    path::{Path, PathBuf},
    process::Command,
};

use codira_abi as abi;
use codira_target::{spec, spec::LinkerFlavor};
use thiserror::Error;

use crate::apple::get_apple_sdk_root;

/// How the final link step is carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// Spawn the platform's LLD executable (`lld-link`/`ld.lld`/`ld64.lld`).
    Subprocess,

    /// Call LLD as a library inside this process, through `codira_lld`.
    InProcess,
}

/// Name of the environment variable that overrides the link mode.
pub const LINK_MODE_ENV: &str = "CODIRA_LINK_MODE";

/// Set once LLD reports that it cannot be run in this process again. Every
/// later link spawns the linker instead -- see `link_in_process`.
#[cfg(feature = "in-process-lld")]
static IN_PROCESS_LLD_POISONED: AtomicBool = AtomicBool::new(false);

/// Whether this compiler was built with LLD linked in.
pub const fn in_process_lld_available() -> bool {
    cfg!(feature = "in-process-lld")
}

/// Parses a `CODIRA_LINK_MODE` value. Unrecognized values are `None` so a
/// typo falls back to the default instead of silently picking a mode.
fn parse_link_mode(value: &str) -> Option<LinkMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "in-process" | "in_process" | "inprocess" => Some(LinkMode::InProcess),
        "subprocess" | "spawn" => Some(LinkMode::Subprocess),
        _ => None,
    }
}

/// The link mode in effect: `CODIRA_LINK_MODE` if it names one, otherwise
/// in-process when LLD is linked into this compiler and a spawned linker
/// when it is not.
///
/// Reports what is *requested*. Whether an individual link can honor
/// `InProcess` is decided per call by `run_lld`.
pub fn link_mode() -> LinkMode {
    if let Some(mode) = env::var(LINK_MODE_ENV)
        .ok()
        .as_deref()
        .and_then(parse_link_mode)
    {
        return mode;
    }

    if in_process_lld_available() {
        LinkMode::InProcess
    } else {
        LinkMode::Subprocess
    }
}

/// The object-file format a linker invocation targets, which selects the LLD
/// driver that handles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LldDriver {
    Coff,
    Elf,
    MachO,
}

impl LldDriver {
    /// Suffix of the `CODIRA_LLD_<FLAVOR>` linker-path override.
    fn env_suffix(self) -> &'static str {
        match self {
            LldDriver::Coff => "COFF",
            LldDriver::Elf => "ELF",
            LldDriver::MachO => "MACHO",
        }
    }

    /// Name the linker executable goes by on `PATH`.
    fn default_program(self) -> &'static str {
        match self {
            LldDriver::Coff => "lld-link",
            LldDriver::Elf => "ld.lld",
            LldDriver::MachO => "ld64.lld",
        }
    }
}

/// Links with LLD, either in this process or by spawning the linker.
///
/// # Two ways to run the same linker
///
/// LLD's job is linking standard platform object files (COFF/ELF/Mach-O),
/// which are stable formats not tied to the LLVM version that produced
/// them, so the standalone `lld-link`/`ld.lld`/`ld64.lld` found on `PATH`
/// and the LLD linked into this compiler are interchangeable for
/// correctness. They differ only in what they cost: spawning pays for a
/// process -- creating it, loading the linker's image, tearing it down --
/// to do a few milliseconds of linking for a small module, and calling LLD
/// in-process does not.
///
/// # In-process is faster, cold as well as warm -- measured
///
/// An earlier attempt at this concluded the opposite: linking a
/// one-function program in-process took 147/149/152 ms against 93/97/106 ms
/// for spawning `lld-link`, which was put down to paging LLD in from a much
/// larger image. That diagnosis was wrong. The time was going to
/// `lld-link`'s search for a Visual Studio installation, which an
/// MSVC-built LLD performs through a COM query and the `lld-link` on that
/// machine's `PATH` (a MinGW build, with no such query compiled in) did
/// not. `no_sysroot_arg` removes the search for both.
///
/// With it gone, building a one-function package with `codira build`, 30
/// interleaved runs per mode on a machine under load:
///
/// * whole process, median: 130 ms in-process, 228 ms spawning
/// * of which the link itself: 19-29 ms in-process for the first (and only)
///   link in a cold process
///
/// and in a process that stays alive (`examples/link_timings.rs`), every
/// link after the first is cheaper again. So in-process is the default
/// wherever LLD is linked in, for the one-shot CLI as much as for the
/// daemon.
///
/// # When the linker is spawned regardless
///
/// * `CODIRA_LINK_MODE=subprocess` asks for it.
/// * The compiler was built without the `in-process-lld` feature. Linking LLD
///   in needs its headers and static libraries next to the LLVM build, which
///   only a from-source LLVM built with `-DLLVM_ENABLE_PROJECTS=lld` has; most
///   prebuilt distributions ship just the executables.
/// * `CODIRA_LLD_<FLAVOR>` (`CODIRA_LLD_COFF`, `CODIRA_LLD_ELF`,
///   `CODIRA_LLD_MACHO`) is set. That names a specific linker executable, and
///   someone who asked for a specific linker should get it rather than the one
///   built into the compiler.
/// * LLD reported, after an earlier in-process link, that it cannot run in this
///   process again.
fn run_lld(driver: LldDriver, args: &[String]) -> Result<(), String> {
    let program_override = env::var(format!("CODIRA_LLD_{}", driver.env_suffix())).ok();

    #[cfg(feature = "in-process-lld")]
    if program_override.is_none()
        && link_mode() == LinkMode::InProcess
        && !IN_PROCESS_LLD_POISONED.load(Ordering::SeqCst)
    {
        return link_in_process(driver, args);
    }

    spawn_lld(driver, program_override, args)
}

/// Runs LLD as a library call.
///
/// A failed link is reported as a failure and is *not* retried by spawning
/// the linker: the same inputs produce the same error, and retrying would
/// only double the time it takes to report it.
#[cfg(feature = "in-process-lld")]
fn link_in_process(driver: LldDriver, args: &[String]) -> Result<(), String> {
    let flavor = match driver {
        LldDriver::Coff => codira_lld::LldFlavor::Coff,
        LldDriver::Elf => codira_lld::LldFlavor::Elf,
        LldDriver::MachO => codira_lld::LldFlavor::MachO,
    };

    let result = codira_lld::link(flavor, args);

    // LLD's drivers run on global state. If LLD says that state did not
    // survive this link, running it again here could miscompile silently
    // or crash, so every link from now on goes to a fresh linker process.
    if !result.can_run_again() {
        IN_PROCESS_LLD_POISONED.store(true, Ordering::SeqCst);
    }

    result.ok()
}

/// Spawns the LLD executable for `driver` and waits for it.
fn spawn_lld(
    driver: LldDriver,
    program_override: Option<String>,
    args: &[String],
) -> Result<(), String> {
    let program = program_override.unwrap_or_else(|| driver.default_program().to_owned());

    let output = Command::new(&program).args(args).output().map_err(|e| {
        format!(
            "failed to run linker `{program}`: {e} (set CODIRA_LLD_{} to override the linker \
             path)",
            driver.env_suffix()
        )
    })?;

    let mut messages = String::new();
    messages.push_str(&String::from_utf8_lossy(&output.stderr));
    messages.push_str(&String::from_utf8_lossy(&output.stdout));

    if output.status.success() {
        Ok(())
    } else {
        Err(messages)
    }
}

#[derive(Error, Debug)]
pub enum LinkerError {
    /// Error emitted by the linker
    LinkError(String),

    /// Error in path conversion
    PathError(PathBuf),

    /// Could not locate platform SDK
    PlatformSdkMissing(String),
}

impl fmt::Display for LinkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            LinkerError::LinkError(e) => write!(f, "{e}"),
            LinkerError::PathError(path) => write!(
                f,
                "path contains invalid UTF-8 characters: {}",
                path.display()
            ),
            LinkerError::PlatformSdkMissing(err) => {
                write!(f, "could not find platform sdk: {err}")
            }
        }
    }
}

pub fn create_with_target(target: &spec::Target) -> Box<dyn Linker> {
    match target.options.linker_flavor {
        LinkerFlavor::Ld => Box::new(LdLinker::new(target)),
        LinkerFlavor::Ld64 => Box::new(Ld64Linker::new(target)),
        LinkerFlavor::Msvc => Box::new(MsvcLinker::new(target)),
    }
}

pub trait Linker {
    fn add_object(&mut self, path: &Path) -> Result<(), LinkerError>;

    /// Links the accumulated objects into a shared object at `path`.
    ///
    /// `exported_symbols` names the functions carrying `@export("C")`, which
    /// must be reachable from outside the assembly under their own names
    /// (`spec/LANGUAGE_SPEC.md` section 10).
    ///
    /// Only the COFF linker needs them: ELF and Mach-O export every symbol
    /// with external linkage by default, so setting the linkage in codegen
    /// is sufficient there, while COFF exports *nothing* from a DLL unless
    /// it is named explicitly. That asymmetry is why this is a linker
    /// argument rather than purely an IR attribute.
    fn build_shared_object(
        &mut self,
        path: &Path,
        exported_symbols: &[String],
    ) -> Result<(), LinkerError>;

    fn finalize(&mut self) -> Result<(), LinkerError>;
}

struct LdLinker {
    args: Vec<String>,
}

impl LdLinker {
    fn new(target: &spec::Target) -> Self {
        LdLinker {
            args: target
                .options
                .pre_link_args
                .iter()
                .cloned()
                .map(Cow::into_owned)
                .collect(),
        }
    }
}

impl Linker for LdLinker {
    fn add_object(&mut self, path: &Path) -> Result<(), LinkerError> {
        let path_str = path
            .to_str()
            .ok_or_else(|| LinkerError::PathError(path.to_owned()))?
            .to_owned();
        self.args.push(path_str);
        Ok(())
    }

    fn build_shared_object(
        &mut self,
        path: &Path,
        _exported_symbols: &[String],
    ) -> Result<(), LinkerError> {
        let path_str = path
            .to_str()
            .ok_or_else(|| LinkerError::PathError(path.to_owned()))?;

        // Link as dynamic library
        self.args.push("--shared".to_owned());

        // Specify output path
        self.args.push("-o".to_owned());
        self.args.push(path_str.to_owned());

        Ok(())
    }

    fn finalize(&mut self) -> Result<(), LinkerError> {
        run_lld(LldDriver::Elf, &self.args).map_err(LinkerError::LinkError)
    }
}

struct Ld64Linker {
    args: Vec<String>,
    target: spec::Target,
}

impl Ld64Linker {
    fn new(target: &spec::Target) -> Self {
        let args = target
            .options
            .pre_link_args
            .iter()
            .cloned()
            .map(Cow::into_owned)
            .collect();

        Ld64Linker {
            args,
            target: target.clone(),
        }
    }

    fn add_apple_sdk(&mut self) -> Result<(), LinkerError> {
        let arch = &self.target.arch;
        let os = &self.target.options.os;
        let llvm_target = &self.target.llvm_target;

        let sdk_name = match (arch.as_ref(), os.as_ref()) {
            ("aarch64", "tvos") => "appletvos",
            ("x86_64", "tvos") => "appletvsimulator",
            ("aarch64" | "x86_64", "ios") if llvm_target.contains("macabi") => "macosx",
            ("aarch64", "ios") if llvm_target.ends_with("-simulator") => "iphonesimulator",
            ("arm" | "aarch64", "ios") => "iphoneos",
            ("x86" | "x86_64", "ios") => "iphonesimulator",
            ("aarch64", "watchos") if llvm_target.ends_with("-simulator") => "watchsimulator",
            ("x86_64", "watchos") => "watchsimulator",
            ("aarch64" | "arm" | "arm64_32", "watchos") => "watchos",
            (_, "macos") => "macosx",
            _ => {
                return Err(LinkerError::PlatformSdkMissing(format!(
                    "unsupported arch `{arch}` for os `{os}`"
                )));
            }
        };

        let sdk_root = get_apple_sdk_root(sdk_name).map_err(LinkerError::PlatformSdkMissing)?;
        self.args.push(String::from("-syslibroot"));
        self.args.push(format!("{}", sdk_root.display()));
        Ok(())
    }
}

impl Linker for Ld64Linker {
    fn add_object(&mut self, path: &Path) -> Result<(), LinkerError> {
        let path_str = path
            .to_str()
            .ok_or_else(|| LinkerError::PathError(path.to_owned()))?
            .to_owned();
        self.args.push(path_str);
        Ok(())
    }

    fn build_shared_object(
        &mut self,
        path: &Path,
        _exported_symbols: &[String],
    ) -> Result<(), LinkerError> {
        let path_str = path
            .to_str()
            .ok_or_else(|| LinkerError::PathError(path.to_owned()))?;

        let filename_str = path
            .file_name()
            .expect("path must have a filename")
            .to_str()
            .ok_or_else(|| LinkerError::PathError(path.to_owned()))?;

        // Link as dynamic library
        self.args.push("-dylib".to_owned());

        self.add_apple_sdk()?;
        self.args.push("-lSystem".to_owned());

        // Specify output path
        self.args.push("-o".to_owned());
        self.args.push(path_str.to_owned());

        // Ensure that the `install_name` is not a full path as it is used as a unique
        // identifier on MacOS
        self.args.push("-install_name".to_owned());
        self.args.push(filename_str.to_owned());

        Ok(())
    }

    fn finalize(&mut self) -> Result<(), LinkerError> {
        run_lld(LldDriver::MachO, &self.args).map_err(LinkerError::LinkError)
    }
}

/// Builds the `/winsysroot` argument that stops `lld-link` from searching the
/// machine for a Visual Studio installation.
///
/// Without it, an MSVC-built LLD goes looking for the VC toolchain and the
/// Windows SDK on every single link, so that it knows where `libcmt.lib` and
/// friends live: first the environment, then Visual Studio's COM setup API,
/// then the registry. Off a developer command prompt that reaches the COM
/// query, which measured at 130-270 ms for the first link in a process and
/// roughly 20 ms for each one after it -- more than linking a small module
/// takes, spent finding libraries that are never used.
///
/// They are never used because a Codira assembly links against nothing: no
/// library is named on the command line, and codegen emits no `/DEFAULTLIB`
/// directives into the object. LLD only consults its search paths to resolve
/// a library name, so with no names to resolve the paths are dead weight and
/// dropping them cannot change what is linked.
///
/// `/winsysroot` is LLD's own switch for this: given a sysroot it trusts the
/// value outright, "to prevent unnecessary file and registry access" as its
/// source puts it. The directory named here is a sibling of the output that
/// is never created -- there is no sysroot, and a path that does not exist
/// says so without sending LLD off to list a real directory.
fn no_sysroot_arg(output_path: &Path) -> String {
    format!(
        "/winsysroot:{}",
        output_path.with_extension("no-sysroot").display()
    )
}

struct MsvcLinker {
    args: Vec<String>,
}

impl MsvcLinker {
    fn new(target: &spec::Target) -> Self {
        MsvcLinker {
            args: target
                .options
                .pre_link_args
                .iter()
                .cloned()
                .map(Cow::into_owned)
                .collect(),
        }
    }
}

impl Linker for MsvcLinker {
    fn add_object(&mut self, path: &Path) -> Result<(), LinkerError> {
        let path_str = path
            .to_str()
            .ok_or_else(|| LinkerError::PathError(path.to_owned()))?
            .to_owned();
        self.args.push(path_str);
        Ok(())
    }

    fn build_shared_object(
        &mut self,
        path: &Path,
        exported_symbols: &[String],
    ) -> Result<(), LinkerError> {
        let dll_path_str = path
            .to_str()
            .ok_or_else(|| LinkerError::PathError(path.to_owned()))?;

        self.args.push("/DLL".to_owned());
        self.args.push("/NOENTRY".to_owned());
        self.args.push(no_sysroot_arg(path));
        self.args.push(format!("/EXPORT:{}", abi::GET_INFO_FN_NAME));
        self.args
            .push(format!("/EXPORT:{}", abi::GET_VERSION_FN_NAME));
        self.args
            .push(format!("/EXPORT:{}", abi::SET_ALLOCATOR_HANDLE_FN_NAME));
        // COFF exports nothing from a DLL unless it is named, so every
        // `@export("C")` function needs its own entry here -- external
        // linkage alone is not enough on this platform.
        for symbol in exported_symbols {
            self.args.push(format!("/EXPORT:{symbol}"));
        }
        // No import library. A `.codiralib` is loaded by path at run time
        // and nothing links against it, so there is no consumer for one --
        // and the one this used to request was written to the very path
        // of the DLL (`/IMPLIB:` and `/OUT:` named the same file), where
        // the DLL then overwrote it. That was a file written purely to be
        // destroyed, at about a millisecond per link.
        self.args.push("/NOIMPLIB".to_owned());
        self.args.push(format!("/OUT:{dll_path_str}"));
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), LinkerError> {
        run_lld(LldDriver::Coff, &self.args).map_err(LinkerError::LinkError)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_link_mode, LinkMode, LldDriver};

    #[test]
    fn link_mode_names_are_recognized() {
        assert_eq!(parse_link_mode("in-process"), Some(LinkMode::InProcess));
        assert_eq!(parse_link_mode("IN_PROCESS"), Some(LinkMode::InProcess));
        assert_eq!(parse_link_mode(" inprocess "), Some(LinkMode::InProcess));
        assert_eq!(parse_link_mode("subprocess"), Some(LinkMode::Subprocess));
        assert_eq!(parse_link_mode("spawn"), Some(LinkMode::Subprocess));
    }

    #[test]
    fn unknown_link_mode_is_not_guessed() {
        assert_eq!(parse_link_mode(""), None);
        assert_eq!(parse_link_mode("fast"), None);
        assert_eq!(parse_link_mode("in process"), None);
    }

    #[test]
    fn every_driver_has_its_own_override_and_program() {
        let drivers = [LldDriver::Coff, LldDriver::Elf, LldDriver::MachO];
        for (i, a) in drivers.iter().enumerate() {
            for b in &drivers[i + 1..] {
                assert_ne!(a.env_suffix(), b.env_suffix());
                assert_ne!(a.default_program(), b.default_program());
            }
        }
    }
}
