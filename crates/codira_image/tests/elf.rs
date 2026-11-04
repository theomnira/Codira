//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 15, 2026
//!
//! Functionality:
//! - Tests the ELF shared-object writer on objects LLVM emitted for real Codira
//!   programs (`tests/elf_corpus`, each next to the library `ld.lld --shared`
//!   produced from it): the image must satisfy everything the dynamic loader
//!   needs, export what LLD's exports and ask the loader for the same symbolic
//!   relocations, be accepted by `llvm-readobj`, and -- where a Linux system is
//!   at hand (Linux itself, or WSL on Windows) -- load in a real dynamic loader
//!   and give the same answers as LLD's library, including from a function it
//!   runs.
use std::{
    path::{Path, PathBuf},
    process::Command,
};

use codira_image::{elfinfo, link_linux_so, DeclineKind, Outcome, SoRequest};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/elf_corpus")
}

fn objects() -> Vec<PathBuf> {
    let mut objects: Vec<PathBuf> = std::fs::read_dir(corpus())
        .expect("corpus directory")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "o"))
        .collect();
    objects.sort();
    objects
}

fn link(object: &[u8]) -> Outcome {
    link_linux_so(&SoRequest {
        object,
        debug_info: false,
        has_native_code: false,
    })
}

fn find_readobj() -> PathBuf {
    if let Ok(path) = std::env::var("LLVM_READOBJ") {
        return path.into();
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(prefix) = std::env::var("LLVM_SYS_221_PREFIX") {
        candidates.push(PathBuf::from(prefix).join("bin"));
    }
    candidates.push("C:/llvm22/bin".into());
    if let Some(paths) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&paths));
    }
    candidates
        .into_iter()
        .find_map(|dir| {
            ["llvm-readobj.exe", "llvm-readobj"]
                .iter()
                .map(|name| dir.join(name))
                .find(|path| path.is_file())
        })
        .expect("llvm-readobj was not found (set LLVM_READOBJ)")
}

