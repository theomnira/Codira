//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Compiles a package's `[native]` sources with Clang: C and C++ to LLVM
//!   bitcode, assembly to object files. See `codira_codegen::NativeInputs` for
//!   what happens to them next.
//!
//! Clang is looked up as `CODIRA_CLANG`, then in the `bin` directory of the
//! LLVM installation this compiler was built against (whose bitcode format
//! matches the backend exactly), then as `clang` on `PATH`. LLVM reads
//! bitcode from older versions, so an older Clang on `PATH` works too.
//!
//! Outputs are cached in the package's output directory under `native/`,
//! keyed by the source path and the full command line. Each compile also
//! writes a dependency file, and an output is reused only while it is newer
//! than every file Clang said it read, headers included.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use codira_codegen::NativeInputs;
use codira_project::NativeSources;
use codira_target::spec::Target;

/// What Clang does with one source file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Language {
    C,
    Cxx,
    /// Assembly that is not preprocessed (`.s`).
    Assembly,
    /// Assembly that goes through the C preprocessor first (`.S`, `.asm`).
    AssemblyWithCpp,
}

impl Language {
    fn of(path: &Path) -> anyhow::Result<Language> {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        Ok(match extension {
            "c" => Language::C,
            "cc" | "cpp" | "cxx" | "c++" => Language::Cxx,
            "s" => Language::Assembly,
            "S" | "asm" => Language::AssemblyWithCpp,
            _ => anyhow::bail!(
                "'{}' is not a native source Codira knows how to compile: expected .c, \
                 .cc/.cpp/.cxx, .s, .S or .asm",
                path.display()
            ),
        })
    }

    fn produces_bitcode(self) -> bool {
        matches!(self, Language::C | Language::Cxx)
    }
}

/// The Clang to compile with.
pub fn find_clang() -> PathBuf {
    if let Some(clang) = std::env::var_os("CODIRA_CLANG") {
        return clang.into();
    }
    if let Some(prefix) = option_env!("LLVM_SYS_221_PREFIX") {
        let bundled = Path::new(prefix)
            .join("bin")
            .join(format!("clang{}", std::env::consts::EXE_SUFFIX));
        if bundled.is_file() {
            return bundled;
        }
    }
    PathBuf::from("clang")
}

/// Compiles every source in `native` for `target` into `out_dir`.
pub fn compile_native_sources(
    package_root: &Path,
    native: &NativeSources,
    target: &Target,
    out_dir: &Path,
) -> anyhow::Result<NativeInputs> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| anyhow::anyhow!("could not create '{}': {e}", out_dir.display()))?;
    let clang = find_clang();
    let is_msvc = target.options.is_like_msvc;

    let mut bitcode = Vec::new();
    let mut objects = Vec::new();
    let mut needs_c_runtime = false;
    let mut needs_cxx_runtime = false;

    for source in &native.sources {
        let path = package_root.join(source);
        let language = Language::of(&path)?;
        if !path.is_file() {
            anyhow::bail!("native source '{}' does not exist", path.display());
        }

        let mut args: Vec<String> = vec![format!("--target={}", target.llvm_target)];
        match language {
            Language::C => args.extend(["-x", "c", "-std=c17"].map(String::from)),
            Language::Cxx => args.extend(["-x", "c++", "-std=c++20"].map(String::from)),
            Language::Assembly => args.extend(["-x", "assembler"].map(String::from)),
            Language::AssemblyWithCpp => {
                args.extend(["-x", "assembler-with-cpp"].map(String::from));
            }
        }
        if language.produces_bitcode() {
            needs_c_runtime = true;
            needs_cxx_runtime |= language == Language::Cxx;
            // Optimized before it is linked into Codira code and optimized
            // again with it. `-O0` would not do: Clang marks every function
            // `optnone` at `-O0`, and LLVM then refuses to inline it into its
            // Codira callers, which is the point of compiling to bitcode.
            args.extend(["-O2", "-emit-llvm"].map(String::from));
        }
        args.extend(["-ffunction-sections", "-fdata-sections"].map(String::from));
        if is_msvc {
            // The DLL-backed C runtime (`/MD`); the static one (`/MT`, Clang's
            // default) cannot be shared with the host process.
            args.push("-fms-runtime-lib=dll".to_owned());
        } else {
            args.push("-fPIC".to_owned());
        }
        for include in &native.include {
            args.push(format!("-I{}", package_root.join(include).display()));
        }
        for define in &native.defines {
            args.push(format!("-D{define}"));
        }
        args.extend(native.flags.iter().cloned());

        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        args.hash(&mut hasher);
        clang.hash(&mut hasher);
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("native");
        let extension = if language.produces_bitcode() {
            "bc"
        } else {
            "o"
        };
        let output = out_dir.join(format!("{stem}-{:016x}.{extension}", hasher.finish()));
        let depfile = output.with_extension("d");

        if !is_fresh(&output, &depfile) {
            let result = Command::new(&clang)
                .args(&args)
                .arg("-c")
                .arg(&path)
                .arg("-o")
                .arg(&output)
                .arg("-MD")
                .arg("-MF")
                .arg(&depfile)
                .output()
                .map_err(|e| {
                    anyhow::anyhow!(
                        "could not run Clang ('{}'): {e}. Set CODIRA_CLANG to a clang executable.",
                        clang.display()
                    )
                })?;
            if !result.status.success() {
                anyhow::bail!(
                    "Clang could not compile '{}':\n{}{}",
                    path.display(),
                    String::from_utf8_lossy(&result.stderr),
                    String::from_utf8_lossy(&result.stdout)
                );
            }
        }

        if language.produces_bitcode() {
            bitcode.push(output);
        } else {
            objects.push(output);
        }
    }

    let mut libraries = native.libs.clone();
    // On ELF a shared object has to name the C++ runtime it uses, or loading
    // it fails on the first `operator new`. The C library needs no such
    // entry: the process loading the assembly already has it. A Mach-O
    // library must resolve every symbol when it is linked, so it names
    // libc++, which the SDK the linker is given has.
    if needs_cxx_runtime && !is_msvc {
        libraries.push(if target.options.is_like_osx {
            "c++".to_owned()
        } else {
            cxx_runtime_for(&clang, target)
        });
    }

    NativeInputs::new(
        bitcode,
        objects,
        libraries,
        needs_c_runtime,
        needs_cxx_runtime,
    )
}

