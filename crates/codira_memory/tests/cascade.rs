//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Tests of the experimental Cascade reclamation engine through its
//!   Rust-level mutator API: the ownership state machine, local and
//!   published reclamation, epochs and quiescence, synchronous and
//!   concurrent cycle reclamation, the tracing backstop and its
//!   resynchronisation.
//! - Randomized mutation of object graphs from several threads checked
//!   against a shadow reachability model (nothing reachable is ever
//!   reclaimed; every unreachable object, cycles included, eventually is).
//! - A concurrent stress test with a use-after-reclaim detector, and
//!   sustained allocation under a heap limit.
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use codira_memory::{
    gc::{
        cascade::{CascadeConfig, Mutator, OwnershipState},
        safepoint, Event, GcPtr, GcRuntime, HasIndirectionPtr, MarkSweep, Observer,
    },
    HasStaticType, StructTypeBuilder, Type,
};
use parking_lot::{Mutex, ReentrantMutex};

/// Records every deallocation the tracing collector performs.
#[derive(Default, Clone)]
struct DeallocLog {
    log: Arc<Mutex<Vec<GcPtr>>>,
}

impl Observer for DeallocLog {
    type Event = Event;

    fn event(&self, event: Event) {
        if let Event::Deallocation(handle) = event {
            self.log.lock().push(handle);
        }
    }
}

type Gc = MarkSweep<DeallocLog>;

const FIELDS: usize = 3;

/// `struct Node { canary: i64, a: Leaf, b: Leaf, c: Leaf }`: three reference
/// fields (any node is stored in them; the tracer only uses the offsets).
struct Types {
    node: Type,
    offsets: [usize; FIELDS],
    list: Type,
}

fn types() -> Types {
    let leaf = StructTypeBuilder::new("CascadeLeaf")
        .add_field("value", i64::type_info().clone())
        .finish();
    let node = StructTypeBuilder::new("CascadeNode")
        .add_field("canary", i64::type_info().clone())
        .add_field("a", leaf.clone())
        .add_field("b", leaf.clone())
        .add_field("c", leaf.clone())
        .finish();
    let fields = node.as_struct().unwrap().fields();
    let offsets = [1, 2, 3].map(|i| fields.get(i).unwrap().offset());
    let list = leaf.array_type();
    Types {
        node,
        offsets,
        list,
    }
}

fn runtime(config: CascadeConfig) -> (Arc<Gc>, DeallocLog) {
    let log = DeallocLog::default();
    let gc = Arc::new(Gc::with_observer(log.clone()));
    gc.enable_cascade(config);
    (gc, log)
}

unsafe fn write_canary(object: GcPtr, value: i64) {
    let mut object = object;
    *object.deref_mut::<i64>() = value;
}

unsafe fn read_canary(object: GcPtr) -> i64 {
    *object.deref::<i64>()
}

fn state(gc: &Gc, object: GcPtr) -> OwnershipState {
    gc.cascade().unwrap().word(object).state()
}

/// Runs enough steps for every pending reclamation to finish (no mutator
/// registered, or all quiescent).
fn settle(gc: &Gc) {
    for _ in 0..64 {
        gc.cascade_step(1 << 20).unwrap();
    }
}

/// Runs enough steps, with `m` handshaking, for every pending reclamation
/// to finish.
fn settle_with(gc: &Gc, m: &mut Mutator) {
    for _ in 0..64 {
        m.safepoint();
        gc.cascade_step(1 << 20).unwrap();
    }
}

#[test]
fn cascade_is_off_by_default() {
    let gc = Arc::new(Gc::default());
    assert!(gc.cascade().is_none());
    assert!(gc.cascade_step(100).is_none());
    assert!(gc.cascade_mutator().is_none());
    assert_eq!(gc.cascade_deferred_bytes(), 0);
    let t = types();
    let object = gc.alloc(&t.node);
    gc.collect();
    // Unrooted and not on a scanned stack in a way that matters: tracing
    // works exactly as before.
    let _ = object;
}

#[test]
fn objects_outside_mutators_stay_untracked() {
    let (gc, log) = runtime(CascadeConfig::default());
    let t = types();
    let object = gc.alloc(&t.node);
    assert_eq!(state(&gc, object), OwnershipState::Untracked);
    settle(&gc);
    gc.collect();
    assert!(log.log.lock().contains(&object));
}

#[test]
fn release_unique_reclaims_immediately() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let a = m.alloc(&t.node).unwrap();
    assert_eq!(state(&gc, a), OwnershipState::Unique);
    let bytes = gc.stats().allocated_memory;
    assert!(m.release_unique(a));
    assert_eq!(state(&gc, a), OwnershipState::Reclaimed);
    assert!(gc.stats().allocated_memory < bytes);
    let stats = gc.cascade().unwrap().stats();
    assert_eq!(stats.unique_reclaimed, 1);
    // Reclaimed cells are reused: a million allocate/release pairs stay
    // within a few blocks.
    for _ in 0..1_000_000 {
        let b = m.alloc(&t.node).unwrap();
        assert!(m.release_unique(b));
    }
    assert!(gc.committed_bytes() <= 4 * 64 * 1024, "{}", gc.committed_bytes());
}

