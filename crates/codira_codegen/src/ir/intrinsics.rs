//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{collections::BTreeMap, sync::Arc};

use codira_hir::{Body, Expr, ExprId, HirDatabase, InferenceResult, ValueNs};
use inkwell::{context::Context, targets::TargetData, types::FunctionType};

use crate::{
    intrinsics::{self, Intrinsic},
    ir::dispatch_table::FunctionPrototype,
};

// Use a `BTreeMap` to guarantee deterministically ordered output
pub type IntrinsicsMap<'ink> = BTreeMap<FunctionPrototype, FunctionType<'ink>>;

/// Stores the type information from the `intrinsic` in `entries`
fn collect_intrinsic<'ink>(
    context: &'ink Context,
    target: &TargetData,
    intrinsic: &(impl Intrinsic + ?Sized),
    entries: &mut IntrinsicsMap<'ink>,
) {
    let prototype = intrinsic.prototype();
    entries
        .entry(prototype)
        .or_insert_with(|| intrinsic.ir_type(context, target));
}

/// Iterates over all expressions and stores information on which intrinsics
/// they use in `entries`.
#[allow(clippy::too_many_arguments)]
fn collect_expr<'ink>(
    context: &'ink Context,
    target: &TargetData,
    db: &'_ dyn HirDatabase,
    intrinsics: &mut IntrinsicsMap<'ink>,
    needs_alloc: &mut bool,
    expr_id: ExprId,
    body: &Arc<Body>,
    infer: &InferenceResult,
) {
    let expr = &body[expr_id];

    // A builtin call (`print`/`println`) lowers to calls into the runtime's
    // printing functions, which therefore need dispatch table slots.
    let uses_tasks = matches!(
        expr,
        Expr::Scope { .. }
            | Expr::Spawn { .. }
            | Expr::ChannelSend { .. }
            | Expr::ChannelReceive { .. }
            | Expr::Select { .. }
            | Expr::Offer { .. }
    ) || infer.builtin_call(expr_id) == Some(codira_hir::BuiltinFn::Channel)
        || matches!(infer[expr_id].interned(), codira_hir::TyKind::Session(..));
    if uses_tasks {
        for intrinsic in intrinsics::task_functions() {
            collect_intrinsic(context, target, intrinsic, intrinsics);
        }
    }
    // A string operation calls the runtime's string functions, and most of
    // them allocate.
    if crate::ir::strings::uses_strings(body, infer, expr_id) {
        for intrinsic in intrinsics::string_functions() {
            collect_intrinsic(context, target, intrinsic, intrinsics);
        }
        collect_intrinsic(
            context,
            target,
            &intrinsics::panic_index_out_of_bounds,
            intrinsics,
        );
        *needs_alloc = true;
    }
    if let Some(builtin) = infer.builtin_call(expr_id) {
        if builtin != codira_hir::BuiltinFn::Channel {
            for intrinsic in intrinsics::print_functions() {
                collect_intrinsic(context, target, intrinsic, intrinsics);
            }
        }
        // `array_filled` allocates its array through the runtime.
        if builtin == codira_hir::BuiltinFn::ArrayFilled {
            collect_intrinsic(context, target, &intrinsics::new_array, intrinsics);
            *needs_alloc = true;
        }
        // The array methods grow, sort, search and copy through the
        // runtime, and check indices like indexing does.
        if let codira_hir::BuiltinFn::Array(_) = builtin {
            collect_array_intrinsics(context, target, intrinsics);
            *needs_alloc = true;
        }
        // `collect` makes and grows an array.
        if builtin == codira_hir::BuiltinFn::Pipeline(codira_hir::PipelineOp::Collect) {
            collect_array_intrinsics(context, target, intrinsics);
            *needs_alloc = true;
        }
    } else if let Expr::Call { callee, .. } = expr {
        match infer[*callee].as_callable_def() {
            Some(codira_hir::CallableDef::Struct(_)) => {
                collect_intrinsic(context, target, &intrinsics::new, intrinsics);
                // self.collect_intrinsic(module, entries, &intrinsics::drop);
                *needs_alloc = true;
            }
            // A call through the dispatch table boxes a value-struct
            // argument and result (the runtime's calling convention, see
            // `gen_call`), and boxing allocates: a module whose only
            // allocation is such a call still needs `new`.
            Some(codira_hir::CallableDef::Function(f)) => {
                let boxes_a_value_struct = f.ty(db).callable_sig(db).is_some_and(|sig| {
                    sig.params()
                        .iter()
                        .chain(std::iter::once(sig.ret()))
                        .any(|ty| {
                            ty.is_string()
                                || ty.as_struct().is_some_and(|s| {
                                    s.data(db).memory_kind == codira_hir::StructMemoryKind::Value
                                })
                        })
                });
                if boxes_a_value_struct {
                    collect_intrinsic(context, target, &intrinsics::new, intrinsics);
                    *needs_alloc = true;
                }
            }
            // A call through a function value calls no runtime function.
            None if infer.is_indirect_call(expr_id) => (),
            None => panic!("expected a callable expression"),
        }
    }

    // Creating a function value allocates its object.
    if matches!(expr, Expr::Closure { .. }) || infer.function_value(expr_id).is_some() {
        collect_intrinsic(context, target, &intrinsics::new, intrinsics);
        *needs_alloc = true;
    }

    if let Expr::RecordLit { .. } = expr {
        collect_intrinsic(context, target, &intrinsics::new, intrinsics);
        // self.collect_intrinsic(module, entries, &intrinsics::drop);
        *needs_alloc = true;
    }

    if let Expr::Path(path) = expr {
        let resolver = codira_hir::resolver_for_expr(db, body.owner(), expr_id);
        if let Some((ValueNs::StructId(_), _)) = resolver.resolve_path_as_value_fully(db, path) {
            collect_intrinsic(context, target, &intrinsics::new, intrinsics);
            // self.collect_intrinsic( module, entries, &intrinsics::drop);
            *needs_alloc = true;
        }
    }

    // `x!` panics on `nil`; a `match` may compare strings, and panics when
    // no arm matches (an enum tag a hot reload removed).
    if let Expr::Unwrap { .. } | Expr::Match { .. } = expr {
        collect_intrinsic(context, target, &intrinsics::panic, intrinsics);
    }
    if let Expr::Match { .. } = expr {
        collect_intrinsic(context, target, &intrinsics::str_eq, intrinsics);
    }
    // An integer division the analysis does not know to be safe checks its
    // divisor and panics through the runtime.
    if codira_hir::realtime::division_needs_check(db, body, infer, expr_id) {
        collect_intrinsic(context, target, &intrinsics::panic, intrinsics);
    }

    // Indexing checks its bounds and panics through the runtime when the
    // index is out of range.
    if let Expr::Index { .. } = expr {
        collect_intrinsic(
            context,
            target,
            &intrinsics::panic_index_out_of_bounds,
            intrinsics,
        );
    }

    // Slicing makes a new array and checks its range.
    if let Expr::Index { index, .. } = expr {
        if matches!(body[*index], Expr::Range { .. }) {
            collect_array_intrinsics(context, target, intrinsics);
            *needs_alloc = true;
        }
    }

    if let Expr::Array(_) = expr {
        collect_intrinsic(context, target, &intrinsics::new_array, intrinsics);
        *needs_alloc = true;
    }

    // Recurse further
    expr.walk_child_exprs(|expr_id| {
        collect_expr(
            context,
            target,
            db,
            intrinsics,
            needs_alloc,
            expr_id,
            body,
            infer,
        );
    });
}

