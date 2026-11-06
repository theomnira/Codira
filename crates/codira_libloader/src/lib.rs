//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{ffi::c_void, path::Path};

use codira_abi as abi;
pub use temp_library::TempLibrary;

mod temp_library;

/// An error that occurs upon construction of a [`CodiraLibrary`].
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error(transparent)]
    FailedToCreateTempLibrary(#[from] temp_library::InitError),
    #[error("Missing symbol for retrieving ABI version: {0}")]
    MissingGetAbiVersionFn(libloading::Error),
    #[error("Missing symbol for retrieving ABI information: {0}")]
    MissingGetInfoFn(libloading::Error),
    #[error("Missing symbol for setting allocator handle: {0}")]
    MissingSetAllocatorHandleFn(libloading::Error),
}

pub struct CodiraLibrary(TempLibrary);

impl CodiraLibrary {
    /// Loads a codiralib library from disk.
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
    /// See [`libloading::Library::new`] for more information.
    pub unsafe fn new(library_path: &Path) -> Result<Self, InitError> {
        // Although loading a library is technically unsafe, we assume here that this is
        // not the case for codiralibs.
        let library = TempLibrary::new(library_path)?;

        // Verify that the `*.codiralib` contains all required functions. Note that this
        // is an unsafe operation because the loaded symbols don't actually
        // contain type information. Casting is therefore unsafe.
        let _get_abi_version_fn: libloading::Symbol<'_, extern "C" fn() -> u32> = library
            .library()
            .get(abi::GET_VERSION_FN_NAME.as_bytes())
            .map_err(InitError::MissingGetAbiVersionFn)?;

        let _get_info_fn: libloading::Symbol<'_, extern "C" fn() -> abi::AssemblyInfo<'static>> =
            library
                .library()
                .get(abi::GET_INFO_FN_NAME.as_bytes())
                .map_err(InitError::MissingGetInfoFn)?;

        let _set_allocator_handle_fn: libloading::Symbol<'_, extern "C" fn(*mut c_void)> = library
            .library()
            .get(abi::SET_ALLOCATOR_HANDLE_FN_NAME.as_bytes())
            .map_err(InitError::MissingSetAllocatorHandleFn)?;

        Ok(CodiraLibrary(library))
    }

    pub fn into_inner(self) -> TempLibrary {
        self.0
    }

    /// Returns the ABI version of this codira library.
    ///
    /// # Safety
    ///
    /// This operations executes a function in the codiralib. There is no
    /// guarantee that the execution of the function wont result in
    /// undefined behavior.
    pub unsafe fn get_abi_version(&self) -> u32 {
        let get_abi_version_fn: libloading::Symbol<'_, extern "C" fn() -> u32> = self
            .0
            .library()
            .get(abi::GET_VERSION_FN_NAME.as_bytes())
            .unwrap();

        get_abi_version_fn()
    }

    /// Returns the assembly info exported by the shared object.
    ///
    /// # Safety
    ///
    /// This operations executes a function in the codiralib. There is no
    /// guarantee that the execution of the function wont result in
    /// undefined behavior.
    pub unsafe fn get_info(&self) -> abi::AssemblyInfo<'static> {
        let get_info_fn: libloading::Symbol<'_, extern "C" fn() -> abi::AssemblyInfo<'static>> =
            self.0
                .library()
                .get(abi::GET_INFO_FN_NAME.as_bytes())
                .unwrap();

        get_info_fn()
    }

    /// Stores the allocator handle inside the shared object. This is used by
    /// the internals of the library to be able to allocate memory.
    ///
    /// # Safety
    ///
    /// This operations executes a function in the codiralib. There is no
    /// guarantee that the execution of the function wont result in
    /// undefined behavior.
    pub unsafe fn set_allocator_handle(&mut self, allocator_ptr: *mut c_void) {
        let set_allocator_handle_fn: libloading::Symbol<'_, extern "C" fn(*mut c_void)> = self
            .0
            .library()
            .get(abi::SET_ALLOCATOR_HANDLE_FN_NAME.as_bytes())
            .unwrap();

        set_allocator_handle_fn(allocator_ptr);
    }

    /// Points a fuel-instrumented library's fuel counter at `counter`.
    /// Returns `false` (and does nothing) if the library was built without
    /// fuel instrumentation.
    ///
    /// # Safety
    ///
    /// This operations executes a function in the codiralib. `counter` must
    /// stay valid for as long as code in the library runs.
    pub unsafe fn set_fuel_counter(&mut self, counter: *mut i64) -> bool {
        match self
            .0
            .library()
            .get::<extern "C" fn(*mut i64)>(abi::SET_FUEL_COUNTER_FN_NAME.as_bytes())
        {
            Ok(set_fuel_counter_fn) => {
                set_fuel_counter_fn(counter);
                true
            }
            Err(_) => false,
        }
    }

    /// Calls the library's `set_gc_hooks` with the collector's hooks: the
    /// chain-head function, the poll word and the poll function. Returns
    /// `false` (and does nothing) if the library has no frame records.
    ///
    /// # Safety
    ///
    /// This operations executes a function in the codiralib. The pointers
    /// must stay valid for as long as code in the library runs.
    pub unsafe fn set_gc_hooks(
        &mut self,
        chain_fn: *const c_void,
        poll_word: *const c_void,
        poll_fn: *const c_void,
    ) -> bool {
        match self
            .0
            .library()
            .get::<extern "C" fn(*const c_void, *const c_void, *const c_void)>(
                abi::SET_GC_HOOKS_FN_NAME.as_bytes(),
            ) {
            Ok(set_gc_hooks_fn) => {
                set_gc_hooks_fn(chain_fn, poll_word, poll_fn);
                true
            }
            Err(_) => false,
        }
    }

    /// Calls a hardware-safety build's `set_hardware_safety` with `table`
    /// (the runtime's capability table) and returns the
    /// `HARDWARE_SAFETY_*` flags the library was built with; `0` for a
    /// library built without one.
    ///
    /// # Safety
    ///
    /// This operations executes a function in the codiralib. `table` must
    /// stay valid for as long as code in the library runs.
    pub unsafe fn set_hardware_safety(&mut self, table: *mut c_void) -> u32 {
        match self
            .0
            .library()
            .get::<extern "C" fn(*mut c_void) -> u32>(abi::SET_HARDWARE_SAFETY_FN_NAME.as_bytes())
        {
            Ok(set_hardware_safety_fn) => set_hardware_safety_fn(table),
            Err(_) => 0,
        }
    }
}