#[test]
fn release_unique_refuses_shared_borrowed_and_published_objects() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();

    // Shared: also referenced from a field.
    let parent = m.alloc(&t.node).unwrap();
    let child = m.alloc(&t.node).unwrap();
    m.store(parent, t.offsets[0], Some(child));
    assert_eq!(state(&gc, child), OwnershipState::Shared);
    assert!(!m.release_unique(child));
    assert_ne!(state(&gc, child), OwnershipState::Reclaimed);

    // Borrowed.
    let lent = m.alloc(&t.node).unwrap();
    assert!(m.borrow(lent));
    assert_eq!(state(&gc, lent), OwnershipState::Borrowed);
    m.hold(lent);
    assert!(!m.release_unique(lent));
    m.end_borrow(lent);
    assert_ne!(state(&gc, lent), OwnershipState::Reclaimed);

    // Published.
    let public = m.alloc(&t.node).unwrap();
    m.publish(public);
    assert!(!m.release_unique(public));

    let stats = gc.cascade().unwrap().stats();
    assert_eq!(stats.unique_fallbacks, 3);
    assert_eq!(stats.unique_reclaimed, 0);

    // The child is still reachable from the parent.
    m.reclaim_local();
    assert_eq!(state(&gc, child), OwnershipState::Shared);
    m.drop_local(lent);
    m.drop_local(parent);
    m.reclaim_local();
    assert_eq!(state(&gc, parent), OwnershipState::Reclaimed);
    assert_eq!(state(&gc, child), OwnershipState::Reclaimed);
    assert_eq!(state(&gc, lent), OwnershipState::Reclaimed);
    drop(m);
    settle(&gc);
    assert_eq!(state(&gc, public), OwnershipState::Reclaimed);
}

#[test]
fn unpublished_chain_is_reclaimed_by_its_owner() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let head = m.alloc(&t.node).unwrap();
    m.hold(head);
    let mut nodes = vec![head];
    let mut tail = head;
    for _ in 0..1000 {
        let next = m.alloc(&t.node).unwrap();
        m.store(tail, t.offsets[0], Some(next));
        m.drop_local(tail);
        tail = next;
        nodes.push(next);
    }
    m.drop_local(tail);
    m.drop_local(head);
    m.safepoint();
    for node in nodes {
        assert_eq!(state(&gc, node), OwnershipState::Reclaimed);
    }
    assert_eq!(gc.cascade().unwrap().stats().local_reclaimed, 1001);
}

#[test]
fn unpublished_cycle_is_reclaimed_by_the_local_pass() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let a = m.alloc(&t.node).unwrap();
    let b = m.alloc(&t.node).unwrap();
    let c = m.alloc(&t.node).unwrap();
    m.store(a, t.offsets[0], Some(b));
    m.store(b, t.offsets[1], Some(c));
    m.store(c, t.offsets[2], Some(a));
    m.drop_local(b);
    m.drop_local(c);
    m.safepoint();
    // `a` is held: nothing is garbage.
    for o in [a, b, c] {
        assert_eq!(state(&gc, o), OwnershipState::Shared);
    }
    m.drop_local(a);
    m.safepoint();
    for o in [a, b, c] {
        assert_eq!(state(&gc, o), OwnershipState::Reclaimed);
    }
    assert_eq!(gc.cascade().unwrap().stats().cycle_reclaimed, 3);
}

#[test]
fn published_object_survives_while_another_thread_holds_it() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let shared = m.alloc(&t.node).unwrap();
    m.publish(shared);

    let (sent, received) = std::sync::mpsc::channel::<usize>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let gc2 = gc.clone();
    let handle = shared;
    let addr = unsafe { std::mem::transmute::<GcPtr, usize>(handle) };
    let other = std::thread::spawn(move || {
        let mut m2 = gc2.cascade_mutator().unwrap();
        let object: GcPtr = unsafe { std::mem::transmute::<usize, GcPtr>(addr) };
        m2.hold(object);
        sent.send(0).unwrap();
        // Keep handshaking while holding it.
        loop {
            m2.safepoint();
            if done_rx.try_recv().is_ok() {
                break;
            }
            std::thread::yield_now();
        }
        assert_ne!(gc2.cascade().unwrap().word(object).state(), OwnershipState::Reclaimed);
        m2.drop_local(object);
        m2.safepoint();
    });
    m.quiescent(|| received.recv().unwrap());
    m.drop_local(shared);
    for _ in 0..50 {
        m.safepoint();
        gc.cascade_step(1000).unwrap();
        assert_ne!(state(&gc, shared), OwnershipState::Reclaimed);
    }
    done_tx.send(()).unwrap();
    m.quiescent(|| other.join().unwrap());
    settle_with(&gc, &mut m);
    assert_eq!(state(&gc, shared), OwnershipState::Reclaimed);
}

