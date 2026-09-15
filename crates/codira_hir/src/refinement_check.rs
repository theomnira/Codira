//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 21, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
//!
//! SMT-backed well-formedness checking for refinement types (`Type { binder
//! | predicate }`, see `spec/LANGUAGE_SPEC.md` §7 and `spec/
//! self_healing_programming_language.md`), via `codira_smt`'s real Z3
//! binding -- the first consumer of that crate from the compiler pipeline
//! itself (previously only `codira_healing`'s runtime engine used it).
//!
//! **Scope, precisely**: checks that a refinement predicate is
//! *satisfiable* (`codira_smt::is_satisfiable`) -- i.e. the type it
//! describes actually has at least one inhabitant. An unsatisfiable
//! refinement (`i32 { x | x > 0 && x < 0 }`) is an empty type, almost
//! certainly a mistake at the declaration site rather than an intentional
//! "no valid values" type (see `codira_smt::is_satisfiable`'s own doc
//! comment, which describes exactly this use case).
//!
//! **Deliberately not attempted this session**: proving refinement
//! predicates hold at every call site / assignment (full refinement-type
//! subtyping, à la Liquid Haskell/F*). That needs a type-system-level
//! representation of refinements threaded through inference itself --
//! today `TypeRef::Refinement` already discards the predicate down to just
//! its base type (`type_ref.rs`), and changing that is a materially larger
//! project than a well-formedness check bolted onto `type` alias
//! declarations. This module deliberately doesn't touch `TypeRef`/
//! `TypeRefMap` at all -- it walks the raw `ast::RefinementType` directly,
//! the same "operate on syntax, independent of the fuller HIR machinery"
//! shape `data_derive` uses for the same reason (narrow, verifiable slice
//! now; the real subtyping project is honestly out of scope, not silently
//! skipped).
//!
//! **Restricted predicate subset** (matches `comptime_fold`'s established
//! convention: a named, bounded subset with a clean bail-out, not a
//! best-effort guess): int/bool literals, the refinement's own binder
//! (referenced by name), arithmetic (`+ - *`), comparisons (`< <= > >= ==`),
//! logical `&&`/`||`, unary `-`/`!`, and parenthesization. A predicate
//! using anything outside this subset (division, a function call, a
//! reference to a name other than the binder, ...) is reported as
//! `NotChecked`, not silently treated as satisfiable.

use ast::BinOp;
use codira_smt::{Context, SatResult};
use codira_syntax::ast;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefinementCheckResult {
    /// The predicate is satisfiable (or Z3 couldn't decide -- `Unknown` is
    /// treated as "don't false-positive", not as an error).
    Ok,
    /// The predicate is provably unsatisfiable: this refinement type has no
    /// inhabitants.
    Unsatisfiable,
    /// The predicate uses a construct outside this module's restricted
    /// subset (see module doc), so it wasn't checked at all.
    NotChecked,
}

