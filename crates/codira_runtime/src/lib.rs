//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! The Codira Runtime
//!
//! The Codira Runtime provides functionality for automatically hot reloading
//! Codira C ABI compliant shared libraries.
#![warn(missing_docs)]

mod assembly;
#[macro_use]
mod garbage_collector;
mod adt;
mod array;
mod builtins;
mod dispatch_table;
mod fn_cells;
mod fuel;
mod function_info;
mod marshal;
pub mod memtag;
pub mod native_ai;
pub mod native_db;
pub mod native_fs;
mod native_handles;
pub mod native_hash;
pub mod native_memory;
mod reflection;
mod tasks;
mod utils;

use std::{
    cmp,
    collections::{BTreeMap, HashMap, VecDeque},
    ffi,
    ffi::c_void,
    fmt::{Debug, Display, Formatter},
    mem::ManuallyDrop,
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::{
        mpsc::{channel, Receiver},
        Arc,
    },
};

use assembly::LoadError;
use codira_abi as abi;
use codira_memory::{
    gc::{self, Array, GcRuntime},
    type_table::TypeTable,
};
// Re-export some useful types so crates dont have to depend on codira_memory as well.
pub use codira_memory::{Field, FieldData, HasStaticType, PointerType, StructType, Type};
use codira_project::LOCKFILE_NAME;
use dispatch_table::DispatchTable;
use garbage_collector::GarbageCollector;
use log::{debug, error, info, warn};
use notify::{
    event::{ModifyKind, RenameMode},
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};

pub use crate::{
    adt::{RootedStruct, StructRef},
    array::{ArrayRef, RawArray, RootedArray},
    assembly::{Assembly, LinkError, LinkFunctionsError},
    builtins::capture_output,
    fuel::FUEL_EXHAUSTED_EXCEPTION_CODE,
    function_info::{
        FunctionDefinition, FunctionPrototype, FunctionSignature, IntoFunctionDefinition,
    },
    marshal::Marshal,
    reflection::{ArgumentReflection, ReturnTypeReflection},
};

/// Options for the construction of a [`Runtime`].
pub struct RuntimeOptions {
    /// Path to the entry point library
    pub library_path: PathBuf,
    /// Custom type table used for the runtime
    pub type_table: TypeTable,
    /// Custom user injected functions
    pub user_functions: Vec<FunctionDefinition>,
    /// The most bytes the garbage-collected heap may hold from the system, or
    /// `None` for no limit. See [`RuntimeBuilder::heap_limit`].
    pub heap_limit: Option<usize>,
    /// The fuel budget of fuel-instrumented code, or `None` for no limit. See
    /// [`RuntimeBuilder::fuel`].
    pub fuel: Option<u64>,
}

/// Retrieve the allocator using the provided handle.
///
/// # Safety
///
/// The allocator must have been set using the `set_allocator_handle` call -
/// exposed by the Codira library.
unsafe fn get_allocator(alloc_handle: *mut ffi::c_void) -> Arc<GarbageCollector> {
    Arc::from_raw(alloc_handle as *const GarbageCollector)
}

/// Retrieve the `TypeInfo` using the provided handle.
///
/// # Safety
///
/// The type handle must have been returned from a call to
/// [`Arc<TypeInfo>::into_raw`][into_raw].
unsafe fn get_type_info(type_handle: *const ffi::c_void) -> Type {
    Type::from_raw(type_handle)
}

extern "C" fn new(
    type_handle: *const ffi::c_void,
    alloc_handle: *mut ffi::c_void,
) -> *const *mut ffi::c_void {
    // SAFETY: The runtime always constructs and uses `Arc<TypeInfo>::into_raw` to
    // set the type type handles in the type LUT.
    let type_info = ManuallyDrop::new(unsafe { get_type_info(type_handle) });

    // Safety: `new` is only called from within Codira assemblies' core logic, so we
    // are guaranteed that the `Runtime` and its `GarbageCollector` still exist
    // if this function is called, and will continue to do so for the duration
    // of this function.
    let allocator = ManuallyDrop::new(unsafe { get_allocator(alloc_handle) });

    // Safety: the Codira Compiler guarantees that `new` is never called with
    // `ptr::null()`.
    match allocator.as_ref().try_alloc(&type_info) {
        Ok(handle) => handle.into(),
        Err(err) => out_of_memory(&type_info, &err),
    }
}

/// Ends the process when generated code cannot get memory for an object.
///
/// Generated code has no way to observe a failed allocation (it would
/// dereference the null handle), and unwinding out of an `extern "C"`
/// function through Codira frames is not defined, so exhaustion of a capped
/// heap is a deterministic abort with a message that says why.
#[cold]
fn out_of_memory(ty: &Type, err: &gc::AllocError) -> ! {
    eprintln!(
        "codira runtime: cannot allocate `{}`: {err}. Aborting.",
        ty.name()
    );
    std::process::abort()
}

extern "C" fn new_array(
    type_handle: *const ffi::c_void,
    length: usize,
    alloc_handle: *mut ffi::c_void,
) -> *const *mut ffi::c_void {
    // SAFETY: The runtime always constructs and uses `Arc<TypeInfo>::into_raw` to
    // set the type type handles in the type LUT.
    let type_info = ManuallyDrop::new(unsafe { get_type_info(type_handle) });

    // Safety: `new` is only called from within Codira assemblies' core logic, so we
    // are guaranteed that the `Runtime` and its `GarbageCollector` still exist
    // if this function is called, and will continue to do so for the duration
    // of this function.
    let allocator = ManuallyDrop::new(unsafe { get_allocator(alloc_handle) });

    match allocator.as_ref().try_alloc_array(&type_info, length) {
        Ok(handle) => handle.as_raw().into(),
        Err(err) => out_of_memory(&type_info, &err),
    }
}

/// A builder for the [`Runtime`].
pub struct RuntimeBuilder {
    options: RuntimeOptions,
}