#[test]
fn quiescent_threads_do_not_hold_up_epochs() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let kept = m.alloc(&t.node).unwrap();
    m.publish(kept);
    let gc2 = gc.clone();
    let start = gc.cascade().unwrap().stats().epoch;
    m.quiescent(|| {
        // While this thread is quiescent, another drives the epoch.
        std::thread::spawn(move || {
            for _ in 0..20 {
                gc2.cascade_step(1000).unwrap();
            }
        })
        .join()
        .unwrap();
    });
    let stats = gc.cascade().unwrap().stats();
    assert!(stats.epoch >= start + 10, "{stats:?}");
    // The standing snapshot protected what the quiescent thread held.
    assert_eq!(state(&gc, kept), OwnershipState::Shared);
    m.drop_local(kept);
    settle_with(&gc, &mut m);
    assert_eq!(state(&gc, kept), OwnershipState::Reclaimed);
}

#[test]
fn a_thread_that_never_handshakes_stops_reclamation_not_safety() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let garbage = m.alloc(&t.node).unwrap();
    m.publish(garbage);
    m.drop_local(garbage);
    m.safepoint();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let silent = std::thread::spawn({
        let gc = gc.clone();
        move || {
            let _m = gc.cascade_mutator().unwrap();
            ready_tx.send(()).unwrap();
            // Never handshakes; blocks (parked for tracing, not quiescent).
            safepoint::blocking(|| release_rx.recv().unwrap());
        }
    });
    m.quiescent(|| ready_rx.recv().unwrap());
    for _ in 0..20 {
        m.safepoint();
        gc.cascade_step(1000).unwrap();
    }
    // The epoch cannot advance past the silent thread, so nothing published
    // is reclaimed -- the object is garbage but stays allocated.
    assert_ne!(state(&gc, garbage), OwnershipState::Reclaimed);
    // The tracing backstop still reclaims it.
    gc.collect();
    release_tx.send(()).unwrap();
    m.quiescent(|| silent.join().unwrap());
    settle_with(&gc, &mut m);
}

#[test]
fn published_cycle_is_reclaimed_by_the_concurrent_pass() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let ring: Vec<GcPtr> = (0..10).map(|_| m.alloc(&t.node).unwrap()).collect();
    for i in 0..ring.len() {
        m.store(ring[i], t.offsets[0], Some(ring[(i + 1) % ring.len()]));
    }
    // Plus a tail hanging off the ring.
    let tail = m.alloc(&t.node).unwrap();
    m.store(ring[3], t.offsets[1], Some(tail));
    m.drop_local(tail);
    m.publish(ring[0]);
    for &node in &ring[1..] {
        m.drop_local(node);
    }
    settle_with(&gc, &mut m);
    for &node in &ring {
        assert_eq!(state(&gc, node), OwnershipState::Shared);
    }
    m.drop_local(ring[0]);
    settle_with(&gc, &mut m);
    for &node in ring.iter().chain([&tail]) {
        assert_eq!(state(&gc, node), OwnershipState::Reclaimed);
    }
    let stats = gc.cascade().unwrap().stats();
    // The tail is white too: only the ring referenced it.
    assert_eq!(stats.cycle_reclaimed, 11, "{stats:?}");
    assert!(stats.cycle_sets_validated >= 1);
}

#[test]
fn a_cycle_touched_before_validation_is_not_reclaimed() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let a = m.alloc(&t.node).unwrap();
    let b = m.alloc(&t.node).unwrap();
    m.store(a, t.offsets[0], Some(b));
    m.store(b, t.offsets[0], Some(a));
    let anchor = m.alloc(&t.node).unwrap();
    m.store(anchor, t.offsets[0], Some(a));
    m.publish(anchor);
    m.drop_local(a);
    m.drop_local(b);
    settle_with(&gc, &mut m);
    // Cut the only external reference, and immediately take a local one
    // again: the dec makes the cycle a candidate; the hold must win.
    let a2 = m.load(anchor, t.offsets[0]).unwrap();
    m.store(anchor, t.offsets[0], None);
    for _ in 0..30 {
        m.safepoint();
        gc.cascade_step(10_000).unwrap();
        assert_eq!(state(&gc, a2), OwnershipState::Shared);
        assert_eq!(state(&gc, b), OwnershipState::Shared);
    }
    m.drop_local(a2);
    settle_with(&gc, &mut m);
    assert_eq!(state(&gc, a), OwnershipState::Reclaimed);
    assert_eq!(state(&gc, b), OwnershipState::Reclaimed);
    m.drop_local(anchor);
    settle_with(&gc, &mut m);
}

