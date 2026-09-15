//! Copyright (c) 2026 Omnira CJSC
//!
//! Symbolic evaluation of a loop-free [`Body`] into `codira_smt` terms.
//!
//! Each op becomes a Z3 term over the shared argument variables; ops that
//! LIA cannot express directly are either
//!
//! * **linearized** when a constant operand makes them linear (`Mul`, `Shl`),
//!   or
//! * **defined by fresh-variable constraints** that are total and functional in
//!   their inputs (`Div`/`Rem`/`Shr` via the appropriate division identity,
//!   `If` via a guarded fresh result variable), or
//! * **rejected** with a precise `Unsupported` reason.
//!
//! Totality + functionality of the side constraints is what keeps the
//! satisfiability query faithful: for *every* argument assignment there is
//! exactly one assignment to the fresh variables satisfying the
//! constraints, so `constraints && before != after` is satisfiable exactly
//! when some argument assignment makes the two bodies disagree.
//!
//! Alongside every term the encoder tracks an optional compile-time value
//! (`Option<i64>` / `Option<bool>`), computed with *checked* arithmetic so
//! a tracked constant always equals the term's value in the unbounded
//! model (a constant chain that overflows `i64` simply stops being
//! tracked). Constants are what license the `Mul`/`Div`/`Rem`/`Shl`/`Shr`
//! encodings above.

use codira_mir::{Attr, Body, Op, OpId, OpKind};
use codira_smt::{BoolExpr, Context, IntExpr};
use rustc_hash::FxHashMap;

use crate::infer::Sort;