impl RuntimeBuilder {
    /// Constructs a new `RuntimeBuilder` for the shared library at
    /// `library_path`.
    fn new<P: Into<PathBuf>>(library_path: P) -> Self {
        Self {
            options: RuntimeOptions {
                library_path: library_path.into(),
                type_table: TypeTable::default(),
                user_functions: Vec::default(),
                heap_limit: None,
                fuel: None,
            },
        }
    }

    /// Bounds how much fuel-instrumented code (`codira build --fuel`) may
    /// run: every function entry and every loop iteration costs one unit.
    /// When the budget is used up the runtime reports it and ends the call
    /// (see [`Runtime::fuel_remaining`] and the `fuel` module docs for how a
    /// HERACLES guard observes it). Code built without instrumentation is
    /// not metered.
    pub fn fuel(mut self, units: u64) -> Self {
        self.options.fuel = Some(units);
        self
    }

    /// Caps the bytes the garbage-collected heap may hold from the system.
    ///
    /// When an allocation would exceed the cap, the collector runs first
    /// (treating the allocating thread's stack conservatively as roots, since
    /// generated code has no stack maps) and the allocation is retried. That
    /// emergency collection only happens while a single thread allocates in
    /// the runtime. If the memory still is not there, the process aborts with
    /// a message naming the type and the limit.
    ///
    /// This is a cap on the GC heap only, not a memory quota for the program:
    /// raw allocations (`codira_alloc`, region arenas, OS pages) and the
    /// runtime's own bookkeeping are not counted.
    pub fn heap_limit(mut self, bytes: usize) -> Self {
        self.options.heap_limit = Some(bytes);
        self
    }

    /// Adds a custom user function to the dispatch table.
    pub fn insert_fn<S: Into<String>, F: IntoFunctionDefinition>(
        mut self,
        name: S,
        func: F,
    ) -> Self {
        self.options.user_functions.push(func.into(name));
        self
    }

    /// Constructs a [`Runtime`] with the builder's options.
    ///
    /// # Safety
    ///
    /// A codiralib is simply a shared object. When a library is loaded,
    /// initialisation routines contained within it are executed. For the
    /// purposes of safety, the execution of these routines is conceptually
    /// the same calling an unknown foreign function and may impose
    /// arbitrary requirements on the caller for the call to be sound.
    ///
    /// Additionally, the callers of this function must also ensure that
    /// execution of the termination routines contained within the library
    /// is safe as well. These routines may be executed when the library is
    /// unloaded.
    ///
    /// See [`Assembly::load`] for more information.
    pub unsafe fn finish(self) -> Result<Runtime, InitError> {
        Runtime::new(self.options)
    }
}

