//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Tests of `GcPolicy` and the collection API: pacing collects a churning
//!   mutator without any limit and follows the live set, pacing off collects
//!   nothing, a failure at the limit reports what is live, back-to-back
//!   blocking collections leave Codira code its window, concurrent blocking
//!   calls coalesce, `request_collection` returns at once and is served, and
//!   the statistics count all of it.
//!
//! In a test binary of its own: the safepoint registry is process-wide, so a
//! collection here stops the mutator threads of every other test in the same
//! process, which would disturb the timing these tests measure.
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Barrier,
    },
    time::{Duration, Instant},
};

use codira_memory::{
    gc::{
        safepoint, AllocError, Event, GcPolicy, GcPtr, GcRootPtr, GcRuntime, HasIndirectionPtr,
        MarkSweep, NoopObserver,
    },
    HasStaticType, StructTypeBuilder, Type,
};
use parking_lot::{const_mutex, Mutex};

type Gc = MarkSweep<NoopObserver<Event>>;

/// The tests measure time and count collections: run them one at a time.
static SERIAL: Mutex<()> = const_mutex(());

const BLOCK: usize = 64 * 1024;

/// A struct with a 32-byte payload (a 64-byte cell with its header).
fn quad() -> Type {
    StructTypeBuilder::new("Quad")
        .add_field("a", i64::type_info().clone())
        .add_field("b", i64::type_info().clone())
        .add_field("c", i64::type_info().clone())
        .add_field("d", i64::type_info().clone())
        .finish()
}

fn policy(min_heap: usize) -> GcPolicy {
    GcPolicy {
        min_heap,
        ..GcPolicy::default()
    }
}

unsafe fn write(handle: GcPtr, value: i64) {
    let mut handle = handle;
    *handle.deref_mut::<i64>() = value;
}

unsafe fn read(handle: GcPtr) -> i64 {
    *handle.deref::<i64>()
}

/// Allocates `n` objects of `ty` as a thread running Codira code, keeping
/// none of them, and checks that an object referenced only from this frame
/// survives every collection in between.
fn churn(gc: &Gc, ty: &Type, n: usize) {
    let _guard = safepoint::enter();
    let kept = gc.alloc(ty);
    unsafe { write(kept, 4242) };
    for i in 0..n {
        let garbage = gc.alloc(ty);
        unsafe { write(garbage, i as i64) };
    }
    assert_eq!(
        unsafe { read(std::hint::black_box(kept)) },
        4242,
        "an object on the stack was freed"
    );
}

#[test]
fn pacing_collects_a_churning_mutator_without_a_limit() {
    let _serial = SERIAL.lock();
    let ty = quad();
    let gc = Gc::default();
    gc.set_policy(policy(256 * 1024));
    assert_eq!(gc.heap_limit(), None);

    let n = 400_000; // 12.8 MB of payload, 25.6 MB of cells
    churn(&gc, &ty, n);

    let stats = gc.heap_stats();
    assert!(
        stats.collections.paced >= 20,
        "{} paced collections",
        stats.collections.paced
    );
    assert_eq!(stats.collections.at_limit, 0);
    assert_eq!(stats.objects_allocated, n as u64 + 1);
    assert_eq!(stats.bytes_allocated, (n as u64 + 1) * 32);
    assert!(stats.objects_reclaimed > n as u64 / 2);
    // Without pacing the heap would have grown to 25.6 MB.
    assert!(
        stats.peak_heap_bytes < 4 << 20,
        "peak {} bytes",
        stats.peak_heap_bytes
    );
    assert_eq!(stats.pauses.count, stats.collections.total());
    assert!(stats.pauses.max >= stats.pauses.p99 && stats.pauses.p99 >= stats.pauses.p50);
    assert!(stats.collection_time >= stats.pauses.max);
}

#[test]
fn without_pacing_nothing_is_collected() {
    let _serial = SERIAL.lock();
    let ty = quad();
    let gc = Gc::default();
    gc.set_policy(policy(256 * 1024).without_pacing());
    churn(&gc, &ty, 100_000);
    let stats = gc.heap_stats();
    assert_eq!(stats.collections.total(), 0);
    assert_eq!(stats.next_paced_collection_at, None);
    assert!(stats.peak_heap_bytes >= 100_000 * 64);
}