#[test]
fn arrays_of_references_are_counted() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let list = m.alloc_array(&t.list, 8).unwrap();
    let element = m.alloc(&t.node).unwrap();
    for i in 0..8 {
        m.store_index(list, i, Some(element));
    }
    m.drop_local(element);
    m.safepoint();
    assert_eq!(gc.cascade().unwrap().word(element).count(), 8);
    for i in 0..7 {
        m.store_index(list, i, None);
    }
    assert_eq!(m.load_index(list, 7), Some(element));
    m.drop_local(element);
    m.drop_local(list);
    m.safepoint();
    assert_eq!(state(&gc, element), OwnershipState::Reclaimed);
    assert_eq!(state(&gc, list), OwnershipState::Reclaimed);
}

#[test]
fn tracing_resynchronises_counts() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let parent = m.alloc(&t.node).unwrap();
    m.publish(parent);
    let child = m.alloc(&t.node).unwrap();
    m.store(parent, t.offsets[0], Some(child));
    m.store(parent, t.offsets[1], Some(child));
    // Two increments are still in the thread's buffer.
    gc.collect();
    let cascade = gc.cascade().unwrap();
    // Two heap references, plus the resynchronisation's own snapshot of the
    // local reference (released two boundaries later).
    assert_eq!(cascade.word(child).count(), 3);
    // Then the regular snapshots cover the local reference.
    settle_with(&gc, &mut m);
    assert_eq!(cascade.word(child).count(), 2 + 1);
    assert_eq!(cascade.stats().resyncs, 1);
    m.store(parent, t.offsets[0], None);
    m.store(parent, t.offsets[1], None);
    m.drop_local(child);
    m.drop_local(parent);
    settle_with(&gc, &mut m);
    assert_eq!(state(&gc, child), OwnershipState::Reclaimed);
    assert_eq!(state(&gc, parent), OwnershipState::Reclaimed);
}

#[test]
fn deferred_bytes_are_visible() {
    let (gc, _) = runtime(CascadeConfig::default());
    let t = types();
    let mut m = gc.cascade_mutator().unwrap();
    let a = m.alloc(&t.node).unwrap();
    let b = m.alloc(&t.node).unwrap();
    m.store(a, t.offsets[0], Some(b));
    m.store(b, t.offsets[0], Some(a));
    m.publish(a);
    m.drop_local(b);
    m.safepoint();
    m.drop_local(a);
    // A budget of one unit is too small for a cycle pass, so the dead
    // cycle stays queued as candidates, and is visible as deferred bytes.
    let mut peak = 0;
    for _ in 0..200 {
        m.safepoint();
        gc.cascade_step(1).unwrap();
        peak = peak.max(gc.cascade_deferred_bytes());
    }
    assert!(peak >= t.node.value_layout().size(), "peak {peak}");
    settle_with(&gc, &mut m);
    assert_eq!(gc.cascade_deferred_bytes(), 0);
}

// ---------------------------------------------------------------------------
// Randomized mutation against a shadow reachability model
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

fn key(object: GcPtr) -> usize {
    unsafe { std::mem::transmute::<GcPtr, usize>(object) }
}

fn ptr(key: usize) -> GcPtr {
    unsafe { std::mem::transmute::<usize, GcPtr>(key) }
}

#[derive(Default)]
struct Shadow {
    /// Live objects: fields and canary.
    objects: HashMap<usize, ([Option<usize>; FIELDS], i64)>,
    /// Per-thread local holds (multiset).
    holds: HashMap<u64, HashMap<usize, u32>>,
    /// Reclamations checked so far.
    checked: usize,
}

impl Shadow {
    fn reachable(&self) -> HashSet<usize> {
        let mut seen = HashSet::new();
        let mut stack: Vec<usize> = self
            .holds
            .values()
            .flat_map(|h| h.keys().copied())
            .collect();
        while let Some(object) = stack.pop() {
            if seen.insert(object) {
                if let Some((fields, _)) = self.objects.get(&object) {
                    stack.extend(fields.iter().flatten().copied());
                }
            }
        }
        seen
    }

    /// Checks reclamations reported since the last call: each object must
    /// be unreachable, and no remaining object may reference it.
    fn check(&mut self, reclaimed: &Mutex<Vec<usize>>) {
        let batch: Vec<usize> = std::mem::take(&mut *reclaimed.lock());
        if batch.is_empty() {
            return;
        }
        let reachable = self.reachable();
        for &object in &batch {
            assert!(
                self.objects.contains_key(&object),
                "reclaimed an unknown or already reclaimed object {object:#x}"
            );
            assert!(
                !reachable.contains(&object),
                "reclaimed a reachable object {object:#x}"
            );
        }
        for &object in &batch {
            self.objects.remove(&object);
        }
        let gone: HashSet<usize> = batch.iter().copied().collect();
        for (&owner, (fields, _)) in &self.objects {
            for field in fields.iter().flatten() {
                assert!(
                    !gone.contains(field),
                    "{owner:#x} still references reclaimed {field:#x}"
                );
            }
        }
        self.checked += batch.len();
    }
}