/// An error that occurs upon construction of a [`Runtime`].
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// Failed to link assembly
    #[error(transparent)]
    LinkAssembly(#[from] LinkError),
    /// Failed to construct watcher
    #[error(transparent)]
    Watcher(#[from] notify::Error),
}

/// A runtime for the Codira language.
///
/// # Logging
///
/// The runtime uses [log] as a logging facade, but does not install a logger.
/// To produce log output, you have to use a [logger implementation][log-impl]
/// compatible with the facade.
///
/// [log]: https://docs.rs/log
/// [log-impl]: https://docs.rs/log/0.4.13/log/#available-logging-implementations
pub struct Runtime {
    assemblies: HashMap<PathBuf, Assembly>,
    /// Assemblies that have changed and thus need to be relinked. Maps the old
    /// to the (potentially) new path.
    assemblies_to_relink: BTreeMap<PathBuf, PathBuf>,
    dispatch_table: DispatchTable,
    type_table: TypeTable,
    watcher: RecommendedWatcher,
    watcher_rx: Receiver<notify::Result<Event>>,
    renamed_files: HashMap<usize, PathBuf>,
    gc: Arc<GarbageCollector>,
    fuel: fuel::FuelCounter,
    /// The code cells function values call through (see `fn_cells`).
    fn_cells: fn_cells::FnCells,
}

impl Runtime {
    /// Constructs a new [`RuntimeBuilder`] to construct a new [`Runtime`]
    /// instance.
    pub fn builder<P: Into<PathBuf>>(library_path: P) -> RuntimeBuilder {
        RuntimeBuilder::new(library_path)
    }

    /// Constructs a new `Runtime` that loads the library at `library_path` and
    /// its dependencies. The `Runtime` contains a file watcher that is
    /// triggered with an interval of `dur`.
    ///
    /// # Safety
    ///
    /// A codiralib is simply a shared object. When a library is loaded,
    /// initialisation routines contained within it are executed. For the
    /// purposes of safety, the execution of these routines is conceptually
    /// the same calling an unknown foreign function and may impose
    /// arbitrary requirements on the caller for the call to be sound.
    ///
    /// Additionally, the callers of this function must also ensure that
    /// execution of the termination routines contained within the library
    /// is safe as well. These routines may be executed when the library is
    /// unloaded.
    ///
    /// See [`Assembly::load`] for more information.
    pub unsafe fn new(mut options: RuntimeOptions) -> Result<Runtime, InitError> {
        let (tx, rx) = channel();

        let mut dispatch_table = DispatchTable::default();
        let type_table = options.type_table;

        // Add internal functions
        options.user_functions.push(IntoFunctionDefinition::into(
            new as extern "C" fn(*const ffi::c_void, *mut ffi::c_void) -> *const *mut ffi::c_void,
            "new",
        ));

        options.user_functions.push(IntoFunctionDefinition::into(
            new_array
                as extern "C" fn(
                    *const ffi::c_void,
                    usize,
                    *mut ffi::c_void,
                ) -> *const *mut ffi::c_void,
            "new_array",
        ));

        // The functions compiled `print`/`println` calls lower to.
        options.user_functions.extend(builtins::definitions());

        // Raw memory services for `std/memory/{region,page}.code`
        options.user_functions.extend(native_memory::functions());

        // File services for `std/fs/fs.code`, hashing for `std/crypto/hash.code`
        options.user_functions.extend(native_fs::functions());
        options.user_functions.extend(native_hash::functions());

        // The storage engine for `std/db/db.code`, tensors for
        // `std/ai/runtime.code`
        options.user_functions.extend(native_db::functions());
        options.user_functions.extend(native_ai::functions());

        // What fuel-instrumented code calls when its budget runs out
        options.user_functions.push(IntoFunctionDefinition::into(
            fuel::fuel_exhausted as extern "C" fn(),
            abi::FUEL_EXHAUSTED_FN_NAME,
        ));

        // `scope`, `spawn` and channels.
        options.user_functions.extend(tasks::functions());

        options.user_functions.into_iter().for_each(|fn_def| {
            dispatch_table.insert_fn(fn_def.prototype.name.clone(), Arc::new(fn_def));
        });

        let watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
            // This runs on the watcher's own thread, which can still be
            // delivering an event while the `Runtime` -- and with it the
            // receiving end -- is being dropped. This used to `expect` the send,
            // which turned an ordinary shutdown into a panic on that thread.
            // With no runtime left there is nobody to reload for, so the event
            // is simply not needed.
            if tx.send(res).is_err() {
                debug!("Discarding a filesystem event, the runtime no longer exists.");
            }
        })?;
        let mut runtime = Runtime {
            assemblies: HashMap::new(),
            assemblies_to_relink: BTreeMap::new(),
            dispatch_table,
            type_table,
            watcher,
            watcher_rx: rx,
            renamed_files: HashMap::new(),
            gc: Arc::new(self::garbage_collector::GarbageCollector::default()),
            fuel: fuel::FuelCounter::new(options.fuel),
            fn_cells: fn_cells::FnCells::default(),
        };
        runtime.gc.set_heap_limit(options.heap_limit);

        runtime.add_assembly(&options.library_path)?;
        Ok(runtime)
    }

    /// Adds an assembly corresponding to the library at `library_path`.
    ///
    /// # Safety
    ///
    /// A codiralib is simply a shared object. When a library is loaded,
    /// initialisation routines contained within it are executed. For the
    /// purposes of safety, the execution of these routines is conceptually
    /// the same calling an unknown foreign function and may impose
    /// arbitrary requirements on the caller for the call to be sound.
    ///
    /// Additionally, the callers of this function must also ensure that
    /// execution of the termination routines contained within the library
    /// is safe as well. These routines may be executed when the library is
    /// unloaded.
    ///
    /// See [`Assembly::load`] for more information.
    unsafe fn add_assembly(&mut self, library_path: &Path) -> Result<(), LinkError> {
        let library_path = library_path
            .canonicalize()
            .map_err(|e| LinkError::LoadAssembly(LoadError::Other(e)))?;

        if self.assemblies.contains_key(&library_path) {
            return Err(LoadError::AlreadyExists.into());
        }

        let mut loaded = HashMap::new();
        let mut to_load = VecDeque::new();
        to_load.push_back(library_path);

        // Load all assemblies and their dependencies
        while let Some(library_path) = to_load.pop_front() {
            // A dependency can be added by multiple dependants, so check that we didn't
            // load it yet
            if loaded.contains_key(&library_path) {
                continue;
            }

            let assembly = Assembly::load(&library_path, self.gc.clone(), self.fuel.as_ptr())?;

            let parent = library_path.parent().expect("Invalid library path");
            let extension = library_path.extension();

            let dependencies: Vec<String> =
                assembly.info().dependencies().map(From::from).collect();
            loaded.insert(library_path.clone(), assembly);

            for dependency in dependencies {
                let mut library_path = parent.join(dependency);
                if let Some(extension) = extension {
                    library_path = library_path.with_extension(extension);
                }

                if !loaded.contains_key(&library_path) {
                    to_load.push_back(library_path);
                }
            }
        }

        (self.dispatch_table, self.type_table) = Assembly::link_all(
            loaded.values_mut(),
            &self.dispatch_table,
            &self.type_table,
            &mut self.fn_cells,
        )?;

        for (library_path, assembly) in loaded {
            self.watcher
                .watch(library_path.parent().unwrap(), RecursiveMode::NonRecursive)
                .expect("Path must exist as we just loaded the library");

            self.assemblies.insert(library_path, assembly);
        }
        self.fn_cells.refresh(self.assemblies.values());

        Ok(())
    }

    /// Retrieves the function definition corresponding to `function_name`, if
    /// available.
    pub fn get_function_definition(&self, function_name: &str) -> Option<Arc<FunctionDefinition>> {
        // TODO: Verify that when someone tries to invoke a non-public function, it
        // should fail.
        self.dispatch_table.get_fn(function_name)
    }

    /// For a given `fn_name`, find the most similar name in `fn_names`
    fn find_best_match_for_fn_name<'a>(
        fn_name: &'a str,
        fn_names: impl Iterator<Item = &'a str>,
        dist: Option<usize>,
    ) -> Option<&'a str> {
        // As a loose rule to avoid the obviously incorrect suggestions, it takes
        // an optional limit for the maximum allowable edit distance, which defaults
        // to one-third of the given word.
        let max_dist = dist.unwrap_or_else(|| cmp::max(fn_name.len(), 3) / 3);

        let found_match = fn_names
            .filter_map(|name| {
                let dist = utils::lev_distance(fn_name, name);
                if dist <= max_dist {
                    Some((name, dist))
                } else {
                    None
                }
            })
            // Ties are broken by name: the dispatch table is a hash map, so
            // without this the suggestion depended on its iteration order.
            .min_by(|(name1, dist1), (name2, dist2)| {
                dist1.cmp(dist2).then_with(|| name1.cmp(name2))
            });
        found_match.map(|(closest_name, _)| closest_name)
    }

    /// Retrieves the type definition corresponding to `type_name`, if
    /// available.
    pub fn get_type_info_by_name(&self, type_name: &str) -> Option<Type> {
        self.type_table.find_type_info_by_name(type_name)
    }

    /// Retrieve the type information corresponding to the `type_id`, if
    /// available.
    pub fn get_type_info_by_id(&self, type_id: &abi::TypeId<'_>) -> Option<Type> {
        self.type_table.find_type_info_by_id(type_id)
    }

    /// Updates the state of the runtime. This includes checking for file
    /// changes, and reloading compiled assemblies.
    /// # Safety
    ///
    /// A codiralib is simply a shared object. When a library is loaded,
    /// initialisation routines contained within it are executed. For the
    /// purposes of safety, the execution of these routines is conceptually
    /// the same calling an unknown foreign function and may impose
    /// arbitrary requirements on the caller for the call to be sound.
    ///
    /// Additionally, the callers of this function must also ensure that
    /// execution of the termination routines contained within the library
    /// is safe as well. These routines may be executed when the library is
    /// unloaded.
    ///
    /// See [`Assembly::load`] for more information.
    pub unsafe fn update(&mut self) -> bool {
        unsafe fn relink_assemblies(
            runtime: &mut Runtime,
        ) -> Result<(DispatchTable, TypeTable), LinkError> {
            let mut loaded = HashMap::new();
            let to_load = &mut runtime.assemblies_to_relink;

            info!("Relinking assemblies:");
            for (old_path, new_path) in to_load.iter() {
                info!(
                    "{} -> {}",
                    old_path.to_string_lossy(),
                    new_path.to_string_lossy()
                );
            }

            // Load all assemblies and their dependencies
            while let Some((old_path, new_path)) = to_load.pop_first() {
                // A dependency can be added by multiple dependants, so check that we didn't
                // load it yet
                if loaded.contains_key(&old_path) {
                    continue;
                }

                let assembly =
                    Assembly::load(&new_path, runtime.gc.clone(), runtime.fuel.as_ptr())?;

                let parent = new_path.parent().expect("Invalid library path");
                let extension = new_path.extension();

                let dependencies: Vec<String> =
                    assembly.info().dependencies().map(From::from).collect();
                loaded.insert(old_path.clone(), assembly);

                for dependency in dependencies {
                    let mut library_path = parent.join(dependency);
                    if let Some(extension) = extension {
                        library_path = library_path.with_extension(extension);
                    }

                    if !loaded.contains_key(&library_path)
                        && !runtime.assemblies.contains_key(&library_path)
                    {
                        to_load.insert(old_path.clone(), library_path);
                    }
                }
            }

            Assembly::relink_all(
                &mut loaded,
                &mut runtime.assemblies,
                &runtime.dispatch_table,
                &runtime.type_table,
                &mut runtime.fn_cells,
            )
        }

        let requires_relink = drain_watcher_events(
            &self.watcher_rx,
            &mut self.renamed_files,
            &mut self.assemblies_to_relink,
        );

        if requires_relink {
            if self.assemblies_to_relink.is_empty() {
                debug!("The compiler didn't write a codiralib.");
            } else {
                match relink_assemblies(self) {
                    Ok((dispatch_table, type_table)) => {
                        info!("Succesfully reloaded assemblies.");

                        self.dispatch_table = dispatch_table;
                        self.type_table = type_table;
                        self.assemblies_to_relink.clear();
                        // Function values created before the reload now
                        // call the reloaded code.
                        self.fn_cells.refresh(self.assemblies.values());

                        return true;
                    }
                    Err(e) => error!("Failed to relink assemblies: {e}"),
                }
            }
        }

        false
    }

    /// Returns a shared reference to the runtime's garbage collector.
    ///
    /// We cannot return an `Arc` here, because the lifetime of data contained
    /// in `GarbageCollector` is dependent on the `Runtime`.
    pub fn gc(&self) -> &GarbageCollector {
        self.gc.as_ref()
    }

    /// Collects all memory that is no longer referenced by rooted objects.
    /// Returns `true` if memory was reclaimed, `false` otherwise. This
    /// behavior will likely change in the future.
    pub fn gc_collect(&self) -> bool {
        self.gc.collect()
    }

    /// Returns the fuel left for fuel-instrumented code, or `None` if it is
    /// unlimited. Each function entry and loop iteration of instrumented code
    /// costs one unit; with several threads running instrumented code the
    /// count is approximate.
    pub fn fuel_remaining(&self) -> Option<u64> {
        self.fuel.remaining()
    }

    /// Returns true if any loaded assembly was built with fuel
    /// instrumentation, i.e. if [`Runtime::fuel_remaining`] meters anything.
    pub fn is_fuel_instrumented(&self) -> bool {
        self.assemblies.values().any(Assembly::is_fuel_instrumented)
    }

    /// Sets the fuel left for fuel-instrumented code (`None`: unlimited).
    pub fn set_fuel(&self, fuel: Option<u64>) {
        self.fuel.set(fuel);
    }

    /// Writes every object of the GC heap -- types, contents, references and
    /// root counts -- to `path`, so [`Runtime::restore_heap`] can rebuild it
    /// in a runtime that loaded the same assembly, in this process or
    /// another. Returns the number of objects written.
    ///
    /// Like a collection, this must not run while another thread executes
    /// Codira code. Objects whose types hold raw pointers cannot be saved.
    pub fn save_heap(&self, path: impl AsRef<Path>) -> Result<usize, gc::SnapshotError> {
        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
        self.gc.write_snapshot(&mut file)
    }

    /// Rebuilds the objects of a snapshot written by [`Runtime::save_heap`]
    /// in this runtime's heap, resolving types by GUID through the loaded
    /// assemblies (a type whose layout changed is refused). Returns a
    /// [`RootedStruct`] for every struct that was rooted when the snapshot
    /// was taken, in snapshot order; that is how the host gets its state
    /// back. Rooted arrays come back rooted with their original root counts.
    pub fn restore_heap(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Vec<RootedStruct>, gc::SnapshotError> {
        let mut file = std::io::BufReader::new(std::fs::File::open(path)?);
        let restored = self.gc.read_snapshot(&mut file, |guid| {
            self.type_table
                .find_type_info_by_id(&abi::TypeId::Concrete(*guid))
        })?;
        let mut roots = Vec::new();
        for (handle, count) in restored.roots {
            if self.gc.ptr_type(handle).is_struct() {
                // Hand the root over to a `RootedStruct`.
                roots.push(RootedStruct::new(&self.gc, adt::RawStruct(handle)));
                for _ in 0..count {
                    self.gc.unroot(handle);
                }
            }
        }
        Ok(roots)
    }

    /// Returns statistics about the garbage collector.
    pub fn gc_stats(&self) -> gc::Stats {
        self.gc.stats()
    }

    /// Constructs an array with a predefined element type.
    pub fn construct_typed_array<
        't,
        T: 't + Marshal<'t> + ArgumentReflection,
        I: IntoIterator<Item = T>,
    >(
        &'t self,
        element_type: &Type,
        iter: I,
    ) -> ArrayRef<'t, T>
    where
        I::IntoIter: ExactSizeIterator,
    {
        let iter = iter.into_iter();
        let array_type = element_type.array_type();
        let array_capacity = iter
            .size_hint()
            .1
            .expect("iterator doesn't return upper bound");
        let mut array_handle = self.gc.alloc_array(&array_type, array_capacity);

        let mut element_ptr = array_handle.data().as_ptr();
        let element_stride = array_handle.element_stride();
        let mut size = 0;
        for (idx, element) in iter.enumerate() {
            debug_assert!(
                idx < array_capacity,
                "looks like the size_hint was not properly implemented"
            );
            assert_eq!(&element.type_info(self), element_type);

            // Safety: the element_ptr came from NonNull and is only moved forward.
            T::marshal_to_ptr(
                element,
                unsafe { NonNull::new_unchecked(element_ptr).cast() },
                element_type,
            );

            // Safety: we trust the element stride
            element_ptr = unsafe { element_ptr.add(element_stride) };

            // Count the number of elements written
            size = idx + 1;
        }

        // Safety: we just initialized all the elements, so this operation is safe.
        unsafe {
            array_handle.set_length(size);
        }

        ArrayRef::new(RawArray(array_handle.as_raw()), self)
    }

    /// Constructs an array from an iterator
    pub fn construct_array<'t, T: 't + Marshal<'t> + HasStaticType, I: IntoIterator<Item = T>>(
        &'t self,
        iter: I,
    ) -> ArrayRef<'t, T>
    where
        I::IntoIter: ExactSizeIterator,
    {
        let iter = iter.into_iter();
        let element_type = T::type_info();
        let array_type = element_type.array_type();
        let array_capacity = iter
            .size_hint()
            .1
            .expect("iterator doesn't return upper bound");
        let mut array_handle = self.gc.alloc_array(&array_type, array_capacity);

        let mut element_ptr = array_handle.data().as_ptr();
        let element_stride = array_handle.element_stride();
        let mut size = 0;
        for (idx, element) in iter.enumerate() {
            debug_assert!(
                idx < array_capacity,
                "looks like the size_hint was not properly implemented"
            );

            // Safety: the element_ptr came from NonNull and is only moved forward.
            T::marshal_to_ptr(
                element,
                unsafe { NonNull::new_unchecked(element_ptr).cast() },
                element_type,
            );

            // Safety: we trust the element stride
            element_ptr = unsafe { element_ptr.add(element_stride) };

            // Count the number of elements written
            size = idx + 1;
        }

        // Safety: we just initialized all the elements, so this operation is safe.
        unsafe {
            array_handle.set_length(size);
        }

        ArrayRef::new(RawArray(array_handle.as_raw()), self)
    }
}

