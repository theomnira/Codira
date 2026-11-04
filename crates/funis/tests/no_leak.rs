//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 15, 2026
//!
//! Functionality:
//! - Links a thousand times in one process, mixing targets, formats, failed links and
//!   informational runs, and checks that memory, handles and threads stay flat.
//!
//! This is the only test in its binary, on purpose: the other tests start threads and open files,
//! which would show up in the numbers.

mod common;

use common::{Dir, args, elf_object, macho_object, shared_args, usage};
use object::Architecture;

const LINKS: usize = 1000;

#[test]
fn a_thousand_links_leave_nothing_behind() {
    let dir = Dir::new("no-leak");
    // Inputs of 200 KiB are mapped; the small ones are read. Both paths must be freed.
    let big = [
        (
            Architecture::X86_64,
            dir.write("x.o", &elf_object(Architecture::X86_64, "x", true, 200 << 10)),
        ),
        (
            Architecture::Aarch64,
            dir.write("a.o", &elf_object(Architecture::Aarch64, "a", true, 200 << 10)),
        ),
        (
            Architecture::Riscv64,
            dir.write("r.o", &elf_object(Architecture::Riscv64, "r", true, 200 << 10)),
        ),
    ];
    let small = [
        dir.write("sx.o", &elf_object(Architecture::X86_64, "s", false, 0)),
        dir.write("sa.o", &elf_object(Architecture::Aarch64, "s", false, 0)),
        dir.write("sr.o", &elf_object(Architecture::Riscv64, "s", false, 0)),
    ];
    let dup = dir.write("dup.o", &elf_object(Architecture::X86_64, "d", true, 0));
    let macho = dir.write("m.o", &macho_object(Architecture::Aarch64, "m", 64 << 10));
    std::fs::create_dir(dir.file("lib")).unwrap();
    dir.write(
        "lib/libSystem.tbd",
        b"--- !tapi-tbd\ntbd-version:     4\ntargets:         [ x86_64-macos, arm64-macos ]\n\
          install-name:    '/usr/lib/libSystem.B.dylib'\ncurrent-version: 1311\nexports:\n  \
          - targets:         [ x86_64-macos, arm64-macos ]\n    symbols:         [ dyld_stub_binder ]\n...\n",
    );
    let out = dir.file("out");

    let one = |i: usize| match i % 10 {
        // A failed link: the unwinding path.
        7 => {
            let err = funis::link(shared_args(&out, &[&big[0].1, &dup])).unwrap_err();
            assert!(!err.is_unsupported());
        }
        // A corrupt input, found by a worker thread.
        6 => {
            let bytes = std::fs::read(&big[0].1).unwrap();
            let bad = dir.write("bad.o", &bytes[..bytes.len() / 2]);
            let mut cmd = shared_args(&out, &[&bad]);
            cmd.extend(args(["-m", "elf_x86_64"]));
            assert!(funis::link(cmd).is_err());
        }
        // A link that asks for information, and one that asks for something unsupported.
        8 if i % 20 == 8 => {
            funis::link(args(["funis", "--version"])).unwrap();
        }
        8 => {
            assert!(funis::link(args(["funis", "--no-such-option"])).unwrap_err().is_unsupported());
        }
        9 => {
            let mut cmd = args(["ld64.funis", "-dylib", "-arch", "arm64", "-platform_version"]);
            cmd.extend(args(["macos", "11.0", "11.0", "-L"]));
            cmd.push(dir.file("lib").into());
            cmd.extend(args(["-lSystem", "-o"]));
            cmd.push(out.clone().into());
            cmd.push(macho.clone().into());
            funis::link(cmd).unwrap_or_else(|e| panic!("link {i}: {e}"));
        }
        k => {
            let (_, input) = &big[k % 3];
            let mut inputs = vec![input.as_path()];
            if k % 2 == 1 {
                inputs.push(small[k % 3].as_path());
            }
            funis::link(shared_args(&out, &inputs)).unwrap_or_else(|e| panic!("link {i}: {e}"));
        }
    };

    // The measurement can see a leak: leak 64 KiB 300 times and it must notice.
    let before = usage();
    for _ in 0..300 {
        Box::leak(vec![1u8; 64 << 10].into_boxed_slice());
    }
    let control = usage();
    assert!(
        control.private_bytes >= before.private_bytes + (8 << 20),
        "a deliberate leak of 19 MiB shows as {} bytes",
        control.private_bytes.saturating_sub(before.private_bytes)
    );

    // Warm up: first links allocate caches that are meant to stay (the allocator's, the
    // loader's), and bring the thread pool machinery of the standard library up.
    for i in 0..50 {
        one(i);
    }
    let start = usage();
    let mut samples = vec![start];
    for i in 0..LINKS {
        one(i);
        if (i + 1) % 250 == 0 {
            samples.push(usage());
        }
    }
    let mut end = *samples.last().unwrap();
    // A worker thread that has done its work can still be exiting when the threads are counted
    // (CI saw 3 against 2 once in several runs); one that leaked stays, so a second is enough.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while end.threads > start.threads && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
        end = usage();
    }
    eprintln!("usage after warm-up: {start:?}");
    for (n, s) in samples.iter().enumerate().skip(1) {
        eprintln!("usage after {:4} links: {s:?}", n * 250);
    }

    assert!(
        end.handles <= start.handles,
        "handles grew from {} to {} over {LINKS} links",
        start.handles,
        end.handles
    );
    assert!(
        end.threads <= start.threads,
        "threads grew from {} to {} over {LINKS} links",
        start.threads,
        end.threads
    );
    // Each link maps or reads 200 KiB; a leak of any of it would show as 190+ MiB. The slack is
    // for the allocator's own fragmentation and caches.
    let grown = end.private_bytes.saturating_sub(start.private_bytes);
    assert!(
        grown < 6 << 20,
        "committed memory grew by {grown} bytes over {LINKS} links: {samples:?}"
    );
    // No temporary file is left next to the output.
    let leftovers: Vec<_> = std::fs::read_dir(&dir.0)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left behind: {leftovers:?}");
}
