//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Mark-sweep garbage collector over the segregated size-class heap
//!   (`gc::heap`): host-rooted tracing, sweeping, and hot-reload memory
//!   migration.
use std::{
    alloc::Layout,
    collections::HashMap,
    ptr::NonNull,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    cast,
    gc::{
        array::ArrayHeader,
        heap::{
            array_payload_layout, handle_of, header_of, AllocError, Heap, HeapInner, ObjectHeader,
            PayloadMigration, COLOR_BLACK, COLOR_WHITE,
        },
        safepoint, Array as GcArray, Event, GcPtr, GcRuntime, Observer, Stats, TypeTrace,
    },
    mapping::{self, resolve_struct_to_struct_edit, Action, FieldMapping, Mapping, MemoryMapper},
    r#type::{Type, TypeData},
};

/// Returns the offset of the first element of an array whose elements are of
/// type `element`, relative to the start of the array payload.
pub(crate) fn array_data_offset(element: &TypeData) -> usize {
    Layout::new::<ArrayHeader>()
        .extend(element.reference_layout())
        .expect("error creating combined layout of header and element")
        .1
}

/// Calls `f` for every non-null GC reference stored in the payload of
/// `header`.
///
/// # Safety
///
/// `header` must be an allocated object whose payload matches its type.
#[inline]
unsafe fn for_each_reference(header: &ObjectHeader, mut f: impl FnMut(NonNull<ObjectHeader>)) {
    let ty = header.type_data();
    let payload = header.data;
    let mut visit = |slot: *const u8| {
        if let Some(child) = NonNull::new(*slot.cast::<*mut ObjectHeader>()) {
            f(child);
        }
    };

    if let Some(element) = ty.array_element() {
        let length = (*payload.cast::<ArrayHeader>()).length;
        if length == 0 {
            return;
        }
        let elements = payload.add(array_data_offset(element));
        let stride = element.reference_layout().pad_to_align().size();
        if element.is_reference() {
            for index in 0..length {
                visit(elements.add(index * stride));
            }
        } else {
            let offsets = element.ref_offsets();
            if offsets.is_empty() {
                return;
            }
            for index in 0..length {
                let element_ptr = elements.add(index * stride);
                for &offset in offsets {
                    visit(element_ptr.add(offset as usize));
                }
            }
        }
    } else {
        for &offset in ty.ref_offsets() {
            visit(payload.add(offset as usize));
        }
    }
}

/// An iterator over all GC objects referenced by another object.
pub struct Trace {
    references: std::vec::IntoIter<GcPtr>,
}

impl Iterator for Trace {
    type Item = GcPtr;

    fn next(&mut self) -> Option<Self::Item> {
        self.references.next()
    }
}

impl TypeTrace for Type {
    type Trace = Trace;

    fn trace(&self, obj: GcPtr) -> Self::Trace {
        let header = header_of(obj);
        let mut references = Vec::new();
        // Safety: the caller provides a handle to an allocated object.
        unsafe {
            for_each_reference(header.as_ref(), |child| references.push(handle_of(child)));
        }
        Trace {
            references: references.into_iter(),
        }
    }
}

/// Implements a mark-sweep garbage collector.
///
/// Objects live in a segregated size-class heap with inline headers (see the
/// `gc::heap` module). Roots are the objects the host rooted through
/// [`GcRuntime::root`], plus -- because generated code has no stack maps --
/// every word on the stack of every thread that runs Codira code, scanned
/// conservatively. A collection stops the world first: it waits until every
/// such thread has parked at a safepoint (an allocation) or in a blocking
/// runtime call, scans their stacks, collects, and resumes them (see
/// [`safepoint`]).
pub struct MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    pub(crate) heap: Arc<Heap>,
    observer: O,
    /// Collections started because an allocation hit the heap limit.
    emergency_collections: AtomicUsize,
}

impl<O> Default for MarkSweep<O>
where
    O: Observer<Event = Event> + Default,
{
    fn default() -> Self {
        Self::with_observer(O::default())
    }
}

