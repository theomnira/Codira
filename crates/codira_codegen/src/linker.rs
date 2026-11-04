//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#[cfg(all(feature = "in-process-lld", windows))]
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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

    /// Write the image with `codira_image` when it can be certain of the
    /// result (a `.codiralib` for `x86_64-pc-windows-msvc`), and link with
    /// LLD, in-process where it is linked in, for everything it declines.
    Direct,
}

/// Name of the environment variable that overrides the link mode.
pub const LINK_MODE_ENV: &str = "CODIRA_LINK_MODE";

/// Set to `0` to keep the direct image writer out even in `Direct` mode;
/// the escape hatch that forces LLD without naming how to run it.
pub const DIRECT_LINK_ENV: &str = "CODIRA_DIRECT_LINK";

/// Set once LLD reports that it cannot be run in this process again. Every
/// later link spawns the linker instead -- see `link_in_process`.
#[cfg(all(feature = "in-process-lld", windows))]
static IN_PROCESS_LLD_POISONED: AtomicBool = AtomicBool::new(false);

/// The one LLD driver this process links with in-process, once it has
/// linked anything; `NO_DRIVER` until then.
///
/// LLD's drivers each keep global state, and `lldMain` resetting it after a
/// link is only good enough for the *same* driver to run again. Linking a
/// COFF DLL and then an ELF shared object in one process corrupts the heap
/// (`STATUS_HEAP_CORRUPTION`, found by `test_targets`, which builds for
/// every target in one test). The build daemon serves `--target` requests
/// in one long-lived process, so a Windows build followed by a Linux
/// cross-build could take it down. The first driver used in-process claims
/// the process; any other driver is spawned as its own linker process.
#[cfg(all(feature = "in-process-lld", windows))]
static IN_PROCESS_LLD_DRIVER: AtomicU8 = AtomicU8::new(NO_DRIVER);

#[cfg(all(feature = "in-process-lld", windows))]
const NO_DRIVER: u8 = u8::MAX;

/// Claims in-process linking for `driver`, unless another driver has it.
#[cfg(all(feature = "in-process-lld", windows))]
fn claim_in_process_driver(driver: LldDriver) -> bool {
    let wanted = driver as u8;
    match IN_PROCESS_LLD_DRIVER.compare_exchange(
        NO_DRIVER,
        wanted,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) => true,
        Err(current) => current == wanted,
    }
}

/// Whether this compiler was built with LLD linked in.
///
/// That takes the `in-process-lld` feature *and* a Windows host; see the
/// feature's comment in `Cargo.toml` for why other hosts are left out.
pub const fn in_process_lld_available() -> bool {
    cfg!(all(feature = "in-process-lld", windows))
}

/// Parses a `CODIRA_LINK_MODE` value. Unrecognized values are `None` so a
/// typo falls back to the default instead of silently picking a mode.
fn parse_link_mode(value: &str) -> Option<LinkMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "in-process" | "in_process" | "inprocess" => Some(LinkMode::InProcess),
        "subprocess" | "spawn" => Some(LinkMode::Subprocess),
        "direct" | "image" => Some(LinkMode::Direct),
        _ => None,
    }
}

/// The way LLD is run when nothing else is asked for: in-process when it is
/// linked into this compiler and a spawned linker when it is not.
fn default_lld_mode() -> LinkMode {
    if in_process_lld_available() {
        LinkMode::InProcess
    } else {
        LinkMode::Subprocess
    }
}

/// The link mode requested: `CODIRA_LINK_MODE` if it names one; otherwise
/// the direct image writer on Windows hosts (which declines, and so links
/// with LLD, whatever it does not model), and LLD -- in-process when it is
/// linked into this compiler, a spawned linker when it is not -- elsewhere.
///
/// `CODIRA_LINK_MODE=in-process` or `subprocess` is the escape hatch that
/// forces LLD for everything. Whether an individual link can honor
/// `InProcess` is decided per call by `run_lld`.
pub fn link_mode() -> LinkMode {
    if let Some(mode) = env::var(LINK_MODE_ENV)
        .ok()
        .as_deref()
        .and_then(parse_link_mode)
    {
        return mode;
    }

    if cfg!(windows) {
        LinkMode::Direct
    } else {
        default_lld_mode()
    }
}

/// How LLD is run when it is run: the requested mode, except that `Direct`
/// leaves the choice between in-process and spawned to the build. Only the
/// in-process path (Windows) asks.
#[cfg(all(feature = "in-process-lld", windows))]
fn lld_mode() -> LinkMode {
    match link_mode() {
        LinkMode::Direct => default_lld_mode(),
        mode => mode,
    }
}

/// Whether `codira_image` should be tried for the targets it supports: in
/// `Direct` mode, unless `CODIRA_DIRECT_LINK` is `0`/`off`/`false`.
pub(crate) fn direct_image_requested() -> bool {
    if matches!(
        env::var(DIRECT_LINK_ENV).as_deref().map(str::trim),
        Ok("0" | "off" | "false")
    ) {
        return false;
    }
    link_mode() == LinkMode::Direct
}

