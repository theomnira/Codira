//! Copyright (c) 2026 Omnira CJSC
//!
//! Bridges a self-contained `comptime { .. }` block's already-lowered HIR
//! into `codira_mir`/`codira_comptime` for real compile-time evaluation.
//!
//! Scope (see `spec/KGEN_SUPERSET_STATUS.md`): only single-tail-expression
//! blocks (no `let`-statements, no expression-statements) built from
//! int/bool/float literals, arithmetic/comparison/logical/bitwise/shift
//! operators, unary neg/not, and `if`/`else` fold today. Anything
//! referencing a name (`Path`), calling a function, indexing, looping, or
//! containing a statement bails out (`None`) and the caller leaves the
//! block to lower exactly as it did before this module existed -- an
//! honest, additive-only improvement: nothing that used to work stops
//! working, and a real (if narrow) subset of `comptime` now actually
//! evaluates at compile time instead of always running at ordinary
//! runtime. (Calls *are* liftable in the more general `mir_lower` pass,
//! which has interprocedural context; a bare `comptime` block evaluated
//! here has no `GeneratorStore` to resolve them against.)

use la_arena::Arena;

use crate::expr::{
    ArithOp, BinaryOp, CmpOp, Expr, ExprId, Literal, LiteralFloat, LiteralFloatKind, LiteralInt,
    LiteralIntKind, LogicOp, Ordering, UnaryOp,
};

/// Attempts to evaluate `block` (already-lowered HIR) at compile time.
/// `None` means "not foldable with today's restricted subset," not an
/// error -- callers should fall back to leaving the block unevaluated.
pub(crate) fn try_eval(exprs: &Arena<Expr>, block: ExprId) -> Option<codira_comptime::Value> {
    let mut body = codira_mir::Body::new();
    build_op(exprs, block, &mut body)?;
    codira_comptime::eval_body(&body, &codira_comptime::Env::new()).ok()
}

/// Converts a concrete evaluation result back into a source `Expr`,
/// allocating any synthetic sub-expressions (the literal inside a negative
/// result's `UnaryOp::Neg` wrapper) into `exprs`. Returns `None` for:
///
/// * `Value::Unit` -- there is no `Expr::Literal` variant that safely stands in
///   for "unit" without risking a type mismatch against the block's inferred
///   type, and it's a low-value case to special-case further today;
/// * `Value::Str`/`Value::Tuple` -- the restricted `build_op` walker below
///   never produces string or tuple values, so a result of that shape means
///   something upstream changed; bailing (leaving the block unevaluated) is the
///   honest move rather than guessing at an `Expr` encoding this module has
///   never emitted;
/// * non-finite `Value::Float` results -- there is no source-level float
///   literal spelling for `inf`/`NaN` to fold back into.
pub(crate) fn value_to_expr(
    exprs: &mut Arena<Expr>,
    value: codira_comptime::Value,
) -> Option<Expr> {
    match value {
        codira_comptime::Value::Bool(b) => Some(Expr::Literal(Literal::Bool(b))),
        codira_comptime::Value::Int(v) if v >= 0 => Some(Expr::Literal(Literal::Int(LiteralInt {
            kind: LiteralIntKind::Unsuffixed,
            value: v as u128,
        }))),
        codira_comptime::Value::Int(v) => {
            let inner = exprs.alloc(Expr::Literal(Literal::Int(LiteralInt {
                kind: LiteralIntKind::Unsuffixed,
                value: u128::from(v.unsigned_abs()),
            })));
            Some(Expr::UnaryOp {
                expr: inner,
                op: UnaryOp::Neg,
            })
        }
        codira_comptime::Value::Float(f) if !f.is_finite() => None,
        // Mirror the integer convention: non-negative results become a
        // bare literal, negative ones (including -0.0, whose sign is
        // observable) wrap `abs` in `UnaryOp::Neg` -- float literals in
        // source are unsigned, negation is an operator.
        codira_comptime::Value::Float(f) if !f.is_sign_negative() => {
            Some(Expr::Literal(Literal::Float(LiteralFloat {
                kind: LiteralFloatKind::Unsuffixed,
                value: f,
            })))
        }
        codira_comptime::Value::Float(f) => {
            let inner = exprs.alloc(Expr::Literal(Literal::Float(LiteralFloat {
                kind: LiteralFloatKind::Unsuffixed,
                value: -f,
            })));
            Some(Expr::UnaryOp {
                expr: inner,
                op: UnaryOp::Neg,
            })
        }
        codira_comptime::Value::Str(_)
        | codira_comptime::Value::Tuple(_)
        | codira_comptime::Value::Unit => None,
    }
}

