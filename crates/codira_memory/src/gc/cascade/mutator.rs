//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The Rust-level mutator API of Cascade: allocation, local references,
//!   the field write barrier, loads, publication, `release_unique`, scoped
//!   borrows, handshakes and quiescence. It has exactly the entry points
//!   compiled code will need (`CASCADE.md` section 9), so the mechanisms
//!   can be driven and verified before compiler integration.
//!
//! A [`Mutator`] belongs to one thread (it is not `Send`) and keeps that
//! thread registered as running Codira code for the safepoint protocol, so
//! a tracing collection waits for it to reach a safepoint (an allocation or
//! [`Mutator::safepoint`]) and sees its local references as roots.
//!
//! Every method that takes an object requires that this mutator holds it
//! locally (debug builds check it): holding is what makes an access legal.

use std::{
    ptr::NonNull,
    sync::{
        atomic::{AtomicPtr, Ordering},
        Arc,
    },
};

use rustc_hash::FxHashSet;

use super::{
    children_of,
    cycles::{trial_deletion, CycleGraph},
    epoch::Slot,
    host_roots, publish_word, set_word, word_of,
    word::{BUFFERED, MAX_BORROWS, ZCT},
    Cascade, CascadeHost, Local, OwnershipState, Packet, Standing, ThreadState, Word,
};
use crate::{
    gc::{
        array::ArrayHeader,
        heap::{handle_of, header_of, AllocError, ObjectHeader, STATE_ALLOCATED},
        mark_sweep::array_data_offset,
        safepoint::{self, MutatorGuard},
        GcPtr,
    },
    r#type::Type,
};

/// A thread's handle on a Cascade engine.
pub struct Mutator {
    cascade: Arc<Cascade>,
    host: Arc<dyn CascadeHost>,
    state: Arc<ThreadState>,
    slot: Arc<Slot>,
    quiescent_before: bool,
    _guard: MutatorGuard,
}

#[inline]
fn addr(object: GcPtr) -> usize {
    header_of(object).as_ptr() as usize
}

#[inline]
fn handle(object: usize) -> GcPtr {
    handle_of(unsafe { NonNull::new_unchecked(object as *mut ObjectHeader) })
}

impl Mutator {
    pub(crate) fn new(cascade: Arc<Cascade>, host: Arc<dyn CascadeHost>) -> Self {
        let guard = safepoint::enter();
        let (state, slot) = cascade.register();
        Self {
            cascade,
            host,
            state,
            slot,
            quiescent_before: false,
            _guard: guard,
        }
    }

    #[inline]
    fn local(&mut self) -> &mut Local {
        // Safety: this is the owning thread; `&mut self` prevents two
        // borrows, and no method keeps one across a safepoint.
        unsafe { self.state.local() }
    }

    /// The engine.
    pub fn cascade(&self) -> &Arc<Cascade> {
        &self.cascade
    }

    /// How many local references this mutator holds to `object`.
    pub fn holds(&mut self, object: GcPtr) -> u32 {
        self.local()
            .holds
            .get(&addr(object))
            .copied()
            .unwrap_or(0)
    }

    #[inline]
    fn debug_check_held(&mut self, object: usize) {
        debug_assert!(
            self.local().holds.contains_key(&object),
            "Cascade: the mutator does not hold {object:#x}"
        );
    }

    /// Allocates a unique object of type `ty`, held once by this thread.
    /// A safepoint.
    pub fn alloc(&mut self, ty: &Type) -> Result<GcPtr, AllocError> {
        let object = self.host.cascade_alloc(ty)?;
        self.adopt(object);
        Ok(object)
    }

    /// Allocates a unique array of `n` elements of the array type `ty`,
    /// held once by this thread. A safepoint.
    pub fn alloc_array(&mut self, ty: &Type, n: usize) -> Result<GcPtr, AllocError> {
        let object = self.host.cascade_alloc_array(ty, n)?;
        self.adopt(object);
        Ok(object)
    }

    fn adopt(&mut self, object: GcPtr) {
        let object = addr(object);
        // Safety: freshly allocated by this thread, unreachable by others.
        unsafe { set_word(object, Word::new_unique()) };
        self.local().holds.insert(object, 1);
    }

    /// Records one more local reference to `object` (a copy into a local).
    pub fn hold(&mut self, object: GcPtr) {
        let object = addr(object);
        let count = {
            let entry = self.local().holds.entry(object).or_insert(0);
            *entry += 1;
            *entry
        };
        if count == 2 {
            let word = unsafe { word_of(object) };
            if !word.is_published()
                && matches!(
                    word.state(),
                    OwnershipState::Unique | OwnershipState::Borrowed
                )
            {
                unsafe { set_word(object, word.with_state(OwnershipState::Shared)) };
            }
        }
    }