/// The object-file format a linker invocation targets, which selects the LLD
/// driver that handles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LldDriver {
    Coff,
    Elf,
    MachO,
    Wasm,
}

impl LldDriver {
    /// Suffix of the `CODIRA_LLD_<FLAVOR>` linker-path override.
    fn env_suffix(self) -> &'static str {
        match self {
            LldDriver::Coff => "COFF",
            LldDriver::Elf => "ELF",
            LldDriver::MachO => "MACHO",
            LldDriver::Wasm => "WASM",
        }
    }

    /// Name the linker executable goes by on `PATH`.
    fn default_program(self) -> &'static str {
        match self {
            LldDriver::Coff => "lld-link",
            LldDriver::Elf => "ld.lld",
            LldDriver::MachO => "ld64.lld",
            LldDriver::Wasm => "wasm-ld",
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
/// * The compiler runs on anything but Windows, the one host in-process linking
///   has been verified on.
/// * `CODIRA_LLD_<FLAVOR>` (`CODIRA_LLD_COFF`, `CODIRA_LLD_ELF`,
///   `CODIRA_LLD_MACHO`) is set. That names a specific linker executable, and
///   someone who asked for a specific linker should get it rather than the one
///   built into the compiler.
/// * LLD reported, after an earlier in-process link, that it cannot run in this
///   process again.
/// * This process already linked in-process with a different LLD driver (see
///   `IN_PROCESS_LLD_DRIVER`).
fn run_lld(driver: LldDriver, args: &[String]) -> Result<(), String> {
    let program_override = env::var(format!("CODIRA_LLD_{}", driver.env_suffix())).ok();

    #[cfg(all(feature = "in-process-lld", windows))]
    if program_override.is_none()
        && lld_mode() == LinkMode::InProcess
        && !IN_PROCESS_LLD_POISONED.load(Ordering::SeqCst)
        && claim_in_process_driver(driver)
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
#[cfg(all(feature = "in-process-lld", windows))]
fn link_in_process(driver: LldDriver, args: &[String]) -> Result<(), String> {
    let flavor = match driver {
        LldDriver::Coff => codira_lld::LldFlavor::Coff,
        LldDriver::Elf => codira_lld::LldFlavor::Elf,
        LldDriver::MachO => codira_lld::LldFlavor::MachO,
        LldDriver::Wasm => codira_lld::LldFlavor::Wasm,
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
    let explicit = program_override.is_some();
    let mut program = program_override.unwrap_or_else(|| driver.default_program().to_owned());

    let mut result = Command::new(&program).args(args).output();
    // Not on `PATH`: try the LLVM installation this compiler was built
    // against, which ships every LLD driver. That is what makes a cross-link
    // work without anyone having put a linker on `PATH` first. A linker
    // named through `CODIRA_LLD_<FLAVOR>` is not second-guessed.
    if !explicit {
        if let (Err(error), Some(prefix)) = (&result, option_env!("LLVM_SYS_221_PREFIX")) {
            if error.kind() == std::io::ErrorKind::NotFound {
                // Appended, not `with_extension`: that would read `ld.lld` as
                // the file `ld` with extension `lld` and produce `ld.exe`.
                let bundled = Path::new(prefix).join("bin").join(format!(
                    "{}{}",
                    driver.default_program(),
                    env::consts::EXE_SUFFIX
                ));
                if bundled.is_file() {
                    program = bundled.display().to_string();
                    result = Command::new(&program).args(args).output();
                }
            }
        }
    }

    let output = result.map_err(|e| {
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
        LinkerFlavor::WasmLld => Box::new(WasmLinker::default()),
    }
}

/// Whether `library` names a file rather than a library for the linker to
/// search for.
fn is_library_path(library: &str) -> bool {
    library.contains('/')
        || library.contains('\\')
        || [".so", ".a", ".lib", ".dylib", ".tbd"]
            .iter()
            .any(|extension| library.ends_with(extension) || library.contains(".so."))
}

pub trait Linker {
    fn add_object(&mut self, path: &Path) -> Result<(), LinkerError>;

    /// Adds the package's native code: its assembled objects, its
    /// libraries, and -- through `uses_c_runtime` -- whatever the platform
    /// needs for C and C++ code to run. See `crate::native`.
    fn add_native(&mut self, native: &crate::NativeInputs) -> Result<(), LinkerError> {
        for object in &native.objects {
            self.add_object(object)?;
        }
        for library in &native.libraries {
            self.add_library(library);
        }
        if native.needs_c_runtime {
            self.uses_c_runtime(native.needs_cxx_runtime);
        }
        Ok(())
    }

    /// Links against `library`: a path to the library file, or a name the
    /// platform's linker resolves.
    fn add_library(&mut self, library: &str);

    /// Records that the assembly contains C (and, with `cxx`, C++) code. Only
    /// COFF has anything to do here; see `MsvcLinker::uses_c_runtime`.
    fn uses_c_runtime(&mut self, _cxx: bool) {}

    /// Keeps a symbol table in the output that names every function,
    /// including local ones (`codira build --certify`). `ld.lld` and
    /// `ld64.lld` keep the full symbol table unless told to strip it, so
    /// only COFF has anything to do here.
    fn keep_symbol_table(&mut self) {}

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

/// Links a freestanding WebAssembly module with LLD's `wasm-ld` driver.
///
/// A module is not a shared library: it has no entry point (`--no-entry`)
/// and exports exactly the `@export("C")` functions, plus the linear memory
/// LLD creates. Everything else is garbage-collected by the linker. An
/// undefined symbol stays an error rather than becoming an import: the
/// module is meant to need nothing from its host.
#[derive(Default)]
struct WasmLinker {
    args: Vec<String>,
}

impl Linker for WasmLinker {
    fn add_object(&mut self, path: &Path) -> Result<(), LinkerError> {
        let path_str = path
            .to_str()
            .ok_or_else(|| LinkerError::PathError(path.to_owned()))?
            .to_owned();
        self.args.push(path_str);
        Ok(())
    }

    fn add_library(&mut self, library: &str) {
        self.args.push(library.to_owned());
    }

    fn build_shared_object(
        &mut self,
        path: &Path,
        exported_symbols: &[String],
    ) -> Result<(), LinkerError> {
        let path_str = path
            .to_str()
            .ok_or_else(|| LinkerError::PathError(path.to_owned()))?;
        self.args.push("--no-entry".to_owned());
        for symbol in exported_symbols {
            self.args.push(format!("--export={symbol}"));
        }
        self.args.push("-o".to_owned());
        self.args.push(path_str.to_owned());
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), LinkerError> {
        run_lld(LldDriver::Wasm, &self.args).map_err(LinkerError::LinkError)
    }
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
    fn add_library(&mut self, library: &str) {
        if is_library_path(library) {
            self.args.push(library.to_owned());
        } else {
            self.args.push(format!("-l{library}"));
        }
    }

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
    fn add_library(&mut self, library: &str) {
        if is_library_path(library) {
            self.args.push(library.to_owned());
        } else {
            self.args.push(format!("-l{library}"));
        }
    }

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
        // An ad-hoc code signature, which arm64 macOS requires before it
        // maps the library and which x86-64 macOS accepts. LLD writes it as
        // the last step of the link, so nothing has to re-sign the file
        // afterwards.
        self.args.push("-adhoc_codesign".to_owned());

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
    /// `Some(cxx)` once the assembly is known to contain C (and, if `cxx`,
    /// C++) code.
    c_runtime: Option<bool>,
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
            c_runtime: None,
        }
    }
}

impl Linker for MsvcLinker {
    fn add_library(&mut self, library: &str) {
        if is_library_path(library) {
            self.args.push(library.to_owned());
        } else {
            self.args.push(format!("/DEFAULTLIB:{library}.lib"));
        }
    }

    /// A Codira-only DLL links nothing and has no entry point. C and C++ code
    /// needs the DLL-backed C runtime that Clang compiled it against
    /// (`-fms-runtime-lib=dll`), and its entry point, `_DllMainCRTStartup`,
    /// which runs C++ static constructors and the runtime's per-DLL setup
    /// when the assembly is loaded. Finding the runtime's import libraries
    /// means letting `lld-link` locate the Windows SDK and MSVC, which
    /// `no_sysroot_arg` otherwise stops it from doing.
    fn uses_c_runtime(&mut self, cxx: bool) {
        self.c_runtime = Some(cxx);
    }

    /// A PE image carries no symbol table by default. `/DEBUG:SYMTAB` is
    /// LLD's switch for writing the COFF one -- static (local) symbols
    /// included -- without a PDB. It also turns off the reference
    /// elimination and identical-code folding that LLD enables when not
    /// debugging, so those are asked for explicitly: the certified image is
    /// laid out the way an ordinary one is.
    fn keep_symbol_table(&mut self) {
        self.args.push("/DEBUG:SYMTAB".to_owned());
        self.args.push("/OPT:REF".to_owned());
        self.args.push("/OPT:ICF".to_owned());
    }

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
        match self.c_runtime {
            None => {
                self.args.push("/NOENTRY".to_owned());
                self.args.push(no_sysroot_arg(path));
            }
            Some(cxx) => {
                for library in ["msvcrt", "ucrt", "vcruntime", "kernel32"] {
                    self.args.push(format!("/DEFAULTLIB:{library}.lib"));
                }
                if cxx {
                    self.args.push("/DEFAULTLIB:msvcprt.lib".to_owned());
                }
            }
        }
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
        assert_eq!(parse_link_mode("direct"), Some(LinkMode::Direct));
        assert_eq!(parse_link_mode(" DIRECT "), Some(LinkMode::Direct));
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
