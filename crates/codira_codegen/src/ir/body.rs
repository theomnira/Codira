//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{collections::HashMap, sync::Arc};

use codira_abi as abi;
use codira_hir::{
    ArithOp, BinaryOp, Body, CmpOp, Expr, ExprId, HirDatabase, HirDisplay, InferenceResult,
    Literal, LogicOp, Name, Ordering, Pat, PatId, Path, ResolveBitness, Resolver, Statement, Ty,
    TyKind, UnaryOp, ValueNs,
};
use inkwell::{
    basic_block::BasicBlock,
    builder::Builder,
    context::Context,
    values::{
        AggregateValueEnum, BasicMetadataValueEnum, BasicValueEnum, FloatValue, FunctionValue,
        GlobalValue, IntValue, PointerValue, StructValue,
    },
    AddressSpace, FloatPredicate, IntPredicate,
};

use crate::{
    intrinsics,
    ir::{
        dispatch_table::DispatchTable, intrinsic_ops, ty::HirTypeCache, type_table::TypeTable,
        RuntimeArrayValue, RuntimeReferenceValue,
    },
    module_group::ModuleGroup,
    value::Global,
};

type BreakSources<'ink> = Vec<Option<(BasicValueEnum<'ink>, BasicBlock<'ink>)>>;

struct LoopInfo<'ink> {
    break_values: BreakSources<'ink>,
    exit_block: BasicBlock<'ink>,
}

#[derive(Clone)]
pub(crate) struct ExternalGlobals<'ink> {
    pub alloc_handle: Option<GlobalValue<'ink>>,
    pub dispatch_table: Option<GlobalValue<'ink>>,
    pub type_table: Option<Global<'ink, [*const std::ffi::c_void]>>,
}

pub(crate) struct BodyIrGenerator<'db, 'ink, 't> {
    context: &'ink Context,
    /// The module being built. Needed to *declare* LLVM intrinsics --
    /// saturating float-to-int casts call `llvm.fpto{s,u}i.sat`, which
    /// must be declared in the module before it can be referenced.
    module: &'t inkwell::module::Module<'ink>,
    db: &'db dyn HirDatabase,
    body: Arc<Body>,
    infer: Arc<InferenceResult>,
    builder: Builder<'ink>,
    fn_value: FunctionValue<'ink>,
    pat_to_param: HashMap<PatId, inkwell::values::BasicValueEnum<'ink>>,
    pat_to_local: HashMap<PatId, inkwell::values::PointerValue<'ink>>,
    pat_to_name: HashMap<PatId, String>,
    function_map: &'t HashMap<codira_hir::Function, FunctionValue<'ink>>,
    dispatch_table: &'t DispatchTable<'ink>,
    type_table: &'t TypeTable<'ink>,
    hir_types: &'t HirTypeCache<'db, 'ink>,
    active_loop: Option<LoopInfo<'ink>>,
    hir_function: codira_hir::Function,
    external_globals: ExternalGlobals<'ink>,
    module_group: &'t ModuleGroup,
}