/// Returns whether `path` is the lockfile the compiler holds while it writes
/// its assemblies.
///
/// A path without a final component (a root, or one ending in `..`) is not a
/// lockfile. This used to `expect` a file name, so such a path in an event
/// would have panicked.
fn is_lockfile(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == LOCKFILE_NAME)
}

/// Processes every filesystem event that is queued on `watcher_rx`, recording
/// changed assemblies in `assemblies_to_relink`. Returns `true` if the compiler
/// released its lockfile, which is what triggers a relink.
///
/// This is a free function over the parts of the [`Runtime`] it needs, so that
/// it can be tested with hand-made events instead of real filesystem activity.
///
/// Nothing a filesystem can report is allowed to panic here or to hold back
/// another event:
///
/// * The queue was drained with `while let Ok(Ok(event))`, which also stopped
///   at the first *error* the watcher reported. The error itself was discarded
///   without a trace and everything queued behind it was left for a later call,
///   so a lockfile removal behind an error did not trigger its relink in the
///   `update` that should have seen it.
/// * A path was canonicalized with a panic as its failure mode, but an event
///   only says that a file existed when the event was raised. The compiler's
///   output directory has short-lived files in it, and one that is gone by the
///   time its event is handled took the whole host down.
/// * A rename was `expect`ed to carry a tracker, which not every event does.
fn drain_watcher_events(
    watcher_rx: &Receiver<notify::Result<Event>>,
    renamed_files: &mut HashMap<usize, PathBuf>,
    assemblies_to_relink: &mut BTreeMap<PathBuf, PathBuf>,
) -> bool {
    let mut requires_relink = false;

    // `try_recv` fails once the queue is empty. It also fails if the sender is
    // gone, but that is owned by the watcher, which lives as long as the
    // receiver does; either way there is nothing left to process.
    while let Ok(event) = watcher_rx.try_recv() {
        let event = match event {
            Ok(event) => event,
            Err(e) => {
                error!("The filesystem watcher reported an error: {e}");
                continue;
            }
        };

        for path in event.paths {
            // The compiler writes a library to a temporary file and renames
            // it onto its path; the temporary file is never an assembly.
            if path
                .extension()
                .is_some_and(|extension| extension == abi::OUTPUT_TEMP_EXTENSION)
            {
                continue;
            }
            if is_lockfile(&path) {
                match event.kind {
                    EventKind::Create(_) => debug!("Lockfile created"),
                    EventKind::Remove(_) => {
                        debug!("Lockfile deleted");

                        requires_relink = true;
                    }
                    _ => (),
                }
            } else {
                let path = match path.canonicalize() {
                    Ok(path) => path,
                    Err(e) => {
                        // The file no longer exists, so there is nothing that
                        // could be loaded from it either.
                        debug!(
                            "Ignoring a filesystem event for {}: {e}",
                            path.to_string_lossy()
                        );
                        continue;
                    }
                };

                match event.kind {
                    EventKind::Modify(ModifyKind::Name(mode)) => {
                        let tracker = event.attrs.tracker();
                        let source = tracker.and_then(|tracker| renamed_files.remove(&tracker));
                        match (source, mode, tracker) {
                            (Some(old_path), _, _) => {
                                assemblies_to_relink.insert(old_path, path);
                                // on_file_changed(self, &old_path, &path);
                            }
                            // Something was renamed onto this path from a
                            // file that is not tracked: the compiler writes
                            // a library to a temporary file and renames it
                            // over the old one, and the temporary file is
                            // gone by the time its event is handled. The
                            // file at this path has new contents. Platforms
                            // that report no tracker (Windows) always end up
                            // here.
                            (None, RenameMode::To, _) => {
                                assemblies_to_relink.insert(path.clone(), path);
                            }
                            (None, _, Some(tracker)) => {
                                renamed_files.insert(tracker, path);
                            }
                            // Without a tracker the two halves of a rename
                            // cannot be matched up.
                            (None, _, None) => {
                                warn!(
                                    "Ignoring a rename event without a tracker for {}",
                                    path.to_string_lossy()
                                );
                            }
                        }
                    }
                    EventKind::Modify(_) => {
                        // TODO: don't overwrite existing
                        assemblies_to_relink.insert(path.clone(), path);
                    }
                    _ => (),
                }
            }
        }
    }

    requires_relink
}

