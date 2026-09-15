//! Copyright (c) 2026 Omnira CJSC. All Rights Reserved.
//! Author: Tunjay Akbarli
//! Date: September 15, 2026
//!
//! `codira_tv` -- SMT-backed **translation validation** for `codira_mir`.
//!
//! After an optimizer (the e-graph rewriter, `simplify`, CSE, constant
//! folding, ...) rewrites a function [`Body`], this crate *proves* that the
//! rewrite preserved semantics -- or produces a concrete counterexample
//! input on which the two bodies disagree. It makes the compiler
//! self-skeptical: the pass manager does not have to trust its own
//! optimizer, it can ask for a proof after every rewrite.
//!
//! # Prior art, and how this differs
//!
//! Translation validation is the classic alternative to verifying a
//! compiler once and for all: instead, validate each *individual*
//! translation the compiler actually performed.
//!
//! * Pnueli, Siegel, Singerman, *Translation Validation* (TACAS 1998) --
//!   introduced the idea for a synchronous-language-to-C compiler.
//! * Necula, *Translation Validation for an Optimizing Compiler* (PLDI 2000) --
//!   scaled it to GCC's intraprocedural optimizations via symbolic evaluation
//!   and a bespoke prover.
//! * Alive/Alive2 (Lopes et al., PLDI 2021) -- SMT-based refinement checking
//!   for LLVM IR, run offline over the LLVM test suite as an external tool.
//!
//! `codira_tv` follows the same architecture as Alive2 (symbolically
//! evaluate both programs into solver terms, assert result disagreement,
//! ask for UNSAT), but with a different deployment model: it validates the
//! **mid-level structured IR** (region-based `cf.if`, not a flat CFG),
//! **online**, **per-function**, as an always-available **library** the
//! pass manager can call after any rewrite -- rather than an offline batch
//! tool over a test corpus. Anything it cannot encode it reports honestly
//! as [`Verdict::Unsupported`] instead of guessing.
//!
//! # The validated fragment
//!
//! Loop-free, call-free, tuple-free, param-free bodies over `Int`/`Bool`:
//! straight-line SSA plus (arbitrarily nested) `cf.if`. This is exactly
//! the fragment the algebraic-rewrite passes actually touch; loops, calls,
//! floats, strings, and tuples yield `Unsupported` with a reason.
//!
//! # Soundness decisions (read this before trusting a verdict)
//!
//! The encoding targets what `codira_smt` exposes: **linear integer
//! arithmetic over unbounded Z3 `Int`**, plus `Bool`. `codira_smt` has no
//! bitvector sorts, so a bit-precise wrapping-`i64` model is not
//! expressible. Consequences, op by op:
//!
//! * **Unbounded integers vs. wrapping `i64`.** `codira_mir::fold` defines
//!   `Int` as wrapping two's-complement `i64`; Z3 `Int` is unbounded. A
//!   [`Verdict::Proven`] therefore certifies equivalence **for all executions
//!   in which no intermediate value overflows `i64`** -- the standard
//!   translation-validation caveat, surfaced explicitly as `Proven {
//!   modulo_overflow: true }` whenever the bodies contain any overflow-capable
//!   op (`Add`/`Sub`/`Mul`/`Neg`/`Shl`, or `Div`/`Rem` with divisor `-1`).
//!   Bodies built only from comparisons, boolean connectives, `Div`/`Rem`/`Shr`
//!   by benign constants, and constants are modeled *exactly*, and get `Proven
//!   { modulo_overflow: false }`.
//! * **`Mul`**: linear arithmetic only -- multiplication is encoded when at
//!   least one operand is a compile-time constant (emitted as `c * x`, a linear
//!   term). Variable-times-variable is `Unsupported`.
//! * **`Div`/`Rem`**: only by a *nonzero constant* `c`. Rust/i64 division
//!   truncates toward zero (not Euclidean, not floor), so it is encoded
//!   precisely with fresh quotient/remainder variables and a sign case-split:
//!   `x = q*c + r`, with `x >= 0 -> 0 <= r < |c|` and `x < 0 -> -|c| < r <= 0`.
//!   These constraints are total and functional (for every `x` a unique `(q,
//!   r)` exists), so they neither block nor admit spurious counterexamples.
//!   Division by a variable, or by constant zero (an unconditional evaluation
//!   error), is `Unsupported`. `c == -1` sets the overflow caveat (`i64::MIN /
//!   -1` errors in `fold`, which the unbounded model cannot see).
//! * **`Shl` by constant `k` in `0..64`**: `x << k` is `x * 2^k` in the
//!   unbounded model; shifted-out bits fall under the overflow caveat.
//! * **`Shr` by constant `k` in `0..64`**: arithmetic right shift of `i64`
//!   **is** floor division by `2^k` (not truncated division!), and floor
//!   division is encoded exactly: `x = q * 2^k + r`, `0 <= r < 2^k`. This op is
//!   exact -- it never sets the overflow caveat. Non-constant or out-of-range
//!   shift amounts are `Unsupported`.
//! * **`BitAnd`/`BitOr`/`BitXor`**: folded when both operands are constants;
//!   otherwise `Unsupported` (not expressible in LIA without bitvectors).
//! * **`If`**: `codira_smt` exposes no `ite` builder, so each `cf.if` is
//!   encoded with a fresh variable `v` constrained by `cond -> v = then` and
//!   `!cond -> v = else` (with `<->` via mutual implication for boolean
//!   results). Again total and functional, so satisfiability is preserved in
//!   both directions.
//! * **Error behavior**: within the accepted fragment every dynamic `FoldError`
//!   is ruled out syntactically (nonzero constant divisors, in-range constant
//!   shift amounts) except `i64::MIN / -1`, which is subsumed by the overflow
//!   caveat. Equivalence is therefore plain value equality over the modeled
//!   executions.
//!
//! # Argument sorts
//!
//! `OpKind::Arg(i)` is untyped in the IR, so each argument's sort is
//! inferred from its **use sites** across *both* bodies (an argument fed
//! to `Add` is an integer; an `If` condition is a boolean; `Eq`/`Ne`
//! propagate the other operand's sort). Conflicting uses yield
//! `Unsupported`; arguments with no constraining use default to `Int`.
//! Both bodies are encoded over the *same* argument variables, which is
//! what makes the equivalence query meaningful.
//!
//! # Counterexamples
//!
//! `codira_smt` does not expose Z3's model API, so when the disagreement
//! query is satisfiable this crate recovers a witness by **greedy bounded
//! probing**: it re-checks satisfiability with each argument pinned to
//! small candidate values in turn, fixing one argument at a time. Every
//! returned witness is solver-verified (the final check is SAT with all
//! arguments pinned), but the search is incomplete -- if only exotic
//! values refute the rewrite, the verdict is still
//! [`Verdict::Refuted`] with `args: None`. Witness values live in the
//! unbounded model; they are genuine `i64` counterexamples whenever the
//! refuting execution does not overflow.

