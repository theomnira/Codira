//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{cell::RefCell, collections::HashMap, sync::Arc};

use codira_abi::Guid;
use codira_hir::{
    mono::{self, Instance},
    FloatBitness, HirDatabase, HirDisplay, IntBitness, ResolveBitness, Signedness, Ty, TyKind,
};
use inkwell::{
    context::Context,
    targets::TargetData,
    types::{
        AnyTypeEnum, BasicType, BasicTypeEnum, FloatType, FunctionType, IntType, PointerType,
        StructType,
    },
    AddressSpace,
};
use smallvec::SmallVec;

use crate::{
    ir::IsIrType,
    type_info::{HasStaticTypeId, TypeId, TypeIdData},
};

/// An object to cache and convert HIR types to Inkwell types.
pub struct HirTypeCache<'db, 'ink> {
    context: &'ink Context,
    db: &'db dyn HirDatabase,
    target_data: TargetData,
    types: RefCell<HashMap<codira_hir::TyKind, StructType<'ink>>>,
    array_ty_to_type_id: RefCell<HashMap<codira_hir::TyKind, Arc<TypeId>>>,
    tuple_ty_to_type_id: RefCell<HashMap<codira_hir::TyKind, Arc<TypeId>>>,
    /// Keyed by the whole struct type, arguments included: `Box[i32]` and
    /// `Box[f64]` are different types with different ids.
    struct_to_type_id: RefCell<HashMap<codira_hir::TyKind, Arc<TypeId>>>,
    /// Keyed by the whole enum type: `Result[i64, str]` and
    /// `Result[f64, str]` are different types with different ids.
    enum_to_type_id: RefCell<HashMap<codira_hir::TyKind, Arc<TypeId>>>,

    /// There is exactly one `str` type, so its id is a single cell rather
    /// than a map keyed by something that never varies.
    str_type_id: std::cell::OnceCell<Arc<TypeId>>,
}

impl<'db, 'ink> HirTypeCache<'db, 'ink> {
    /// Constructs a new `HirTypeContext`
    pub fn new(context: &'ink Context, db: &'db dyn HirDatabase, target_data: TargetData) -> Self {
        Self {
            context,
            db,
            target_data,
            types: RefCell::new(HashMap::default()),
            struct_to_type_id: RefCell::default(),
            enum_to_type_id: RefCell::default(),
            array_ty_to_type_id: RefCell::default(),
            tuple_ty_to_type_id: RefCell::default(),
            str_type_id: std::cell::OnceCell::new(),
        }
    }