impl<O> MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    /// Creates a `MarkSweep` memory collector with the specified `Observer`.
    pub fn with_observer(observer: O) -> Self {
        Self {
            heap: Heap::new(None),
            observer,
            emergency_collections: AtomicUsize::new(0),
        }
    }

    /// Returns the observer
    pub fn observer(&self) -> &O {
        &self.observer
    }

    /// Returns the number of bytes the heap currently holds from the system:
    /// size-class blocks (including cached empty blocks), individually
    /// allocated large objects and out-of-line payloads. This is larger than
    /// [`Stats::allocated_memory`], which only counts live payload bytes.
    pub fn committed_bytes(&self) -> usize {
        self.heap.committed()
    }

    /// Limits the bytes the heap may hold from the system (size-class blocks,
    /// large objects and out-of-line payloads; see
    /// [`MarkSweep::committed_bytes`]). `None` removes the limit. Lowering
    /// the limit below the current size releases nothing; the heap simply
    /// stops growing.
    ///
    /// This caps the garbage-collected heap only: raw allocations a program
    /// makes outside the GC (arenas, `codira_alloc`, pages) are not counted.
    pub fn set_heap_limit(&self, limit: Option<usize>) {
        self.heap.inner.write().set_limit(limit);
    }

    /// Returns the heap limit, if any.
    pub fn heap_limit(&self) -> Option<usize> {
        self.heap.inner.read().limit()
    }

    /// Returns how many collections were started because an allocation hit
    /// the heap limit.
    pub fn emergency_collections(&self) -> usize {
        self.emergency_collections.load(Ordering::Relaxed)
    }

    /// Allocates an object of type `ty`, or returns why the heap could not
    /// provide the memory.
    ///
    /// Every allocation is a safepoint: a thread running Codira code parks
    /// here while another thread collects.
    ///
    /// When the allocation would exceed the heap limit, the collector runs
    /// first (stopping the world, so every thread running Codira code is
    /// parked and its stack scanned conservatively, this thread's included)
    /// and the allocation is retried once. If the collection cannot run
    /// safely -- the current thread is itself collecting or migrating, or
    /// some thread's stack cannot be scanned on this platform -- the
    /// allocation fails with [`AllocError::LimitExceeded`] without
    /// collecting.
    pub fn try_alloc(&self, ty: &Type) -> Result<GcPtr, AllocError> {
        assert!(ty.is_concrete());

        let header = self.alloc_payload(ty, ty.value_layout())?;
        let handle = handle_of(header);
        self.observer.event(Event::Allocation(handle));
        Ok(handle)
    }

    /// Allocates an array of type `ty` with `n` elements, or returns why the
    /// heap could not provide the memory. See [`MarkSweep::try_alloc`].
    pub fn try_alloc_array(&self, ty: &Type, n: usize) -> Result<ArrayHandle, AllocError> {
        let element = unsafe { ty.data_ptr().as_ref() }
            .array_element()
            .expect("array type doesnt have an element type");
        let layout = array_payload_layout(element, n).ok_or(AllocError::TooLarge)?;
        let header = self.alloc_payload(ty, layout)?;
        unsafe { init_array_header(header.as_ref().data, n) };
        self.observer.event(Event::Allocation(handle_of(header)));
        Ok(ArrayHandle { obj: header })
    }

    /// Allocates an object with a payload of `layout`, collecting once if
    /// the heap limit is hit and that is safe.
    #[inline]
    fn alloc_payload(
        &self,
        ty: &Type,
        layout: Layout,
    ) -> Result<NonNull<ObjectHeader>, AllocError> {
        if safepoint::is_running() {
            safepoint::poll();
            self.alloc_or_collect(ty, layout)
        } else if safepoint::holds_stop() {
            // Every other thread is parked, and this one must not start a
            // collection in the middle of its own stop.
            self.heap.alloc(ty, layout)
        } else {
            // Not running Codira code: allocate in a brief running section,
            // so no collection can sweep the cell while it is initialized.
            safepoint::with_allocation_section(|| self.alloc_or_collect(ty, layout))
        }
    }

    fn alloc_or_collect(
        &self,
        ty: &Type,
        layout: Layout,
    ) -> Result<NonNull<ObjectHeader>, AllocError> {
        match self.heap.alloc(ty, layout) {
            Err(err @ AllocError::LimitExceeded { .. }) => {
                // Collecting needs this thread to be able to park; a thread
                // that is exiting cannot.
                if !safepoint::is_running() {
                    return Err(err);
                }
                // Retry while the world is still stopped: otherwise the
                // other threads, resumed first, could use up what the
                // collection freed before this thread gets any of it.
                self.collect_stopped_then(|| self.heap.alloc(ty, layout))
                    .map_err(|CollectRefused| err)
                    .and_then(|(_, result)| {
                        self.emergency_collections.fetch_add(1, Ordering::Relaxed);
                        result
                    })
            }
            result => result,
        }
    }

    /// Collects all memory that is no longer referenced by rooted objects
    /// or from the stack of a thread running Codira code. Returns `true` if
    /// memory was reclaimed, `false` otherwise -- including when the
    /// collection was refused (see [`MarkSweep::try_collect`]).
    ///
    /// Safe to call from any thread at any time: the collection stops the
    /// world first. It waits for every thread running Codira code to reach a
    /// safepoint, so it can take as long as the longest stretch of Codira
    /// code that neither allocates nor blocks.
    pub fn collect(&self) -> bool {
        self.try_collect().unwrap_or(false)
    }

    /// Like [`MarkSweep::collect`], but says when it refused to collect:
    /// some thread inside Codira code is parked on a platform whose stacks
    /// cannot be scanned, so collecting could free objects only it refers
    /// to.
    pub fn try_collect(&self) -> Result<bool, CollectRefused> {
        self.collect_stopped_then(|| ()).map(|(freed, ())| freed)
    }

    /// Stops the world, collects, and runs `then` before resuming the
    /// other threads. Returns whether memory was reclaimed and what `then`
    /// returned.
    fn collect_stopped_then<R>(
        &self,
        then: impl FnOnce() -> R,
    ) -> Result<(bool, R), CollectRefused> {
        safepoint::stop_the_world(|world| {
            if world.has_unscannable_threads() {
                return Err(CollectRefused);
            }
            let mut words = world.stack_words();
            let freed = if words.is_empty() {
                self.collect_impl(None)
            } else {
                self.collect_impl(Some(&mut words))
            };
            Ok((freed, then()))
        })
    }

    fn collect_impl(&self, stack_words: Option<&mut Vec<usize>>) -> bool {
        self.observer.event(Event::Start);

        let mut inner = self.heap.inner.write();

        // In debug builds, validate every traced reference before following it.
        #[cfg(debug_assertions)]
        let known: FxHashSet<usize> = {
            let mut known = FxHashSet::default();
            inner.for_each_object(|o| {
                known.insert(o.as_ptr() as usize);
            });
            known
        };

        // Mark all roots
        let mut stack: Vec<NonNull<ObjectHeader>> = Vec::new();
        inner.for_each_object(|object| {
            let header = object.as_ptr();
            unsafe {
                if (*header).roots.load(std::sync::atomic::Ordering::Acquire) > 0
                    && (*header).color == COLOR_WHITE
                {
                    (*header).color = COLOR_BLACK;
                    stack.push(object);
                }
            }
        });

        // Objects the scanned stack may refer to are roots too
        if let Some(words) = stack_words {
            for object in inner.objects_referenced_by(words) {
                let header = object.as_ptr();
                unsafe {
                    if (*header).color == COLOR_WHITE {
                        (*header).color = COLOR_BLACK;
                        stack.push(object);
                    }
                }
            }
        }

        // Trace everything reachable from the roots
        while let Some(object) = stack.pop() {
            unsafe {
                for_each_reference(object.as_ref(), |child| {
                    #[cfg(debug_assertions)]
                    assert!(
                        known.contains(&(child.as_ptr() as usize)),
                        "found invalid reference"
                    );
                    let child_header = child.as_ptr();
                    if (*child_header).color == COLOR_WHITE {
                        (*child_header).color = COLOR_BLACK;
                        stack.push(child);
                    }
                });
            }
        }

        let freed = inner.sweep(|handle| self.observer.event(Event::Deallocation(handle)));
        drop(inner);

        self.observer.event(Event::End);

        freed > 0
    }
}