/// An error that might occur when calling a codira function from Rust.
pub struct InvokeErr<'name, T> {
    msg: String,
    function_name: &'name str,
    arguments: T,
}

impl<T> Debug for InvokeErr<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl<T> Display for InvokeErr<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl<T: InvokeArgs> InvokeErr<'_, T> {
    /// Retries a function invocation once, resulting in a potentially
    /// successful invocation.
    // FIXME: `unwrap_or_else` does not compile for `StructRef`, due to
    // https://doc.rust-lang.org/nomicon/lifetime-mismatch.html#improperly-reduced-borrows
    pub fn retry<'r, 'o, Output>(self, runtime: &'r mut Runtime) -> Result<Output, Self>
    where
        Output: 'o + ReturnTypeReflection + Marshal<'o>,
        'r: 'o,
    {
        // Safety: The output of `retry_impl` is guaranteed to only contain a shared
        // reference.
        unsafe { self.retry_impl(runtime) }
    }

    /// Retries the function invocation until it succeeds, resulting in an
    /// output.
    // FIXME: `unwrap_or_else` does not compile for `StructRef`, due to
    // https://doc.rust-lang.org/nomicon/lifetime-mismatch.html#improperly-reduced-borrows
    pub fn wait<'r, 'o, Output>(mut self, runtime: &'r mut Runtime) -> Output
    where
        Output: 'o + ReturnTypeReflection + Marshal<'o>,
        'r: 'o,
    {
        loop {
            self = match unsafe { self.retry_impl(runtime) } {
                Ok(output) => return output,
                Err(e) => e,
            };
        }
    }

    /// Inner implementation that retries a function invocation once, resulting
    /// in a potentially successful invocation. This is a workaround for:
    /// <https://doc.rust-lang.org/nomicon/lifetime-mismatch.html>
    ///
    /// # Safety
    ///
    /// When calling this function, you have to guarantee that `runtime` can be
    /// dereferenced and is valid for `'o`. The `Output` value can only
    /// contain a shared borrow of `runtime`.
    unsafe fn retry_impl<'o, Output>(self, runtime: *mut Runtime) -> Result<Output, Self>
    where
        Output: 'o + ReturnTypeReflection + Marshal<'o>,
    {
        // Safety: Guaranteed by the caller to be valid to dereference.
        let runtime = &mut *runtime;

        eprintln!("{}", self.msg);
        while !runtime.update() {
            // Wait until there has been an update that might fix the error
        }

        runtime.invoke(self.function_name, self.arguments)
    }
}

/// A trait that handles calling a certain function with a set of arguments.
/// This trait is implemented for tuples up to and including 20 elements.
pub trait InvokeArgs {
    /// Determines whether the specified function can be called with these
    /// arguments
    fn can_invoke(&self, runtime: &Runtime, signature: &FunctionSignature) -> Result<(), String>;

    /// Calls the specified function with these function arguments
    ///
    /// # Safety
    ///
    /// The `fn_ptr` is cast and invoked which might result in undefined
    /// behavior.
    unsafe fn invoke<ReturnType>(self, fn_ptr: *const c_void) -> ReturnType;
}

// Implement `InvokeTraits` for tuples up to and including 20 elements
seq_macro::seq!(N in 0..=20 {#(
seq_macro::seq!(I in 0..N {
    #[allow(clippy::extra_unused_lifetimes)]
    impl<'arg, #(T~I: ArgumentReflection + Marshal<'arg>,)*> InvokeArgs for (#(T~I,)*) {
        #[allow(unused_variables)]
        fn can_invoke(&self, runtime: &Runtime, signature: &FunctionSignature) -> Result<(), String> {
            let arg_types = &signature.arg_types;

            // Ensure the number of arguments match
            #[allow(clippy::len_zero)]
            if N != arg_types.len() {
                return Err(format!("Invalid argument count. Expected {} arguments, got {}", arg_types.len(), N))
            }

            #(
            if arg_types[I] != self.I.type_info(runtime) {
                return Err(format!(
                    "Invalid argument type at index {}. Expected: {}. Found: {}.",
                    I,
                    self.I.type_info(runtime).name(),
                    arg_types[I].name(),
                ));
            }
            )*

            Ok(())
        }

        unsafe fn invoke<ReturnType>(self, fn_ptr: *const c_void) -> ReturnType {
            #[allow(clippy::type_complexity)]
            let function: fn(#(T~I::CodiraType,)*) -> ReturnType = core::mem::transmute(fn_ptr);
            function(#(self.I.marshal_into(),)*)
        }
    }
});
)*});