#[test]
fn shared_objects_satisfy_the_loader_and_export_what_lld_exports() {
    let readobj = find_readobj();
    let scratch = std::env::temp_dir().join(format!("codira_image_elf_{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let mut written = 0;
    for path in objects() {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let object = std::fs::read(&path).unwrap();
        let image = match link(&object) {
            Outcome::Image(image) => image,
            // The programs that call one of their own public functions
            // directly need a PLT; that is declined (see the next test).
            Outcome::Declined(why) => {
                assert!(why.detail.contains("PLT"), "{name} was declined: {why}");
                continue;
            }
        };
        let lld = std::fs::read(path.with_extension("so")).unwrap();
        let expected = elfinfo::validate(&lld).expect("LLD's library validates");
        written += 1;
        let actual = elfinfo::validate(&image)
            .unwrap_or_else(|e| panic!("{name}: the library does not validate: {e}"));
        elfinfo::compare(&expected, &actual).unwrap_or_else(|e| panic!("{name}: {e}"));

        let file = scratch.join(format!("{name}.so"));
        std::fs::write(&file, &image).unwrap();
        let out = Command::new(&readobj)
            .args([
                "--file-headers",
                "--program-headers",
                "--sections",
                "--dyn-symbols",
                "--dyn-relocations",
                "--dynamic-table",
            ])
            .arg(&file)
            .output()
            .expect("llvm-readobj runs");
        assert!(
            out.status.success() && out.stderr.is_empty(),
            "{name}: llvm-readobj rejected the library:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(written >= 15, "only {written} libraries were written");
}

#[test]
fn a_direct_call_to_a_public_function_is_declined_because_it_needs_a_plt() {
    let object = std::fs::read(corpus().join("c071.o")).unwrap();
    match link(&object) {
        Outcome::Declined(why) => {
            assert_eq!(why.kind, DeclineKind::UnknownRelocation, "{why}");
            assert!(why.detail.contains("PLT"), "{why}");
        }
        Outcome::Image(_) => panic!("a call that another library could override was accepted"),
    }
}

#[test]
fn damaged_objects_are_declined_or_give_valid_libraries() {
    let object = std::fs::read(corpus().join("p1.o")).unwrap();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    // Every prefix.
    for len in 0..object.len() {
        if let Outcome::Image(image) = link(&object[..len]) {
            elfinfo::validate(&image).expect("an accepted prefix still validates");
        }
    }
    // Random damage.
    let (mut accepted, mut declined) = (0, 0);
    for _ in 0..4000 {
        let mut damaged = object.clone();
        for _ in 0..=(next() % 3) {
            let at = (next() % damaged.len() as u64) as usize;
            damaged[at] = next() as u8;
        }
        match link(&damaged) {
            Outcome::Image(image) => {
                accepted += 1;
                elfinfo::validate(&image).unwrap_or_else(|e| {
                    panic!("damage at the bytes of a mutant gave an invalid library: {e}")
                });
            }
            Outcome::Declined(_) => declined += 1,
        }
    }
    assert!(accepted > 100 && declined > 100, "{accepted} / {declined}");
}

/// Runs `script` (Python) with `library` and `reference` as arguments on a
/// Linux system, if there is one at hand. `None`: there is none.
fn run_on_linux(script: &Path, library: &Path, reference: &Path) -> Option<String> {
    let linux_path = |p: &Path| -> String {
        let text = p.display().to_string().replace('\\', "/");
        match text.as_bytes() {
            [drive, b':', ..] if cfg!(windows) => format!(
                "/mnt/{}{}",
                (*drive as char).to_ascii_lowercase(),
                &text[2..]
            ),
            _ => text,
        }
    };
    let mut command = if cfg!(target_os = "linux") {
        Command::new("python3")
    } else if cfg!(windows) {
        // wsl.exe reports a missing distribution on stdout, so the check
        // below cannot tell that from a failed load: ask WSL first.
        let wsl_runs_python = Command::new("wsl.exe")
            .args(["-e", "python3", "-c", ""])
            .output()
            .is_ok_and(|output| output.status.success());
        if !wsl_runs_python {
            return None;
        }
        let mut c = Command::new("wsl.exe");
        c.args(["-e", "python3"]);
        c
    } else {
        return None;
    };
    command
        .arg("-u")
        .arg(linux_path(script))
        .arg(linux_path(library))
        .arg(linux_path(reference))
        // MSYS-style shells rewrite arguments that look like paths.
        .env("MSYS_NO_PATHCONV", "1");
    let output = command.output().ok()?;
    if !output.status.success() && output.stdout.is_empty() {
        // Not a Linux with Python to run it (WSL missing or no distribution).
        return None;
    }
    assert!(
        output.status.success(),
        "loading on Linux failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

const LOADER_SCRIPT: &str = r#"
import ctypes, sys

class Info(ctypes.Structure):
    _fields_ = [("path", ctypes.c_void_p), ("functions", ctypes.c_void_p), ("types", ctypes.c_void_p),
                ("num_functions", ctypes.c_uint32), ("num_types", ctypes.c_uint32),
                ("dt_protos", ctypes.c_void_p), ("dt_fn_ptrs", ctypes.c_void_p), ("dt_num", ctypes.c_uint32),
                ("lut_ids", ctypes.c_void_p), ("lut_handles", ctypes.c_void_p), ("lut_names", ctypes.c_void_p),
                ("lut_num", ctypes.c_uint32), ("deps", ctypes.c_void_p), ("num_deps", ctypes.c_uint32)]

def summary(path):
    lib = ctypes.CDLL(path)
    gv = lib.get_version
    gv.restype = ctypes.c_uint32
    gi = lib.get_info
    gi.restype = Info
    gi.argtypes = []
    info = gi()
    names = ctypes.cast(info.lut_names, ctypes.POINTER(ctypes.c_char_p)) if info.lut_num else []
    return [gv(), ctypes.string_at(info.path).decode(), info.num_functions, info.num_types,
            info.dt_num, info.lut_num, info.num_deps, [names[k].decode() for k in range(info.lut_num)]]

ours, theirs = summary(sys.argv[1]), summary(sys.argv[2])
assert ours == theirs, (ours, theirs)
print("same", ours[0], ours[2], ours[5])
"#;

#[test]
fn libraries_load_in_a_real_dynamic_loader_and_answer_like_lld_s() {
    let scratch = std::env::temp_dir().join(format!("codira_image_load_{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let script = scratch.join("load.py");
    std::fs::write(&script, LOADER_SCRIPT).unwrap();
    let mut loaded = 0;
    let mut unavailable = false;
    for path in objects() {
        let object = std::fs::read(&path).unwrap();
        let Outcome::Image(image) = link(&object) else {
            continue;
        };
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let ours = scratch.join(format!("{name}.so"));
        std::fs::write(&ours, &image).unwrap();
        let theirs = scratch.join(format!("{name}.lld.so"));
        std::fs::copy(path.with_extension("so"), &theirs).unwrap();
        if let Some(out) = run_on_linux(&script, &ours, &theirs) {
            assert!(out.contains("same"), "{name}: {out}");
            loaded += 1;
        } else {
            unavailable = true;
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    if unavailable {
        // Not silent: this test proves nothing on a machine with no Linux.
        eprintln!(
            "NOT RUN: no Linux system (a Linux host, or WSL with python3) to load the libraries in"
        );
        assert!(
            std::env::var_os("CODIRA_REQUIRE_LINUX").is_none(),
            "CODIRA_REQUIRE_LINUX is set but no Linux system is available"
        );
    } else {
        assert!(loaded >= 15, "only {loaded} libraries were loaded");
    }
}