#[test]
fn the_next_paced_collection_follows_the_live_set() {
    let _serial = SERIAL.lock();
    let ty = quad();
    let gc = Arc::new(Gc::default());
    gc.set_policy(GcPolicy {
        growth_percent: Some(50),
        min_heap: 64 * 1024,
        ..GcPolicy::default()
    });
    assert_eq!(gc.heap_stats().next_paced_collection_at, Some(64 * 1024));

    // 1 MB live, rooted.
    let live: Vec<_> = (0..32_768)
        .map(|_| GcRootPtr::new(&gc, gc.alloc(&ty)))
        .collect();
    gc.collect_blocking().unwrap();
    let stats = gc.heap_stats();
    assert_eq!(stats.live_bytes_after_last_collection, 32_768 * 32);
    assert_eq!(stats.next_paced_collection_at, Some(32_768 * 32 * 3 / 2));
    drop(live);
}

#[test]
fn a_failure_at_the_limit_reports_what_is_live() {
    let _serial = SERIAL.lock();
    let ty = quad();
    let gc = Arc::new(Gc::default());
    gc.set_heap_limit(Some(4 * BLOCK));

    let _guard = safepoint::enter();
    let mut kept = Vec::new();
    let err = loop {
        match gc.try_alloc(&ty) {
            Ok(handle) => kept.push(GcRootPtr::new(&gc, handle)),
            Err(err) => break err,
        }
    };
    let AllocError::LimitExceeded {
        limit,
        committed,
        requested,
        live,
    } = err
    else {
        panic!("unexpected error {err:?}");
    };
    assert_eq!(limit, 4 * BLOCK);
    assert_eq!(committed, 4 * BLOCK);
    assert_eq!(requested, BLOCK);
    assert_eq!(live, kept.len() * 32, "every live object is rooted");
    let stats = gc.heap_stats();
    assert!(stats.collections.at_limit >= 1);
    assert_eq!(stats.peak_heap_bytes, 4 * BLOCK);
    assert_eq!(stats.objects_reclaimed, 0);
}

/// A host thread calls `collect_blocking` in a loop while another thread
/// runs "Codira code" (allocates as a mutator). Every collection waits for
/// the mutator window, so there are at most about `elapsed / window` of
/// them, and the mutator keeps allocating.
#[test]
fn back_to_back_blocking_collections_leave_the_mutator_its_window() {
    let _serial = SERIAL.lock();
    let ty = quad();
    let gc = Gc::default();
    let window = Duration::from_millis(5);
    gc.set_policy(GcPolicy {
        min_mutator_window: window,
        ..GcPolicy::default().without_pacing()
    });

    let run_for = Duration::from_millis(300);
    let stop = AtomicBool::new(false);
    let allocated = AtomicU64::new(0);
    let started = Barrier::new(2);
    let mut collections = 0u64;
    let elapsed = std::thread::scope(|s| {
        s.spawn(|| {
            let _guard = safepoint::enter();
            started.wait();
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..64 {
                    std::hint::black_box(gc.alloc(&ty));
                }
                allocated.fetch_add(64, Ordering::Relaxed);
            }
        });
        started.wait();
        let start = Instant::now();
        while start.elapsed() < run_for {
            let report = gc.collect_blocking().unwrap();
            assert!(!report.coalesced);
            collections += 1;
        }
        let elapsed = start.elapsed();
        stop.store(true, Ordering::Relaxed);
        elapsed
    });

    let bound = (elapsed.as_secs_f64() / window.as_secs_f64()).ceil() as u64 + 1;
    assert!(
        collections <= bound,
        "{collections} collections in {elapsed:?} with a {window:?} window"
    );
    assert!(collections >= 2, "only {collections} collections");
    assert_eq!(gc.heap_stats().collections.explicit, collections);
    assert!(
        allocated.load(Ordering::Relaxed) > 10_000,
        "the mutator made {} allocations",
        allocated.load(Ordering::Relaxed)
    );
}

