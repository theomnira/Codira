//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Code generation for function values: creating one (a closure literal, or a
//!   named function used as a value), calling through one, and the code of each
//!   target (a closure body, or a thunk that calls a named function). The
//!   representation and its hot-reload story are documented in `ir::closures`.
//!
//! # Calling convention
//!
//! The code of a target is `(ptr value, params..) -> ret`, every type in the
//! internal representation (value structs by value). `value` is the function
//! value itself -- its GC handle -- from which a closure reads its captures.
//! A call is
//!
//! ```text
//! object = *value            ; the handle points at the object
//! cell   = object.cell        ; a runtime-owned cell
//! code   = *cell              ; the target's code currently loaded
//! code(value, args..)
//! ```
//!
//! so it costs three dependent loads and an indirect call. A value whose
//! handle is null -- which only a hot reload that zero-initialises a new
//! field of function type can produce -- traps instead of being called.

use codira_hir::{Expr, ExprId, Pat, Ty, TyKind};
use inkwell::{
    types::BasicType,
    values::{BasicMetadataValueEnum, BasicValueEnum},
    AddressSpace,
};

use super::BodyIrGenerator;
use crate::{
    intrinsics,
    ir::{
        closures::{FnValueTarget, FnValueTargets, TargetKind},
        RuntimeReferenceValue,
    },
};

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// Creates the function value of the closure literal `expr`, capturing
    /// the current value of every binding it captures.
    pub(super) fn gen_closure(&mut self, expr: ExprId) -> Option<BasicValueEnum<'ink>> {
        let target = self
            .dispatch_table
            .fn_values()
            .closure(&self.instance, expr)
            .clone();
        let mut values = Vec::with_capacity(target.captures.len());
        for capture in &target.captures {
            let ptr = *self
                .pat_to_local
                .get(&capture.pat)
                .expect("a captured binding is a local of the creating code");
            let ty = self
                .hir_types
                .get_basic_type(&capture.ty)
                .expect("a capture has a basic type");
            values.push(
                self.builder
                    .build_load(ty, ptr, &format!("capture.{}", capture.name))
                    .expect("failed to load a captured binding"),
            );
        }
        Some(self.gen_fn_value_object(&target, &values))
    }

    /// Creates a function value for the named function `item`.
    pub(super) fn gen_fn_item_value(&mut self, item: codira_hir::Function) -> BasicValueEnum<'ink> {
        let target = self.dispatch_table.fn_values().item(item).clone();
        self.gen_fn_value_object(&target, &[])
    }

    /// Allocates a function value object for `target` and fills it: the cell,
    /// then `captures`.
    fn gen_fn_value_object(
        &mut self,
        target: &FnValueTarget,
        captures: &[BasicValueEnum<'ink>],
    ) -> BasicValueEnum<'ink> {
        let object_ty = FnValueTargets::object_type(target, self.hir_types);
        let type_id = FnValueTargets::object_type_id(target, self.db, self.hir_types);
        let (new_fn_ty, new_fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            &intrinsics::new,
        );
        let type_info_ptr = self.type_table.gen_type_info_lookup(
            self.context,
            &self.builder,
            &type_id,
            self.external_globals.type_table,
        );
        let allocator_handle = self.get_allocator_handle_ptr();
        let handle = self
            .builder
            .build_indirect_call(
                new_fn_ty,
                new_fn_ptr,
                &[type_info_ptr.into(), allocator_handle.into()],
                "fn_value",
            )
            .expect("failed to build call to `new` intrinsic")
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        // Safety: `new` returns a reference to an object of the type asked for.
        let reference = unsafe { RuntimeReferenceValue::from_ptr_unchecked(handle, object_ty) };
        let data = reference.get_data_ptr(&self.builder);

        let cell = self.dispatch_table.gen_fn_value_cell_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            &target.key,
        );
        let cell = self
            .builder
            .build_ptr_to_int(cell, self.hir_types.get_usize_type(), "cell")
            .expect("failed to convert the cell address");
        let fields = std::iter::once(BasicValueEnum::from(cell)).chain(captures.iter().copied());
        for (idx, value) in fields.enumerate() {
            let field = self
                .builder
                .build_struct_gep(object_ty, data, idx as u32, "fn_value.field")
                .expect("a function value object has this field");
            self.builder
                .build_store(field, value)
                .expect("failed to initialize a function value field");
        }
        handle.into()
    }

    /// Calls the function value `callee` (of type `fn_ty`) with `args`.
    pub(super) fn gen_indirect_call(
        &mut self,
        call: ExprId,
        callee: BasicValueEnum<'ink>,
        fn_ty: &Ty,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let fn_ty = match fn_ty.interned() {
            TyKind::TypeAlias(alias) => alias.target_type(self.db),
            _ => fn_ty.clone(),
        };
        let mut values = Vec::with_capacity(args.len());
        for &arg in args {
            values.push(self.gen_expr(arg)?);
        }
        let never = matches!(self.infer[call].interned(), TyKind::Never);
        self.gen_fn_value_call(callee, &fn_ty, &values, never)
    }

    /// Calls the function value `callee` (of the function type `fn_ty`,
    /// without an alias) with the already evaluated `values`. `never` says
    /// the call does not return.
    pub(super) fn gen_fn_value_call(
        &mut self,
        callee: BasicValueEnum<'ink>,
        fn_ty: &Ty,
        values: &[BasicValueEnum<'ink>],
        never: bool,
    ) -> Option<BasicValueEnum<'ink>> {
        let handle = callee.into_pointer_value();
        let mut call_args: Vec<BasicMetadataValueEnum<'ink>> = vec![handle.into()];
        call_args.extend(values.iter().map(|&value| BasicMetadataValueEnum::from(value)));

        // Only a hot reload can leave a function value empty; stop there
        // rather than call through null.
        let is_null = self
            .builder
            .build_is_null(handle, "fn_value.is_null")
            .expect("failed to compare a function value with null");
        let empty = self
            .context
            .append_basic_block(self.fn_value, "fn_value.empty");
        let present = self
            .context
            .append_basic_block(self.fn_value, "fn_value.present");
        self.builder
            .build_conditional_branch(is_null, empty, present)
            .expect("failed to branch on an empty function value");
        self.builder.position_at_end(empty);
        let trap = inkwell::intrinsics::Intrinsic::find("llvm.trap")
            .and_then(|intrinsic| intrinsic.get_declaration(self.module, &[]))
            .expect("llvm.trap is always available");
        self.builder
            .build_call(trap, &[], "")
            .expect("failed to build trap");
        self.builder
            .build_unreachable()
            .expect("failed to build unreachable");
        self.builder.position_at_end(present);

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let object = self
            .builder
            .build_load(ptr_ty, handle, "fn_value.object")
            .expect("failed to load a function value's object")
            .into_pointer_value();
        let cell = self
            .builder
            .build_load(self.hir_types.get_usize_type(), object, "fn_value.cell")
            .expect("failed to load a function value's cell")
            .into_int_value();
        let cell = self
            .builder
            .build_int_to_ptr(cell, ptr_ty, "fn_value.cell_ptr")
            .expect("failed to convert a cell address");
        let code = self
            .builder
            .build_load(ptr_ty, cell, "fn_value.code")
            .expect("failed to load a function value's code")
            .into_pointer_value();
        let code_ty = self.hir_types.get_fn_value_code_type(fn_ty);
        let result = self
            .builder
            .build_indirect_call(code_ty, code, &call_args, "fn_value.call")
            .expect("failed to build a call through a function value")
            .try_as_basic_value()
            .basic();

        if never {
            self.builder
                .build_unreachable()
                .expect("failed to build unreachable");
            None
        } else {
            Some(result.unwrap_or_else(|| self.context.const_struct(&[], false).into()))
        }
    }

    /// Generates the code of `target` into this generator's function: a
    /// closure's body, or a thunk calling a named function.
    pub(crate) fn gen_fn_value_target(&mut self, target: &FnValueTarget) {
        // A target is a function of its own, not the body the escape
        // analysis placed objects for: its allocations go to the heap (a
        // closure body's would anyway; an inlined closure's target is never
        // called), and it has no region.
        self.escapes = None;
        self.region_function = false;
        let value = self
            .fn_value
            .get_nth_param(0)
            .expect("function-value code takes the value first")
            .into_pointer_value();
        let (_, ret_ty) = target
            .fn_ty
            .as_function()
            .map(|(p, r)| (p.to_vec(), r.clone()))
            .expect("a target has a function type");
        let param_count = self.fn_value.count_params() - 1;

        let (result, diverges) = match &target.kind {
            TargetKind::Item(item) => {
                let args: Vec<BasicMetadataValueEnum<'ink>> = (0..param_count)
                    .map(|idx| {
                        self.fn_value
                            .get_nth_param(idx + 1)
                            .expect("thunk parameter")
                            .into()
                    })
                    .collect();
                let result = self.gen_call(None, *item, &args);
                (result, ret_ty.is_never())
            }
            TargetKind::Closure { expr, .. } => {
                let body = self.body.clone();
                let Expr::Closure {
                    params,
                    body: closure_body,
                    ..
                } = &body[*expr]
                else {
                    unreachable!("a closure target names a closure expression");
                };

                // The captures: copied out of the object into locals, so the
                // body reads them like any other binding.
                if !target.captures.is_empty() {
                    let object_ty = FnValueTargets::object_type(target, self.hir_types);
                    // Safety: the code of a closure is only ever called with
                    // a value of that closure.
                    let data =
                        unsafe { RuntimeReferenceValue::from_ptr_unchecked(value, object_ty) }
                            .get_data_ptr(&self.builder);
                    for (idx, capture) in target.captures.iter().enumerate() {
                        let field = self
                            .builder
                            .build_struct_gep(object_ty, data, idx as u32 + 1, &capture.name)
                            .expect("a closure object has a field per capture");
                        let ty = object_ty
                            .get_field_type_at_index(idx as u32 + 1)
                            .expect("a closure object has a field per capture");
                        let loaded = self
                            .builder
                            .build_load(ty, field, &capture.name)
                            .expect("failed to load a capture");
                        let slot = self
                            .new_alloca_builder()
                            .build_alloca(ty.as_basic_type_enum(), &capture.name)
                            .expect("failed to allocate a capture");
                        self.builder
                            .build_store(slot, loaded)
                            .expect("failed to store a capture");
                        self.pat_to_local.insert(capture.pat, slot);
                        self.pat_to_name.insert(capture.pat, capture.name.clone());
                    }
                }

                for (idx, &(pat, _)) in params.iter().enumerate() {
                    let param = self
                        .fn_value
                        .get_nth_param(idx as u32 + 1)
                        .expect("a closure parameter");
                    match &body[pat] {
                        Pat::Bind { name } => {
                            let slot = self
                                .new_alloca_builder()
                                .build_alloca(param.get_type(), &name.to_string())
                                .expect("failed to allocate a closure parameter");
                            self.builder
                                .build_store(slot, param)
                                .expect("failed to store a closure parameter");
                            self.pat_to_local.insert(pat, slot);
                            self.pat_to_name.insert(pat, name.to_string());
                        }
                        Pat::Tuple(_) => self.bind_pattern_to_value(pat, param),
                        _ => {}
                    }
                }

                // Calling a closure costs one unit of fuel, like any call.
                self.gen_fuel_check();

                let result = self.gen_expr(*closure_body);
                (result, self.infer[*closure_body].is_never())
            }
        };

        if diverges {
            let terminated = self
                .builder
                .get_insert_block()
                .is_some_and(|block| block.get_terminator().is_some());
            if !terminated {
                self.builder
                    .build_unreachable()
                    .expect("failed to build unreachable");
            }
            self.finish_safety();
            return;
        }
        if ret_ty.is_empty() || ret_ty.is_never() {
            self.builder
                .build_return(None)
                .expect("failed to build return");
        } else if let Some(value) = result {
            self.builder
                .build_return(Some(&value))
                .expect("failed to build return");
        }
        // Undo what a hardware-safety mode set up, before every return.
        self.finish_safety();
        self.unify_aggregate_returns();
    }
}