fn build_op(
    exprs: &Arena<Expr>,
    id: ExprId,
    body: &mut codira_mir::Body,
) -> Option<codira_mir::OpId> {
    use codira_mir::{Attr, OpKind, Region};

    match &exprs[id] {
        Expr::Literal(Literal::Bool(b)) => Some(body.push(OpKind::Const(Attr::Bool(*b)), [])),
        Expr::Literal(Literal::Int(LiteralInt { value, .. })) => {
            let v = i64::try_from(*value).ok()?;
            Some(body.push(OpKind::Const(Attr::Int(v)), []))
        }
        Expr::Literal(Literal::Float(LiteralFloat { value, .. })) => {
            // HIR float literals store the parsed `f64` directly, which is
            // exactly what `Attr::float` wants (it keeps the bit pattern).
            Some(body.push(OpKind::Const(Attr::float(*value)), []))
        }
        Expr::UnaryOp { expr, op } => {
            let inner = build_op(exprs, *expr, body)?;
            let kind = match op {
                UnaryOp::Neg => OpKind::Neg,
                UnaryOp::Not => OpKind::Not,
                UnaryOp::BitNot => OpKind::BitNot,
            };
            Some(body.push(kind, [inner]))
        }
        Expr::BinaryOp {
            lhs,
            rhs,
            op: Some(op),
        } => {
            let lhs_id = build_op(exprs, *lhs, body)?;
            let rhs_id = build_op(exprs, *rhs, body)?;
            let kind = binary_op_kind(*op)?;
            Some(body.push(kind, [lhs_id, rhs_id]))
        }
        Expr::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let cond_id = build_op(exprs, *condition, body)?;

            let mut then_body = codira_mir::Body::new();
            build_op(exprs, *then_branch, &mut then_body)?;

            let mut else_body = codira_mir::Body::new();
            if let Some(else_branch) = else_branch {
                build_op(exprs, *else_branch, &mut else_body)?;
            }

            Some(body.push_with_regions(
                OpKind::If,
                [cond_id],
                [Region::new(then_body), Region::new(else_body)],
            ))
        }
        Expr::Block { statements, tail } if statements.is_empty() => {
            build_op(exprs, (*tail)?, body)
        }
        // `Path`, `Call`, `MethodCall`, `Index`, `Array`, `RecordLit`,
        // `Field`, `Loop`, `While`, `Return`, `Break`, blocks with
        // statements, `Missing`, and string literals are all out of scope
        // for this restricted walker -- see module doc and
        // spec/KGEN_SUPERSET_STATUS.md.
        _ => None,
    }
}

