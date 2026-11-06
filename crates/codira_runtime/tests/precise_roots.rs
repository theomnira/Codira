//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of precise stack roots: integers that look like
//!   pointers into the heap no longer keep objects alive, references held
//!   only by compiled frames survive collections at every safepoint, and
//!   the conservative fallback still retains what it always did.
//!
//! The root mode and stress mode are process-wide, so the tests that change
//! them run one at a time.
use std::sync::{
    atomic::{AtomicPtr, Ordering},
    Mutex,
};

use codira_memory::gc::frames::{self, RootMode};
use codira_runtime::{GarbageCollector, Runtime};
use codira_test::CompileAndRunTestDriver;

/// Serializes the tests: root and stress modes are global.
static SERIAL: Mutex<()> = Mutex::new(());

/// The collector the host functions below act on.
static GC: AtomicPtr<GarbageCollector> = AtomicPtr::new(std::ptr::null_mut());

fn gc() -> &'static GarbageCollector {
    // Safety: set to the live runtime's collector before any Codira code
    // runs, and the runtime outlives every call.
    unsafe { &*GC.load(Ordering::SeqCst) }
}

/// Addresses the host hands to the probe, one per `take_address` call.
static ADDRESSES: Mutex<Vec<u64>> = Mutex::new(Vec::new());

extern "C" fn take_address() -> u64 {
    ADDRESSES.lock().unwrap().pop().expect("an address per call")
}

extern "C" fn collect_now() {
    let _ = gc().collect_blocking();
}

extern "C" fn is_live(address: u64) -> i64 {
    i64::from(gc().holds_object_at(address as usize))
}

/// Number of objects whose interior addresses the probe holds as integers.
const OBJECTS: usize = 40;

/// A program that keeps `OBJECTS` integers, each the address of a field of
/// an object nothing else references, live across a collection, and counts
/// how many of those objects are still allocated afterwards.
fn integer_probe_source() -> String {
    let fields: String = (0..16).map(|i| format!("f{i}: i64, ")).collect();
    let inits: String = (0..16).map(|i| format!("f{i}: k + {i}, ")).collect();
    let lets: String = (0..OBJECTS)
        .map(|k| format!("    let a{k} = take_address();\n"))
        .collect();
    let checks: String = (0..OBJECTS)
        .map(|k| format!("    live += is_live(a{k});\n"))
        .collect();
    format!(
        r"
public class Big {{ {fields} }}
extern func take_address() -> u64;
extern func collect_now();
extern func is_live(a: u64) -> i64;

public func make(k: i64) -> Big {{
    Big {{ {inits} }}
}}

// It also keeps an object of its own across the collection: a function that
// holds no reference across a call gets no frame record, and its frame is
// scanned conservatively.
public func probe() -> i64 {{
    let own = make(7);
{lets}    collect_now();
    var live = own.f0 - 7;
{checks}    live
}}
"
    )
}

fn driver_for(source: &str) -> CompileAndRunTestDriver {
    let driver = CompileAndRunTestDriver::new(source, |builder| {
        builder
            .insert_fn("take_address", take_address as extern "C" fn() -> u64)
            .insert_fn("collect_now", collect_now as extern "C" fn())
            .insert_fn("is_live", is_live as extern "C" fn(u64) -> i64)
    })
    .expect("Failed to build test driver");
    GC.store(
        driver.runtime.gc() as *const GarbageCollector as *mut GarbageCollector,
        Ordering::SeqCst,
    );
    driver
}

/// Allocates `OBJECTS` objects, keeps only the address of a field of
/// each, as an integer, and runs the probe on them.
#[inline(never)]
fn probe(runtime: &Runtime) -> i64 {
    fill_addresses(runtime);
    runtime.invoke("probe", ()).unwrap()
}

#[inline(never)]
fn fill_addresses(runtime: &Runtime) {
    let mut addresses = Vec::new();
    for k in 0..OBJECTS as i64 {
        let object: codira_runtime::StructRef<'_> = runtime.invoke("make", (k,)).unwrap();
        // Safety: the object is live (nothing has collected since).
        let payload = unsafe { object.into_raw().get_ptr() } as u64;
        addresses.push(payload + (k as u64 % 16) * 8);
    }
    *ADDRESSES.lock().unwrap() = addresses;
}

