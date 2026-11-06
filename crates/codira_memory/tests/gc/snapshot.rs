//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Heap snapshots round-trip a graph (structs, arrays of references, arrays
//!   of value structs, cycles, roots) into a fresh heap, and refuse what they
//!   cannot carry.
use std::{collections::HashMap, sync::Arc};

use codira_abi::StructMemoryKind;
use codira_memory::{
    gc::{
        Array, GcPtr, GcRootPtr, GcRuntime, HasIndirectionPtr, MarkSweep, NoopObserver,
        SnapshotError,
    },
    HasStaticType, StructTypeBuilder, Type,
};

type Gc = MarkSweep<NoopObserver<codira_memory::gc::Event>>;

unsafe fn write<T>(handle: GcPtr, offset: usize, value: T) {
    let mut handle = handle;
    *handle.deref_mut::<u8>().add(offset).cast::<T>() = value;
}

unsafe fn read<T: Copy>(handle: GcPtr, offset: usize) -> T {
    *handle.deref::<u8>().add(offset).cast::<T>()
}

struct Types {
    node: Type,
    pair: Type,
    pairs: Type,
    nodes: Type,
}

fn types() -> Types {
    // A GC struct that refers to itself needs its type to exist first, so
    // `Node.next` is modelled as `[Node]` (a reference either way).
    let pair = StructTypeBuilder::new("SnapPair")
        .set_memory_kind(StructMemoryKind::Value)
        .add_field("weight", f64::type_info().clone())
        .add_field("count", u32::type_info().clone())
        .finish();
    let pairs = pair.array_type();
    let node = StructTypeBuilder::new("SnapNode")
        .add_field("value", i64::type_info().clone())
        .add_field("pairs", pairs.clone())
        .finish();
    let nodes = node.array_type();
    Types {
        node,
        pair,
        pairs,
        nodes,
    }
}

fn resolver(types: &Types) -> impl Fn(&codira_abi::Guid) -> Option<Type> {
    let mut by_guid: HashMap<codira_abi::Guid, Type> = HashMap::new();
    for ty in [
        &types.node,
        &types.pair,
        i64::type_info(),
        f64::type_info(),
        u32::type_info(),
    ] {
        by_guid.insert(*ty.as_concrete().unwrap(), ty.clone());
    }
    move |guid| by_guid.get(guid).cloned()
}

#[test]
fn a_graph_round_trips_into_a_fresh_heap() {
    let types = types();
    let gc = Arc::new(Gc::default());

    // nodes: [n0, n1, n0] (n0 appears twice: sharing must survive)
    let n0 = gc.alloc(&types.node);
    let n1 = gc.alloc(&types.node);
    let mut pairs = gc.alloc_array(&types.pairs, 2);
    for (i, element) in pairs.elements().enumerate() {
        unsafe {
            *element.as_ptr().cast::<f64>() = 0.5 + i as f64;
            *element.as_ptr().add(8).cast::<u32>() = 10 + i as u32;
        }
    }
    unsafe { pairs.set_length(2) };
    unsafe {
        write(n0, 0, 7i64);
        write(n0, 8, pairs.as_raw());
        write(n1, 0, -8i64);
        // n1.pairs stays null
    }
    let mut list = gc.alloc_array(&types.nodes, 3);
    for (element, node) in list.elements().zip([n0, n1, n0]) {
        unsafe { *element.cast::<GcPtr>().as_ptr() = node };
    }
    unsafe { list.set_length(3) };
    let root = GcRootPtr::new(&gc, list.as_raw());
    // Garbage is part of the heap too; it is written and restored as such.
    gc.alloc(&types.node);

    let mut bytes = Vec::new();
    let written = gc.write_snapshot(&mut bytes).unwrap();
    assert_eq!(written, 5);

    // Restore into a different heap
    let fresh = Arc::new(Gc::default());
    let restored = fresh
        .read_snapshot(&mut bytes.as_slice(), resolver(&types))
        .unwrap();
    assert_eq!(restored.objects.len(), 5);
    assert_eq!(restored.roots.len(), 1);
    let (list, count) = restored.roots[0];
    assert_eq!(count, 1);

    let list = fresh.array(list).expect("the root is the array");
    assert_eq!(list.length(), 3);
    let nodes: Vec<GcPtr> = list
        .elements()
        .map(|e| unsafe { *e.cast::<GcPtr>().as_ptr() })
        .collect();
    assert_eq!(nodes[0], nodes[2], "sharing is preserved");
    assert_ne!(nodes[0], nodes[1]);
    unsafe {
        assert_eq!(read::<i64>(nodes[0], 0), 7);
        assert_eq!(read::<i64>(nodes[1], 0), -8);
        assert!(read::<*const u8>(nodes[1], 8).is_null());
        let pairs = fresh.array(read::<GcPtr>(nodes[0], 8)).unwrap();
        let values: Vec<(f64, u32)> = pairs
            .elements()
            .map(|e| (*e.as_ptr().cast::<f64>(), *e.as_ptr().add(8).cast::<u32>()))
            .collect();
        assert_eq!(values, [(0.5, 10), (1.5, 11)]);
    }

    // The restored root keeps the graph alive and the garbage is collectable
    assert!(fresh.collect_blocking().unwrap().reclaimed_anything());
    assert_eq!(
        fresh.stats().allocated_memory,
        gc.stats().allocated_memory - types.node.value_layout().size()
    );
    unsafe { assert_eq!(read::<i64>(nodes[1], 0), -8) };

    // Releasing the restored root frees everything
    fresh.unroot(restored.roots[0].0);
    assert!(fresh.collect_blocking().unwrap().reclaimed_anything());
    assert_eq!(fresh.stats().allocated_memory, 0);
    drop(root);
}

#[test]
fn unknown_types_and_garbage_are_refused() {
    let types = types();
    let gc = Gc::default();
    gc.alloc(&types.node);
    let mut bytes = Vec::new();
    gc.write_snapshot(&mut bytes).unwrap();

    let fresh = Gc::default();
    let err = fresh
        .read_snapshot(&mut bytes.as_slice(), |_| None)
        .unwrap_err();
    assert!(matches!(err, SnapshotError::UnknownType(_)), "{err}");

    let err = fresh
        .read_snapshot(&mut &b"not a snapshot at all"[..], resolver(&types))
        .unwrap_err();
    assert!(matches!(err, SnapshotError::Format(_)), "{err}");

    let truncated = &bytes[..bytes.len() - 3];
    let err = fresh
        .read_snapshot(&mut &truncated[..], resolver(&types))
        .unwrap_err();
    assert!(matches!(err, SnapshotError::Io(_)), "{err}");
}

#[test]
fn types_with_raw_pointers_are_refused() {
    let with_pointer = StructTypeBuilder::new("WithPointer")
        .add_field("p", <*const u8>::type_info().clone())
        .finish();
    let gc = Gc::default();
    gc.alloc(&with_pointer);
    let err = gc.write_snapshot(&mut Vec::new()).unwrap_err();
    assert!(matches!(err, SnapshotError::RawPointers(_)), "{err}");
}