struct Model {
    shadow: ReentrantMutex<std::cell::RefCell<Shadow>>,
    reclaimed: Arc<Mutex<Vec<usize>>>,
    next_id: AtomicU64,
}

impl Model {
    /// Runs `f` with the shadow locked. Waiting for the lock parks the
    /// thread, so a collection never waits for a thread blocked here.
    fn with<R>(&self, f: impl FnOnce(&mut Shadow) -> R) -> R {
        let guard = safepoint::blocking(|| self.shadow.lock());
        let mut shadow = guard.borrow_mut();
        let result = f(&mut shadow);
        shadow.check(&self.reclaimed);
        result
    }
}

struct Worker<'a> {
    id: u64,
    m: Mutator,
    t: &'a Types,
    model: &'a Model,
    gc: &'a Gc,
    rng: Rng,
    held: Vec<usize>,
    hubs: Vec<usize>,
}

impl Worker<'_> {
    fn hold_shadow(shadow: &mut Shadow, thread: u64, object: usize) {
        *shadow
            .holds
            .entry(thread)
            .or_default()
            .entry(object)
            .or_insert(0) += 1;
    }

    fn drop_shadow(shadow: &mut Shadow, thread: u64, object: usize) {
        let holds = shadow.holds.get_mut(&thread).unwrap();
        let count = holds.get_mut(&object).unwrap();
        *count -= 1;
        if *count == 0 {
            holds.remove(&object);
        }
    }

    fn check_canary(&self, shadow: &Shadow, object: usize) {
        let (_, canary) = shadow
            .objects
            .get(&object)
            .unwrap_or_else(|| panic!("held object {object:#x} is not live"));
        assert_eq!(unsafe { read_canary(ptr(object)) }, *canary);
        assert_ne!(
            self.gc.cascade().unwrap().word(ptr(object)).state(),
            OwnershipState::Reclaimed
        );
    }

    fn op(&mut self, allow_trace: bool) {
        let choice = self.rng.below(100);
        let Self {
            id,
            m,
            t,
            model,
            gc,
            rng,
            held,
            hubs,
        } = self;
        let id = *id;
        let trace = std::env::var_os("CASCADE_TRACE").is_some();
        model.with(|shadow| {
            if trace {
                eprintln!("[{id}] op {choice} held={}", held.len());
            }
            let pick = |rng: &mut Rng, held: &Vec<usize>| held[rng.below(held.len())];
            match choice {
                0..=19 => {
                    let object = m.alloc(&t.node).unwrap();
                    let canary = model.next_id.fetch_add(1, Ordering::Relaxed) as i64;
                    unsafe { write_canary(object, canary) };
                    let object = key(object);
                    // The cell may be reused: an earlier object at this
                    // address must have been reported reclaimed.
                    shadow.check(&model.reclaimed);
                    assert!(!shadow.objects.contains_key(&object));
                    shadow.objects.insert(object, ([None; FIELDS], canary));
                    Self::hold_shadow(shadow, id, object);
                    held.push(object);
                }
                20..=34 if held.len() > hubs.len() => {
                    // The hubs stay held (at the front) until `finish`.
                    let index = hubs.len() + rng.below(held.len() - hubs.len());
                    let object = held.swap_remove(index);
                    Self::drop_shadow(shadow, id, object);
                    if rng.chance(30) {
                        m.release_unique(ptr(object));
                    } else {
                        m.drop_local(ptr(object));
                    }
                }
                35..=59 if !held.is_empty() => {
                    let parent = pick(rng, held);
                    let value = if rng.chance(20) {
                        None
                    } else {
                        Some(pick(rng, held))
                    };
                    let field = rng.below(FIELDS);
                    m.store(ptr(parent), t.offsets[field], value.map(ptr));
                    shadow.objects.get_mut(&parent).unwrap().0[field] = value;
                }
                60..=74 if !held.is_empty() => {
                    let parent = pick(rng, held);
                    let field = rng.below(FIELDS);
                    let loaded = m.load(ptr(parent), t.offsets[field]).map(key);
                    assert_eq!(loaded, shadow.objects[&parent].0[field]);
                    if let Some(object) = loaded {
                        self_check(shadow, gc, object);
                        Self::hold_shadow(shadow, id, object);
                        held.push(object);
                    }
                }
                75..=79 if !held.is_empty() => {
                    let object = pick(rng, held);
                    m.publish(ptr(object));
                }
                80..=86 if !hubs.is_empty() && !held.is_empty() => {
                    // Share through a hub (published).
                    let hub = hubs[rng.below(hubs.len())];
                    let value = pick(rng, held);
                    let field = rng.below(FIELDS);
                    m.store(ptr(hub), t.offsets[field], Some(ptr(value)));
                    shadow.objects.get_mut(&hub).unwrap().0[field] = Some(value);
                }
                87..=93 if !hubs.is_empty() => {
                    let hub = hubs[rng.below(hubs.len())];
                    let field = rng.below(FIELDS);
                    let loaded = m.load(ptr(hub), t.offsets[field]).map(key);
                    assert_eq!(loaded, shadow.objects[&hub].0[field]);
                    if let Some(object) = loaded {
                        self_check(shadow, gc, object);
                        Self::hold_shadow(shadow, id, object);
                        held.push(object);
                    }
                }
                94..=95 if !held.is_empty() => {
                    let object = pick(rng, held);
                    m.hold(ptr(object));
                    Self::hold_shadow(shadow, id, object);
                    held.push(object);
                }
                96 if allow_trace && rng.chance(10) => {
                    if trace {
                        eprintln!("[{id}] collect");
                    }
                    gc.collect();
                    let log: Vec<GcPtr> = gc.observer().log.lock().drain(..).collect();
                    if trace {
                        for o in &log {
                            eprintln!("traced-free {:#x}", key(*o));
                        }
                    }
                    model.reclaimed.lock().extend(log.into_iter().map(key));
                }
                _ => m.safepoint(),
            }
        });
        if self.rng.chance(2) {
            self.m.quiescent(std::thread::yield_now);
        }
        if let Some(&object) = self.held.last() {
            self.model.with(|shadow| self.check_canary(shadow, object));
        }
    }

    fn finish(&mut self) {
        let Self {
            id, m, model, held, ..
        } = self;
        model.with(|shadow| {
            for object in held.drain(..) {
                Self::drop_shadow(shadow, *id, object);
                m.drop_local(ptr(object));
            }
            m.safepoint();
        });
    }
}

