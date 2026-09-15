//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
//!
//! Defines an FFI compatible type interface for type information.
#![allow(dead_code)]

use std::{
    ffi::{c_void, CString},
    mem::ManuallyDrop,
    ops::Deref,
    os::raw::c_char,
    ptr,
    ptr::NonNull,
    sync::{atomic::Ordering, Arc},
};

use codira_abi::Guid;
use codira_capi_utils::{codira_error_try, try_deref_mut, ErrorHandle};
pub use r#array::ArrayInfo;
pub use r#pointer::PointerInfo;
pub use r#struct::{Field, Fields, StructInfo};

use crate::r#type::{ArrayData, PointerData, StructData, TypeData, TypeDataKind, TypeDataStore};

mod array;
mod pointer;
mod primitive;
mod r#struct;

/// A [`Type`] holds information about a codira type.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Type(*const c_void, *const c_void);

impl From<crate::Type> for Type {
    fn from(ty: crate::Type) -> Self {
        let ty = ManuallyDrop::new(ty);
        Type(ty.inner.as_ptr() as *const _, Arc::as_ptr(&ty.store).cast())
    }
}

impl Type {
    /// Returns the store associated with the Type or
    unsafe fn store(&self) -> Result<ManuallyDrop<Arc<TypeDataStore>>, String> {
        if self.1.is_null() {
            return Err(String::from("null pointer"));
        }

        Ok(ManuallyDrop::new(Arc::from_raw(
            self.1.cast::<TypeDataStore>(),
        )))
    }

    /// Returns the store associated with the Type or
    unsafe fn inner(&self) -> Result<&TypeData, String> {
        self.0
            .cast::<TypeData>()
            .as_ref()
            .ok_or_else(|| String::from("null pointer"))
    }

    /// Converts this FFI type into an owned Rust type. This transfers the
    /// ownership from the FFI type back to Rust.
    ///
    /// # Safety
    ///
    /// The caller must ensure that self contains valid pointers.
    pub unsafe fn to_owned(self) -> Result<super::Type, String> {
        if self.0.is_null() {
            return Err(String::from("null pointer"));
        }

        if self.1.is_null() {
            return Err(String::from("null pointer"));
        }

        Ok(super::Type {
            inner: NonNull::new_unchecked(self.0 as *mut _),
            store: Arc::from_raw(self.1.cast()),
        })
    }

    /// Returns an invalid Type
    pub const fn null() -> Self {
        Self(ptr::null(), ptr::null())
    }
}

/// Notifies the runtime that the specified type is no longer used. Any use of
/// the type after calling this function results in undefined behavior.
///
/// # Safety
///
/// This function results in undefined behavior if the passed in `Type` has been
/// deallocated in a previous call to [`codira_type_release`].
#[no_mangle]
pub unsafe extern "C" fn codira_type_release(ty: Type) -> ErrorHandle {
    // Transfer ownership to Rust and immediately drop the instance
    let _ = codira_error_try!(ty
        .to_owned()
        .map_err(|e| format!("invalid argument 'ty': {e}")));

    ErrorHandle::default()
}

