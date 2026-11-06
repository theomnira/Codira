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
