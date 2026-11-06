//! Copyright (c) 2026 Omnira CJSC
//! Date: November 5, 2026
//!
//! Functionality:
//! - Code generation for postfix `?` (error propagation; see
//!   `codira_hir`'s `ty::infer::propagate` for the typing rules).
//!
//! `r?` on a `Result` is a test of the tag and a branch, nothing else:
//!
//! ```text
//!   %tag = extractvalue %Result %r, 0
//!   %ok  = icmp eq i32 %tag, <Ok>
//!   %ok  = call i1 @llvm.expect.i1(i1 %ok, i1 true)
//!   br i1 %ok, label %propagate.ok, label %propagate.err
//! propagate.err:                      ; build the caller's Err, return it
//!   %e   = extractvalue %Result %r, <Err slot>
//!   ret %Ret { <Err>, .., %e, .. }
//! propagate.ok:
//!   %v   = extractvalue %Result %r, <Ok slot>
//! ```
//!
//! The branch is weighted towards `Ok` (`llvm.expect`, which becomes branch
//! weights): an error is expected to be rare, so LLVM lays the error path
//! out of line, after the function's hot code, keeps the success path
//! straight-line, and does not spend inlining on calls made only to build
//! the error (an `@error_conversion`). With the callee inlined -- a
//! function of the same library, an instance of a generic one, or an
//! `@inline` function of another library, which the caller compiles a copy
//! of -- the `Result` is never materialized: its tag test folds into the
//! callee's own branches.
//!
//! No allocation and no call -- unless the error passes through the
//! enclosing result's `@error_conversion` (`AnyError.from_error(e)`), which
//! is called on the `Err` path like any call of a generic function, at the
//! instance inference recorded for the `?`. The early return goes through the
//! same steps
//! as `return`: open `scope`s end first, and a hardware-safety mode undoes
//! its setup before every `ret` (see `safety`). `x?` on an optional is the
//! same with the present flag, returning the all-zero `nil`.

use codira_hir::{mono, ExprId, TyKind};

use crate::intrinsics;
use inkwell::{
    values::{AggregateValueEnum, BasicValueEnum, IntValue},
    IntPredicate,
};

