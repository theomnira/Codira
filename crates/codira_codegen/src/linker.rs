//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
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

/// Invokes an external LLD binary (`lld-link`/`ld.lld`/`ld64.lld`) with the
/// given flags.
///
/// Codira's codegen used to link in-process via the `lld_rs` crate (LLD
/// compiled as a static library linked directly into the compiler). That
/// requires LLD's own headers and static libraries to be present alongside
/// whatever LLVM build `LLVM_SYS_140_PREFIX` points at, which most prebuilt
/// LLVM 14 distributions don't include (only a full from-source LLVM build
/// with `-DLLVM_ENABLE_PROJECTS=lld` does). Standalone `lld-link`/`ld.lld`/
/// `ld64.lld` executables are, by contrast, part of every common LLVM
/// distribution (the official installer, package managers, etc.) from
/// version 14 all the way to whatever is newest -- LLD's job is linking
/// standard platform object files (COFF/ELF/Mach-O), which are stable
/// formats not tied to the LLVM version that produced them, so a newer LLD
/// linking Codira's LLVM-14-generated objects is not a version mismatch in
/// any way that matters here. This shells out to the same linker instead.
///
/// The binary name can be overridden with the `CODIRA_LLD_<FLAVOR>` env var
/// (`CODIRA_LLD_COFF`, `CODIRA_LLD_ELF`, `CODIRA_LLD_MACHO`) when it isn't on
/// `PATH` under its default name.
fn run_lld(flavor_env_suffix: &str, default_name: &str, args: &[String]) -> Result<(), String> {
    let program = env::var(format!("CODIRA_LLD_{flavor_env_suffix}"))
        .unwrap_or_else(|_| default_name.to_owned());

    let output = Command::new(&program).args(args).output().map_err(|e| {
        format!(
            "failed to run linker `{program}`: {e} (set CODIRA_LLD_{flavor_env_suffix} to \
             override the linker path)"
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
        run_lld("ELF", "ld.lld", &self.args).map_err(LinkerError::LinkError)
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
        run_lld("MACHO", "ld64.lld", &self.args).map_err(LinkerError::LinkError)
    }
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

        let dll_lib_path_str = path
            .to_str()
            .ok_or_else(|| LinkerError::PathError(path.to_owned()))?;

        self.args.push("/DLL".to_owned());
        self.args.push("/NOENTRY".to_owned());
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
        self.args.push(format!("/IMPLIB:{dll_lib_path_str}"));
        self.args.push(format!("/OUT:{dll_path_str}"));
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), LinkerError> {
        run_lld("COFF", "lld-link", &self.args).map_err(LinkerError::LinkError)
    }
}