/// Collects every runtime function an array method or a slice may call.
fn collect_array_intrinsics<'ink>(
    context: &'ink Context,
    target: &TargetData,
    intrinsics: &mut IntrinsicsMap<'ink>,
) {
    for intrinsic in intrinsics::array_functions() {
        collect_intrinsic(context, target, intrinsic, intrinsics);
    }
    collect_intrinsic(context, target, &intrinsics::new_array, intrinsics);
    collect_intrinsic(context, target, &intrinsics::str_eq, intrinsics);
    collect_intrinsic(context, target, &intrinsics::panic, intrinsics);
    collect_intrinsic(
        context,
        target,
        &intrinsics::panic_index_out_of_bounds,
        intrinsics,
    );
}

/// Collects all intrinsics from the specified `body`.
pub fn collect_fn_body<'ink>(
    context: &'ink Context,
    target: TargetData,
    db: &dyn HirDatabase,
    intrinsics: &mut IntrinsicsMap<'ink>,
    needs_alloc: &mut bool,
    body: &Arc<Body>,
    infer: &InferenceResult,
) {
    collect_expr(
        context,
        &target,
        db,
        intrinsics,
        needs_alloc,
        body.body_expr(),
        body,
        infer,
    );
}

/// Collects the runtime functions fuel-instrumented code calls.
pub fn collect_fuel<'ink>(
    context: &'ink Context,
    target: TargetData,
    intrinsics: &mut IntrinsicsMap<'ink>,
) {
    collect_intrinsic(context, &target, &intrinsics::fuel_exhausted, intrinsics);
}

/// Collects all intrinsics from a function wrapper body.
pub fn collect_wrapper_body<'ink>(
    context: &'ink Context,
    target: TargetData,
    intrinsics: &mut IntrinsicsMap<'ink>,
    needs_alloc: &mut bool,
) {
    collect_intrinsic(context, &target, &intrinsics::new, intrinsics);
    // self.collect_intrinsic(entries, &intrinsics::drop, module);
    *needs_alloc = true;
}