/// The C++ runtime library for `target`, as a path when Clang can find one
/// and as a library name otherwise.
fn cxx_runtime_for(clang: &Path, target: &Target) -> String {
    let found = Command::new(clang)
        .arg(format!("--target={}", target.llvm_target))
        .arg("-print-file-name=libstdc++.so")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|path| path.trim().to_owned())
        .filter(|path| Path::new(path).is_absolute() && Path::new(path).is_file());
    found.unwrap_or_else(|| "stdc++".to_owned())
}

/// Whether `output` exists and is newer than every input `depfile` lists.
fn is_fresh(output: &Path, depfile: &Path) -> bool {
    let Some(built) = modified(output) else {
        return false;
    };
    let Ok(deps) = std::fs::read_to_string(depfile) else {
        return false;
    };
    dependency_paths(&deps)
        .iter()
        .all(|dep| modified(Path::new(dep)).is_some_and(|m| m <= built))
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The prerequisites in a Makefile-style dependency file: everything after
/// the first unescaped `:`, split on whitespace, with `\` line continuations
/// and `\ ` escaped spaces handled. Drive letters (`C:\...`) are not taken
/// for the rule separator.
fn dependency_paths(depfile: &str) -> Vec<String> {
    let text = depfile.replace("\\\r\n", " ").replace("\\\n", " ");
    let bytes: Vec<char> = text.chars().collect();
    let mut start = None;
    for (i, c) in bytes.iter().enumerate() {
        let is_drive = i == 1
            || (i >= 2 && bytes[i - 2].is_whitespace() && bytes[i - 1].is_ascii_alphabetic());
        if *c == ':' && !(is_drive && bytes.get(i + 1).is_some_and(|n| *n == '\\' || *n == '/')) {
            start = Some(i + 1);
            break;
        }
    }
    let Some(start) = start else {
        return Vec::new();
    };

    let mut paths = Vec::new();
    let mut current = String::new();
    let mut chars = bytes[start..].iter().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&&' ') => {
                current.push(' ');
                chars.next();
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    paths.push(std::mem::take(&mut current));
                }
            }
            c => current.push(*c),
        }
    }
    if !current.is_empty() {
        paths.push(current);
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::dependency_paths;

    #[test]
    fn dependency_files_are_read_on_windows_and_unix() {
        assert_eq!(
            dependency_paths("out.bc: src/a.c include/a.h \\\n  include/b\\ c.h\n"),
            vec!["src/a.c", "include/a.h", "include/b c.h"]
        );
        assert_eq!(
            dependency_paths("C:\\pkg\\out.bc: C:\\pkg\\c\\add.c C:\\pkg\\c\\add.h\n"),
            vec!["C:\\pkg\\c\\add.c", "C:\\pkg\\c\\add.h"]
        );
    }
}
