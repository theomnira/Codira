//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Tests of relocation (experimental, opt-in): live objects move out of
//!   sparse size-class blocks and every heap reference to them is
//!   rewritten, while objects a frame, a root or a pin refers to stay put;
//!   and a fragmentation workload that fits a heap limit only with it.
use std::sync::atomic::{AtomicPtr, Ordering};

use codira_runtime::{GarbageCollector, StructRef};
use codira_test::CompileAndRunTestDriver;

static GC: AtomicPtr<GarbageCollector> = AtomicPtr::new(std::ptr::null_mut());

fn gc() -> &'static GarbageCollector {
    // Safety: set before Codira code runs; the runtime outlives the calls.
    unsafe { &*GC.load(Ordering::SeqCst) }
}

extern "C" fn collect_now() {
    let _ = gc().collect_blocking();
}

extern "C" fn moved_last() -> i64 {
    gc().last_relocation().moved as i64
}

const SOURCE: &str = r"
public class Tag { label: i64 }
public class Item { id: i64, tag: Tag, next: Item? }
public class Holder { items: [Item] }
extern func collect_now();
extern func moved_last() -> i64;

// `n` items, of which every 16th is kept in `items` (and linked to the
// previous kept one); the rest is garbage, so the blocks the kept ones live
// in end up sparse.
public func build(n: i64) -> Holder {
    let h = Holder { items: [] };
    var prev: Item? = nil;
    var i = 0;
    while i < n {
        let item = Item { id: i, tag: Tag { label: i * 3 }, next: prev };
        if i % 16 == 0 {
            h.items.push(item);
            prev = item;
        };
        i += 1;
    };
    h
}

public func check(h: Holder) -> i64 {
    var bad = 0;
    var k = 0;
    while k < h.items.len() {
        let item = h.items[k];
        if item.id != (k as i64) * 16 { bad += 1 };
        if item.tag.label != item.id * 3 { bad += 1 };
        if k > 0 {
            if let prev = item.next {
                if prev.id != item.id - 16 { bad += 1 };
            } else {
                bad += 1;
            }
        };
        k += 1;
    };
    bad
}

// Builds, collects (moving the kept items), overwrites the freed cells with
// other values, and checks everything; returns the number of moved objects
// times 1000 plus the number of mismatches.
public func relocate(n: i64) -> i64 {
    let h = build(n);
    collect_now();
    let moved = moved_last();
    let junk = build(n);
    moved * 1000 + check(h) + check(junk)
}

public func item(id: i64) -> Item { Item { id: id, tag: Tag { label: id * 3 }, next: nil } }
public func label_of(item: Item) -> i64 { item.tag.label }
";

fn driver() -> CompileAndRunTestDriver {
    let driver = CompileAndRunTestDriver::new(SOURCE, |builder| {
        builder
            .compaction(true)
            .insert_fn("collect_now", collect_now as extern "C" fn())
            .insert_fn("moved_last", moved_last as extern "C" fn() -> i64)
    })
    .expect("Failed to build test driver");
    GC.store(
        driver.runtime.gc() as *const GarbageCollector as *mut GarbageCollector,
        Ordering::SeqCst,
    );
    driver
}

/// Heap references (array elements and object fields) to moved objects are
/// rewritten; the moved objects keep their contents.
#[test]
fn heap_references_follow_moved_objects() {
    let driver = driver();
    let result: i64 = driver.runtime.invoke("relocate", (20_000i64,)).unwrap();
    let (moved, bad) = (result / 1000, result % 1000);
    assert_eq!(bad, 0, "a reference to a moved object was not rewritten");
    assert!(moved > 100, "nothing moved ({moved})");
}

/// A rooted (pinned) object stays where it is, and a host pin works the
/// same way.
#[test]
fn rooted_and_pinned_objects_do_not_move() {
    let driver = driver();
    let gc = driver.runtime.gc();
    // Fill blocks with garbage around the two objects so their blocks are
    // sparse after the collection.
    let rooted = {
        let item: StructRef<'_> = driver.runtime.invoke("item", (7i64,)).unwrap();
        item.root()
    };
    let pinned: StructRef<'_> = driver.runtime.invoke("item", (9i64,)).unwrap();
    let pinned_raw = pinned.clone().into_raw();
    // Safety: the object is live.
    let pinned_address = unsafe { pinned_raw.get_ptr() } as usize;
    let rooted_address = unsafe { rooted.as_ref(&driver.runtime).into_raw().get_ptr() } as usize;
    let handle: codira_memory::gc::GcPtr = (pinned_address as *const u8)
        .wrapping_sub(32)
        .cast::<*mut std::ffi::c_void>()
        .into();
    let _ = &pinned_raw;
    gc.pin(handle);
    drop(pinned);
    let _: i64 = driver.runtime.invoke("relocate", (20_000i64,)).unwrap();
    assert!(gc.last_relocation().moved > 0);
    assert!(gc.holds_object_at(pinned_address));
    assert!(gc.holds_object_at(rooted_address));
    // The payloads are where they were, with their contents.
    // Safety: both objects are alive (pinned, rooted).
    unsafe {
        assert_eq!(*(pinned_address as *const i64), 9);
        assert_eq!(*(rooted_address as *const i64), 7);
    }
    let label: i64 = driver
        .runtime
        .invoke("label_of", (rooted.as_ref(&driver.runtime),))
        .unwrap();
    assert_eq!(label, 21);
    gc.unpin(handle);
}