fn binary_op_kind(op: BinaryOp) -> Option<codira_mir::OpKind> {
    use codira_mir::OpKind;
    Some(match op {
        BinaryOp::ArithOp(ArithOp::Add) => OpKind::Add,
        BinaryOp::ArithOp(ArithOp::Subtract) => OpKind::Sub,
        BinaryOp::ArithOp(ArithOp::Multiply) => OpKind::Mul,
        BinaryOp::ArithOp(ArithOp::Divide) => OpKind::Div,
        BinaryOp::ArithOp(ArithOp::Remainder) => OpKind::Rem,
        BinaryOp::ArithOp(ArithOp::LeftShift) => OpKind::Shl,
        BinaryOp::ArithOp(ArithOp::RightShift) => OpKind::Shr,
        BinaryOp::ArithOp(ArithOp::BitAnd) => OpKind::BitAnd,
        BinaryOp::ArithOp(ArithOp::BitOr) => OpKind::BitOr,
        BinaryOp::ArithOp(ArithOp::BitXor) => OpKind::BitXor,
        BinaryOp::LogicOp(LogicOp::And) => OpKind::And,
        BinaryOp::LogicOp(LogicOp::Or) => OpKind::Or,
        BinaryOp::CmpOp(CmpOp::Eq { negated: false }) => OpKind::Eq,
        BinaryOp::CmpOp(CmpOp::Eq { negated: true }) => OpKind::Ne,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Less,
            strict: true,
        }) => OpKind::Lt,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Less,
            strict: false,
        }) => OpKind::Le,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Greater,
            strict: true,
        }) => OpKind::Gt,
        BinaryOp::CmpOp(CmpOp::Ord {
            ordering: Ordering::Greater,
            strict: false,
        }) => OpKind::Ge,
        BinaryOp::Assignment { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use la_arena::Arena;

    use super::*;
    use crate::expr::{Literal, LiteralInt, LiteralIntKind};

    #[test]
    fn folds_two_plus_two() {
        let mut exprs = Arena::new();
        let two_a = exprs.alloc(Expr::Literal(Literal::Int(LiteralInt {
            kind: LiteralIntKind::Unsuffixed,
            value: 2,
        })));
        let two_b = exprs.alloc(Expr::Literal(Literal::Int(LiteralInt {
            kind: LiteralIntKind::Unsuffixed,
            value: 2,
        })));
        let sum = exprs.alloc(Expr::BinaryOp {
            lhs: two_a,
            rhs: two_b,
            op: Some(BinaryOp::ArithOp(ArithOp::Add)),
        });
        let block = exprs.alloc(Expr::Block {
            statements: Vec::new(),
            tail: Some(sum),
        });

        let value = try_eval(&exprs, block).unwrap();
        assert_eq!(value, codira_comptime::Value::Int(4));

        let folded = value_to_expr(&mut exprs, value).unwrap();
        assert_eq!(
            folded,
            Expr::Literal(Literal::Int(LiteralInt {
                kind: LiteralIntKind::Unsuffixed,
                value: 4,
            }))
        );
    }

    #[test]
    fn negative_result_wraps_in_unary_neg() {
        let mut exprs = Arena::new();
        let zero = exprs.alloc(Expr::Literal(Literal::Int(LiteralInt {
            kind: LiteralIntKind::Unsuffixed,
            value: 0,
        })));
        let five = exprs.alloc(Expr::Literal(Literal::Int(LiteralInt {
            kind: LiteralIntKind::Unsuffixed,
            value: 5,
        })));
        let diff = exprs.alloc(Expr::BinaryOp {
            lhs: zero,
            rhs: five,
            op: Some(BinaryOp::ArithOp(ArithOp::Subtract)),
        });

        let value = try_eval(&exprs, diff).unwrap();
        assert_eq!(value, codira_comptime::Value::Int(-5));

        let folded = value_to_expr(&mut exprs, value).unwrap();
        match folded {
            Expr::UnaryOp {
                expr,
                op: UnaryOp::Neg,
            } => {
                assert_eq!(
                    exprs[expr],
                    Expr::Literal(Literal::Int(LiteralInt {
                        kind: LiteralIntKind::Unsuffixed,
                        value: 5,
                    }))
                );
            }
            other => panic!("expected UnaryOp::Neg, got {other:?}"),
        }
    }

    #[test]
    fn folds_bitwise_and_shift_ops() {
        // `(1 << 4) | 3` -> 19: the bitwise/shift operators now map to
        // real codira_mir ops instead of bailing.
        let mut exprs = Arena::new();
        let one = exprs.alloc(Expr::Literal(Literal::Int(LiteralInt {
            kind: LiteralIntKind::Unsuffixed,
            value: 1,
        })));
        let four = exprs.alloc(Expr::Literal(Literal::Int(LiteralInt {
            kind: LiteralIntKind::Unsuffixed,
            value: 4,
        })));
        let shifted = exprs.alloc(Expr::BinaryOp {
            lhs: one,
            rhs: four,
            op: Some(BinaryOp::ArithOp(ArithOp::LeftShift)),
        });
        let three = exprs.alloc(Expr::Literal(Literal::Int(LiteralInt {
            kind: LiteralIntKind::Unsuffixed,
            value: 3,
        })));
        let ored = exprs.alloc(Expr::BinaryOp {
            lhs: shifted,
            rhs: three,
            op: Some(BinaryOp::ArithOp(ArithOp::BitOr)),
        });

        let value = try_eval(&exprs, ored).unwrap();
        assert_eq!(value, codira_comptime::Value::Int(19));
    }

    #[test]
    fn folds_float_arithmetic_back_to_a_float_literal() {
        // `1.5 + 2.25` -> the float literal `3.75` (exact in binary
        // floating point, so the round-trip is loss-free).
        let mut exprs = Arena::new();
        let a = exprs.alloc(Expr::Literal(Literal::Float(LiteralFloat {
            kind: LiteralFloatKind::Unsuffixed,
            value: 1.5,
        })));
        let b = exprs.alloc(Expr::Literal(Literal::Float(LiteralFloat {
            kind: LiteralFloatKind::Unsuffixed,
            value: 2.25,
        })));
        let sum = exprs.alloc(Expr::BinaryOp {
            lhs: a,
            rhs: b,
            op: Some(BinaryOp::ArithOp(ArithOp::Add)),
        });

        let value = try_eval(&exprs, sum).unwrap();
        assert_eq!(value, codira_comptime::Value::Float(3.75));

        let folded = value_to_expr(&mut exprs, value).unwrap();
        assert_eq!(
            folded,
            Expr::Literal(Literal::Float(LiteralFloat {
                kind: LiteralFloatKind::Unsuffixed,
                value: 3.75,
            }))
        );
    }

    #[test]
    fn negative_float_result_wraps_in_unary_neg() {
        // `1.5 - 2.0` -> `-(0.5)`: float literals in source are unsigned,
        // so a negative fold result becomes Neg(abs), same as ints.
        let mut exprs = Arena::new();
        let a = exprs.alloc(Expr::Literal(Literal::Float(LiteralFloat {
            kind: LiteralFloatKind::Unsuffixed,
            value: 1.5,
        })));
        let b = exprs.alloc(Expr::Literal(Literal::Float(LiteralFloat {
            kind: LiteralFloatKind::Unsuffixed,
            value: 2.0,
        })));
        let diff = exprs.alloc(Expr::BinaryOp {
            lhs: a,
            rhs: b,
            op: Some(BinaryOp::ArithOp(ArithOp::Subtract)),
        });

        let value = try_eval(&exprs, diff).unwrap();
        assert_eq!(value, codira_comptime::Value::Float(-0.5));

        match value_to_expr(&mut exprs, value).unwrap() {
            Expr::UnaryOp {
                expr,
                op: UnaryOp::Neg,
            } => {
                assert_eq!(
                    exprs[expr],
                    Expr::Literal(Literal::Float(LiteralFloat {
                        kind: LiteralFloatKind::Unsuffixed,
                        value: 0.5,
                    }))
                );
            }
            other => panic!("expected UnaryOp::Neg, got {other:?}"),
        }
    }

    #[test]
    fn non_finite_float_results_do_not_fold() {
        // `1.0 / 0.0` is IEEE infinity -- evaluation succeeds (fold_op's
        // documented float semantics), but there is no literal spelling
        // for it, so value_to_expr must bail rather than invent one.
        let mut exprs = Arena::new();
        let one = exprs.alloc(Expr::Literal(Literal::Float(LiteralFloat {
            kind: LiteralFloatKind::Unsuffixed,
            value: 1.0,
        })));
        let zero = exprs.alloc(Expr::Literal(Literal::Float(LiteralFloat {
            kind: LiteralFloatKind::Unsuffixed,
            value: 0.0,
        })));
        let div = exprs.alloc(Expr::BinaryOp {
            lhs: one,
            rhs: zero,
            op: Some(BinaryOp::ArithOp(ArithOp::Divide)),
        });

        let value = try_eval(&exprs, div).unwrap();
        assert_eq!(value, codira_comptime::Value::Float(f64::INFINITY));
        assert!(value_to_expr(&mut exprs, value).is_none());
    }

    #[test]
    fn bails_out_on_unsupported_construct() {
        // `Missing` stands in for anything this restricted interpreter
        // doesn't handle (name references, calls, etc. -- see module doc);
        // the important property under test is that an unsupported node
        // anywhere in the tree causes a clean `None`, not a panic or a
        // wrong answer.
        let mut exprs = Arena::new();
        let unsupported = exprs.alloc(Expr::Missing);
        assert!(try_eval(&exprs, unsupported).is_none());
    }
}