impl Runtime {
    /// Invokes the Codira function called `function_name` with the specified
    /// `arguments`.
    pub fn invoke<
        'runtime,
        'ret,
        'name,
        ReturnType: ReturnTypeReflection + Marshal<'ret> + 'ret,
        ArgTypes: InvokeArgs,
    >(
        &'runtime self,
        function_name: &'name str,
        arguments: ArgTypes,
    ) -> Result<ReturnType, InvokeErr<'name, ArgTypes>>
    where
        'runtime: 'ret,
    {
        // Get the function information from the runtime
        let function_info = match self.get_function_definition(function_name).ok_or_else(|| {
            format!("failed to obtain function '{function_name}', no such function exists.")
        }) {
            Ok(function_info) => function_info,
            Err(msg) => {
                // Runtime-provided functions (`codira.print_str`, ...) are
                // not something a caller could have meant to invoke.
                let available_names = self
                    .dispatch_table
                    .get_fn_names()
                    .filter(|name| !name.contains('.'));
                let suggested_name =
                    Self::find_best_match_for_fn_name(function_name, available_names, None);

                let suggested_message = suggested_name.map_or_else(
                    || msg.clone(),
                    |name| format!("{msg} There is a function with a similar name: {name}"),
                );

                return Err(InvokeErr {
                    msg: suggested_message,
                    function_name,
                    arguments,
                });
            }
        };

        // Validate the arguments
        match arguments.can_invoke(self, &function_info.prototype.signature) {
            Ok(_) => {}
            Err(msg) => {
                return Err(InvokeErr {
                    msg,
                    function_name,
                    arguments,
                })
            }
        };

        // Validate the return type
        if !ReturnType::accepts_type(&function_info.prototype.signature.return_type) {
            return Err(InvokeErr {
                msg: format!(
                    "unexpected return type, got '{}', expected '{}",
                    function_info.prototype.signature.return_type.name(),
                    ReturnType::type_hint()
                ),
                function_name,
                arguments,
            });
        }

        let result: ReturnType::CodiraType = unsafe { arguments.invoke(function_info.fn_ptr) };
        Ok(Marshal::marshal_from(result, self))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        fs,
        path::{Path, PathBuf},
        sync::mpsc::channel,
    };