/// A symbolic value: a Z3 term plus, when statically known, its constant
/// value (consistent with the unbounded model -- see module docs).
#[derive(Clone, Copy)]
pub(crate) enum Sym<'ctx> {
    Int(IntExpr<'ctx>, Option<i64>),
    Bool(BoolExpr<'ctx>, Option<bool>),
}

/// Boolean equivalence, built from mutual implication since `codira_smt`
/// has no `iff`/`xor` builder.
pub(crate) fn iff<'ctx>(a: BoolExpr<'ctx>, b: BoolExpr<'ctx>) -> BoolExpr<'ctx> {
    a.implies(b).and(b.implies(a))
}

pub(crate) struct Encoder<'a, 'ctx> {
    ctx: &'ctx Context,
    arg_sorts: &'a FxHashMap<u32, Sort>,
    /// Argument variables, shared across every body this encoder touches
    /// (that sharing is the whole point of the equivalence query).
    pub(crate) args: FxHashMap<u32, Sym<'ctx>>,
    /// Side constraints defining fresh variables (division identities,
    /// `If` results). Must be asserted alongside the disagreement atom.
    pub(crate) constraints: Vec<BoolExpr<'ctx>>,
    /// True once any op whose unbounded-model encoding can diverge from
    /// wrapping-`i64` semantics has been emitted; surfaces as
    /// `Proven { modulo_overflow }`.
    pub(crate) overflow_possible: bool,
    fresh: u32,
}

impl<'a, 'ctx> Encoder<'a, 'ctx> {
    pub(crate) fn new(ctx: &'ctx Context, arg_sorts: &'a FxHashMap<u32, Sort>) -> Self {
        Encoder {
            ctx,
            arg_sorts,
            args: FxHashMap::default(),
            constraints: Vec::new(),
            overflow_possible: false,
            fresh: 0,
        }
    }

    /// Encodes a region body, returning the symbolic value of its result
    /// (last) op. Region bodies are closed except for `Arg` references,
    /// so each gets its own op-id map while sharing argument variables
    /// and side constraints through `self`.
    pub(crate) fn encode_body(&mut self, body: &Body) -> Result<Sym<'ctx>, String> {
        let Some(result) = body.result() else {
            return Err("empty body has no result value to compare".to_string());
        };
        let mut local: FxHashMap<OpId, Sym<'ctx>> = FxHashMap::default();
        for (id, op) in body.iter() {
            let sym = self.encode_op(op, &local)?;
            local.insert(id, sym);
        }
        Ok(local[&result])
    }

    fn fresh_int(&mut self) -> IntExpr<'ctx> {
        let n = self.fresh;
        self.fresh += 1;
        self.ctx.int_var(&format!("tv!{n}"))
    }

    fn fresh_bool(&mut self) -> BoolExpr<'ctx> {
        let n = self.fresh;
        self.fresh += 1;
        self.ctx.bool_var(&format!("tv!{n}"))
    }

    /// The (lazily created) variable for `Arg(i)`, with the sort chosen
    /// by inference; unconstrained arguments default to `Int` (crate
    /// docs).
    fn arg(&mut self, i: u32) -> Sym<'ctx> {
        if let Some(sym) = self.args.get(&i) {
            return *sym;
        }
        let sym = match self.arg_sorts.get(&i).copied().unwrap_or(Sort::Int) {
            Sort::Int => Sym::Int(self.ctx.int_var(&format!("a{i}")), None),
            Sort::Bool => Sym::Bool(self.ctx.bool_var(&format!("b{i}")), None),
        };
        self.args.insert(i, sym);
        sym
    }

    fn encode_op(
        &mut self,
        op: &Op,
        local: &FxHashMap<OpId, Sym<'ctx>>,
    ) -> Result<Sym<'ctx>, String> {
        use OpKind::{
            Add, And, Arg, BitAnd, BitNot, BitOr, BitXor, BlockArg, Call, Cast, Const, Div, Eq,
            For, Ge, Gt, If, Le, Lt, Mul, Ne, Neg, Not, Or, ParamRef, Rem, Shl, Shr, Sub, Tuple,
            TupleGet, While, Yield,
        };

        let operand = |n: usize| -> Result<Sym<'ctx>, String> {
            op.operands
                .get(n)
                .and_then(|id| local.get(id).copied())
                .ok_or_else(|| "operand refers to an op outside this region".to_string())
        };

        match &op.kind {
            Const(Attr::Int(v)) => Ok(Sym::Int(self.ctx.int_lit(*v), Some(*v))),
            Const(Attr::Bool(true)) => Ok(Sym::Bool(self.ctx.bool_true(), Some(true))),
            Const(Attr::Bool(false)) => Ok(Sym::Bool(self.ctx.bool_false(), Some(false))),
            Const(attr) => Err(format!(
                "only int/bool constants are encodable; found {}",
                attr_kind_name(attr)
            )),

            Arg(i) => Ok(self.arg(*i)),

            Add | Sub => {
                let (x, xc) = expect_int(operand(0)?)?;
                let (y, yc) = expect_int(operand(1)?)?;
                self.overflow_possible = true;
                let (term, konst) = match op.kind {
                    Add => (x.add(y), checked(xc, yc, i64::checked_add)),
                    _ => (x.sub(y), checked(xc, yc, i64::checked_sub)),
                };
                Ok(Sym::Int(term, konst))
            }

            Mul => {
                let (x, xc) = expect_int(operand(0)?)?;
                let (y, yc) = expect_int(operand(1)?)?;
                self.overflow_possible = true;
                // Linear arithmetic: at least one factor must be a known
                // constant, emitted as a literal so the term is manifestly
                // linear regardless of the shape of the constant's own
                // syntax tree.
                let (term, konst) = match (xc, yc) {
                    (Some(a), Some(b)) => match a.checked_mul(b) {
                        Some(v) => (self.ctx.int_lit(v), Some(v)),
                        // Constant product outside i64: keep the exact
                        // unbounded term, stop tracking the constant.
                        None => (self.ctx.int_lit(a).mul(self.ctx.int_lit(b)), None),
                    },
                    (Some(c), None) => (self.ctx.int_lit(c).mul(y), None),
                    (None, Some(c)) => (self.ctx.int_lit(c).mul(x), None),
                    (None, None) => {
                        return Err(
                            "nonlinear multiplication (neither operand is a compile-time \
                             constant) is not expressible in linear integer arithmetic"
                                .to_string(),
                        )
                    }
                };
                Ok(Sym::Int(term, konst))
            }

            Neg => {
                let (x, xc) = expect_int(operand(0)?)?;
                self.overflow_possible = true;
                Ok(Sym::Int(
                    self.ctx.int_lit(0).sub(x),
                    xc.and_then(i64::checked_neg),
                ))
            }

            Div | Rem => {
                let (x, xc) = expect_int(operand(0)?)?;
                let (_, yc) = expect_int(operand(1)?)?;
                let Some(c) = yc else {
                    return Err(
                        "division/remainder by a non-constant divisor is not encodable \
                         (cannot rule out division by zero, and the quotient would be \
                         nonlinear)"
                            .to_string(),
                    );
                };
                if c == 0 {
                    return Err(
                        "division/remainder by constant zero is an unconditional evaluation \
                         error; the body has no value semantics to validate"
                            .to_string(),
                    );
                }
                if c == -1 {
                    // i64::MIN / -1 errors in fold; the unbounded model
                    // cannot see that, so it falls under the overflow
                    // caveat.
                    self.overflow_possible = true;
                }
                if let Some(a) = xc {
                    if a == i64::MIN && c == -1 {
                        return Err(
                            "constant division overflow (i64::MIN / -1) is an unconditional \
                             evaluation error"
                                .to_string(),
                        );
                    }
                    let v = if matches!(op.kind, Div) { a / c } else { a % c };
                    return Ok(Sym::Int(self.ctx.int_lit(v), Some(v)));
                }
                // Truncated (toward-zero) division, exactly as Rust/i64:
                //   x = q*c + r,  sign(r) in {0, sign(x)},  |r| < |c|.
                // Total and functional in x, so satisfiability-faithful.
                let q = self.fresh_int();
                let r = self.fresh_int();
                let zero = self.ctx.int_lit(0);
                let abs_c = if c > 0 {
                    self.ctx.int_lit(c)
                } else {
                    // Built as 0 - c so c == i64::MIN (|c| = 2^63) still
                    // works: Z3 numerals are unbounded.
                    zero.sub(self.ctx.int_lit(c))
                };
                self.constraints
                    .push(x.eq(self.ctx.int_lit(c).mul(q).add(r)));
                self.constraints
                    .push(x.ge(zero).implies(r.ge(zero).and(r.lt(abs_c))));
                self.constraints
                    .push(x.lt(zero).implies(r.le(zero).and(r.gt(zero.sub(abs_c)))));
                Ok(Sym::Int(if matches!(op.kind, Div) { q } else { r }, None))
            }

            Shl => {
                let (x, xc) = expect_int(operand(0)?)?;
                let (_, yc) = expect_int(operand(1)?)?;
                let k = shift_amount(yc)?;
                self.overflow_possible = true;
                let konst = xc.and_then(|a| {
                    let wide = i128::from(a) << k;
                    i64::try_from(wide).ok()
                });
                Ok(Sym::Int(self.pow2(k).mul(x), konst))
            }

            Shr => {
                let (x, xc) = expect_int(operand(0)?)?;
                let (_, yc) = expect_int(operand(1)?)?;
                let k = shift_amount(yc)?;
                if let Some(a) = xc {
                    let v = a >> k;
                    return Ok(Sym::Int(self.ctx.int_lit(v), Some(v)));
                }
                // Arithmetic right shift of i64 IS floor division by 2^k
                // (unlike truncated division, no sign case-split needed):
                //   x = q*2^k + r,  0 <= r < 2^k.
                // Exact for all of i64 -- no overflow caveat.
                let q = self.fresh_int();
                let r = self.fresh_int();
                let zero = self.ctx.int_lit(0);
                let pow = self.pow2(k);
                self.constraints.push(x.eq(pow.mul(q).add(r)));
                self.constraints.push(r.ge(zero).and(r.lt(pow)));
                Ok(Sym::Int(q, None))
            }

            // Bitwise complement of a non-constant operand needs the same
            // bitvector encoding the binary bitwise ops need; fold it when
            // the operand is constant, decline otherwise.
            BitNot => {
                let (_, xc) = expect_int(operand(0)?)?;
                match xc {
                    Some(v) => Ok(Sym::Int(self.ctx.int_lit(!v), Some(!v))),
                    None => Err(
                        "bitwise complement of a non-constant operand is not expressible in                          linear integer arithmetic (codira_smt exposes no bitvector sorts here)"
                            .to_string(),
                    ),
                }
            }

            BitAnd | BitOr | BitXor => {
                let (_, xc) = expect_int(operand(0)?)?;
                let (_, yc) = expect_int(operand(1)?)?;
                match (xc, yc) {
                    (Some(a), Some(b)) => {
                        let v = match op.kind {
                            BitAnd => a & b,
                            BitOr => a | b,
                            _ => a ^ b,
                        };
                        Ok(Sym::Int(self.ctx.int_lit(v), Some(v)))
                    }
                    _ => Err(
                        "bitwise and/or/xor on non-constant operands is not expressible in \
                         linear integer arithmetic (codira_smt exposes no bitvector sorts)"
                            .to_string(),
                    ),
                }
            }

            Eq | Ne => {
                let l = operand(0)?;
                let r = operand(1)?;
                let (equal, konst) = match (l, r) {
                    (Sym::Int(x, xc), Sym::Int(y, yc)) => {
                        (x.eq(y), checked(xc, yc, |a, b| Some(a == b)))
                    }
                    (Sym::Bool(x, xc), Sym::Bool(y, yc)) => {
                        (iff(x, y), checked(xc, yc, |a, b| Some(a == b)))
                    }
                    _ => {
                        return Err(
                            "equality between an integer and a boolean value is a type error"
                                .to_string(),
                        )
                    }
                };
                Ok(if matches!(op.kind, Eq) {
                    Sym::Bool(equal, konst)
                } else {
                    Sym::Bool(equal.not(), konst.map(|b| !b))
                })
            }

            Lt | Le | Gt | Ge => {
                let (x, xc) = expect_int(operand(0)?)?;
                let (y, yc) = expect_int(operand(1)?)?;
                let (term, konst) = match op.kind {
                    Lt => (x.lt(y), checked(xc, yc, |a, b| Some(a < b))),
                    Le => (x.le(y), checked(xc, yc, |a, b| Some(a <= b))),
                    Gt => (x.gt(y), checked(xc, yc, |a, b| Some(a > b))),
                    _ => (x.ge(y), checked(xc, yc, |a, b| Some(a >= b))),
                };
                Ok(Sym::Bool(term, konst))
            }

            And | Or => {
                let (x, xc) = expect_bool(operand(0)?)?;
                let (y, yc) = expect_bool(operand(1)?)?;
                let (term, konst) = match op.kind {
                    And => (x.and(y), checked(xc, yc, |a, b| Some(a && b))),
                    _ => (x.or(y), checked(xc, yc, |a, b| Some(a || b))),
                };
                Ok(Sym::Bool(term, konst))
            }

            Not => {
                let (x, xc) = expect_bool(operand(0)?)?;
                Ok(Sym::Bool(x.not(), xc.map(|b| !b)))
            }

            If => {
                let (cond, _) = expect_bool(operand(0)?)?;
                let [then_r, else_r] = op.regions.as_slice() else {
                    return Err("cf.if without exactly two regions".to_string());
                };
                if then_r.body.is_empty() || else_r.body.is_empty() {
                    return Err(
                        "cf.if with an empty branch (unit-valued if) has no comparable result"
                            .to_string(),
                    );
                }
                let then_sym = self.encode_body(&then_r.body)?;
                let else_sym = self.encode_body(&else_r.body)?;
                // No ite in codira_smt: define the result as a fresh
                // variable v with cond -> v = then and !cond -> v = else.
                // Exactly one guard fires per assignment, so v is uniquely
                // determined -- total and functional, as required.
                match (then_sym, else_sym) {
                    (Sym::Int(t, _), Sym::Int(e, _)) => {
                        let v = self.fresh_int();
                        self.constraints.push(cond.implies(v.eq(t)));
                        self.constraints.push(cond.not().implies(v.eq(e)));
                        Ok(Sym::Int(v, None))
                    }
                    (Sym::Bool(t, _), Sym::Bool(e, _)) => {
                        let v = self.fresh_bool();
                        self.constraints.push(cond.implies(iff(v, t)));
                        self.constraints.push(cond.not().implies(iff(v, e)));
                        Ok(Sym::Bool(v, None))
                    }
                    _ => Err("cf.if branches yield values of different sorts".to_string()),
                }
            }

            // A cast's meaning depends on its *target type*, which lives
            // on the Op rather than the OpKind. Encoding it exactly needs
            // the width-accurate bitvector encoding of RFC-001 phase 3
            // (and, for float conversions, the FP theory in z3_fpa.h --
            // see RFC-002 section 4.1). Refusing is the honest answer
            // until then: an approximate cast encoding would produce
            // *false proofs*, which is the one outcome a translation
            // validator must never produce.
            Cast(..) => Err(
                "casts are not yet encodable: exact encoding requires the                  width-accurate bitvector model (RFC-001 phase 3)"
                    .to_string(),
            ),

            While | For => Err(
                "loops (cf.while/cf.for) are outside the loop-free fragment this validator \
                 encodes"
                    .to_string(),
            ),
            Yield => Err("cf.yield only occurs inside loop bodies, which are unsupported"
                .to_string()),
            Call(name) => Err(format!(
                "call to @{name} is outside the call-free fragment this validator encodes"
            )),
            Tuple | TupleGet(_) => {
                Err("tuple values are outside the fragment this validator encodes".to_string())
            }
            ParamRef(name) => Err(format!(
                "unresolved generator parameter @{name}; run elaboration before validating"
            )),
            BlockArg(_) => Err(
                "block arguments only occur inside loop regions, which are unsupported"
                    .to_string(),
            ),
        }
    }

    /// `2^k` for `k` in `0..64` as an exact unbounded term (`2^63` does
    /// not fit an `i64` literal, so it is built as `2^62 * 2`).
    fn pow2(&self, k: u32) -> IntExpr<'ctx> {
        if k < 63 {
            self.ctx.int_lit(1i64 << k)
        } else {
            self.ctx.int_lit(1i64 << 62).mul(self.ctx.int_lit(2))
        }
    }
}