mod encode;
mod infer;

use codira_mir::{verify_body, Body};
use codira_smt::{BoolExpr, Context, SatResult};

use crate::encode::{iff, Encoder, Sym};

/// The outcome of validating one rewrite. See the crate docs for the exact
/// guarantees behind each variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The two bodies are semantically equivalent.
    ///
    /// `modulo_overflow: true` means the proof holds in the unbounded
    /// integer model, i.e. for every execution in which no intermediate
    /// value leaves `i64` range (the bodies contain at least one
    /// overflow-capable op). `false` means every op involved is modeled
    /// exactly and the proof is unconditional.
    Proven { modulo_overflow: bool },
    /// The bodies disagree on at least one input.
    ///
    /// `args`, when present, is a solver-verified witness assignment:
    /// `args[i]` is the value of `Arg(i)` (booleans encoded as `0`/`1`;
    /// arguments referenced by neither body filled with `0`). `None`
    /// means the disagreement is proven but the bounded witness search
    /// did not land on a concrete assignment.
    Refuted { args: Option<Vec<i64>> },
    /// The bodies fall outside the encodable fragment; `reason` says
    /// exactly which construct was the obstacle.
    Unsupported { reason: String },
    /// Z3 could not decide the query.
    Unknown,
}

/// A configurable validator.
///
/// The only knob today is whether to spend extra solver calls searching
/// for a concrete counterexample on refutation. A per-query *timeout* is
/// deliberately absent: `codira_smt` does not expose Z3's parameter API
/// (`Z3_set_param_value` / solver params), so a timeout is not expressible
/// through it -- see the crate README/summary for the wishlist.
#[derive(Debug, Clone)]
pub struct Validator {
    search_witness: bool,
}

impl Validator {
    /// A validator with default settings (witness search enabled).
    pub fn new() -> Self {
        Validator {
            search_witness: true,
        }
    }

    /// Disables the bounded counterexample search: refutations come back
    /// as `Refuted { args: None }` after a single solver call.
    pub fn without_witness_search(mut self) -> Self {
        self.search_witness = false;
        self
    }

