//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - How JIT-compiled code reaches the runtime: the functions it imports
//!   (declared from the runtime function catalogue, resolved to the very
//!   functions a runtime registers for LLVM-compiled code), the garbage
//!   collector its allocations go to, and the type descriptors (`Type`s) the
//!   collector reads to size and trace the objects.
//!
//! The descriptors are built from `codira_hir::layout`, which is checked
//! against LLVM's layout, so an object allocated here has the size, field
//! offsets and traced references an LLVM-compiled one has. A reference
//! field is described by one opaque class type: the collector only needs to
//! know that the slot holds a handle, and traces the object it points to by
//! that object's own header. That keeps descriptors of recursive classes
//! (`class Node { next: Node? }`) finite.

use std::{alloc::Layout, ffi::c_void, sync::Arc};

use codira_abi::{
    runtime_functions::{Repr, RuntimeFunction},
    Guid, StructMemoryKind,
};
use codira_hir::{
    layout::{object_layout, string_layout, value_layout, ValueLayout},
    mono, HirDatabase, IntBitness, ResolveBitness, StructMemoryKind as HirMemoryKind, Ty, TyKind,
};
use codira_runtime::{GarbageCollector, HasStaticType, Type};
use cranelift_codegen::ir::{types, AbiParam, Signature};
use cranelift_jit::JITBuilder;
use cranelift_module::{FuncId, Linkage, Module};
use rustc_hash::FxHashMap;

/// Registers every runtime function with the JIT's symbol table under its
/// catalogue name, pointing at the function the runtime registers for
/// LLVM-compiled code: one implementation for both backends.
pub(crate) fn register_symbols(jit: &mut JITBuilder) {
    for definition in codira_runtime::generated_code_functions() {
        jit.symbol(
            definition.prototype.name.clone(),
            definition.fn_ptr.cast::<u8>(),
        );
    }
}

/// The Cranelift signature of a catalogued runtime function, as the C
/// calling convention passes it: a byte is zero-extended by the caller.
pub(crate) fn signature(module: &dyn Module, function: &RuntimeFunction) -> Signature {
    let pointer = module.target_config().pointer_type();
    let param = |repr: Repr| match repr {
        Repr::U8 => AbiParam::new(types::I8).uext(),
        Repr::U32 | Repr::I32 => AbiParam::new(types::I32),
        Repr::I64 | Repr::U64 => AbiParam::new(types::I64),
        Repr::F32 => AbiParam::new(types::F32),
        Repr::F64 => AbiParam::new(types::F64),
        Repr::Usize
        | Repr::ConstBytes
        | Repr::MutBytes
        | Repr::Handle
        | Repr::MutHandle
        | Repr::Object
        | Repr::Unit => AbiParam::new(pointer),
    };
    let mut sig = module.make_signature();
    sig.params
        .extend(function.params.iter().map(|&(_, repr)| param(repr)));
    if function.ret != Repr::Unit {
        sig.returns.push(param(function.ret));
    }
    sig
}

/// The runtime functions a program imports, declared once each.
#[derive(Default)]
pub(crate) struct Imports {
    declared: FxHashMap<&'static str, FuncId>,
}

impl Imports {
    pub(crate) fn get(
        &mut self,
        module: &mut dyn Module,
        function: &'static RuntimeFunction,
    ) -> Result<FuncId, String> {
        if let Some(&id) = self.declared.get(function.symbol) {
            return Ok(id);
        }
        let sig = signature(module, function);
        let id = module
            .declare_function(function.symbol, Linkage::Import, &sig)
            .map_err(|e| e.to_string())?;
        self.declared.insert(function.symbol, id);
        Ok(id)
    }
}

/// The collector a program allocates in and the type descriptors its code
/// hands to the runtime. Owned by the `Program`, so both outlive its code.
pub(crate) struct Heap {
    gc: Arc<GarbageCollector>,
    types: FxHashMap<Ty, Type>,
    /// One opaque class type that stands for any referenced object.
    reference: Type,
    /// Handles given out with `Type::into_raw`, returned on drop.
    handles: Vec<*const c_void>,
}

impl Heap {
    pub(crate) fn new() -> Self {
        let reference = Type::new_struct(
            "codira.object",
            Layout::from_size_align(0, 1).expect("an empty layout"),
            Guid::from_str("codira.object"),
            Vec::new(),
            StructMemoryKind::Gc,
        );
        Heap {
            gc: Arc::new(GarbageCollector::default()),
            types: FxHashMap::default(),
            reference,
            handles: Vec::new(),
        }
    }

    /// The allocator handle the runtime's allocating functions take.
    pub(crate) fn allocator(&self) -> *mut c_void {
        Arc::as_ptr(&self.gc).cast_mut().cast()
    }

    /// The collector, for tests and statistics.
    pub(crate) fn collector(&self) -> &Arc<GarbageCollector> {
        &self.gc
    }

    /// The type handle of `ty` (a class or an array type, or `[u8]` for the
    /// string functions), valid while this heap lives.
    pub(crate) fn handle(
        &mut self,
        db: &dyn HirDatabase,
        ty: &Ty,
    ) -> Result<*const c_void, String> {
        let ty = self.memory_type(db, ty)?;
        let raw = Type::into_raw(ty);
        self.handles.push(raw);
        Ok(raw)
    }