fn attr_kind_name(attr: &Attr) -> &'static str {
    match attr {
        Attr::Int(_) => "an int",
        Attr::Bool(_) => "a bool",
        Attr::Float(_) => "a float constant",
        Attr::Str(_) => "a string constant",
        Attr::Unit => "a unit constant",
        Attr::ParamRef(_) => "an unresolved parameter reference",
    }
}

fn expect_int(sym: Sym<'_>) -> Result<(IntExpr<'_>, Option<i64>), String> {
    match sym {
        Sym::Int(term, konst) => Ok((term, konst)),
        Sym::Bool(..) => Err("expected an integer operand, found a boolean".to_string()),
    }
}

fn expect_bool(sym: Sym<'_>) -> Result<(BoolExpr<'_>, Option<bool>), String> {
    match sym {
        Sym::Bool(term, konst) => Ok((term, konst)),
        Sym::Int(..) => Err("expected a boolean operand, found an integer".to_string()),
    }
}

/// Lifts a binary constant-fold over two optional constants.
fn checked<A, B>(x: Option<A>, y: Option<A>, f: impl FnOnce(A, A) -> Option<B>) -> Option<B> {
    match (x, y) {
        (Some(a), Some(b)) => f(a, b),
        _ => None,
    }
}

/// Validates a shift amount: must be a known constant in `0..64` (out of
/// range is an unconditional evaluation error in `fold`, so the body has
/// no value to compare; non-constant amounts would make the power-of-two
/// factor nonlinear).
fn shift_amount(yc: Option<i64>) -> Result<u32, String> {
    let Some(k) = yc else {
        return Err(
            "shift by a non-constant amount is not encodable in linear integer \
                    arithmetic"
                .to_string(),
        );
    };
    if !(0..64).contains(&k) {
        return Err(format!(
            "shift amount {k} is outside 0..64, an unconditional evaluation error"
        ));
    }
    Ok(k as u32)
}
