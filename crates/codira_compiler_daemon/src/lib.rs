//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{
    io::stderr,
    path::Path,
    sync::{mpsc::channel, Arc},
    time::{Duration, Instant},
};

use codira_compiler::{compute_source_relative_path, is_source_file, Config, DisplayColor, Driver};
use notify::{
    event::{ModifyKind, RenameMode},
    EventKind, RecursiveMode, Watcher,
};

/// Recompiles and reports how long it took.
///
/// Watch mode is the answer to "the compiler is slow": a one-shot `codira
/// build` pays ~40 ms of process startup (loading a ~65 MB
/// statically-linked-LLVM binary) and ~40 ms of linking before any
/// compilation happens at all, and throws the salsa cache away afterwards. A
/// watching process pays both once, then re-runs only the queries an edit
/// actually invalidated.
///
/// The elapsed time is printed because that difference is invisible
/// otherwise -- the user sees a rebuild happen either way, and has no way to
/// tell that the second one cost a fraction of the first.
fn recompile(driver: &mut Driver, display_color: DisplayColor) -> Result<(), anyhow::Error> {
    let started = Instant::now();
    if !driver.emit_diagnostics(&mut stderr(), display_color)? {
        driver.write_all_assemblies(false)?;
    }
    let elapsed = started.elapsed();

    // Sub-millisecond rebuilds are normal here once the cache is warm, so
    // milliseconds alone would round most of them to "0 ms" and read as a
    // measurement failure rather than a fast rebuild.
    if elapsed.as_millis() >= 1 {
        println!("rebuilt in {:.2} ms", elapsed.as_secs_f64() * 1_000.0);
    } else {
        println!("rebuilt in {} us", elapsed.as_micros());
    }
    Ok(())
}

/// Compiles and watches the package at the specified path. Recompiles changes
/// that occur.
pub fn compile_and_watch_manifest(
    manifest_path: &Path,
    config: Config,
    display_color: DisplayColor,
) -> Result<bool, anyhow::Error> {
    // Create the compiler driver
    let (package, mut driver) = Driver::with_package_path(manifest_path, config)?;

    // Start watching the source directory
    let (watcher_tx, watcher_rx) = channel();
    let mut watcher = notify::recommended_watcher(watcher_tx)?;
    let source_directory = package.source_directory();

    watcher.watch(&source_directory, RecursiveMode::Recursive)?;
    println!("Watching: {}", source_directory.display());

    // Emit all current errors, and write the assemblies if no errors occured
    recompile(&mut driver, display_color)?;

    // Insert Ctrl+C handler so we can gracefully quit
    let should_quit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let r = should_quit.clone();
    ctrlc::set_handler(move || {
        r.store(true, std::sync::atomic::Ordering::SeqCst);
    })
    .expect("error setting ctrl-c handler");

    // Start watching filesystem events. notify 5 delivers raw (undebounced)
    // events; salsa's early-exit makes a redundant `update_file` with
    // unchanged contents cheap, so no separate debouncing layer is needed.
    while !should_quit.load(std::sync::atomic::Ordering::SeqCst) {
        if let Ok(Ok(event)) = watcher_rx.recv_timeout(Duration::from_millis(1)) {
            match event.kind {
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if event.paths.len() == 2 => {
                    // Renaming is done by changing the relative path of the original source file
                    // but not modifying any text. This ensures that most of the
                    // cache for the renamed file stays alive. This is
                    // effectively a rename of the file_id in the database.
                    let (from, to) = (&event.paths[0], &event.paths[1]);
                    let from_relative_path = compute_source_relative_path(&source_directory, from)?;
                    let to_relative_path = compute_source_relative_path(&source_directory, to)?;

                    log::info!("Renaming {} to {}", from_relative_path, to_relative_path,);
                    driver.rename(from_relative_path, to_relative_path);
                    recompile(&mut driver, display_color)?;
                }
                // A rename observed as two separate From/To events (or a
                // platform that can't pair them) degrades to remove+create:
                // correctness is identical, only the file_id-preserving
                // cache optimization above is lost.
                EventKind::Modify(ModifyKind::Name(RenameMode::From)) | EventKind::Remove(_) => {
                    for path in event.paths.iter().filter(|p| is_source_file(p)) {
                        // Simply remove the source file from the source root
                        let relative_path = compute_source_relative_path(&source_directory, path)?;
                        log::info!("Removing {}", relative_path);
                        // TODO: Remove assembly files if there are no files referencing it.
                        driver.remove_file(relative_path);
                        driver.emit_diagnostics(&mut stderr(), display_color)?;
                    }
                }
                EventKind::Modify(ModifyKind::Name(RenameMode::To)) | EventKind::Create(_) => {
                    for path in event.paths.iter().filter(|p| is_source_file(p)) {
                        let relative_path = compute_source_relative_path(&source_directory, path)?;
                        let file_contents = std::fs::read_to_string(path)?;
                        log::info!("Creating {}", relative_path);
                        driver.add_file(relative_path, file_contents);
                        recompile(&mut driver, display_color)?;
                    }
                }
                EventKind::Modify(_) => {
                    for path in event.paths.iter().filter(|p| is_source_file(p)) {
                        let relative_path = compute_source_relative_path(&source_directory, path)?;
                        let file_contents = std::fs::read_to_string(path)?;
                        log::info!("Modifying {}", relative_path);
                        driver.update_file(relative_path, file_contents);
                        recompile(&mut driver, display_color)?;
                    }
                }
                _ => {}
            }
        }
    }

    Ok(true)
}