    /// Drops one local reference to `object`.
    pub fn drop_local(&mut self, object: GcPtr) {
        let object = addr(object);
        let local = self.local();
        let Some(count) = local.holds.get_mut(&object) else {
            debug_assert!(false, "Cascade: dropping an object that is not held");
            return;
        };
        *count -= 1;
        if *count > 0 {
            return;
        }
        local.holds.remove(&object);
        let word = unsafe { word_of(object) };
        if word.is_published() {
            // The root snapshots stop mentioning it; the collector's
            // decrement of the last snapshot queues it.
            return;
        }
        if word.count() == 0 {
            if !word.is(ZCT) {
                unsafe {
                    set_word(
                        object,
                        word.set(ZCT).with_state(OwnershipState::Retiring),
                    )
                };
                local.zct.push(object);
            }
        } else if !word.is(BUFFERED) && !word.is(ZCT) && !word.is_sticky() {
            // The local reference was external to any cycle through the
            // object: it may have been the last one.
            unsafe { set_word(object, word.set(BUFFERED)) };
            local.candidates.insert(object);
        }
    }

    /// The address of the reference slot at byte `offset` of the payload of
    /// the struct `parent`.
    fn field_slot(&mut self, parent: usize, offset: usize) -> *const AtomicPtr<ObjectHeader> {
        let header = unsafe { &*(parent as *const ObjectHeader) };
        let ty = unsafe { header.type_data() };
        assert!(
            ty.ref_offsets().contains(&(offset as u32)),
            "offset {offset} is not a reference field"
        );
        unsafe { header.data.add(offset).cast() }
    }

    /// The address of element `index` of the reference array `array`.
    fn element_slot(&mut self, array: usize, index: usize) -> *const AtomicPtr<ObjectHeader> {
        let header = unsafe { &*(array as *const ObjectHeader) };
        let ty = unsafe { header.type_data() };
        let element = ty.array_element().expect("not an array");
        assert!(element.is_reference(), "not an array of references");
        let length = unsafe { (*header.data.cast::<ArrayHeader>()).length };
        assert!(index < length, "index {index} out of bounds ({length})");
        let stride = element.reference_layout().pad_to_align().size();
        unsafe {
            header
                .data
                .add(array_data_offset(element) + index * stride)
                .cast()
        }
    }

    /// Stores `value` into the reference field at byte `offset` of the
    /// struct `parent` (the write barrier).
    pub fn store(&mut self, parent: GcPtr, offset: usize, value: Option<GcPtr>) {
        let parent = addr(parent);
        self.debug_check_held(parent);
        let slot = self.field_slot(parent, offset);
        self.store_slot(parent, slot, value.map(addr));
    }

    /// Stores `value` into element `index` of the reference array `array`.
    pub fn store_index(&mut self, array: GcPtr, index: usize, value: Option<GcPtr>) {
        let array = addr(array);
        self.debug_check_held(array);
        let slot = self.element_slot(array, index);
        self.store_slot(array, slot, value.map(addr));
    }

    /// Loads the reference field at byte `offset` of `parent`; a non-null
    /// result is held locally.
    pub fn load(&mut self, parent: GcPtr, offset: usize) -> Option<GcPtr> {
        let parent = addr(parent);
        self.debug_check_held(parent);
        let slot = self.field_slot(parent, offset);
        self.load_slot(slot)
    }

    /// Loads element `index` of the reference array `array`.
    pub fn load_index(&mut self, array: GcPtr, index: usize) -> Option<GcPtr> {
        let array = addr(array);
        self.debug_check_held(array);
        let slot = self.element_slot(array, index);
        self.load_slot(slot)
    }

    fn load_slot(&mut self, slot: *const AtomicPtr<ObjectHeader>) -> Option<GcPtr> {
        let value = unsafe { (*slot).load(Ordering::Acquire) };
        if value.is_null() {
            return None;
        }
        let value = handle(value as usize);
        self.hold(value);
        Some(value)
    }

