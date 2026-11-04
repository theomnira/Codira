//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Tests of the size-class heap behind `MarkSweep`: tracing through arrays
//!   and inline value structs, large objects, exact statistics, cell reuse and
//!   allocation from many threads.
use std::sync::Arc;

use codira_abi::StructMemoryKind;
use codira_memory::{
    gc::{
        AllocError, Array, GcPtr, GcRootPtr, GcRuntime, HasIndirectionPtr, MarkSweep, NoopObserver,
    },
    HasStaticType, StructTypeBuilder, Type,
};

type Gc = MarkSweep<NoopObserver<codira_memory::gc::Event>>;

/// `struct Node { value: i64, next: Node }` (a GC struct)
fn node_type() -> (Type, Type) {
    let leaf = StructTypeBuilder::new("Leaf")
        .add_field("value", i64::type_info().clone())
        .finish();
    let node = StructTypeBuilder::new("Node")
        .add_field("value", i64::type_info().clone())
        .add_field("leaf", leaf.clone())
        .finish();
    (node, leaf)
}

unsafe fn write<T>(handle: GcPtr, offset: usize, value: T) {
    let mut handle = handle;
    *handle.deref_mut::<u8>().add(offset).cast::<T>() = value;
}

unsafe fn read<T: Copy>(handle: GcPtr, offset: usize) -> T {
    *handle.deref::<u8>().add(offset).cast::<T>()
}

#[test]
fn reference_fields_are_pointer_sized() {
    let (node, leaf) = node_type();
    let two_refs = StructTypeBuilder::new("TwoRefs")
        .add_field("a", leaf.clone())
        .add_field("b", node)
        .finish();
    let fields = two_refs.as_struct().unwrap().fields();
    assert_eq!(fields.get(0).unwrap().offset(), 0);
    assert_eq!(
        fields.get(1).unwrap().offset(),
        std::mem::size_of::<GcPtr>()
    );
    assert_eq!(
        two_refs.value_layout().size(),
        2 * std::mem::size_of::<GcPtr>()
    );
}

#[test]
fn struct_field_holding_an_array_keeps_the_array_alive() {
    let (_, leaf) = node_type();
    let leaves = leaf.array_type();
    let holder = StructTypeBuilder::new("Holder")
        .add_field("count", i64::type_info().clone())
        .add_field("leaves", leaves.clone())
        .finish();
    let leaves_offset = holder
        .as_struct()
        .unwrap()
        .fields()
        .find_by_name("leaves")
        .unwrap()
        .offset();

    let gc = Arc::new(Gc::default());
    let root = GcRootPtr::new(&gc, gc.alloc(&holder));

    let mut array = gc.alloc_array(&leaves, 3);
    let mut elements = Vec::new();
    for (index, element) in array.elements().enumerate() {
        let leaf_handle = gc.alloc(&leaf);
        unsafe {
            write(leaf_handle, 0, 100 + index as i64);
            *element.cast::<GcPtr>().as_ptr() = leaf_handle;
        }
        elements.push(leaf_handle);
    }
    unsafe {
        array.set_length(3);
        write(root.handle(), leaves_offset, array.as_raw());
    }

    // Garbage that must go
    for _ in 0..10 {
        gc.alloc(&leaf);
    }

    assert!(gc.collect(), "the unreachable leaves are collected");
    for (index, element) in elements.iter().enumerate() {
        assert_eq!(unsafe { read::<i64>(*element, 0) }, 100 + index as i64);
    }
    assert_eq!(
        gc.stats().allocated_memory,
        holder.value_layout().size()
            + std::mem::size_of::<usize>() * 2
            + 3 * std::mem::size_of::<GcPtr>()
            + 3 * leaf.value_layout().size()
    );

    // Nothing else is reclaimable
    assert!(!gc.collect());

    drop(root);
    assert!(gc.collect());
    assert_eq!(gc.stats().allocated_memory, 0);
}

