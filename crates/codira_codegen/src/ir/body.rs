//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{collections::HashMap, sync::Arc};

use codira_abi as abi;
use codira_hir::{
    mono::Instance, ArithOp, BinaryOp, Body, BuiltinFn, CmpOp, Expr, ExprId, HirDatabase,
    HirDisplay, InferenceResult, Literal, LogicOp, MatchArm, Name, Ordering, Pat, PatId, Path,
    ResolveBitness, Resolver, Statement, Ty, TyKind, UnaryOp, ValueNs,
};
use inkwell::{
    basic_block::BasicBlock,
    builder::Builder,
    context::Context,
    types::{BasicMetadataTypeEnum, BasicType},
    values::{
        AggregateValueEnum, BasicMetadataValueEnum, BasicValueEnum, FloatValue, FunctionValue,
        GlobalValue, IntValue, PointerValue, StructValue,
    },
    AddressSpace, FloatPredicate, InlineAsmDialect, IntPredicate,
};

use crate::{
    intrinsics,
    ir::{
        array_tbaa::{self, ArrayAccess},
        dispatch_table::DispatchTable,
        intrinsic_ops,
        ty::HirTypeCache,
        type_table::TypeTable,
        FunctionMap, RuntimeArrayValue, RuntimeReferenceValue,
    },
    module_group::ModuleGroup,
    value::Global,
};

mod arrays;
mod closures;
mod effects;
mod fabric;
mod format;
mod merge;
mod pipelines;
mod propagate;
mod safety;
mod strings;
mod tasks;

pub(crate) use effects::has_evidence;

type BreakSources<'ink> = Vec<Option<(BasicValueEnum<'ink>, BasicBlock<'ink>)>>;

struct LoopInfo<'ink> {
    break_values: BreakSources<'ink>,
    exit_block: BasicBlock<'ink>,
    /// Where `continue` jumps: a `while` loop's condition, a `loop`'s top,
    /// a `for` loop's increment.
    continue_block: BasicBlock<'ink>,
}

#[derive(Clone)]
pub(crate) struct ExternalGlobals<'ink> {
    pub alloc_handle: Option<GlobalValue<'ink>>,
    /// The `fuelCounter` global (a pointer to the runtime's `i64` counter),
    /// present when the module is fuel-instrumented.
    pub fuel_counter: Option<GlobalValue<'ink>>,
    pub dispatch_table: Option<GlobalValue<'ink>>,
    pub type_table: Option<Global<'ink, [*const std::ffi::c_void]>>,
    /// The experimental hardware-safety mode (see `safety`).
    pub hardware_safety: crate::HardwareSafety,
    /// Where `safety` records code the mode refuses to build.
    pub safety_refusals: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
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
    function_map: &'t FunctionMap<'ink>,
    dispatch_table: &'t DispatchTable<'ink>,
    type_table: &'t TypeTable<'ink>,
    hir_types: &'t HirTypeCache<'db, 'ink>,
    active_loop: Option<LoopInfo<'ink>>,
    hir_function: codira_hir::Function,
    /// The instance of `hir_function` being generated: the function itself
    /// when it is not generic. `infer` is the function's inference result
    /// read at this instance, so every type in it is concrete.
    instance: Instance,
    external_globals: ExternalGlobals<'ink>,
    module_group: &'t ModuleGroup,
    /// The optimizer and proven rules Eidos runs with; `None` skips Eidos
    /// entirely (`--mode=dev`). See `crate::EidosOptions`.
    eidos: Option<crate::EidosRun>,
    /// Evidence in scope, `handle` frames and the arm being generated. See
    /// `effects`.
    effects: effects::EffectState<'ink>,
    /// Open `scope`s and loops, for joining tasks on every exit. See
    /// `tasks`.
    tasks: tasks::TaskState<'ink>,
    /// What the hardware-safety mode undoes before each return. See
    /// `safety`.
    safety: safety::SafetyState<'ink>,
    /// The slot holding the last result of a format plan in the function
    /// being generated, from which the next plan continues (see `fabric`).
    plan_arena: Option<(FunctionValue<'ink>, PointerValue<'ink>)>,
}

