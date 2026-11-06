//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - `str` and `String` in JIT-compiled code: literals, the conversion of a
//!   `str` to a `String`, printing, `+`, `+=`, the comparisons, `to_string()`
//!   of numbers, `bool`s and text, and the builtin methods that read a
//!   string or make a new one (`len`, `is_empty`, `char_count`, `hash`,
//!   `contains`, `starts_with`, `ends_with`, `to_upper`, `to_lower`,
//!   `replace`, `repeat`).
//!
//! As on the LLVM backend (`codira_codegen`'s `ir::body::strings`), a `str` is
//! `{ data, len }` and a `String` `{ buf, start, len }`, `buf` the handle of a
//! `[u8]` array; the text of a `String` starts `start` bytes into the
//! buffer's elements. Everything but reading the words is a call to the
//! runtime's `codira.string_*` functions, with the same arguments in the same
//! order. A function that makes a `String` writes it to a slot in the
//! caller's frame (`out`), where the conservative collector sees the new
//! buffer's handle, and the value is read back from the slot.

use codira_abi::runtime_functions::{self as rt, RuntimeFunction};
use codira_hir::{ArithOp, BinaryOp, CmpOp, ExprId, IntTy, Ordering, StringOp, Ty, TyKind};
use cranelift_codegen::ir::{
    condcodes::IntCC, types, InstBuilder, MemFlagsData, StackSlotData, StackSlotKind, Value,
};

use crate::{
    translate::{Lowered, Translator},
    ScalarTy,
};

impl Translator<'_, '_> {
    /// The data address and byte length of `value`, a `String` or a `str` as
    /// `ty` says.
    pub(crate) fn text_of(
        &mut self,
        ty: &Ty,
        value: &[Value],
    ) -> Result<(Value, Value), crate::Unsupported> {
        match (ty.interned(), value) {
            (TyKind::String, &[buf, start, len]) => {
                let (first, _) = codira_hir::layout::array_elements(
                    self.state.db,
                    &TyKind::Int(IntTy::u8()).intern(),
                )
                .ok_or_else(|| ())
                .or_else(|()| self.unsupported("a string buffer without a layout"))?;
                let payload =
                    self.builder
                        .ins()
                        .load(self.state.pointer, MemFlagsData::trusted(), buf, 0);
                let elements = self
                    .builder
                    .ins()
                    .iadd_imm_s(payload, i64::try_from(first).unwrap_or(i64::MAX));
                Ok((self.builder.ins().iadd(elements, start), len))
            }
            (TyKind::Str, &[data, len]) => Ok((data, len)),
            _ => self.unsupported("text of an unexpected shape"),
        }
    }

    /// A `str` literal: the address and length of its bytes.
    pub(crate) fn str_literal(&mut self, text: &str) -> Lowered {
        let (address, len) = self.string_value(text)?;
        Ok(vec![address, len])
    }