/// [`MarkSweep::try_collect`] refused to collect: a thread inside Codira code
/// cannot have its stack scanned on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a thread running Codira code cannot have its stack scanned on this platform")]
pub struct CollectRefused;

/// Helper object to work with [`GcPtr`] that represents an array.
///
/// Arrays are stored in memory with a header which holds the length and
/// capacity. The memory layout of an array looks like this in memory:
///
/// ```text
/// object.data ───►┌──────────────┐
///                 │ ArrayHeader  │
///                 └─┬────────────┘
///                   │ padding to align elements
///                 ┌─┴────────────┐
///                 │ element #1   │
///                 └──────────────┘
///                  :
///                 ┌──────────────┐
///                 │ element #n   │
///                 └──────────────┘
/// ```
pub struct ArrayHandle {
    /// Pointer to the object header.
    obj: NonNull<ObjectHeader>,
}

impl ArrayHandle {
    /// Returns a reference to the header
    pub fn header(&self) -> &ArrayHeader {
        // Safety: the handle refers to an allocated array object.
        unsafe { &*self.obj.as_ref().data.cast::<ArrayHeader>() }
    }

    /// Sets the length of the array.
    ///
    /// # Safety
    ///
    /// This function is unsafe because the array elements might be left
    /// uninitialized.
    pub unsafe fn set_length(&mut self, length: usize) {
        let header = &mut *self.obj.as_ref().data.cast::<ArrayHeader>();
        debug_assert!(header.capacity >= length);
        header.length = length;
    }