#[test]
fn references_inside_inline_value_structs_are_traced() {
    let (_, leaf) = node_type();
    let pair = StructTypeBuilder::new("Pair")
        .set_memory_kind(StructMemoryKind::Value)
        .add_field("tag", u8::type_info().clone())
        .add_field("leaf", leaf.clone())
        .finish();
    let outer = StructTypeBuilder::new("Outer")
        .add_field("first", pair.clone())
        .add_field("second", pair.clone())
        .finish();
    let fields = outer.as_struct().unwrap().fields();
    let leaf_in_pair = pair.as_struct().unwrap().fields().get(1).unwrap().offset();
    let first = fields.get(0).unwrap().offset() + leaf_in_pair;
    let second = fields.get(1).unwrap().offset() + leaf_in_pair;

    let gc = Arc::new(Gc::default());
    let root = GcRootPtr::new(&gc, gc.alloc(&outer));
    let a = gc.alloc(&leaf);
    let b = gc.alloc(&leaf);
    unsafe {
        write(a, 0, 7i64);
        write(b, 0, 9i64);
        write(root.handle(), first, a);
        write(root.handle(), second, b);
    }

    assert!(
        !gc.collect(),
        "both leaves are reachable through value structs"
    );
    assert_eq!(unsafe { read::<i64>(a, 0) }, 7);
    assert_eq!(unsafe { read::<i64>(b, 0) }, 9);
}

#[test]
fn arrays_of_value_structs_trace_their_references() {
    let (_, leaf) = node_type();
    let pair = StructTypeBuilder::new("Pair")
        .set_memory_kind(StructMemoryKind::Value)
        .add_field("tag", u64::type_info().clone())
        .add_field("leaf", leaf.clone())
        .finish();
    let pairs = pair.array_type();

    let gc = Arc::new(Gc::default());
    let mut array = gc.alloc_array(&pairs, 4);
    let root = GcRootPtr::new(&gc, array.as_raw());
    let mut leaves = Vec::new();
    for element in array.elements() {
        let leaf_handle = gc.alloc(&leaf);
        unsafe { *element.as_ptr().add(8).cast::<GcPtr>() = leaf_handle };
        leaves.push(leaf_handle);
    }
    unsafe { array.set_length(4) };

    assert!(!gc.collect());
    drop(root);
    assert!(gc.collect());
    assert_eq!(gc.stats().allocated_memory, 0);
}

#[test]
fn large_objects_are_allocated_and_freed() {
    let big = StructTypeBuilder::new("Big")
        .add_fields((0..600).map(|i| (format!("f{i}"), i64::type_info().clone())))
        .finish();
    assert!(big.value_layout().size() > 4000);

    let gc = Arc::new(Gc::default());
    let rooted = GcRootPtr::new(&gc, gc.alloc(&big));
    unsafe { write(rooted.handle(), 4792, 0x5eed_i64) };
    let garbage = gc.alloc(&big);
    let _ = garbage;

    // A large array too
    let large_array = gc.alloc_array(&i64::type_info().array_type(), 10_000);
    assert_eq!(large_array.capacity(), 10_000);

    let committed = gc.committed_bytes();
    assert!(committed >= 3 * 4000);
    assert!(gc.collect());
    assert!(gc.committed_bytes() < committed);
    assert_eq!(unsafe { read::<i64>(rooted.handle(), 4792) }, 0x5eed);
    assert_eq!(gc.stats().allocated_memory, big.value_layout().size());

    drop(rooted);
    assert!(gc.collect());
    assert_eq!(gc.stats().allocated_memory, 0);
}

#[test]
fn freed_cells_are_reused() {
    let (node, _) = node_type();
    let gc = Gc::default();

    for _ in 0..10_000 {
        gc.alloc(&node);
    }
    assert!(gc.collect());
    let committed = gc.committed_bytes();

    for _ in 0..10 {
        for _ in 0..10_000 {
            gc.alloc(&node);
        }
        assert!(gc.collect());
    }
    assert_eq!(
        gc.committed_bytes(),
        committed,
        "steady-state churn must not grow the heap"
    );
}