    use codira_project::LOCKFILE_NAME;
    use notify::{
        event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode},
        Event, EventKind,
    };

    use crate::drain_watcher_events;

    /// What `drain_watcher_events` leaves behind for a queue of events.
    struct Drained {
        requires_relink: bool,
        renamed_files: HashMap<usize, PathBuf>,
        assemblies_to_relink: BTreeMap<PathBuf, PathBuf>,
        /// Whether every queued event was taken off the queue.
        queue_is_empty: bool,
    }

    fn drain(events: Vec<notify::Result<Event>>) -> Drained {
        let (tx, rx) = channel();
        for event in events {
            tx.send(event).unwrap();
        }

        let mut renamed_files = HashMap::new();
        let mut assemblies_to_relink = BTreeMap::new();
        let requires_relink =
            drain_watcher_events(&rx, &mut renamed_files, &mut assemblies_to_relink);

        Drained {
            requires_relink,
            renamed_files,
            assemblies_to_relink,
            queue_is_empty: rx.try_recv().is_err(),
        }
    }

    fn modified(path: &Path) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(path.into())
    }

    fn renamed(path: &Path, mode: RenameMode) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Name(mode))).add_path(path.into())
    }

    fn lockfile_removed(dir: &Path) -> Event {
        Event::new(EventKind::Remove(RemoveKind::File)).add_path(dir.join(LOCKFILE_NAME))
    }

    /// Creates a file in `dir`. Returns the path an event would report for it
    /// and the canonical path the runtime tracks it by.
    fn existing_file(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
        let path = dir.join(name);
        fs::write(&path, []).unwrap();
        let canonical = path.canonicalize().unwrap();
        (path, canonical)
    }

    #[test]
    fn modified_assembly_and_released_lockfile_require_relink() {
        let dir = tempfile::tempdir().unwrap();
        let (path, canonical) = existing_file(dir.path(), "mod.codiralib");

        let drained = drain(vec![Ok(modified(&path)), Ok(lockfile_removed(dir.path()))]);

        assert!(drained.requires_relink);
        assert!(drained.queue_is_empty);
        assert_eq!(
            drained.assemblies_to_relink,
            BTreeMap::from([(canonical.clone(), canonical)])
        );
    }

    #[test]
    fn lockfile_creation_does_not_require_relink() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile = dir.path().join(LOCKFILE_NAME);

        let drained = drain(vec![Ok(
            Event::new(EventKind::Create(CreateKind::File)).add_path(lockfile)
        )]);

        assert!(!drained.requires_relink);
        assert!(drained.queue_is_empty);
        assert!(drained.assemblies_to_relink.is_empty());
    }

    /// An error used to end the drain: the lockfile removal behind it stayed on
    /// the queue, and the `update` that should have relinked did not.
    #[test]
    fn error_event_does_not_stop_draining() {
        let dir = tempfile::tempdir().unwrap();
        let (path, canonical) = existing_file(dir.path(), "mod.codiralib");

        let drained = drain(vec![
            Err(notify::Error::generic("first")),
            Ok(modified(&path)),
            Err(notify::Error::generic("second")),
            Ok(lockfile_removed(dir.path())),
            Err(notify::Error::generic("third")),
        ]);

        assert!(drained.requires_relink);
        assert!(drained.queue_is_empty);
        assert_eq!(
            drained.assemblies_to_relink,
            BTreeMap::from([(canonical.clone(), canonical)])
        );
    }

    /// Canonicalizing the path of a file that is already gone used to panic.
    #[test]
    fn vanished_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let vanished = dir.path().join("vanished.codiralib");
        let (path, canonical) = existing_file(dir.path(), "mod.codiralib");

        let drained = drain(vec![
            Ok(modified(&vanished)),
            // One event can name several paths; one of them being gone must
            // not cost the others.
            Ok(modified(&vanished).add_path(path)),
            Ok(renamed(&vanished, RenameMode::From).set_tracker(1)),
            Ok(lockfile_removed(dir.path())),
        ]);

        assert!(drained.requires_relink);
        assert!(drained.queue_is_empty);
        assert!(drained.renamed_files.is_empty());
        assert_eq!(
            drained.assemblies_to_relink,
            BTreeMap::from([(canonical.clone(), canonical)])
        );
    }

    /// A rename event without a tracker used to panic. Its destination is
    /// still a file that changed, and a source that is not a file of ours
    /// cannot be told apart from the compiler's temporary file.
    #[test]
    fn rename_onto_a_path_without_tracker_is_a_change_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let (untracked, untracked_canonical) = existing_file(dir.path(), "untracked.codiralib");
        let (path, canonical) = existing_file(dir.path(), "mod.codiralib");

        let drained = drain(vec![
            Ok(renamed(&untracked, RenameMode::To)),
            Ok(renamed(&path, RenameMode::From)),
            Ok(modified(&path)),
        ]);

        assert!(!drained.requires_relink);
        assert!(drained.queue_is_empty);
        assert!(drained.renamed_files.is_empty());
        assert_eq!(
            drained.assemblies_to_relink,
            BTreeMap::from([
                (untracked_canonical.clone(), untracked_canonical),
                (canonical.clone(), canonical)
            ])
        );
    }

    /// Two rename events that share a tracker are the two halves of one
    /// rename, and that pairing is not what the fixes above were about.
    #[test]
    fn tracked_rename_maps_old_path_to_new_path() {
        let dir = tempfile::tempdir().unwrap();
        let (old, old_canonical) = existing_file(dir.path(), "old.codiralib");
        let (new, new_canonical) = existing_file(dir.path(), "new.codiralib");

        let drained = drain(vec![
            Ok(renamed(&old, RenameMode::From).set_tracker(7)),
            Ok(renamed(&new, RenameMode::To).set_tracker(7)),
        ]);

        assert!(drained.queue_is_empty);
        assert!(drained.renamed_files.is_empty());
        assert_eq!(
            drained.assemblies_to_relink,
            BTreeMap::from([(old_canonical, new_canonical)])
        );
    }

    /// The compiler writes a library to a temporary file and renames it onto
    /// its path. The first half of that rename names a file that is already
    /// gone; the second half is the library changing.
    #[test]
    fn rename_onto_an_assembly_is_a_change_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join(format!(
            ".mod.codiralib.1.{}",
            codira_abi::OUTPUT_TEMP_EXTENSION
        ));
        let (new, canonical) = existing_file(dir.path(), "mod.codiralib");

        let drained = drain(vec![
            Ok(modified(&temp)),
            Ok(renamed(&temp, RenameMode::From).set_tracker(3)),
            Ok(renamed(&new, RenameMode::To).set_tracker(3)),
            Ok(lockfile_removed(dir.path())),
        ]);

        assert!(drained.requires_relink);
        assert!(drained.queue_is_empty);
        assert!(drained.renamed_files.is_empty());
        assert_eq!(
            drained.assemblies_to_relink,
            BTreeMap::from([(canonical.clone(), canonical)])
        );
    }

    /// The same rename as the platform reports it on Windows: no trackers.
    #[test]
    fn rename_onto_an_assembly_without_trackers_is_a_change_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join(format!(
            ".mod.codiralib.1.{}",
            codira_abi::OUTPUT_TEMP_EXTENSION
        ));
        let (new, canonical) = existing_file(dir.path(), "mod.codiralib");

        let drained = drain(vec![
            Ok(renamed(&temp, RenameMode::From)),
            Ok(renamed(&new, RenameMode::To)),
            Ok(lockfile_removed(dir.path())),
        ]);

        assert!(drained.requires_relink);
        assert!(drained.queue_is_empty);
        assert_eq!(
            drained.assemblies_to_relink,
            BTreeMap::from([(canonical.clone(), canonical)])
        );
    }

    /// While the compiler is still writing its temporary file, events about
    /// the file itself must not make the runtime try to load it.
    #[test]
    fn compiler_temporary_files_are_never_assemblies() {
        let dir = tempfile::tempdir().unwrap();
        let (temp, _) = existing_file(
            dir.path(),
            &format!(".mod.codiralib.1.{}", codira_abi::OUTPUT_TEMP_EXTENSION),
        );

        let drained = drain(vec![Ok(modified(&temp)), Ok(lockfile_removed(dir.path()))]);

        assert!(drained.requires_relink);
        assert!(drained.assemblies_to_relink.is_empty());
    }

    /// A path without a final component used to panic when it was compared to
    /// the lockfile's name.
    #[test]
    fn path_without_file_name_is_not_a_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("..");

        let drained = drain(vec![
            Ok(Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(parent)),
            Ok(lockfile_removed(dir.path())),
        ]);

        assert!(drained.requires_relink);
        assert!(drained.queue_is_empty);
        assert!(drained.assemblies_to_relink.is_empty());
    }
}