    /// Returns the layout of an element stored in the array.
    ///
    /// Note that this may be different from the layout of the
    /// [`Self::element_type`]. If the element type is a garbage collected
    /// type, the array stores references instead of raw elements.
    pub fn element_layout(&self) -> Layout {
        self.element_type().reference_layout()
    }

    /// Returns the stride in bytes between elements.
    ///
    /// The stride is determined by the size of [`Self::element_layout`] padded
    /// to alignment of layout.
    pub fn element_stride(&self) -> usize {
        self.element_layout().pad_to_align().size()
    }

    /// Returns a pointer to the data.
    pub fn data(&self) -> NonNull<u8> {
        unsafe {
            let header = self.obj.as_ref();
            let element = header
                .type_data()
                .array_element()
                .expect("unable to determine element_type, type is not an array");
            NonNull::new_unchecked(header.data.add(array_data_offset(element)))
        }
    }
}

impl GcArray for ArrayHandle {
    type Iterator = ArrayHandleIter;

    fn as_raw(&self) -> GcPtr {
        handle_of(self.obj)
    }

    fn element_type(&self) -> Type {
        let array_ty = unsafe { self.obj.as_ref().type_view() };
        array_ty
            .as_array()
            .expect("unable to determine element_type, type is not an array")
            .element_type()
    }

    fn length(&self) -> usize {
        self.header().length
    }

    fn capacity(&self) -> usize {
        self.header().capacity
    }

    fn elements(&self) -> Self::Iterator {
        ArrayHandleIter {
            remaining: self.length(),
            next: self.data(),
            stride: self.element_stride(),
        }
    }
}

/// An iterator implementation.
///
/// TODO: Note that this iterator is highly non-thread safe. Any operation that
/// modifies the original array could cause undefined behavior.
pub struct ArrayHandleIter {
    /// Pointer to the next element
    next: NonNull<u8>,

    /// The number of remaning elements in the iterator.
    remaining: usize,