const THREADS: usize = 4;
const PER_THREAD: usize = 5_000;

#[test]
fn allocation_from_many_threads() {
    let (node, leaf) = node_type();
    let gc = Arc::new(Gc::default());

    let rooted: Vec<Vec<(GcRootPtr<Gc>, GcPtr)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|thread| {
                let gc = gc.clone();
                let node = node.clone();
                let leaf = leaf.clone();
                scope.spawn(move || {
                    let mut kept = Vec::new();
                    for index in 0..PER_THREAD {
                        let n = gc.alloc(&node);
                        let l = gc.alloc(&leaf);
                        let value = (thread * PER_THREAD + index) as i64;
                        unsafe {
                            write(n, 0, value);
                            write(n, 8, l);
                            write(l, 0, -value);
                        }
                        if index % 10 == 0 {
                            kept.push((GcRootPtr::new(&gc, n), l));
                        }
                    }
                    kept
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // All threads have exited and returned their runs; every allocation is
    // still accounted for.
    let object_bytes = node.value_layout().size() + leaf.value_layout().size();
    assert_eq!(
        gc.stats().allocated_memory,
        THREADS * PER_THREAD * object_bytes
    );

    assert!(gc.collect());
    let kept: usize = rooted.iter().map(Vec::len).sum();
    assert_eq!(kept, THREADS * PER_THREAD / 10);
    assert_eq!(gc.stats().allocated_memory, kept * object_bytes);
    for (root, leaf_handle) in rooted.iter().flatten() {
        let value = unsafe { read::<i64>(root.handle(), 0) };
        assert_eq!(unsafe { read::<GcPtr>(root.handle(), 8) }, *leaf_handle);
        assert_eq!(unsafe { read::<i64>(*leaf_handle, 0) }, -value);
    }

    drop(rooted);
    assert!(gc.collect());
    assert_eq!(gc.stats().allocated_memory, 0);
}

#[test]
fn heap_outlives_thread_caches_and_vice_versa() {
    let (node, _) = node_type();
    // A thread allocates in a heap that is dropped while the thread lives on
    // and allocates in a second heap.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let first = Gc::default();
            for _ in 0..1000 {
                first.alloc(&node);
            }
            drop(first);
            let second = Gc::default();
            for _ in 0..1000 {
                second.alloc(&node);
            }
            assert!(second.collect());
        });
    });

    // A heap whose allocating thread has exited keeps working.
    let gc = Arc::new(Gc::default());
    std::thread::scope(|scope| {
        let gc = gc.clone();
        let node = node.clone();
        scope.spawn(move || {
            for _ in 0..1000 {
                gc.alloc(&node);
            }
        });
    });
    assert!(gc.collect());
    assert_eq!(gc.stats().allocated_memory, 0);
}

const BLOCK: usize = 64 * 1024;

#[test]
fn heap_limit_collects_then_retries_with_stack_roots() {
    let (node, leaf) = node_type();
    let gc = Arc::new(Gc::default());
    gc.set_heap_limit(Some(4 * BLOCK));
    assert_eq!(gc.heap_limit(), Some(4 * BLOCK));

    let rooted = GcRootPtr::new(&gc, gc.alloc(&leaf));
    // Referenced only from this stack frame, like a handle in generated code.
    let on_stack = gc.alloc(&leaf);
    unsafe {
        write(rooted.handle(), 0, 11i64);
        write(on_stack, 0, 22i64);
    }

    // Leaves and nodes share a size class, so a wrongly freed `on_stack`
    // would be reused and overwritten here.
    for i in 0..100_000i64 {
        let n = gc.alloc(&node);
        unsafe { write(n, 0, i) };
    }

    assert!(gc.emergency_collections() > 0);
    assert!(gc.committed_bytes() <= 4 * BLOCK);
    assert_eq!(
        unsafe { read::<i64>(std::hint::black_box(on_stack), 0) },
        22
    );
    assert_eq!(unsafe { read::<i64>(rooted.handle(), 0) }, 11);
}

