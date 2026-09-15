//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Locates the real Z3 SMT solver's import library and links against it.
//!
//! `codira_smt` binds directly to Z3's C API (`libz3.dll`/`libz3.lib`)
//! rather than depending on a bindgen-based crate, since bindgen requires
//! libclang to be discoverable at build time and that's an extra, brittle
//! dependency this crate doesn't need for the small API surface it uses.
//!
//! Search order for the Z3 install:
//! 1. `Z3_LIB_DIR` env var, if set (points directly at the directory containing
//!    `libz3.lib`).
//! 2. `Z3_ROOT` env var, if set (a Z3 install prefix; `bin/bin` and `lib` are
//!    both tried underneath it).
//! 3. The default Chocolatey install location on Windows.

use std::{env, path::PathBuf};

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(dir) = env::var("Z3_LIB_DIR") {
        dirs.push(PathBuf::from(dir));
    }

    if let Ok(root) = env::var("Z3_ROOT") {
        let root = PathBuf::from(root);
        dirs.push(root.join("bin").join("bin"));
        dirs.push(root.join("lib"));
    }

    if let Ok(local_app_data) = env::var("LOCALAPPDATA") {
        dirs.push(PathBuf::from(&local_app_data).join("UniGetUI/Chocolatey/lib/z3/tools/bin/bin"));
        dirs.push(PathBuf::from(local_app_data).join("Chocolatey/lib/z3/tools/bin/bin"));
    }
    if let Ok(program_data) = env::var("ProgramData") {
        dirs.push(PathBuf::from(program_data).join("chocolatey/lib/z3/tools/bin/bin"));
    }

    dirs
}

fn main() {
    let lib_name = if cfg!(windows) { "libz3" } else { "z3" };
    let lib_file = if cfg!(windows) {
        "libz3.lib"
    } else {
        "libz3.so"
    };

    let found_dir = candidate_dirs()
        .into_iter()
        .find(|dir| dir.join(lib_file).is_file());

    let Some(dir) = found_dir else {
        panic!(
            "could not find {lib_file} (searched Z3_LIB_DIR, Z3_ROOT, and the default \
             Chocolatey install location). Install Z3 (e.g. `choco install z3`) or set \
             Z3_LIB_DIR to the directory containing {lib_file}."
        );
    };

    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=dylib={lib_name}");
    println!("cargo:rerun-if-env-changed=Z3_LIB_DIR");
    println!("cargo:rerun-if-env-changed=Z3_ROOT");

    // Re-exported so dependent crates' tests can find `libz3.dll` on PATH at
    // runtime without the caller having to know the search logic above.
    println!("cargo:z3_dll_dir={}", dir.display());

    // `libz3.dll`'s directory usually isn't on PATH (Chocolatey only shims
    // `z3.exe` itself onto PATH, not its lib directory), so the Windows
    // loader won't find it for any binary linked against `libz3.lib`
    // unless it's placed alongside that binary. Copy it into every profile
    // directory under `target/` so `cargo test`/`cargo run` both find it
    // without extra environment setup.
    if cfg!(windows) {
        let dll_src = dir.join("libz3.dll");
        if dll_src.is_file() {
            if let Ok(out_dir) = env::var("OUT_DIR") {
                // OUT_DIR looks like `target/<profile>/build/<crate>-<hash>/out`;
                // walk up to `target/<profile>/`.
                let mut profile_dir = PathBuf::from(out_dir);
                for _ in 0..3 {
                    profile_dir.pop();
                }
                let dll_dst = profile_dir.join("libz3.dll");
                let _ = std::fs::copy(&dll_src, &dll_dst);
            }
        }
    }
}
