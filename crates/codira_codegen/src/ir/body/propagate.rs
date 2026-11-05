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
//!   br i1 %ok, label %propagate.ok, label %propagate.err
//! propagate.err:                      ; build the caller's Err, return it
//!   %e   = extractvalue %Result %r, <Err slot>
//!   ret %Ret { <Err>, .., %e, .. }
//! propagate.ok:
//!   %v   = extractvalue %Result %r, <Ok slot>
//! ```
//!
//! No allocation and no call. The early return goes through the same steps
//! as `return`: open `scope`s end first, and a hardware-safety mode undoes
//! its setup before every `ret` (see `safety`). `x?` on an optional is the
//! same with the present flag, returning the all-zero `nil`.

use codira_hir::{mono, ExprId, TyKind};
use inkwell::{
    values::{AggregateValueEnum, BasicValueEnum},
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

    /// `return value`, from wherever code generation is: every open scope
    /// ends first, as for a `return` expression.
    fn gen_early_return(&mut self, value: BasicValueEnum<'ink>) {
        self.gen_end_scopes(0);
        self.builder
            .build_return(Some(&value))
            .expect("failed to build return");
    }
}
