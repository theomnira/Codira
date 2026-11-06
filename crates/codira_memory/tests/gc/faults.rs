//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Allocation fault injection (`MarkSweep::fail_nth_allocation`) through
//!   the host API: the injected failure fires exactly once, at exactly the
//!   requested allocation point, on any thread, and for every `N` up to the
//!   number of allocations a workload makes, failing at the `N`th leaves the
//!   heap consistent -- what was built and rooted before is intact, the
//!   partial work is reclaimable, and allocation works again afterwards.
//!
//! These tests arm the heap they own only. They check the host-facing
//! error paths; the out-of-memory exit of generated code is checked in
//! `codira_runtime`'s `fault_injection` tests.
use std::sync::Arc;

use codira_memory::{
    gc::{
        AllocError, Array, GcPtr, GcRootPtr, GcRuntime, HasIndirectionPtr, MarkSweep, NoopObserver,
    },
    HasStaticType, StructTypeBuilder, Type,
};

type Gc = MarkSweep<NoopObserver<codira_memory::gc::Event>>;

/// `Leaf { value: i64, check: i64 }`, `Holder { id: i64, leaves: [Leaf] }`.
struct Types {
    leaf: Type,
    leaves: Type,
    holder: Type,
    holders: Type,
}

fn types() -> Types {
    let leaf = StructTypeBuilder::new("FaultLeaf")
        .add_field("value", i64::type_info().clone())
        .add_field("check", i64::type_info().clone())
        .finish();
    let leaves = leaf.array_type();
    let holder = StructTypeBuilder::new("FaultHolder")
        .add_field("id", i64::type_info().clone())
        .add_field("leaves", leaves.clone())
        .finish();
    let holders = holder.array_type();
    Types {
        leaf,
        leaves,
        holder,
        holders,
    }
}

unsafe fn write<T>(handle: GcPtr, offset: usize, value: T) {
    let mut handle = handle;
    *handle.deref_mut::<u8>().add(offset).cast::<T>() = value;
}

unsafe fn read<T: Copy>(handle: GcPtr, offset: usize) -> T {
    *handle.deref::<u8>().add(offset).cast::<T>()
}

/// The address of element `i` of an array of references, which may be past
/// its length but must be below its capacity.
unsafe fn slot(array: &<Gc as GcRuntime>::Array, i: usize) -> *mut GcPtr {
    array.data().as_ptr().add(i * array.element_stride()).cast()
}

fn check_of(value: i64) -> i64 {
    value.wrapping_mul(0x9E37_79B9_7F4A_7C15_u64 as i64) ^ 0x5555
}

/// Builds `holders` holders of `per` leaves each into the rooted array
/// `out`, growing it one holder at a time, with every step fallible. Returns
/// how many holders are complete (all of them unless an allocation failed).
fn build(
    gc: &Arc<Gc>,
    t: &Types,
    out: &GcRootPtr<Gc>,
    holders: usize,
    per: usize,
) -> Result<usize, (usize, AllocError)> {
    for h in 0..holders {
        let leaves = gc.try_alloc_array(&t.leaves, per).map_err(|e| (h, e))?;
        // Rooted while the leaves are allocated: this host thread is not a
        // mutator, so its stack is not scanned.
        let leaves_root = GcRootPtr::new(gc, leaves.as_raw());
        unsafe { gc.array(leaves_root.handle()).unwrap().set_length(0) };
        for i in 0..per {
            let leaf = gc.try_alloc(&t.leaf).map_err(|e| (h, e))?;
            let value = (h * 1000 + i) as i64;
            unsafe {
                write(leaf, 0, value);
                write(leaf, 8, check_of(value));
                let mut array = gc.array(leaves_root.handle()).unwrap();
                *slot(&array, i) = leaf;
                array.set_length(i + 1);
            }
        }
        let holder = gc.try_alloc(&t.holder).map_err(|e| (h, e))?;
        unsafe {
            write(holder, 0, h as i64);
            write(holder, 8, leaves_root.handle());
        }
        let holder_root = GcRootPtr::new(gc, holder);
        // Append to `out`, growing it: a third allocation point.
        unsafe {
            gc.try_grow_array(out.handle(), h + 1).map_err(|e| (h, e))?;
            let mut array = gc.array(out.handle()).unwrap();
            assert!(array.capacity() > h);
            *slot(&array, h) = holder_root.handle();
            array.set_length(h + 1);
        }
    }
    Ok(holders)
}

/// Checks that `out` holds `complete` intact holders of `per` leaves.
fn verify(gc: &Arc<Gc>, out: &GcRootPtr<Gc>, complete: usize, per: usize) {
    let array = gc.array(out.handle()).unwrap();
    assert_eq!(array.length(), complete);
    for (h, element) in array.elements().enumerate() {
        let holder = unsafe { *element.cast::<GcPtr>().as_ptr() };
        assert_eq!(unsafe { read::<i64>(holder, 0) }, h as i64);
        let leaves = gc.array(unsafe { read::<GcPtr>(holder, 8) }).unwrap();
        assert_eq!(leaves.length(), per);
        for (i, element) in leaves.elements().enumerate() {
            let leaf = unsafe { *element.cast::<GcPtr>().as_ptr() };
            let value = (h * 1000 + i) as i64;
            assert_eq!(unsafe { read::<i64>(leaf, 0) }, value);
            assert_eq!(unsafe { read::<i64>(leaf, 8) }, check_of(value));
        }
    }
}

