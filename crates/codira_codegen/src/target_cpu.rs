//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - `--target-cpu`: resolves a CPU name to a concrete CPU and the feature
//!   string it implies, before either reaches the salsa input.
//!
//! # Why resolve at all
//!
//! LLVM accepts any CPU string. An unknown one prints a warning on stderr
//! and builds for the generic CPU, so a typo in `--target-cpu` silently
//! cost the user the code they asked for. And `native` means "whatever
//! machine this happens to run on", which is not an input a reproducible
//! build can have. Resolution fixes both:
//!
//! * a named CPU is checked against LLVM's own subtarget table for the target,
//!   and the features it implies are written out explicitly;
//! * `native` is replaced by the host's CPU name and the features the host
//!   actually has (which can differ from the name's defaults: a virtual machine
//!   may hide AVX-512, say).
//!
//! The result is stored in `Target::options.{cpu, features}` -- a salsa
//! input -- so the build is a pure function of a concrete, recorded CPU.
//! The e-graph cost table is chosen from the same field (see
//! `EidosOptions::optimizer`).

use std::ffi::{c_char, c_int, CStr, CString};

use codira_target::spec::Target;
use inkwell::targets::TargetMachine;

extern "C" {
    fn codira_llvm_cpu_features(
        triple: *const c_char,
        cpu: *const c_char,
        out: *mut *mut c_char,
    ) -> c_int;
    fn codira_llvm_free(text: *mut c_char);
}

const CPU_OK: c_int = 0;
const CPU_UNKNOWN_TARGET: c_int = 1;

/// Why a `--target-cpu` value was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetCpuError {
    #[error(
        "'{cpu}' is not a CPU LLVM knows for {triple}; known CPUs include: {}",
        sample(known)
    )]
    UnknownCpu {
        cpu: String,
        triple: String,
        known: Vec<String>,
    },
    #[error(
        "--target-cpu=native describes the machine running the compiler ({host}), so it can \
         only be used when building for that machine, not for {triple}"
    )]
    NativeForAnotherTarget { host: String, triple: String },
    #[error("LLVM has no subtarget information for {triple}: {message}")]
    UnknownTarget { triple: String, message: String },
}

/// A short, readable excerpt of a long CPU list for an error message.
fn sample(known: &[String]) -> String {
    const SHOWN: usize = 24;
    if known.len() <= SHOWN {
        known.join(", ")
    } else {
        format!(
            "{}, ... ({} in all)",
            known[..SHOWN].join(", "),
            known.len()
        )
    }
}

/// Returns `target` with its CPU set to `cpu`, resolved: a CPU name LLVM
/// knows for the target, and the explicit feature string it implies (the
/// target's own mandatory features, if any, are kept after them).
///
/// `"native"` resolves to the host CPU and the host's actual features,
/// and is refused for any target other than the host's.
pub fn resolve_target_cpu(target: &Target, cpu: &str) -> Result<Target, TargetCpuError> {
    crate::db::initialize_target_backends_once();
    let triple = target.llvm_target.to_string();

    let (name, features) = if cpu == "native" {
        let host = Target::host_target().map(|t| t.llvm_target.to_string());
        if host.as_deref() != Some(triple.as_str()) {
            return Err(TargetCpuError::NativeForAnotherTarget {
                host: host.unwrap_or_else(|| codira_target::host_triple().to_owned()),
                triple,
            });
        }
        let name = TargetMachine::get_host_cpu_name().to_string();
        // Validate the host's own name too: an LLVM older than the CPU
        // reports a name its tables may not have.
        let table_features = cpu_features(&triple, &name)?;
        // LLVM reads the host's features where the OS says what they are
        // (x86's CPUID, Linux's /proc/cpuinfo). On macOS AArch64 it reports
        // none, and the CPU's name (apple-m1, ...) is all there is to go by,
        // so its features are the ones LLVM's table gives that CPU.
        let host_features = TargetMachine::get_host_cpu_features().to_string();
        let features = if host_features.is_empty() {
            table_features
        } else {
            host_features
        };
        (name, features)
    } else {
        let features = cpu_features(&triple, cpu)?;
        (cpu.to_owned(), features)
    };

    let mut resolved = target.clone();
    resolved.options.cpu = name;
    resolved.options.features = match (features.is_empty(), target.options.features.is_empty()) {
        (_, true) => features,
        (true, false) => target.options.features.clone(),
        (false, false) => format!("{features},{}", target.options.features),
    };
    Ok(resolved)
}

