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
}
