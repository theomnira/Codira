//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! Exposes Codira garbage collection.

use std::mem::ManuallyDrop;

use codira_capi_utils::{codira_error_try, error::ErrorHandle, try_deref_mut};
pub use codira_memory::gc::GcPtr;
use codira_memory::{ffi::Type, gc::GcRuntime};

use crate::runtime::Runtime;

/// Allocates an object in the runtime of the given `ty`. If successful, `obj`
/// is set, otherwise a non-zero error handle is returned.
///
/// If a non-zero error handle is returned, it must be manually destructed using
/// [`codira_error_destroy`].
///
/// # Safety
///
/// This function receives raw pointers as parameters. If any of the arguments
/// is a null pointer, an error will be returned. Passing pointers to invalid
/// data, will lead to undefined behavior.
#[no_mangle]
pub unsafe extern "C" fn codira_gc_alloc(
    runtime: Runtime,
    ty: Type,
    obj: *mut GcPtr,
) -> ErrorHandle {
    let runtime = codira_error_try!(runtime
        .inner()
        .map_err(|e| format!("invalid argument 'runtime': {e}")));
    let ty = codira_error_try!(ty
        .to_owned()
        .map_err(|e| format!("invalid argument 'obj': {e}"))
        .map(ManuallyDrop::new));
    let obj = try_deref_mut!(obj);
    *obj = runtime.gc().alloc(&ty);
    ErrorHandle::default()
}

/// Retrieves the `ty` for the specified `obj` from the runtime. If successful,
/// `ty` is set, otherwise a non-zero error handle is returned.
///
/// If a non-zero error handle is returned, it must be manually destructed using
/// [`codira_error_destroy`].
///
/// # Safety
///
/// This function receives raw pointers as parameters. If any of the arguments
/// is a null pointer, an error will be returned. Passing pointers to invalid
/// data, will lead to undefined behavior.
#[no_mangle]
pub unsafe extern "C" fn codira_gc_ptr_type(
    runtime: Runtime,
    obj: GcPtr,
    ty: *mut Type,
) -> ErrorHandle {
    let runtime = codira_error_try!(runtime
        .inner()
        .map_err(|e| format!("invalid argument 'runtime': {e}")));
    let ty = try_deref_mut!(ty);
    *ty = runtime.gc().ptr_type(obj).into();
    ErrorHandle::default()
}

/// Roots the specified `obj`, which keeps it and objects it references alive.
/// Objects marked as root, must call `codira_gc_unroot` before they can be
/// collected. An object can be rooted multiple times, but you must make sure to
/// call `codira_gc_unroot` an equal number of times before the object can be
/// collected. If successful, `obj` has been rooted, otherwise a non-zero error
/// handle is returned.
///
/// If a non-zero error handle is returned, it must be manually destructed using
/// [`codira_error_destroy`].
///
/// # Safety
///
/// This function receives raw pointers as parameters. If any of the arguments
/// is a null pointer, an error will be returned. Passing pointers to invalid
/// data, will lead to undefined behavior.
#[no_mangle]
pub unsafe extern "C" fn codira_gc_root(runtime: Runtime, obj: GcPtr) -> ErrorHandle {
    let runtime = codira_error_try!(runtime
        .inner()
        .map_err(|e| format!("invalid argument 'runtime': {e}")));
    runtime.gc().root(obj);
    ErrorHandle::default()
}

/// Unroots the specified `obj`, potentially allowing it and objects it
/// references to be collected. An object can be rooted multiple times, so you
/// must make sure to call `codira_gc_unroot` the same number of times as
/// `codira_gc_root` was called before the object can be collected. If
/// successful, `obj` has been unrooted, otherwise a non-zero error handle is
/// returned.
///
/// If a non-zero error handle is returned, it must be manually destructed using
/// [`codira_error_destroy`].
///
/// # Safety
///
/// This function receives raw pointers as parameters. If any of the arguments
/// is a null pointer, an error will be returned. Passing pointers to invalid
/// data, will lead to undefined behavior.
#[no_mangle]
pub unsafe extern "C" fn codira_gc_unroot(runtime: Runtime, obj: GcPtr) -> ErrorHandle {
    let runtime = codira_error_try!(runtime
        .inner()
        .map_err(|e| format!("invalid argument 'runtime': {e}")));
    runtime.gc().unroot(obj);
    ErrorHandle::default()
}