    /// Calls `function` with `args`, then the `[u8]` type handle, the
    /// allocator and a slot, and returns the `String` it wrote there.
    pub(crate) fn string_making_call(
        &mut self,
        function: &'static RuntimeFunction,
        args: &[Value],
    ) -> Lowered {
        let pointer = self.state.pointer;
        let string_ty = TyKind::String.intern();
        let layout = codira_hir::layout::string_layout(&self.state.layout);
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            u32::try_from(layout.size).unwrap_or(24),
            u8::try_from(layout.align.trailing_zeros()).unwrap_or(3),
        ));
        let out = self.builder.ins().stack_addr(pointer, slot, 0);
        let byte_array = TyKind::Array(TyKind::Int(IntTy::u8()).intern()).intern();
        let ty = self.type_handle(&byte_array)?;
        let alloc = self.allocator();
        let mut all = args.to_vec();
        all.extend([ty, alloc, out]);
        self.call_runtime(function, &all)?;
        self.load_value(&string_ty, out, 0)
    }

    /// A `String` copy of `value`, a `str`.
    pub(crate) fn string_from_str(&mut self, value: &[Value]) -> Lowered {
        let str_ty = TyKind::Str.intern();
        let (data, len) = self.text_of(&str_ty, value)?;
        self.string_making_call(&rt::STRING_FROM_BYTES, &[data, len])
    }

    /// `x.to_string()` for an `x` of type `ty`.
    pub(crate) fn to_string_of(&mut self, ty: &Ty, value: &[Value]) -> Lowered {
        match ty.interned() {
            // Immutable, so the value itself is its own copy.
            TyKind::String => Ok(value.to_vec()),
            TyKind::Str => self.string_from_str(value),
            _ => {
                let Some(scalar) = self.scalar_of(ty)? else {
                    return self.unsupported("`to_string` of unit");
                };
                let value = match value {
                    [value] => *value,
                    _ => return self.unsupported("`to_string` of an unexpected shape"),
                };
                match scalar {
                    ScalarTy::Int { bits, signed } => {
                        let wide = match (bits, signed) {
                            (64, _) => value,
                            (_, true) => self.builder.ins().sextend(types::I64, value),
                            (_, false) => self.builder.ins().uextend(types::I64, value),
                        };
                        let function = if signed {
                            &rt::STRING_FROM_I64
                        } else {
                            &rt::STRING_FROM_U64
                        };
                        self.string_making_call(function, &[wide])
                    }
                    ScalarTy::Float { bits: 32 } => {
                        self.string_making_call(&rt::STRING_FROM_F32, &[value])
                    }
                    ScalarTy::Float { .. } => {
                        self.string_making_call(&rt::STRING_FROM_F64, &[value])
                    }
                    ScalarTy::Bool => self.string_making_call(&rt::STRING_FROM_BOOL, &[value]),
                }
            }
        }
    }

    /// Prints `value`, a `String` or a `str`.
    pub(crate) fn print_text_value(
        &mut self,
        ty: &Ty,
        value: &[Value],
    ) -> Result<(), crate::Unsupported> {
        let (data, len) = self.text_of(ty, value)?;
        self.call_runtime(&rt::PRINT_STR, &[data, len])?;
        Ok(())
    }

    /// `current + text` for a `String` `current`: the runtime's
    /// `string_append`, which appends in place when it can.
    fn append(&mut self, current: &[Value], text_ty: &Ty, text: &[Value]) -> Lowered {
        let &[buf, start, len] = current else {
            return self.unsupported("a `String` of an unexpected shape");
        };
        let (data, data_len) = self.text_of(text_ty, text)?;
        self.string_making_call(&rt::STRING_APPEND, &[buf, start, len, data, data_len])
    }

    /// `+`, `+=` and the comparisons on `String`s and `str`s.
    pub(crate) fn string_binary(&mut self, lhs: ExprId, rhs: ExprId, op: BinaryOp) -> Lowered {
        let lhs_ty = self.value_ty(lhs);
        let rhs_ty = self.value_ty(rhs);
        match op {
            BinaryOp::Assignment {
                op: Some(ArithOp::Add),
            } => {
                let rhs_value = self.operand_vals(rhs)?;
                self.update_place(lhs, |this, current| {
                    this.append(&current, &rhs_ty, &rhs_value)
                })
            }
            BinaryOp::ArithOp(ArithOp::Add) => {
                let lhs_value = self.operand_vals(lhs)?;
                let rhs_value = self.operand_vals(rhs)?;
                if matches!(lhs_ty.interned(), TyKind::String) {
                    self.append(&lhs_value, &rhs_ty, &rhs_value)
                } else {
                    let (a, a_len) = self.text_of(&lhs_ty, &lhs_value)?;
                    let (b, b_len) = self.text_of(&rhs_ty, &rhs_value)?;
                    self.string_making_call(&rt::STRING_CONCAT, &[a, a_len, b, b_len])
                }
            }
            BinaryOp::CmpOp(cmp) => {
                let lhs_value = self.operand_vals(lhs)?;
                let rhs_value = self.operand_vals(rhs)?;
                let (a, a_len) = self.text_of(&lhs_ty, &lhs_value)?;
                let (b, b_len) = self.text_of(&rhs_ty, &rhs_value)?;
                let args = [a, a_len, b, b_len];
                Ok(vec![match cmp {
                    CmpOp::Eq { negated } => {
                        let Some(equal) = self.call_runtime(&rt::STRING_EQ, &args)? else {
                            return self.unsupported("`string_eq` without a result");
                        };
                        let cc = if negated {
                            IntCC::Equal
                        } else {
                            IntCC::NotEqual
                        };
                        self.builder.ins().icmp_imm_u(cc, equal, 0)
                    }
                    CmpOp::Ord { ordering, strict } => {
                        let Some(order) = self.call_runtime(&rt::STRING_CMP, &args)? else {
                            return self.unsupported("`string_cmp` without a result");
                        };
                        let cc = match (ordering, strict) {
                            (Ordering::Less, true) => IntCC::SignedLessThan,
                            (Ordering::Less, false) => IntCC::SignedLessThanOrEqual,
                            (Ordering::Greater, true) => IntCC::SignedGreaterThan,
                            (Ordering::Greater, false) => IntCC::SignedGreaterThanOrEqual,
                        };
                        self.builder.ins().icmp_imm_u(cc, order, 0)
                    }
                }])
            }
            _ => self.unsupported("this operator on strings"),
        }
    }

    /// A builtin method of a `String` or a `str`, or `to_string()`.
    pub(crate) fn string_method(
        &mut self,
        op: StringOp,
        receiver: ExprId,
        args: &[ExprId],
    ) -> Lowered {
        let receiver_ty = self.value_ty(receiver);
        let receiver_value = self.operand_vals(receiver)?;
        let mut arg_values = Vec::with_capacity(args.len());
        for &arg in args {
            arg_values.push(self.operand_vals(arg)?);
        }
        if op == StringOp::ToString {
            return self.to_string_of(&receiver_ty, &receiver_value);
        }
        // A method called on a `str` works on a `String` copy of it.
        let receiver_value = if matches!(receiver_ty.interned(), TyKind::Str) {
            self.to_string_of(&receiver_ty, &receiver_value)?
        } else if matches!(receiver_ty.interned(), TyKind::String) {
            receiver_value
        } else {
            return self.unsupported(format!("the string method `{}`", op.name()));
        };
        let string_ty = TyKind::String.intern();
        let (data, len) = self.text_of(&string_ty, &receiver_value)?;
        let arg_ty = |this: &Self, index: usize| this.value_ty(args[index]);
        match op {
            StringOp::Len => Ok(vec![len]),
            StringOp::IsEmpty => Ok(vec![self.builder.ins().icmp_imm_u(IntCC::Equal, len, 0)]),
            StringOp::CharCount | StringOp::Hash => {
                let function = if op == StringOp::CharCount {
                    &rt::STRING_CHAR_COUNT
                } else {
                    &rt::STRING_HASH
                };
                let Some(result) = self.call_runtime(function, &[data, len])? else {
                    return self.unsupported("a string function without a result");
                };
                Ok(vec![result])
            }
            StringOp::Contains | StringOp::StartsWith | StringOp::EndsWith => {
                let ty = arg_ty(self, 0);
                let (other, other_len) = self.text_of(&ty, &arg_values[0])?;
                let function = match op {
                    StringOp::Contains => &rt::STRING_CONTAINS,
                    StringOp::StartsWith => &rt::STRING_STARTS_WITH,
                    _ => &rt::STRING_ENDS_WITH,
                };
                let Some(byte) = self.call_runtime(function, &[data, len, other, other_len])?
                else {
                    return self.unsupported("a string function without a result");
                };
                Ok(vec![self.builder.ins().icmp_imm_u(
                    IntCC::NotEqual,
                    byte,
                    0,
                )])
            }
            StringOp::ToUpper | StringOp::ToLower => {
                let upper = self
                    .builder
                    .ins()
                    .iconst(types::I8, i64::from(op == StringOp::ToUpper));
                self.string_making_call(&rt::STRING_CHANGE_CASE, &[data, len, upper])
            }
            StringOp::Replace => {
                let (from_ty, to_ty) = (arg_ty(self, 0), arg_ty(self, 1));
                let (from, from_len) = self.text_of(&from_ty, &arg_values[0])?;
                let (to, to_len) = self.text_of(&to_ty, &arg_values[1])?;
                self.string_making_call(
                    &rt::STRING_REPLACE,
                    &[data, len, from, from_len, to, to_len],
                )
            }
            StringOp::Repeat => {
                let count = match arg_values[0].as_slice() {
                    [count] => *count,
                    _ => return self.unsupported("a count of an unexpected shape"),
                };
                let Some(ScalarTy::Int { bits, signed }) = self.scalar_of(&arg_ty(self, 0))? else {
                    return self.unsupported("a count that is not an integer");
                };
                let wide = match (bits, signed) {
                    (64, _) => count,
                    (_, true) => self.builder.ins().sextend(types::I64, count),
                    (_, false) => self.builder.ins().uextend(types::I64, count),
                };
                let signed = self.builder.ins().iconst(types::I8, i64::from(signed));
                self.string_making_call(&rt::STRING_REPEAT, &[data, len, wide, signed])
            }
            other => self.unsupported(format!("the string method `{}`", other.name())),
        }
    }
}