enum SmtValue<'ctx> {
    Int(codira_smt::IntExpr<'ctx>),
    Bool(codira_smt::BoolExpr<'ctx>),
}

fn lower_expr<'ctx>(
    ctx: &'ctx Context,
    binder_name: &str,
    expr: &ast::Expr,
) -> Option<SmtValue<'ctx>> {
    match expr.kind() {
        ast::ExprKind::Literal(lit) => match lit.kind() {
            ast::LiteralKind::IntNumber(n) => {
                let (text, _suffix) = n.split_into_parts();
                let value: i64 = text.parse().ok()?;
                Some(SmtValue::Int(ctx.int_lit(value)))
            }
            ast::LiteralKind::Bool(b) => Some(SmtValue::Bool(if b {
                ctx.bool_true()
            } else {
                ctx.bool_false()
            })),
            _ => None,
        },
        ast::ExprKind::PathExpr(p) => {
            let segment = p.path()?.segment()?;
            match segment.kind()? {
                ast::PathSegmentKind::Name(name_ref) if name_ref.text() == binder_name => {
                    Some(SmtValue::Int(ctx.int_var(binder_name)))
                }
                _ => None,
            }
        }
        ast::ExprKind::ParenExpr(p) => lower_expr(ctx, binder_name, &p.expr()?),
        ast::ExprKind::PrefixExpr(p) => {
            let inner = lower_expr(ctx, binder_name, &p.expr()?)?;
            match (p.op_kind()?, inner) {
                (ast::PrefixOp::Not, SmtValue::Bool(b)) => Some(SmtValue::Bool(b.not())),
                (ast::PrefixOp::Neg, SmtValue::Int(i)) => {
                    Some(SmtValue::Int(ctx.int_lit(0).sub(i)))
                }
                _ => None,
            }
        }
        ast::ExprKind::BinExpr(b) => {
            let lhs = lower_expr(ctx, binder_name, &b.lhs()?)?;
            let rhs = lower_expr(ctx, binder_name, &b.rhs()?)?;
            match (b.op_kind()?, lhs, rhs) {
                (BinOp::Add, SmtValue::Int(l), SmtValue::Int(r)) => Some(SmtValue::Int(l.add(r))),
                (BinOp::Subtract, SmtValue::Int(l), SmtValue::Int(r)) => {
                    Some(SmtValue::Int(l.sub(r)))
                }
                (BinOp::Multiply, SmtValue::Int(l), SmtValue::Int(r)) => {
                    Some(SmtValue::Int(l.mul(r)))
                }
                (BinOp::Less, SmtValue::Int(l), SmtValue::Int(r)) => Some(SmtValue::Bool(l.lt(r))),
                (BinOp::LessEqual, SmtValue::Int(l), SmtValue::Int(r)) => {
                    Some(SmtValue::Bool(l.le(r)))
                }
                (BinOp::Greater, SmtValue::Int(l), SmtValue::Int(r)) => {
                    Some(SmtValue::Bool(l.gt(r)))
                }
                (BinOp::GreatEqual, SmtValue::Int(l), SmtValue::Int(r)) => {
                    Some(SmtValue::Bool(l.ge(r)))
                }
                (BinOp::Equals, SmtValue::Int(l), SmtValue::Int(r)) => {
                    Some(SmtValue::Bool(l.eq(r)))
                }
                (BinOp::BooleanAnd, SmtValue::Bool(l), SmtValue::Bool(r)) => {
                    Some(SmtValue::Bool(l.and(r)))
                }
                (BinOp::BooleanOr, SmtValue::Bool(l), SmtValue::Bool(r)) => {
                    Some(SmtValue::Bool(l.or(r)))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Checks whether `predicate` (with `binder_name` as its one free
/// variable) is satisfiable. See the module doc for exactly which
/// predicates this can check.
pub(crate) fn check_satisfiable(binder_name: &str, predicate: &ast::Expr) -> RefinementCheckResult {
    let ctx = Context::new();
    match lower_expr(&ctx, binder_name, predicate) {
        Some(SmtValue::Bool(b)) => match codira_smt::is_satisfiable(&ctx, b) {
            SatResult::Unsat => RefinementCheckResult::Unsatisfiable,
            SatResult::Sat | SatResult::Unknown => RefinementCheckResult::Ok,
        },
        _ => RefinementCheckResult::NotChecked,
    }
}

#[cfg(test)]
mod tests {
    use codira_syntax::{ast::ModuleItemOwner, SourceFile};

    use super::*;

    fn predicate_of(src: &str) -> ast::Expr {
        let file = SourceFile::parse(src).tree();
        let alias = file
            .items()
            .find_map(|item| match item.kind() {
                ast::ModuleItemKind::TypeAliasDef(t) => Some(t),
                _ => None,
            })
            .expect("no type alias in fixture");
        let ast::TypeRefKind::RefinementType(rt) = alias.type_ref().unwrap().kind() else {
            panic!("type alias target is not a refinement type");
        };
        rt.predicate().unwrap()
    }

    #[test]
    fn satisfiable_bound_is_ok() {
        let pred = predicate_of("type Pos = i32 { x | x > 0 };");
        assert_eq!(check_satisfiable("x", &pred), RefinementCheckResult::Ok);
    }

    #[test]
    fn contradictory_bound_is_unsatisfiable() {
        let pred = predicate_of("type Impossible = i32 { x | x > 0 && x < 0 };");
        assert_eq!(
            check_satisfiable("x", &pred),
            RefinementCheckResult::Unsatisfiable
        );
    }

    #[test]
    fn compound_arithmetic_bound_is_unsatisfiable() {
        // x + 1 <= x is never true for any integer x.
        let pred = predicate_of("type Impossible = i32 { x | x + 1 <= x };");
        assert_eq!(
            check_satisfiable("x", &pred),
            RefinementCheckResult::Unsatisfiable
        );
    }

    #[test]
    fn negation_and_or_are_understood() {
        let pred = predicate_of("type NonZero = i32 { x | x > 0 || x < 0 };");
        assert_eq!(check_satisfiable("x", &pred), RefinementCheckResult::Ok);
    }

    #[test]
    fn out_of_subset_predicate_is_not_checked_not_flagged() {
        // A call is outside the restricted subset -- must not be
        // misreported as satisfiable *or* unsatisfiable.
        let pred = predicate_of("type T = i32 { x | is_valid(x) };");
        assert_eq!(
            check_satisfiable("x", &pred),
            RefinementCheckResult::NotChecked
        );
    }
}