/// With precise roots, integers in compiled frames with a frame record (a
/// function that holds a reference across a call) are not roots: most of
/// the objects are freed. (Integers that sit in a callee-saved register at
/// the safepoint still are -- those registers may hold a host frame's
/// references, so they are scanned conservatively -- hence "most".) With
/// the conservative scan every one of them is retained.
#[test]
fn integers_that_look_like_pointers_do_not_retain_objects() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let driver = driver_for(&integer_probe_source());

    frames::set_root_mode(RootMode::Conservative);
    let conservative = probe(&driver.runtime);
    frames::set_root_mode(RootMode::Precise);
    let precise = probe(&driver.runtime);
    eprintln!("retained: conservative {conservative}, precise {precise} of {OBJECTS}");

    assert_eq!(
        conservative, OBJECTS as i64,
        "the conservative scan should retain every object"
    );
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        assert!(
            precise <= 8,
            "precise roots retained {precise} of {OBJECTS} objects held only by integers"
        );
    } else {
        // No frame is left out of the conservative scan on this target.
        assert_eq!(precise, OBJECTS as i64);
    }
}

const KEEP_SOURCE: &str = r"
public class Leaf { a: i64, b: i64 }
public class Node { leaf: Leaf, next: Node? }
extern func collect_now();

func chain(n: i64) -> Node? {
    var head: Node? = nil;
    var i = 0;
    while i < n {
        head = Node { leaf: Leaf { a: i, b: i * 3 }, next: head };
        i += 1;
    };
    head
}

func check(list: Node?, n: i64) -> i64 {
    var bad = 0;
    var cur = list;
    var i = n - 1;
    loop {
        if let node = cur {
            if node.leaf.a != i { bad += 1 };
            if node.leaf.b != i * 3 { bad += 1 };
            cur = node.next;
            i -= 1;
        } else {
            break;
        }
    };
    if i != 0 - 1 { bad += 1000 };
    bad
}

// Builds lists held only by locals, collects in between, allocates garbage
// that reuses whatever was freed, and checks the lists.
public func keep(rounds: i64) -> i64 {
    var bad = 0;
    var r = 0;
    while r < rounds {
        let first = chain(50);
        collect_now();
        let junk = chain(200);
        let second = chain(30);
        collect_now();
        let more_junk = chain(100);
        bad += check(first, 50) + check(second, 30);
        bad += check(junk, 200) + check(more_junk, 100);
        r += 1;
    };
    bad
}
";

/// References held only by compiled frames survive collections at every
/// safepoint (stress mode: every allocation and every poll collects), in
/// both root modes.
#[test]
fn frame_references_survive_collections_at_every_safepoint() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let driver = driver_for(KEEP_SOURCE);
    for mode in [RootMode::Precise, RootMode::Conservative] {
        frames::set_root_mode(mode);
        frames::set_stress(1);
        let bad: i64 = driver.runtime.invoke("keep", (3i64,)).unwrap();
        frames::set_stress(0);
        assert_eq!(bad, 0, "{mode:?}: a collection freed an object a frame still used");
    }
    frames::set_root_mode(RootMode::Precise);
}

extern "C" fn slow_foreign(millis: i64) -> i64 {
    std::thread::sleep(std::time::Duration::from_millis(millis as u64));
    millis
}

const FOREIGN_SOURCE: &str = r"
public class Leaf { a: i64, b: i64 }
@noalloc extern func slow_foreign(millis: i64) -> i64;
extern func slow_running(millis: i64) -> i64;

func check(leaf: Leaf, k: i64) -> i64 {
    var bad = 0;
    if leaf.a != k { bad += 1 };
    if leaf.b != k * 7 { bad += 1 };
    bad
}

// Holds objects only in its frame across a foreign call, with garbage
// allocated before and after so a cell freed by mistake gets reused.
public func across_foreign(millis: i64) -> i64 {
    let keep = Leaf { a: 11, b: 77 };
    var i = 0;
    while i < 100 { let junk = Leaf { a: 0 - i, b: 0 - i }; i += 1 };
    let slept = slow_foreign(millis);
    i = 0;
    while i < 1000 { let junk = Leaf { a: 0 - i, b: 0 - i }; i += 1 };
    check(keep, 11) + (slept - millis)
}

public func across_running(millis: i64) -> i64 {
    slow_running(millis)
}
";