    /// The number of bytes to skip to get to the next element
    stride: usize,
}

impl Iterator for ArrayHandleIter {
    type Item = NonNull<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining > 0 {
            let element = self.next;
            self.remaining -= 1;
            self.next = unsafe { NonNull::new_unchecked(self.next.as_ptr().add(self.stride)) };
            Some(element)
        } else {
            None
        }
    }
}

/// Returns the payload layout of an array of type `array_ty` with `length`
/// elements.
fn array_layout(array_ty: &Type, length: usize) -> Layout {
    let element = unsafe { array_ty.data_ptr().as_ref() }
        .array_element()
        .expect("array type doesnt have an element type");
    array_payload_layout(element, length).expect("unable to create memory layout for array")
}

/// Writes the length and capacity of a freshly allocated array payload.
unsafe fn init_array_header(payload: *mut u8, length: usize) {
    let header = &mut *payload.cast::<ArrayHeader>();
    header.length = length;
    header.capacity = length;
}

impl<O> GcRuntime for MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    type Array = ArrayHandle;

    fn alloc(&self, ty: &Type) -> GcPtr {
        self.try_alloc(ty)
            .unwrap_or_else(|err| panic!("GC allocation of `{}` failed: {err}", ty.name()))
    }

    fn alloc_array(&self, ty: &Type, n: usize) -> Self::Array {
        self.try_alloc_array(ty, n)
            .unwrap_or_else(|err| panic!("GC allocation of `{}` failed: {err}", ty.name()))
    }

    fn ptr_type(&self, handle: GcPtr) -> Type {
        let _lock = self.heap.inner.read();
        unsafe { header_of(handle).as_ref().type_view().clone() }
    }

    fn array(&self, handle: GcPtr) -> Option<Self::Array> {
        let _lock = self.heap.inner.read();
        let obj = header_of(handle);
        if unsafe { obj.as_ref().type_data() }.is_array() {
            Some(ArrayHandle { obj })
        } else {
            None
        }
    }

    fn root(&self, handle: GcPtr) {
        unsafe { header_of(handle).as_ref() }
            .roots
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn unroot(&self, handle: GcPtr) {
        unsafe { header_of(handle).as_ref() }
            .roots
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn stats(&self) -> Stats {
        Stats {
            allocated_memory: self.heap.allocated_bytes(),
        }
    }
}

/// State shared by the migration helpers of [`MarkSweep::map_memory`].
struct MigrationContext<'a> {
    inner: &'a mut HeapInner,
    conversions: &'a HashMap<Type, mapping::StructMapping>,
    /// Payload migrations in flight, keyed by header address, with the type
    /// the object gets once the migration completes.
    migrations: FxHashMap<usize, (PayloadMigration, Type)>,
    /// Objects allocated while migrating, reported to the observer afterwards.
    new_objects: Vec<GcPtr>,
}