use super::BodyIrGenerator;

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// Generates `operand?`, the expression `expr`.
    pub(super) fn gen_propagate(
        &mut self,
        expr: ExprId,
        operand: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        let operand_ty = self.infer[operand].clone();
        let returned = self
            .infer
            .propagation(expr)
            .expect("inference records the early return of every `?`")
            .clone();
        let value = self.gen_expr(operand)?.into_struct_value();

        let continue_block = self
            .context
            .append_basic_block(self.fn_value, "propagate.ok");
        let return_block = self
            .context
            .append_basic_block(self.fn_value, "propagate.return");

        match operand_ty.interned() {
            TyKind::Optional(_) => {
                let present = self
                    .builder
                    .build_extract_value(value, 0, "is_some")
                    .expect("an optional has a present flag")
                    .into_int_value();
                let present = self.likely_ok(present, "is_some");
                self.builder
                    .build_conditional_branch(present, continue_block, return_block)
                    .expect("failed to branch on an optional");

                self.builder.position_at_end(return_block);
                let nil = self
                    .hir_types
                    .get_basic_type(&returned)
                    .expect("an optional is a basic type")
                    .const_zero();
                self.gen_early_return(nil);

                self.builder.position_at_end(continue_block);
                Some(
                    self.builder
                        .build_extract_value(value, 1, "value")
                        .expect("an optional has a payload"),
                )
            }
            TyKind::Enum(e, args) => {
                let (ok, err) =
                    codira_hir::result_variants(self.db, *e).expect("`?` on a `Result`");
                let tag = self
                    .builder
                    .build_extract_value(value, 0, "tag")
                    .expect("an enum has a tag")
                    .into_int_value();
                let is_ok = self
                    .builder
                    .build_int_compare(
                        IntPredicate::EQ,
                        tag,
                        tag.get_type().const_int(u64::from(ok.index()), false),
                        "is_ok",
                    )
                    .expect("failed to compare an enum tag");
                let is_ok = self.likely_ok(is_ok, "is_ok");
                self.builder
                    .build_conditional_branch(is_ok, continue_block, return_block)
                    .expect("failed to branch on a `Result`");

                // The caller's `Result[U, E]` has its own layout: the error
                // moves from this value's `Err` slot to that one's.
                self.builder.position_at_end(return_block);
                let err_slot = err.slots_at(self.db, args)[0] as u32 + 1;
                let mut error = self
                    .builder
                    .build_extract_value(value, err_slot, "error")
                    .expect("the `Err` slot exists in the layout");
                let returned_args = mono::enum_args(&returned);
                // `AnyError.from_error(e)`, when the function's error type
                // converts this one.
                if let Some(conversion) = self.infer.method_resolution(expr) {
                    error = self.gen_call(
                        Some(expr),
                        codira_hir::Function::from(conversion),
                        &[error.into()],
                    )?;
                }
                // `Problem.Io(e)`, when the function's error type wraps
                // this one.
                if let Some(wrap) = self.infer.propagation_wrap(expr) {
                    let wrapper_ty = err.field_types_at(self.db, &returned_args).remove(0);
                    let wrapper_args = mono::enum_args(&wrapper_ty);
                    let tag = self
                        .context
                        .i32_type()
                        .const_int(u64::from(wrap.index()), false);
                    let wrapped = self
                        .builder
                        .build_insert_value(
                            self.hir_types.get_enum_type(&wrapper_ty).const_zero(),
                            tag,
                            0,
                            "wrap",
                        )
                        .expect("failed to set an enum tag");
                    error = self
                        .builder
                        .build_insert_value(
                            wrapped,
                            error,
                            wrap.slots_at(self.db, &wrapper_args)[0] as u32 + 1,
                            "wrap",
                        )
                        .expect("failed to set an enum payload")
                        .into_struct_value()
                        .into();
                }
                let returned_type = self.hir_types.get_enum_type(&returned);
                let tag = self
                    .context
                    .i32_type()
                    .const_int(u64::from(err.index()), false);
                let mut result: AggregateValueEnum<'ink> = self
                    .builder
                    .build_insert_value(returned_type.const_zero(), tag, 0, "err")
                    .expect("failed to set an enum tag");
                result = self
                    .builder
                    .build_insert_value(
                        result,
                        error,
                        err.slots_at(self.db, &returned_args)[0] as u32 + 1,
                        "err",
                    )
                    .expect("failed to set an enum payload");
                self.gen_early_return(result.into_struct_value().into());

                self.builder.position_at_end(continue_block);
                let ok_slot = ok.slots_at(self.db, args)[0] as u32 + 1;
                Some(
                    self.builder
                        .build_extract_value(value, ok_slot, "ok")
                        .expect("the `Ok` slot exists in the layout"),
                )
            }
            _ => unreachable!("inference admits `?` only on a `Result` or an optional"),
        }
    }

    /// For a `main` that returns a `Result` (see `codira_hir::main_result`):
    /// before each return, an `Err` writes `Error: <the error>` to standard
    /// error and ends the program with exit status 1, so that the host is
    /// only ever handed an `Ok`.
    pub(super) fn gen_main_result_check(&mut self) {
        let Some(report) = codira_hir::main_result(self.db, self.hir_function) else {
            return;
        };
        let returned = self.instance.sig(self.db).ret().clone();
        let args = mono::enum_args(&returned);
        let returns: Vec<_> = self
            .fn_value
            .get_basic_blocks()
            .into_iter()
            .filter_map(|block| {
                let terminator = block.get_terminator()?;
                if terminator.get_opcode() != inkwell::values::InstructionOpcode::Return {
                    return None;
                }
                let value = terminator.get_operand(0)?.value()?;
                Some((block, terminator, value))
            })
            .collect();
        let saved = self.builder.get_insert_block();
        for (block, terminator, value) in returns {
            terminator.erase_from_basic_block();
            self.builder.position_at_end(block);
            let result = value.into_struct_value();
            let tag = self
                .builder
                .build_extract_value(result, 0, "tag")
                .expect("an enum has a tag")
                .into_int_value();
            let is_err = self
                .builder
                .build_int_compare(
                    IntPredicate::EQ,
                    tag,
                    tag.get_type()
                        .const_int(u64::from(report.err.index()), false),
                    "is_err",
                )
                .expect("failed to compare an enum tag");
            let ok_block = self.context.append_basic_block(self.fn_value, "main.ok");
            let err_block = self.context.append_basic_block(self.fn_value, "main.err");
            self.builder
                .build_conditional_branch(is_err, err_block, ok_block)
                .expect("failed to branch on `main`'s result");
            self.builder.position_at_end(ok_block);
            self.builder
                .build_return(Some(&result))
                .expect("failed to return `main`'s result");

            self.builder.position_at_end(err_block);
            let slot = report.err.slots_at(self.db, &args)[0] as u32 + 1;
            let error = self
                .builder
                .build_extract_value(result, slot, "error")
                .expect("the `Err` slot exists in the layout");
            let text = match (report.error_ty.interned(), report.to_string) {
                (_, Some(to_string)) => self
                    .gen_call(None, to_string, &[error.into()])
                    .expect("`to_string` returns a `String`"),
                (TyKind::String | TyKind::Str, None) => error,
                (TyKind::Int(_) | TyKind::Float(_), None) => {
                    self.gen_to_string_of(&report.error_ty, error)
                }
                _ => self.gen_debug_string(&report.error_ty, error, false),
            };
            let text_ty = match report.error_ty.interned() {
                TyKind::Str if report.to_string.is_none() => TyKind::Str.intern(),
                _ => TyKind::String.intern(),
            };
            let (data, len) = self.string_text_of(&text_ty, text);
            self.string_call(&intrinsics::exit_with_error, &[data.into(), len.into()]);
            self.builder
                .build_unreachable()
                .expect("failed to end `main`'s error path");
        }
        if let Some(block) = saved {
            self.builder.position_at_end(block);
        }
    }

    /// `condition`, through `llvm.expect.i1(condition, true)`: the branch
    /// on it is weighted towards true.
    fn likely_ok(&mut self, condition: IntValue<'ink>, name: &str) -> IntValue<'ink> {
        let bool_ty = self.context.bool_type();
        let expect = inkwell::intrinsics::Intrinsic::find("llvm.expect")
            .and_then(|i| i.get_declaration(self.module, &[bool_ty.into()]))
            .expect("llvm.expect is always available");
        self.builder
            .build_call(
                expect,
                &[condition.into(), bool_ty.const_all_ones().into()],
                name,
            )
            .expect("failed to build llvm.expect")
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value()
    }

    /// `return value`, from wherever code generation is: every open scope
    /// ends first, as for a `return` expression.
    fn gen_early_return(&mut self, value: BasicValueEnum<'ink>) {
        self.gen_end_scopes(0);
        self.builder
            .build_return(Some(&value))
            .expect("failed to build return");
    }
}