/// Collects all memory that is no longer referenced by rooted objects. If
/// successful, `reclaimed` is set, otherwise a non-zero error handle is
/// returned. If `reclaimed` is `true`, memory was reclaimed, otherwise nothing
/// happend. This behavior will likely change in the future.
///
/// If a non-zero error handle is returned, it must be manually destructed using
/// [`codira_error_destroy`].
///
/// # Safety
///
/// This function receives raw pointers as parameters. If any of the arguments
/// is a null pointer, an error will be returned. Passing pointers to invalid
/// data, will lead to undefined behavior.
#[no_mangle]
pub unsafe extern "C" fn codira_gc_collect(runtime: Runtime, reclaimed: *mut bool) -> ErrorHandle {
    let runtime = codira_error_try!(runtime
        .inner()
        .map_err(|e| format!("invalid argument 'runtime': {e}")));
    let reclaimed = try_deref_mut!(reclaimed);
    *reclaimed = runtime.gc_collect();
    ErrorHandle::default()
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::CString,
        mem::{self},
        ptr,
    };

    use codira_capi_utils::{
        assert_error_snapshot, assert_getter1, assert_getter2, error::codira_error_destroy,
    };
    use codira_memory::{
        ffi::{codira_type_equal, Type},
        gc::{HasIndirectionPtr, RawGcPtr},
    };

    use super::*;
    use crate::{
        runtime::codira_runtime_get_type_info_by_name, test_invalid_runtime, test_util::TestDriver,
    };

    test_invalid_runtime!(
        gc_alloc(Type::null(), ptr::null_mut()),
        gc_ptr_type(mem::zeroed::<GcPtr>(), ptr::null_mut()),
        gc_root(mem::zeroed::<GcPtr>()),
        gc_unroot(mem::zeroed::<GcPtr>()),
        gc_collect(ptr::null_mut())
    );

    #[test]
    fn test_gc_alloc_invalid_type_info() {
        let driver = TestDriver::new(
            r#"
        public class Foo;
    "#,
        );

        assert_error_snapshot!(
            unsafe { codira_gc_alloc(driver.runtime, Type::null(), ptr::null_mut()) },
            @r#""invalid argument \'obj\': null pointer""#
        );
    }

    #[test]
    fn test_gc_alloc_invalid_obj() {
        let driver = TestDriver::new(
            r#"
        public class Foo;
    "#,
        );

        let type_name = CString::new("Foo").expect("Invalid type name.");
        assert_getter2!(codira_runtime_get_type_info_by_name(
            driver.runtime,
            type_name.as_ptr(),
            has_type,
            ty,
        ));
        assert!(has_type);

        assert_error_snapshot!(
            unsafe { codira_gc_alloc(driver.runtime, ty, ptr::null_mut()) },
            @r#""invalid argument \'obj\': null pointer""#
        );
    }

    #[test]
    fn test_gc_alloc() {
        let driver = TestDriver::new(
            r#"
        public class Foo;
    "#,
        );

        let type_name = CString::new("Foo").expect("Invalid type name.");
        assert_getter2!(codira_runtime_get_type_info_by_name(
            driver.runtime,
            type_name.as_ptr(),
            has_type,
            ty,
        ));
        assert!(has_type);

        assert_getter2!(codira_gc_alloc(driver.runtime, ty, obj));
        assert_ne!(unsafe { obj.deref::<u8>() }, ptr::null());

        assert_getter1!(codira_gc_collect(driver.runtime, reclaimed));
        assert!(reclaimed);
    }

    #[test]
    fn test_gc_ptr_type_invalid_type_info() {
        let driver = TestDriver::new(
            r#"
        public class Foo;
    "#,
        );

        assert_error_snapshot!(
            unsafe {
                let raw_ptr: RawGcPtr = ptr::null();
                codira_gc_ptr_type(driver.runtime, raw_ptr.into(), ptr::null_mut())
            },
            @r#""invalid argument \'ty\': null pointer""#
        );
    }

    #[test]
    fn test_gc_ptr_type() {
        let driver = TestDriver::new(
            r#"
        public class Foo;
    "#,
        );

        let type_name = CString::new("Foo").expect("Invalid type name.");
        assert_getter2!(codira_runtime_get_type_info_by_name(
            driver.runtime,
            type_name.as_ptr(),
            has_type,
            ty,
        ));
        assert!(has_type);

        assert_getter2!(codira_gc_alloc(driver.runtime, ty, obj));
        assert_ne!(unsafe { obj.deref::<u8>() }, ptr::null());

        assert_getter2!(codira_gc_ptr_type(driver.runtime, obj, ptr_ty));
        assert!(unsafe { codira_type_equal(ty, ptr_ty) });

        assert_getter1!(codira_gc_collect(driver.runtime, reclaimed));
        assert!(reclaimed);
    }

    #[test]
    fn test_gc_rooting() {
        let driver = TestDriver::new(
            r#"
        public class Foo;
    "#,
        );

        let type_name = CString::new("Foo").expect("Invalid type name.");
        assert_getter2!(codira_runtime_get_type_info_by_name(
            driver.runtime,
            type_name.as_ptr(),
            has_type,
            ty,
        ));
        assert!(has_type);

        assert_getter2!(codira_gc_alloc(driver.runtime, ty, obj));
        assert_ne!(unsafe { obj.deref::<u8>() }, ptr::null());

        assert!(unsafe { codira_gc_root(driver.runtime, obj) }.is_ok());

        assert_getter1!(codira_gc_collect(driver.runtime, reclaimed));
        assert!(!reclaimed);

        assert!(unsafe { codira_gc_unroot(driver.runtime, obj) }.is_ok());

        assert_getter1!(codira_gc_collect(driver.runtime, reclaimed));
        assert!(reclaimed);
    }

    #[test]
    fn test_gc_ptr_collect_invalid_reclaimed() {
        let driver = TestDriver::new(
            r#"
        public class Foo;

        public func main() -> Foo { Foo }
    "#,
        );

        assert_error_snapshot!(
            unsafe { codira_gc_collect(driver.runtime, ptr::null_mut()) },
            @r#""invalid argument \'reclaimed\': null pointer""#
        );
    }
}