    fn store_slot(
        &mut self,
        parent: usize,
        slot: *const AtomicPtr<ObjectHeader>,
        value: Option<usize>,
    ) {
        if let Some(value) = value {
            self.debug_check_held(value);
        }
        let new = value.map_or(std::ptr::null_mut(), |v| v as *mut ObjectHeader);
        let parent_word = unsafe { word_of(parent) };
        let old = if parent_word.is_published() {
            // Publication closure: what a published object references must
            // be published before another thread can load it.
            if let Some(value) = value {
                if !unsafe { word_of(value) }.is_published() {
                    self.publish_object(value);
                }
            }
            // Other threads may store into the same field: swap, so that
            // every overwritten reference is released exactly once.
            unsafe { (*slot).swap(new, Ordering::AcqRel) }
        } else {
            // Only this thread can reach an unpublished parent.
            let old = unsafe { (*slot).load(Ordering::Relaxed) };
            unsafe { (*slot).store(new, Ordering::Release) };
            old
        };
        // Increment before decrement: storing the value a field already
        // holds never passes through zero.
        if let Some(value) = value {
            self.inc(value);
        }
        if !old.is_null() {
            self.dec(old as usize);
        }
        self.maybe_flush();
    }

    /// Counts a new heap reference to `object`.
    fn inc(&mut self, object: usize) {
        let word = unsafe { word_of(object) };
        if word.is_published() {
            self.local().incs.push(object);
        } else {
            let mut word = word.inc();
            if word.state() != OwnershipState::Shared {
                word = word.with_state(OwnershipState::Shared);
            }
            unsafe { set_word(object, word) };
        }
    }

    /// Counts the removal of a heap reference to `object`.
    fn dec(&mut self, object: usize) {
        let word = unsafe { word_of(object) };
        if word.is_published() {
            self.local().decs.push(object);
            return;
        }
        let word = word.dec();
        let local = self.local();
        if word.count() == 0 {
            if !word.is(ZCT) {
                unsafe {
                    set_word(
                        object,
                        word.set(ZCT).with_state(OwnershipState::Retiring),
                    )
                };
                local.zct.push(object);
                return;
            }
        } else if !word.is(BUFFERED) && !word.is(ZCT) && !word.is_sticky() {
            unsafe { set_word(object, word.set(BUFFERED)) };
            local.candidates.insert(object);
            return;
        }
        unsafe { set_word(object, word) };
    }

    fn maybe_flush(&mut self) {
        let capacity = self.cascade.config.buffer_capacity;
        let local = self.local();
        if local.incs.len() + local.decs.len() < capacity {
            return;
        }
        let packet = Packet {
            thread: self.state.id,
            segment: self.slot.epoch(),
            incs: std::mem::take(&mut self.local().incs),
            decs: std::mem::take(&mut self.local().decs),
            checks: std::mem::take(&mut self.local().checks),
            roots: None,
            standing: Standing::Keep,
        };
        self.cascade.epochs.push(packet);
    }

    /// Publishes `object` and the unpublished objects it reaches: from now
    /// on any thread may access them (`CASCADE.md` section 4).
    pub fn publish(&mut self, object: GcPtr) {
        let object = addr(object);
        self.debug_check_held(object);
        self.publish_object(object);
    }

    fn publish_object(&mut self, root: usize) {
        let mut stack = vec![root];
        while let Some(object) = stack.pop() {
            let word = unsafe { word_of(object) };
            if word.is_published() || !word.is_tracked() {
                continue;
            }
            let local = self.local();
            if word.is(BUFFERED) {
                local.candidates.remove(&object);
            }
            // The collector examines the root and every zero-count object
            // at the next boundary: a zero is retired, any other count makes
            // the object a cycle candidate. Without this, an object
            // published and dropped before any root snapshot named it would
            // never be looked at again if only a cycle keeps its count up.
            if word.count() == 0 || object == root {
                local.checks.push(object);
            }
            // A local zero-count entry is skipped once published.
            let word = word
                .clear(ZCT | BUFFERED)
                .set(super::word::PUBLISHED)
                .with_state(OwnershipState::Shared);
            unsafe { children_of(object, &mut |child| stack.push(child)) };
            unsafe { publish_word(object, word) };
        }
    }

    /// Releases the last owning reference to `object` at its last use. If
    /// the object is provably unique -- unpublished, `UNIQUE`, no heap
    /// reference, no borrow, no host root, held once -- it is reclaimed now
    /// and `true` is returned. Otherwise this is [`Mutator::drop_local`]
    /// and the object is reclaimed when it is proven dead (`false`).
    pub fn release_unique(&mut self, object: GcPtr) -> bool {
        let a = addr(object);
        let word = unsafe { word_of(a) };
        let unique = self.local().holds.get(&a) == Some(&1)
            && !word.is_published()
            && word.state() == OwnershipState::Unique
            && word.count() == 0
            && word.borrows() == 0
            && !word.is(ZCT)
            && !word.is(BUFFERED)
            && unsafe { host_roots(a) } == 0;
        if !unique {
            self.cascade
                .counters
                .unique_fallbacks
                .fetch_add(1, Ordering::Relaxed);
            self.drop_local(object);
            return false;
        }
        self.local().holds.remove(&a);
        let mut freed = Vec::new();
        self.free_local(a, &mut freed, None);
        self.cascade
            .counters
            .unique_reclaimed
            .fetch_add(1, Ordering::Relaxed);
        self.drain_zct(&mut freed);
        self.release(freed);
        true
    }