fn self_check(shadow: &Shadow, gc: &Gc, object: usize) {
    let (_, canary) = shadow
        .objects
        .get(&object)
        .unwrap_or_else(|| panic!("loaded object {object:#x} is not live"));
    assert_eq!(unsafe { read_canary(ptr(object)) }, *canary);
    assert_ne!(
        gc.cascade().unwrap().word(ptr(object)).state(),
        OwnershipState::Reclaimed
    );
}

fn randomized(seed: u64, threads: usize, ops: usize, allow_trace: bool) {
    let reclaimed = Arc::new(Mutex::new(Vec::new()));
    let hook = {
        let reclaimed = reclaimed.clone();
        let trace = std::env::var_os("CASCADE_TRACE").is_some();
        Arc::new(move |object: GcPtr| {
            if trace {
                eprintln!(
                    "reclaim {:#x}\n{}",
                    key(object),
                    std::backtrace::Backtrace::force_capture()
                );
            }
            reclaimed.lock().push(key(object))
        })
    };
    let (gc, _) = runtime(CascadeConfig {
        buffer_capacity: 64,
        local_cycle_budget: 1 << 16,
        on_reclaim: Some(hook),
        ..CascadeConfig::default()
    });
    let t = types();
    let model = Model {
        shadow: ReentrantMutex::new(std::cell::RefCell::new(Shadow::default())),
        reclaimed,
        next_id: AtomicU64::new(1),
    };

    // Hubs: published objects every thread holds while it runs.
    let mut main = gc.cascade_mutator().unwrap();
    let hubs: Vec<usize> = model.with(|shadow| {
        (0..4)
            .map(|_| {
                let hub = main.alloc(&t.node).unwrap();
                unsafe { write_canary(hub, 0) };
                main.publish(hub);
                let hub = key(hub);
                shadow.objects.insert(hub, ([None; FIELDS], 0));
                Worker::hold_shadow(shadow, 0, hub);
                hub
            })
            .collect()
    });

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let collector = scope.spawn(|| {
            let mut rng = Rng(seed ^ 0xC011EC7);
            while !stop.load(Ordering::Relaxed) {
                let budget = 1 + rng.below(3000);
                model.with(|_| gc.cascade_step(budget).unwrap());
                std::thread::yield_now();
            }
        });
        let workers: Vec<_> = (0..threads)
            .map(|index| {
                let (gc, t, model, hubs) = (&gc, &t, &model, &hubs);
                let id = index as u64 + 1;
                scope.spawn(move || {
                    let mut worker = Worker {
                        id,
                        m: gc.cascade_mutator().unwrap(),
                        t,
                        model,
                        gc,
                        rng: Rng(seed.wrapping_mul(index as u64 + 7) | 1),
                        held: Vec::new(),
                        hubs: hubs.clone(),
                    };
                    // Legal: the main thread holds the hubs until every
                    // worker has finished.
                    model.with(|shadow| {
                        for &hub in hubs.iter() {
                            worker.m.hold(ptr(hub));
                            Worker::hold_shadow(shadow, id, hub);
                        }
                    });
                    worker.held.extend(hubs.iter().copied());
                    for _ in 0..ops {
                        worker.op(allow_trace);
                    }
                    worker.finish();
                })
            })
            .collect();
        // The spawning thread keeps holding the hubs until the workers have
        // taken their own references; it may now drop them.
        main.quiescent(|| {
            for worker in workers {
                worker.join().unwrap();
            }
        });
        stop.store(true, Ordering::Relaxed);
        main.quiescent(|| collector.join().unwrap());
    });

    model.with(|shadow| {
        for &hub in &hubs {
            Worker::drop_shadow(shadow, 0, hub);
            main.drop_local(ptr(hub));
        }
        main.safepoint();
    });
    drop(main);

    // Everything is unreachable now; Cascade alone must reclaim all of it.
    for _ in 0..200 {
        model.with(|_| gc.cascade_step(1 << 20).unwrap());
    }
    model.with(|shadow| {
        let stats = gc.cascade().unwrap().stats();
        assert!(
            shadow.objects.is_empty(),
            "seed {seed}: {} objects never reclaimed; {stats:?}",
            shadow.objects.len()
        );
        assert!(shadow.checked > 0);
    });
}