/// The feature string LLVM's subtarget table gives `cpu` on `triple`.
fn cpu_features(triple: &str, cpu: &str) -> Result<String, TargetCpuError> {
    let unknown_cpu = |known: Vec<String>| TargetCpuError::UnknownCpu {
        cpu: cpu.to_owned(),
        triple: triple.to_owned(),
        known,
    };
    let (Ok(c_triple), Ok(c_cpu)) = (CString::new(triple), CString::new(cpu)) else {
        return Err(unknown_cpu(Vec::new()));
    };
    let mut out: *mut c_char = std::ptr::null_mut();
    // SAFETY: both strings are NUL-terminated and outlive the call; `out`
    // is a valid place for the shim to store a `malloc`ed string, which is
    // released below with the shim's own `free`.
    let status = unsafe { codira_llvm_cpu_features(c_triple.as_ptr(), c_cpu.as_ptr(), &mut out) };
    let text = if out.is_null() {
        String::new()
    } else {
        // SAFETY: the shim returns a NUL-terminated string or null.
        let text = unsafe { CStr::from_ptr(out) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: allocated by the shim with `malloc`, freed exactly once.
        unsafe { codira_llvm_free(out) };
        text
    };
    match status {
        CPU_OK => Ok(text),
        CPU_UNKNOWN_TARGET => Err(TargetCpuError::UnknownTarget {
            triple: triple.to_owned(),
            message: text,
        }),
        _ => Err(unknown_cpu(
            text.split(',')
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use codira_target::spec::Target;

    use super::{resolve_target_cpu, TargetCpuError};

    fn x86_64_linux() -> Target {
        Target::search("x86_64-unknown-linux-gnu").unwrap()
    }

    #[test]
    fn a_named_cpu_resolves_to_its_features() {
        let resolved = resolve_target_cpu(&x86_64_linux(), "znver4").unwrap();
        assert_eq!(resolved.options.cpu, "znver4");
        let features: Vec<&str> = resolved.options.features.split(',').collect();
        for expected in ["+avx512f", "+avx2", "+sse4.2", "+bmi2"] {
            assert!(features.contains(&expected), "{features:?}");
        }
        // Everything else about the target is untouched.
        assert_eq!(resolved.llvm_target, x86_64_linux().llvm_target);
        assert_eq!(resolved.data_layout, x86_64_linux().data_layout);
    }

    #[test]
    fn an_older_cpu_lacks_what_a_newer_one_has() {
        let old = resolve_target_cpu(&x86_64_linux(), "nehalem").unwrap();
        assert!(old.options.features.contains("+sse4.2"));
        assert!(
            !old.options.features.contains("+avx"),
            "{}",
            old.options.features
        );
    }

    #[test]
    fn an_unknown_cpu_is_refused_with_the_known_ones() {
        let error = resolve_target_cpu(&x86_64_linux(), "znver44").unwrap_err();
        match &error {
            TargetCpuError::UnknownCpu { known, .. } => {
                assert!(known.iter().any(|name| name == "znver4"), "{known:?}");
            }
            other => panic!("expected UnknownCpu, got {other:?}"),
        }
        let message = error.to_string();
        assert!(
            message.contains("'znver44' is not a CPU LLVM knows"),
            "{message}"
        );
    }

    #[test]
    fn cpus_are_checked_against_the_right_architecture() {
        // An AArch64 core is not an x86-64 CPU, and the reverse.
        assert!(resolve_target_cpu(&x86_64_linux(), "cortex-a76").is_err());
        let darwin = Target::search("aarch64-apple-darwin").unwrap();
        assert!(resolve_target_cpu(&darwin, "znver4").is_err());
        let apple = resolve_target_cpu(&darwin, "apple-m1").unwrap();
        assert!(
            apple.options.features.contains("+neon"),
            "{}",
            apple.options.features
        );
    }

    #[test]
    fn native_names_the_host_concretely() {
        let host = Target::host_target().unwrap();
        let resolved = resolve_target_cpu(&host, "native").unwrap();
        assert_ne!(resolved.options.cpu, "native");
        assert!(!resolved.options.cpu.is_empty());
        assert!(!resolved.options.features.is_empty());
        // Resolving twice on the same machine gives the same input.
        assert_eq!(resolve_target_cpu(&host, "native").unwrap(), resolved);
    }

    #[test]
    fn the_target_machine_is_built_for_the_resolved_cpu() {
        use codira_hir::HirDatabase;

        let mut db = crate::mock::MockDatabase::default();
        let resolved = resolve_target_cpu(&x86_64_linux(), "znver4").unwrap();
        db.set_target(resolved.clone());
        let machine = crate::db::build_target_machine(&db);
        assert_eq!(machine.get_cpu().to_string(), "znver4");
        assert_eq!(
            machine.get_feature_string().to_string_lossy(),
            resolved.options.features
        );
    }

    #[test]
    fn native_is_refused_for_another_target() {
        let host = Target::host_target().unwrap();
        let other = ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]
            .into_iter()
            .map(|t| Target::search(t).unwrap())
            .find(|t| t.llvm_target != host.llvm_target)
            .unwrap();
        assert!(matches!(
            resolve_target_cpu(&other, "native"),
            Err(TargetCpuError::NativeForAnotherTarget { .. })
        ));
    }
}
