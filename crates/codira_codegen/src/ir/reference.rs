//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::ffi::CStr;

use inkwell::{
    builder::Builder,
    types::{BasicType, BasicTypeEnum},
    values::{BasicValueEnum, PointerValue},
};

/// A helper struct that wraps an object on the heap.
///
/// Objects on the heap are represented as an indirection. The stored pointer
/// points to an object on the heap where the first field points to the actual
/// data of the object:
///
/// ```c
/// struct Obj {
///     ObjectData *data;
///     ...
/// }
/// ```
///
/// This enables the runtime to modify the contents of the object without having
/// to modify the references that point to it.
///
/// The `RuntimeReferenceValue` stores the indirection as `**T` (a pointer to a
/// pointer to `T`), where T is the type of the object stored on the heap.
///
/// Under LLVM's opaque pointers (15+), a `PointerValue`'s LLVM type carries no
/// pointee information at all -- every pointer is just `ptr`. This struct
/// therefore stores `object_type` explicitly (the caller always knows it --
/// see every construction site) rather than trying to recover it from `ptr`
/// via LLVM introspection (`get_element_type`, removed in LLVM 15). This is
/// the same fix applied throughout `codira_codegen`'s LLVM-14 -> 22 migration:
/// stop asking LLVM for pointee types, carry the already-known Codira/LLVM
/// type alongside the pointer instead.
// Note: no `Hash` -- `inkwell::types::BasicTypeEnum` doesn't implement it
// (it did before this struct stored one; `PointerValue` alone used to).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct RuntimeReferenceValue<'ink> {
    ptr: PointerValue<'ink>,
    object_type: BasicTypeEnum<'ink>,
}

impl<'ink> RuntimeReferenceValue<'ink> {
    /// Constructs a new `RuntimeReferenceValue` from a reference pointer to a
    /// specific type.
    ///
    /// The pointer passed must be of type `**T` under typed pointers; under
    /// opaque pointers there is nothing left to validate about `ptr` itself
    /// (every pointer has the same `ptr` type), so this always succeeds --
    /// the `Result` return type is kept for source compatibility with
    /// existing call sites.
    pub fn from_ptr(
        ptr: PointerValue<'ink>,
        object_type: impl BasicType<'ink>,
    ) -> Result<Self, String> {
        Ok(Self {
            ptr,
            object_type: object_type.as_basic_type_enum(),
        })
    }

    /// Constructs a new instance from an inkwell `PointerValue` and its
    /// already-known object type, without checking that `ptr` is actually a
    /// pointer to an object on the heap of that type.
    pub unsafe fn from_ptr_unchecked(
        ptr: PointerValue<'ink>,
        object_type: impl BasicType<'ink>,
    ) -> Self {
        Self {
            ptr,
            object_type: object_type.as_basic_type_enum(),
        }
    }

    /// Returns the name of the inkwell value
    pub fn get_name(&self) -> &CStr {
        self.ptr.get_name()
    }

    /// Generates code to dereference the reference to get to the data of the
    /// reference.
    pub fn get_data_ptr(&self, builder: &Builder<'ink>) -> PointerValue<'ink> {
        let value_name = self.ptr.get_name().to_string_lossy();

        // Dereference the pointer to get the pointer to the data
        //
        // ```c
        // data_ptr:*const T: = *data_ptr_ptr;
        // ```
        // Under opaque pointers every pointer type is the same `ptr`
        // regardless of pointee, so `self.object_type`'s own pointer type
        // (rather than anything more specific) is the correct pointee type
        // to load through here.
        // Opaque pointers: loading through a `ptr` needs no pointee
        // type, so the value's own type is the right load type.
        let data_ptr_ty = self.ptr.get_type();
        builder
            .build_load(data_ptr_ty, self.ptr, &format!("{value_name}->data"))
            .expect("failed to build load for reference data pointer")
            .into_pointer_value()
    }

    /// Returns the type of the object this instance points to
    pub fn get_type(&self) -> BasicTypeEnum<'ink> {
        self.object_type
    }
}

impl<'ink> From<RuntimeReferenceValue<'ink>> for BasicValueEnum<'ink> {
    fn from(value: RuntimeReferenceValue<'ink>) -> Self {
        value.ptr.into()
    }
}

impl<'ink> From<RuntimeReferenceValue<'ink>> for PointerValue<'ink> {
    fn from(value: RuntimeReferenceValue<'ink>) -> Self {
        value.ptr
    }
}