#[test]
fn randomized_single_thread_against_the_oracle() {
    for seed in 1..=6 {
        randomized(seed * 0x9E37_79B9, 1, 3000, false);
    }
}

#[test]
fn randomized_threads_against_the_oracle() {
    for seed in 1..=4 {
        randomized(seed * 0x5851_F42D, 4, 1500, false);
    }
}

#[test]
fn randomized_threads_with_tracing_against_the_oracle() {
    for seed in 1..=3 {
        randomized(seed * 0x2545_F491, 3, 1500, true);
    }
}

// ---------------------------------------------------------------------------
// Unserialized concurrent stress with a use-after-reclaim detector
// ---------------------------------------------------------------------------

#[test]
fn concurrent_stress_never_reclaims_a_held_object() {
    // address -> canary of every live tracked object.
    let live: Arc<Mutex<HashMap<usize, i64>>> = Arc::default();
    let hook = {
        let live = live.clone();
        Arc::new(move |object: GcPtr| {
            live.lock().remove(&key(object)).expect("reclaimed twice");
        })
    };
    let (gc, _) = runtime(CascadeConfig {
        buffer_capacity: 128,
        local_cycle_budget: 1 << 20,
        on_reclaim: Some(hook),
        ..CascadeConfig::default()
    });
    let t = types();
    let next = AtomicU64::new(1);
    let mut main = gc.cascade_mutator().unwrap();
    let hubs: Vec<usize> = (0..8)
        .map(|_| {
            let hub = main.alloc(&t.node).unwrap();
            main.publish(hub);
            live.lock().insert(key(hub), 0);
            key(hub)
        })
        .collect();
    let stop = AtomicBool::new(false);
    let threads = 4;
    std::thread::scope(|scope| {
        let collector = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                gc.cascade_step(500).unwrap();
            }
        });
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let workers: Vec<_> = (0..threads)
            .map(|index| {
                let (gc, t, live, next, hubs, ready_tx) =
                    (&gc, &t, &live, &next, &hubs, ready_tx.clone());
                scope.spawn(move || {
                    let mut m = gc.cascade_mutator().unwrap();
                    for &hub in hubs {
                        m.hold(ptr(hub));
                    }
                    ready_tx.send(()).unwrap();
                    let mut rng = Rng(0xABCD_EF01 * (index as u64 + 3));
                    let mut held: Vec<usize> = hubs.clone();
                    let check = |m: &mut Mutator, object: usize| {
                        let canary = unsafe { read_canary(ptr(object)) };
                        assert_eq!(
                            live.lock().get(&object).copied(),
                            Some(canary),
                            "a held object was reclaimed"
                        );
                        assert!(m.holds(ptr(object)) > 0);
                    };
                    for step in 0..20_000 {
                        let pick = held[rng.below(held.len())];
                        match rng.below(10) {
                            0 | 1 => {
                                let object = m.alloc(&t.node).unwrap();
                                let canary = next.fetch_add(1, Ordering::Relaxed) as i64;
                                unsafe { write_canary(object, canary) };
                                live.lock().insert(key(object), canary);
                                held.push(key(object));
                            }
                            2 if held.len() > hubs.len() => {
                                let index = hubs.len() + rng.below(held.len() - hubs.len());
                                let object = held.swap_remove(index);
                                m.drop_local(ptr(object));
                            }
                            3 | 4 => {
                                let value = held[rng.below(held.len())];
                                let field = rng.below(FIELDS);
                                m.store(ptr(pick), t.offsets[field], Some(ptr(value)));
                            }
                            5 => {
                                let field = rng.below(FIELDS);
                                m.store(ptr(pick), t.offsets[field], None);
                            }
                            6 | 7 => {
                                let field = rng.below(FIELDS);
                                if let Some(object) = m.load(ptr(pick), t.offsets[field]) {
                                    check(&mut m, key(object));
                                    held.push(key(object));
                                }
                            }
                            8 => check(&mut m, pick),
                            _ => m.safepoint(),
                        }
                        if held.len() > 64 {
                            while held.len() > hubs.len() + 8 {
                                let object = held.pop().unwrap();
                                m.drop_local(ptr(object));
                            }
                        }
                        if step % 1000 == 999 {
                            m.quiescent(std::thread::yield_now);
                        }
                    }
                    for object in held {
                        m.drop_local(ptr(object));
                    }
                    m.safepoint();
                })
            })
            .collect();
        main.quiescent(|| {
            for _ in 0..threads {
                ready_rx.recv().unwrap();
            }
        });
        for &hub in &hubs {
            main.drop_local(ptr(hub));
        }
        main.quiescent(|| {
            for worker in workers {
                worker.join().unwrap();
            }
        });
        stop.store(true, Ordering::Relaxed);
        main.quiescent(|| collector.join().unwrap());
    });
    drop(main);
    for _ in 0..200 {
        gc.cascade_step(1 << 20).unwrap();
    }
    let stats = gc.cascade().unwrap().stats();
    let words: Vec<String> = live
        .lock()
        .keys()
        .map(|&o| {
            let fields: Vec<String> = t
                .offsets
                .iter()
                .map(|&off| unsafe {
                    format!("{:#x}", *ptr(o).deref::<u8>().add(off).cast::<usize>())
                })
                .collect();
            format!("{o:#x}: {:?} -> {fields:?}", gc.cascade().unwrap().word(ptr(o)))
        })
        .collect();
    assert!(
        words.is_empty(),
        "{} objects never reclaimed: {words:?} {stats:?}",
        words.len()
    );
}