impl<'db, 'ink, 't> BodyIrGenerator<'db, 'ink, 't> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        context: &'ink Context,
        module: &'t inkwell::module::Module<'ink>,
        db: &'db dyn HirDatabase,
        function: (codira_hir::Function, FunctionValue<'ink>),
        function_map: &'t HashMap<codira_hir::Function, FunctionValue<'ink>>,
        dispatch_table: &'t DispatchTable<'ink>,
        type_table: &'t TypeTable<'ink>,
        external_globals: ExternalGlobals<'ink>,
        hir_types: &'t HirTypeCache<'db, 'ink>,
        module_group: &'t ModuleGroup,
    ) -> Self {
        let (hir_function, ir_function) = function;

        // Get the type information from the `codira_hir::Function`
        let body = hir_function.body(db);
        let infer = hir_function.infer(db);

        // Construct a builder for the IR function
        let builder = context.create_builder();
        let body_ir = context.append_basic_block(ir_function, "body");
        builder.position_at_end(body_ir);

        BodyIrGenerator {
            context,
            module,
            db,
            body,
            infer,
            builder,
            fn_value: ir_function,
            pat_to_param: HashMap::default(),
            pat_to_local: HashMap::default(),
            pat_to_name: HashMap::default(),
            function_map,
            dispatch_table,
            type_table,
            active_loop: None,
            hir_function,
            external_globals,
            hir_types,
            module_group,
        }
    }

    /// Generates IR for the body of the function.
    pub fn gen_fn_body(&mut self) {
        // Eidos fast path: if the whole body reduces to a compile-time
        // constant (after inlining, unrolling, equality saturation and
        // interpretation -- see `crate::eidos_fold`), emit just that
        // constant. Declining is always safe: the ordinary lowering
        // below produces correct code either way.
        if let Some(value) =
            crate::eidos_fold::fold_to_constant(self.db, self.hir_function, self.fn_value)
        {
            self.builder
                .build_return(Some(&value))
                .expect("failed to build return for a constant-folded function");
            return;
        }

        // Iterate over all parameters and their type and store them so we can reference
        // them later in code.
        //
        // The `self` receiver, when present, is LLVM parameter 0 (see
        // `HirTypeCache::receiver_and_param_tys`), so the value parameters
        // start one slot later. `self` is bound through exactly the same
        // alloca-and-store path as any other parameter -- `Body` gives it an
        // ordinary `Pat::Bind { name: self }` -- so nothing downstream has to
        // treat a method's receiver specially.
        let self_param = self.body.self_param().copied();

        // Collected up front so the loop body is free to take `&mut self`
        // (the destructuring case below does).
        let param_pats: Vec<PatId> = self_param
            .iter()
            .chain(self.body.params().iter())
            .map(|(pat, _ty)| *pat)
            .collect();

        for (i, pat) in param_pats.iter().enumerate() {
            let body = self.body.clone(); // Avoid borrow issues

            match &body[*pat] {
                Pat::Bind { name } => {
                    let name = name.to_string();
                    let param = self.fn_value.get_nth_param(i as u32).unwrap();
                    let builder = self.new_alloca_builder();
                    let param_ptr = builder
                        .build_alloca(param.get_type(), &name)
                        .expect("failed to build alloca for parameter");
                    builder
                        .build_store(param_ptr, param)
                        .expect("failed to build store for parameter");
                    self.pat_to_local.insert(*pat, param_ptr);
                    self.pat_to_name.insert(*pat, name);
                }
                Pat::Wild => {
                    // Wildcard patterns cannot be referenced from code. So
                    // nothing to do.
                }
                // `func f((a, b): (i32, i32))` -- the parameter arrives as
                // one aggregate and is destructured into its bindings,
                // exactly as a `let (a, b) = ..` would be. The tuple itself
                // never needs an `alloca`; only the named bindings do.
                Pat::Tuple(_) => {
                    let param = self
                        .fn_value
                        .get_nth_param(i as u32)
                        .expect("every parameter pattern has a matching LLVM parameter");
                    self.bind_pattern_to_value(*pat, param);
                }
                Pat::Path(_) => unreachable!(
                    "Path patterns are not supported as parameters, are we missing a diagnostic?"
                ),
                Pat::Literal(_) | Pat::TupleStruct { .. } => unreachable!(
                    "refutable patterns are not supported as parameters, are we missing a \
                     diagnostic?"
                ),
                Pat::Missing => unreachable!(
                    "found missing Pattern, should not be generating IR for incomplete code"
                ),
            }
        }

        // Generate code for the body of the function
        let ret_value = self.gen_expr(self.body.body_expr());

        // Construct a return statement from the returned value of the body if a return
        // is expected in the first place. If the return type of the body is
        // `never` there is no need to generate a return statement.
        let block_ret_type = &self.infer[self.body.body_expr()];
        let fn_ret_type = self
            .hir_function
            .ty(self.db)
            .callable_sig(self.db)
            .unwrap()
            .ret()
            .clone();
        if !block_ret_type.is_never() {
            if fn_ret_type.is_empty() {
                self.builder
                    .build_return(None)
                    .expect("failed to build return");
            } else if let Some(value) = ret_value {
                self.builder
                    .build_return(Some(&value))
                    .expect("failed to build return");
            }
        }
    }

    pub fn gen_fn_wrapper(&mut self) {
        let fn_sig = self.hir_function.ty(self.db).callable_sig(self.db).unwrap();
        let args: Vec<BasicMetadataValueEnum<'_>> = fn_sig
            .params()
            .iter()
            .enumerate()
            .map(|(idx, ty)| {
                let param = self.fn_value.get_nth_param(idx as u32).unwrap();
                if let Some(s) = ty.as_struct() {
                    if s.data(self.db).memory_kind == abi::StructMemoryKind::Value {
                        deref_heap_value(&self.builder, param, self.hir_types.get_struct_type(s))
                    } else {
                        param
                    }
                } else {
                    param
                }
                .into()
            })
            .collect();

        let ret_value = self.gen_call(self.hir_function, &args);

        let call_return_type = &self.infer[self.body.body_expr()];
        if !call_return_type.is_never() {
            let fn_ret_type = self
                .hir_function
                .ty(self.db)
                .callable_sig(self.db)
                .unwrap()
                .ret()
                .clone();

            if fn_ret_type.is_empty() {
                self.builder
                    .build_return(None)
                    .expect("failed to build return");
            } else if let Some(value) = ret_value {
                let ret_value = if let Some(hir_struct) = fn_ret_type.as_struct() {
                    if hir_struct.data(self.db).memory_kind == codira_hir::StructMemoryKind::Value {
                        self.gen_struct_alloc_on_heap(hir_struct, value.into_struct_value())
                    } else {
                        value
                    }
                } else {
                    value
                };
                self.builder
                    .build_return(Some(&ret_value))
                    .expect("failed to build return");
            }
        }
    }

    /// Generates IR for the specified expression. Dependending on the type of
    /// expression an IR value is returned.
    fn gen_expr(&mut self, expr: ExprId) -> Option<inkwell::values::BasicValueEnum<'ink>> {
        let body = self.body.clone();
        match &body[expr] {
            Expr::Block {
                ref statements,
                tail,
            } => self.gen_block(expr, statements, *tail),
            Expr::Path(ref p) => {
                let resolver = codira_hir::resolver_for_expr(self.db, self.body.owner(), expr);
                Some(self.gen_path_expr(p, expr, &resolver))
            }
            Expr::Literal(lit) => Some(self.gen_literal(lit, expr)),
            Expr::RecordLit { fields, .. } => Some(self.gen_record_lit(expr, fields)),
            Expr::BinaryOp { lhs, rhs, op } => {
                self.gen_binary_op(expr, *lhs, *rhs, op.expect("missing op"))
            }
            Expr::UnaryOp { expr, op } => self.gen_unary_op(*expr, *op),
            Expr::MethodCall {
                receiver, ref args, ..
            } => self.gen_method_call(expr, *receiver, args),
            Expr::Call {
                ref callee,
                ref args,
            } => {
                // Get the callable definition from the map
                match self.infer[*callee].as_callable_def() {
                    Some(codira_hir::CallableDef::Function(def)) => {
                        // Get all the arguments
                        let args: Vec<BasicMetadataValueEnum<'_>> = args
                            .iter()
                            .map(|expr| self.gen_expr(*expr).expect("expected a value").into())
                            .collect();

                        // An `extern "codira-intrinsic"` callee has no symbol
                        // to call: it names an operation the compiler emits
                        // here, in place. See `intrinsic_ops::gen_intrinsic`.
                        if def.is_intrinsic(self.db) {
                            return self.gen_intrinsic_call(expr, def, &args);
                        }

                        self.gen_call(def, &args)
                            // If the called function is a void function it doesn't return anything.
                            // If this method (`gen_expr`) returns None we assume the return value
                            // is `never`. We return a const unit struct here to ensure that at
                            // least something is returned. This matches with the codira_hir where a
                            // `nothing` is returned instead of a `never`.
                            //
                            // This unit value will also be optimized out.
                            .or_else(|| match self.infer[expr].interned() {
                                TyKind::Never => None,
                                _ => Some(self.context.const_struct(&[], false).into()),
                            })
                    }
                    Some(codira_hir::CallableDef::Struct(_)) => {
                        Some(self.gen_named_tuple_lit(expr, args))
                    }
                    None => panic!("expected a callable expression"),
                }
            }
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => self.gen_if(expr, *condition, *then_branch, *else_branch),
            Expr::Return { expr: ret_expr } => self.gen_return(expr, *ret_expr),
            Expr::Loop { body } => self.gen_loop(expr, *body),
            Expr::While { condition, body } => self.gen_while(expr, *condition, *body),
            Expr::Break { expr: break_expr } => self.gen_break(expr, *break_expr),
            Expr::Field {
                expr: receiver_expr,
                name,
            } => self.gen_field(expr, *receiver_expr, name),
            Expr::Array(exprs) => self.gen_array(expr, exprs).map(Into::into),
            Expr::Tuple(exprs) => self.gen_tuple(expr, exprs),
            Expr::Index { base, index } => self.gen_index(expr, *base, *index),
            Expr::Cast { expr: operand, .. } => self.gen_cast(expr, *operand),
            Expr::Missing => {
                unimplemented!("unimplemented expr type {:?}", &body[expr])
            }
        }
    }

    /// Generates IR for an `as` cast.
    ///
    /// The choice of machine operation is delegated to
    /// [`codira_hir::check_cast`] -- the *same* function
    /// `codira_hir::mir_lower` consults when it selects an
    /// `OpKind::Cast`. Duplicating the matrix here would let the two
    /// lowering paths disagree about what `as` means, which is exactly
    /// the class of divergence a single shared decision function exists
    /// to prevent.
    ///
    /// See `spec/LANGUAGE_SPEC.md` section 18 for the normative semantics:
    /// integer conversions wrap, and float-to-integer conversions
    /// saturate (with NaN mapping to zero), which is why the latter lower
    /// to LLVM's `llvm.fpto{s,u}i.sat` intrinsics rather than the raw
    /// `fptosi`/`fptoui` instructions -- those are *poison* out of range.
    fn gen_cast(
        &mut self,
        cast_expr: ExprId,
        operand_expr: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        use codira_hir::{check_cast, CastCheck, CastOp};

        let value = self.gen_expr(operand_expr)?;

        let source = self.infer[operand_expr].clone();
        let target = self.infer[cast_expr].clone();
        let layout = self.db.target_data_layout();

        match check_cast(&source, &target, &layout) {
            // Same machine representation: the cast is a no-op.
            CastCheck::Legal(CastOp::Identity) => Some(value),
            CastCheck::Legal(CastOp::Convert(kind, mode)) => {
                // A `Checked` cast carries an undischarged proof
                // obligation (refinement types, milestone M7). `as` never
                // produces one today; if that changes, emitting it as a
                // silent wrapping conversion would be precisely the
                // miscompile the mode exists to prevent.
                if !matches!(mode, codira_mir::CastMode::Wrapping) {
                    unimplemented!("checked casts require refinement checking (M7)");
                }
                let target_ir = self.hir_types.get_basic_type(&target)?;
                self.build_cast(kind, value, target_ir)
            }
            // An illegal cast is a type error that inference already
            // reported; code generation only runs on bodies that
            // type-checked, so reaching here means a diagnostic was
            // missed upstream.
            CastCheck::Illegal(_) | CastCheck::Undetermined => {
                unreachable!("cast that failed type checking reached code generation")
            }
        }
    }

    /// Emits the LLVM instruction for one cast kind.
    fn build_cast(
        &self,
        kind: codira_mir::CastKind,
        source: BasicValueEnum<'ink>,
        target: inkwell::types::BasicTypeEnum<'ink>,
    ) -> Option<BasicValueEnum<'ink>> {
        use codira_mir::CastKind as K;
        let b = &self.builder;
        Some(match kind {
            K::Trunc => b
                .build_int_truncate(source.into_int_value(), target.into_int_type(), "trunc")
                .ok()?
                .into(),
            K::Zext => b
                .build_int_z_extend(source.into_int_value(), target.into_int_type(), "zext")
                .ok()?
                .into(),
            K::Sext => b
                .build_int_s_extend(source.into_int_value(), target.into_int_type(), "sext")
                .ok()?
                .into(),
            K::FpTrunc => b
                .build_float_trunc(
                    source.into_float_value(),
                    target.into_float_type(),
                    "fptrunc",
                )
                .ok()?
                .into(),
            K::FpExt => b
                .build_float_ext(source.into_float_value(), target.into_float_type(), "fpext")
                .ok()?
                .into(),
            K::SiToFp => b
                .build_signed_int_to_float(
                    source.into_int_value(),
                    target.into_float_type(),
                    "sitofp",
                )
                .ok()?
                .into(),
            K::UiToFp => b
                .build_unsigned_int_to_float(
                    source.into_int_value(),
                    target.into_float_type(),
                    "uitofp",
                )
                .ok()?
                .into(),
            K::FpToSi | K::FpToUi => self.build_saturating_fp_to_int(
                source.into_float_value(),
                target.into_int_type(),
                matches!(kind, K::FpToSi),
            )?,
            K::Bitcast => b.build_bit_cast(source, target, "bitcast").ok()?,
        })
    }

    /// Saturating float-to-integer conversion via `llvm.fpto{s,u}i.sat`.
    ///
    /// See `spec/LANGUAGE_SPEC.md` section 18.3: out-of-range saturates and
    /// NaN maps to zero. The raw `fptosi`/`fptoui` instructions are poison
    /// out of range, and poison propagates through `select`, so clamping
    /// after the fact would be unsound rather than merely slower.
    fn build_saturating_fp_to_int(
        &self,
        source: FloatValue<'ink>,
        target: inkwell::types::IntType<'ink>,
        signed: bool,
    ) -> Option<BasicValueEnum<'ink>> {
        let module = self.module;
        let int_bits = target.get_bit_width();
        let float_bits = if source.get_type() == self.context.f32_type() {
            32
        } else {
            64
        };
        let name = format!(
            "llvm.fpto{}i.sat.i{int_bits}.f{float_bits}",
            if signed { 's' } else { 'u' }
        );
        let intrinsic = inkwell::intrinsics::Intrinsic::find(&name)?;
        let declaration =
            intrinsic.get_declaration(module, &[target.into(), source.get_type().into()])?;
        self.builder
            .build_call(declaration, &[source.into()], "fptoint_sat")
            .ok()?
            .try_as_basic_value()
            .basic()
    }

    /// Generates an IR value that represents the given `Literal`.
    fn gen_literal(&mut self, lit: &Literal, expr: ExprId) -> BasicValueEnum<'ink> {
        match lit {
            Literal::Int(v) => {
                let ty = match &self.infer[expr].interned() {
                    TyKind::Int(int_ty) => int_ty,
                    _ => unreachable!(
                        "cannot construct an IR value for anything but an integral type"
                    ),
                };

                let context = self.context;
                let ir_ty = match ty.resolve(&self.db.target_data_layout()).bitness {
                    codira_hir::IntBitness::X8 => {
                        context.i8_type().const_int(v.value as u64, false)
                    }
                    codira_hir::IntBitness::X16 => {
                        context.i16_type().const_int(v.value as u64, false)
                    }
                    codira_hir::IntBitness::X32 => {
                        context.i32_type().const_int(v.value as u64, false)
                    }
                    codira_hir::IntBitness::X64 => {
                        context.i64_type().const_int(v.value as u64, false)
                    }
                    codira_hir::IntBitness::X128 => {
                        context.i128_type().const_int_arbitrary_precision(&unsafe {
                            std::mem::transmute::<u128, [u64; 2]>(v.value)
                        })
                    }
                    codira_hir::IntBitness::Xsize => {
                        unreachable!("unresolved bitness in code generation")
                    }
                };

                ir_ty.into()
            }

            Literal::Float(v) => {
                let ty = &self.infer[expr];
                let ty = match ty.interned()  {
                    TyKind::Float(float_ty) => float_ty,
                    _ => unreachable!("cannot construct an IR value for anything but a float type (literal type: {})", ty.display(self.db)),
                };

                let context = self.context;
                let ir_ty = match ty.bitness.resolve(&self.db.target_data_layout()) {
                    codira_hir::FloatBitness::X32 => context.f32_type().const_float(v.value),
                    codira_hir::FloatBitness::X64 => context.f64_type().const_float(v.value),
                };

                ir_ty.into()
            }

            Literal::Bool(value) => {
                let ty = self.context.bool_type();
                if *value {
                    ty.const_all_ones().into()
                } else {
                    ty.const_zero().into()
                }
            }

            Literal::String(value) => self.gen_string_literal(value),

            Literal::Nil => unimplemented!(
                "`nil`/optional codegen is not implemented yet -- `Type?` currently lowers \
                 transparently to `Type` in the type system (see type_ref.rs), so there is no \
                 IR representation for the absent case yet"
            ),
        }
    }

    /// Emits a string literal as `{ ptr, usize }` over constant bytes.
    ///
    /// The bytes go into a private, constant, `unnamed_addr` global: private
    /// because nothing outside this module can name it, constant because a
    /// literal cannot be written to, and `unnamed_addr` because the address
    /// itself carries no meaning -- which is what lets the linker merge two
    /// modules that both contain `"hello"` into one copy.
    ///
    /// The value is a compile-time constant struct, so a literal costs no
    /// instructions at all: it is materialised where it is used.
    ///
    /// A trailing NUL is appended but *not* counted in the length. Nothing
    /// in Codira needs it -- the length is right there -- but it means the
    /// pointer can be handed to a C function expecting a `const char *`
    /// without copying, which is the whole reason an FFI-oriented language
    /// would pay the extra byte.
    fn gen_string_literal(&mut self, value: &str) -> BasicValueEnum<'ink> {
        let bytes = self.context.const_string(value.as_bytes(), true);

        let global = self.module.add_global(
            bytes.get_type(),
            Some(AddressSpace::default()),
            "codira.str",
        );
        global.set_initializer(&bytes);
        global.set_constant(true);
        global.set_unnamed_addr(true);
        global.set_linkage(inkwell::module::Linkage::Private);

        let length = self
            .hir_types
            .get_usize_type()
            .const_int(value.len() as u64, false);

        self.context
            .const_struct(&[global.as_pointer_value().into(), length.into()], false)
            .into()
    }

    /// Constructs an empty struct value e.g. `{}`
    fn gen_empty(&mut self) -> BasicValueEnum<'ink> {
        self.context.const_struct(&[], false).into()
    }

    /// Allocate a struct literal either on the stack or the heap based on the
    /// type of the struct.
    fn gen_struct_alloc(
        &mut self,
        hir_struct: codira_hir::Struct,
        args: Vec<BasicValueEnum<'ink>>,
    ) -> BasicValueEnum<'ink> {
        // Construct the struct literal
        let struct_ty = self.hir_types.get_struct_type(hir_struct);
        let mut value: AggregateValueEnum<'_> = struct_ty.get_undef().into();
        for (i, arg) in args.into_iter().enumerate() {
            value = self
                .builder
                .build_insert_value(value, arg, i as u32, "init")
                .expect("Failed to initialize struct field.");
        }
        let struct_lit = value.into_struct_value();

        match hir_struct.data(self.db).memory_kind {
            codira_hir::StructMemoryKind::Value => struct_lit.into(),
            codira_hir::StructMemoryKind::Gc => {
                // TODO: Root memory in GC
                self.gen_struct_alloc_on_heap(hir_struct, struct_lit)
            }
        }
    }

    fn gen_struct_alloc_on_heap(
        &mut self,
        hir_struct: codira_hir::Struct,
        struct_lit: StructValue<'_>,
    ) -> BasicValueEnum<'ink> {
        let struct_ir_ty = self.hir_types.get_struct_type(hir_struct);
        let (new_fn_ty, new_fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            &intrinsics::new,
        );

        // Under opaque pointers every pointer is the same untyped `ptr`, so
        // the bitcast this used to do (retype for the `new` intrinsic's
        // `i8*` parameter) is a no-op -- `type_info_ptr` is passed as-is.
        let type_info_ptr = self.type_table.gen_type_info_lookup(
            self.context,
            &self.builder,
            &self.hir_types.type_id(&hir_struct.ty(self.db)),
            self.external_globals.type_table,
        );

        let allocator_handle = self.get_allocator_handle_ptr();

        // Safety: we can be sure that the new intrinsic returns a reference.
        let untyped_reference = self
            .builder
            .build_indirect_call(
                new_fn_ty,
                new_fn_ptr,
                &[type_info_ptr.into(), allocator_handle.into()],
                "ref",
            )
            .expect("failed to build call to `new` intrinsic")
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // Under opaque pointers, `untyped_reference` needs no further cast
        // to be treated as `**StructTy` -- see note above.
        let reference = RuntimeReferenceValue::from_ptr(untyped_reference, struct_ir_ty)
            .expect("unable to construct codira reference type");

        // Store the struct value
        let struct_ptr = reference.get_data_ptr(&self.builder);
        self.builder
            .build_store(struct_ptr, struct_lit)
            .expect("failed to build store for struct literal");

        reference.into()
    }

    /// Generates IR for a record literal, e.g. `Foo { a: 1.23, b: 4 }`
    fn gen_record_lit(
        &mut self,
        type_expr: ExprId,
        fields: &[codira_hir::RecordLitField],
    ) -> BasicValueEnum<'ink> {
        let struct_ty = self.infer[type_expr].clone();
        let hir_struct = struct_ty.as_struct().unwrap(); // Can only really get here if the type is a struct
        let fields: Vec<BasicValueEnum<'ink>> = fields
            .iter()
            .map(|field| self.gen_expr(field.expr).expect("expected a field value"))
            .collect();

        self.gen_struct_alloc(hir_struct, fields)
    }

    /// Generates IR for `receiver.method(args...)`.
    ///
    /// Inference has already done the hard part: `infer_method_call` resolves
    /// the callee against the receiver's type and records the winner in
    /// `InferenceResult::method_resolutions`. All that is left is to build the
    /// argument list and hand it to the same `gen_call` a free-function call
    /// uses -- so methods get dispatch table treatment, hot reloading and
    /// value-struct boxing for free, rather than through a parallel code path
    /// that could drift.
    ///
    /// This covers both spellings, because they are the same syntax:
    /// `value.method(..)` passes the receiver as argument 0, while
    /// `Type.assoc_fn(..)` (`LANGUAGE_SPEC` section 3's static member access)
    /// passes no receiver at all. The two are told apart by asking the
    /// *callee* whether it declares a `self` parameter, which is precisely
    /// the thing that decides whether an argument has to be passed -- rather
    /// than by re-deriving what the receiver expression was, which inference
    /// has already settled.
    fn gen_method_call(
        &mut self,
        tgt_expr: ExprId,
        receiver: ExprId,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let resolved = self
            .infer
            .method_resolution(tgt_expr)
            .expect("inference resolved this method call, or it would not have type-checked");
        let function = codira_hir::Function::from(resolved);

        let mut call_args: Vec<BasicMetadataValueEnum<'ink>> = Vec::with_capacity(args.len() + 1);
        if function.data(self.db).self_param().is_some() {
            // A diverging receiver or argument makes the call itself
            // unreachable; propagate that rather than emitting a call that
            // can never run.
            call_args.push(self.gen_expr(receiver)?.into());
        }
        for arg in args {
            call_args.push(self.gen_expr(*arg)?.into());
        }

        self.gen_call(function, &call_args)
            // Same convention as `Expr::Call`: a void method returns the unit
            // struct so callers always get *something*, while a `never`
            // method genuinely returns nothing.
            .or_else(|| match self.infer[tgt_expr].interned() {
                TyKind::Never => None,
                _ => Some(self.context.const_struct(&[], false).into()),
            })
    }

    /// Generates IR for a named tuple literal, e.g. `Foo(1.23, 4)`
    fn gen_named_tuple_lit(&mut self, type_expr: ExprId, args: &[ExprId]) -> BasicValueEnum<'ink> {
        let struct_ty = self.infer[type_expr].clone();
        let hir_struct = struct_ty.as_struct().unwrap(); // Can only really get here if the type is a struct
        let args: Vec<BasicValueEnum<'ink>> = args
            .iter()
            .map(|expr| self.gen_expr(*expr).expect("expected a field value"))
            .collect();

        self.gen_struct_alloc(hir_struct, args)
    }

    /// Generates IR for a tuple literal, e.g. `(1.23, 4)`.
    ///
    /// A tuple is an unnamed value-kind aggregate: it lowers to the anonymous
    /// LLVM struct `get_tuple_type` already produces for `TyKind::Tuple`, and
    /// is built the same way `gen_struct_alloc` builds a value struct --
    /// `undef` plus one `insertvalue` per element. Unlike `Expr::Array`, no
    /// allocation and no runtime call is involved, so a tuple costs exactly
    /// what the equivalent hand-written value struct costs and stays fully
    /// visible to the Eidos optimiser rather than hiding behind an opaque
    /// object pointer.
    ///
    /// `()` falls out of this naturally as the zero-element case, producing
    /// the same empty struct as `gen_empty`.
    fn gen_tuple(&mut self, tgt_expr: ExprId, exprs: &[ExprId]) -> Option<BasicValueEnum<'ink>> {
        let tuple_ty = self.infer[tgt_expr].clone();
        let TyKind::Tuple(_, substs) = tuple_ty.interned() else {
            unreachable!("the type of a tuple literal expression must be a Tuple");
        };

        let struct_ty = self.hir_types.get_tuple_type(substs.as_ref());
        let mut value: AggregateValueEnum<'_> = struct_ty.get_undef().into();
        for (idx, expr) in exprs.iter().enumerate() {
            // A diverging element (`(foo(), never_returns())`) means the rest
            // of the tuple is unreachable; propagate that instead of building
            // an aggregate that can never be observed.
            let elem = self.gen_expr(*expr)?;
            value = self
                .builder
                .build_insert_value(value, elem, idx as u32, "tuple_init")
                .expect("failed to initialize tuple element");
        }

        Some(value.into_struct_value().into())
    }

    /// Generates IR for a unit struct literal, e.g `Foo`
    fn gen_unit_struct_lit(&mut self, type_expr: ExprId) -> BasicValueEnum<'ink> {
        let struct_ty = self.infer[type_expr].clone();
        let hir_struct = struct_ty.as_struct().unwrap(); // Can only really get here if the type is a struct
        self.gen_struct_alloc(hir_struct, Vec::new())
    }

    /// Generates IR for the specified block expression.
    fn gen_block(
        &mut self,
        _tgt_expr: ExprId,
        statements: &[Statement],
        tail: Option<ExprId>,
    ) -> Option<BasicValueEnum<'ink>> {
        for statement in statements.iter() {
            match statement {
                Statement::Let {
                    pat, initializer, ..
                } => {
                    // If the let statement never finishes, there is no need to generate more code
                    if !self.gen_let_statement(*pat, *initializer) {
                        return None;
                    }
                }
                Statement::Expr(expr) => {
                    // No need to generate code after a statement that has a `never` return type.
                    self.gen_expr(*expr)?;
                }
            };
        }

        if let Some(tail) = tail {
            self.gen_expr(tail)
        } else {
            Some(self.gen_empty())
        }
    }

    /// Constructs a builder that should be used to emit an `alloca`
    /// instruction. These instructions should be at the start of the IR.
    fn new_alloca_builder(&self) -> Builder<'ink> {
        let temp_builder = self.context.create_builder();
        let block = self
            .fn_value
            .get_first_basic_block()
            .expect("at this stage there must be a block");
        if let Some(first_instruction) = block.get_first_instruction() {
            temp_builder.position_before(&first_instruction);
        } else {
            temp_builder.position_at_end(block);
        }
        temp_builder
    }

    /// Generate IR for a let statement: `let a:int = 3`. Returns `false` if the
    /// initializer of the statement never returns; `true` otherwise.
    fn gen_let_statement(&mut self, pat: PatId, initializer: Option<ExprId>) -> bool {
        let initializer = match initializer {
            Some(expr) => match self.gen_expr(expr) {
                Some(expr) => Some(expr),
                None => {
                    // If the initializer doesnt return a value it never returns
                    return false;
                }
            },
            None => None,
        };

        match &self.body[pat] {
            Pat::Bind { name } => {
                let builder = self.new_alloca_builder();
                let pat_ty = self.infer[pat].clone();
                let ty = self
                    .hir_types
                    .get_basic_type(&pat_ty)
                    .expect("expected basic type");
                let ptr = builder
                    .build_alloca(ty, &name.to_string())
                    .expect("failed to build alloca for let binding");
                self.pat_to_local.insert(pat, ptr);
                self.pat_to_name.insert(pat, name.to_string());
                if !(pat_ty.is_empty() || pat_ty.is_never()) {
                    if let Some(value) = initializer {
                        self.builder
                            .build_store(ptr, value)
                            .expect("failed to build store for let binding");
                    };
                }
            }
            Pat::Wild => {}
            // `let (a, b) = pair` -- bind each element to its sub-pattern.
            //
            // Irrefutable, so there is no test and no branch: the arity is
            // fixed by the type and inference has already checked it. Each
            // element is `extractvalue`d out of the aggregate, which is why
            // this needs no `alloca` for the tuple itself -- only for the
            // bindings, and only for the ones that are actually named.
            Pat::Tuple(args) => {
                let args = args.clone();
                let Some(value) = initializer else {
                    // No initializer means nothing to destructure; the
                    // bindings stay unallocated, exactly as `Pat::Bind`
                    // leaves them.
                    return true;
                };
                let aggregate = value.into_struct_value();
                for (idx, arg) in args.iter().enumerate() {
                    let element = self
                        .builder
                        .build_extract_value(aggregate, idx as u32, &format!("tuple.{idx}"))
                        .expect("tuple element index checked by inference");
                    self.bind_pattern_to_value(*arg, element);
                }
            }
            Pat::Missing | Pat::Path(_) | Pat::Literal(_) | Pat::TupleStruct { .. } => {
                unreachable!()
            }
        }
        true
    }

    /// Binds `pat` to an already-computed `value`.
    ///
    /// Split out of `gen_let_statement` so a tuple pattern's sub-patterns go
    /// through the same allocate-and-store as a top-level binding, and so
    /// nesting (`let ((a, b), c) = ..`) falls out by recursion rather than
    /// needing its own case.
    fn bind_pattern_to_value(&mut self, pat: PatId, value: BasicValueEnum<'ink>) {
        let body = self.body.clone();
        match &body[pat] {
            Pat::Bind { name } => {
                let builder = self.new_alloca_builder();
                let ptr = builder
                    .build_alloca(value.get_type(), &name.to_string())
                    .expect("failed to build alloca for destructured binding");
                self.builder
                    .build_store(ptr, value)
                    .expect("failed to build store for destructured binding");
                self.pat_to_local.insert(pat, ptr);
                self.pat_to_name.insert(pat, name.to_string());
            }
            Pat::Wild => {}
            Pat::Tuple(args) => {
                let args = args.clone();
                let aggregate = value.into_struct_value();
                for (idx, arg) in args.iter().enumerate() {
                    let element = self
                        .builder
                        .build_extract_value(aggregate, idx as u32, &format!("tuple.{idx}"))
                        .expect("tuple element index checked by inference");
                    self.bind_pattern_to_value(*arg, element);
                }
            }
            Pat::Missing | Pat::Path(_) | Pat::Literal(_) | Pat::TupleStruct { .. } => {
                unreachable!("refutable patterns cannot appear in an irrefutable position")
            }
        }
    }

    /// Generates IR for looking up a certain path expression.
    fn gen_path_expr(
        &mut self,
        path: &Path,
        expr: ExprId,
        resolver: &Resolver,
    ) -> inkwell::values::BasicValueEnum<'ink> {
        match resolver
            .resolve_path_as_value_fully(self.db, path)
            .expect("unknown path")
            .0
        {
            ValueNs::ImplSelf(_) => unimplemented!("no support for self types"),
            ValueNs::LocalBinding(pat) => {
                if let Some(param) = self.pat_to_param.get(&pat) {
                    *param
                } else if let Some(ptr) = self.pat_to_local.get(&pat) {
                    let name = self.pat_to_name.get(&pat).expect("could not find pat name");
                    let pat_ty = self.infer[pat].clone();
                    let ty = self
                        .hir_types
                        .get_basic_type(&pat_ty)
                        .expect("expected basic type");
                    self.builder
                        .build_load(ty, *ptr, name)
                        .expect("failed to build load for local binding")
                } else {
                    unreachable!("could not find the pattern..");
                }
            }
            ValueNs::StructId(_) => self.gen_unit_struct_lit(expr),
            ValueNs::FunctionId(_) => panic!("unable to generate path expression from a function"),
        }
    }

    /// Given an expression and its value optionally dereference the value to
    /// get to the actual value. This is useful if we need to do an
    /// indirection to get to the actual value.
    fn opt_deref_value(
        &mut self,
        expr: ExprId,
        value: BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let ty = &self.infer[expr];
        if let Some(s) = ty.as_struct() {
            if s.data(self.db).memory_kind == codira_hir::StructMemoryKind::Gc {
                return deref_heap_value(&self.builder, value, self.hir_types.get_struct_type(s));
            }
        }
        value
    }

    /// The place-context counterpart of [`Self::opt_deref_value`]: for heap
    /// (GC) structs the place slot holds the GC handle (`**T`), so instead of
    /// loading the whole struct value this resolves the handle to the *data
    /// pointer* (`*T`), which callers can GEP into.
    fn opt_deref_place(
        &mut self,
        expr: ExprId,
        place_ptr: inkwell::values::PointerValue<'ink>,
    ) -> inkwell::values::PointerValue<'ink> {
        let ty = &self.infer[expr];
        if let Some(s) = ty.as_struct() {
            if s.data(self.db).memory_kind == codira_hir::StructMemoryKind::Gc {
                let struct_ty = self.hir_types.get_struct_type(s);
                let handle = self
                    .builder
                    .build_load(
                        self.context.ptr_type(AddressSpace::default()),
                        place_ptr,
                        "handle",
                    )
                    .expect("failed to build load for GC handle")
                    .into_pointer_value();
                // Safety: values of GC struct type are always represented as
                // a runtime reference handle.
                let reference =
                    unsafe { RuntimeReferenceValue::from_ptr_unchecked(handle, struct_ty) };
                return reference.get_data_ptr(&self.builder);
            }
        }
        place_ptr
    }

    /// Generates IR for looking up a certain path expression.
    fn gen_path_place_expr(
        &self,
        path: &Path,
        _expr: ExprId,
        resolver: &Resolver,
    ) -> inkwell::values::PointerValue<'ink> {
        match resolver
            .resolve_path_as_value_fully(self.db, path)
            .expect("unknown path")
            .0
        {
            ValueNs::ImplSelf(_) => unimplemented!("no support for self types"),
            ValueNs::LocalBinding(pat) => *self
                .pat_to_local
                .get(&pat)
                .expect("unresolved local binding"),
            ValueNs::FunctionId(_) | ValueNs::StructId(_) => {
                panic!("no support for module definitions")
            }
        }
    }

    /// Generates IR to calculate a binary operation between two expressions.
    fn gen_binary_op(
        &mut self,
        _tgt_expr: ExprId,
        lhs: ExprId,
        rhs: ExprId,
        op: BinaryOp,
    ) -> Option<BasicValueEnum<'ink>> {
        let lhs_type = self.infer[lhs].clone();
        match lhs_type.interned() {
            TyKind::Bool => self.gen_binary_op_bool(lhs, rhs, op),
            TyKind::Float(_) => self.gen_binary_op_float(lhs, rhs, op),
            TyKind::Int(ty) => self.gen_binary_op_int(lhs, rhs, op, ty.signedness),
            TyKind::Struct(s, _) => {
                if s.data(self.db).memory_kind == codira_hir::StructMemoryKind::Value {
                    self.gen_binary_op_value_struct(lhs, rhs, op)
                } else {
                    self.gen_binary_op_heap_struct(lhs, rhs, op)
                }
            }
            _ => {
                let rhs_type = self.infer[rhs].clone();
                unimplemented!(
                    "unimplemented operation {0}op{1}",
                    lhs_type.display(self.db),
                    rhs_type.display(self.db)
                )
            }
        }
    }

    /// Generates IR to calculate a unary operation on an expression.
    fn gen_unary_op(&mut self, expr: ExprId, op: UnaryOp) -> Option<BasicValueEnum<'ink>> {
        let ty = &self.infer[expr];
        match ty.interned() {
            TyKind::Float(_) => self.gen_unary_op_float(expr, op),
            &TyKind::Int(int_ty) => self.gen_unary_op_int(expr, op, int_ty.signedness),
            TyKind::Bool => self.gen_unary_op_bool(expr, op),
            _ => unimplemented!("unimplemented operation op{0}", ty.display(self.db)),
        }
    }

    /// Generates IR to calculate a unary operation on a floating point value.
    fn gen_unary_op_float(&mut self, expr: ExprId, op: UnaryOp) -> Option<BasicValueEnum<'ink>> {
        let value: FloatValue<'ink> = self
            .gen_expr(expr)
            .map(|value| self.opt_deref_value(expr, value))
            .expect("no value")
            .into_float_value();
        match op {
            UnaryOp::Neg => Some(
                self.builder
                    .build_float_neg(value, "neg")
                    .expect("failed to build float neg")
                    .into(),
            ),
            UnaryOp::Not | UnaryOp::BitNot => {
                unimplemented!("Operator {:?} is not implemented for float", op)
            }
        }
    }

    /// Generates IR to calculate a unary operation on an integer value.
    fn gen_unary_op_int(
        &mut self,
        expr: ExprId,
        op: UnaryOp,
        signedness: codira_hir::Signedness,
    ) -> Option<BasicValueEnum<'ink>> {
        let value: IntValue<'ink> = self
            .gen_expr(expr)
            .map(|value| self.opt_deref_value(expr, value))
            .expect("no value")
            .into_int_value();
        match op {
            UnaryOp::Neg => {
                if signedness == codira_hir::Signedness::Signed {
                    Some(
                        self.builder
                            .build_int_neg(value, "neg")
                            .expect("failed to build int neg")
                            .into(),
                    )
                } else {
                    unimplemented!("Operator {:?} is not implemented for unsigned integer", op)
                }
            }
            // Both `!` (on an integer) and `~` are the bitwise complement
            // at the machine level; inference is what keeps `~` off
            // non-integers and `!` meaningful on bools.
            UnaryOp::Not | UnaryOp::BitNot => Some(
                self.builder
                    .build_not(value, "not")
                    .expect("failed to build not")
                    .into(),
            ),
            //_ => unimplemented!("Operator {:?} is not implemented for integer", op),
        }
    }

    /// Generates IR to calculate a unary operation on a boolean value.
    fn gen_unary_op_bool(&mut self, expr: ExprId, op: UnaryOp) -> Option<BasicValueEnum<'ink>> {
        let value: IntValue<'ink> = self
            .gen_expr(expr)
            .map(|value| self.opt_deref_value(expr, value))
            .expect("no value")
            .into_int_value();
        match op {
            // Both `!` (on an integer) and `~` are the bitwise complement
            // at the machine level; inference is what keeps `~` off
            // non-integers and `!` meaningful on bools.
            UnaryOp::Not | UnaryOp::BitNot => Some(
                self.builder
                    .build_not(value, "not")
                    .expect("failed to build not")
                    .into(),
            ),
            UnaryOp::Neg => unimplemented!("Operator {:?} is not implemented for boolean", op),
        }
    }

    /// Generates IR to calculate a binary operation between two boolean value.
    fn gen_binary_op_bool(
        &mut self,
        lhs_expr: ExprId,
        rhs_expr: ExprId,
        op: BinaryOp,
    ) -> Option<BasicValueEnum<'ink>> {
        let lhs: IntValue<'ink> = self
            .gen_expr(lhs_expr)
            .map(|value| self.opt_deref_value(lhs_expr, value))?
            .into_int_value();
        let rhs: IntValue<'ink> = self
            .gen_expr(rhs_expr)
            .map(|value| self.opt_deref_value(rhs_expr, value))?
            .into_int_value();
        match op {
            BinaryOp::ArithOp(op) => Some(self.gen_arith_bin_op_bool(lhs, rhs, op).into()),
            BinaryOp::Assignment { op } => {
                let rhs = match op {
                    Some(op) => self.gen_arith_bin_op_bool(lhs, rhs, op),
                    None => rhs,
                };
                let place = self.gen_place_expr(lhs_expr)?;
                self.builder
                    .build_store(place, rhs)
                    .expect("failed to build store");
                Some(self.gen_empty())
            }
            BinaryOp::LogicOp(op) => Some(self.gen_logic_bin_op(lhs, rhs, op).into()),
            BinaryOp::CmpOp(op) => Some(
                self.gen_cmp_bin_op_int(lhs, rhs, op, codira_hir::Signedness::Unsigned)
                    .into(),
            ),
        }
    }

    /// Generates IR to calculate a binary operation between two floating point
    /// values.
    fn gen_binary_op_float(
        &mut self,
        lhs_expr: ExprId,
        rhs_expr: ExprId,
        op: BinaryOp,
    ) -> Option<BasicValueEnum<'ink>> {
        let lhs = self
            .gen_expr(lhs_expr)
            .map(|value| self.opt_deref_value(lhs_expr, value))
            .expect("no lhs value")
            .into_float_value();
        let rhs = self
            .gen_expr(rhs_expr)
            .map(|value| self.opt_deref_value(rhs_expr, value))
            .expect("no rhs value")
            .into_float_value();
        match op {
            BinaryOp::ArithOp(op) => Some(self.gen_arith_bin_op_float(lhs, rhs, op).into()),
            BinaryOp::CmpOp(op) => {
                let (name, predicate) = match op {
                    CmpOp::Eq { negated: false } => ("eq", FloatPredicate::OEQ),
                    CmpOp::Eq { negated: true } => ("neq", FloatPredicate::ONE),
                    CmpOp::Ord {
                        ordering: Ordering::Less,
                        strict: false,
                    } => ("lesseq", FloatPredicate::OLE),
                    CmpOp::Ord {
                        ordering: Ordering::Less,
                        strict: true,
                    } => ("less", FloatPredicate::OLT),
                    CmpOp::Ord {
                        ordering: Ordering::Greater,
                        strict: false,
                    } => ("greatereq", FloatPredicate::OGE),
                    CmpOp::Ord {
                        ordering: Ordering::Greater,
                        strict: true,
                    } => ("greater", FloatPredicate::OGT),
                };
                Some(
                    self.builder
                        .build_float_compare(predicate, lhs, rhs, name)
                        .expect("failed to build float compare")
                        .into(),
                )
            }
            BinaryOp::Assignment { op } => {
                let rhs = match op {
                    Some(op) => self.gen_arith_bin_op_float(lhs, rhs, op),
                    None => rhs,
                };
                let place = self.gen_place_expr(lhs_expr)?;
                self.builder
                    .build_store(place, rhs)
                    .expect("failed to build store");
                Some(self.gen_empty())
            }
            BinaryOp::LogicOp(_) => {
                unimplemented!("Operator {:?} is not implemented for float", op)
            }
        }
    }

    /// Generates IR to calculate a binary operation between two integer values.
    fn gen_binary_op_int(
        &mut self,
        lhs_expr: ExprId,
        rhs_expr: ExprId,
        op: BinaryOp,
        signedness: codira_hir::Signedness,
    ) -> Option<BasicValueEnum<'ink>> {
        let lhs = self
            .gen_expr(lhs_expr)
            .map(|value| self.opt_deref_value(lhs_expr, value))
            .expect("no lhs value")
            .into_int_value();
        let rhs = self
            .gen_expr(rhs_expr)
            .map(|value| self.opt_deref_value(rhs_expr, value))
            .expect("no rhs value")
            .into_int_value();
        match op {
            BinaryOp::ArithOp(op) => {
                Some(self.gen_arith_bin_op_int(lhs, rhs, op, signedness).into())
            }
            BinaryOp::CmpOp(op) => Some(self.gen_cmp_bin_op_int(lhs, rhs, op, signedness).into()),
            BinaryOp::Assignment { op } => {
                let rhs = match op {
                    Some(op) => self.gen_arith_bin_op_int(lhs, rhs, op, signedness),
                    None => rhs,
                };
                let place = self.gen_place_expr(lhs_expr)?;
                self.builder
                    .build_store(place, rhs)
                    .expect("failed to build store");
                Some(self.gen_empty())
            }
            BinaryOp::LogicOp(_) => {
                unreachable!("Operator {:?} is not implemented for integer", op)
            }
        }
    }

    /// Generates IR to calculate a binary operation between two heap struct
    /// values (e.g. a Codira `struct(gc)`).
    fn gen_binary_op_heap_struct(
        &mut self,
        lhs_expr: ExprId,
        rhs_expr: ExprId,
        op: BinaryOp,
    ) -> Option<BasicValueEnum<'ink>> {
        let rhs = self
            .gen_expr(rhs_expr)
            .expect("no rhs value")
            .into_pointer_value();
        match op {
            BinaryOp::Assignment { op } => {
                let rhs = match op {
                    Some(op) => unimplemented!(
                        "Assignment with {:?} operator is not implemented for struct",
                        op
                    ),
                    None => rhs,
                };
                let place = self.gen_place_expr(lhs_expr)?;
                self.builder
                    .build_store(place, rhs)
                    .expect("failed to build store");
                Some(self.gen_empty())
            }
            _ => unimplemented!("Operator {:?} is not implemented for struct", op),
        }
    }

    /// Generates IR to calculate a binary operation between two value struct
    /// values, denoted in Codira as `struct(value)`.
    fn gen_binary_op_value_struct(
        &mut self,
        lhs_expr: ExprId,
        rhs_expr: ExprId,
        op: BinaryOp,
    ) -> Option<BasicValueEnum<'ink>> {
        let rhs = self
            .gen_expr(rhs_expr)
            .expect("no rhs value")
            .into_struct_value();
        match op {
            BinaryOp::Assignment { op } => {
                let rhs = match op {
                    Some(op) => unimplemented!(
                        "Assignment with {:?} operator is not implemented for struct",
                        op
                    ),
                    None => rhs,
                };
                let place = self.gen_place_expr(lhs_expr)?;
                self.builder
                    .build_store(place, rhs)
                    .expect("failed to build store");
                Some(self.gen_empty())
            }
            _ => unimplemented!("Operator {:?} is not implemented for struct", op),
        }
    }

    fn gen_arith_bin_op_bool(
        &mut self,
        lhs: IntValue<'ink>,
        rhs: IntValue<'ink>,
        op: ArithOp,
    ) -> IntValue<'ink> {
        (match op {
            ArithOp::BitAnd => self.builder.build_and(lhs, rhs, "bit_and"),
            ArithOp::BitOr => self.builder.build_or(lhs, rhs, "bit_or"),
            ArithOp::BitXor => self.builder.build_xor(lhs, rhs, "bit_xor"),
            _ => unimplemented!(
                "Assignment with {:?} operator is not implemented for boolean",
                op
            ),
        })
        .expect("failed to build boolean bit operation")
    }

    fn gen_cmp_bin_op_int(
        &mut self,
        lhs: IntValue<'ink>,
        rhs: IntValue<'ink>,
        op: CmpOp,
        signedness: codira_hir::Signedness,
    ) -> IntValue<'ink> {
        let (name, predicate) = match op {
            CmpOp::Eq { negated: false } => ("eq", IntPredicate::EQ),
            CmpOp::Eq { negated: true } => ("neq", IntPredicate::NE),
            CmpOp::Ord {
                ordering: Ordering::Less,
                strict: false,
            } => (
                "lesseq",
                match signedness {
                    codira_hir::Signedness::Signed => IntPredicate::SLE,
                    codira_hir::Signedness::Unsigned => IntPredicate::ULE,
                },
            ),
            CmpOp::Ord {
                ordering: Ordering::Less,
                strict: true,
            } => (
                "less",
                match signedness {
                    codira_hir::Signedness::Signed => IntPredicate::SLT,
                    codira_hir::Signedness::Unsigned => IntPredicate::ULT,
                },
            ),
            CmpOp::Ord {
                ordering: Ordering::Greater,
                strict: false,
            } => (
                "greatereq",
                match signedness {
                    codira_hir::Signedness::Signed => IntPredicate::SGE,
                    codira_hir::Signedness::Unsigned => IntPredicate::UGE,
                },
            ),
            CmpOp::Ord {
                ordering: Ordering::Greater,
                strict: true,
            } => (
                "greater",
                match signedness {
                    codira_hir::Signedness::Signed => IntPredicate::SGT,
                    codira_hir::Signedness::Unsigned => IntPredicate::UGT,
                },
            ),
        };

        self.builder
            .build_int_compare(predicate, lhs, rhs, name)
            .expect("failed to build int compare")
    }

    fn gen_arith_bin_op_int(
        &mut self,
        lhs: IntValue<'ink>,
        rhs: IntValue<'ink>,
        op: ArithOp,
        signedness: codira_hir::Signedness,
    ) -> IntValue<'ink> {
        (match op {
            ArithOp::Add => self.builder.build_int_add(lhs, rhs, "add"),
            ArithOp::Subtract => self.builder.build_int_sub(lhs, rhs, "sub"),
            ArithOp::Divide => match signedness {
                codira_hir::Signedness::Signed => {
                    self.builder.build_int_signed_div(lhs, rhs, "div")
                }
                codira_hir::Signedness::Unsigned => {
                    self.builder.build_int_unsigned_div(lhs, rhs, "div")
                }
            },
            ArithOp::Multiply => self.builder.build_int_mul(lhs, rhs, "mul"),
            ArithOp::Remainder => match signedness {
                codira_hir::Signedness::Signed => {
                    self.builder.build_int_signed_rem(lhs, rhs, "rem")
                }
                codira_hir::Signedness::Unsigned => {
                    self.builder.build_int_unsigned_rem(lhs, rhs, "rem")
                }
            },
            ArithOp::LeftShift => self.builder.build_left_shift(lhs, rhs, "left_shift"),
            ArithOp::RightShift => {
                self.builder
                    .build_right_shift(lhs, rhs, signedness.is_signed(), "right_shift")
            }
            ArithOp::BitAnd => self.builder.build_and(lhs, rhs, "bit_and"),
            ArithOp::BitOr => self.builder.build_or(lhs, rhs, "bit_or"),
            ArithOp::BitXor => self.builder.build_xor(lhs, rhs, "bit_xor"),
        })
        .expect("failed to build int arithmetic operation")
    }

    fn gen_arith_bin_op_float(
        &mut self,
        lhs: FloatValue<'ink>,
        rhs: FloatValue<'ink>,
        op: ArithOp,
    ) -> FloatValue<'ink> {
        (match op {
            ArithOp::Add => self.builder.build_float_add(lhs, rhs, "add"),
            ArithOp::Subtract => self.builder.build_float_sub(lhs, rhs, "sub"),
            ArithOp::Divide => self.builder.build_float_div(lhs, rhs, "div"),
            ArithOp::Multiply => self.builder.build_float_mul(lhs, rhs, "mul"),
            ArithOp::Remainder => self.builder.build_float_rem(lhs, rhs, "rem"),
            ArithOp::LeftShift
            | ArithOp::RightShift
            | ArithOp::BitAnd
            | ArithOp::BitOr
            | ArithOp::BitXor => {
                unreachable!("Operator {:?} is not implemented for float", op)
            }
        })
        .expect("failed to build float arithmetic operation")
    }

    fn gen_logic_bin_op(
        &mut self,
        lhs: IntValue<'ink>,
        rhs: IntValue<'ink>,
        op: LogicOp,
    ) -> IntValue<'ink> {
        (match op {
            LogicOp::And => self.builder.build_and(lhs, rhs, "and"),
            LogicOp::Or => self.builder.build_or(lhs, rhs, "or"),
        })
        .expect("failed to build logic operation")
    }

    /// Given an expression generate code that results in a memory address that
    /// can be used for other place operations.
    fn gen_place_expr(&mut self, expr: ExprId) -> Option<PointerValue<'ink>> {
        let body = self.body.clone();
        match &body[expr] {
            Expr::Path(ref p) => {
                let resolver = codira_hir::resolver_for_expr(self.db, self.body.owner(), expr);
                Some(self.gen_path_place_expr(p, expr, &resolver))
            }
            Expr::Field {
                expr: receiver_expr,
                name,
            } => self.gen_place_field(expr, *receiver_expr, name),
            Expr::Index { base, index } => self.gen_place_index(expr, *base, *index),
            _ => unreachable!("invalid place expression"),
        }
    }

    /// Returns true if the specified expression refers to an expression that
    /// results in a memory address that can be used for other place
    /// operations.
    fn is_place_expr(&self, expr: ExprId) -> bool {
        let body = self.body.clone();
        match &body[expr] {
            Expr::Path(..) | Expr::Array(_) => true,
            Expr::Field { expr, .. } => self.is_place_expr(*expr),
            Expr::Index { base, .. } => self.is_place_expr(*base),
            _ => false,
        }
    }

    /// Returns true if a call to the specified function should be looked up in
    /// the dispatch table; if false is returned the function should be
    /// called directly.
    fn should_use_dispatch_table(&self, function: codira_hir::Function) -> bool {
        self.module_group.should_runtime_link_fn(self.db, function)
    }

    /// Generates IR for a function call.
    ///
    /// A dispatch-table call goes through the same `_wrapper` entry point
    /// used for host-language marshalling (see `gen_fn_wrapper` and the
    /// comment on `DispatchTableBuilder::collect_fn_def`'s `ir_type`), which
    /// boxes struct-by-value arguments into GC references on the way in and
    /// boxes a struct-by-value return into a GC reference on the way out.
    /// A direct (same-module) call goes straight to the plain internal
    /// function and needs none of that. This box/unbox step is the
    /// corresponding caller-side half of that convention.
    /// Emits a call to an `extern "codira-intrinsic"` function as inline IR.
    ///
    /// The name is checked here rather than at the declaration because the
    /// declaration is just a signature -- what makes `load_u32` meaningful is
    /// that codegen knows how to emit it. A name codegen does not recognise
    /// is a hard error rather than a silent no-op: the alternative is a
    /// program that compiles, returns garbage, and gives no indication why.
    fn gen_intrinsic_call(
        &mut self,
        expr: ExprId,
        function: codira_hir::Function,
        args: &[BasicMetadataValueEnum<'ink>],
    ) -> Option<BasicValueEnum<'ink>> {
        let name = function.name(self.db).to_string();
        match intrinsic_ops::gen_intrinsic(&name, args, self.context, self.module, &self.builder) {
            Ok(Some(value)) => Some(value),
            // A `store_*` yields nothing. Unit is substituted for the same
            // reason a void call does above: `None` here would be read as
            // `never`, and a store does return.
            Ok(None) => match self.infer[expr].interned() {
                TyKind::Never => None,
                _ => Some(self.context.const_struct(&[], false).into()),
            },
            Err(intrinsic_ops::IntrinsicError::Unknown) => panic!(
                "`{name}` is not a known compiler intrinsic. Functions declared in an                  `extern \"codira-intrinsic\"` block must name an operation the compiler                  can emit; see `codira_codegen::ir::intrinsic_ops` for the list."
            ),
            Err(intrinsic_ops::IntrinsicError::Arity { expected, found }) => panic!(
                "intrinsic `{name}` takes {expected} argument(s) but was declared with {found}"
            ),
        }
    }

    fn gen_call(
        &mut self,
        function: codira_hir::Function,
        args: &[BasicMetadataValueEnum<'ink>],
    ) -> Option<BasicValueEnum<'ink>> {
        let sig = function.ty(self.db).callable_sig(self.db).unwrap();

        let call_site = if self.should_use_dispatch_table(function) {
            let boxed_args: Vec<BasicMetadataValueEnum<'ink>> = args
                .iter()
                .zip(sig.params().iter())
                .map(|(arg, param_ty)| {
                    if let Some(hir_struct) = param_ty.as_struct() {
                        if hir_struct.data(self.db).memory_kind
                            == codira_hir::StructMemoryKind::Value
                        {
                            return self
                                .gen_struct_alloc_on_heap(hir_struct, arg.into_struct_value())
                                .into();
                        }
                    }
                    *arg
                })
                .collect();

            let (fn_ty, fn_ptr) = self.dispatch_table.gen_function_lookup(
                self.db,
                self.external_globals.dispatch_table,
                &self.builder,
                function,
            );
            self.builder
                .build_indirect_call(
                    fn_ty,
                    fn_ptr,
                    &boxed_args,
                    &function.name(self.db).to_string(),
                )
                .expect("failed to build indirect call")
        } else {
            let llvm_function = self.function_map.get(&function).unwrap_or_else(|| {
                panic!(
                    "missing function value for codira_hir function: '{}'",
                    function.name(self.db),
                )
            });
            self.builder
                .build_call(*llvm_function, args, &function.name(self.db).to_string())
                .expect("failed to build call")
        };

        let ret_value = call_site.try_as_basic_value().basic();

        if self.should_use_dispatch_table(function) {
            if let Some(hir_struct) = sig.ret().as_struct() {
                if hir_struct.data(self.db).memory_kind == codira_hir::StructMemoryKind::Value {
                    let struct_ty = self.hir_types.get_struct_type(hir_struct);
                    return ret_value
                        .map(|value| deref_heap_value(&self.builder, value, struct_ty));
                }
            }
        }

        ret_value
    }

    /// Generates IR for an if statement.
    fn gen_if(
        &mut self,
        _expr: ExprId,
        condition: ExprId,
        then_branch: ExprId,
        else_branch: Option<ExprId>,
    ) -> Option<inkwell::values::BasicValueEnum<'ink>> {
        // Generate IR for the condition
        let condition_ir = self
            .gen_expr(condition)
            .map(|value| self.opt_deref_value(condition, value))?
            .into_int_value();

        // Generate the code blocks to branch to
        let mut then_block = self.context.append_basic_block(self.fn_value, "then");
        let else_block_and_expr = match &else_branch {
            Some(else_branch) => Some((
                self.context.append_basic_block(self.fn_value, "else"),
                else_branch,
            )),
            None => None,
        };
        let merge_block = self.context.append_basic_block(self.fn_value, "if_merge");

        // Build the actual branching IR for the if statement
        let else_block = else_block_and_expr.map_or(merge_block, |e| e.0);
        self.builder
            .build_conditional_branch(condition_ir, then_block, else_block)
            .expect("failed to build conditional_branch");

        // Fill the then block
        self.builder.position_at_end(then_block);
        let then_block_ir = self.gen_expr(then_branch);
        if !self.infer[then_branch].is_never() {
            self.builder
                .build_unconditional_branch(merge_block)
                .expect("failed to build unconditional_branch");
        }
        then_block = self.builder.get_insert_block().unwrap();

        // Fill the else block, if it exists and get the result back
        let else_ir_and_block = if let Some((else_block, else_branch)) = else_block_and_expr {
            else_block
                .move_after(then_block)
                .expect("programmer error, then_block is invalid");
            self.builder.position_at_end(else_block);
            let result_ir = self.gen_expr(*else_branch);
            if result_ir.is_some() {
                self.builder
                    .build_unconditional_branch(merge_block)
                    .expect("failed to build unconditional_branch");
            }
            Some((result_ir, self.builder.get_insert_block().unwrap()))
        } else {
            None
        };

        // Create merge block
        let current_block = self.builder.get_insert_block().unwrap();
        merge_block.move_after(current_block).unwrap();
        self.builder.position_at_end(merge_block);

        // Construct phi block if a value was returned
        if let Some(then_block_ir) = then_block_ir {
            if let Some((Some(else_block_ir), else_block)) = else_ir_and_block {
                let phi = self
                    .builder
                    .build_phi(then_block_ir.get_type(), "iftmp")
                    .expect("failed to build phi");
                phi.add_incoming(&[(&then_block_ir, then_block), (&else_block_ir, else_block)]);
                Some(phi.as_basic_value())
            } else {
                Some(then_block_ir)
            }
        } else if let Some((else_block_ir, _else_block)) = else_ir_and_block {
            // If both the then and the else block never return, the entire if statement
            // will never return. Therefor we have to remove the merge block
            // because it has no predecessor.
            if else_block_ir.is_none() {
                merge_block
                    .remove_from_function()
                    .expect("merge block must have a parent");
            }
            else_block_ir
        } else {
            Some(self.gen_empty())
        }
    }

    fn gen_return(
        &mut self,
        _expr: ExprId,
        ret_expr: Option<ExprId>,
    ) -> Option<BasicValueEnum<'ink>> {
        let ret_value = ret_expr.and_then(|expr| self.gen_expr(expr));

        // Construct a return statement from the returned value of the body
        if let Some(value) = ret_value {
            self.builder
                .build_return(Some(&value))
                .expect("failed to build return");
        } else {
            self.builder
                .build_return(None)
                .expect("failed to build return");
        }

        None
    }

    fn gen_break(
        &mut self,
        _expr: ExprId,
        break_expr: Option<ExprId>,
    ) -> Option<BasicValueEnum<'ink>> {
        if let Some(expr) = break_expr {
            // There is an expression
            // e.g. break x;
            // Turn that expression into IR.
            let break_value = self.gen_expr(expr);

            // If the expression never returns, we can stop what we're doing.
            if let Some(break_value) = break_value {
                let loop_info = self.active_loop.as_mut().unwrap();
                loop_info.break_values.push(Some((
                    break_value,
                    self.builder.get_insert_block().unwrap(),
                )));
                self.builder
                    .build_unconditional_branch(loop_info.exit_block)
                    .expect("failed to build unconditional_branch");
            }
        } else {
            // If the break expression doesnt contain a break statement. Add a none to the
            // break values.
            let loop_info = self.active_loop.as_mut().unwrap();
            loop_info.break_values.push(None);
            self.builder
                .build_unconditional_branch(loop_info.exit_block)
                .expect("failed to build unconditional_branch");
        };

        None
    }

    fn gen_loop_block_expr(
        &mut self,
        block: ExprId,
        exit_block: BasicBlock<'ink>,
    ) -> (
        BasicBlock<'ink>,
        BreakSources<'ink>,
        Option<BasicValueEnum<'ink>>,
    ) {
        // Build a new loop info struct
        let loop_info = LoopInfo {
            exit_block,
            break_values: Vec::new(),
        };

        // Replace previous loop info
        let prev_loop = self.active_loop.replace(loop_info);

        // Start generating code inside the loop
        let value = self.gen_expr(block);

        let LoopInfo {
            exit_block,
            break_values,
        } = std::mem::replace(&mut self.active_loop, prev_loop).unwrap();

        (exit_block, break_values, value)
    }

    fn gen_while(
        &mut self,
        _expr: ExprId,
        condition_expr: ExprId,
        body_expr: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        let context = self.context;
        let cond_block = context.append_basic_block(self.fn_value, "whilecond");
        let loop_block = context.append_basic_block(self.fn_value, "while");
        let exit_block = context.append_basic_block(self.fn_value, "afterwhile");

        // Insert an explicit fall through from the current block to the condition check
        self.builder
            .build_unconditional_branch(cond_block)
            .expect("failed to build unconditional_branch");

        // Generate condition block
        self.builder.position_at_end(cond_block);
        let condition_ir = self
            .gen_expr(condition_expr)
            .map(|value| self.opt_deref_value(condition_expr, value));
        {
            let condition_ir = condition_ir?;
            self.builder
                .build_conditional_branch(condition_ir.into_int_value(), loop_block, exit_block)
                .expect("failed to build conditional_branch");
        }

        // Generate loop block
        self.builder.position_at_end(loop_block);
        let (exit_block, _, value) = self.gen_loop_block_expr(body_expr, exit_block);
        if value.is_some() {
            self.builder
                .build_unconditional_branch(cond_block)
                .expect("failed to build unconditional_branch");
        }

        // Generate exit block
        self.builder.position_at_end(exit_block);

        Some(self.gen_empty())
    }

    fn gen_loop(&mut self, _expr: ExprId, body_expr: ExprId) -> Option<BasicValueEnum<'ink>> {
        let context = self.context;
        let loop_block = context.append_basic_block(self.fn_value, "loop");
        let exit_block = context.append_basic_block(self.fn_value, "exit");

        // Insert an explicit fall through from the current block to the loop
        self.builder
            .build_unconditional_branch(loop_block)
            .expect("failed to build unconditional_branch");

        // Generate the body of the loop
        self.builder.position_at_end(loop_block);
        let (exit_block, break_values, value) = self.gen_loop_block_expr(body_expr, exit_block);
        if value.is_some() {
            self.builder
                .build_unconditional_branch(loop_block)
                .expect("failed to build unconditional_branch");
        }

        if break_values.is_empty() {
            // Not a single code entry point jumped to the exit block through a break.
            // Therefor we can completely remove the exit block since it doesnt
            // have a predecessor.
            exit_block
                .remove_from_function()
                .expect("the exit block must have a parent");
            None
        } else {
            // Move the builder to the exit block
            self.builder.position_at_end(exit_block);

            // If the break values contain values, (so there where `break x;` statements),
            // generate a phi value. This then assumes that all breaks had
            // values.
            if let Some(Some((value, _))) = break_values.first() {
                let phi = self
                    .builder
                    .build_phi(value.get_type(), "exit")
                    .expect("failed to build phi");
                for (value, block) in break_values.into_iter().map(Option::unwrap) {
                    phi.add_incoming(&[(&value, block)]);
                }
                Some(phi.as_basic_value())
            } else {
                // Otherwise, in the case of `break;` (without an expression) the return value
                // is just empty.
                Some(self.gen_empty())
            }
        }
    }

    fn gen_field(
        &mut self,
        _expr: ExprId,
        receiver_expr: ExprId,
        name: &Name,
    ) -> Option<BasicValueEnum<'ink>> {
        // `Expr::Field` covers both `point.x` and `pair.0`. A tuple has no
        // `Struct` behind it -- its layout comes straight from the type's
        // element list -- so it is handled first, before the struct lookup
        // that would otherwise fail.
        if let TyKind::Tuple(_, substs) = self.infer[receiver_expr].clone().interned() {
            return self.gen_tuple_field(receiver_expr, name, substs.as_ref());
        }

        let hir_struct = self.infer[receiver_expr]
            .as_struct()
            .expect("expected a struct");

        let hir_struct_name = hir_struct.name(self.db);

        let field_idx = hir_struct
            .field(self.db, name)
            .expect("expected a struct field")
            .index(self.db);

        let field_ir_name = &format!("{hir_struct_name}.{name}");
        if self.is_place_expr(receiver_expr) {
            let struct_ty = self.hir_types.get_struct_type(hir_struct);
            let receiver_ptr = self.gen_place_expr(receiver_expr)?;
            let receiver_ptr = self.opt_deref_place(receiver_expr, receiver_ptr);
            let field_ptr = self
                .builder
                .build_struct_gep(
                    struct_ty,
                    receiver_ptr,
                    field_idx,
                    &format!("{hir_struct_name}->{name}"),
                )
                .unwrap_or_else(|_| {
                    panic!(
                        "could not get pointer to field `{hir_struct_name}::{name}` at index {field_idx}"
                    )
                });
            let field_ty = struct_ty
                .get_field_type_at_index(field_idx)
                .expect("field index out of bounds");
            Some(
                self.builder
                    .build_load(field_ty, field_ptr, field_ir_name)
                    .expect("failed to build load for struct field"),
            )
        } else {
            let receiver_value = self.gen_expr(receiver_expr)?;
            let receiver_value = self.opt_deref_value(receiver_expr, receiver_value);
            let receiver_struct = receiver_value.into_struct_value();
            Some(
                self.builder
                    .build_extract_value(receiver_struct, field_idx, field_ir_name)
                    .unwrap_or_else(|_| {
                        panic!(
                            "could not extract field {name} (index: {field_idx}) from struct {hir_struct_name}"
                        )
                    }),
            )
        }
    }

    /// Generates IR for `tuple.N`. The caller has already established that
    /// the receiver is a tuple and passes its element types along, so the
    /// returned `Option` carries only the usual meaning: `None` means the
    /// receiver diverges and the rest of the block is unreachable.
    ///
    /// Element extraction is `extractvalue` on the aggregate directly, with
    /// no `alloca` and no GEP, so `pair.0` is free after optimisation --
    /// tuples stay the zero-overhead multiple-return mechanism they need to
    /// be for stdlib signatures like `frexp(x) -> (f64, i32)`.
    fn gen_tuple_field(
        &mut self,
        receiver_expr: ExprId,
        name: &Name,
        element_tys: &[Ty],
    ) -> Option<BasicValueEnum<'ink>> {
        let idx = name
            .as_tuple_index()
            .expect("inference accepted a non-index field name on a tuple");

        let tuple_ty = self.hir_types.get_tuple_type(element_tys);
        let field_ir_name = &format!("tuple.{idx}");

        if self.is_place_expr(receiver_expr) {
            let receiver_ptr = self.gen_place_expr(receiver_expr)?;
            let receiver_ptr = self.opt_deref_place(receiver_expr, receiver_ptr);
            let field_ptr = self
                .builder
                .build_struct_gep(tuple_ty, receiver_ptr, idx as u32, field_ir_name)
                .unwrap_or_else(|_| panic!("could not get pointer to tuple element {idx}"));
            let field_ty = tuple_ty
                .get_field_type_at_index(idx as u32)
                .expect("tuple element index out of bounds");
            Some(
                self.builder
                    .build_load(field_ty, field_ptr, field_ir_name)
                    .expect("failed to build load for tuple element"),
            )
        } else {
            let receiver_value = self.gen_expr(receiver_expr)?;
            let receiver_value = self.opt_deref_value(receiver_expr, receiver_value);
            Some(
                self.builder
                    .build_extract_value(
                        receiver_value.into_struct_value(),
                        idx as u32,
                        field_ir_name,
                    )
                    .unwrap_or_else(|_| panic!("could not extract tuple element {idx}")),
            )
        }
    }

    fn gen_place_field(
        &mut self,
        _expr: ExprId,
        receiver_expr: ExprId,
        name: &Name,
    ) -> Option<PointerValue<'ink>> {
        // As in `gen_field`: a tuple receiver has no `Struct` to look up.
        if let TyKind::Tuple(_, substs) = self.infer[receiver_expr].clone().interned() {
            let idx = name
                .as_tuple_index()
                .expect("inference accepted a non-index field name on a tuple");
            let tuple_ty = self.hir_types.get_tuple_type(substs.as_ref());
            let receiver_ptr = self.gen_place_expr(receiver_expr)?;
            let receiver_ptr = self.opt_deref_place(receiver_expr, receiver_ptr);
            return Some(
                self.builder
                    .build_struct_gep(tuple_ty, receiver_ptr, idx as u32, &format!("tuple.{idx}"))
                    .unwrap_or_else(|_| panic!("could not get pointer to tuple element {idx}")),
            );
        }

        let hir_struct = self.infer[receiver_expr]
            .as_struct()
            .expect("expected a struct");

        let hir_struct_name = hir_struct.name(self.db);

        let field_idx = hir_struct
            .field(self.db, name)
            .expect("expected a struct field")
            .index(self.db);

        let receiver_ptr = self.gen_place_expr(receiver_expr)?;
        let receiver_ptr = self.opt_deref_place(receiver_expr, receiver_ptr);
        Some(
            self.builder
                .build_struct_gep(
                    self.hir_types.get_struct_type(hir_struct),
                    receiver_ptr,
                    field_idx,
                    &format!("{hir_struct_name}->{name}"),
                )
                .unwrap_or_else(|_| {
                    panic!(
                        "could not get pointer to field `{hir_struct_name}::{name}` at index {field_idx}"
                    )
                }),
        )
    }

    /// Generates code to construct an array literal at runtime. Returns `None`
    /// if the code generation for the array literal never returns.
    fn gen_array(&mut self, expr: ExprId, exprs: &[ExprId]) -> Option<RuntimeArrayValue<'ink>> {
        let array_ty = &self.infer[expr];
        let element_ty = array_ty
            .as_array()
            .expect("the type of an array literal expression must be an Array");

        let (new_array_fn_ty, new_array_fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            &intrinsics::new_array,
        );

        // No bitcast needed under opaque pointers -- see gen_struct_alloc_on_heap's
        // note.
        let type_info_ptr = self.type_table.gen_type_info_lookup(
            self.context,
            &self.builder,
            &self.hir_types.type_id(array_ty),
            self.external_globals.type_table,
        );

        let allocator_handle = self.get_allocator_handle_ptr();

        let length_value = self
            .hir_types
            .get_usize_type()
            .const_int(exprs.len() as u64, false);

        // An object pointer adds an extra layer of indirection to allow for hot
        // reloading. To make it struct type agnostic, it is stored in a `*const
        // *mut std::ffi::c_void`.
        let untyped_array_ptr = self
            .builder
            .build_indirect_call(
                new_array_fn_ty,
                new_array_fn_ptr,
                &[
                    type_info_ptr.into(),
                    length_value.into(),
                    allocator_handle.into(),
                ],
                "ref",
            )
            .expect("failed to build call to `new_array` intrinsic")
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // No cast needed under opaque pointers -- `untyped_array_ptr` is
        // treated as `**ArrayValueT` directly, see gen_struct_alloc_on_heap.
        let array_ty = self.hir_types.get_array_type(element_ty);
        let array = RuntimeArrayValue::from_ptr(untyped_array_ptr, array_ty)
            .expect("unable to convert pointer to typed reference");
        let array_elements = array.get_elements(&self.builder);
        let element_basic_ty = array.element_ty();
        for (idx, expr) in exprs.iter().enumerate() {
            let element_ptr = unsafe {
                self.builder
                    .build_gep(
                        element_basic_ty,
                        array_elements,
                        &[self.context.i64_type().const_int(idx as u64, false)],
                        &format!("{}[{}]", array_elements.get_name().to_string_lossy(), idx),
                    )
                    .expect("failed to build GEP into array elements")
            };

            let expr_value = self.gen_expr(*expr)?;
            self.builder
                .build_store(element_ptr, expr_value)
                .expect("failed to build store for array element");
        }

        // Once all values have been stored in the array, update the length of the array
        let length = array.length_ty().const_int(exprs.len() as u64, false);
        let array_length_ptr = array.get_length_ptr(&self.builder);
        self.builder
            .build_store(array_length_ptr, length)
            .expect("failed to build store for array length");

        Some(array)
    }

    /// Generates an index into an array
    fn gen_index(
        &mut self,
        expr: ExprId,
        base: ExprId,
        index: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        let element_ty = self.infer[base]
            .as_array()
            .expect("indexing base must be an array");
        let element_basic_ty = self
            .hir_types
            .get_basic_type(element_ty)
            .expect("expected basic type");
        let element_ptr = self.gen_place_index(expr, base, index)?;
        Some(
            self.builder
                .build_load(element_basic_ty, element_ptr, "")
                .expect("failed to build load for array index"),
        )
    }

    /// Generates an index into an array
    fn gen_place_index(
        &mut self,
        _expr: ExprId,
        base: ExprId,
        index: ExprId,
    ) -> Option<PointerValue<'ink>> {
        let element_ty = self.infer[base]
            .as_array()
            .expect("indexing base must be an array");
        let array_ty = self.hir_types.get_array_type(element_ty);

        // Safety: place expression can only be generated if the base expression is an
        // array.
        let base = unsafe {
            RuntimeArrayValue::from_ptr_unchecked(
                self.gen_expr(base)?.into_pointer_value(),
                array_ty,
            )
        };
        let index = self.gen_expr(index)?.into_int_value();

        let elements = base.get_elements(&self.builder);
        let element_basic_ty = base.element_ty();
        Some(unsafe {
            self.builder
                .build_gep(
                    element_basic_ty,
                    elements,
                    &[index],
                    &format!("{}+index", elements.get_name().to_string_lossy()),
                )
                .expect("failed to build GEP for array index")
        })
    }

    /// Returns a pointer to the allocator handle
    fn get_allocator_handle_ptr(&self) -> PointerValue<'ink> {
        let global = self
            .external_globals
            .alloc_handle
            .expect("no allocator handle was specified, this is required for structs");
        let value_type = global.get_value_type().into_pointer_type();
        self.builder
            .build_load(value_type, global.as_pointer_value(), "allocator_handle")
            .expect("failed to build load for allocator handle")
            .into_pointer_value()
    }
}

/// Derefs a heap-allocated value. As we introduce a layer of indirection for
/// hot reloading, we need to first load the pointer that points to the memory
/// block.
///
/// `value_type` is the type of the struct stored on the heap -- every
/// caller already knows this (it's how they decided to call this function
/// in the first place), and it's now required up front rather than
/// recovered from `value`'s LLVM pointer type (impossible under opaque
/// pointers -- see `RuntimeReferenceValue`'s doc comment).
fn deref_heap_value<'ink>(
    builder: &Builder<'ink>,
    value: BasicValueEnum<'ink>,
    value_type: inkwell::types::StructType<'ink>,
) -> BasicValueEnum<'ink> {
    // Safety: we can assume that the input is a RuntimeReferenceValue
    let mem_ptr = unsafe {
        RuntimeReferenceValue::from_ptr_unchecked(value.into_pointer_value(), value_type)
    }
    .get_data_ptr(builder);

    builder
        .build_load(value_type, mem_ptr, "deref")
        .expect("failed to build load for heap value deref")
}