    /// The descriptor of a value of type `ty`, as a field or an element.
    fn memory_type(&mut self, db: &dyn HirDatabase, ty: &Ty) -> Result<Type, String> {
        if let Some(found) = self.types.get(ty) {
            return Ok(found.clone());
        }
        let layout = db.target_data_layout();
        let made = match ty.interned() {
            TyKind::Bool => bool::type_info().clone(),
            TyKind::Int(int) => {
                let resolved = int.resolve(&layout);
                let signed = resolved.signedness.is_signed();
                match (resolved.bitness, signed) {
                    (IntBitness::X8, true) => i8::type_info().clone(),
                    (IntBitness::X8, false) => u8::type_info().clone(),
                    (IntBitness::X16, true) => i16::type_info().clone(),
                    (IntBitness::X16, false) => u16::type_info().clone(),
                    (IntBitness::X32, true) => i32::type_info().clone(),
                    (IntBitness::X32, false) => u32::type_info().clone(),
                    (IntBitness::X64, true) => i64::type_info().clone(),
                    (IntBitness::X64, false) => u64::type_info().clone(),
                    (IntBitness::X128, true) => i128::type_info().clone(),
                    (IntBitness::X128, false) => u128::type_info().clone(),
                    (IntBitness::Xsize, true) => isize::type_info().clone(),
                    (IntBitness::Xsize, false) => usize::type_info().clone(),
                }
            }
            TyKind::Float(float) => match float.resolve(&layout).bitness {
                codira_hir::FloatBitness::X32 => f32::type_info().clone(),
                codira_hir::FloatBitness::X64 => f64::type_info().clone(),
            },
            TyKind::Tuple(0, _) => <()>::type_info().clone(),
            TyKind::Array(element) => {
                let element = self.field_type(db, element)?;
                element.array_type()
            }
            TyKind::Struct(s, args) if s.data(db).memory_kind == HirMemoryKind::Gc => {
                let (payload, offsets) =
                    object_layout(db, ty).ok_or("a class field of an unsupported type")?;
                let field_types = mono::struct_field_types(db, ty);
                let mut fields = Vec::with_capacity(field_types.len());
                for ((field, field_ty), offset) in
                    s.fields(db).into_iter().zip(&field_types).zip(offsets)
                {
                    fields.push((
                        field.name(db).to_string(),
                        self.field_type(db, field_ty)?,
                        u16::try_from(offset).map_err(|_| "a class larger than 64 KiB")?,
                    ));
                }
                let name = mono::struct_name(db, *s, args);
                Type::new_struct(
                    name.clone(),
                    std_layout(&payload)?,
                    Guid::from_str(&format!("codira.jit {name}")),
                    fields,
                    StructMemoryKind::Gc,
                )
            }
            TyKind::Optional(payload) => {
                let optional = value_layout(db, ty).ok_or("an optional without a layout")?;
                let payload_offset = optional
                    .leaves
                    .get(1)
                    .map_or(optional.size, |leaf| leaf.offset);
                let fields = vec![
                    ("present".to_owned(), bool::type_info().clone(), 0),
                    (
                        "payload".to_owned(),
                        self.field_type(db, payload)?,
                        u16::try_from(payload_offset).map_err(|e| e.to_string())?,
                    ),
                ];
                let name = ty.guid_string(db).unwrap_or_else(|| "optional".to_owned());
                Type::new_struct(
                    name.clone(),
                    std_layout(&optional)?,
                    Guid::from_str(&format!("codira.jit {name}")),
                    fields,
                    StructMemoryKind::Value,
                )
            }
            TyKind::String => {
                let layout = string_layout(&layout);
                let usize = usize::type_info().clone();
                let fields = vec![
                    (
                        "buf".to_owned(),
                        u8::type_info().array_type(),
                        offset(&layout, 0)?,
                    ),
                    ("start".to_owned(), usize.clone(), offset(&layout, 1)?),
                    ("len".to_owned(), usize, offset(&layout, 2)?),
                ];
                Type::new_struct(
                    "String",
                    std_layout(&layout)?,
                    Guid::from_str("codira.jit String"),
                    fields,
                    StructMemoryKind::Value,
                )
            }
            _ => {
                return Err(format!(
                    "a heap value of type `{}`",
                    codira_hir::HirDisplay::display(ty, db)
                ))
            }
        };
        self.types.insert(ty.clone(), made.clone());
        Ok(made)
    }

    /// The descriptor of a value stored in a field or an element: a
    /// reference (a class instance, an array, a function value) is the
    /// opaque reference type; anything else its own descriptor.
    fn field_type(&mut self, db: &dyn HirDatabase, ty: &Ty) -> Result<Type, String> {
        match ty.interned() {
            TyKind::Array(_) | TyKind::Function(_) => Ok(self.reference.clone()),
            TyKind::Struct(s, _) if s.data(db).memory_kind == HirMemoryKind::Gc => {
                Ok(self.reference.clone())
            }
            _ => {
                if value_layout(db, ty).is_none() {
                    return Err(format!(
                        "a value of type `{}` on the heap",
                        codira_hir::HirDisplay::display(ty, db)
                    ));
                }
                self.memory_type(db, ty)
            }
        }
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        for raw in self.handles.drain(..) {
            // Safety: each came from `Type::into_raw` and is returned once.
            drop(unsafe { Type::from_raw(raw) });
        }
    }
}

fn std_layout(layout: &ValueLayout) -> Result<Layout, String> {
    Layout::from_size_align(
        usize::try_from(layout.size).map_err(|e| e.to_string())?,
        usize::try_from(layout.align).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// The offset of the `index`th leaf of `layout` (a `String`'s words).
fn offset(layout: &ValueLayout, index: usize) -> Result<u16, String> {
    u16::try_from(layout.leaves[index].offset).map_err(|e| e.to_string())
}