    /// Starts a scoped borrow of the held object `object`. Returns `true`
    /// if the borrow was recorded (the object is unpublished and unique);
    /// otherwise local references already protect it and nothing changes.
    pub fn borrow(&mut self, object: GcPtr) -> bool {
        let a = addr(object);
        self.debug_check_held(a);
        let word = unsafe { word_of(a) };
        if word.is_published()
            || !matches!(
                word.state(),
                OwnershipState::Unique | OwnershipState::Borrowed
            )
            || word.borrows() == MAX_BORROWS
        {
            return false;
        }
        let word = word
            .with_borrows(word.borrows() + 1)
            .with_state(OwnershipState::Borrowed);
        unsafe { set_word(a, word) };
        true
    }

    /// Ends a borrow that [`Mutator::borrow`] recorded.
    pub fn end_borrow(&mut self, object: GcPtr) {
        let a = addr(object);
        let word = unsafe { word_of(a) };
        if word.is_published() || word.borrows() == 0 {
            return;
        }
        let mut word = word.with_borrows(word.borrows() - 1);
        if word.borrows() == 0 && word.state() == OwnershipState::Borrowed {
            word = word.with_state(OwnershipState::Unique);
        }
        unsafe { set_word(a, word) };
    }

    /// A safepoint and an epoch handshake: parks if a collection waits,
    /// reclaims this thread's dead unpublished objects (zero counts, then
    /// one bounded local cycle pass), and hands the logged operations and a
    /// root snapshot to the collector.
    pub fn safepoint(&mut self) {
        safepoint::poll();
        self.reclaim_local();
        self.handshake(false, false);
    }

    /// Runs `f` with this thread quiescent: the epoch may advance without
    /// it, its locals being protected by the snapshot it hands over now. `f`
    /// runs in a `safepoint::blocking` region, so it must not touch the GC
    /// heap.
    pub fn quiescent<R>(&mut self, f: impl FnOnce() -> R) -> R {
        self.reclaim_local();
        self.handshake(true, false);
        let result = safepoint::blocking(f);
        self.handshake(false, false);
        result
    }

    /// Reclaims this thread's dead unpublished objects now.
    pub fn reclaim_local(&mut self) {
        let mut freed = Vec::new();
        self.drain_zct(&mut freed);
        self.local_cycles(&mut freed);
        self.drain_zct(&mut freed);
        self.release(freed);
    }

