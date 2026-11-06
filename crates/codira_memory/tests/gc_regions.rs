//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Tests of region arenas and root ranges: a heap object referred to only
//!   from a region object survives collections while the region is open and
//!   is reclaimed once it closes; the check at close reports a heap object
//!   that still refers into the region.
//!
//! In a test binary of its own: the safepoint registry is process-wide.
use std::alloc::Layout;

use codira_memory::{
    gc::{Event, GcPtr, GcRuntime, HasIndirectionPtr, MarkSweep, NoopObserver},
    HasStaticType, StructTypeBuilder, Type,
};
use parking_lot::{const_mutex, Mutex};

type Gc = MarkSweep<NoopObserver<Event>>;

static SERIAL: Mutex<()> = const_mutex(());

fn leaf() -> Type {
    StructTypeBuilder::new("Leaf")
        .add_field("value", i64::type_info().clone())
        .finish()
}

/// The payload address of a handle: the header's first word.
fn payload(handle: GcPtr) -> *mut usize {
    unsafe { *(codira_memory::gc::RawGcPtr::from(handle) as *const *mut usize) }
}

#[test]
fn a_heap_object_held_only_by_a_region_object_survives() {
    let _serial = SERIAL.lock();
    let gc = Gc::default();
    let ty = leaf();

    // This thread is not a mutator: its stack is not scanned, so the only
    // reference to `kept` the collector can see is the region object's.
    let mut region = gc.region_open();
    let holder = gc.region_alloc(&mut region, Layout::new::<usize>());
    {
        let mut kept = gc.alloc(&ty);
        unsafe { *kept.deref_mut::<i64>() = 4242 };
        unsafe { *payload(holder) = codira_memory::gc::RawGcPtr::from(kept) as usize };
    }
    for _ in 0..3 {
        gc.collect_blocking().unwrap();
    }
    let kept: GcPtr = (unsafe { *payload(holder) } as codira_memory::gc::RawGcPtr).into();
    assert_eq!(
        unsafe { *kept.deref::<i64>() },
        4242,
        "the heap object was freed"
    );
    assert_eq!(gc.heap_stats().objects_reclaimed, 0);

    gc.region_close(region).unwrap();
    gc.collect_blocking().unwrap();
    assert_eq!(
        gc.heap_stats().objects_reclaimed,
        1,
        "once the region is closed nothing refers to the object"
    );
    let stats = gc.region_stats();
    assert_eq!((stats.opened, stats.closed, stats.live_chunks), (1, 1, 0));
}

#[test]
fn the_check_at_close_reports_a_heap_reference_into_the_region() {
    let _serial = SERIAL.lock();
    let gc = Gc::default();
    gc.set_region_checks(true);
    let leaf = leaf();
    let holder_ty = StructTypeBuilder::new("Holder")
        .add_field("leaf", leaf.clone())
        .finish();

    let mut region = gc.region_open();
    let inside = gc.region_alloc(&mut region, Layout::new::<i64>());
    let mut holder = gc.alloc(&holder_ty);
    // A heap object that refers to a region object: what the compiler's
    // escape analysis rules out.
    unsafe { *holder.deref_mut::<usize>() = codira_memory::gc::RawGcPtr::from(inside) as usize };
    let err = gc.region_close(region).unwrap_err();
    assert!(
        err.to_string().contains("a heap object of type `Holder`"),
        "{err}"
    );
    // Undo it before anything collects.
    unsafe { *holder.deref_mut::<usize>() = 0 };

    // A clean region closes without complaint.
    let mut clean = gc.region_open();
    let _ = gc.region_alloc(&mut clean, Layout::new::<i64>());
    gc.region_close(clean).unwrap();
}