/// A thread in a call to an `extern` function declared `@noalloc` is
/// parked: a collection on another thread runs without waiting for the call
/// to return, and the caller's objects survive it. A call to an `extern`
/// function without the assertion still holds a collection up.
#[test]
fn collections_do_not_wait_for_foreign_calls() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let driver = CompileAndRunTestDriver::new(FOREIGN_SOURCE, |builder| {
        builder
            .insert_fn("slow_foreign", slow_foreign as extern "C" fn(i64) -> i64)
            .insert_fn("slow_running", slow_foreign as extern "C" fn(i64) -> i64)
    })
    .expect("Failed to build test driver");
    let gc = driver.runtime.gc();
    const SLEEP: i64 = 400;

    let collect_during = |function: &str| -> std::time::Duration {
        std::thread::scope(|s| {
            let collector = s.spawn(|| {
                // Let the call reach the foreign function first.
                std::thread::sleep(std::time::Duration::from_millis(100));
                let start = std::time::Instant::now();
                let _ = gc.collect_blocking();
                start.elapsed()
            });
            let bad: i64 = driver.runtime.invoke(function, (SLEEP,)).unwrap();
            assert_eq!(bad, if function == "across_foreign" { 0 } else { SLEEP });
            collector.join().unwrap()
        })
    };
    let parked = collect_during("across_foreign");
    let running = collect_during("across_running");
    assert!(
        parked < std::time::Duration::from_millis(100),
        "a collection waited {parked:?} for a foreign call"
    );
    assert!(
        running >= std::time::Duration::from_millis(200),
        "a collection did not wait for a running call ({running:?})"
    );
}

const SPIN_SOURCE: &str = r"
// A loop that neither allocates nor calls, and whose trip count nothing
// bounds on entry (it depends on the values it computes): only its
// back-edge poll lets a collection on another thread stop it.
public func spin(n: i64, seed: i64) -> i64 {
    var x = seed;
    var i = 0;
    while i < n || (x & 7) != 0 {
        x = x * 6364136223846793005 + 1442695040888963407;
        if (x >> 60) == 3 { x = x ^ i };
        i += 1;
    };
    x
}
";

/// Before back-edge polls, a collection waited for such a loop to end.
#[test]
fn a_loop_without_safepoints_does_not_hold_up_a_collection() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let driver = CompileAndRunTestDriver::new(SPIN_SOURCE, |builder| builder)
        .expect("Failed to build test driver");
    let gc = driver.runtime.gc();
    // Long enough to run for well over a second in a debug test build.
    let n: i64 = 1_500_000_000;
    let (latency, elapsed) = std::thread::scope(|s| {
        let collector = s.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let start = std::time::Instant::now();
            let _ = gc.collect_blocking();
            start.elapsed()
        });
        let start = std::time::Instant::now();
        let _: i64 = driver.runtime.invoke("spin", (n, 7i64)).unwrap();
        (collector.join().unwrap(), start.elapsed())
    });
    assert!(
        elapsed > std::time::Duration::from_millis(400),
        "the loop ended too soon ({elapsed:?}) to tell"
    );
    assert!(
        latency < std::time::Duration::from_millis(50),
        "a collection waited {latency:?} for a loop (the loop ran {elapsed:?})"
    );
}

const RECURSE_SOURCE: &str = r"
// Recursion on integers: no loop, no allocation, no call into the runtime,
// so no poll. Only preemption lets a collection on another thread stop it.
func fib(n: i64) -> i64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

public func recurse(n: i64) -> i64 { fib(n) }
";

/// Before preemption (and without polls at calls), a collection waited for
/// such a recursion to return.
#[test]
fn recursion_without_safepoints_does_not_hold_up_a_collection() {
    if !cfg!(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))) {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let driver = CompileAndRunTestDriver::new(RECURSE_SOURCE, |builder| builder)
        .expect("Failed to build test driver");
    let gc = driver.runtime.gc();
    let (latency, elapsed, value) = std::thread::scope(|s| {
        let collector = s.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let start = std::time::Instant::now();
            let _ = gc.collect_blocking();
            start.elapsed()
        });
        let start = std::time::Instant::now();
        let value: i64 = driver.runtime.invoke("recurse", (43i64,)).unwrap();
        (collector.join().unwrap(), start.elapsed(), value)
    });
    assert_eq!(value, 433_494_437);
    assert!(
        elapsed > std::time::Duration::from_millis(400),
        "the recursion ended too soon ({elapsed:?}) to tell"
    );
    assert!(
        latency < std::time::Duration::from_millis(50),
        "a collection waited {latency:?} for a recursion (it ran {elapsed:?})"
    );
}