const FRAGMENT_SOURCE: &str = r"
public class Small { a: i64, b: i64 }
public class Wide { f0: i64, f1: i64, f2: i64, f3: i64, f4: i64, f5: i64, f6: i64,
                    f7: i64, f8: i64, f9: i64, f10: i64, f11: i64, f12: i64, f13: i64 }
public class Bag { smalls: [Small], wides: [Wide] }
extern func phase_done();

// Phase 1 leaves every size-class block of `Small` one sixteenth full; phase
// 2 then needs blocks of another size class.
public func fragment(smalls: i64, wides: i64) -> i64 {
    let bag = Bag { smalls: [], wides: [] };
    var i = 0;
    while i < smalls {
        let s = Small { a: i, b: i * 5 };
        if i % 16 == 0 { bag.smalls.push(s) };
        i += 1;
    };
    phase_done();
    var j = 0;
    while j < wides {
        bag.wides.push(Wide { f0: j, f1: 0, f2: 0, f3: 0, f4: 0, f5: 0, f6: 0,
                              f7: 0, f8: 0, f9: 0, f10: 0, f11: 0, f12: 0, f13: j * 2 });
        j += 1;
    };
    phase_done();
    var bad = 0;
    var k = 0;
    while k < bag.smalls.len() {
        let s = bag.smalls[k];
        if s.a != (k as i64) * 16 || s.b != s.a * 5 { bad += 1 };
        k += 1;
    };
    k = 0;
    while k < bag.wides.len() {
        let w = bag.wides[k];
        if w.f0 != k as i64 || w.f13 != w.f0 * 2 { bad += 1 };
        k += 1;
    };
    bad
}
";

const SMALLS: i64 = 400_000;
const WIDES: i64 = 60_000;
const LIMIT: usize = 16 << 20;

/// Collections the fragmentation workload's host function made, with what
/// each reported.
static REPORTS: std::sync::Mutex<Vec<codira_memory::gc::CollectionReport>> =
    std::sync::Mutex::new(Vec::new());

extern "C" fn phase_done() {
    gc().collect();
    REPORTS.lock().unwrap().push(gc().last_collection());
}

fn fragment_driver(compaction: bool, limit: Option<usize>) -> CompileAndRunTestDriver {
    let driver = CompileAndRunTestDriver::new(FRAGMENT_SOURCE, |builder| {
        let builder = builder
            .compaction(compaction)
            .insert_fn("phase_done", phase_done as extern "C" fn());
        match limit {
            Some(limit) => builder.heap_limit(limit),
            None => builder,
        }
    })
    .expect("Failed to build test driver");
    GC.store(
        driver.runtime.gc() as *const GarbageCollector as *mut GarbageCollector,
        Ordering::SeqCst,
    );
    driver
}

/// Without a limit, collecting after each phase: with relocation the heap
/// holds far less memory, and the pause is reported for both.
#[test]
fn fragmentation_uses_less_memory_with_relocation() {
    let _serial = FRAGMENT_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut results = Vec::new();
    for compaction in [false, true] {
        REPORTS.lock().unwrap().clear();
        let driver = fragment_driver(compaction, None);
        let bad: i64 = driver.runtime.invoke("fragment", (SMALLS, WIDES)).unwrap();
        assert_eq!(bad, 0, "compaction {compaction}: wrong contents");
        let reports = REPORTS.lock().unwrap().clone();
        eprintln!("compaction {compaction}: {reports:?}");
        results.push(reports);
    }
    let (plain, moving) = (&results[0], &results[1]);
    assert!(moving[0].moved > 0);
    assert!(
        moving[1].committed * 2 < plain[1].committed,
        "relocation did not reduce the heap: {} vs {} bytes",
        moving[1].committed,
        plain[1].committed
    );
}

static FRAGMENT_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Under a 16 MiB limit the workload completes with relocation; without
/// it, phase 2 cannot get blocks of its size class and the process aborts
/// (run in a child process).
#[test]
fn fragmentation_fits_a_heap_limit_only_with_relocation() {
    if let Ok(mode) = std::env::var("CODIRA_FRAGMENT_CHILD") {
        let driver = fragment_driver(mode == "compaction", Some(LIMIT));
        let bad: i64 = driver.runtime.invoke("fragment", (SMALLS, WIDES)).unwrap();
        assert_eq!(bad, 0);
        assert!(driver.runtime.gc().committed_bytes() <= LIMIT);
        println!("CHILD-OK committed {}", driver.runtime.gc().committed_bytes());
        return;
    }
    let run = |mode: &str| {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fragmentation_fits_a_heap_limit_only_with_relocation",
                "--nocapture",
            ])
            .env("CODIRA_FRAGMENT_CHILD", mode)
            .output()
            .expect("failed to run the child")
    };
    let moving = run("compaction");
    let moving_out = String::from_utf8_lossy(&moving.stdout);
    assert!(
        moving.status.success() && moving_out.contains("CHILD-OK"),
        "with relocation: {moving_out}\n{}",
        String::from_utf8_lossy(&moving.stderr)
    );
    let plain = run("plain");
    assert!(
        !plain.status.success() && !String::from_utf8_lossy(&plain.stdout).contains("CHILD-OK"),
        "the non-moving collector fit the limit too"
    );
}
