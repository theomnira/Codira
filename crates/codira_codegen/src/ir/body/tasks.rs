//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Code generation for structured concurrency: `scope`, `spawn`,
//!   `channel(..)`, `ch <- v` and `<-ch`, as calls into the runtime's task pool
//!   (`codira_runtime::tasks`) through the dispatch table.
//!
//! # Scopes are joined on every exit
//!
//! `scope { .. }` calls `codira.scope_begin` on entry and `codira.scope_end`
//! -- which waits for every task the scope spawned, then frees its channels
//! -- on *every* way out of the block, not just its end:
//!
//! * `return` ends every scope of the function before returning;
//! * `break` ends the scopes opened inside the loop it leaves;
//! * an effect handler abandoning the computation (see `effects`) ends the
//!   scopes between the abort check and wherever control lands;
//! * `resume` in a handler arm ends the arm's scopes before returning.
//!
//! Each of those paths is a separate block, and exactly one of them runs for
//! any given entry into the scope, so each scope ends exactly once.
//!
//! # Tasks
//!
//! `spawn f(a, b)` evaluates `a` and `b`, stores them in a stack struct and
//! hands the runtime a pointer to it, its size, and a *trampoline*
//! `f.task(ptr)` that loads them back and calls `f`. The runtime copies the
//! bytes before `scope_spawn` returns, so the struct can live in the
//! spawning frame. The HIR validator guarantees that `f` takes only scalars
//! and channel handles, so the copy shares nothing with the spawner.
//!
//! # Groups
//!
//! `group { .. }` is a scope whose tasks return values
//! (`codira_runtime::tasks::group`): `codira.group_begin(result_size)` on
//! entry; `spawn f(a)` inside it hands the runtime a trampoline
//! `f.group_task(args, out) -> usize` that stores `f`'s result at `out` and
//! returns 1 when it is an `Err` (for a group of `Result`s). At the end,
//! `codira.group_end` joins the tasks and returns their count, and the value
//! is assembled from the results the runtime kept: an array of all of them,
//! or -- for `Result`s -- the `Ok` payloads of the tasks before the first
//! failure (`codira.group_first_error`), then `Ok(array)` or that task's
//! `Err`. `codira.group_free` releases the results. Every other way out of
//! the block calls `group_end` and `group_free` and reads nothing, exactly
//! where a scope would be ended.
//!
//! # `select`
//!
//! A `select` evaluates every arm's channel and sent value in source order,
//! storing each sent value in a stack slot and allocating a slot for each
//! received one, and lays the operations out as an array of
//! `(channel, kind, slot address)` triples for `codira.channel_select`. The
//! index it returns -- or the operation count, for `default` -- switches to
//! the chosen arm, which loads its received value (if it binds one) and runs
//! its body; the arms meet in one block with a phi of their values.
//!
//! # Sessions
//!
//! `S.open()` asks the runtime for two endpoint handles and makes the tuple
//! of them. On an endpoint, `ep <- v` and `<-ep` go through
//! `codira.session_send` / `codira.session_receive` with the payload's
//! size; `ep.choose(k)` sends `k` as an `i64`, and `offer` receives that
//! `i64` and switches on it to the branch blocks.
use codira_hir::{
    direct_spawns, group_shape, Expr, ExprId, GroupShape, HirDatabase, SelectArm, SelectOp, Ty,
    TyKind,
};
use inkwell::{
    module::Linkage,
    types::BasicTypeEnum,
    values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue},
    AddressSpace, IntPredicate,
};

use super::BodyIrGenerator;
use crate::{
    intrinsics::{self, Intrinsic},
    ir::array::RuntimeArrayValue,
};

/// What a handle on the task stack is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum OwnerKind {
    Scope,
    Group,
}

/// The type the tasks of the group `group` return, and the shape of the
/// group's value; `None` for a group without tasks (rejected by the
/// validator).
pub(crate) fn group_result(
    db: &dyn HirDatabase,
    body: &codira_hir::Body,
    infer: &codira_hir::InferenceResult,
    group: ExprId,
) -> Option<(Ty, GroupShape)> {
    let spawn = *direct_spawns(body, group).first()?;
    let Expr::Spawn { call } = body[spawn] else {
        return None;
    };
    let result = infer[call].clone();
    let shape = group_shape(db, &result);
    Some((result, shape))
}