    /// Validates that `after` computes the same result as `before` on all
    /// (non-overflowing) inputs. See [`Verdict`] and the crate docs.
    pub fn validate(&self, before: &Body, after: &Body) -> Verdict {
        // Structurally malformed IR has no semantics to compare; report it
        // rather than encoding garbage.
        if let Err(e) = verify_body(before) {
            return Verdict::Unsupported {
                reason: format!("before body fails structural verification: {e}"),
            };
        }
        if let Err(e) = verify_body(after) {
            return Verdict::Unsupported {
                reason: format!("after body fails structural verification: {e}"),
            };
        }

        let arg_sorts = match infer::infer_arg_sorts(&[before, after]) {
            Ok(sorts) => sorts,
            Err(reason) => return Verdict::Unsupported { reason },
        };

        let ctx = Context::new();
        let mut enc = Encoder::new(&ctx, &arg_sorts);

        let sym_before = match enc.encode_body(before) {
            Ok(sym) => sym,
            Err(reason) => return Verdict::Unsupported { reason },
        };
        let sym_after = match enc.encode_body(after) {
            Ok(sym) => sym,
            Err(reason) => return Verdict::Unsupported { reason },
        };

        // The equivalence query: do the encodings (which are functional in
        // the shared argument variables) admit an argument assignment on
        // which the results differ? UNSAT <=> no counterexample <=> the
        // rewrite is proven (in the unbounded model).
        let disagree = match (sym_before, sym_after) {
            (Sym::Int(b, _), Sym::Int(a, _)) => b.eq(a).not(),
            (Sym::Bool(b, _), Sym::Bool(a, _)) => iff(b, a).not(),
            _ => {
                return Verdict::Unsupported {
                    reason: "before and after bodies compute results of different sorts \
                             (int vs bool)"
                        .to_string(),
                }
            }
        };

        let solver = ctx.solver();
        for c in &enc.constraints {
            solver.assert(*c);
        }
        solver.assert(disagree);

        match solver.check() {
            SatResult::Unsat => Verdict::Proven {
                modulo_overflow: enc.overflow_possible,
            },
            SatResult::Sat => Verdict::Refuted {
                args: if self.search_witness {
                    search_witness(&ctx, &enc, disagree)
                } else {
                    None
                },
            },
            SatResult::Unknown => Verdict::Unknown,
        }
    }
}

impl Default for Validator {
    fn default() -> Self {
        Self::new()
    }
}

/// Validates a single rewrite with default settings. The library entry
/// point a pass manager calls after an optimizer touches a body.
pub fn validate_rewrite(before: &Body, after: &Body) -> Verdict {
    Validator::new().validate(before, after)
}

/// Candidate values tried (in order) for each integer argument during
/// witness recovery. Small values first: they are the counterexamples
/// humans want to read, and the ones rewrite bugs almost always admit.
const PROBE_CANDIDATES: &[i64] = &[
    0, 1, -1, 2, -2, 3, -3, 4, -4, 5, -5, 6, -6, 7, -7, 8, -8, 9, -9, 10, -10, 15, -15, 16, -16,
    17, 31, 32, 33, 63, 64, 100, -100, 127, 128, 255, 256, 1000, -1000,
];

/// Greedy bounded witness search (see crate docs): fixes arguments one at
/// a time to the first candidate value that keeps the disagreement
/// satisfiable. `codira_smt` exposes neither models nor push/pop, so each
/// probe is a fresh solver over the same assertions -- cheap at these
/// formula sizes.
fn search_witness<'ctx>(
    ctx: &'ctx Context,
    enc: &Encoder<'_, 'ctx>,
    disagree: BoolExpr<'ctx>,
) -> Option<Vec<i64>> {
    let mut arg_syms: Vec<(u32, Sym<'ctx>)> = enc.args.iter().map(|(i, s)| (*i, *s)).collect();
    arg_syms.sort_by_key(|(i, _)| *i);

    let check_with = |fixed: &[(Sym<'ctx>, i64)]| -> bool {
        let solver = ctx.solver();
        for c in &enc.constraints {
            solver.assert(*c);
        }
        solver.assert(disagree);
        for (sym, value) in fixed {
            let pin = match sym {
                Sym::Int(term, _) => term.eq(ctx.int_lit(*value)),
                Sym::Bool(term, _) => {
                    if *value != 0 {
                        *term
                    } else {
                        term.not()
                    }
                }
            };
            solver.assert(pin);
        }
        solver.check() == SatResult::Sat
    };

    let mut fixed: Vec<(Sym<'ctx>, i64)> = Vec::with_capacity(arg_syms.len());
    let mut values: Vec<(u32, i64)> = Vec::with_capacity(arg_syms.len());
    for (index, sym) in &arg_syms {
        let candidates: &[i64] = match sym {
            Sym::Int(..) => PROBE_CANDIDATES,
            Sym::Bool(..) => &[1, 0],
        };
        let mut chosen = None;
        for &candidate in candidates {
            fixed.push((*sym, candidate));
            if check_with(&fixed) {
                chosen = Some(candidate);
                break;
            }
            fixed.pop();
        }
        let value = chosen?;
        values.push((*index, value));
    }

    // Dense witness vector indexed by argument number; arguments no body
    // references cannot influence either result, so 0 is as good a value
    // as any there.
    let len = values
        .iter()
        .map(|(i, _)| *i as usize + 1)
        .max()
        .unwrap_or(0);
    let mut witness = vec![0i64; len];
    for (index, value) in values {
        witness[index as usize] = value;
    }
    Some(witness)
}