#[test]
fn heap_limit_exhaustion_is_an_error() {
    let (_, leaf) = node_type();
    let gc = Arc::new(Gc::default());
    gc.set_heap_limit(Some(2 * BLOCK));

    let mut kept = Vec::new();
    let err = loop {
        match gc.try_alloc(&leaf) {
            Ok(handle) => kept.push(GcRootPtr::new(&gc, handle)),
            Err(err) => break err,
        }
    };
    assert!(matches!(err, AllocError::LimitExceeded { limit, .. } if limit == 2 * BLOCK));
    assert!(kept.len() > 2 * BLOCK / 64);

    // A single object larger than the limit fails the same way
    let big = gc.try_alloc_array(&i64::type_info().array_type(), 1_000_000);
    assert!(matches!(big, Err(AllocError::LimitExceeded { .. })));

    // Releasing the roots makes room again
    drop(kept);
    assert!(gc.collect());
    gc.try_alloc(&leaf)
        .expect("memory is available after a collection");

    // Removing the limit lets the heap grow again
    gc.set_heap_limit(None);
    gc.try_alloc_array(&i64::type_info().array_type(), 1_000_000)
        .expect("no limit");
}

#[test]
fn emergency_collection_scans_every_thread_in_codira_code() {
    use codira_memory::gc::safepoint;

    let (node, leaf) = node_type();
    let gc = Arc::new(Gc::default());
    gc.set_heap_limit(Some(4 * BLOCK));

    let ready = Arc::new(std::sync::Barrier::new(2));
    let done = Arc::new(std::sync::Barrier::new(2));
    let other = {
        let (gc, leaf, ready, done) = (gc.clone(), leaf.clone(), ready.clone(), done.clone());
        std::thread::spawn(move || {
            // Like a task: inside Codira code, holding an object referenced
            // only from its own stack, and blocked in a runtime call.
            let _guard = safepoint::enter();
            let held = gc.alloc(&leaf);
            unsafe { write(held, 0, 33i64) };
            safepoint::blocking(|| {
                ready.wait();
                done.wait();
            });
            unsafe { read::<i64>(std::hint::black_box(held), 0) }
        })
    };
    ready.wait();

    // Leaves and nodes share a size class, so a wrongly freed `held` would
    // be reused and overwritten here.
    for i in 0..100_000i64 {
        let n = gc.alloc(&node);
        unsafe { write(n, 0, i) };
    }
    assert!(gc.emergency_collections() > 0);
    done.wait();
    assert_eq!(other.join().unwrap(), 33, "the other thread's object was freed");
}

#[test]
fn a_thread_outside_codira_code_does_not_hold_up_a_collection() {
    let (_, leaf) = node_type();
    let gc = Arc::new(Gc::default());
    gc.set_heap_limit(Some(2 * BLOCK));

    let ready = Arc::new(std::sync::Barrier::new(2));
    let done = Arc::new(std::sync::Barrier::new(2));
    let other = {
        let (gc, leaf, ready, done) = (gc.clone(), leaf.clone(), ready.clone(), done.clone());
        std::thread::spawn(move || {
            gc.alloc(&leaf);
            ready.wait();
            // Keep this thread (and its cache of the heap) alive.
            done.wait();
        })
    };
    ready.wait();
    for _ in 0..10_000 {
        gc.try_alloc(&leaf)
            .expect("emergency collection reclaims garbage");
    }
    assert!(gc.emergency_collections() >= 1);
    done.wait();
    other.join().unwrap();
}