    /// The LLVM context the types live in.
    pub fn context(&self) -> &'ink Context {
        self.context
    }

    /// Returns the type of the specified floating-point type
    pub fn get_float_type(&self, ty: codira_hir::FloatTy) -> FloatType<'ink> {
        match ty.bitness {
            FloatBitness::X64 => self.context.f64_type(),
            FloatBitness::X32 => self.context.f32_type(),
        }
    }

    /// Returns the type of the specified integer type
    pub fn get_int_type(&self, ty: codira_hir::IntTy) -> IntType<'ink> {
        match ty.bitness {
            IntBitness::X128 => self.context.i128_type(),
            IntBitness::X64 => self.context.i64_type(),
            IntBitness::X32 => self.context.i32_type(),
            IntBitness::X16 => self.context.i16_type(),
            IntBitness::X8 => self.context.i8_type(),
            IntBitness::Xsize => self.get_usize_type(),
        }
    }

    /// Returns the type for booleans
    pub fn get_bool_type(&self) -> IntType<'ink> {
        self.context.bool_type()
    }

    /// Returns the type for usize. The size of the type depends on the target
    /// architecture.
    pub fn get_usize_type(&self) -> IntType<'ink> {
        usize::ir_type(self.context, &self.target_data)
    }

    /// The target's data layout, for callers that need to size or align an
    /// arbitrary LLVM type rather than one of the fixed cases above -- e.g.
    /// lowering a `sizeof(T)`-shaped intrinsic for a `T` known only as an
    /// already-generated `BasicTypeEnum`.
    pub fn target_data(&self) -> &TargetData {
        &self.target_data
    }

    /// Returns the IR type of the specified struct type.
    ///
    /// `struct_ty` is the whole type, arguments included: an instantiated
    /// generic struct (`Box[i32]`) gets a layout of its own, built from its
    /// fields read through its arguments (see `codira_hir::mono`).
    pub fn get_struct_type(&self, struct_ty: &Ty) -> StructType<'ink> {
        // TODO: This assumes the contents of the codira_hir::Struct does not change. It
        // definitely does  between compilations. We have to have a way to
        // uniquely identify the `codira_hir::Struct` and  its contents.
        let ty = struct_ty.interned().clone();
        assert!(
            matches!(ty, TyKind::Struct(..)),
            "`get_struct_type` takes a struct type"
        );

        // Get the type from the cache
        if let Some(ir_ty) = self.types.borrow().get(&ty) {
            return *ir_ty;
        };

        // Opaquely construct the struct type and store it in the cache
        let ir_ty = self
            .context
            .opaque_struct_type(&struct_ty.display(self.db).to_string());
        self.types.borrow_mut().insert(ty, ir_ty);

        // Fill the struct members
        let field_types: Vec<_> = mono::struct_field_types(self.db, struct_ty)
            .into_iter()
            .map(|ty| {
                self.get_basic_type(&ty)
                    .expect("could not convert struct field to basic type")
            })
            .collect();
        ir_ty.set_body(&field_types, false);

        ir_ty
    }

    /// Returns the IR type of an enum: a named value struct of a `u32` tag
    /// and the payload slots of its layout (see `codira_hir::EnumLayout`),
    /// `{ i32, slot0, slot1, .. }`.
    ///
    /// `enum_ty` is the whole type, arguments included: an instantiated
    /// generic enum (`Result[i64, str]`) has a layout of its own.
    pub fn get_enum_type(&self, enum_ty: &Ty) -> StructType<'ink> {
        let ty = enum_ty.interned().clone();
        let TyKind::Enum(hir_enum, args) = &ty else {
            panic!("`get_enum_type` takes an enum type");
        };
        if let Some(ir_ty) = self.types.borrow().get(&ty) {
            return *ir_ty;
        }

        // Opaque first, like a struct, so a slot type that mentions this
        // enum (through a class, say) finds it in the cache.
        let ir_ty = self
            .context
            .opaque_struct_type(&mono::enum_name(self.db, *hir_enum, args));
        let layout = hir_enum.layout_at(self.db, args);
        self.types.borrow_mut().insert(ty, ir_ty);
        let field_types: Vec<BasicTypeEnum<'ink>> = std::iter::once(self.context.i32_type().into())
            .chain(layout.slots.iter().map(|slot| {
                self.get_basic_type(&slot.ty)
                    .expect("an enum payload should be a basic type")
            }))
            .collect();
        ir_ty.set_body(&field_types, false);
        ir_ty
    }

    /// Returns the IR type of the specified array type.
    pub fn get_array_type(&self, element_ty: &Ty) -> StructType<'ink> {
        // Get the type from the cache
        let ty = TyKind::Array(element_ty.clone());
        if let Some(ir_ty) = self.types.borrow().get(&ty) {
            return *ir_ty;
        };

        // Opaquely construct the array struct type and store it in the cache
        let ir_ty = self
            .context
            .opaque_struct_type(&format!("[{}]", element_ty.display(self.db)));
        self.types.borrow_mut().insert(ty, ir_ty);

        // Codira Arrays are represented as:
        //
        // ```c
        // struct Obj {
        //     ArrayValueT *value;
        //     ...
        // }
        //
        // struct ArrayValueT {
        //     usize_t len;
        //     usize_t capacity;
        //     T elements[capacity];
        // }
        // ```

        let length_ir_type = self.context.ptr_sized_int_type(&self.target_data, None);
        let capacity_ir_type = self.context.ptr_sized_int_type(&self.target_data, None);
        let element_ir_type = self
            .get_basic_type(element_ty)
            .expect("could not convert array element type to basic type");

        // Fill the struct members
        ir_ty.set_body(
            &[
                length_ir_type.into(),
                capacity_ir_type.into(),
                element_ir_type,
            ],
            false,
        );

        ir_ty
    }

    /// Returns the type of an array that should be used for variables. Arrays
    /// are always stored on the heap so this will always be a pointer to an
    /// Array<Ty>.
    pub fn get_array_reference_type(&self, element_ty: &Ty) -> PointerType<'ink> {
        // A pointer-to-pointer is still just `ptr` under opaque
        // pointers; the pointee chain carries no LLVM-level meaning.
        let _ = self.get_array_type(element_ty);
        self.context.ptr_type(AddressSpace::default())
    }

    /// Returns the type of the struct that should be used for variables.
    /// Depending on the memory type of the struct this is either a pointer
    /// to a `GCHandle` which holds a pointer to a struct, or, in case of a
    /// value struct, the struct type itself. Returns the type of the struct
    /// that should be used for variables.
    pub fn get_struct_reference_type(&self, struct_ty: &Ty) -> BasicTypeEnum<'ink> {
        let ir_ty = self.get_struct_type(struct_ty);
        let strukt = struct_ty.as_struct().expect("a struct type");
        match strukt.data(self.db).memory_kind {
            codira_hir::StructMemoryKind::Gc => {
                // GC values are pointers to pointers
                // struct Foo {}
                // Foo**
                self.context.ptr_type(AddressSpace::default()).into()
            }
            codira_hir::StructMemoryKind::Value => {
                // Value structs are passed as values
                ir_ty.into()
            }
        }
    }

    /// Returns the type of the struct that should be used in the public API. In
    /// the public API we don't deal with value types, only with pointers.
    pub fn get_public_struct_reference_type(&self, struct_ty: &Ty) -> BasicTypeEnum<'ink> {
        let ir_ty = self.get_struct_type(struct_ty);

        // GC values are pointers to pointers
        // struct Foo {}
        // Foo**
        //
        // Value structs are converted to GC types in the public API.
        let _ = ir_ty;
        self.context.ptr_type(AddressSpace::default()).into()
    }

    /// This function's LLVM parameter types in order: the `self` receiver
    /// first, when it has one, then the ordinary value parameters.
    ///
    /// A method's receiver is passed as parameter 0 -- the obvious ABI
    /// choice, and the one that lets `gen_call` stay positional for methods
    /// and free functions alike. It is not part of `FnSig::params()` (see
    /// `FunctionData::self_param`), so it has to be spliced in here rather
    /// than falling out of the signature.
    fn receiver_and_param_tys<'a>(
        &self,
        func: &Instance,
        sig: &'a codira_hir::FnSig,
    ) -> impl Iterator<Item = Ty> + use<'a> {
        func.self_param_ty(self.db)
            .into_iter()
            .chain(sig.params().iter().cloned())
    }

    /// Returns the type of the specified function definition
    ///
    /// A function with a `uses` clause takes one hidden evidence pointer per
    /// effect after its ordinary parameters (see `ir::body::effects`). Only
    /// this internal signature has them; the host-visible one
    /// (`get_public_function_type`) never does.
    pub fn get_function_type(&self, func: &Instance) -> FunctionType<'ink> {
        let ty = func.sig(self.db);
        let evidence = func.function().effects(self.db).len();
        let param_tys: Vec<_> = self
            .receiver_and_param_tys(func, &ty)
            .map(|p| {
                self.get_basic_type(&p)
                    .expect("could not convert function argument to basic type")
                    .into()
            })
            .chain(std::iter::repeat_n(
                self.context.ptr_type(AddressSpace::default()).into(),
                evidence,
            ))
            .collect();

        // A function returning `never` returns nothing: it is `void` in
        // LLVM, carries `noreturn` (see `ir::function::mark_noreturn`), and
        // every call to it is followed by `unreachable`.
        let return_type = ty.ret();
        match return_type.interned() {
            TyKind::Tuple(0, _) | TyKind::Never => {
                self.context.void_type().fn_type(&param_tys, false)
            }
            _ => self
                .get_basic_type(return_type)
                .expect("could not convert return value")
                .fn_type(&param_tys, false),
        }
    }

    /// Returns the type of a specified function definition that is callable
    /// from the outside of the Codira code. This function should be C ABI
    /// compatible.
    pub fn get_public_function_type(&self, func: &Instance) -> FunctionType<'ink> {
        let ty = func.sig(self.db);
        let param_tys: Vec<_> = self
            .receiver_and_param_tys(func, &ty)
            .map(|p| {
                self.get_public_basic_type(&p)
                    .expect("could not convert function argument to public basic type")
                    .into()
            })
            .collect();

        // `never` is `void` here too, as in `get_function_type`.
        let return_type = ty.ret();
        match return_type.interned() {
            TyKind::Tuple(0, _) | TyKind::Never => {
                self.context.void_type().fn_type(&param_tys, false)
            }
            _ => self
                .get_public_basic_type(return_type)
                .expect("could not convert return value")
                .fn_type(&param_tys, false),
        }
    }

    /// Returns the IR type of a string literal: `{ ptr, usize }`.
    ///
    /// A fat pointer rather than a bare `ptr` to a NUL-terminated buffer,
    /// so that taking a length is a field read rather than a scan, and so
    /// that a literal containing a NUL byte is still one string. The bytes
    /// themselves live in a constant global; this is only the handle.
    pub fn get_str_type(&self) -> StructType<'ink> {
        self.context.struct_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.get_usize_type().into(),
            ],
            false,
        )
    }

    /// Returns the IR type of a `String`: `{ ptr buffer, usize start, usize
    /// len }`, where `buffer` is a reference to a `[u8]` array object (see
    /// `codira_runtime`'s `string` module for what the three words mean).
    pub fn get_string_type(&self) -> StructType<'ink> {
        let _ = self.get_array_type(&TyKind::Int(codira_hir::IntTy::u8()).intern());
        self.context.struct_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.get_usize_type().into(),
                self.get_usize_type().into(),
            ],
            false,
        )
    }

    /// Returns the IR type of `T?`: `{ i1 present, T payload }`.
    ///
    /// No allocation and no pointer: an optional is a value exactly as large
    /// as its payload plus a flag, and a `nil` is the all-zero value -- so a
    /// payload is never undefined, and comparing two `nil`s' payloads is
    /// well defined.
    pub fn get_optional_type(&self, payload: &Ty) -> StructType<'ink> {
        self.context.struct_type(
            &[
                self.get_bool_type().into(),
                self.get_basic_type(payload)
                    .expect("an optional's payload should be a basic type"),
            ],
            false,
        )
    }

    /// Returns the inkwell type of the specified HIR type as a basic value. If
    /// the type cannot be represented as a basic type enum, `None` is
    /// returned.
    pub fn get_basic_type(&self, ty: &codira_hir::Ty) -> Option<BasicTypeEnum<'ink>> {
        match ty.interned() {
            TyKind::Optional(payload) => Some(self.get_optional_type(payload).into()),
            TyKind::Tuple(_, substs) => Some(self.get_tuple_type(substs).into()),
            TyKind::Float(float_ty) => Some(self.get_float_type(*float_ty).into()),
            TyKind::Int(int_ty) => Some(self.get_int_type(*int_ty).into()),
            TyKind::Struct(..) => Some(self.get_struct_reference_type(ty)),
            TyKind::Enum(..) => Some(self.get_enum_type(ty).into()),
            TyKind::Bool => Some(self.get_bool_type().into()),
            TyKind::Str => Some(self.get_str_type().into()),
            TyKind::String => Some(self.get_string_type().into()),
            TyKind::Array(element_ty) => Some(self.get_array_reference_type(element_ty).into()),
            // A runtime handle (see `ir::body::tasks`).
            TyKind::Channel(_) | TyKind::Session(..) => Some(self.get_usize_type().into()),
            // A reference to the function value's GC object (see
            // `ir::body::closures`).
            TyKind::Function(_) => Some(self.context.ptr_type(AddressSpace::default()).into()),
            _ => None,
        }
    }

    /// The LLVM type of the code a function value of type `fn_ty` calls:
    /// the function value itself (its GC handle) first, then the parameters,
    /// all in the internal representation (value structs by value).
    pub fn get_fn_value_code_type(&self, fn_ty: &Ty) -> FunctionType<'ink> {
        let (params, ret) = fn_ty
            .as_function()
            .expect("a function value has a function type");
        let param_tys: Vec<_> =
            std::iter::once(self.context.ptr_type(AddressSpace::default()).into())
                .chain(params.iter().map(|p| {
                    self.get_basic_type(p)
                        .expect("could not convert a function value's parameter to a basic type")
                        .into()
                }))
                .collect();
        match ret.interned() {
            TyKind::Tuple(0, _) | TyKind::Never => {
                self.context.void_type().fn_type(&param_tys, false)
            }
            _ => self
                .get_basic_type(ret)
                .expect("could not convert a function value's return type")
                .fn_type(&param_tys, false),
        }
    }

    /// Returns the inkwell type of the specified HIR type as a basic value that
    /// is usable from the public API. Internally this means that struct
    /// types are always pointers. If the type cannot be represented as a
    /// basic type enum, `None` is returned.
    pub fn get_public_basic_type(&self, ty: &codira_hir::Ty) -> Option<BasicTypeEnum<'ink>> {
        match ty.interned() {
            TyKind::Optional(payload) => Some(self.get_optional_type(payload).into()),
            TyKind::Tuple(_, substs) => Some(self.get_tuple_type(substs).into()),
            TyKind::Float(float_ty) => Some(self.get_float_type(*float_ty).into()),
            TyKind::Int(int_ty) => Some(self.get_int_type(*int_ty).into()),
            TyKind::Struct(..) => Some(self.get_public_struct_reference_type(ty)),
            // Passed by value even across the host boundary, like a tuple or
            // an optional.
            TyKind::Enum(..) => Some(self.get_enum_type(ty).into()),
            TyKind::Bool => Some(self.get_bool_type().into()),
            TyKind::Str => Some(self.get_str_type().into()),
            // Boxed on the GC heap like a value struct, so that the host and the
            // dispatch table pass one pointer (see `ir::body::strings`).
            TyKind::String => Some(self.context.ptr_type(AddressSpace::default()).into()),
            TyKind::Array(element_ty) => Some(self.get_array_reference_type(element_ty).into()),
            // A runtime handle (see `ir::body::tasks`).
            TyKind::Channel(_) | TyKind::Session(..) => Some(self.get_usize_type().into()),
            TyKind::Function(_) => Some(self.context.ptr_type(AddressSpace::default()).into()),
            _ => None,
        }
    }

    /// Returns the inkwell type of the specified HIR type. If the type cannot
    /// be represented as an inkwell type, `None` is returned.
    pub fn get_any_type(&self, ty: &codira_hir::Ty) -> Option<AnyTypeEnum<'ink>> {
        match ty.interned() {
            TyKind::Optional(payload) => Some(self.get_optional_type(payload).into()),
            TyKind::Tuple(_, substs) => Some(self.get_tuple_type(substs).into()),
            TyKind::Float(float_ty) => Some(self.get_float_type(*float_ty).into()),
            TyKind::Int(int_ty) => Some(self.get_int_type(*int_ty).into()),
            TyKind::Struct(..) => Some(self.get_struct_type(ty).into()),
            TyKind::Enum(..) => Some(self.get_enum_type(ty).into()),
            TyKind::FnDef(codira_hir::CallableDef::Function(fn_ty), type_params) => {
                if !type_params.is_empty() {
                    unimplemented!("cannot yet deal with type parameters in functions");
                }
                Some(self.get_function_type(&Instance::of(*fn_ty)).into())
            }
            TyKind::Bool => Some(self.get_bool_type().into()),
            TyKind::Str => Some(self.get_str_type().into()),
            TyKind::String => Some(self.get_string_type().into()),
            TyKind::Array(element_ty) => Some(self.get_array_reference_type(element_ty).into()),
            // A runtime handle (see `ir::body::tasks`).
            TyKind::Channel(_) | TyKind::Session(..) => Some(self.get_usize_type().into()),
            TyKind::Function(_) => Some(self.context.ptr_type(AddressSpace::default()).into()),
            _ => None,
        }
    }

    /// Returns the empty type
    pub fn get_empty_type(&self) -> StructType<'ink> {
        self.context.struct_type(&[], false)
    }

    /// Returns the type for a tuple
    pub fn get_tuple_type(&self, type_params: &[Ty]) -> StructType<'ink> {
        let mut tuple_ir_types: SmallVec<[BasicTypeEnum<'ink>; 2]> =
            SmallVec::with_capacity(type_params.len());
        for ty in type_params {
            tuple_ir_types.push(
                self.get_basic_type(ty)
                    .expect("tuple type should be a basic type"),
            );
        }
        self.context.struct_type(&tuple_ir_types, false)
    }

    /// The `TypeInfo` of a function's result in its reflected signature:
    /// `()` for unit and for `never`, which are both `void` in LLVM, so the
    /// signature a caller expects (`ir::dispatch_table`) and the one a
    /// definition -- compiled, or registered by the runtime -- declares
    /// agree. That `never` does not return is in the code (`noreturn`,
    /// `unreachable` after each call), not in the signature check.
    pub fn ret_type_id(&self, ret: &Ty) -> Arc<TypeId> {
        if ret.is_empty() || ret.is_never() {
            <()>::type_id().clone()
        } else {
            self.type_id(ret)
        }
    }

    /// Returns a `TypeInfo` for the specified `ty`
    pub fn type_id(&self, ty: &Ty) -> Arc<TypeId> {
        match ty.interned() {
            &TyKind::Float(ty) => {
                let resolved_ty = ty.resolve(&self.db.target_data_layout());
                match resolved_ty.bitness {
                    FloatBitness::X32 => f32::type_id().clone(),
                    FloatBitness::X64 => f64::type_id().clone(),
                }
            }
            &TyKind::Int(ty) => {
                let resolved_ty = ty.resolve(&self.db.target_data_layout());
                match (resolved_ty.signedness, resolved_ty.bitness) {
                    (Signedness::Signed, IntBitness::X8) => i8::type_id().clone(),
                    (Signedness::Signed, IntBitness::X16) => i16::type_id().clone(),
                    (Signedness::Signed, IntBitness::X32) => i32::type_id().clone(),
                    (Signedness::Signed, IntBitness::X64) => i64::type_id().clone(),
                    (Signedness::Signed, IntBitness::X128) => i128::type_id().clone(),
                    (Signedness::Unsigned, IntBitness::X8) => u8::type_id().clone(),
                    (Signedness::Unsigned, IntBitness::X16) => u16::type_id().clone(),
                    (Signedness::Unsigned, IntBitness::X32) => u32::type_id().clone(),
                    (Signedness::Unsigned, IntBitness::X64) => u64::type_id().clone(),
                    (Signedness::Unsigned, IntBitness::X128) => u128::type_id().clone(),
                    (_, IntBitness::Xsize) => unreachable!(
                        "after resolve there should no longer be an undefined size type"
                    ),
                }
            }
            TyKind::Bool => bool::type_id().clone(),
            // A channel is an opaque runtime handle. It gets an id of its own
            // so a signature mentioning one still has reflection metadata;
            // the host has no way to make one.
            TyKind::Channel(_) | TyKind::Session(..) => Arc::new(TypeId {
                data: TypeIdData::Concrete(Guid::from_str(
                    &ty.guid_string(self.db)
                        .expect("a channel's element type, or a session, has a guid string"),
                )),
                name: ty.display(self.db).to_string(),
            }),
            // Structural, like the tuple case below, and for the same
            // reason: to the runtime a `str` is an anonymous value struct
            // of a pointer and a length. There is exactly one `str`, so the
            // id is built once from a constant name rather than cached per
            // instantiation.
            TyKind::Str => self
                .str_type_id
                .get_or_init(|| {
                    let name = ty
                        .guid_string(self.db)
                        .expect("`str` has a constant guid string");
                    Arc::new(TypeId {
                        data: TypeIdData::Concrete(Guid::from_str(&name)),
                        name,
                    })
                })
                .clone(),
            TyKind::Struct(s, args) => {
                if let Some(id) = self.struct_to_type_id.borrow().get(ty.interned()) {
                    return id.clone();
                }
                let id = Arc::new(TypeId {
                    name: mono::struct_name(self.db, *s, args),
                    data: TypeIdData::Concrete(guid_from_struct(self.db, ty)),
                });
                self.struct_to_type_id
                    .borrow_mut()
                    .insert(ty.interned().clone(), id.clone());
                id
            }
            // Named like a struct (hot reloading matches types by name), and
            // identified by the GUID of its layout, so a re-laid-out enum is
            // a changed type.
            TyKind::Enum(e, args) => self
                .enum_to_type_id
                .borrow_mut()
                .entry(ty.interned().clone())
                .or_insert_with(|| {
                    Arc::new(TypeId {
                        name: mono::enum_name(self.db, *e, args),

                        data: TypeIdData::Concrete(Guid::from_str(
                            &ty.guid_string(self.db)
                                .expect("an enum of representable payloads has a guid string"),
                        )),
                    })
                })
                .clone(),
            TyKind::Array(a) => {
                {
                    let read_only = self.array_ty_to_type_id.borrow();
                    if let Some(a) = read_only.get(a.interned()) {
                        return a.clone();
                    }
                }

                let element_type_id = self.type_id(a);
                let array_type_id = Arc::new(TypeId {
                    name: format!("[{}]", element_type_id.name),
                    data: TypeIdData::Array(element_type_id),
                });

                let previous_entry = self
                    .array_ty_to_type_id
                    .borrow_mut()
                    .insert(a.interned().clone(), array_type_id.clone());
                assert!(previous_entry.is_none(), "array cyclic reference?");

                array_type_id
            }
            // Structural like a tuple: one id per signature, hashed from the
            // `core::fn(..) -> R` guid string. To the runtime it is a garbage
            // collected struct whose one field locates the code; every
            // function value object starts with that field (see
            // `ir::closures`).
            TyKind::Function(_) => {
                {
                    let read_only = self.tuple_ty_to_type_id.borrow();
                    if let Some(id) = read_only.get(ty.interned()) {
                        return id.clone();
                    }
                }
                let name = ty
                    .guid_string(self.db)
                    .expect("a function type of representable types has a guid string");
                let fn_type_id = Arc::new(TypeId {
                    data: TypeIdData::Concrete(Guid::from_str(&name)),
                    name,
                });
                self.tuple_ty_to_type_id
                    .borrow_mut()
                    .insert(ty.interned().clone(), fn_type_id.clone());
                fn_type_id
            }
            // A tuple's identity is structural, so -- unlike an array, which
            // gets a dedicated `TypeIdData::Array` wrapper around its element
            // -- it is `Concrete`, hashed from the same `(A, B)` name
            // `guid_string` produces. That keeps the ABI stable without
            // adding a tuple case to every consumer of `TypeIdData`: a tuple
            // is, to the runtime, just an anonymous value struct.
            // An optional is structural too, and identified the same way.
            // `String` is a value struct to the runtime (see
            // `code_gen::symbols`), identified by its constant name.
            TyKind::Tuple(..) | TyKind::Optional(_) | TyKind::String => {
                {
                    let read_only = self.tuple_ty_to_type_id.borrow();
                    if let Some(id) = read_only.get(ty.interned()) {
                        return id.clone();
                    }
                }

                let name = ty
                    .guid_string(self.db)
                    .expect("a tuple or optional of representable types has a guid string");
                let tuple_type_id = Arc::new(TypeId {
                    data: TypeIdData::Concrete(Guid::from_str(&name)),
                    name,
                });

                self.tuple_ty_to_type_id
                    .borrow_mut()
                    .insert(ty.interned().clone(), tuple_type_id.clone());

                tuple_type_id
            }
            _ => unimplemented!("{} unhandled", ty.display(self.db)),
        }
    }
}

/// The GUID of the struct type `ty`: its name -- with its arguments, for an
/// instantiated generic struct -- and the name and type of every field, read
/// through those arguments. A change to the layout of `Box[i32]` changes the
/// GUID of `Box[i32]`, which is what tells hot reloading to migrate its
/// objects.
pub fn guid_from_struct(db: &dyn HirDatabase, ty: &Ty) -> Guid {
    let TyKind::Struct(s, args) = ty.interned() else {
        panic!("`guid_from_struct` takes a struct type");
    };
    let name = mono::struct_name(db, *s, args);
    let fields: Vec<String> = s
        .fields(db)
        .into_iter()
        .zip(mono::struct_field_types(db, ty))
        .map(|(f, field_ty)| {
            let ty_string = field_ty
                .guid_string(db)
                .expect("type should be convertible to a string");
            format!("{}: {}", f.name(db), ty_string)
        })
        .collect();

    Guid::from_str(&format!(
        "struct {name}{{{fields}}}",
        name = name,
        fields = fields.join(",")
    ))
}