// ---------------------------------------------------------------------------
// Sustained allocation under a heap limit
// ---------------------------------------------------------------------------

fn sustained(max_cycle_pass: usize, threads: usize) -> codira_memory::gc::cascade::CascadeStats {
    let limit = 4 << 20;
    let (gc, _) = runtime(CascadeConfig {
        max_cycle_pass,
        pressure_bytes: 1 << 20,
        ..CascadeConfig::default()
    });
    gc.set_heap_limit(Some(limit));
    let t = types();
    let node_bytes = t.node.value_layout().size() + 32;
    // Allocate ~40x the limit in total.
    let per_thread = 40 * limit / node_bytes / threads;
    std::thread::scope(|scope| {
        for index in 0..threads {
            let (gc, t) = (&gc, &t);
            scope.spawn(move || {
                let mut m = gc.cascade_mutator().unwrap();
                let mut rng = Rng(77 + index as u64);
                let mut allocated = 0;
                while allocated < per_thread {
                    // A small structure: a chain, a cycle, published or not.
                    let size = 1 + rng.below(16);
                    let nodes: Vec<GcPtr> = (0..size)
                        .map(|_| m.alloc(&t.node).expect("heap limit exceeded"))
                        .collect();
                    allocated += size;
                    for i in 0..size - 1 {
                        m.store(nodes[i], t.offsets[0], Some(nodes[i + 1]));
                    }
                    if rng.chance(50) {
                        m.store(nodes[size - 1], t.offsets[1], Some(nodes[0]));
                    }
                    if rng.chance(50) {
                        m.publish(nodes[0]);
                    }
                    for node in nodes {
                        m.drop_local(node);
                    }
                    if rng.chance(10) {
                        m.safepoint();
                        gc.cascade_step(4096).unwrap();
                    }
                }
                m.safepoint();
            });
        }
    });
    assert!(gc.committed_bytes() <= limit);
    gc.cascade().unwrap().stats()
}

#[test]
fn sustained_allocation_under_a_heap_limit() {
    let stats = sustained(1 << 16, 2);
    assert!(stats.local_reclaimed + stats.retired_reclaimed > 0, "{stats:?}");
    assert!(stats.cycle_reclaimed > 0, "{stats:?}");
}

#[test]
fn sustained_allocation_with_cycle_passes_too_small_relies_on_tracing() {
    // Every published cycle pass is abandoned; the tracing backstop (heap
    // limit and pressure) must keep the program running.
    let stats = sustained(1, 2);
    assert!(stats.abandoned_passes > 0, "{stats:?}");
    assert!(stats.resyncs > 0, "{stats:?}");
}
