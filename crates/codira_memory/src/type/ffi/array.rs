//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{ffi::c_void, mem::ManuallyDrop, ops::Deref, sync::Arc};

use codira_capi_utils::{codira_error_try, try_deref_mut, ErrorHandle};

use crate::{
    ffi::Type,
    r#type::{ArrayData, Type as RustType, TypeDataStore},
};

/// Additional information of an array [`Type`].
///
/// Ownership of this type lies with the [`Type`] that created this instance. As
/// long as the original type is not released through [`codira_type_release`]
/// this type stays alive.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ArrayInfo(pub(super) *const c_void, pub(super) *const c_void);

impl<'t> From<crate::ArrayType<'t>> for ArrayInfo {
    fn from(ty: crate::ArrayType<'t>) -> Self {
        ArrayInfo(
            (ty.inner as *const ArrayData).cast(),
            (&ty.store as *const &Arc<TypeDataStore>).cast(),
        )
    }
}

impl ArrayInfo {
    /// Returns the store associated with this instance
    unsafe fn store(&self) -> Result<ManuallyDrop<Arc<TypeDataStore>>, String> {
        if self.1.is_null() {
            return Err(String::from("null pointer"));
        }

        Ok(ManuallyDrop::new(Arc::from_raw(
            self.1.cast::<TypeDataStore>(),
        )))
    }

    /// Returns the struct info associated with the Type
    unsafe fn inner(&self) -> Result<&ArrayData, String> {
        match self.0.cast::<ArrayData>().as_ref() {
            Some(store) => Ok(store),
            None => Err(String::from("null pointer")),
        }
    }
}

/// Returns the type of the elements stored in this type. Ownership is
/// transferred if this function returns successfully.
///
/// # Safety
///
/// This function results in undefined behavior if the passed in `ArrayInfo` has
/// been deallocated by a previous call to [`codira_type_release`].
#[no_mangle]
pub unsafe extern "C" fn codira_array_type_element_type(
    ty: ArrayInfo,
    element_ty: *mut Type,
) -> ErrorHandle {
    let store = codira_error_try!(ty
        .store()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    let ty = codira_error_try!(ty
        .inner()
        .map_err(|e| format!("invalid argument 'ty': {e}")));
    let element_ty = try_deref_mut!(element_ty);
    *element_ty =
        RustType::new_unchecked(ty.element_ty, ManuallyDrop::deref(&store).clone()).into();
    ErrorHandle::default()
}

#[cfg(test)]
mod test {
    use std::{mem::MaybeUninit, ptr};

    use codira_capi_utils::{assert_error_snapshot, assert_getter1};

    use super::{codira_array_type_element_type, ArrayInfo};
    use crate::{
        ffi::{
            codira_type_array_type, codira_type_equal, codira_type_kind, codira_type_release, Type,
            TypeKind,
        },
        r#type::ffi::primitive::{codira_type_primitive, PrimitiveType},
    };

    /// Returns the array type of the specified type. Asserts if that fails.
    unsafe fn array_type(ty: Type) -> (Type, ArrayInfo) {
        assert_getter1!(codira_type_array_type(ty, array_ty));

        assert_getter1!(codira_type_kind(array_ty, ty_kind));
        let array_ty = match ty_kind {
            TypeKind::Array(a) => a,
            _ => panic!("invalid type kind for array"),
        };

        (ty, array_ty)
    }

    #[test]
    fn test_codira_array_type_pointee() {
        let ffi_f32 = codira_type_primitive(PrimitiveType::F32);
        let (ffi_f32_ptr, array_info) = unsafe { array_type(ffi_f32) };

        assert_getter1!(codira_array_type_element_type(array_info, element_ty));
        assert!(unsafe { codira_type_equal(element_ty, ffi_f32) });

        unsafe { codira_type_release(element_ty) };
        unsafe { codira_type_release(ffi_f32_ptr) };
        unsafe { codira_type_release(ffi_f32) };
    }

    #[test]
    fn test_codira_array_type_pointee_invalid_null() {
        let mut pointee_ty = MaybeUninit::uninit();
        assert_error_snapshot!(
            unsafe {
                codira_array_type_element_type(
                    ArrayInfo(ptr::null(), ptr::null()),
                    pointee_ty.as_mut_ptr(),
                )
            },
            @r###""invalid argument \'ty\': null pointer""###
        );

        let ffi_f32 = codira_type_primitive(PrimitiveType::F32);
        let (ffi_f32_ptr, ptr_info) = unsafe { array_type(ffi_f32) };
        assert_error_snapshot!(
            unsafe { codira_array_type_element_type(ptr_info, ptr::null_mut()) },
            @r###""invalid argument \'element_ty\': null pointer""###
        );

        unsafe { codira_type_release(ffi_f32_ptr) };
        unsafe { codira_type_release(ffi_f32) };
    }
}