impl<'db, 'ink, 't> BodyIrGenerator<'db, 'ink, 't> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        context: &'ink Context,
        module: &'t inkwell::module::Module<'ink>,
        db: &'db dyn HirDatabase,
        function: (Instance, Arc<InferenceResult>, FunctionValue<'ink>),
        function_map: &'t FunctionMap<'ink>,
        dispatch_table: &'t DispatchTable<'ink>,
        type_table: &'t TypeTable<'ink>,
        external_globals: ExternalGlobals<'ink>,
        hir_types: &'t HirTypeCache<'db, 'ink>,
        module_group: &'t ModuleGroup,
        eidos: Option<crate::EidosRun>,
    ) -> Self {
        let (instance, infer, ir_function) = function;
        let hir_function = instance.function();

        // Get the type information from the `codira_hir::Function`
        let body = hir_function.body(db);

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
            instance,
            external_globals,
            hir_types,
            module_group,
            eidos,
            effects: effects::EffectState::default(),
            tasks: tasks::TaskState::default(),
            safety: safety::SafetyState::default(),
            plan_arena: None,
        }
    }

    /// Generates IR for the body of the function.
    pub fn gen_fn_body(&mut self) {
        // `@runtime_checked` preconditions (see `codira_hir::verify`) have to
        // run before anything else, so a function that carries them never
        // takes the constant-folding shortcut below, which would skip them.
        let runtime_checks = !self.body.contracts().requires.is_empty()
            && codira_hir::verify::is_runtime_checked(self.db, self.hir_function);

        // Eidos fast path: if the whole body reduces to a compile-time
        // constant (after inlining, unrolling, equality saturation and
        // interpretation -- see `crate::eidos_fold`), emit just that
        // constant. Declining is always safe: the ordinary lowering
        // below produces correct code either way. This is the one place
        // Eidos enters a build, so `--mode=dev` (no optimizer) skips it
        // here and nowhere else.
        //
        // Not for an instance of a generic function: the fold lowers the
        // declaration's body, which is written in terms of its parameters.
        if let Some(eidos) = self.eidos.as_ref().filter(|_| {
            !runtime_checks
                && !self.instance.is_generic()
                && !self.instance.sig(self.db).ret().is_never()
        }) {
            if let Some(value) = crate::eidos_fold::fold_to_constant(
                self.db,
                self.hir_function,
                self.fn_value,
                eidos,
            ) {
                self.builder
                    .build_return(Some(&value))
                    .expect("failed to build return for a constant-folded function");
                return;
            }
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

        // A `uses` function's evidence follows its ordinary parameters.
        self.bind_evidence_params(param_pats.len());

        if runtime_checks {
            self.gen_precondition_checks();
        }

        // Every call costs one unit of fuel (when instrumented)
        self.gen_fuel_check();

        // Generate code for the body of the function
        let ret_value = self.gen_expr(self.body.body_expr());

        // Construct a return statement from the returned value of the body if a return
        // is expected in the first place. If the return type of the body is
        // `never` there is no need to generate a return statement.
        let block_ret_type = &self.infer[self.body.body_expr()];
        let fn_ret_type = self.instance.sig(self.db).ret().clone();
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

        // Undo what a hardware-safety mode set up, before every return.
        self.finish_safety();
        // One return block for an aggregate result (see `merge`).
        self.unify_aggregate_returns();
        // A `main` returning a `Result` ends the program on an `Err`.
        self.gen_main_result_check();
    }

    /// Emits a `@runtime_checked` function's `requires` clauses at its
    /// entry: each one that evaluates to false executes `llvm.trap`. The
    /// clauses are ordinary, already type-checked expressions of the body's
    /// arena that the executable tree never references; the HIR ghost rules
    /// guarantee they have no effects, so evaluating them here changes
    /// nothing but the failure mode.
    fn gen_precondition_checks(&mut self) {
        let body = self.body.clone();
        let trap = inkwell::intrinsics::Intrinsic::find("llvm.trap")
            .and_then(|intrinsic| intrinsic.get_declaration(self.module, &[]))
            .expect("llvm.trap is always available");
        for &clause in &body.contracts().requires {
            let Some(value) = self.gen_expr(clause) else {
                continue;
            };
            let condition = self.opt_deref_value(clause, value).into_int_value();
            let holds = self
                .context
                .append_basic_block(self.fn_value, "requires_holds");
            let violated = self
                .context
                .append_basic_block(self.fn_value, "requires_violated");
            self.builder
                .build_conditional_branch(condition, holds, violated)
                .expect("failed to build precondition branch");
            self.builder.position_at_end(violated);
            self.builder
                .build_call(trap, &[], "")
                .expect("failed to build precondition trap");
            self.builder
                .build_unreachable()
                .expect("failed to build unreachable after trap");
            self.builder.position_at_end(holds);
        }
    }

    pub fn gen_fn_wrapper(&mut self) {
        // The host has no handlers to give: see `effects`.
        self.bind_unhandled_evidence();
        let fn_sig = self.hir_function.ty(self.db).callable_sig(self.db).unwrap();
        // A method's receiver is the wrapper's first parameter, boxed like
        // the others.
        let param_tys: Vec<Ty> = Instance::of(self.hir_function)
            .self_param_ty(self.db)
            .into_iter()
            .chain(fn_sig.params().iter().cloned())
            .collect();
        let args: Vec<BasicMetadataValueEnum<'_>> = param_tys
            .iter()
            .enumerate()
            .map(|(idx, ty)| {
                let param = self.fn_value.get_nth_param(idx as u32).unwrap();
                if let Some(s) = ty.as_struct() {
                    if s.data(self.db).memory_kind == abi::StructMemoryKind::Value {
                        deref_heap_value(&self.builder, param, self.hir_types.get_struct_type(ty))
                    } else {
                        param
                    }
                } else if ty.is_string() {
                    self.unbox_string(param)
                } else {
                    param
                }
                .into()
            })
            .collect();

        let mut ret_value = self.gen_call(None, self.hir_function, &args);
        let mut fn_sig = fn_sig;

        // A `main` returning a `Result` comes back only with an `Ok` (see
        // `gen_main_result_check`), whose payload the host is handed.
        if let Some(report) = codira_hir::main_result(self.db, self.hir_function) {
            let args = codira_hir::mono::enum_args(fn_sig.ret());
            let slot = report.ok.slots_at(self.db, &args).first().copied();
            ret_value = match (ret_value, slot) {
                (Some(value), Some(slot)) if !report.ok_ty.is_empty() => Some(
                    self.builder
                        .build_extract_value(value.into_struct_value(), slot as u32 + 1, "ok")
                        .expect("the `Ok` slot exists in the layout"),
                ),
                _ => None,
            };
            fn_sig =
                codira_hir::FnSig::from_params_and_return(fn_sig.params().to_vec(), report.ok_ty);
        }

        // What the wrapper returns is decided by the function's declared
        // result, never by the type of its body expression: a body ending
        // in `return x;` is a block of type `never` although the function
        // returns `x`, and reading the body's type here left the wrapper
        // without a terminator -- invalid IR that crashed LLVM's optimizer
        // for every `public` function with a value-struct parameter or
        // result whose body ended that way.
        let fn_ret_type = fn_sig.ret().clone();
        if fn_ret_type.is_never() {
            // `gen_call` has ended the block with `unreachable` already.
        } else if fn_ret_type.is_empty() {
            self.builder
                .build_return(None)
                .expect("failed to build return");
        } else if let Some(value) = ret_value {
            let ret_value = if let Some(hir_struct) = fn_ret_type.as_struct() {
                if hir_struct.data(self.db).memory_kind == codira_hir::StructMemoryKind::Value {
                    self.gen_struct_alloc_on_heap(&fn_ret_type, value.into_struct_value())
                } else {
                    value
                }
            } else if fn_ret_type.is_string() {
                self.box_string(value).into()
            } else {
                value
            };
            self.builder
                .build_return(Some(&ret_value))
                .expect("failed to build return");
        } else {
            // The call produced no value although the function's type says
            // it returns one: the callee cannot return normally.
            self.builder
                .build_unreachable()
                .expect("failed to build unreachable");
        }
    }

    /// Generates IR for the specified expression. Dependending on the type of
    /// expression an IR value is returned.
    ///
    /// A value used where an optional is expected (`return x` from a
    /// `-> i32?` function) is wrapped here, after it is computed, so no
    /// individual construct has to know about the conversion.
    fn gen_expr(&mut self, expr: ExprId) -> Option<inkwell::values::BasicValueEnum<'ink>> {
        let value = self.gen_expr_inner(expr)?;
        // A `str` used where a `String` is expected: see `strings`.
        let value = if self.infer.is_string_conversion(expr) {
            self.gen_string_conversion(expr, value)
        } else {
            value
        };
        match self.infer.optional_wrap(expr).cloned() {
            Some(optional_ty) => Some(self.wrap_optional(&optional_ty, value)),
            None => Some(value),
        }
    }

    /// The value of `expr` itself, before any implicit optional wrapping.
    fn gen_expr_inner(&mut self, expr: ExprId) -> Option<inkwell::values::BasicValueEnum<'ink>> {
        // `Shape.Circle(..)` and `Shape.Point` look like a method call and a
        // field access; inference has recorded what they really are.
        if let Some(variant) = self.infer.variant_of_expr(expr) {
            return self.gen_variant(expr, variant);
        }
        let body = self.body.clone();
        match &body[expr] {
            Expr::Block {
                ref statements,
                tail,
            } => self.gen_block(expr, statements, *tail),
            Expr::Path(ref p) => {
                // A named function used as a value: see `closures`.
                if let Some(item) = self.infer.function_value(expr) {
                    return Some(self.gen_fn_item_value(item));
                }
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
            } => {
                let body = self.body.clone();
                if codira_hir::opened_session(self.db, &body, expr).is_some() {
                    return self.gen_session_open(expr);
                }
                if self.is_endpoint(*receiver) {
                    return self.gen_session_choose(*receiver, args);
                }
                self.gen_method_call(expr, *receiver, args)
            }
            Expr::Call {
                ref callee,
                ref args,
            } => {
                // `print`/`println`: no callee definition, lowered here.
                if let Some(builtin) = self.infer.builtin_call(expr) {
                    if builtin == BuiltinFn::Channel {
                        return self.gen_channel_new(expr, args);
                    }
                    if builtin == BuiltinFn::ArrayFilled {
                        return self.gen_array_filled(expr, args);
                    }
                    if let BuiltinFn::String(_) = builtin {
                        return self.gen_string_from_utf8(expr, args);
                    }
                    return self.gen_builtin_call(builtin, args);
                }

                // A call through a function value: see `closures`.
                if self.infer.is_indirect_call(expr) {
                    let callee_value = self.gen_expr(*callee)?;
                    let fn_ty = self.infer[*callee].clone();
                    return self.gen_indirect_call(expr, callee_value, &fn_ty, args);
                }

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
                        // An `@asm` function likewise: its assembly goes here.
                        if def.is_inline_asm(self.db) {
                            return self.gen_inline_asm_call(expr, def, &args);
                        }

                        self.gen_call(Some(expr), def, &args)
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
            Expr::For {
                pat,
                iterable,
                body: loop_body,
            } => self.gen_for(expr, *pat, *iterable, *loop_body),
            Expr::Continue => self.gen_continue(),
            Expr::Match { expr: scrutinee, arms, .. } => self.gen_match(expr, *scrutinee, arms),
            Expr::Unwrap { expr: inner } => self.gen_unwrap(expr, *inner),
            Expr::Propagate { expr: inner } => self.gen_propagate(expr, *inner),
            Expr::Format { .. } => self.gen_format(expr),
            Expr::Range { .. } => unreachable!(
                "inference rejects a range anywhere but a `for` loop's iterable, which `gen_for` lowers itself"
            ),
            Expr::Break { expr: break_expr } => self.gen_break(expr, *break_expr),
            Expr::Field {
                expr: receiver_expr,
                name,
            } => self.gen_field(expr, *receiver_expr, name),
            Expr::Array(exprs) => self.gen_array(expr, exprs).map(Into::into),
            Expr::Tuple(exprs) => self.gen_tuple(expr, exprs),
            Expr::Index { base, index } => match body[*index] {
                // `xs[lo..hi]`: a copy of those elements (see `arrays`).
                Expr::Range {
                    start,
                    end,
                    inclusive,
                } => self.gen_array_slice(expr, *base, start, end, inclusive),
                _ => self.gen_index(expr, *base, *index),
            },
            Expr::Cast { expr: operand, .. } => self.gen_cast(expr, *operand),
            Expr::Perform { args, .. } => self.gen_perform(expr, args),
            Expr::Handle { body: handled, arms } => self.gen_handle(expr, *handled, arms),
            Expr::Resume { value } => self.gen_resume(*value),
            Expr::Scope { body: scoped } => self.gen_scope(*scoped),
            Expr::Spawn { call } => self.gen_spawn(*call),
            Expr::ChannelSend { channel, value } => self.gen_channel_send(*channel, *value),
            Expr::ChannelReceive { channel } => self.gen_channel_receive(expr, *channel),
            Expr::Select { arms } => self.gen_select(expr, arms),
            Expr::Offer { endpoint, branches } => self.gen_offer(*endpoint, branches),
            Expr::Closure { .. } => self.gen_closure(expr),
            // Every construct that lowers to `Missing` is reported by
            // `codira_hir`'s `validate_codegen_support`, and a `Missing` from a
            // syntax error comes with a parse error; either way the build
            // stops before code generation is entered.
            Expr::Missing => unreachable!(
                "`Expr::Missing` reached code generation; `validate_codegen_support` should have reported it"
            ),
        }
    }

    /// Generates IR for a call to a compiler builtin. See
    /// [`codira_hir::BuiltinFn`].
    ///
    /// Every argument is evaluated before anything is printed, left to
    /// right, the way an ordinary call evaluates its arguments before the
    /// callee runs: a diverging argument then means nothing of the call
    /// happens, rather than half a line appearing.
    fn gen_builtin_call(
        &mut self,
        builtin: BuiltinFn,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        match builtin {
            BuiltinFn::Channel => unreachable!("lowered by `gen_channel_new`"),
            BuiltinFn::ArrayLen
            | BuiltinFn::ArrayUncheckedGet
            | BuiltinFn::StrLen
            | BuiltinFn::Array(_)
            | BuiltinFn::String(_)
            | BuiltinFn::Pipeline(_) => {
                unreachable!("method builtins are lowered by `gen_builtin_method`")
            }
            BuiltinFn::Print | BuiltinFn::Println => {
                let mut values = Vec::with_capacity(args.len());
                for &arg in args {
                    values.push((arg, self.gen_expr(arg)?));
                }
                for (arg, value) in values {
                    self.gen_print_value(arg, value);
                }
                if builtin == BuiltinFn::Println {
                    let newline = self.gen_string_literal(
                        "
",
                    );
                    self.gen_print_str(newline.into_struct_value());
                }
                Some(self.gen_empty())
            }
            // An identity: the qualifier it removes exists only for the
            // information-flow checker.
            BuiltinFn::Declassify => {
                let arg = *args.first()?;
                let value = self.gen_expr(arg)?;
                Some(self.opt_deref_value(arg, value))
            }
            BuiltinFn::CtSelect => self.gen_ct_select(args),
            BuiltinFn::Select => self.gen_value_select(args),
            BuiltinFn::Likely | BuiltinFn::Unlikely => {
                self.gen_branch_hint(args, builtin == BuiltinFn::Likely)
            }
            BuiltinFn::Fma => self.gen_fma(args),
            BuiltinFn::ArrayFilled => unreachable!("lowered by `gen_array_filled`"),
        }
    }

    /// `select(c, a, b)`: both operands are evaluated, then one LLVM `select`
    /// picks the result. Unlike `ct_select` it says nothing about
    /// predictability, so the backend is free to pick a conditional move, a
    /// blend or a branch, whichever its cost model prefers.
    fn gen_value_select(&mut self, args: &[ExprId]) -> Option<BasicValueEnum<'ink>> {
        let [cond, then_value, else_value] = args else {
            return None;
        };
        let condition = self.gen_expr(*cond)?;
        let condition = self.opt_deref_value(*cond, condition).into_int_value();
        let then_ir = self.gen_expr(*then_value)?;
        let else_ir = self.gen_expr(*else_value)?;
        Some(
            self.builder
                .build_select(condition, then_ir, else_ir, "select")
                .expect("failed to build select"),
        )
    }

    /// `likely(c)` / `unlikely(c)`: `c` itself, through `llvm.expect.i1`, which
    /// the optimizer turns into branch weights where `c` is a branch condition.
    fn gen_branch_hint(&mut self, args: &[ExprId], likely: bool) -> Option<BasicValueEnum<'ink>> {
        let [cond] = args else {
            return None;
        };
        let condition = self.gen_expr(*cond)?;
        let condition = self.opt_deref_value(*cond, condition).into_int_value();
        let bool_ty = self.context.bool_type();
        let expect = inkwell::intrinsics::Intrinsic::find("llvm.expect")
            .and_then(|intrinsic| intrinsic.get_declaration(self.module, &[bool_ty.into()]))
            .expect("llvm.expect is always available");
        let expected = bool_ty.const_int(u64::from(likely), false);
        self.builder
            .build_call(
                expect,
                &[condition.into(), expected.into()],
                if likely { "likely" } else { "unlikely" },
            )
            .expect("failed to build llvm.expect")
            .try_as_basic_value()
            .basic()
    }

    /// `fma(a, b, c)`: `llvm.fma`, one rounding, one instruction. It needs a
    /// target with a fused multiply-add: without one LLVM would call libm's
    /// `fma`, which a Codira assembly does not link (the build would die in the
    /// linker with an undefined symbol), and an emulation or a plain
    /// `a * b + c` would silently round twice. So the build is refused here,
    /// naming the remedy.
    fn gen_fma(&mut self, args: &[ExprId]) -> Option<BasicValueEnum<'ink>> {
        let [a, b, c] = args else {
            return None;
        };
        if !target_has_fused_multiply_add(&self.db.target()) {
            self.refuse_build(
                *a,
                "`fma` needs a target with fused multiply-add; build with                  `--target-cpu haswell` (or newer) or `--target-feature +fma`",
            );
        }
        let a = self.gen_expr(*a)?;
        let b = self.gen_expr(*b)?;
        let c = self.gen_expr(*c)?;
        let fma = inkwell::intrinsics::Intrinsic::find("llvm.fma")
            .and_then(|intrinsic| intrinsic.get_declaration(self.module, &[a.get_type()]))
            .expect("llvm.fma is always available");
        self.builder
            .build_call(fma, &[a.into(), b.into(), c.into()], "fma")
            .expect("failed to build llvm.fma")
            .try_as_basic_value()
            .basic()
    }

    /// `array_filled(value, count)`: allocate `count` elements, then store
    /// `value` in each with a counted loop (which LLVM recognizes as a memset
    /// for zero bytes and may vectorize otherwise).
    fn gen_array_filled(&mut self, expr: ExprId, args: &[ExprId]) -> Option<BasicValueEnum<'ink>> {
        let [value, count] = args else {
            return None;
        };
        let value = self.gen_expr(*value)?;
        let count = self.gen_expr(*count)?.into_int_value();
        let array = self.alloc_array(expr, count);
        let elements = array.get_elements(&self.builder);
        let element_ty = array.element_ty();
        let index_ty = count.get_type();

        let before = self.builder.get_insert_block()?;
        let cond_block = self.context.append_basic_block(self.fn_value, "fill.cond");
        let body_block = self.context.append_basic_block(self.fn_value, "fill.body");
        let done_block = self.context.append_basic_block(self.fn_value, "fill.done");
        self.builder
            .build_unconditional_branch(cond_block)
            .expect("failed to build fill branch");

        self.builder.position_at_end(cond_block);
        let index = self
            .builder
            .build_phi(index_ty, "fill.i")
            .expect("failed to build fill index");
        let index_value = index.as_basic_value().into_int_value();
        let in_range = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                index_value,
                count,
                "fill.in_range",
            )
            .expect("failed to build fill comparison");
        self.builder
            .build_conditional_branch(in_range, body_block, done_block)
            .expect("failed to build fill branch");

        self.builder.position_at_end(body_block);
        // Safety: `index < count`, and the allocation holds `count` elements.
        let slot = unsafe {
            self.builder
                .build_gep(element_ty, elements, &[index_value], "fill.slot")
                .expect("failed to build GEP into array elements")
        };
        let store = self
            .builder
            .build_store(slot, value)
            .expect("failed to store a filled element");
        array_tbaa::tag(store, ArrayAccess::Element);
        let next = self
            .builder
            .build_int_add(index_value, index_ty.const_int(1, false), "fill.next")
            .expect("failed to build fill increment");
        self.builder
            .build_unconditional_branch(cond_block)
            .expect("failed to build fill back edge");
        index.add_incoming(&[(&index_ty.const_zero(), before), (&next, body_block)]);

        self.builder.position_at_end(done_block);
        let length_ty = array.length_ty();
        let length = if length_ty == index_ty {
            count
        } else {
            self.builder
                .build_int_cast(count, length_ty, "fill.len")
                .expect("failed to cast a filled array's length")
        };
        let array_length_ptr = array.get_length_ptr(&self.builder);
        let store = self
            .builder
            .build_store(array_length_ptr, length)
            .expect("failed to store a filled array's length");
        array_tbaa::tag(store, ArrayAccess::Length);
        Some(array.into())
    }

    /// `ct_select(c, a, b)`: both operands are evaluated, then one LLVM
    /// `select` picks the result -- no branch is emitted for the condition.
    /// The `select` carries `!unpredictable`, which asks the optimizer and
    /// the backend not to turn it into a branch (the x86 cmov converter and
    /// `CodeGenPrepare`'s select-to-branch both honour it). That is a request
    /// LLVM documents, not a proof about the final machine code; see
    /// `codira_hir::verify::flow` for the threat model.
    fn gen_ct_select(&mut self, args: &[ExprId]) -> Option<BasicValueEnum<'ink>> {
        let [cond, then_value, else_value] = args else {
            return None;
        };
        let condition = self.gen_expr(*cond)?;
        let condition = self.opt_deref_value(*cond, condition).into_int_value();
        let then_ir = self.gen_expr(*then_value)?;
        let then_ir = self.opt_deref_value(*then_value, then_ir);
        let else_ir = self.gen_expr(*else_value)?;
        let else_ir = self.opt_deref_value(*else_value, else_ir);
        // The x86-64 back end has no conditional move for 8-bit registers,
        // and at -O0 it lowers an 8-bit `select` through a pseudo-instruction
        // that becomes a conditional jump on the condition. Selecting on
        // integers of at least 32 bits keeps it a `cmov`, so narrower
        // integers and `bool` are widened (`ct_select` takes only integers
        // and `bool`).
        let (then_wide, else_wide, restore) = self.ct_select_operands(then_ir, else_ir);
        // Without these `freeze`s the -O0 back end folds a `select` of two
        // loads into a load from a `select` of their addresses, which is a
        // memory access at a secret-dependent address. `freeze` does not
        // change a value that is neither `undef` nor poison.
        let then_wide = self.freeze(then_wide, "ct_select.then");
        let else_wide = self.freeze(else_wide, "ct_select.else");
        let selected = self
            .builder
            .build_select(condition, then_wide, else_wide, "ct_select")
            .expect("failed to build ct_select");
        if let Some(instruction) = inkwell::values::BasicValue::as_instruction_value(&selected) {
            let kind = self.context.get_kind_id("unpredictable");
            let empty = self.context.metadata_node(&[]);
            instruction
                .set_metadata(empty, kind)
                .expect("an empty node is valid `!unpredictable` metadata");
        }
        Some(restore(self, selected))
    }

    /// Emits `freeze value` (inkwell 0.10 has no builder method for it).
    fn freeze(&self, value: BasicValueEnum<'ink>, name: &str) -> BasicValueEnum<'ink> {
        use inkwell::values::AsValueRef;
        let name = std::ffi::CString::new(name).expect("a name without NUL bytes");
        // Safety: the builder is positioned in a function body, `value` is a
        // first-class value of this context, and `freeze` returns a value of
        // the same type.
        unsafe {
            BasicValueEnum::new(llvm_sys::core::LLVMBuildFreeze(
                self.builder.as_mut_ptr(),
                value.as_value_ref(),
                name.as_ptr(),
            ))
        }
    }

    /// Operands for `ct_select`'s `select`, and how to turn its result back
    /// into the original type (see [`Self::gen_ct_select`]).
    #[allow(clippy::type_complexity)]
    fn ct_select_operands(
        &self,
        then_ir: BasicValueEnum<'ink>,
        else_ir: BasicValueEnum<'ink>,
    ) -> (
        BasicValueEnum<'ink>,
        BasicValueEnum<'ink>,
        Box<dyn Fn(&Self, BasicValueEnum<'ink>) -> BasicValueEnum<'ink> + 'ink>,
    ) {
        match (then_ir, else_ir) {
            (BasicValueEnum::IntValue(a), BasicValueEnum::IntValue(b))
                if a.get_type().get_bit_width() < 32 =>
            {
                let narrow = a.get_type();
                let wide = self.context.i32_type();
                let widen = |v: inkwell::values::IntValue<'ink>| -> BasicValueEnum<'ink> {
                    self.builder
                        .build_int_z_extend(v, wide, "ct_select.wide")
                        .expect("failed to widen a ct_select operand")
                        .into()
                };
                (
                    widen(a),
                    widen(b),
                    Box::new(move |this: &Self, r: BasicValueEnum<'ink>| {
                        this.builder
                            .build_int_truncate(r.into_int_value(), narrow, "ct_select.narrow")
                            .expect("failed to narrow a ct_select result")
                            .into()
                    }),
                )
            }
            _ => (then_ir, else_ir, Box::new(|_: &Self, r| r)),
        }
    }

    /// Generates IR for a builtin method: `len()` on an array or `str`, or
    /// `unchecked_get(i)` on an array.
    fn gen_builtin_method(
        &mut self,
        builtin: BuiltinFn,
        receiver: ExprId,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let receiver_value = self.gen_expr(receiver)?;
        match builtin {
            BuiltinFn::StrLen => Some(
                self.builder
                    .build_extract_value(receiver_value.into_struct_value(), 1, "str.len")
                    .expect("a str has a length"),
            ),
            BuiltinFn::ArrayLen => {
                let array = self.array_value(receiver, receiver_value);
                let length = self
                    .builder
                    .build_load(
                        array.length_ty(),
                        array.get_length_ptr(&self.builder),
                        "len",
                    )
                    .expect("failed to load an array's length");
                array_tbaa::tag_value(length, ArrayAccess::Length);
                Some(length)
            }
            BuiltinFn::ArrayUncheckedGet => {
                let array = self.array_value(receiver, receiver_value);
                let index = self.gen_expr(args[0])?.into_int_value();
                let index = self.index_to_length_type(args[0], index, array.length_ty());
                let elements = array.get_elements(&self.builder);
                // Safety: none -- that is this builtin's contract with its
                // caller, who has established `index < len` some other way.
                let element_ptr = unsafe {
                    self.builder
                        .build_gep(array.element_ty(), elements, &[index], "element_ptr")
                        .expect("failed to build GEP for an unchecked element")
                };
                Some(
                    self.builder
                        .build_load(array.element_ty(), element_ptr, "element")
                        .expect("failed to load an unchecked element"),
                )
            }
            BuiltinFn::Print
            | BuiltinFn::Println
            | BuiltinFn::Declassify
            | BuiltinFn::CtSelect
            | BuiltinFn::Select
            | BuiltinFn::Likely
            | BuiltinFn::Unlikely
            | BuiltinFn::Fma
            | BuiltinFn::ArrayFilled
            | BuiltinFn::Channel
            | BuiltinFn::String(_) => {
                unreachable!("free-function builtins are lowered by `gen_builtin_call`")
            }
            BuiltinFn::Array(_) => unreachable!("array methods are lowered by `gen_array_method`"),
            BuiltinFn::Pipeline(_) => unreachable!("pipelines are lowered by `pipelines`"),
        }
    }

    /// Views `value`, the result of the array-typed expression `expr`, as an
    /// array.
    fn array_value(&self, expr: ExprId, value: BasicValueEnum<'ink>) -> RuntimeArrayValue<'ink> {
        let element_ty = self.infer[expr]
            .as_array()
            .expect("expected an array-typed expression");
        let array_ty = self.hir_types.get_array_type(element_ty);
        // Safety: the expression's type is an array, so its value is one.
        unsafe { RuntimeArrayValue::from_ptr_unchecked(value.into_pointer_value(), array_ty) }
    }

    /// Converts an index of the integer type of `index_expr` to the array
    /// length's type, extending by the index's signedness. GEP itself
    /// sign-extends a narrow index, which turned a `u8` index of 200 into -56.
    fn index_to_length_type(
        &self,
        index_expr: ExprId,
        index: IntValue<'ink>,
        length_type: inkwell::types::IntType<'ink>,
    ) -> IntValue<'ink> {
        let signed = self.is_signed_int(index_expr);
        let index_bits = index.get_type().get_bit_width();
        let length_bits = length_type.get_bit_width();
        if index_bits == length_bits {
            index
        } else if index_bits > length_bits {
            self.builder
                .build_int_truncate(index, length_type, "index")
                .expect("failed to truncate an index")
        } else if signed {
            self.builder
                .build_int_s_extend(index, length_type, "index")
                .expect("failed to extend an index")
        } else {
            self.builder
                .build_int_z_extend(index, length_type, "index")
                .expect("failed to extend an index")
        }
    }

    /// Whether `expr`'s type is a signed integer.
    fn is_signed_int(&self, expr: ExprId) -> bool {
        matches!(
            self.infer[expr].interned(),
            TyKind::Int(int_ty) if int_ty.signedness == codira_hir::Signedness::Signed
        )
    }

    /// The source location of `expr` as a `str` constant naming the file,
    /// and 1-based line and column, for a runtime panic report.
    ///
    /// The file is named relative to the package's source directory, the
    /// same way diagnostics name it. A compiler-generated expression has no
    /// location; it reports `<unknown>:0:0` rather than a wrong one.
    fn source_location(&mut self, expr: ExprId) -> (StructValue<'ink>, u32, u32) {
        let (file, line, column) = match self.hir_function.expr_source_range(self.db, expr) {
            Some((file_id, range)) => {
                let line_col = self.db.line_index(file_id).line_col(range.start());
                (
                    self.db.file_relative_path(file_id).to_string(),
                    line_col.line + 1,
                    line_col.col_utf16 + 1,
                )
            }
            None => ("<unknown>".to_string(), 0, 0),
        };
        let file = self.gen_string_literal(&file).into_struct_value();
        (file, line, column)
    }

    /// Emits the runtime call that prints one already-evaluated value,
    /// chosen by its type. Inference admits only the types handled here.
    fn gen_print_value(&mut self, arg: ExprId, value: BasicValueEnum<'ink>) {
        let ty = self.infer[arg].clone();
        match ty.interned() {
            TyKind::Str => self.gen_print_str(value.into_struct_value()),
            TyKind::String => self.gen_print_string(arg, value),
            TyKind::Int(int_ty) => {
                let int_value = value.into_int_value();
                let signed = int_ty.signedness == codira_hir::Signedness::Signed;
                let i64_type = self.context.i64_type();
                if int_value.get_type().get_bit_width() > 64 {
                    // 128-bit: passed as two halves, see `intrinsics::print_i128`.
                    let low = self
                        .builder
                        .build_int_truncate(int_value, i64_type, "low")
                        .expect("failed to truncate to the low half");
                    let shift = int_value.get_type().const_int(64, false);
                    let high = self
                        .builder
                        .build_right_shift(int_value, shift, false, "high_bits")
                        .and_then(|high| self.builder.build_int_truncate(high, i64_type, "high"))
                        .expect("failed to extract the high half");
                    let args = [low.into(), high.into()];
                    if signed {
                        self.gen_runtime_call(&intrinsics::print_i128, &args);
                    } else {
                        self.gen_runtime_call(&intrinsics::print_u128, &args);
                    }
                } else if signed {
                    let widened = self
                        .builder
                        .build_int_s_extend_or_bit_cast(int_value, i64_type, "widen")
                        .expect("failed to widen integer for printing");
                    self.gen_runtime_call(&intrinsics::print_i64, &[widened.into()]);
                } else {
                    let widened = self
                        .builder
                        .build_int_z_extend_or_bit_cast(int_value, i64_type, "widen")
                        .expect("failed to widen integer for printing");
                    self.gen_runtime_call(&intrinsics::print_u64, &[widened.into()]);
                }
            }
            TyKind::Float(_) => {
                let float_value = value.into_float_value();
                if float_value.get_type() == self.context.f32_type() {
                    self.gen_runtime_call(&intrinsics::print_f32, &[float_value.into()]);
                } else {
                    self.gen_runtime_call(&intrinsics::print_f64, &[float_value.into()]);
                }
            }
            TyKind::Bool => {
                let byte = self
                    .builder
                    .build_int_z_extend(value.into_int_value(), self.context.i8_type(), "byte")
                    .expect("failed to widen bool for printing");
                self.gen_runtime_call(&intrinsics::print_bool, &[byte.into()]);
            }
            _ => unreachable!(
                "inference admits only printable types as `print` arguments, found `{}`",
                ty.display(self.db)
            ),
        }
    }

    /// Prints a `str` value: its `{ ptr, len }` halves go to
    /// `codira.print_str`.
    fn gen_print_str(&mut self, value: StructValue<'ink>) {
        let data = self
            .builder
            .build_extract_value(value, 0, "str.data")
            .expect("a str has a data pointer");
        let len = self
            .builder
            .build_extract_value(value, 1, "str.len")
            .expect("a str has a length");
        self.gen_runtime_call(&intrinsics::print_str, &[data.into(), len.into()]);
    }

    /// Calls a function the runtime provides, through the dispatch table
    /// slot `ir::intrinsics` reserved for it.
    fn gen_runtime_call(
        &mut self,
        function: &impl intrinsics::Intrinsic,
        args: &[BasicMetadataValueEnum<'ink>],
    ) {
        let (fn_ty, fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            function,
        );
        self.builder
            .build_indirect_call(fn_ty, fn_ptr, args, "")
            .expect("failed to build call to a runtime function");
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

            // `nil` is the all-zero optional: absent, with a zero payload.
            Literal::Nil => {
                let ty = self.infer[expr].clone();
                self.hir_types
                    .get_basic_type(&ty)
                    .expect("`nil` has an optional type")
                    .const_zero()
            }
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
        hir_ty: &Ty,
        args: Vec<BasicValueEnum<'ink>>,
    ) -> BasicValueEnum<'ink> {
        let hir_struct = hir_ty.as_struct().expect("a struct type");
        // Construct the struct literal
        let struct_ty = self.hir_types.get_struct_type(hir_ty);
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
                self.gen_struct_alloc_on_heap(hir_ty, struct_lit)
            }
        }
    }

    fn gen_struct_alloc_on_heap(
        &mut self,
        hir_ty: &Ty,
        struct_lit: StructValue<'_>,
    ) -> BasicValueEnum<'ink> {
        let struct_ir_ty = self.hir_types.get_struct_type(hir_ty);
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
            &self.hir_types.type_id(hir_ty),
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
        let fields: Vec<BasicValueEnum<'ink>> = fields
            .iter()
            .map(|field| self.gen_expr(field.expr).expect("expected a field value"))
            .collect();

        self.gen_struct_alloc(&struct_ty, fields)
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
        if let Some(builtin) = self.infer.builtin_call(tgt_expr) {
            if let BuiltinFn::Array(method) = builtin {
                return self.gen_array_method(tgt_expr, method, receiver, args);
            }
            if let BuiltinFn::String(op) = builtin {
                return self.gen_string_method(op, tgt_expr, receiver, args);
            }
            if let BuiltinFn::Pipeline(op) = builtin {
                // Only a terminal is ever generated as an expression: the
                // stages before it are part of its loop.
                return self.gen_pipeline_terminal(tgt_expr, op, receiver, args);
            }
            return self.gen_builtin_method(builtin, receiver, args);
        }

        // `obj.callback(x)`: a call through the function value in a field.
        if self.infer.is_field_call(tgt_expr) {
            let body = self.body.clone();
            let Expr::MethodCall { method_name, .. } = &body[tgt_expr] else {
                unreachable!("a field call is a method-call expression");
            };
            let receiver_ty = self.infer[receiver].clone();
            let fn_ty = receiver_ty
                .as_struct()
                .and_then(|s| s.field(self.db, method_name))
                .and_then(|field| {
                    codira_hir::mono::struct_field_types(self.db, &receiver_ty)
                        .get(field.index(self.db) as usize)
                        .cloned()
                })
                .expect("inference resolved the called field");
            let callee_value = self.gen_field(tgt_expr, receiver, method_name)?;
            return self.gen_indirect_call(tgt_expr, callee_value, &fn_ty, args);
        }

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

        self.gen_call(Some(tgt_expr), function, &call_args)
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
        let args: Vec<BasicValueEnum<'ink>> = args
            .iter()
            .map(|expr| self.gen_expr(*expr).expect("expected a field value"))
            .collect();

        self.gen_struct_alloc(&struct_ty, args)
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
        self.gen_struct_alloc(&struct_ty, Vec::new())
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
                return deref_heap_value(&self.builder, value, self.hir_types.get_struct_type(ty));
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
                let struct_ty = self.hir_types.get_struct_type(ty);
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
        tgt_expr: ExprId,
        lhs: ExprId,
        rhs: ExprId,
        op: BinaryOp,
    ) -> Option<BasicValueEnum<'ink>> {
        if let BinaryOp::LogicOp(op) = op {
            return self.gen_short_circuit(lhs, rhs, op);
        }
        let lhs_type = self.infer[lhs].clone();
        match (lhs_type.interned(), op) {
            // An array is a GC reference: assigning one (`a = b`,
            // `rows[i] = row`) stores the handle, as for a class instance.
            // Both used to reach the `unimplemented!` below.
            (
                TyKind::Optional(_)
                | TyKind::Str
                | TyKind::Tuple(..)
                | TyKind::Enum(..)
                | TyKind::Function(_)
                | TyKind::Array(_)
                | TyKind::String,
                BinaryOp::Assignment { op: None },
            ) => return self.gen_plain_assignment(lhs, rhs),
            (TyKind::String | TyKind::Str, _) => {
                return self.gen_string_binary_op(tgt_expr, lhs, rhs, op);
            }
            (TyKind::Enum(..), BinaryOp::CmpOp(CmpOp::Eq { negated })) => {
                return self.gen_enum_eq(lhs, rhs, &lhs_type.clone(), negated)
            }
            (TyKind::Optional(payload), BinaryOp::CmpOp(CmpOp::Eq { negated })) => {
                return self.gen_optional_eq(lhs, rhs, &payload.clone(), negated)
            }
            _ => {}
        }
        match lhs_type.interned() {
            TyKind::Bool => self.gen_binary_op_bool(lhs, rhs, op),
            TyKind::Float(_) => self.gen_binary_op_float(lhs, rhs, op),
            TyKind::Int(ty) => self.gen_binary_op_int(tgt_expr, lhs, rhs, op, ty.signedness),
            TyKind::Struct(s, _) => {
                if s.data(self.db).memory_kind == codira_hir::StructMemoryKind::Value {
                    self.gen_binary_op_value_struct(lhs, rhs, op)
                } else {
                    self.gen_binary_op_heap_struct(lhs, rhs, op)
                }
            }
            // A function value is a GC reference like a class instance;
            // inference admits only plain assignment for it.
            TyKind::Function(_) => self.gen_binary_op_heap_struct(lhs, rhs, op),
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

    /// Generates IR for `lhs && rhs` and `lhs || rhs`, evaluating `rhs` only
    /// when `lhs` does not already decide the result.
    ///
    /// Both operands used to be evaluated unconditionally, which made
    /// `i < a.len() && a[i] > 0` index out of range exactly when the guard
    /// said not to, and ran any call in `rhs` for its side effects even when
    /// the result was already known.
    fn gen_short_circuit(
        &mut self,
        lhs_expr: ExprId,
        rhs_expr: ExprId,
        op: LogicOp,
    ) -> Option<BasicValueEnum<'ink>> {
        let lhs = self
            .gen_expr(lhs_expr)
            .map(|value| self.opt_deref_value(lhs_expr, value))?
            .into_int_value();
        let lhs_block = self.builder.get_insert_block().unwrap();
        let rhs_block = self.context.append_basic_block(self.fn_value, "logic.rhs");
        let merge_block = self
            .context
            .append_basic_block(self.fn_value, "logic.merge");

        // `&&` only needs `rhs` when `lhs` is true, `||` only when it is
        // false; otherwise `lhs` is the answer.
        let (if_true, if_false) = match op {
            LogicOp::And => (rhs_block, merge_block),
            LogicOp::Or => (merge_block, rhs_block),
        };
        self.builder
            .build_conditional_branch(lhs, if_true, if_false)
            .expect("failed to build short-circuit branch");

        self.builder.position_at_end(rhs_block);
        let rhs = self
            .gen_expr(rhs_expr)
            .map(|value| self.opt_deref_value(rhs_expr, value));
        let rhs_end_block = self.builder.get_insert_block().unwrap();
        if rhs.is_some() {
            self.builder
                .build_unconditional_branch(merge_block)
                .expect("failed to build short-circuit merge");
        }

        self.builder.position_at_end(merge_block);
        let phi = self
            .builder
            .build_phi(self.context.bool_type(), "logic")
            .expect("failed to build short-circuit phi");
        phi.add_incoming(&[(&lhs, lhs_block)]);
        if let Some(rhs) = rhs {
            phi.add_incoming(&[(&rhs, rhs_end_block)]);
        }
        Some(phi.as_basic_value())
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
                let store = self
                    .builder
                    .build_store(place, rhs)
                    .expect("failed to build store");
                self.tag_if_indexed(lhs_expr, store);
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
                // IEEE 754: `==` and every ordering are *ordered* (false
                // when either side is NaN); `!=` is the negation of `==`, so
                // it is *unordered* (true when either side is NaN). `ONE`
                // here would make `NaN != x` false -- the answer C, SQL and
                // the Eidos folder (`codira_mir::fold`) all disagree with.
                let (name, predicate) = match op {
                    CmpOp::Eq { negated: false } => ("eq", FloatPredicate::OEQ),
                    CmpOp::Eq { negated: true } => ("neq", FloatPredicate::UNE),
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
                let store = self
                    .builder
                    .build_store(place, rhs)
                    .expect("failed to build store");
                self.tag_if_indexed(lhs_expr, store);
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
        expr: ExprId,
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
        if let BinaryOp::ArithOp(arith) | BinaryOp::Assignment { op: Some(arith) } = op {
            if codira_hir::realtime::division_needs_check(self.db, &self.body, &self.infer, expr) {
                self.gen_division_check(expr, arith, lhs, rhs, signedness);
            }
        }
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
                let store = self
                    .builder
                    .build_store(place, rhs)
                    .expect("failed to build store");
                self.tag_if_indexed(lhs_expr, store);
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
                let store = self
                    .builder
                    .build_store(place, rhs)
                    .expect("failed to build store");
                self.tag_if_indexed(lhs_expr, store);
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
                let store = self
                    .builder
                    .build_store(place, rhs)
                    .expect("failed to build store");
                self.tag_if_indexed(lhs_expr, store);
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

    /// Checks the divisor of the integer division or remainder `expr`
    /// before it is computed: a zero divisor, and for a signed type
    /// `MIN / -1` or `MIN % -1` (whose quotient does not fit), call
    /// `codira.panic` with the location of `expr`. LLVM's `sdiv`, `udiv`,
    /// `srem` and `urem` are undefined behaviour on those operands.
    ///
    /// The effect analysis decides which divisions need the check
    /// (`codira_hir::realtime::division_needs_check`): a positive literal
    /// divisor, or one the verifier proved safe, gets none.
    fn gen_division_check(
        &mut self,
        expr: ExprId,
        op: ArithOp,
        lhs: IntValue<'ink>,
        rhs: IntValue<'ink>,
        signedness: codira_hir::Signedness,
    ) {
        let int_type = rhs.get_type();
        let is_zero = self
            .builder
            .build_int_compare(IntPredicate::EQ, rhs, int_type.const_zero(), "div.zero")
            .expect("failed to build a divisor check");
        let zero_block = self
            .context
            .append_basic_block(self.fn_value, "div.by_zero");
        let nonzero_block = self
            .context
            .append_basic_block(self.fn_value, "div.nonzero");
        self.builder
            .build_conditional_branch(is_zero, zero_block, nonzero_block)
            .expect("failed to branch on a divisor check");
        self.builder.position_at_end(zero_block);
        self.gen_panic(
            if op == ArithOp::Divide {
                "attempt to divide by zero"
            } else {
                "attempt to calculate the remainder with a divisor of zero"
            },
            expr,
        );
        self.builder.position_at_end(nonzero_block);
        if !signedness.is_signed() {
            return;
        }
        let bits = int_type.get_bit_width();
        let min = if bits > 64 {
            int_type.const_int_arbitrary_precision(&[0, 1 << (bits - 65)])
        } else {
            int_type.const_int(1 << (bits - 1), false)
        };
        let lhs_min = self
            .builder
            .build_int_compare(IntPredicate::EQ, lhs, min, "div.min")
            .expect("failed to build an overflow check");
        let rhs_minus_one = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                rhs,
                int_type.const_all_ones(),
                "div.minus_one",
            )
            .expect("failed to build an overflow check");
        let overflow = self
            .builder
            .build_and(lhs_min, rhs_minus_one, "div.overflow")
            .expect("failed to build an overflow check");
        let overflow_block = self
            .context
            .append_basic_block(self.fn_value, "div.overflow");
        let ok_block = self.context.append_basic_block(self.fn_value, "div.ok");
        self.builder
            .build_conditional_branch(overflow, overflow_block, ok_block)
            .expect("failed to branch on an overflow check");
        self.builder.position_at_end(overflow_block);
        self.gen_panic(
            if op == ArithOp::Divide {
                "attempt to divide with overflow"
            } else {
                "attempt to calculate the remainder with overflow"
            },
            expr,
        );
        self.builder.position_at_end(ok_block);
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
            ArithOp::LeftShift => {
                let rhs = self.mask_shift_amount(rhs);
                self.builder.build_left_shift(lhs, rhs, "left_shift")
            }
            ArithOp::RightShift => {
                let rhs = self.mask_shift_amount(rhs);
                self.builder
                    .build_right_shift(lhs, rhs, signedness.is_signed(), "right_shift")
            }
            ArithOp::BitAnd => self.builder.build_and(lhs, rhs, "bit_and"),
            ArithOp::BitOr => self.builder.build_or(lhs, rhs, "bit_or"),
            ArithOp::BitXor => self.builder.build_xor(lhs, rhs, "bit_xor"),
        })
        .expect("failed to build int arithmetic operation")
    }

    /// Shifts take the amount modulo the operand's width (its low
    /// `log2(width)` bits, read as unsigned), as Rust's wrapping shifts,
    /// Java and C# do: `x << 65` on an `i64` is `x << 1`, and a negative
    /// amount `-k` shifts by `width - k`. LLVM's `shl`, `lshr` and `ashr`
    /// are poison for an amount of the width or more, so the mask is what
    /// defines them. It costs nothing on x86-64 and `AArch64` at 32 and 64
    /// bits (their shift instructions mask the same way, and LLVM drops the
    /// `and`); 8- and 16-bit shifts keep one `and`.
    fn mask_shift_amount(&self, amount: IntValue<'ink>) -> IntValue<'ink> {
        let int_type = amount.get_type();
        let mask = int_type.const_int(u64::from(int_type.get_bit_width() - 1), false);
        self.builder
            .build_and(amount, mask, "shift.amount")
            .expect("failed to mask a shift amount")
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
    /// Tags a store through the place `place_expr` as an array-element
    /// access when that place is a bounds-checked `xs[i]` (see
    /// [`array_tbaa`]).
    fn tag_if_indexed(&self, place_expr: ExprId, store: inkwell::values::InstructionValue<'ink>) {
        if matches!(self.body[place_expr], Expr::Index { .. }) {
            array_tbaa::tag(store, ArrayAccess::Element);
        }
    }

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
    /// Emits the `@asm` variant of `function` for this target in place of a
    /// call to it. See `codira_hir::inline_asm` for the attribute; the HIR
    /// validator has already checked that a variant exists for the target,
    /// that every operand is an integer or a float, and that the constraint
    /// string names one output per returned value and one input per
    /// parameter.
    fn gen_inline_asm_call(
        &mut self,
        expr: ExprId,
        function: codira_hir::Function,
        args: &[BasicMetadataValueEnum<'ink>],
    ) -> Option<BasicValueEnum<'ink>> {
        let variant = function
            .inline_asm(self.db)
            .expect("the validator rejects an `@asm` function with no variant for the target");
        let sig = function.ty(self.db).callable_sig(self.db).unwrap();

        let param_types: Vec<BasicMetadataTypeEnum<'ink>> = sig
            .params()
            .iter()
            .map(|ty| {
                self.hir_types
                    .get_basic_type(ty)
                    .expect("an `@asm` operand is an integer or a float")
                    .into()
            })
            .collect();
        let ret = sig.ret();
        let fn_type = if ret.is_empty() || ret.is_never() {
            self.context.void_type().fn_type(&param_types, false)
        } else {
            self.hir_types
                .get_basic_type(ret)
                .expect("an `@asm` result is an integer or a float")
                .fn_type(&param_types, false)
        };

        let mut constraints = variant.constraints.clone();
        // GCC-style inline assembly on x86 may change the direction flag,
        // the x87 status word and the condition flags without saying so, and
        // Clang tells LLVM as much on every `asm` statement. A template that
        // compares or adds would otherwise leave LLVM believing flags it set
        // before the assembly still hold after it.
        if matches!(&*self.db.target().arch, "x86_64" | "x86") {
            for clobber in ["~{dirflag}", "~{fpsr}", "~{flags}"] {
                if !constraints.split(',').any(|c| c.trim() == clobber) {
                    if !constraints.trim().is_empty() {
                        constraints.push(',');
                    }
                    constraints.push_str(clobber);
                }
            }
        }

        let dialect = match variant.dialect {
            codira_hir::inline_asm::AsmDialect::Att => InlineAsmDialect::ATT,
            codira_hir::inline_asm::AsmDialect::Intel => InlineAsmDialect::Intel,
        };
        let asm = self.context.create_inline_asm(
            fn_type,
            variant.template,
            constraints,
            !variant.pure,
            false,
            Some(dialect),
            false,
        );
        let call = self
            .builder
            .build_indirect_call(fn_type, asm, args, &function.name(self.db).to_string())
            .expect("failed to build inline assembly call");

        if ret.is_never() {
            self.builder
                .build_unreachable()
                .expect("failed to build unreachable after assembly returning `never`");
            return None;
        }
        call.try_as_basic_value()
            .basic()
            .or_else(|| match self.infer[expr].interned() {
                TyKind::Never => None,
                _ => Some(self.context.const_struct(&[], false).into()),
            })
    }

    fn gen_intrinsic_call(
        &mut self,
        expr: ExprId,
        function: codira_hir::Function,
        args: &[BasicMetadataValueEnum<'ink>],
    ) -> Option<BasicValueEnum<'ink>> {
        let name = function.name(self.db).to_string();
        if let safety::Lowered::Emitted(value) = self.gen_safety_intrinsic(expr, &name, args) {
            return value;
        }
        match intrinsic_ops::gen_intrinsic(
            &name,
            args,
            self.context,
            self.module,
            &self.builder,
            self.hir_types.target_data(),
        ) {
            Ok(Some(value)) => Some(value),
            // A `store_*` yields nothing. Unit is substituted for the same
            // reason a void call does above: `None` here would be read as
            // `never`, and a store does return.
            Ok(None) => match self.infer[expr].interned() {
                TyKind::Never => None,
                _ => Some(self.context.const_struct(&[], false).into()),
            },
            Err(intrinsic_ops::IntrinsicError::Unknown) => panic!(
                "`{name}` is not a known compiler intrinsic. Functions declared in an `extern \"codira-intrinsic\"` block must name an operation the compiler can emit; see `codira_codegen::ir::intrinsic_ops` for the list."
            ),
            Err(intrinsic_ops::IntrinsicError::Arity { expected, found }) => panic!(
                "intrinsic `{name}` takes {expected} argument(s) but was declared with {found}"
            ),
        }
    }

    ///
    /// `call` is the call expression, which says -- through its recorded
    /// instantiation -- which instance of a generic `function` is called.
    /// It is `None` only for the host wrapper's call of its own function,
    /// which is never generic.
    fn gen_call(
        &mut self,
        call: Option<ExprId>,
        function: codira_hir::Function,
        args: &[BasicMetadataValueEnum<'ink>],
    ) -> Option<BasicValueEnum<'ink>> {
        let instance = call
            .and_then(|call| Instance::callee(self.db, &self.infer, call, function))
            .unwrap_or_else(|| Instance::of(function));
        let sig = instance.sig(self.db);
        // A function this module has code for -- its own, an instance of a
        // generic one, or a copy of another module's private helper -- is
        // called directly, never through the dispatch table.
        let local = self.function_map.get(&instance).copied();

        // A `uses` callee takes the innermost evidence for each of its
        // effects after its ordinary arguments, and may then abort to a
        // handler: checked right after the call. See `effects`.
        let evidence = self.evidence_args(function);
        let with_evidence;
        let args = if evidence.is_empty() {
            args
        } else {
            with_evidence = args
                .iter()
                .copied()
                .chain(evidence.iter().copied())
                .collect::<Vec<_>>();
            &with_evidence[..]
        };

        let through_table = local.is_none() && self.should_use_dispatch_table(function);
        let foreign = crate::gc_roots::is_foreign_transition(self.db, function);
        if foreign {
            self.gen_foreign_transition(true);
        }
        let call_site = if through_table {
            // A method's receiver comes first, and is boxed like any other
            // argument.
            let param_tys: Vec<Ty> = instance
                .self_param_ty(self.db)
                .into_iter()
                .chain(sig.params().iter().cloned())
                .collect();
            let boxed_args: Vec<BasicMetadataValueEnum<'ink>> = args
                .iter()
                .zip(param_tys.iter())
                .map(|(arg, param_ty)| {
                    if let Some(hir_struct) = param_ty.as_struct() {
                        if hir_struct.data(self.db).memory_kind
                            == codira_hir::StructMemoryKind::Value
                        {
                            return self
                                .gen_struct_alloc_on_heap(param_ty, arg.into_struct_value())
                                .into();
                        }
                    }
                    if param_ty.is_string() {
                        return self
                            .box_string((*arg).try_into().expect("a String value"))
                            .into();
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
            let llvm_function = match local {
                Some(function) => function,
                // An `extern` declaration of a function the package's native
                // code defines: declared here, on first use, under its own
                // name, so the linker -- or, for C and C++, the bitcode linked
                // into this module (`ir::file::link_native_bitcode`) --
                // resolves it. It never reaches the dispatch table.
                None if self.module_group.links_natively(self.db, function) => {
                    let name = function.name(self.db).to_string();
                    self.module.get_function(&name).unwrap_or_else(|| {
                        crate::ir::function::gen_prototype(
                            self.db,
                            self.hir_types,
                            &instance,
                            self.module,
                            false,
                        )
                    })
                }
                None => panic!(
                    "missing function value for codira_hir function: '{}'",
                    function.name(self.db),
                ),
            };
            self.builder
                .build_call(llvm_function, args, &function.name(self.db).to_string())
                .expect("failed to build call")
        };

        if foreign {
            self.gen_foreign_transition(false);
        }
        let ret_value = call_site.try_as_basic_value().basic();
        if !evidence.is_empty() {
            self.gen_abort_check();
        }

        // A call to a function returning `never` does not come back (an
        // abort out of a `uses` callee has just been branched away by the
        // check above): nothing follows it.
        if sig.ret().is_never() {
            self.builder
                .build_unreachable()
                .expect("failed to build unreachable after a call returning `never`");
            return None;
        }

        if through_table {
            if let Some(hir_struct) = sig.ret().as_struct() {
                if hir_struct.data(self.db).memory_kind == codira_hir::StructMemoryKind::Value {
                    let struct_ty = self.hir_types.get_struct_type(sig.ret());
                    return ret_value
                        .map(|value| deref_heap_value(&self.builder, value, struct_ty));
                }
            }
            if sig.ret().is_string() {
                return ret_value.map(|value| self.unbox_string(value));
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
            match else_ir_and_block {
                Some((Some(else_block_ir), else_block)) => Some(self.gen_merge(
                    &[(then_block_ir, then_block), (else_block_ir, else_block)],
                    "iftmp",
                )),
                // The `else` branch never reaches the merge block, so only
                // the `then` branch's value arrives there.
                Some((None, _)) => Some(then_block_ir),
                // Without `else` the `if` is a statement and its value is
                // unit. The `then` branch's value must not stand in for it:
                // the merge block is also reached straight from the
                // condition, so that value (a `phi {}` of an inner `if`,
                // say) does not dominate the merge block, and an enclosing
                // `if` that took it as its own branch value built an
                // invalid phi ("Instruction does not dominate all uses!").
                None => Some(self.gen_empty()),
            }
        } else if let Some((else_block_ir, _else_block)) = else_ir_and_block {
            // If both the then and the else block never return, the entire if statement
            // will never return. Therefor we have to remove the merge block
            // because it has no predecessor.
            if else_block_ir.is_none() {
                merge_block
                    .remove_from_function()
                    .expect("merge block must have a parent");
                // Leave the builder in a block of the function (the last
                // one the `else` branch built, which it terminated), as a
                // `loop` or a `match` that never finishes does. Left in the
                // removed merge block, an enclosing `if` would try to move
                // its own merge block after a block that is no longer in
                // the function (`else if a { continue } else { return }`).
                self.builder.position_at_end(current_block);
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
        let ret_value = match ret_expr {
            Some(expr) => {
                // `return f()` where `f` never returns: nothing is left to
                // return, and the block is already terminated.
                Some(self.gen_expr(expr)?)
            }
            None => None,
        };

        // Every task of every open scope finishes before the function does.
        self.gen_end_scopes(0);

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
                // The scopes opened inside the loop end before leaving it.
                self.gen_end_loop_scopes();
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
            self.gen_end_loop_scopes();
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
        continue_block: BasicBlock<'ink>,
    ) -> (
        BasicBlock<'ink>,
        BreakSources<'ink>,
        Option<BasicValueEnum<'ink>>,
    ) {
        // Build a new loop info struct
        let loop_info = LoopInfo {
            exit_block,
            break_values: Vec::new(),
            continue_block,
        };

        // Replace previous loop info
        let prev_loop = self.active_loop.replace(loop_info);

        // Start generating code inside the loop
        self.enter_loop_scopes();
        let value = self.gen_expr(block);
        self.leave_loop_scopes();

        let LoopInfo {
            exit_block,
            break_values,
            ..
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
        let (exit_block, _, value) = self.gen_loop_block_expr(body_expr, exit_block, cond_block);
        if value.is_some() {
            // Every iteration costs one unit of fuel (when instrumented)
            self.gen_fuel_check();
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
        let (exit_block, break_values, value) =
            self.gen_loop_block_expr(body_expr, exit_block, loop_block);
        if value.is_some() {
            // Every iteration costs one unit of fuel (when instrumented)
            self.gen_fuel_check();
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

    /// Wraps `value`, a `T`, into the present `T?` of type `optional_ty`.
    fn wrap_optional(
        &mut self,
        optional_ty: &Ty,
        value: BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let optional_type = self
            .hir_types
            .get_basic_type(optional_ty)
            .expect("an optional is a basic type")
            .into_struct_type();
        let with_flag = self
            .builder
            .build_insert_value(
                optional_type.const_zero(),
                self.context.bool_type().const_all_ones(),
                0,
                "some.flag",
            )
            .expect("failed to set an optional's present flag");
        self.builder
            .build_insert_value(with_flag, value, 1, "some")
            .expect("failed to set an optional's payload")
            .into_struct_value()
            .into()
    }

    /// `x!`: the payload of a present optional, or a panic naming the
    /// source location when it is `nil`.
    fn gen_unwrap(&mut self, unwrap_expr: ExprId, inner: ExprId) -> Option<BasicValueEnum<'ink>> {
        let optional = self.gen_expr(inner)?.into_struct_value();
        let present = self
            .builder
            .build_extract_value(optional, 0, "is_some")
            .expect("an optional has a present flag")
            .into_int_value();
        let ok_block = self
            .context
            .append_basic_block(self.fn_value, "unwrap.some");
        let nil_block = self.context.append_basic_block(self.fn_value, "unwrap.nil");
        self.builder
            .build_conditional_branch(present, ok_block, nil_block)
            .expect("failed to branch on an optional");

        self.builder.position_at_end(nil_block);
        self.gen_panic("unwrapped `nil` with postfix `!`", unwrap_expr);

        self.builder.position_at_end(ok_block);
        Some(
            self.builder
                .build_extract_value(optional, 1, "value")
                .expect("an optional has a payload"),
        )
    }

    /// Emits a call to `codira.panic` with `message` and the location of
    /// `expr`, followed by `unreachable`: the runtime never returns.
    fn gen_panic(&mut self, message: &str, expr: ExprId) {
        let message = self.gen_string_literal(message).into_struct_value();
        let (file, line, column) = self.source_location(expr);
        let mut args: Vec<BasicMetadataValueEnum<'ink>> = Vec::with_capacity(6);
        for value in [message, file] {
            args.push(
                self.builder
                    .build_extract_value(value, 0, "str.data")
                    .expect("a str has a data pointer")
                    .into(),
            );
            args.push(
                self.builder
                    .build_extract_value(value, 1, "str.len")
                    .expect("a str has a length")
                    .into(),
            );
        }
        let u32_type = self.context.i32_type();
        args.push(u32_type.const_int(u64::from(line), false).into());
        args.push(u32_type.const_int(u64::from(column), false).into());
        self.gen_runtime_call(&intrinsics::panic, &args);
        self.builder
            .build_unreachable()
            .expect("failed to build unreachable after a panic");
    }

    /// `place = value` for a type that is stored as a whole: an optional, a
    /// `str` or a tuple.
    fn gen_plain_assignment(&mut self, lhs: ExprId, rhs: ExprId) -> Option<BasicValueEnum<'ink>> {
        let value = self.gen_expr(rhs)?;
        let place = self.gen_place_expr(lhs)?;
        let store = self
            .builder
            .build_store(place, value)
            .expect("failed to build store");
        self.tag_if_indexed(lhs, store);
        Some(self.gen_empty())
    }

    /// `a == b` / `a != b` on optionals: equal when both are `nil`, or both
    /// are present with equal payloads. A `nil` payload is zero, so the
    /// payload comparison is always well defined, and it only decides the
    /// result when both are present.
    fn gen_optional_eq(
        &mut self,
        lhs: ExprId,
        rhs: ExprId,
        payload_ty: &Ty,
        negated: bool,
    ) -> Option<BasicValueEnum<'ink>> {
        let lhs_value = self.gen_expr(lhs)?.into_struct_value();
        let rhs_value = self.gen_expr(rhs)?.into_struct_value();
        let b = &self.builder;
        let lhs_present = b
            .build_extract_value(lhs_value, 0, "lhs.is_some")
            .expect("an optional has a present flag")
            .into_int_value();

        let equal = if matches!(self.body[rhs], Expr::Literal(Literal::Nil)) {
            b.build_not(lhs_present, "is_nil")
                .expect("failed to negate a present flag")
        } else {
            let rhs_present = b
                .build_extract_value(rhs_value, 0, "rhs.is_some")
                .expect("an optional has a present flag")
                .into_int_value();
            let lhs_payload = b
                .build_extract_value(lhs_value, 1, "lhs.value")
                .expect("an optional has a payload");
            let rhs_payload = b
                .build_extract_value(rhs_value, 1, "rhs.value")
                .expect("an optional has a payload");
            let payloads_equal = match payload_ty.interned() {
                TyKind::Float(_) => b
                    .build_float_compare(
                        FloatPredicate::OEQ,
                        lhs_payload.into_float_value(),
                        rhs_payload.into_float_value(),
                        "payload_eq",
                    )
                    .expect("failed to compare payloads"),
                TyKind::Int(_) | TyKind::Bool => b
                    .build_int_compare(
                        IntPredicate::EQ,
                        lhs_payload.into_int_value(),
                        rhs_payload.into_int_value(),
                        "payload_eq",
                    )
                    .expect("failed to compare payloads"),
                _ => unreachable!(
                    "inference only admits optional equality on scalar payloads, or against `nil`"
                ),
            };
            let flags_equal = b
                .build_int_compare(IntPredicate::EQ, lhs_present, rhs_present, "flags_eq")
                .expect("failed to compare present flags");
            let absent = b
                .build_not(lhs_present, "absent")
                .expect("failed to negate a present flag");
            let absent_or_equal = b
                .build_or(absent, payloads_equal, "absent_or_equal")
                .expect("failed to combine optional comparison");
            b.build_and(flags_equal, absent_or_equal, "optional_eq")
                .expect("failed to combine optional comparison")
        };

        Some(if negated {
            self.builder
                .build_not(equal, "optional_ne")
                .expect("failed to negate an optional comparison")
                .into()
        } else {
            equal.into()
        })
    }

    /// Generates IR for a `match` (and the `if let`/`while let` that lower
    /// to one).
    ///
    /// The scrutinee is evaluated once; then each arm's pattern is tested in
    /// order, and the first that matches binds its names and runs its arm.
    /// An arm whose pattern always matches ends the chain. Falling off the
    /// end is `unreachable`: inference has proved the match exhaustive, and
    /// an `if let` ends in a `_` arm.
    fn gen_match(
        &mut self,
        match_expr: ExprId,
        scrutinee: ExprId,
        arms: &[MatchArm],
    ) -> Option<BasicValueEnum<'ink>> {
        let value = self.gen_expr(scrutinee)?;
        let ty = self.infer[scrutinee].clone();
        let merge_block = self.context.append_basic_block(self.fn_value, "match.end");
        let mut incoming: Vec<(BasicValueEnum<'ink>, BasicBlock<'ink>)> = Vec::new();
        let mut falls_through = true;

        for arm in arms {
            let arm_block = self.context.append_basic_block(self.fn_value, "match.arm");
            let next_block = if let Some(matched) = self.gen_pat_test(arm.pat, value, &ty) {
                let next_block = self.context.append_basic_block(self.fn_value, "match.next");
                self.builder
                    .build_conditional_branch(matched, arm_block, next_block)
                    .expect("failed to branch on a match arm");
                Some(next_block)
            } else {
                self.builder
                    .build_unconditional_branch(arm_block)
                    .expect("failed to branch to a match arm");
                None
            };

            self.builder.position_at_end(arm_block);
            self.gen_pat_bind(arm.pat, value, &ty);
            if let Some(arm_value) = self.gen_expr(arm.expr) {
                let end_block = self.builder.get_insert_block().unwrap();
                self.builder
                    .build_unconditional_branch(merge_block)
                    .expect("failed to branch out of a match arm");
                incoming.push((arm_value, end_block));
            }

            if let Some(next_block) = next_block {
                self.builder.position_at_end(next_block);
            } else {
                falls_through = false;
                break;
            }
        }

        // Inference has proved the match exhaustive, so this is reached only
        // by a value no declaration describes -- an enum tag a hot reload
        // has removed the variant of. Stop with the location rather than
        // run on.
        if falls_through {
            self.gen_panic("no `match` arm matches this value", match_expr);
        }

        if incoming.is_empty() {
            merge_block
                .remove_from_function()
                .expect("the merge block must have a parent");
            return None;
        }

        self.builder.position_at_end(merge_block);
        if self.infer[match_expr].is_empty() {
            return Some(self.gen_empty());
        }
        Some(self.gen_merge(&incoming, "match"))
    }

    /// Emits the test of `pat` against `value` (of type `ty`), or returns
    /// `None` when the pattern matches every value and needs no test.
    fn gen_pat_test(
        &mut self,
        pat: PatId,
        value: BasicValueEnum<'ink>,
        ty: &Ty,
    ) -> Option<IntValue<'ink>> {
        let body = self.body.clone();
        if let TyKind::Optional(payload) = ty.interned() {
            return match &body[pat] {
                Pat::Wild | Pat::Missing => None,
                Pat::Literal(Literal::Nil) => {
                    let present = self.optional_present(value);
                    Some(
                        self.builder
                            .build_not(present, "is_nil")
                            .expect("failed to negate a present flag"),
                    )
                }
                // Any other pattern tests the payload of a present value.
                _ => {
                    let present = self.optional_present(value);
                    let payload_value = self
                        .builder
                        .build_extract_value(value.into_struct_value(), 1, "value")
                        .expect("an optional has a payload");
                    Some(match self.gen_pat_test(pat, payload_value, payload) {
                        None => present,
                        Some(matched) => self
                            .builder
                            .build_and(present, matched, "some_and")
                            .expect("failed to combine pattern tests"),
                    })
                }
            };
        }

        match &body[pat] {
            Pat::Wild | Pat::Missing | Pat::Bind { .. } => None,
            Pat::Literal(literal) => Some(self.gen_literal_test(literal, value)),
            Pat::Tuple(args) => {
                let TyKind::Tuple(_, elements) = ty.interned() else {
                    unreachable!("inference checked that a tuple pattern matches a tuple");
                };
                let mut all: Option<IntValue<'ink>> = None;
                for (idx, (&arg, element_ty)) in args.iter().zip(elements.iter()).enumerate() {
                    let element = self
                        .builder
                        .build_extract_value(value.into_struct_value(), idx as u32, "element")
                        .expect("tuple element index checked by inference");
                    if let Some(matched) = self.gen_pat_test(arg, element, element_ty) {
                        all = Some(match all {
                            None => matched,
                            Some(previous) => self
                                .builder
                                .build_and(previous, matched, "and")
                                .expect("failed to combine pattern tests"),
                        });
                    }
                }
                all
            }
            Pat::Path(_) | Pat::TupleStruct { .. } => {
                let variant = self
                    .infer
                    .variant_of_pat(pat)
                    .expect("inference resolved every variant pattern");
                let enum_value = value.into_struct_value();
                let tag = self
                    .builder
                    .build_extract_value(enum_value, 0, "tag")
                    .expect("an enum has a tag")
                    .into_int_value();
                let mut all = self
                    .builder
                    .build_int_compare(
                        IntPredicate::EQ,
                        tag,
                        tag.get_type().const_int(u64::from(variant.index()), false),
                        "is_variant",
                    )
                    .expect("failed to compare an enum tag");
                // The payload tests read the variant's slots whatever the
                // tag is: an inactive slot holds a zero, which every test
                // can safely look at, and the tag test decides the result.
                let args = match &body[pat] {
                    Pat::TupleStruct { args, .. } => args.clone(),
                    _ => Vec::new(),
                };
                let enum_args = codira_hir::mono::enum_args(ty);
                let field_tys = variant.field_types_at(self.db, &enum_args);
                let slots = variant.slots_at(self.db, &enum_args);
                for ((&arg, field_ty), slot) in args.iter().zip(field_tys.iter()).zip(slots) {
                    let field = self
                        .builder
                        .build_extract_value(enum_value, slot as u32 + 1, "payload")
                        .expect("a payload slot exists in the layout");
                    if let Some(matched) = self.gen_pat_test(arg, field, field_ty) {
                        all = self
                            .builder
                            .build_and(all, matched, "and")
                            .expect("failed to combine pattern tests");
                    }
                }
                Some(all)
            }
        }
    }

    /// The present flag of an optional value.
    fn optional_present(&self, value: BasicValueEnum<'ink>) -> IntValue<'ink> {
        self.builder
            .build_extract_value(value.into_struct_value(), 0, "is_some")
            .expect("an optional has a present flag")
            .into_int_value()
    }

    /// Compares `value` against a literal pattern.
    fn gen_literal_test(
        &mut self,
        literal: &Literal,
        value: BasicValueEnum<'ink>,
    ) -> IntValue<'ink> {
        match literal {
            Literal::Bool(b) => {
                let constant = self.context.bool_type().const_int(u64::from(*b), false);
                self.builder
                    .build_int_compare(IntPredicate::EQ, value.into_int_value(), constant, "is_lit")
                    .expect("failed to compare with a literal")
            }
            Literal::Int(int) => {
                let int_value = value.into_int_value();
                let int_type = int_value.get_type();
                let constant = if int_type.get_bit_width() > 64 {
                    int_type.const_int_arbitrary_precision(&[
                        int.value as u64,
                        (int.value >> 64) as u64,
                    ])
                } else {
                    int_type.const_int(int.value as u64, false)
                };
                self.builder
                    .build_int_compare(IntPredicate::EQ, int_value, constant, "is_lit")
                    .expect("failed to compare with a literal")
            }
            Literal::Float(float) => {
                let float_value = value.into_float_value();
                let constant = float_value.get_type().const_float(float.value);
                self.builder
                    .build_float_compare(FloatPredicate::OEQ, float_value, constant, "is_lit")
                    .expect("failed to compare with a literal")
            }
            Literal::String(text) => {
                let literal = self.gen_string_literal(text).into_struct_value();
                let value = value.into_struct_value();
                let mut args: Vec<BasicMetadataValueEnum<'ink>> = Vec::with_capacity(4);
                for string in [value, literal] {
                    args.push(
                        self.builder
                            .build_extract_value(string, 0, "str.data")
                            .expect("a str has a data pointer")
                            .into(),
                    );
                    args.push(
                        self.builder
                            .build_extract_value(string, 1, "str.len")
                            .expect("a str has a length")
                            .into(),
                    );
                }
                let (fn_ty, fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
                    self.external_globals.dispatch_table,
                    &self.builder,
                    &intrinsics::str_eq,
                );
                let equal = self
                    .builder
                    .build_indirect_call(fn_ty, fn_ptr, &args, "str_eq")
                    .expect("failed to call str_eq")
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                self.builder
                    .build_int_compare(
                        IntPredicate::NE,
                        equal,
                        equal.get_type().const_zero(),
                        "is_lit",
                    )
                    .expect("failed to compare with a literal")
            }
            Literal::Nil => unreachable!("`nil` patterns are tested on the optional itself"),
        }
    }

    /// Binds the names in `pat`, which has already matched `value`.
    fn gen_pat_bind(&mut self, pat: PatId, value: BasicValueEnum<'ink>, ty: &Ty) {
        let body = self.body.clone();
        if let TyKind::Optional(payload) = ty.interned() {
            if !matches!(
                &body[pat],
                Pat::Wild | Pat::Missing | Pat::Literal(Literal::Nil)
            ) {
                let payload_value = self
                    .builder
                    .build_extract_value(value.into_struct_value(), 1, "value")
                    .expect("an optional has a payload");
                self.gen_pat_bind(pat, payload_value, payload);
            }
            return;
        }
        match &body[pat] {
            Pat::Bind { .. } => self.bind_pattern_to_value(pat, value),
            Pat::Tuple(args) => {
                let TyKind::Tuple(_, elements) = ty.interned() else {
                    return;
                };
                for (idx, (&arg, element_ty)) in args.iter().zip(elements.iter()).enumerate() {
                    let element = self
                        .builder
                        .build_extract_value(value.into_struct_value(), idx as u32, "element")
                        .expect("tuple element index checked by inference");
                    self.gen_pat_bind(arg, element, element_ty);
                }
            }
            Pat::TupleStruct { args, .. } => {
                let Some(variant) = self.infer.variant_of_pat(pat) else {
                    return;
                };
                let enum_value = value.into_struct_value();
                let enum_args = codira_hir::mono::enum_args(ty);
                let field_tys = variant.field_types_at(self.db, &enum_args);
                let slots = variant.slots_at(self.db, &enum_args);
                for ((&arg, field_ty), slot) in args.iter().zip(field_tys.iter()).zip(slots) {
                    let field = self
                        .builder
                        .build_extract_value(enum_value, slot as u32 + 1, "payload")
                        .expect("a payload slot exists in the layout");
                    self.gen_pat_bind(arg, field, field_ty);
                }
            }
            Pat::Wild | Pat::Missing | Pat::Literal(_) | Pat::Path(_) => {}
        }
    }

    /// Generates IR for `Shape.Circle(args)` or `Shape.Point`: the enum's
    /// all-zero value with the variant's tag and its payload in the
    /// variant's slots (see `codira_hir::EnumLayout`). Every slot the
    /// variant does not use stays zero, so a reference slot is null.
    fn gen_variant(
        &mut self,
        expr: ExprId,
        variant: codira_hir::EnumVariant,
    ) -> Option<BasicValueEnum<'ink>> {
        let body = self.body.clone();
        let args: Vec<ExprId> = match &body[expr] {
            Expr::MethodCall { args, .. } => args.clone(),
            _ => Vec::new(),
        };
        // The construction's type is concrete here (a generic body is
        // generated per instance), so it names the instantiation and with
        // it the layout.
        let enum_ty = self.infer[expr].clone();
        let enum_args = codira_hir::mono::enum_args(&enum_ty);
        let enum_type = self.hir_types.get_enum_type(&enum_ty);
        let tag = self
            .context
            .i32_type()
            .const_int(u64::from(variant.index()), false);
        let mut value: AggregateValueEnum<'ink> = self
            .builder
            .build_insert_value(enum_type.const_zero(), tag, 0, "variant")
            .expect("failed to set an enum tag");
        for (&arg, slot) in args.iter().zip(variant.slots_at(self.db, &enum_args)) {
            let payload = self.gen_expr(arg)?;
            value = self
                .builder
                .build_insert_value(value, payload, slot as u32 + 1, "payload")
                .expect("failed to set an enum payload");
        }
        Some(value.into_struct_value().into())
    }

    /// `a == b` / `a != b` on an enum whose payloads are all numbers or
    /// `bool`s: the tags are equal and so is every slot. A slot the variant
    /// does not use is zero on both sides, so it never decides the result;
    /// a float slot compares like float `==` (`NaN` is unequal to itself).
    fn gen_enum_eq(
        &mut self,
        lhs: ExprId,
        rhs: ExprId,
        enum_ty: &Ty,
        negated: bool,
    ) -> Option<BasicValueEnum<'ink>> {
        let lhs_value = self.gen_expr(lhs)?.into_struct_value();
        let rhs_value = self.gen_expr(rhs)?.into_struct_value();
        let layout = codira_hir::mono::enum_layout(self.db, enum_ty).expect("`==` on an enum type");

        let b = &self.builder;
        let field = |value: StructValue<'ink>, idx: u32| {
            b.build_extract_value(value, idx, "field")
                .expect("an enum field exists in its layout")
        };
        let mut equal = b
            .build_int_compare(
                IntPredicate::EQ,
                field(lhs_value, 0).into_int_value(),
                field(rhs_value, 0).into_int_value(),
                "tag_eq",
            )
            .expect("failed to compare enum tags");
        for (idx, slot) in layout.slots.iter().enumerate() {
            let (l, r) = (
                field(lhs_value, idx as u32 + 1),
                field(rhs_value, idx as u32 + 1),
            );
            let slot_equal = match slot.ty.interned() {
                TyKind::Float(_) => b
                    .build_float_compare(
                        FloatPredicate::OEQ,
                        l.into_float_value(),
                        r.into_float_value(),
                        "slot_eq",
                    )
                    .expect("failed to compare enum payloads"),
                TyKind::Int(_) | TyKind::Bool => b
                    .build_int_compare(
                        IntPredicate::EQ,
                        l.into_int_value(),
                        r.into_int_value(),
                        "slot_eq",
                    )
                    .expect("failed to compare enum payloads"),
                _ => unreachable!("inference only admits `==` on enums with scalar payloads"),
            };
            equal = b
                .build_and(equal, slot_equal, "enum_eq")
                .expect("failed to combine enum comparison");
        }
        Some(if negated {
            b.build_not(equal, "enum_ne")
                .expect("failed to negate an enum comparison")
                .into()
        } else {
            equal.into()
        })
    }

    /// Generates IR for `continue`: a jump to the innermost loop's
    /// `continue_block`. Like `break`, it never yields a value.
    fn gen_continue(&mut self) -> Option<BasicValueEnum<'ink>> {
        let continue_block = self
            .active_loop
            .as_ref()
            .expect("inference rejects `continue` outside a loop")
            .continue_block;
        self.builder
            .build_unconditional_branch(continue_block)
            .expect("failed to build branch for continue");
        None
    }

    /// Generates IR for `for pat in iterable { body }`.
    fn gen_for(
        &mut self,
        for_expr: ExprId,
        pat: PatId,
        iterable: ExprId,
        body_expr: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        let body = self.body.clone();
        // Over a lazy pipeline or an iterator object: see `pipelines`.
        if self.infer.iterator_next(for_expr).is_some()
            || matches!(self.infer[iterable].interned(), TyKind::Iter(_))
        {
            return self.gen_pipeline_for(for_expr, pat, iterable, body_expr);
        }
        // Over the pieces of a string, without the array: see `pipelines`.
        if self.text_pieces_call(iterable).is_some() {
            return self.gen_text_for(pat, iterable, body_expr);
        }
        if let Expr::Range {
            start,
            end,
            inclusive,
        } = body[iterable]
        {
            self.gen_for_range(pat, start, end, inclusive, body_expr)
        } else if self.infer.builtin_call(iterable)
            == Some(BuiltinFn::Array(codira_hir::ArrayMethod::Enumerate))
        {
            // `for (i, x) in xs.enumerate()`: the array loop, binding pairs.
            let Expr::MethodCall { receiver, .. } = body[iterable] else {
                unreachable!("`enumerate()` is a method call")
            };
            self.gen_for_array(pat, receiver, body_expr, Some(iterable))
        } else {
            self.gen_for_array(pat, iterable, body_expr, None)
        }
    }

    /// Emits the condition/body/step/exit skeleton shared by both kinds of
    /// `for` loop, around a counter stored in `counter`.
    ///
    /// `in_range` builds the loop condition from the current index;
    /// `element` turns the index into the value bound to the pattern; and
    /// `last` -- for an inclusive range -- is the index after which the loop
    /// stops *before* stepping, so a range ending at its type's maximum never
    /// wraps around.
    #[allow(clippy::too_many_arguments)]
    fn gen_counted_loop(
        &mut self,
        pat: PatId,
        body_expr: ExprId,
        counter: PointerValue<'ink>,
        int_type: inkwell::types::IntType<'ink>,
        in_range: &dyn Fn(&Self, IntValue<'ink>) -> IntValue<'ink>,
        element: &dyn Fn(&Self, IntValue<'ink>) -> BasicValueEnum<'ink>,
        last: Option<IntValue<'ink>>,
    ) -> Option<BasicValueEnum<'ink>> {
        let context = self.context;
        let cond_block = context.append_basic_block(self.fn_value, "for.cond");
        let body_block = context.append_basic_block(self.fn_value, "for.body");
        let step_block = context.append_basic_block(self.fn_value, "for.step");
        let exit_block = context.append_basic_block(self.fn_value, "for.end");

        self.builder
            .build_unconditional_branch(cond_block)
            .expect("failed to branch to a for loop condition");

        // cond: is the index still in range?
        self.builder.position_at_end(cond_block);
        let index = self
            .builder
            .build_load(int_type, counter, "i")
            .expect("failed to load a for loop index")
            .into_int_value();
        let condition = in_range(self, index);
        self.builder
            .build_conditional_branch(condition, body_block, exit_block)
            .expect("failed to branch on a for loop condition");

        // body: bind the element, run the body.
        self.builder.position_at_end(body_block);
        let value = element(self, index);
        self.bind_pattern_to_value(pat, value);
        let (exit_block, _, value) = self.gen_loop_block_expr(body_expr, exit_block, step_block);
        if value.is_some() {
            self.builder
                .build_unconditional_branch(step_block)
                .expect("failed to branch to a for loop step");
        }

        // step: advance the index.
        self.builder.position_at_end(step_block);
        let index = self
            .builder
            .build_load(int_type, counter, "i")
            .expect("failed to load a for loop index")
            .into_int_value();
        if let Some(last) = last {
            let is_last = self
                .builder
                .build_int_compare(IntPredicate::EQ, index, last, "is_last")
                .expect("failed to compare a for loop index");
            let increment_block = context.append_basic_block(self.fn_value, "for.increment");
            self.builder
                .build_conditional_branch(is_last, exit_block, increment_block)
                .expect("failed to branch on the last iteration");
            self.builder.position_at_end(increment_block);
        }
        let next = self
            .builder
            .build_int_add(index, int_type.const_int(1, false), "next")
            .expect("failed to increment a for loop index");
        self.builder
            .build_store(counter, next)
            .expect("failed to store a for loop index");
        self.builder
            .build_unconditional_branch(cond_block)
            .expect("failed to branch back to a for loop condition");

        self.builder.position_at_end(exit_block);
        Some(self.gen_empty())
    }

    /// `for i in start..end` / `start..=end`: a counted loop over an integer.
    ///
    /// Both bounds are evaluated once, before the first iteration. The
    /// half-open form tests `i < end` and steps `i + 1`, which cannot
    /// overflow because `i < end`. The inclusive form tests `i <= end` and
    /// leaves *before* stepping when `i == end`, so `0..=255` over a `u8`
    /// runs 256 times and stops instead of wrapping around forever.
    fn gen_for_range(
        &mut self,
        pat: PatId,
        start: ExprId,
        end: ExprId,
        inclusive: bool,
        body_expr: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        let start_value = self.gen_expr(start)?.into_int_value();
        let end_value = self.gen_expr(end)?.into_int_value();
        let signed = match self.infer[start].interned() {
            TyKind::Int(int_ty) => int_ty.signedness == codira_hir::Signedness::Signed,
            _ => true,
        };
        let int_type = start_value.get_type();

        let counter = self
            .new_alloca_builder()
            .build_alloca(int_type, "for.index")
            .expect("failed to build alloca for a for loop index");
        self.builder
            .build_store(counter, start_value)
            .expect("failed to initialize a for loop index");

        let predicate = match (inclusive, signed) {
            (false, true) => IntPredicate::SLT,
            (false, false) => IntPredicate::ULT,
            (true, true) => IntPredicate::SLE,
            (true, false) => IntPredicate::ULE,
        };
        self.gen_counted_loop(
            pat,
            body_expr,
            counter,
            int_type,
            &|this, index| {
                this.builder
                    .build_int_compare(predicate, index, end_value, "in_range")
                    .expect("failed to compare a for loop index")
            },
            &|_, index| index.into(),
            inclusive.then_some(end_value),
        )
    }

    /// `for x in array`: visits each element in order.
    ///
    /// The array and its length are read once, before the first iteration,
    /// and the loop also stops at the array's current length, read on every
    /// iteration: an element the body pushes is not visited, and a body that
    /// shortens the array ends the loop at its new end (see `arrays`). So
    /// the element loads need no bounds check, and when nothing in the loop
    /// writes a length LLVM hoists the second load and folds the two tests
    /// into one -- the loop costs what hand-written unchecked indexing
    /// would.
    ///
    /// `enumerate` is the `xs.enumerate()` expression when the loop binds
    /// `(index, element)` pairs.
    fn gen_for_array(
        &mut self,
        pat: PatId,
        iterable: ExprId,
        body_expr: ExprId,
        enumerate: Option<ExprId>,
    ) -> Option<BasicValueEnum<'ink>> {
        let element_ty = self.infer[iterable]
            .as_array()
            .expect("inference accepts only ranges and arrays as iterables")
            .clone();
        let array_ty = self.hir_types.get_array_type(&element_ty);
        let array_value = self.gen_expr(iterable)?;
        // Safety: the iterable's type is an array, so its value is one.
        let array = unsafe {
            RuntimeArrayValue::from_ptr_unchecked(array_value.into_pointer_value(), array_ty)
        };
        let length_type = array.length_ty();
        let length = self
            .builder
            .build_load(length_type, array.get_length_ptr(&self.builder), "len")
            .expect("failed to load an array's length");
        array_tbaa::tag_value(length, ArrayAccess::Length);
        let length = length.into_int_value();
        let pair_ty = enumerate.map(|enumerate| {
            let pair = self.infer[enumerate]
                .as_array()
                .expect("`enumerate()` is an array of pairs")
                .clone();
            self.hir_types
                .get_basic_type(&pair)
                .expect("a pair is a basic type")
                .into_struct_type()
        });

        let counter = self
            .new_alloca_builder()
            .build_alloca(length_type, "for.index")
            .expect("failed to build alloca for a for loop index");
        self.builder
            .build_store(counter, length_type.const_zero())
            .expect("failed to initialize a for loop index");

        self.gen_counted_loop(
            pat,
            body_expr,
            counter,
            length_type,
            &|this, index| this.gen_live_in_range(array, index, length),
            &|this, index| {
                let elements = array.get_elements(&this.builder);
                let element_basic_ty = array.element_ty();
                // Safety: `index` is below the current length, checked by
                // the loop condition.
                let element_ptr = unsafe {
                    this.builder
                        .build_gep(element_basic_ty, elements, &[index], "element_ptr")
                        .expect("failed to build GEP for a for loop element")
                };
                let element = this
                    .builder
                    .build_load(element_basic_ty, element_ptr, "element")
                    .expect("failed to load a for loop element");
                match pair_ty {
                    None => element,
                    Some(pair_ty) => {
                        let pair = this
                            .builder
                            .build_insert_value(pair_ty.get_undef(), index, 0, "pair.index")
                            .expect("failed to build an index pair");
                        this.builder
                            .build_insert_value(pair, element, 1, "pair")
                            .expect("failed to build an index pair")
                            .into_struct_value()
                            .into()
                    }
                }
            },
            None,
        )
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

        let receiver_ty = self.infer[receiver_expr].clone();
        let hir_struct = receiver_ty.as_struct().expect("expected a struct");

        let hir_struct_name = hir_struct.name(self.db);

        let field_idx = hir_struct
            .field(self.db, name)
            .expect("expected a struct field")
            .index(self.db);

        let field_ir_name = &format!("{hir_struct_name}.{name}");
        if self.is_place_expr(receiver_expr) {
            let struct_ty = self.hir_types.get_struct_type(&receiver_ty);
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

        let receiver_ty = self.infer[receiver_expr].clone();
        let hir_struct = receiver_ty.as_struct().expect("expected a struct");

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
                    self.hir_types.get_struct_type(&receiver_ty),
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

    /// Allocates the array `expr` evaluates to, with room for `length_value`
    /// elements. The length field is left for the caller to set once the
    /// elements are stored.
    fn alloc_array(
        &mut self,
        expr: ExprId,
        length_value: inkwell::values::IntValue<'ink>,
    ) -> RuntimeArrayValue<'ink> {
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
        RuntimeArrayValue::from_ptr(untyped_array_ptr, array_ty)
            .expect("unable to convert pointer to typed reference")
    }

    /// Generates code to construct an array literal at runtime. Returns `None`
    /// if the code generation for the array literal never returns.
    fn gen_array(&mut self, expr: ExprId, exprs: &[ExprId]) -> Option<RuntimeArrayValue<'ink>> {
        let length_value = self
            .hir_types
            .get_usize_type()
            .const_int(exprs.len() as u64, false);
        let array = self.alloc_array(expr, length_value);
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
            let store = self
                .builder
                .build_store(element_ptr, expr_value)
                .expect("failed to build store for array element");
            array_tbaa::tag(store, ArrayAccess::Element);
        }

        // Once all values have been stored in the array, update the length of the array
        let length = array.length_ty().const_int(exprs.len() as u64, false);
        let array_length_ptr = array.get_length_ptr(&self.builder);
        let store = self
            .builder
            .build_store(array_length_ptr, length)
            .expect("failed to build store for array length");
        array_tbaa::tag(store, ArrayAccess::Length);

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
        let element = self
            .builder
            .build_load(element_basic_ty, element_ptr, "")
            .expect("failed to build load for array index");
        array_tbaa::tag_value(element, ArrayAccess::Element);
        Some(element)
    }

    /// Generates the address of `base[index]`, checking that the index is
    /// in range first.
    ///
    /// An out-of-range index -- including a negative one, which the unsigned
    /// comparison sees as huge -- calls `codira.panic_index_out_of_bounds`
    /// with the index, the length and this expression's source location,
    /// which ends the program; the access itself is never executed. The
    /// check is one compare and a branch the optimizer knows is unlikely
    /// (the failing side never returns), and LLVM removes it outright where
    /// it can prove the index in range. `unchecked_get` skips it.
    fn gen_place_index(
        &mut self,
        expr: ExprId,
        base: ExprId,
        index: ExprId,
    ) -> Option<PointerValue<'ink>> {
        let base_value = self.gen_expr(base)?;
        let base = self.array_value(base, base_value);
        let index_value = self.gen_expr(index)?.into_int_value();

        let length_type = base.length_ty();
        let length = self
            .builder
            .build_load(length_type, base.get_length_ptr(&self.builder), "len")
            .expect("failed to load an array's length");
        array_tbaa::tag_value(length, ArrayAccess::Length);
        let length = length.into_int_value();

        // Compare in whichever of the two widths is wider, so an `i128`
        // index is not truncated into range before it is checked.
        let signed = self.is_signed_int(index);
        let index_bits = index_value.get_type().get_bit_width();
        let (wide_index, wide_length) = if index_bits > length_type.get_bit_width() {
            let wide_length = self
                .builder
                .build_int_z_extend(length, index_value.get_type(), "len.wide")
                .expect("failed to widen an array length");
            (index_value, wide_length)
        } else {
            (
                self.index_to_length_type(index, index_value, length_type),
                length,
            )
        };
        let in_bounds = self
            .builder
            .build_int_compare(IntPredicate::ULT, wide_index, wide_length, "in_bounds")
            .expect("failed to build a bounds check");

        let ok_block = self.context.append_basic_block(self.fn_value, "index.ok");
        let fail_block = self
            .context
            .append_basic_block(self.fn_value, "index.out_of_bounds");
        self.builder
            .build_conditional_branch(in_bounds, ok_block, fail_block)
            .expect("failed to branch on a bounds check");

        // The failing side: report and never return.
        self.builder.position_at_end(fail_block);
        let i64_type = self.context.i64_type();
        let reported_index = if index_bits > 64 {
            self.builder
                .build_int_truncate(index_value, i64_type, "index")
        } else if signed {
            self.builder
                .build_int_s_extend_or_bit_cast(index_value, i64_type, "index")
        } else {
            self.builder
                .build_int_z_extend_or_bit_cast(index_value, i64_type, "index")
        }
        .expect("failed to widen an index for a panic report");
        let reported_length = self
            .builder
            .build_int_z_extend_or_bit_cast(length, i64_type, "len")
            .expect("failed to widen a length for a panic report");
        let (file, line, column) = self.source_location(expr);
        let file_data = self
            .builder
            .build_extract_value(file, 0, "file.data")
            .expect("a str has a data pointer");
        let file_len = self
            .builder
            .build_extract_value(file, 1, "file.len")
            .expect("a str has a length");
        let u32_type = self.context.i32_type();
        self.gen_runtime_call(
            &intrinsics::panic_index_out_of_bounds,
            &[
                reported_index.into(),
                self.context
                    .i8_type()
                    .const_int(u64::from(signed), false)
                    .into(),
                reported_length.into(),
                file_data.into(),
                file_len.into(),
                u32_type.const_int(u64::from(line), false).into(),
                u32_type.const_int(u64::from(column), false).into(),
            ],
        );
        self.builder
            .build_unreachable()
            .expect("failed to build unreachable after a panic");

        // The in-range side: the access itself.
        self.builder.position_at_end(ok_block);
        let index_value = self.index_to_length_type(index, index_value, length_type);
        let elements = base.get_elements(&self.builder);
        let element_basic_ty = base.element_ty();
        // Safety: `index < len`, checked above.
        Some(unsafe {
            self.builder
                .build_gep(
                    element_basic_ty,
                    elements,
                    &[index_value],
                    &format!("{}+index", elements.get_name().to_string_lossy()),
                )
                .expect("failed to build GEP for array index")
        })
    }

    /// Charges one unit of fuel: decrements the counter `fuelCounter` points
    /// at and, when it drops below zero, calls the runtime's
    /// `codira.fuel_exhausted`. Does nothing in a module that is not
    /// fuel-instrumented.
    ///
    /// The counter is read and written with monotonic (relaxed) atomics so
    /// that instrumented code running on several threads shares one budget
    /// without a data race; on x86-64 and `AArch64` these are plain loads and
    /// stores. Decrements from different threads can overlap, so with
    /// several threads the budget is approximate; with one it is exact.
    fn gen_fuel_check(&mut self) {
        use inkwell::values::BasicValue;

        let Some(counter_global) = self.external_globals.fuel_counter else {
            return;
        };
        let context = self.context;
        let i64_type = context.i64_type();
        let ptr_type = context.ptr_type(AddressSpace::default());

        let counter = self
            .builder
            .build_load(ptr_type, counter_global.as_pointer_value(), "fuel.counter")
            .expect("failed to load the fuel counter pointer")
            .into_pointer_value();
        let fuel = self
            .builder
            .build_load(i64_type, counter, "fuel")
            .expect("failed to load the fuel counter");
        let load = fuel
            .as_instruction_value()
            .expect("a load is an instruction");
        load.set_alignment(8).expect("8 is a valid alignment");
        load.set_atomic_ordering(inkwell::AtomicOrdering::Monotonic)
            .expect("a load can be atomic");

        let remaining = self
            .builder
            .build_int_sub(
                fuel.into_int_value(),
                i64_type.const_int(1, false),
                "fuel.next",
            )
            .expect("failed to decrement the fuel counter");
        let store = self
            .builder
            .build_store(counter, remaining)
            .expect("failed to store the fuel counter");
        store.set_alignment(8).expect("8 is a valid alignment");
        store
            .set_atomic_ordering(inkwell::AtomicOrdering::Monotonic)
            .expect("a store can be atomic");

        let exhausted = self
            .builder
            .build_int_compare(
                IntPredicate::SLT,
                remaining,
                i64_type.const_zero(),
                "fuel.exhausted",
            )
            .expect("failed to compare the fuel counter");
        let exhausted_block = context.append_basic_block(self.fn_value, "fuel.exhausted");
        let continue_block = context.append_basic_block(self.fn_value, "fuel.ok");
        self.builder
            .build_conditional_branch(exhausted, exhausted_block, continue_block)
            .expect("failed to branch on the fuel counter");

        self.builder.position_at_end(exhausted_block);
        let (fn_type, fn_ptr) = self.dispatch_table.gen_intrinsic_lookup(
            self.external_globals.dispatch_table,
            &self.builder,
            &intrinsics::fuel_exhausted,
        );
        self.builder
            .build_indirect_call(fn_type, fn_ptr, &[], "")
            .expect("failed to call `codira.fuel_exhausted`");
        self.builder
            .build_unconditional_branch(continue_block)
            .expect("failed to branch after `codira.fuel_exhausted`");

        self.builder.position_at_end(continue_block);
    }

    /// Emits one half of a foreign transition (see
    /// `crate::gc_roots::is_foreign_transition`): `enter` passes the stack
    /// pointer at the call, from which a collection scans this thread while
    /// the foreign function runs.
    fn gen_foreign_transition(&mut self, enter: bool) {
        let (fn_type, fn_ptr) = if enter {
            self.dispatch_table.gen_intrinsic_lookup(
                self.external_globals.dispatch_table,
                &self.builder,
                &intrinsics::gc_foreign_enter,
            )
        } else {
            self.dispatch_table.gen_intrinsic_lookup(
                self.external_globals.dispatch_table,
                &self.builder,
                &intrinsics::gc_foreign_leave,
            )
        };
        let args: Vec<BasicMetadataValueEnum<'ink>> = if enter {
            let ptr_type = self.context.ptr_type(AddressSpace::default());
            let stacksave = inkwell::intrinsics::Intrinsic::find("llvm.stacksave")
                .expect("llvm.stacksave exists")
                .get_declaration(self.module, &[ptr_type.into()])
                .expect("llvm.stacksave has a declaration");
            let sp = self
                .builder
                .build_call(stacksave, &[], "gc.foreign.sp")
                .expect("failed to read the stack pointer")
                .try_as_basic_value()
                .unwrap_basic();
            vec![sp.into()]
        } else {
            Vec::new()
        };
        let call = self
            .builder
            .build_indirect_call(fn_type, fn_ptr, &args, "")
            .expect("failed to build a foreign transition");
        let attribute = self
            .context
            .create_string_attribute("codira-gc-foreign", "");
        call.add_attribute(inkwell::attributes::AttributeLoc::Function, attribute);
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

/// Whether code for `target` can use a fused multiply-add instruction for
/// `llvm.fma`: every `AArch64` CPU has one; on x86 it is the `fma` feature
/// (`--target-cpu haswell` or newer, or `--target-feature +fma`). Any other
/// architecture is treated as lacking one, which errs toward refusing.
fn target_has_fused_multiply_add(target: &codira_target::spec::Target) -> bool {
    match &*target.arch {
        "aarch64" | "arm64" => true,
        "x86_64" | "x86" => target
            .options
            .features
            .split(',')
            .map(str::trim)
            .any(|feature| feature == "+fma"),
        _ => false,
    }
}