/// The task-related state of a `BodyIrGenerator`.
#[derive(Default)]
pub(super) struct TaskState<'ink> {
    /// The runtime handles of the `scope`s and `group`s enclosing the code
    /// being generated, innermost last.
    pub(super) scopes: Vec<IntValue<'ink>>,
    /// What each entry of `scopes` is.
    kinds: Vec<OwnerKind>,
    /// For each enclosing loop, innermost last, how many scopes were open
    /// when it started: a `break` ends the ones opened since.
    loop_depths: Vec<usize>,
}

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// Calls a runtime function through its dispatch table slot and returns
    /// its result, if it has one.
    pub(super) fn call_runtime(
        &mut self,
        function: &impl Intrinsic,
        args: &[BasicMetadataValueEnum<'ink>],
    ) -> Option<BasicValueEnum<'ink>> {
        let (fn_ty, fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            function,
        );
        self.builder
            .build_indirect_call(fn_ty, fn_ptr, args, "")
            .expect("failed to build call to a runtime function")
            .try_as_basic_value()
            .basic()
    }

    fn address_of(&self, ptr: inkwell::values::PointerValue<'ink>) -> IntValue<'ink> {
        self.builder
            .build_ptr_to_int(ptr, self.hir_types.get_usize_type(), "address")
            .expect("failed to take an address")
    }

    /// Notes that a loop starts here, for `break`.
    pub(super) fn enter_loop_scopes(&mut self) {
        self.tasks.loop_depths.push(self.tasks.scopes.len());
    }

    /// Notes that the innermost loop has been generated.
    pub(super) fn leave_loop_scopes(&mut self) {
        self.tasks.loop_depths.pop();
    }

    /// Ends, innermost first, every open scope beyond the first `keep`,
    /// without forgetting them: code generation continues on other paths
    /// that are still inside them.
    pub(super) fn gen_end_scopes(&mut self, keep: usize) {
        let from = keep.min(self.tasks.scopes.len());
        let open: Vec<(IntValue<'ink>, OwnerKind)> = self.tasks.scopes[from..]
            .iter()
            .copied()
            .zip(self.tasks.kinds[from..].iter().copied())
            .rev()
            .collect();
        for (handle, kind) in open {
            match kind {
                OwnerKind::Scope => {
                    self.call_runtime(&intrinsics::scope_end, &[handle.into()]);
                }
                OwnerKind::Group => {
                    self.call_runtime(&intrinsics::group_end, &[handle.into()]);
                    self.call_runtime(&intrinsics::group_free, &[handle.into()]);
                }
            }
        }
    }

    /// The handle of the innermost `scope` (not `group`): the one that owns
    /// the channels and sessions created here.
    fn innermost_scope(&self) -> Option<IntValue<'ink>> {
        self.tasks
            .scopes
            .iter()
            .zip(&self.tasks.kinds)
            .rev()
            .find(|(_, kind)| **kind == OwnerKind::Scope)
            .map(|(handle, _)| *handle)
    }

    /// Before a `break`: ends the scopes opened inside the loop it leaves.
    pub(super) fn gen_end_loop_scopes(&mut self) {
        let keep = self.tasks.loop_depths.last().copied().unwrap_or(0);
        self.gen_end_scopes(keep);
    }

    /// `scope { body }`.
    pub(super) fn gen_scope(&mut self, body: ExprId) -> Option<BasicValueEnum<'ink>> {
        let scope = self
            .call_runtime(&intrinsics::scope_begin, &[])
            .expect("`codira.scope_begin` returns a handle")
            .into_int_value();
        self.tasks.scopes.push(scope);
        self.tasks.kinds.push(OwnerKind::Scope);
        let value = self.gen_expr(body);
        self.tasks.scopes.pop();
        self.tasks.kinds.pop();
        if value.is_some() {
            self.call_runtime(&intrinsics::scope_end, &[scope.into()]);
        }
        value
    }

    /// `group { body }`.
    pub(super) fn gen_group(&mut self, expr: ExprId, body: ExprId) -> Option<BasicValueEnum<'ink>> {
        let (result_ty, shape) = group_result(self.db, &self.body, &self.infer, expr)
            .expect("the task validator rejects a group without tasks");
        let result_llvm = self
            .hir_types
            .get_basic_type(&result_ty)
            .expect("a task's result has a basic type");
        let usize_ty = self.hir_types.get_usize_type();
        let size = usize_ty.const_int(self.hir_types.target_data().get_abi_size(&result_llvm), false);
        let group = self
            .call_runtime(&intrinsics::group_begin, &[size.into()])
            .expect("`codira.group_begin` returns a handle")
            .into_int_value();
        self.tasks.scopes.push(group);
        self.tasks.kinds.push(OwnerKind::Group);
        let value = self.gen_expr(body);
        self.tasks.scopes.pop();
        self.tasks.kinds.pop();
        value?;

        let count = self
            .call_runtime(&intrinsics::group_end, &[group.into()])
            .expect("`codira.group_end` returns the task count")
            .into_int_value();
        let slot = self
            .new_alloca_builder()
            .build_alloca(result_llvm, "group.result")
            .expect("failed to allocate a result slot");
        let value = match &shape {
            GroupShape::Array { value } => {
                let array = self.gen_group_array(group, value, count, slot, result_llvm, None);
                array.into()
            }
            GroupShape::Result {
                ok,
                err,
                value,
                array,
            } => {
                let first_error = self
                    .call_runtime(&intrinsics::group_first_error, &[group.into()])
                    .expect("`codira.group_first_error` returns an index")
                    .into_int_value();
                let result_args = codira_hir::mono::enum_args(&result_ty);
                let ok_slot = ok.slots_at(self.db, &result_args)[0] as u32 + 1;
                let err_slot = err.slots_at(self.db, &result_args)[0] as u32 + 1;
                let oks =
                    self.gen_group_array(group, array, first_error, slot, result_llvm, Some(ok_slot));
                let failed = self
                    .builder
                    .build_int_compare(IntPredicate::ULT, first_error, count, "group.failed")
                    .expect("failed to compare the first error");
                let err_block = self.context.append_basic_block(self.fn_value, "group.err");
                let ok_block = self.context.append_basic_block(self.fn_value, "group.ok");
                let done_block = self.context.append_basic_block(self.fn_value, "group.done");
                self.builder
                    .build_conditional_branch(failed, err_block, ok_block)
                    .expect("failed to branch on a group's failure");

                let value_args = codira_hir::mono::enum_args(value);
                let value_type = self.hir_types.get_enum_type(value);

                self.builder.position_at_end(err_block);
                let address = self.address_of(slot);
                self.call_runtime(
                    &intrinsics::group_result,
                    &[group.into(), first_error.into(), address.into()],
                );
                let failed_result = self
                    .builder
                    .build_load(result_llvm, slot, "group.failed_result")
                    .expect("failed to load a task result")
                    .into_struct_value();
                let error = self
                    .builder
                    .build_extract_value(failed_result, err_slot, "group.error")
                    .expect("the `Err` slot exists in the layout");
                let err_value = self.gen_result_variant(value_type, err.index(), error, err.slots_at(self.db, &value_args)[0] as u32 + 1);
                let err_end = self.builder.get_insert_block().expect("in a block");
                self.builder
                    .build_unconditional_branch(done_block)
                    .expect("failed to branch to the group's end");

                self.builder.position_at_end(ok_block);
                let ok_value = self.gen_result_variant(value_type, ok.index(), oks.into(), ok.slots_at(self.db, &value_args)[0] as u32 + 1);
                let ok_end = self.builder.get_insert_block().expect("in a block");
                self.builder
                    .build_unconditional_branch(done_block)
                    .expect("failed to branch to the group's end");

                self.builder.position_at_end(done_block);
                let phi = self
                    .builder
                    .build_phi(value_type, "group.value")
                    .expect("failed to merge a group's value");
                phi.add_incoming(&[(&err_value, err_end), (&ok_value, ok_end)]);
                phi.as_basic_value()
            }
        };
        self.call_runtime(&intrinsics::group_free, &[group.into()]);
        Some(value)
    }

    /// `Variant(payload)` of the enum type `ty`, the payload in field `slot`.
    fn gen_result_variant(
        &self,
        ty: inkwell::types::StructType<'ink>,
        tag: u32,
        payload: BasicValueEnum<'ink>,
        slot: u32,
    ) -> BasicValueEnum<'ink> {
        let tagged = self
            .builder
            .build_insert_value(
                ty.const_zero(),
                self.context.i32_type().const_int(u64::from(tag), false),
                0,
                "variant",
            )
            .expect("failed to set an enum tag");
        self.builder
            .build_insert_value(tagged, payload, slot, "variant")
            .expect("failed to set an enum payload")
            .into_struct_value()
            .into()
    }

    /// An array of type `array_ty` holding the first `count` results of
    /// `group` (each loaded through `slot`; with `field`, that field of it).
    fn gen_group_array(
        &mut self,
        group: IntValue<'ink>,
        array_ty: &Ty,
        count: IntValue<'ink>,
        slot: PointerValue<'ink>,
        result_llvm: BasicTypeEnum<'ink>,
        field: Option<u32>,
    ) -> RuntimeArrayValue<'ink> {
        let array = self.alloc_array_of(array_ty, count);
        let index_ty = count.get_type();
        let before = self
            .builder
            .get_insert_block()
            .expect("generating code inside a block");
        let cond_block = self.context.append_basic_block(self.fn_value, "group.cond");
        let body_block = self.context.append_basic_block(self.fn_value, "group.body");
        let done_block = self.context.append_basic_block(self.fn_value, "group.collected");
        self.builder
            .build_unconditional_branch(cond_block)
            .expect("failed to build a branch");

        self.builder.position_at_end(cond_block);
        let index = self
            .builder
            .build_phi(index_ty, "group.i")
            .expect("failed to build an index");
        let index_value = index.as_basic_value().into_int_value();
        let in_range = self
            .builder
            .build_int_compare(IntPredicate::ULT, index_value, count, "group.in_range")
            .expect("failed to build a comparison");
        self.builder
            .build_conditional_branch(in_range, body_block, done_block)
            .expect("failed to build a branch");

        self.builder.position_at_end(body_block);
        let address = self.address_of(slot);
        self.call_runtime(
            &intrinsics::group_result,
            &[group.into(), index_value.into(), address.into()],
        );
        let mut element = self
            .builder
            .build_load(result_llvm, slot, "group.element")
            .expect("failed to load a task result");
        if let Some(field) = field {
            element = self
                .builder
                .build_extract_value(element.into_struct_value(), field, "group.ok")
                .expect("the `Ok` slot exists in the layout");
        }
        let length_index = self.index_as_length(index_value, array);
        self.store_element(array, length_index, element);
        let next = self
            .builder
            .build_int_add(index_value, index_ty.const_int(1, false), "group.next")
            .expect("failed to build an increment");
        let body_end = self
            .builder
            .get_insert_block()
            .expect("generating code inside a block");
        self.builder
            .build_unconditional_branch(cond_block)
            .expect("failed to build a back edge");
        index.add_incoming(&[(&index_ty.const_zero(), before), (&next, body_end)]);

        self.builder.position_at_end(done_block);
        let length = self.index_as_length(count, array);
        self.set_array_len(array, length);
        array
    }

    /// `value` (a `usize`) cast to `array`'s length type.
    fn index_as_length(&self, value: IntValue<'ink>, array: RuntimeArrayValue<'ink>) -> IntValue<'ink> {
        let length_ty = array.length_ty();
        if value.get_type() == length_ty {
            value
        } else {
            self.builder
                .build_int_cast(value, length_ty, "group.len")
                .expect("failed to cast an array length")
        }
    }

    /// `spawn f(args)`.
    pub(super) fn gen_spawn(&mut self, call: ExprId) -> Option<BasicValueEnum<'ink>> {
        let body = self.body.clone();
        let Expr::Call { callee, args } = &body[call] else {
            unreachable!("the task validator rejects a `spawn` of anything but a call");
        };
        let Some(codira_hir::CallableDef::Function(function)) =
            self.infer[*callee].as_callable_def()
        else {
            unreachable!("the task validator rejects a `spawn` of anything but a function");
        };

        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.gen_expr(*arg)?);
        }

        let scope = *self
            .tasks
            .scopes
            .last()
            .expect("the task validator rejects a `spawn` outside a `scope`");
        let in_group = self.tasks.kinds.last() == Some(&OwnerKind::Group);
        let trampoline = if in_group {
            self.group_trampoline(function)
        } else {
            self.task_trampoline(function)
        };

        let field_types: Vec<BasicTypeEnum<'ink>> =
            values.iter().map(BasicValueEnum::get_type).collect();
        let args_ty = self.context.struct_type(&field_types, false);
        let args_ptr = self
            .new_alloca_builder()
            .build_alloca(args_ty, "task_args")
            .expect("failed to allocate task arguments");
        for (idx, value) in values.into_iter().enumerate() {
            let field = self
                .builder
                .build_struct_gep(args_ty, args_ptr, idx as u32, "task_arg")
                .expect("failed to address task argument");
            self.builder
                .build_store(field, value)
                .expect("failed to store task argument");
        }
        let size = self
            .hir_types
            .get_usize_type()
            .const_int(self.hir_types.target_data().get_abi_size(&args_ty), false);
        let entry = self.address_of(trampoline.as_global_value().as_pointer_value());
        let args_address = self.address_of(args_ptr);
        let spawn_args = [scope.into(), entry.into(), args_address.into(), size.into()];
        if in_group {
            self.call_runtime(&intrinsics::group_spawn, &spawn_args);
        } else {
            self.call_runtime(&intrinsics::scope_spawn, &spawn_args);
        }
        Some(self.gen_empty())
    }

    /// `f.group_task(args, out)`: loads `f`'s arguments from the struct
    /// `args` points at, calls `f`, stores its result at `out` and returns
    /// 1 if it is an `Err` of a `Result` (0 otherwise).
    fn group_trampoline(&self, function: codira_hir::Function) -> FunctionValue<'ink> {
        let target = *self
            .function_map
            .get(&codira_hir::mono::Instance::of(function))
            .expect("the task validator only allows functions of this module");
        let name = format!("{}.group_task", target.get_name().to_string_lossy());
        if let Some(existing) = self.module.get_function(&name) {
            return existing;
        }
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let usize_ty = self.hir_types.get_usize_type();
        let fn_type = usize_ty.fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let trampoline = self
            .module
            .add_function(&name, fn_type, Some(Linkage::Private));

        let builder = self.context.create_builder();
        builder.position_at_end(self.context.append_basic_block(trampoline, "task"));
        let args_ptr = trampoline
            .get_nth_param(0)
            .expect("a trampoline takes its argument pointer")
            .into_pointer_value();
        let out_ptr = trampoline
            .get_nth_param(1)
            .expect("a group trampoline takes its result pointer")
            .into_pointer_value();
        let sig = function
            .ty(self.db)
            .callable_sig(self.db)
            .expect("a function has a signature");
        let param_types: Vec<BasicTypeEnum<'ink>> = sig
            .params()
            .iter()
            .map(|ty| {
                self.hir_types
                    .get_basic_type(ty)
                    .expect("a task parameter has a basic type")
            })
            .collect();
        let args_ty = self.context.struct_type(&param_types, false);
        let mut args: Vec<BasicMetadataValueEnum<'ink>> = Vec::with_capacity(param_types.len());
        for (idx, ty) in param_types.iter().enumerate() {
            let field = builder
                .build_struct_gep(args_ty, args_ptr, idx as u32, "arg")
                .expect("failed to address task argument");
            args.push(
                builder
                    .build_load(*ty, field, "arg")
                    .expect("failed to load task argument")
                    .into(),
            );
        }
        let result = builder
            .build_call(target, &args, "result")
            .expect("failed to call the task's function")
            .try_as_basic_value()
            .basic()
            .expect("the validator rejects a group task that returns nothing");
        builder
            .build_store(out_ptr, result)
            .expect("failed to store a task's result");
        let failed = match group_shape(self.db, sig.ret()) {
            GroupShape::Result { err, .. } => {
                let tag = builder
                    .build_extract_value(result.into_struct_value(), 0, "tag")
                    .expect("an enum has a tag")
                    .into_int_value();
                let is_err = builder
                    .build_int_compare(
                        IntPredicate::EQ,
                        tag,
                        tag.get_type().const_int(u64::from(err.index()), false),
                        "is_err",
                    )
                    .expect("failed to compare an enum tag");
                builder
                    .build_int_z_extend(is_err, usize_ty, "failed")
                    .expect("failed to widen a flag")
            }
            GroupShape::Array { .. } => usize_ty.const_zero(),
        };
        builder
            .build_return(Some(&failed))
            .expect("failed to return from a task");
        trampoline
    }

    /// `f.task(ptr)`: loads `f`'s arguments from the struct `ptr` points at
    /// and calls `f` with them, discarding its result.
    fn task_trampoline(&self, function: codira_hir::Function) -> FunctionValue<'ink> {
        let target = *self
            .function_map
            .get(&codira_hir::mono::Instance::of(function))
            .expect("the task validator only allows functions of this module");
        let name = format!("{}.task", target.get_name().to_string_lossy());
        if let Some(existing) = self.module.get_function(&name) {
            return existing;
        }
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let fn_type = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let trampoline = self
            .module
            .add_function(&name, fn_type, Some(Linkage::Private));

        let builder = self.context.create_builder();
        builder.position_at_end(self.context.append_basic_block(trampoline, "task"));
        let args_ptr = trampoline
            .get_nth_param(0)
            .expect("a trampoline takes its argument pointer")
            .into_pointer_value();
        let param_types: Vec<BasicTypeEnum<'ink>> = function
            .ty(self.db)
            .callable_sig(self.db)
            .expect("a function has a signature")
            .params()
            .iter()
            .map(|ty| {
                self.hir_types
                    .get_basic_type(ty)
                    .expect("a task parameter has a basic type")
            })
            .collect();
        let args_ty = self.context.struct_type(&param_types, false);
        let mut args: Vec<BasicMetadataValueEnum<'ink>> = Vec::with_capacity(param_types.len());
        for (idx, ty) in param_types.iter().enumerate() {
            let field = builder
                .build_struct_gep(args_ty, args_ptr, idx as u32, "arg")
                .expect("failed to address task argument");
            args.push(
                builder
                    .build_load(*ty, field, "arg")
                    .expect("failed to load task argument")
                    .into(),
            );
        }
        builder
            .build_call(target, &args, "")
            .expect("failed to call the task's function");
        builder
            .build_return(None)
            .expect("failed to return from a task");
        trampoline
    }

    /// The `channel(capacity)` builtin: a channel owned by the innermost
    /// scope.
    pub(super) fn gen_channel_new(
        &mut self,
        expr: ExprId,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let capacity = self.gen_expr(args[0])?;
        let element_size = self.channel_element_size(expr);
        let scope = self
            .innermost_scope()
            .expect("the task validator rejects a channel created outside a `scope`");
        self.call_runtime(
            &intrinsics::channel_new,
            &[scope.into(), capacity.into(), element_size.into()],
        )
    }

    /// The size, as a `usize` constant, of the elements of the channel `expr`
    /// evaluates to.
    fn channel_element_size(&self, expr: ExprId) -> IntValue<'ink> {
        let size = self
            .hir_types
            .target_data()
            .get_abi_size(&self.channel_element_type(expr));
        self.hir_types.get_usize_type().const_int(size, false)
    }

    fn channel_element_type(&self, expr: ExprId) -> BasicTypeEnum<'ink> {
        let TyKind::Channel(element) = self.infer[expr].interned() else {
            unreachable!("inference types a channel operand as a channel");
        };
        self.hir_types
            .get_basic_type(element)
            .expect("the task validator only allows scalar channel elements")
    }

    /// `channel <- value`, or a send on a session endpoint.
    pub(super) fn gen_channel_send(
        &mut self,
        channel: ExprId,
        value: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        let handle = self.gen_expr(channel)?;
        let value = self.gen_expr(value)?;
        if self.is_endpoint(channel) {
            self.session_send(handle.into_int_value(), value);
            return Some(self.gen_empty());
        }
        let element_ty = self.channel_element_type(channel);
        let slot = self
            .new_alloca_builder()
            .build_alloca(element_ty, "sent")
            .expect("failed to allocate a channel element");
        self.builder
            .build_store(slot, value)
            .expect("failed to store a channel element");
        let address = self.address_of(slot);
        self.call_runtime(&intrinsics::channel_send, &[handle.into(), address.into()]);
        Some(self.gen_empty())
    }

    /// Whether `expr` evaluates to a session endpoint.
    pub(super) fn is_endpoint(&self, expr: ExprId) -> bool {
        matches!(self.infer[expr].interned(), TyKind::Session(..))
    }

    /// `S.open()`: the tuple of the two endpoints.
    pub(super) fn gen_session_open(&mut self, expr: ExprId) -> Option<BasicValueEnum<'ink>> {
        let usize_ty = self.hir_types.get_usize_type();
        let scope = self
            .innermost_scope()
            .expect("the task validator rejects a session opened outside a `scope`");
        let out = self
            .new_alloca_builder()
            .build_alloca(usize_ty.array_type(2), "endpoints")
            .expect("failed to allocate the endpoints");
        let address = self.address_of(out);
        self.call_runtime(&intrinsics::session_open, &[scope.into(), address.into()]);
        let tuple_ty = self
            .hir_types
            .get_basic_type(&self.infer[expr])
            .expect("a pair of endpoints has a basic type")
            .into_struct_type();
        let mut tuple = tuple_ty.get_undef();
        for idx in 0..2u32 {
            // SAFETY: `idx` is below the array's length of two.
            let slot = unsafe {
                self.builder
                    .build_gep(
                        usize_ty,
                        out,
                        &[usize_ty.const_int(u64::from(idx), false)],
                        "endpoint",
                    )
                    .expect("failed to address an endpoint")
            };
            let handle = self
                .builder
                .build_load(usize_ty, slot, "endpoint")
                .expect("failed to load an endpoint");
            tuple = self
                .builder
                .build_insert_value(tuple, handle, idx, "endpoints")
                .expect("failed to build the endpoint pair")
                .into_struct_value();
        }
        Some(tuple.into())
    }

    /// Sends `value`, a `ty`, on the endpoint `handle`.
    fn session_send(&mut self, handle: IntValue<'ink>, value: BasicValueEnum<'ink>) {
        let slot = self
            .new_alloca_builder()
            .build_alloca(value.get_type(), "sent")
            .expect("failed to allocate a session payload");
        self.builder
            .build_store(slot, value)
            .expect("failed to store a session payload");
        let size = self.size_of(value.get_type());
        let address = self.address_of(slot);
        self.call_runtime(
            &intrinsics::session_send,
            &[handle.into(), address.into(), size.into()],
        );
    }

    /// Receives a value of LLVM type `ty` on the endpoint `handle`.
    fn session_receive(
        &mut self,
        handle: IntValue<'ink>,
        ty: BasicTypeEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let slot = self
            .new_alloca_builder()
            .build_alloca(ty, "received")
            .expect("failed to allocate a session payload");
        let size = self.size_of(ty);
        let address = self.address_of(slot);
        self.call_runtime(
            &intrinsics::session_receive,
            &[handle.into(), address.into(), size.into()],
        );
        self.builder
            .build_load(ty, slot, "received")
            .expect("failed to load a session payload")
    }

    fn size_of(&self, ty: BasicTypeEnum<'ink>) -> IntValue<'ink> {
        let size = self.hir_types.target_data().get_abi_size(&ty);
        self.hir_types.get_usize_type().const_int(size, false)
    }

    /// `ep.choose(k)`: sends the branch index.
    pub(super) fn gen_session_choose(
        &mut self,
        receiver: ExprId,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let handle = self.gen_expr(receiver)?.into_int_value();
        let Some(Expr::Literal(codira_hir::Literal::Int(branch))) =
            args.first().map(|&arg| &self.body[arg])
        else {
            unreachable!("the session checker rejects a `choose` without a literal branch");
        };
        let branch = self.context.i64_type().const_int(
            u64::try_from(branch.value).expect("the session checker bounds the branch"),
            false,
        );
        self.session_send(handle, branch.into());
        Some(self.gen_empty())
    }

    /// `offer ep { .. } or { .. }`.
    pub(super) fn gen_offer(
        &mut self,
        endpoint: ExprId,
        branches: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let handle = self.gen_expr(endpoint)?.into_int_value();
        let i64_ty = self.context.i64_type();
        let chosen = self.session_receive(handle, i64_ty.into()).into_int_value();
        let blocks: Vec<_> = branches
            .iter()
            .map(|_| {
                self.context
                    .append_basic_block(self.fn_value, "offer_branch")
            })
            .collect();
        let none = self.context.append_basic_block(self.fn_value, "offer_none");
        let cases: Vec<_> = blocks
            .iter()
            .enumerate()
            .map(|(idx, block)| (i64_ty.const_int(idx as u64, false), *block))
            .collect();
        self.builder
            .build_switch(chosen, none, &cases)
            .expect("failed to build the offer switch");
        // Both sides follow dual protocols, so the other side only ever
        // chooses a branch that exists.
        self.builder.position_at_end(none);
        self.builder
            .build_unreachable()
            .expect("failed to build unreachable");

        let merge = self
            .context
            .append_basic_block(self.fn_value, "offer_merge");
        let mut incoming = Vec::new();
        for (idx, &branch) in branches.iter().enumerate() {
            self.builder.position_at_end(blocks[idx]);
            if let Some(value) = self.gen_expr(branch) {
                let block = self
                    .builder
                    .get_insert_block()
                    .expect("a branch ends in a block");
                self.builder
                    .build_unconditional_branch(merge)
                    .expect("failed to branch out of an offer");
                incoming.push((value, block));
            }
        }
        let Some(&(first, _)) = incoming.first() else {
            merge
                .remove_from_function()
                .expect("the merge block is in the function");
            return None;
        };
        self.builder.position_at_end(merge);
        let phi = self
            .builder
            .build_phi(first.get_type(), "offer_value")
            .expect("failed to build phi");
        for (value, block) in &incoming {
            phi.add_incoming(&[(value, *block)]);
        }
        Some(phi.as_basic_value())
    }

    /// The LLVM type of the value `<-ep` at `expr` receives.
    fn received_type(&self, expr: ExprId) -> BasicTypeEnum<'ink> {
        let ty: &Ty = &self.infer[expr];
        self.hir_types
            .get_basic_type(ty)
            .expect("a session payload is a scalar")
    }

    /// `select { arm, .. }`.
    pub(super) fn gen_select(
        &mut self,
        _expr: ExprId,
        arms: &[SelectArm],
    ) -> Option<BasicValueEnum<'ink>> {
        let usize_ty = self.hir_types.get_usize_type();

        // The operands, in source order: (arm, channel, kind, slot).
        let mut cases: Vec<(usize, IntValue<'ink>, u64, PointerValue<'ink>)> = Vec::new();
        let mut slots: Vec<Option<(BasicTypeEnum<'ink>, PointerValue<'ink>)>> =
            vec![None; arms.len()];
        let mut default_arm = None;
        for (idx, arm) in arms.iter().enumerate() {
            match arm.op {
                SelectOp::Receive { channel } => {
                    let handle = self.gen_expr(channel)?.into_int_value();
                    let element_ty = self.channel_element_type(channel);
                    let slot = self
                        .new_alloca_builder()
                        .build_alloca(element_ty, "selected")
                        .expect("failed to allocate a channel element");
                    cases.push((idx, handle, 0, slot));
                    slots[idx] = Some((element_ty, slot));
                }
                SelectOp::Send { channel, value } => {
                    let handle = self.gen_expr(channel)?.into_int_value();
                    let value = self.gen_expr(value)?;
                    let element_ty = self.channel_element_type(channel);
                    let slot = self
                        .new_alloca_builder()
                        .build_alloca(element_ty, "offered")
                        .expect("failed to allocate a channel element");
                    self.builder
                        .build_store(slot, value)
                        .expect("failed to store a channel element");
                    cases.push((idx, handle, 1, slot));
                }
                SelectOp::Default => default_arm = Some(idx),
                SelectOp::Invalid { .. } => {
                    unreachable!(
                        "the task validator rejects a `select` arm that is not a channel operation"
                    )
                }
            }
        }

        // The case array: three `usize`s per operation.
        let words = u32::try_from(cases.len() * 3)
            .expect("a `select` has fewer than 2^32 arms")
            .max(1);
        let array = self
            .new_alloca_builder()
            .build_alloca(usize_ty.array_type(words), "select_cases")
            .expect("failed to allocate the select cases");
        let mut word = 0u64;
        for &(_, handle, kind, slot) in &cases {
            let address = self.address_of(slot);
            for value in [handle, usize_ty.const_int(kind, false), address] {
                // SAFETY: `word` is below the array's length.
                let field = unsafe {
                    self.builder
                        .build_gep(usize_ty, array, &[usize_ty.const_int(word, false)], "case")
                        .expect("failed to address a select case")
                };
                self.builder
                    .build_store(field, value)
                    .expect("failed to store a select case");
                word += 1;
            }
        }
        let array_address = self.address_of(array);
        let count = usize_ty.const_int(cases.len() as u64, false);
        let has_default = usize_ty.const_int(u64::from(default_arm.is_some()), false);
        let chosen = self
            .call_runtime(
                &intrinsics::channel_select,
                &[array_address.into(), count.into(), has_default.into()],
            )
            .expect("`codira.channel_select` returns the chosen case")
            .into_int_value();

        // Switch to the chosen arm.
        let arm_blocks: Vec<_> = (0..arms.len())
            .map(|_| self.context.append_basic_block(self.fn_value, "select_arm"))
            .collect();
        let otherwise = if let Some(idx) = default_arm {
            arm_blocks[idx]
        } else {
            // The runtime returns `count` only for a `default` arm.
            let block = self
                .context
                .append_basic_block(self.fn_value, "select_none");
            let current = self.builder.get_insert_block();
            self.builder.position_at_end(block);
            self.builder
                .build_unreachable()
                .expect("failed to build unreachable");
            if let Some(current) = current {
                self.builder.position_at_end(current);
            }
            block
        };
        let switch_cases: Vec<_> = cases
            .iter()
            .enumerate()
            .map(|(case, &(arm, ..))| (usize_ty.const_int(case as u64, false), arm_blocks[arm]))
            .collect();
        self.builder
            .build_switch(chosen, otherwise, &switch_cases)
            .expect("failed to build the select switch");

        let merge = self
            .context
            .append_basic_block(self.fn_value, "select_merge");
        let mut incoming = Vec::new();
        for (idx, arm) in arms.iter().enumerate() {
            self.builder.position_at_end(arm_blocks[idx]);
            if let (Some(binding), Some((element_ty, slot))) = (arm.binding, slots[idx]) {
                let value = self
                    .builder
                    .build_load(element_ty, slot, "received")
                    .expect("failed to load a channel element");
                self.bind_pattern_to_value(binding, value);
            }
            if let Some(value) = self.gen_expr(arm.body) {
                let block = self
                    .builder
                    .get_insert_block()
                    .expect("an arm ends in a block");
                self.builder
                    .build_unconditional_branch(merge)
                    .expect("failed to branch out of a select arm");
                incoming.push((value, block));
            }
        }

        let Some(&(first, _)) = incoming.first() else {
            // Every arm diverges, so the `select` does too.
            merge
                .remove_from_function()
                .expect("the merge block is in the function");
            return None;
        };
        self.builder.position_at_end(merge);
        let phi = self
            .builder
            .build_phi(first.get_type(), "select_value")
            .expect("failed to build phi");
        for (value, block) in &incoming {
            phi.add_incoming(&[(value, *block)]);
        }
        Some(phi.as_basic_value())
    }

    /// `<-channel`, or a receive on a session endpoint (`expr` is the
    /// receive itself).
    pub(super) fn gen_channel_receive(
        &mut self,
        expr: ExprId,
        channel: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        let handle = self.gen_expr(channel)?;
        if self.is_endpoint(channel) {
            let ty = self.received_type(expr);
            return Some(self.session_receive(handle.into_int_value(), ty));
        }
        let element_ty = self.channel_element_type(channel);
        let slot = self
            .new_alloca_builder()
            .build_alloca(element_ty, "received")
            .expect("failed to allocate a channel element");
        let address = self.address_of(slot);
        self.call_runtime(
            &intrinsics::channel_receive,
            &[handle.into(), address.into()],
        );
        Some(
            self.builder
                .build_load(element_ty, slot, "received")
                .expect("failed to load a channel element"),
        )
    }
}
