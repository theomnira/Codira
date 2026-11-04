//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 15, 2026
//!
//! Functionality:
//! - A build writes its `.codiralib` with the direct image writer
//!   (`codira_image`) where it can, falls back to LLD with
//!   `CODIRA_DIRECT_LINK=0`, writes the library by renaming a finished
//!   temporary file over the old one, and -- for x86-64 Linux, which is opt-in
//!   with `CODIRA_DIRECT_ELF=1` -- writes a shared object the loader accepts.
//! - The environment variables that choose the linker are process-wide, so the
//!   tests take a lock and set them for their own duration.
use std::sync::Mutex;

use codira_compiler::{
    image_link_stats, Config, Driver, Target, DIRECT_ELF_ENV, DIRECT_LINK_ENV, LINK_MODE_ENV,
    LINK_VERIFY_ENV,
};
use codira_image::{elfinfo, peinfo};
use tempfile::TempDir;

static ENVIRONMENT: Mutex<()> = Mutex::new(());

/// Runs `test` with the linker variables set as given (`None`: removed).
fn with_environment<T>(vars: &[(&str, Option<&str>)], test: impl FnOnce() -> T) -> T {
    let _guard = ENVIRONMENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let saved: Vec<_> = vars
        .iter()
        .map(|(k, _)| (*k, std::env::var_os(k)))
        .collect();
    for (key, value) in vars {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
    for (key, value) in saved {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn package(source: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname=\"img\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src").join("mod.code"), source).unwrap();
    dir
}

fn build(dir: &TempDir, target: Option<&str>) -> anyhow::Result<()> {
    let mut config = Config::default();
    if let Some(target) = target {
        config.target = Target::search(target).unwrap();
    }
    let (_, mut driver) = Driver::with_package_path(dir.path().join("codira.toml"), config)?;
    driver.write_all_assemblies(true)
}

fn library(dir: &TempDir) -> Vec<u8> {
    std::fs::read(dir.path().join("target").join("mod.codiralib")).unwrap()
}

const SOURCE: &str = "public func answer(x: i64) -> i64 { x * 2 + 1 }\n";

#[cfg(windows)]
#[test]
fn a_windows_build_is_written_by_the_image_writer_and_checked_against_lld() {
    with_environment(
        &[(DIRECT_LINK_ENV, None), (LINK_VERIFY_ENV, Some("1"))],
        || {
            let dir = package(SOURCE);
            let before = image_link_stats();
            build(&dir, None).unwrap();
            let after = image_link_stats();
            assert!(after.written > before.written, "{after:?}");
            assert!(after.verified > before.verified, "{after:?}");
            assert_eq!(after.declined, before.declined, "{after:?}");

            let bytes = library(&dir);
            assert_eq!(&bytes[..2], b"MZ");
            let info = peinfo::validate(&bytes).expect("a valid image");
            assert!(info.exports.iter().any(|(name, _)| name == "get_info"));

            // The finished library came by rename: nothing else is left in
            // the output directory but the library and its lock's remains.
            let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("target"))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".codira-tmp"))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");

            // Building again replaces it in place, and it is still whole.
            build(&dir, None).unwrap();
            peinfo::validate(&library(&dir)).expect("a valid image after the rebuild");
        },
    );
}

#[cfg(windows)]
#[test]
fn the_escape_hatch_links_with_lld_alone() {
    with_environment(&[(DIRECT_LINK_ENV, Some("0"))], || {
        let dir = package(SOURCE);
        let before = image_link_stats();
        build(&dir, None).unwrap();
        assert_eq!(image_link_stats().written, before.written);
        // LLD's image is a valid image too.
        peinfo::validate(&library(&dir)).expect("a valid image");
    });
}

#[test]
fn a_linux_build_with_the_elf_writer_gives_a_library_the_loader_accepts() {
    with_environment(
        &[
            // Direct mode is the default only on Windows hosts; ask for it so
            // the test means the same on every host.
            (LINK_MODE_ENV, Some("direct")),
            (DIRECT_LINK_ENV, Some("1")),
            (DIRECT_ELF_ENV, Some("1")),
            (LINK_VERIFY_ENV, Some("1")),
        ],
        || {
            let dir = package(SOURCE);
            let before = image_link_stats();
            build(&dir, Some("x86_64-unknown-linux-gnu")).unwrap();
            let after = image_link_stats();
            assert!(after.written > before.written, "{after:?}");
            assert!(after.verified > before.verified, "{after:?}");

            let bytes = library(&dir);
            assert_eq!(&bytes[..4], b"\x7fELF");
            let info = elfinfo::validate(&bytes).expect("a library the loader accepts");
            for name in ["get_info", "get_version", "set_allocator_handle"] {
                assert!(
                    info.exports.iter().any(|e| e.name == name),
                    "{name} is not exported"
                );
            }
        },
    );
}

#[test]
fn without_the_opt_in_a_linux_build_goes_to_lld() {
    with_environment(
        &[
            (LINK_MODE_ENV, Some("direct")),
            (DIRECT_LINK_ENV, Some("1")),
            (DIRECT_ELF_ENV, None),
        ],
        || {
            let dir = package(SOURCE);
            let before = image_link_stats();
            build(&dir, Some("x86_64-unknown-linux-gnu")).unwrap();
            assert_eq!(image_link_stats().written, before.written);
            assert_eq!(&library(&dir)[..4], b"\x7fELF");
        },
    );
}