impl MigrationContext<'_> {
    /// Allocates a new object during migration.
    fn alloc(&mut self, ty: &Type, layout: Layout) -> NonNull<ObjectHeader> {
        let header = self
            .inner
            .alloc_locked(ty, layout)
            .unwrap_or_else(|err: AllocError| panic!("allocation during migration failed: {err}"));
        self.new_objects.push(handle_of(header));
        header
    }

    /// Returns the payload of `header` as it was before migration started.
    fn old_payload(&self, header: NonNull<ObjectHeader>) -> NonNull<u8> {
        match self.migrations.get(&(header.as_ptr() as usize)) {
            Some((migration, _)) => migration.src,
            None => unsafe { NonNull::new_unchecked(header.as_ref().data) },
        }
    }

    /// Starts migrating `header` to `new_ty` with a payload of `layout`.
    /// Returns `(old payload, new payload)`.
    fn begin(
        &mut self,
        header: NonNull<ObjectHeader>,
        layout: Layout,
        new_ty: &Type,
    ) -> (NonNull<u8>, NonNull<u8>) {
        let migration = unsafe { self.inner.begin_payload_migration(header, layout) }
            .unwrap_or_else(|err| panic!("migrating an object failed: {err}"));
        let pair = (migration.src, migration.dest);
        self.migrations
            .insert(header.as_ptr() as usize, (migration, new_ty.clone()));
        pair
    }

    /// Migrates an array object to `new_ty`, mapping every element with
    /// `element_action`. Arrays that are already being migrated are left
    /// alone.
    fn migrate_array(
        &mut self,
        header: NonNull<ObjectHeader>,
        element_action: &Action,
        new_ty: &Type,
    ) {
        if self.migrations.contains_key(&(header.as_ptr() as usize)) {
            return;
        }
        let old_ty = unsafe { header.as_ref().type_view().clone() };
        let old_element = unsafe { old_ty.data_ptr().as_ref() }
            .array_element()
            .expect("migrating an array that is not an array");
        let length = unsafe { (*header.as_ref().data.cast::<ArrayHeader>()).length };

        let (src, dest) = self.begin(header, array_layout(new_ty, length), new_ty);
        unsafe { init_array_header(dest.as_ptr(), length) };

        let new_element_ty = new_ty.as_array().expect("Must be an array.").element_type();
        let new_element = unsafe { new_ty.data_ptr().as_ref() }
            .array_element()
            .expect("Must be an array.");
        let src_stride = old_element.reference_layout().pad_to_align().size();
        let dest_stride = new_element.reference_layout().pad_to_align().size();
        let src_elements = unsafe { src.as_ptr().add(array_data_offset(old_element)) };
        let dest_elements = unsafe { dest.as_ptr().add(array_data_offset(new_element)) };
        for index in 0..length {
            unsafe {
                self.map_type(
                    NonNull::new_unchecked(src_elements.add(index * src_stride)),
                    NonNull::new_unchecked(dest_elements.add(index * dest_stride)),
                    element_action,
                    &new_element_ty,
                );
            }
        }
    }

    fn map_struct(&mut self, mapping: &[FieldMapping], src: NonNull<u8>, dest: NonNull<u8>) {
        for FieldMapping {
            new_ty,
            new_offset,
            action,
        } in mapping.iter()
        {
            let field_dest = unsafe { NonNull::new_unchecked(dest.as_ptr().add(*new_offset)) };
            self.map_type(src, field_dest, action, new_ty);
        }
    }

    fn conversion(&self, old_ty: &Type) -> &'_ mapping::StructMapping {
        self.conversions.get(old_ty).unwrap_or_else(|| {
            panic!("If the struct changed, there must also be a conversion for type: {old_ty:#?}.")
        })
    }

    fn map_type(&mut self, src: NonNull<u8>, dest: NonNull<u8>, action: &Action, new_ty: &Type) {
        unsafe fn field(ptr: NonNull<u8>, offset: usize) -> NonNull<u8> {
            NonNull::new_unchecked(ptr.as_ptr().add(offset))
        }
        unsafe fn write_handle(dest: NonNull<u8>, header: NonNull<ObjectHeader>) {
            *dest.cast::<GcPtr>().as_ptr() = handle_of(header);
        }
        unsafe fn read_header(src: NonNull<u8>) -> Option<NonNull<ObjectHeader>> {
            NonNull::new(*src.cast::<*mut ObjectHeader>().as_ptr())
        }

        match action {
            Action::ArrayAlloc => {
                let header = self.alloc(new_ty, array_layout(new_ty, 0));
                unsafe {
                    init_array_header(header.as_ref().data, 0);
                    write_handle(dest, header);
                }
            }
            Action::ArrayFromValue {
                element_action,
                old_offset,
            } => {
                let header = self.alloc(new_ty, array_layout(new_ty, 1));
                let element_ty = new_ty.as_array().expect("Must be an array.").element_type();
                let element = unsafe { element_ty.data_ptr().as_ref() };
                unsafe {
                    let payload = header.as_ref().data;
                    init_array_header(payload, 1);
                    let data = NonNull::new_unchecked(payload.add(array_data_offset(element)));
                    self.map_type(field(src, *old_offset), data, element_action, &element_ty);
                    write_handle(dest, header);
                }
            }
            Action::ArrayMap {
                element_action,
                old_offset,
            } => unsafe {
                let src_ptr = field(src, *old_offset);
                if let Some(array) = read_header(src_ptr) {
                    self.migrate_array(array, element_action, new_ty);
                }
                std::ptr::copy_nonoverlapping(
                    src_ptr.as_ptr(),
                    dest.as_ptr(),
                    std::mem::size_of::<GcPtr>(),
                );
            },
            Action::Cast { old_offset, old_ty } => {
                // A failed cast leaves the zero-initialized value in place.
                let _ = cast::try_cast_from_to(
                    old_ty.clone(),
                    new_ty.clone(),
                    unsafe { field(src, *old_offset) },
                    dest,
                );
            }
            Action::Copy { old_offset, size } => unsafe {
                std::ptr::copy_nonoverlapping(
                    field(src, *old_offset).as_ptr(),
                    dest.as_ptr(),
                    *size,
                );
            },
            Action::ElementFromArray {
                element_action,
                old_offset,
            } => unsafe {
                if let Some(array) = read_header(field(src, *old_offset)) {
                    let payload = self.old_payload(array);
                    if (*payload.as_ptr().cast::<ArrayHeader>()).length > 0 {
                        let old_element_ty = array.as_ref().type_view();
                        let old_element = old_element_ty
                            .data_ptr()
                            .as_ref()
                            .array_element()
                            .expect("Must be an array.");
                        let data = NonNull::new_unchecked(
                            payload.as_ptr().add(array_data_offset(old_element)),
                        );
                        self.map_type(data, dest, element_action, new_ty);
                    }
                }
            },
            Action::StructAlloc => {
                let header = self.alloc(new_ty, new_ty.value_layout());
                unsafe { write_handle(dest, header) };
            }
            Action::StructMapFromGc { old_ty, old_offset } => unsafe {
                if let Some(object) = read_header(field(src, *old_offset)) {
                    let payload = self.old_payload(object);
                    let mapping =
                        &self.conversion(old_ty).field_mapping as *const Vec<FieldMapping>;
                    self.map_struct(&*mapping, payload, dest);
                }
            },
            Action::StructMapFromValue { old_ty, old_offset } => {
                let header = self.alloc(new_ty, new_ty.value_layout());
                let mapping = &self.conversion(old_ty).field_mapping as *const Vec<FieldMapping>;
                unsafe {
                    let payload = NonNull::new_unchecked(header.as_ref().data);
                    self.map_struct(&*mapping, field(src, *old_offset), payload);
                    write_handle(dest, header);
                }
            }
            Action::StructMapInPlace { old_ty, old_offset } => {
                let mapping = &self.conversion(old_ty).field_mapping as *const Vec<FieldMapping>;
                unsafe { self.map_struct(&*mapping, field(src, *old_offset), dest) };
            }
            Action::ZeroInitialize => {
                // The destination is already zero-initialized.
            }
        }
    }
}