/// Several host threads ask for a blocking collection at once, while a
/// mutator runs: the first collection after the window satisfies them all.
#[test]
fn concurrent_blocking_calls_coalesce() {
    let _serial = SERIAL.lock();
    let gc = Gc::default();
    gc.set_policy(GcPolicy {
        min_mutator_window: Duration::from_millis(100),
        ..GcPolicy::default().without_pacing()
    });
    let stop = AtomicBool::new(false);
    let mutator_in = Barrier::new(2);
    let callers = 4;
    let call = Barrier::new(callers);
    std::thread::scope(|s| {
        s.spawn(|| {
            let _guard = safepoint::enter();
            mutator_in.wait();
            while !stop.load(Ordering::Relaxed) {
                safepoint::blocking(|| std::thread::sleep(Duration::from_micros(200)));
            }
        });
        mutator_in.wait();
        // Start the window.
        gc.collect_blocking().unwrap();
        let reports: Vec<_> = (0..callers)
            .map(|_| {
                s.spawn(|| {
                    call.wait();
                    gc.collect_blocking().unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|caller| caller.join().unwrap())
            .collect();
        stop.store(true, Ordering::Relaxed);
        let coalesced = reports.iter().filter(|r| r.coalesced).count();
        assert_eq!(coalesced, callers - 1, "{reports:?}");
    });
    assert_eq!(gc.heap_stats().collections.explicit, 2);
}

#[test]
fn request_collection_returns_at_once_and_is_served() {
    let _serial = SERIAL.lock();
    let ty = quad();
    let gc = Arc::new(Gc::default());
    let garbage: Vec<_> = (0..1000).map(|_| gc.alloc(&ty)).collect();
    drop(garbage);

    let start = Instant::now();
    assert!(gc.request_collection(), "the request is queued");
    assert!(start.elapsed() < Duration::from_millis(50));

    let deadline = Instant::now() + Duration::from_secs(10);
    while gc.heap_stats().collections.requested == 0 {
        assert!(Instant::now() < deadline, "the request was never served");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(gc.heap_stats().objects_reclaimed >= 1000);

    // A later request is served too.
    assert!(gc.request_collection());
    while gc.heap_stats().collections.requested < 2 {
        assert!(
            Instant::now() < deadline,
            "the second request was never served"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// While a request waits for the mutator window, more requests are folded
/// into it.
#[test]
fn requests_during_the_window_coalesce() {
    let _serial = SERIAL.lock();
    let gc = Gc::default();
    let window = Duration::from_millis(200);
    gc.set_policy(GcPolicy {
        min_mutator_window: window,
        ..GcPolicy::default()
    });
    let stop = AtomicBool::new(false);
    let in_codira = Barrier::new(2);
    std::thread::scope(|s| {
        s.spawn(|| {
            let _guard = safepoint::enter();
            in_codira.wait();
            while !stop.load(Ordering::Relaxed) {
                safepoint::blocking(|| std::thread::sleep(Duration::from_millis(1)));
            }
        });
        in_codira.wait();
        gc.collect_blocking().unwrap();
        let start = Instant::now();
        assert!(gc.request_collection());
        assert!(!gc.request_collection(), "a request is already pending");
        assert!(!gc.request_collection(), "a request is already pending");
        let deadline = Instant::now() + Duration::from_secs(10);
        while gc.heap_stats().collections.requested == 0 {
            assert!(Instant::now() < deadline, "the request was never served");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            start.elapsed() >= window - Duration::from_millis(5),
            "served after {:?}, before the window ended",
            start.elapsed()
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(gc.heap_stats().collections.requested, 1);
        stop.store(true, Ordering::Relaxed);
    });
}

/// Dropping a collector whose request thread is waiting out a window does
/// not hang.
#[test]
fn dropping_the_collector_stops_its_request_thread() {
    let _serial = SERIAL.lock();
    let gc = Gc::default();
    gc.set_policy(GcPolicy {
        min_mutator_window: Duration::from_secs(3600),
        ..GcPolicy::default()
    });
    let stop = Arc::new(AtomicBool::new(false));
    let in_codira = Arc::new(Barrier::new(2));
    let mutator = {
        let (stop, in_codira) = (stop.clone(), in_codira.clone());
        std::thread::spawn(move || {
            let _guard = safepoint::enter();
            in_codira.wait();
            while !stop.load(Ordering::Relaxed) {
                safepoint::blocking(|| std::thread::sleep(Duration::from_millis(1)));
            }
        })
    };
    in_codira.wait();
    gc.collect_blocking().unwrap();
    assert!(gc.request_collection());
    let start = Instant::now();
    drop(gc);
    assert!(start.elapsed() < Duration::from_secs(5));
    stop.store(true, Ordering::Relaxed);
    mutator.join().unwrap();
}