#[test]
fn the_nth_allocation_fails_exactly_once() {
    let t = types();
    let gc = Arc::new(Gc::default());
    gc.fail_nth_allocation(Some(3));
    assert!(gc.try_alloc(&t.leaf).is_ok());
    assert!(gc.try_alloc_array(&t.leaves, 4).is_ok());
    assert_eq!(gc.try_alloc(&t.leaf).unwrap_err(), AllocError::Injected);
    assert_eq!(gc.injected_failures(), 1);
    // Disarmed after firing.
    for _ in 0..100 {
        gc.try_alloc(&t.leaf).unwrap();
    }
    assert_eq!(gc.injected_failures(), 1);

    // Growth counts as an allocation point.
    let array = GcRootPtr::new(&gc, gc.alloc_array(&t.leaves, 0).as_raw());
    gc.fail_nth_allocation(Some(1));
    let err = unsafe { gc.try_grow_array(array.handle(), 8) }.unwrap_err();
    assert_eq!(err, AllocError::Injected);
    assert_eq!(
        gc.array(array.handle()).unwrap().capacity(),
        0,
        "a failed growth changes nothing"
    );
    unsafe { gc.try_grow_array(array.handle(), 8) }.unwrap();

    // Disarming works.
    gc.fail_nth_allocation(Some(2));
    gc.fail_nth_allocation(None);
    for _ in 0..10 {
        gc.try_alloc(&t.leaf).unwrap();
    }
    assert_eq!(gc.injected_failures(), 2);
}

/// One failure among many threads allocating at once: exactly one
/// allocation fails, whichever thread makes it.
#[test]
fn exactly_one_allocation_fails_across_threads() {
    let t = types();
    let gc = Arc::new(Gc::default());
    for n in [1u64, 17, 999, 3_999] {
        gc.fail_nth_allocation(Some(n));
        let failures: usize = std::thread::scope(|s| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        (0..1_000)
                            .filter(|_| gc.try_alloc(&t.leaf).is_err())
                            .count()
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).sum()
        });
        assert_eq!(failures, 1, "n = {n}");
        gc.collect_blocking().unwrap();
    }
    assert_eq!(gc.injected_failures(), 4);
}

/// The fault-injection gate for the host API: fail allocation `N` of a
/// graph-building workload for every `N` until it completes, and check the
/// heap after each failure.
#[test]
fn failing_every_allocation_point_leaves_the_heap_consistent() {
    const HOLDERS: usize = 6;
    const PER: usize = 5;
    let t = types();
    let mut n = 1u64;
    loop {
        let gc = Arc::new(Gc::default());
        let out = GcRootPtr::new(&gc, gc.alloc_array(&t.holders, 0).as_raw());
        gc.fail_nth_allocation(Some(n));
        match build(&gc, &t, &out, HOLDERS, PER) {
            Ok(done) => {
                assert_eq!(done, HOLDERS);
                assert_eq!(gc.injected_failures(), 0, "fired without failing the build");
                verify(&gc, &out, HOLDERS, PER);
                break;
            }
            Err((complete, err)) => {
                assert_eq!(err, AllocError::Injected, "n = {n}");
                assert_eq!(gc.injected_failures(), 1);
                // Collections reclaim the partial holder, and the complete
                // ones are intact before and after.
                verify(&gc, &out, complete, PER);
                gc.collect_blocking().unwrap();
                verify(&gc, &out, complete, PER);
                let rooted = gc.stats().allocated_memory;
                // The heap still works: finishing the build from scratch
                // into a second root succeeds.
                let again = GcRootPtr::new(&gc, gc.alloc_array(&t.holders, 0).as_raw());
                build(&gc, &t, &again, HOLDERS, PER).expect("disarmed after one failure");
                verify(&gc, &again, HOLDERS, PER);
                drop(again);
                gc.collect_blocking().unwrap();
                assert_eq!(
                    gc.stats().allocated_memory,
                    rooted,
                    "n = {n}: garbage was left"
                );
                drop(out);
                gc.collect_blocking().unwrap();
                assert_eq!(gc.stats().allocated_memory, 0, "n = {n}: something leaked");
            }
        }
        n += 1;
        assert!(n < 1_000, "the workload never completed");
    }
    // `out` is allocated before arming; each holder makes 1 + PER + 1
    // allocations, and `out` grows twice (capacity 4, then 8). A growth
    // call that needs no new buffer allocates nothing and does not count.
    assert_eq!(n, (HOLDERS * (PER + 2)) as u64 + 2 + 1);
}

/// A heap snapshot restore that hits an allocation failure returns an error
/// and leaves only collectable garbage, for every allocation it makes.
#[test]
fn a_failed_snapshot_restore_leaves_collectable_garbage() {
    let t = types();
    let source = Arc::new(Gc::default());
    let out = GcRootPtr::new(&source, source.alloc_array(&t.holders, 0).as_raw());
    build(&source, &t, &out, 3, 4).unwrap();
    let mut bytes = Vec::new();
    let objects = source.write_snapshot(&mut bytes).unwrap();
    let resolve = |guid: &codira_abi::Guid| {
        [&t.leaf, &t.holder, i64::type_info()]
            .into_iter()
            .find(|ty| ty.as_concrete() == Some(guid))
            .cloned()
    };
    for n in 1..=objects as u64 {
        let fresh = Gc::default();
        fresh.fail_nth_allocation(Some(n));
        let err = fresh
            .read_snapshot(&mut bytes.as_slice(), resolve)
            .unwrap_err();
        assert!(
            err.to_string().contains("injected allocation failure"),
            "{err}"
        );
        fresh.collect_blocking().unwrap();
        assert_eq!(fresh.stats().allocated_memory, 0, "n = {n}");
    }
    let fresh = Gc::default();
    fresh.fail_nth_allocation(Some(objects as u64 + 1));
    let restored = fresh.read_snapshot(&mut bytes.as_slice(), resolve).unwrap();
    assert_eq!(restored.objects.len(), objects);
    for (root, count) in restored.roots {
        for _ in 0..count {
            fresh.unroot(root);
        }
    }
}