impl<O> MemoryMapper for MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    #[allow(clippy::mutable_key_type)]
    fn map_memory(&self, mapping: Mapping) -> Vec<GcPtr> {
        // Rewriting objects in place while another thread runs Codira code
        // would race with it; stop the world (a no-op for the thread that
        // already holds the stop, as a reload does).
        safepoint::stop_the_world(|_| self.map_memory_stopped(mapping))
    }
}

impl<O> MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    #[allow(clippy::mutable_key_type)]
    fn map_memory_stopped(&self, mapping: Mapping) -> Vec<GcPtr> {
        let mut inner = self.heap.inner.write();
        let objects = inner.objects();

        // Determine which objects still use deleted types
        let deleted = objects
            .iter()
            .filter(|o| {
                mapping
                    .deletions
                    .contains(&*unsafe { o.as_ref().type_view() })
            })
            .map(|o| handle_of(*o))
            .collect();

        // Update the types of objects whose type didn't change
        let identical: HashMap<Type, Type> = mapping.identical.iter().cloned().collect();
        for object in &objects {
            let header = object.as_ptr();
            let new_ty = identical.get(&*unsafe { (*header).type_view() });
            if let Some(new_ty) = new_ty {
                inner.register_type(new_ty);
                unsafe { (*header).ty = new_ty.data_ptr().as_ptr() };
            }
        }

        // Plan the struct and array migrations
        let mut struct_migrations = Vec::new();
        let mut array_migrations = Vec::new();
        let mut retyped_arrays = Vec::new();
        for &object in &objects {
            let ty = unsafe { object.as_ref().type_view() };
            if ty.is_struct() {
                if let Some(conversion) = mapping.struct_mappings.get(&*ty) {
                    struct_migrations.push((object, conversion));
                }
            } else if ty.is_array() {
                // Find the innermost element type
                let mut element_ty = ty.clone();
                let mut depth = 0;
                while let Some(array) = element_ty.as_array() {
                    element_ty = array.element_type();
                    depth += 1;
                }

                if let Some(conversion) = mapping.struct_mappings.get(&element_ty) {
                    let mut new_ty = conversion.new_ty.clone();
                    for _ in 0..depth {
                        new_ty = new_ty.array_type();
                    }

                    // Only arrays containing structs need to be mapped, as an array of
                    // arrays merely contains `GcPtr`s.
                    let new_element_ty = new_ty.as_array().unwrap().element_type();
                    if new_element_ty.is_struct() {
                        // Conversion between ADTs are already handled in struct mappings
                        assert!(element_ty.is_struct());
                        let element_action =
                            resolve_struct_to_struct_edit(&element_ty, &new_element_ty, 0);
                        array_migrations.push((object, element_action, new_ty));
                    } else {
                        retyped_arrays.push((object, new_ty));
                    }
                }
            }
        }

        let mut ctx = MigrationContext {
            inner: &mut inner,
            conversions: &mapping.struct_mappings,
            migrations: FxHashMap::default(),
            new_objects: Vec::new(),
        };

        // Detach every struct payload first, so that mapping one object can
        // read the old payload of any other object regardless of order.
        let mut struct_payloads = Vec::with_capacity(struct_migrations.len());
        for (object, conversion) in &struct_migrations {
            let (src, dest) = ctx.begin(
                *object,
                conversion.new_ty.value_layout(),
                &conversion.new_ty,
            );
            struct_payloads.push((src, dest, *conversion));
        }
        for (src, dest, conversion) in struct_payloads {
            ctx.map_struct(&conversion.field_mapping, src, dest);
        }
        for (object, element_action, new_ty) in &array_migrations {
            ctx.migrate_array(*object, element_action, new_ty);
        }

        // Complete the migrations: switch types and release the old payloads
        let migrations = std::mem::take(&mut ctx.migrations);
        let new_objects = std::mem::take(&mut ctx.new_objects);
        let migrated: FxHashSet<usize> = migrations.keys().copied().collect();
        for (address, (migration, new_ty)) in migrations {
            inner.register_type(&new_ty);
            unsafe { (*(address as *mut ObjectHeader)).ty = new_ty.data_ptr().as_ptr() };
            inner.finish_payload_migration(migration);
        }
        for (object, new_ty) in retyped_arrays {
            // An array whose element type changed shape (e.g. `[[T]]` to
            // `[T]`) was migrated through its owner's field mapping; that
            // migration decides its type, not the element-type rename.
            if migrated.contains(&(object.as_ptr() as usize)) {
                continue;
            }
            inner.register_type(&new_ty);
            unsafe { (*object.as_ptr()).ty = new_ty.data_ptr().as_ptr() };
        }

        // Types no live object uses anymore no longer need to be kept alive
        inner.prune_types();
        drop(inner);
        self.heap.bump_type_epoch();

        for handle in new_objects {
            self.observer.event(Event::Allocation(handle));
        }

        deleted
    }
}
