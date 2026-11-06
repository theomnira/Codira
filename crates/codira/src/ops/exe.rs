//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - `codira build --exe`: one executable file that runs a Codira package with
//!   nothing else installed.
//!
//! The executable is `codira-launcher` -- a small program that is the Codira
//! runtime and nothing else -- with the package's compiled libraries appended
//! to it. The same layout `bun build --compile` and `deno compile` use:
//!
//! ```text
//! [launcher][entry name][library count]{[name][bytes]}...[payload offset: u64][magic]
//! ```
//!
//! On start the launcher reads the payload off the end of its own file and
//! unpacks the libraries into a cache directory named after the payload's
//! hash, once. Later runs of the same executable go straight to loading. The
//! runtime loads them exactly as `codira start` would and calls the entry
//! point: `main` returning nothing exits with 0, returning an integer or
//! `bool` exits with that value.
//!
//! Proof certificates (`codira build --certify`, `<library>.cert.json`) travel
//! in the payload next to their libraries, so `codira certify app.exe` can
//! check the libraries the executable will actually load.
//!
//! The libraries are unpacked rather than loaded from memory because every
//! platform's loader takes a file; that is the one cost of a single-file
//! executable, paid on the first run only.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context};
use codira_runtime::Runtime;

/// Marks the end of an executable that carries a payload.
const MAGIC: &[u8; 8] = b"CODIRAX1";

/// The launcher binary's file name.
pub fn launcher_name() -> String {
    format!("codira-launcher{}", std::env::consts::EXE_SUFFIX)
}

