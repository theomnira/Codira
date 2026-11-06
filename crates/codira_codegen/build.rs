//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Compiles `cpp/target_info.cpp`, the small C++ shim that asks LLVM's
//!   subtarget tables which CPUs a target knows and which features a CPU
//!   implies (see `src/target_cpu.rs`). LLVM itself is linked by `llvm-sys`
//!   through `inkwell`; the shim only needs LLVM's headers and the flags LLVM
//!   was built with.

use std::{env, path::Path, process::Command};

fn llvm_config(binary: &Path, arg: &str) -> String {
    let output = Command::new(binary)
        .arg(arg)
        .output()
        .unwrap_or_else(|e| panic!("could not run {}: {e}", binary.display()));
    String::from_utf8(output.stdout).expect("llvm-config printed non-UTF-8 output")
}

fn main() {
    println!("cargo:rerun-if-changed=cpp/target_info.cpp");
    println!("cargo:rerun-if-changed=cpp/gc_roots.cpp");

    // `llvm-sys` (`links = "llvm-22"`) tells its direct dependents where the
    // `llvm-config` it was built against lives, so the shim is compiled
    // against exactly the headers of the LLVM that is linked.
    let Some(config) = env::var_os("DEP_LLVM_22_CONFIG_PATH") else {
        panic!("llvm-sys did not report its llvm-config (DEP_LLVM_22_CONFIG_PATH)");
    };
    let config = Path::new(&config);

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .file("cpp/target_info.cpp")
        .file("cpp/gc_roots.cpp");
    let msvc = build.get_compiler().is_like_msvc();
    // LLVM's own compile flags: include path, language standard, the
    // defines its headers expect, no exceptions or RTTI. Warning flags are
    // dropped on GCC-like compilers, as `codira_lld`'s build does, because
    // they are LLVM's choices for LLVM's sources, not ours.
    for flag in llvm_config(config, "--cxxflags").split_whitespace() {
        if !msvc && flag.starts_with("-W") {
            continue;
        }
        build.flag(flag);
    }
    if msvc {
        // Prebuilt LLVM for MSVC uses the static CRT; the shim must agree,
        // or linking fails with LNK2038 (`codira_lld` hit exactly this).
        build.static_crt(true);
    }
    build.warnings(false);
    build.compile("codira_target_info");
}