    fn drain_zct(&mut self, freed: &mut Vec<usize>) {
        while let Some(object) = self.local().zct.pop() {
            let word = unsafe { word_of(object) };
            if word.is_published() || !word.is(ZCT) {
                continue;
            }
            let held = self.local().holds.contains_key(&object);
            if word.count() > 0 || held || word.borrows() > 0 || unsafe { host_roots(object) } > 0
            {
                // Still referenced. A later release of a local reference or
                // borrow queues it again; but a count that rose from zero
                // while it was queued here may now be kept up by a cycle
                // through it, so it becomes a cycle candidate.
                let mut word = word.clear(ZCT);
                if word.state() == OwnershipState::Retiring {
                    word = word.with_state(OwnershipState::Shared);
                }
                if word.count() > 0 && !held && !word.is(BUFFERED) && !word.is_sticky() {
                    word = word.set(BUFFERED);
                    self.local().candidates.insert(object);
                }
                unsafe { set_word(object, word) };
                continue;
            }
            self.free_local(object, freed, None);
            self.cascade
                .counters
                .local_reclaimed
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Marks the dead unpublished `object` reclaimed and releases its
    /// references (outside `except`).
    fn free_local(&mut self, object: usize, freed: &mut Vec<usize>, except: Option<&FxHashSet<usize>>) {
        let word = unsafe { word_of(object) };
        if word.is(BUFFERED) {
            self.local().candidates.remove(&object);
        }
        let mut children = Vec::new();
        unsafe { children_of(object, &mut |child| children.push(child)) };
        for child in children {
            if except.is_some_and(|set| set.contains(&child)) {
                continue;
            }
            if unsafe { word_of(child) }.is_tracked() {
                self.dec(child);
            }
        }
        unsafe { set_word(object, Word::UNTRACKED.with_state(OwnershipState::Reclaimed)) };
        freed.push(object);
    }

    /// The owner's synchronous cycle pass (`CASCADE.md` section 6).
    fn local_cycles(&mut self, freed: &mut Vec<usize>) {
        if self.local().candidates.is_empty() {
            return;
        }
        let candidates: Vec<usize> = self.local().candidates.drain().collect();
        let mut roots = Vec::new();
        for object in candidates {
            let word = unsafe { word_of(object) };
            unsafe { set_word(object, word.clear(BUFFERED)) };
            if !word.is_published() && !word.is(ZCT) && word.count() > 0 {
                roots.push(object);
            }
        }
        let budget = self.cascade.config.local_cycle_budget;
        let white = {
            let view = LocalView {
                local: self.local(),
            };
            let mut work = 0;
            trial_deletion(&view, &roots, budget, &mut work)
        };
        let Ok(white) = white else {
            // Left to the tracing collector.
            return;
        };
        if white.is_empty() {
            return;
        }
        let set: FxHashSet<usize> = white.iter().copied().collect();
        for &member in &white {
            self.free_local(member, freed, Some(&set));
        }
        self.cascade
            .counters
            .cycle_reclaimed
            .fetch_add(white.len() as u64, Ordering::Relaxed);
    }

    fn release(&mut self, mut freed: Vec<usize>) {
        if freed.is_empty() {
            return;
        }
        let bytes = {
            let mut inner = self.cascade.heap.inner.write();
            self.cascade.release_to_heap(&mut inner, &mut freed)
        };
        self.cascade
            .counters
            .reclaimed_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Hands the logged operations (and, entering a new epoch, a root
    /// snapshot) to the collector and announces the epoch.
    fn handshake(&mut self, quiescent: bool, last: bool) {
        let state = self.state.clone();
        let quiescent_before = self.quiescent_before;
        self.cascade.epochs.handshake(&self.slot, quiescent, |h| {
            // Safety: the owning thread, inside `&mut self`.
            let local = unsafe { state.local() };
            let snapshot = h.new_epoch || quiescent || last;
            let roots = snapshot.then(|| {
                if last {
                    return Vec::new();
                }
                local
                    .holds
                    .keys()
                    .copied()
                    .filter(|&o| unsafe { word_of(o) }.is_published())
                    .collect::<Vec<_>>()
            });
            let standing = if last || (quiescent_before && !quiescent) {
                Standing::Clear
            } else if quiescent {
                Standing::Set
            } else {
                Standing::Keep
            };
            if roots.is_none()
                && local.incs.is_empty()
                && local.decs.is_empty()
                && local.checks.is_empty()
                && standing == Standing::Keep
            {
                return None;
            }
            Some(Packet {
                thread: state.id,
                segment: h.segment,
                incs: std::mem::take(&mut local.incs),
                decs: std::mem::take(&mut local.decs),
                checks: std::mem::take(&mut local.checks),
                roots,
                standing,
            })
        });
        self.quiescent_before = quiescent;
    }
}

impl Drop for Mutator {
    fn drop(&mut self) {
        self.reclaim_local();
        // Published objects still held are simply no longer held; the
        // remaining unpublished ones are left to the tracing collector.
        self.handshake(false, true);
        self.local().holds.clear();
        self.cascade.unregister(&self.state, &self.slot);
    }
}

/// The owner's view for its synchronous cycle pass: its unpublished,
/// counted objects; local holds, borrows and host roots are external.
struct LocalView<'a> {
    local: &'a Local,
}

impl CycleGraph for LocalView<'_> {
    fn count(&self, object: usize) -> Option<u32> {
        let header = object as *const ObjectHeader;
        if unsafe { (*header).state.load(Ordering::Relaxed) } != STATE_ALLOCATED {
            return None;
        }
        let word = unsafe { word_of(object) };
        let usable =
            word.is_tracked() && !word.is_published() && !word.is(ZCT) && !word.is_sticky();
        usable.then(|| {
            word.count()
                + self.local.holds.get(&object).copied().unwrap_or(0)
                + word.borrows()
                + u32::from(unsafe { host_roots(object) } > 0)
        })
    }

    fn children(&self, object: usize, f: &mut dyn FnMut(usize)) {
        unsafe { children_of(object, f) }
    }
}