/// Where the launcher is: `CODIRA_LAUNCHER`, else next to this executable.
pub fn find_launcher() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("CODIRA_LAUNCHER") {
        return Ok(path.into());
    }
    let exe = std::env::current_exe().context("could not locate the codira executable")?;
    let launcher = exe.with_file_name(launcher_name());
    if launcher.is_file() {
        Ok(launcher)
    } else {
        bail!(
            "could not find `{}` next to '{}'; it ships with codira (set CODIRA_LAUNCHER to its \
             path)",
            launcher_name(),
            exe.display()
        )
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Writes `output`: `launcher` followed by every `*.codiralib` in
/// `library_dir` (and its proof certificate, if it has one), with `entry`
/// as the function to run.
pub fn bundle(
    launcher: &Path,
    library_dir: &Path,
    entry: &str,
    output: &Path,
) -> anyhow::Result<()> {
    let mut libraries: Vec<PathBuf> = std::fs::read_dir(library_dir)
        .with_context(|| format!("could not read '{}'", library_dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|e| e == "codiralib")
                || path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".codiralib.cert.json"))
        })
        .collect();
    libraries.sort();
    if !libraries
        .iter()
        .any(|path| path.file_stem().is_some_and(|s| s == "mod"))
    {
        bail!("'{}' has no mod.codiralib to run", library_dir.display());
    }

    let mut out = std::fs::read(launcher)
        .with_context(|| format!("could not read the launcher '{}'", launcher.display()))?;
    let payload_start = out.len() as u64;
    put_bytes(&mut out, entry.as_bytes());
    out.extend_from_slice(&(libraries.len() as u64).to_le_bytes());
    for library in &libraries {
        let name = library
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("'{}' has no usable file name", library.display()))?;
        put_bytes(&mut out, name.as_bytes());
        put_bytes(&mut out, &std::fs::read(library)?);
    }
    out.extend_from_slice(&payload_start.to_le_bytes());
    out.extend_from_slice(MAGIC);

    std::fs::write(output, &out)
        .with_context(|| format!("could not write '{}'", output.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// A payload read back from an executable.
pub(crate) struct Payload {
    pub(crate) entry: String,
    /// Every file carried, by name: the libraries and their certificates.
    pub(crate) libraries: Vec<(String, Vec<u8>)>,
    hash: u64,
}

fn take_u64(input: &mut &[u8]) -> anyhow::Result<u64> {
    let mut word = [0u8; 8];
    input.read_exact(&mut word)?;
    Ok(u64::from_le_bytes(word))
}

fn take_bytes(input: &mut &[u8]) -> anyhow::Result<Vec<u8>> {
    let len = usize::try_from(take_u64(input)?)?;
    let mut data = vec![0u8; len];
    input.read_exact(&mut data)?;
    Ok(data)
}

/// The payload appended to `exe`, or `None` if it carries none.
pub(crate) fn read_payload(exe: &Path) -> anyhow::Result<Option<Payload>> {
    let bytes = std::fs::read(exe)?;
    if bytes.len() < 16 || &bytes[bytes.len() - 8..] != MAGIC {
        return Ok(None);
    }
    let offset_bytes: [u8; 8] = bytes[bytes.len() - 16..bytes.len() - 8].try_into()?;
    let start = usize::try_from(u64::from_le_bytes(offset_bytes))?;
    let mut payload = bytes
        .get(start..bytes.len() - 16)
        .ok_or_else(|| anyhow!("the executable's payload offset is out of range"))?;

    let mut hasher = DefaultHasher::new();
    payload.hash(&mut hasher);

    let entry = String::from_utf8(take_bytes(&mut payload)?)?;
    let count = take_u64(&mut payload)?;
    let mut libraries = Vec::new();
    for _ in 0..count {
        let name = String::from_utf8(take_bytes(&mut payload)?)?;
        if name.contains(['/', '\\']) || name == ".." {
            bail!("the executable's payload names a library '{name}' outside its directory");
        }
        libraries.push((name, take_bytes(&mut payload)?));
    }
    Ok(Some(Payload {
        entry,
        libraries,
        hash: hasher.finish(),
    }))
}

/// Unpacks `payload` into its cache directory, once, and returns the
/// directory.
fn unpack(payload: &Payload) -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir()
        .join("codira-exe")
        .join(format!("{:016x}", payload.hash));
    let complete = dir.join(".complete");
    if complete.is_file() {
        return Ok(dir);
    }
    // Unpacked beside the final directory and renamed into place, so a run
    // that is interrupted, or one racing another first run, never loads a
    // half-written library.
    let staging = dir.with_extension(format!("tmp{}", std::process::id()));
    std::fs::create_dir_all(&staging)?;
    for (name, bytes) in &payload.libraries {
        std::fs::write(staging.join(name), bytes)?;
    }
    std::fs::write(staging.join(".complete"), b"")?;
    if std::fs::rename(&staging, &dir).is_err() {
        // Another first run got there first; its copy is identical.
        let _ = std::fs::remove_dir_all(&staging);
        if !complete.is_file() {
            bail!(
                "could not unpack the executable's libraries to '{}'",
                dir.display()
            );
        }
    }
    Ok(dir)
}

/// What a launcher run produced.
pub enum Launched {
    /// The executable carries no payload: it is the bare launcher.
    NoPayload,
    /// The entry point ran; the process should exit with this code.
    Exited(i32),
}

/// Runs the package embedded in this executable.
pub fn run_embedded() -> anyhow::Result<Launched> {
    let exe = std::env::current_exe().context("could not locate this executable")?;
    let Some(payload) = read_payload(&exe)? else {
        return Ok(Launched::NoPayload);
    };
    let dir = unpack(&payload)?;
    // The executable's own arguments belong to the program, so the heap
    // settings come from the environment (`CODIRA_HEAP_LIMIT`,
    // `CODIRA_GC_GROWTH`) or the default.
    let heap = crate::ops::heap::HeapSettings::resolve(&Default::default(), None)?;
    heap.warn_if_unlimited();
    let runtime = unsafe {
        heap.apply(Runtime::builder(dir.join("mod.codiralib")))
            .finish()
    }
    .map_err(|e| anyhow!("could not load the program: {e}"))?;
    let definition = runtime
        .get_function_definition(&payload.entry)
        .ok_or_else(|| anyhow!("the program has no entry point `{}`", payload.entry))?;
    let return_type = &definition.prototype.signature.return_type;

    macro_rules! exit_with {
        ($($ty:ty),+) => {
            $(
                if return_type.equals::<$ty>() {
                    let code: $ty = runtime
                        .invoke(&payload.entry, ())
                        .map_err(|e| anyhow!("{e}"))?;
                    // An exit code outside `i32` cannot be reported faithfully,
                    // so it is reported as failure rather than truncated.
                    return Ok(Launched::Exited(i32::try_from(code).unwrap_or(1)));
                }
            )+
        };
    }
    if return_type.equals::<()>() {
        let (): () = runtime
            .invoke(&payload.entry, ())
            .map_err(|e| anyhow!("{e}"))?;
        return Ok(Launched::Exited(0));
    }
    if return_type.equals::<bool>() {
        let ok: bool = runtime
            .invoke(&payload.entry, ())
            .map_err(|e| anyhow!("{e}"))?;
        return Ok(Launched::Exited(i32::from(!ok)));
    }
    exit_with!(i8, i16, i32, i64, u8, u16, u32, u64);
    bail!(
        "the entry point `{}` returns `{}`; an executable's entry point returns nothing, an \
         integer (the exit code) or `bool` (true is success)",
        payload.entry,
        return_type.name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundle_reads_back_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("launcher.bin");
        std::fs::write(&launcher, b"\x7fELF-ish launcher bytes").unwrap();
        let libs = dir.path().join("libs");
        std::fs::create_dir(&libs).unwrap();
        std::fs::write(libs.join("mod.codiralib"), b"main library").unwrap();
        std::fs::write(libs.join("util.codiralib"), b"helper library").unwrap();
        std::fs::write(libs.join("notes.txt"), b"not a library").unwrap();

        let exe = dir.path().join("app");
        bundle(&launcher, &libs, "main", &exe).unwrap();

        let bytes = std::fs::read(&exe).unwrap();
        assert!(bytes.starts_with(b"\x7fELF-ish launcher bytes"));
        let payload = read_payload(&exe).unwrap().expect("a payload");
        assert_eq!(payload.entry, "main");
        assert_eq!(
            payload.libraries,
            vec![
                ("mod.codiralib".to_owned(), b"main library".to_vec()),
                ("util.codiralib".to_owned(), b"helper library".to_vec()),
            ]
        );
        assert!(read_payload(&launcher).unwrap().is_none());
    }
}