/// Increments the usage count of the specified type.
///
/// # Safety
///
/// This function results in undefined behavior if the passed in `Type` has been
/// deallocated in a previous call to [`codira_type_release`].
#[no_mangle]
pub unsafe extern "C" fn codira_type_add_reference(ty: Type) -> ErrorHandle {
    let store = codira_error_try!(ty
        .store()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    let inner = codira_error_try!(ty
        .inner()
        .map_err(|e| format!("invalid argument 'ty': {e}")));

    // Release the references owned by the type
    inner.external_references.fetch_add(1, Ordering::Relaxed);
    Arc::increment_strong_count(Arc::as_ptr(&store));

    ErrorHandle::default()
}

/// Retrieves the type's name.
///
/// # Safety
///
/// The caller is responsible for calling `codira_string_destroy` on the return
/// pointer - if it is not null.
///
/// This function results in undefined behavior if the passed in `Type` has been
/// deallocated in a previous call to [`codira_type_release`].
#[no_mangle]
pub unsafe extern "C" fn codira_type_name(ty: Type, name: *mut *const c_char) -> ErrorHandle {
    let inner = codira_error_try!(ty
        .inner()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    let name = try_deref_mut!(name);
    *name = CString::new(inner.name.clone()).unwrap().into_raw();
    ErrorHandle::default()
}

/// Compares two different Types. Returns `true` if the two types are equal. If
/// either of the two types is invalid because for instance it contains null
/// pointers this function returns `false`.
///
/// # Safety
///
/// This function results in undefined behavior if the passed in `Type`s have
/// been deallocated in a previous call to [`codira_type_release`].
#[no_mangle]
pub unsafe extern "C" fn codira_type_equal(a: Type, b: Type) -> bool {
    match (a.inner(), b.inner()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Returns the storage size required for a type. The storage size does not
/// include any padding to align the size. Call [`codira_type_alignment`] to
/// request the alignment of the type.
///
/// # Safety
///
/// This function results in undefined behavior if the passed in `Type`s have
/// been deallocated in a previous call to [`codira_type_release`].
#[no_mangle]
pub unsafe extern "C" fn codira_type_size(ty: Type, size: *mut usize) -> ErrorHandle {
    let size = try_deref_mut!(size);
    let inner = codira_error_try!(ty
        .inner()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    *size = inner.layout.size();
    ErrorHandle::default()
}

/// Returns the alignment requirements of the type.
///
/// # Safety
///
/// This function results in undefined behavior if the passed in `Type`s have
/// been deallocated in a previous call to [`codira_type_release`].
#[no_mangle]
pub unsafe extern "C" fn codira_type_alignment(ty: Type, align: *mut usize) -> ErrorHandle {
    let align = try_deref_mut!(align);
    let inner = codira_error_try!(ty
        .inner()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    *align = inner.layout.align();
    ErrorHandle::default()
}

/// Returns a new [`Type`] that is a pointer to the specified type.
///
/// # Safety
///
/// This function results in undefined behavior if the passed in `Type`s have
/// been deallocated in a previous call to [`codira_type_release`].
#[no_mangle]
pub unsafe extern "C" fn codira_type_pointer_type(
    ty: Type,
    mutable: bool,
    pointer_ty: *mut Type,
) -> ErrorHandle {
    let pointer_ty = try_deref_mut!(pointer_ty);
    let store = codira_error_try!(ty
        .store()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    let inner = codira_error_try!(ty
        .inner()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    *pointer_ty = inner.pointer_type(mutable, &store).into();
    ErrorHandle::default()
}

/// Returns a new [`Type`] that is an array of the specified type.
///
/// # Safety
///
/// This function results in undefined behavior if the passed in `Type`s have
/// been deallocated in a previous call to [`codira_type_release`].
#[no_mangle]
pub unsafe extern "C" fn codira_type_array_type(ty: Type, array_ty: *mut Type) -> ErrorHandle {
    let array_ty = try_deref_mut!(array_ty);
    let store = codira_error_try!(ty
        .store()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    let inner = codira_error_try!(ty
        .inner()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    *array_ty = inner.array_type(&store).into();
    ErrorHandle::default()
}

/// An enum that defines the kind of type.
#[repr(u8)]
pub enum TypeKind {
    Primitive(Guid),
    Pointer(r#pointer::PointerInfo),
    Struct(r#struct::StructInfo),
    Array(r#array::ArrayInfo),
}

/// Returns information about what kind of type this is.
///
/// # Safety
///
/// This function results in undefined behavior if the passed in `Type`s have
/// been deallocated in a previous call to [`codira_type_release`].
#[no_mangle]
pub unsafe extern "C" fn codira_type_kind(ty: Type, kind: *mut TypeKind) -> ErrorHandle {
    let kind = try_deref_mut!(kind);
    let store = codira_error_try!(ty
        .store()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    let inner = codira_error_try!(ty
        .inner()
        .map_err(|e| format!("invalid argument 'ty': {e}")));

    *kind = match &inner.data {
        TypeDataKind::Primitive(guid) => TypeKind::Primitive(*guid),
        TypeDataKind::Pointer(pointer) => TypeKind::Pointer(PointerInfo(
            (pointer as *const PointerData).cast(),
            Arc::as_ptr(ManuallyDrop::deref(&store)).cast(),
        )),
        TypeDataKind::Struct(s) => TypeKind::Struct(StructInfo(
            (s as *const StructData).cast(),
            Arc::as_ptr(ManuallyDrop::deref(&store)).cast(),
        )),
        TypeDataKind::Array(a) => TypeKind::Array(ArrayInfo(
            (a as *const ArrayData).cast(),
            Arc::as_ptr(ManuallyDrop::deref(&store)).cast(),
        )),
        TypeDataKind::Uninitialized => unreachable!(),
    };

    ErrorHandle::default()
}

/// An array of [`Type`]s.
///
/// The `Types` struct owns the `Type`s it references. Ownership of the `Type`
/// can be shared by calling [`codira_type_add_reference`].
///
/// This is backed by a dynamically allocated array. Ownership is transferred
/// via this struct and its contents must be destroyed with
/// [`codira_types_destroy`].
#[repr(C)]
pub struct Types {
    pub types: *const Type,
    pub count: usize,
}

impl From<Vec<Type>> for Types {
    fn from(mut vec: Vec<Type>) -> Self {
        vec.shrink_to_fit();
        let vec = ManuallyDrop::new(vec);
        Types {
            types: if vec.is_empty() {
                ptr::null()
            } else {
                vec.as_ptr()
            },
            count: vec.len(),
        }
    }
}

/// Destroys the contents of a [`Types`] struct.
///
/// # Safety
///
/// This function results in undefined behavior if the passed in `Types` has
/// been deallocated by a previous call to [`codira_types_destroy`].
#[no_mangle]
pub unsafe extern "C" fn codira_types_destroy(types: Types) -> ErrorHandle {
    if types.types.is_null() && types.count > 0 {
        return ErrorHandle::new("invalid argument 'types': null pointer");
    } else if types.count > 0 {
        let types = Vec::from_raw_parts(types.types as *mut Type, types.count, types.count);
        for ty in types {
            // Take ownership of the stored type and drop it
            drop(codira_error_try!(ty
                .to_owned()
                .map_err(|e| format!("fields contain invalid type: {e}"))));
        }
    }
    ErrorHandle::default()
}

#[cfg(test)]
mod test {
    use std::{
        ffi::{c_void, CStr, CString},
        mem::MaybeUninit,
        ptr,
    };

    use codira_capi_utils::{
        assert_error, assert_error_snapshot, assert_getter1, assert_getter2, codira_string_destroy,
    };

    use super::{
        codira_type_equal, codira_type_name, codira_type_release,
        primitive::{codira_type_primitive, PrimitiveType},
        Type,
    };
    use crate::{
        ffi::{codira_types_destroy, Types},
        r#type::ffi::{
            codira_type_add_reference, codira_type_alignment, codira_type_array_type,
            codira_type_pointer_type, codira_type_size,
        },
        HasStaticType,
    };

    const FFI_TYPE_NULL_INNER: Type = Type(ptr::null(), 0xDEAD as *const c_void);
    const FFI_TYPE_NULL_STORE: Type = Type(0xDEAD as *const c_void, ptr::null());
    const FFI_TYPE_NULL: Type = Type(ptr::null(), ptr::null());

    #[test]
    fn test_codira_type_release_null_type() {
        assert_error!(unsafe { codira_type_release(FFI_TYPE_NULL_INNER) });
        assert_error!(unsafe { codira_type_release(FFI_TYPE_NULL_STORE) });
    }

    #[test]
    fn test_codira_type_add_reference_null_type() {
        assert_error!(unsafe { codira_type_add_reference(FFI_TYPE_NULL_INNER) });
        assert_error!(unsafe { codira_type_add_reference(FFI_TYPE_NULL_STORE) });
    }

    #[test]
    fn test_codira_type_equal() {
        let ffi_f32 = codira_type_primitive(PrimitiveType::F32);
        let ffi_i32 = codira_type_primitive(PrimitiveType::I32);

        assert!(unsafe { codira_type_equal(ffi_f32, ffi_f32) });
        assert!(unsafe { codira_type_equal(ffi_i32, ffi_i32) });
        assert!(!unsafe { codira_type_equal(ffi_f32, ffi_i32) });
        assert!(!unsafe { codira_type_equal(ffi_i32, ffi_f32) });
        assert!(!unsafe { codira_type_equal(ffi_f32, FFI_TYPE_NULL) });
        assert!(!unsafe { codira_type_equal(FFI_TYPE_NULL, ffi_f32) });
        assert!(!unsafe { codira_type_equal(FFI_TYPE_NULL, FFI_TYPE_NULL) });

        unsafe { codira_type_release(ffi_f32) };
        unsafe { codira_type_release(ffi_i32) };
    }

    #[test]
    fn test_codira_type_name() {
        let ffi_f32 = codira_type_primitive(PrimitiveType::F32);
        let ffi_empty = codira_type_primitive(PrimitiveType::Empty);

        assert_getter1!(codira_type_name(ffi_f32, f32_name));
        assert_getter1!(codira_type_name(ffi_empty, empty_name));

        assert_eq!(
            unsafe { CStr::from_ptr(f32_name) },
            CString::new("core::f32").unwrap().as_ref()
        );
        assert_eq!(
            unsafe { CStr::from_ptr(empty_name) },
            CString::new("core::empty").unwrap().as_ref()
        );

        unsafe { codira_string_destroy(empty_name) };
        unsafe { codira_string_destroy(f32_name) };
        unsafe { codira_type_release(ffi_empty) };
        unsafe { codira_type_release(ffi_f32) };
    }

    #[test]
    fn test_codira_type_name_invalid_type() {
        let mut name = MaybeUninit::uninit();
        assert_error_snapshot!(
            unsafe { codira_type_name(FFI_TYPE_NULL, name.as_mut_ptr()) },
            @r###""invalid argument \'ty\': null pointer""###
        );

        let ffi_i8 = codira_type_primitive(PrimitiveType::I8);
        assert_error_snapshot!(
            unsafe { codira_type_name(ffi_i8, ptr::null_mut()) },
            @r###""invalid argument \'name\': null pointer""###
        );

        unsafe { codira_type_release(ffi_i8) };
    }

    #[test]
    fn test_codira_type_size() {
        fn assert_size(ty: Type, expected_size: usize) {
            let mut size = MaybeUninit::uninit();
            assert!(unsafe { codira_type_size(ty, size.as_mut_ptr()) }.is_ok());
            assert_eq!(unsafe { size.assume_init() }, expected_size);
        }

        let ffi_i8 = codira_type_primitive(PrimitiveType::I8);
        let ffi_u16 = codira_type_primitive(PrimitiveType::U16);
        let ffi_i32 = codira_type_primitive(PrimitiveType::I32);
        let ffi_u64 = codira_type_primitive(PrimitiveType::U64);

        assert_size(ffi_i8, 1);
        assert_size(ffi_u16, 2);
        assert_size(ffi_i32, 4);
        assert_size(ffi_u64, 8);

        unsafe { codira_type_release(ffi_u64) };
        unsafe { codira_type_release(ffi_i32) };
        unsafe { codira_type_release(ffi_u16) };
        unsafe { codira_type_release(ffi_i8) };
    }

    #[test]
    fn test_codira_type_size_invalid_null() {
        let mut size = MaybeUninit::uninit();
        assert_error!(unsafe { codira_type_size(FFI_TYPE_NULL, size.as_mut_ptr()) });

        let ffi_i8 = codira_type_primitive(PrimitiveType::I8);
        assert_error!(unsafe { codira_type_size(ffi_i8, ptr::null_mut()) });
        unsafe { codira_type_release(ffi_i8) };
    }

    #[test]
    fn test_codira_type_alignment() {
        fn assert_alignment(ty: Type, expected_alignment: usize) {
            let mut align = MaybeUninit::uninit();
            assert!(unsafe { codira_type_alignment(ty, align.as_mut_ptr()) }.is_ok());
            assert_eq!(unsafe { align.assume_init() }, expected_alignment);
        }

        let ffi_i8 = codira_type_primitive(PrimitiveType::I8);
        let ffi_u16 = codira_type_primitive(PrimitiveType::U16);
        let ffi_i32 = codira_type_primitive(PrimitiveType::I32);
        let ffi_u64 = codira_type_primitive(PrimitiveType::U64);

        assert_alignment(ffi_i8, 1);
        assert_alignment(ffi_u16, 2);
        assert_alignment(ffi_i32, 4);
        assert_alignment(ffi_u64, 8);

        unsafe { codira_type_release(ffi_u64) };
        unsafe { codira_type_release(ffi_i32) };
        unsafe { codira_type_release(ffi_u16) };
        unsafe { codira_type_release(ffi_i8) };
    }

    #[test]
    fn test_codira_type_alignment_invalid_null() {
        let mut size = MaybeUninit::uninit();
        assert_error_snapshot!(
            unsafe { codira_type_alignment(FFI_TYPE_NULL, size.as_mut_ptr()) },
            @r###""invalid argument \'ty\': null pointer""###
        );

        let ffi_i8 = codira_type_primitive(PrimitiveType::I8);
        assert_error_snapshot!(
            unsafe { codira_type_alignment(ffi_i8, ptr::null_mut()) },
            @r###""invalid argument \'align\': null pointer""###
        );

        unsafe { codira_type_release(ffi_i8) };
    }

    #[test]
    fn test_codira_type_pointer_type() {
        let ffi_u64 = codira_type_primitive(PrimitiveType::U64);

        assert_getter2!(codira_type_pointer_type(ffi_u64, true, ffi_u64_pointer));
        let rust_u64_pointer = unsafe { ffi_u64_pointer.to_owned() }.unwrap();
        let pointer_info = rust_u64_pointer
            .as_pointer()
            .expect("type is not a pointer");
        assert_eq!(&pointer_info.pointee(), u64::type_info());
        assert!(&pointer_info.is_mutable());

        unsafe { codira_type_release(ffi_u64) };
    }

    #[test]
    fn test_codira_type_pointer_type_invalid_null() {
        let mut ffi_u64_pointer = MaybeUninit::uninit();
        assert_error_snapshot!(
            unsafe { codira_type_pointer_type(FFI_TYPE_NULL, true, ffi_u64_pointer.as_mut_ptr()) },
            @r###""invalid argument \'ty\': null pointer""###
        );

        let ffi_u64 = codira_type_primitive(PrimitiveType::U64);
        assert_error_snapshot!(
            unsafe { codira_type_pointer_type(ffi_u64, true, ptr::null_mut()) },
            @r###""invalid argument \'pointer_ty\': null pointer""###
        );
        unsafe { codira_type_release(ffi_u64) };
    }

    #[test]
    fn test_codira_type_array_type() {
        let ffi_u64 = codira_type_primitive(PrimitiveType::U64);

        assert_getter1!(codira_type_array_type(ffi_u64, ffi_u64_array));
        let rust_u64_array = unsafe { ffi_u64_array.to_owned() }.unwrap();
        let array_info = rust_u64_array.as_array().expect("type is not an array");
        assert_eq!(&array_info.element_type(), u64::type_info());

        unsafe { codira_type_release(ffi_u64) };
    }

    #[test]
    fn test_codira_type_array_type_invalid_null() {
        let mut ffi_u64_array = MaybeUninit::uninit();
        assert_error_snapshot!(
            unsafe { codira_type_array_type(FFI_TYPE_NULL, ffi_u64_array.as_mut_ptr()) },
            @r###""invalid argument \'ty\': null pointer""###
        );

        let ffi_u64 = codira_type_primitive(PrimitiveType::U64);
        assert_error_snapshot!(
            unsafe { codira_type_array_type(ffi_u64, ptr::null_mut()) },
            @r###""invalid argument \'array_ty\': null pointer""###
        );
        unsafe { codira_type_release(ffi_u64) };
    }

    #[test]
    fn test_codira_types_destroy() {
        let types: Types = [i32::type_info().clone(), f32::type_info().clone()]
            .iter()
            .map(|ty| ty.clone().into())
            .collect::<Vec<_>>()
            .into();
        assert!(unsafe { codira_types_destroy(types) }.is_ok());
    }

    #[test]
    fn test_codira_types_destroy_invalid_ptr() {
        let types = Types {
            types: ptr::null(),
            count: 1,
        };
        assert_error!(unsafe { codira_types_destroy(types) });
    }

    #[test]
    fn test_codira_types_destroy_empty() {
        let types = Types {
            types: ptr::null(),
            count: 0,
        };
        assert!(unsafe { codira_types_destroy(types) }.is_ok());
    }
}
