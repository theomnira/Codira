//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Static verification of function contracts with Z3: `requires`, `ensures`
//!   (with `result`), loop `invariant`/`decreases`, `ghost let`, and call-site
//!   refinement subtyping.
//!
//! # What is checked
//!
//! Every function is symbolically executed over its HIR body, and each of
//! the following becomes a proof obligation discharged by Z3
//! (`codira_smt`):
//!
//! * at every call: the callee's `requires` clauses, and the refinement
//!   predicate of every refined parameter (`x: i64 { v | v > 0 }`, directly or
//!   through a `type` alias), proven from the caller's own facts -- its path
//!   condition, its `requires`, its parameters' refinements, and the `ensures`
//!   of everything it called before;
//! * at every return point: the function's `ensures` clauses and its refined
//!   return type;
//! * for every loop with an `invariant`: that it holds on entry and is
//!   preserved by one arbitrary iteration (the classic inductive check:
//!   variables the loop assigns are havocked and the invariant assumed);
//! * for every loop with a `decreases` measure: that the measure is
//!   non-negative at the head and strictly smaller after the body, which proves
//!   the loop terminates;
//! * in a function that has a contract of its own: every `/` and `%` has a
//!   non-zero divisor and no signed `MIN / -1` overflow -- LLVM gives both
//!   undefined behaviour, so a verified function may not contain either.
//!
//! A proven obligation costs nothing at run time: the clauses live in
//! `Body::contracts`, which nothing reachable from the body expression
//! refers to, so code generation never sees them.
//!
//! # Semantics, exactly
//!
//! Machine integers are modelled as bit-vectors of their own width, with
//! the operations the code generator emits: wrapping `+ - *` (spec section
//! 18.2), signed or unsigned division/remainder/comparison by the operand
//! type, arithmetic or logical right shift. Contract clauses are evaluated
//! with the *same* semantics, so `ensures result > x` for `x + 1` is refuted
//! at `x = i64::MAX`, exactly as the compiled code behaves. Inside `ensures`
//! a parameter name means the parameter's value on entry.
//!
//! # What is not modelled (and so cannot be proven, only not refuted)
//!
//! Struct fields, arrays, strings, floats, function values and anything
//! read through them are fresh unconstrained values at every read. That is
//! sound -- nothing is assumed about them -- but a clause that *mentions*
//! one cannot be encoded and is reported as unsupported, never silently
//! accepted. Calls inside contract clauses are not supported yet (the
//! clause would need the callee to be pure). A loop without an `invariant`
//! is havocked: whatever it assigns is unknown afterwards. Shifts by more
//! than the operand width are LLVM poison; they are modelled as Z3's total
//! shift, not flagged.
//!
//! Every Z3 query runs under the deterministic `rlimit` budget
//! (`codira_smt::DEFAULT_RLIMIT`); a query that exhausts it is reported as
//! "unknown (resource limit)", which is an error, never a pass.

pub mod api;
pub(crate) mod exec;
pub(crate) mod flow;
mod ghost;
pub mod query;
pub(crate) mod refinement;
pub mod synth;
pub mod testgen;
#[cfg(test)]
mod tests;
pub(crate) mod units;

use std::{fmt, sync::Arc};

use codira_syntax::SyntaxNodePtr;

pub use self::exec::is_participating;
use crate::{
    code_model::Function,
    diagnostics::{ContractViolation, DiagnosticSink},
    in_file::InFile,
    HasSource, HirDatabase,
};

/// Every proof obligation of one function and what Z3 said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    /// The function's parameters, in order, as the counterexamples name
    /// them.
    pub params: Vec<ParamInfo>,
    pub obligations: Vec<Obligation>,
}

impl VerificationReport {
    /// The obligations that were not proven.
    pub fn failures(&self) -> impl Iterator<Item = &Obligation> {
        self.obligations
            .iter()
            .filter(|o| !matches!(o.verdict, Verdict::Proven))
    }
}

/// A parameter of the verified function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamInfo {
    pub name: String,
    /// `None` for a parameter whose type the verifier does not model (a
    /// struct, a float, ...). A counterexample cannot give it a value.
    pub ty: Option<ScalarTy>,
}

/// A type the verifier models exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarTy {
    Int { signed: bool, width: u32 },
    Bool,
}

impl fmt::Display for ScalarTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScalarTy::Int { signed, width } => {
                write!(f, "{}{width}", if *signed { "i" } else { "u" })
            }
            ScalarTy::Bool => f.write_str("bool"),
        }
    }
}

/// One thing that has to be true for the function to be correct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obligation {
    pub kind: ObligationKind,
    /// Where the obligation arises: the call, the clause, the division.
    pub site: InFile<SyntaxNodePtr>,
    /// The predicate that has to hold, as written in the source.
    pub clause: String,
    pub verdict: Verdict,
    /// Whether a run-time check enforces this obligation: a `requires`
    /// clause of a `@runtime_checked` function. If Z3 cannot decide it, that
    /// is not an error -- the callee traps on entry instead. A *refuted*
    /// one still is (see `contract_diagnostics`).
    pub runtime_checked: bool,
}

/// Why an obligation exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ObligationKind {
    /// A callee's `requires` clause, at a call site.
    Precondition { callee: String },
    /// A callee's refined parameter type, at a call site.
    RefinedArgument { callee: String, param: String },
    /// This function's `ensures` clause, at a return point.
    Postcondition,
    /// This function's refined return type, at a return point.
    RefinedReturn,
    /// A loop invariant, before the first iteration.
    InvariantOnEntry,
    /// A loop invariant, after an arbitrary iteration.
    InvariantPreserved,
    /// A `decreases` measure, at the loop head.
    MeasureNonNegative,
    /// A `decreases` measure, after the body.
    MeasureDecreases,
    /// A divisor in a verified function.
    DivisionByZero,
    /// A signed division in a verified function.
    DivisionOverflow,
    /// One of this function's own clauses, which the verifier cannot
    /// encode; reported so it is never silently ignored.
    UncheckableClause,
    /// A closure in code the verifier checks: its body is not verified, so
    /// whatever it would have to prove is reported instead of passed.
    UnverifiedClosure,
    /// The predicate of a `choose x where pred` (synthesis only).
    ChoosePredicate,
}

/// What Z3 said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Proven,
    /// Z3 found inputs that violate the obligation.
    Refuted(Counterexample),
    Unknown(UnknownCause),
}

/// Why an obligation is neither proven nor refuted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnknownCause {
    /// The deterministic `rlimit` budget ran out.
    ResourceLimit,
    /// Z3 answered `unknown` for another reason (its own words).
    Solver(String),
    /// The clause or the code around it is outside what the verifier
    /// models.
    Unsupported(String),
}

impl fmt::Display for UnknownCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnknownCause::ResourceLimit => f.write_str("unknown (resource limit)"),
            UnknownCause::Solver(reason) => write!(f, "unknown ({reason})"),
            UnknownCause::Unsupported(reason) => f.write_str(reason),
        }
    }
}

/// Concrete inputs on which an obligation fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counterexample {
    /// A value for every parameter the verifier models, in parameter
    /// order.
    pub inputs: Vec<(String, Value)>,
    /// Values at the failing point that explain the failure: the callee's
    /// arguments at a call, `result` at a return.
    pub at_site: Vec<(String, Value)>,
}

/// A concrete value from a Z3 model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    Int {
        bits: u128,
        signed: bool,
        width: u32,
    },
    Bool(bool),
}

impl Value {
    /// The value as a mathematical integer (`None` for a `bool`, or a
    /// `u128` above `i128::MAX`).
    pub fn as_i128(&self) -> Option<i128> {
        match *self {
            Value::Int {
                bits,
                signed,
                width,
            } => {
                if signed && width < 128 && (bits >> (width - 1)) & 1 == 1 {
                    Some(bits as i128 - (1i128 << width))
                } else if signed || bits <= i128::MAX as u128 {
                    Some(bits as i128)
                } else {
                    None
                }
            }
            Value::Bool(_) => None,
        }
    }

    /// The value written as Codira source that evaluates to it.
    ///
    /// This differs from [`fmt::Display`] in exactly one case: the minimum
    /// value of a signed type. `-9223372036854775808` is the negation of a
    /// literal that does not fit in `i64`, so it is spelled
    /// `(-9223372036854775807 - 1)` instead.
    pub fn to_source(&self) -> String {
        match *self {
            Value::Int {
                signed: true,
                width,
                bits,
            } if width <= 128 && bits == 1u128 << (width - 1) => {
                let max = if width == 128 {
                    i128::MAX
                } else {
                    (1i128 << (width - 1)) - 1
                };
                format!("(-{max} - 1)")
            }
            _ => self.to_string(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int { bits, signed, .. } => match self.as_i128() {
                Some(v) if signed || v >= 0 => write!(f, "{v}"),
                _ => write!(f, "{bits}"),
            },
        }
    }
}

impl Counterexample {
    /// `name(a: 1, b: 0)`, the call that reproduces the failure.
    pub fn call(&self, function: &str) -> String {
        let args: Vec<String> = self
            .inputs
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect();
        format!("{function}({})", args.join(", "))
    }
}

impl Obligation {
    /// The diagnostic headline for an obligation that was not proven.
    pub fn message(&self) -> String {
        let what = match &self.kind {
            ObligationKind::Precondition { callee } => {
                format!(
                    "precondition of `{callee}` might not hold: `{}`",
                    self.clause
                )
            }
            ObligationKind::RefinedArgument { callee, param } => format!(
                "argument `{param}` of `{callee}` might not satisfy its refinement `{}`",
                self.clause
            ),
            ObligationKind::Postcondition => {
                format!("postcondition might not hold: `{}`", self.clause)
            }
            ObligationKind::RefinedReturn => format!(
                "return value might not satisfy its refinement `{}`",
                self.clause
            ),
            ObligationKind::InvariantOnEntry => {
                format!("loop invariant might not hold on entry: `{}`", self.clause)
            }
            ObligationKind::InvariantPreserved => format!(
                "loop invariant might not be preserved by the loop body: `{}`",
                self.clause
            ),
            ObligationKind::MeasureNonNegative => format!(
                "termination measure `{}` might be negative at the loop head",
                self.clause
            ),
            ObligationKind::MeasureDecreases => format!(
                "termination measure `{}` might not decrease, so the loop might not terminate",
                self.clause
            ),
            ObligationKind::DivisionByZero => {
                format!("divisor `{}` might be zero", self.clause)
            }
            ObligationKind::DivisionOverflow => format!(
                "signed division `{}` might overflow (minimum value divided by -1)",
                self.clause
            ),
            ObligationKind::UncheckableClause => {
                format!("contract clause `{}` cannot be checked", self.clause)
            }
            ObligationKind::UnverifiedClosure => {
                "this closure's body is not verified yet".to_string()
            }
            ObligationKind::ChoosePredicate => {
                format!("the chosen value might not satisfy `{}`", self.clause)
            }
        };
        match &self.verdict {
            Verdict::Proven | Verdict::Refuted(_) => what,
            Verdict::Unknown(cause) => format!("{what}: {cause}"),
        }
    }
}

/// Whether `func` carries `@runtime_checked`: its `requires` clauses are
/// evaluated on entry at run time (a violation executes a trap
/// instruction), so a precondition the verifier cannot decide at a call site
/// is enforced instead of rejected. One it refutes is still an error.
/// Postconditions, loop invariants and measures are not run-time checked; they
/// stay static obligations.
pub fn is_runtime_checked(db: &dyn HirDatabase, func: Function) -> bool {
    use codira_syntax::ast::AttributeOwner;
    func.source(db)
        .value
        .attribute_list()
        .is_some_and(|attrs| {
            attrs.attributes().any(|attr| {
                attr.path().and_then(|p| p.segment()).is_some_and(|s| {
                    matches!(
                        s.kind(),
                        Some(codira_syntax::ast::PathSegmentKind::Name(n)) if n.text() == "runtime_checked"
                    )
                })
            })
        })
}

/// What the signature of a `@constant_time` or `oblivious` function says
/// about secrets: per ordinary parameter whether it is `secret`, and whether
/// the result is. `None` for a function that is neither. For checks outside
/// this crate, such as the machine-code constant-time check.
pub fn constant_time_signature(db: &dyn HirDatabase, func: Function) -> Option<(Vec<bool>, bool)> {
    let signature = flow::secret_signature(db, func);
    signature
        .constant_time
        .then_some((signature.params, signature.result))
}

/// The salsa query behind [`HirDatabase::verify_function`].
pub(crate) fn verify_function_query(
    db: &dyn HirDatabase,
    func: Function,
) -> Arc<VerificationReport> {
    Arc::new(exec::verify(db, func))
}

/// One obligation of a function, with the queries that decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertifiedObligation {
    pub obligation: Obligation,
    /// Every Z3 query asked for this obligation, as a self-contained
    /// SMT-LIB2 script: the premise (path condition, `requires`, parameter
    /// refinements, callee `ensures`) asserted first, the negated goal
    /// last. The obligation is proven exactly when every one is `unsat`.
    /// Empty when the obligation could not be encoded.
    pub queries: Vec<String>,
}

/// Verifies `func` again, keeping the text of every Z3 query: what a proof
/// certificate (`codira build --certify`) records so that a checker can
/// re-decide each obligation without this crate. Verdicts are those of
/// [`HirDatabase::verify_function`]; the queries are not cached, so this
/// costs a second verification of the function.
pub fn certify_function(db: &dyn HirDatabase, func: Function) -> Vec<CertifiedObligation> {
    let (report, queries) = exec::verify_recording(db, func, true);
    report
        .obligations
        .into_iter()
        .zip(queries)
        .map(|(obligation, queries)| CertifiedObligation {
            obligation,
            queries,
        })
        .collect()
}

/// Pushes a diagnostic for every obligation of `func` that was not proven,
/// plus the ghost-code checks.
pub(crate) fn contract_diagnostics(
    db: &dyn HirDatabase,
    func: Function,
    sink: &mut DiagnosticSink<'_>,
) {
    ghost::ghost_diagnostics(db, func, sink);
    flow::flow_diagnostics(db, func, sink);
    units::unit_diagnostics(db, func, sink);

    // A function with an unfilled synthesis hole has no meaning to verify
    // yet: report the holes and nothing else.
    if synth::report_holes(db, func, sink) {
        return;
    }

    let report = db.verify_function(func);
    let name = func.name(db).to_string();
    // A run-time check covers an obligation Z3 could not decide. It does not
    // excuse one Z3 refuted: that input is a real way to reach the call, and
    // a call whose arguments the optimizer can fold would also be folded
    // past the check.
    let excused = |o: &&Obligation| o.runtime_checked && matches!(o.verdict, Verdict::Unknown(_));
    for obligation in report.failures().filter(|o| !excused(o)) {
        let mut notes = Vec::new();
        if let Verdict::Refuted(cex) = &obligation.verdict {
            notes.push(format!("counterexample: {}", cex.call(&name)));
            if !cex.at_site.is_empty() {
                let values: Vec<String> = cex
                    .at_site
                    .iter()
                    .map(|(n, v)| format!("{n} = {v}"))
                    .collect();
                notes.push(format!("at the failing point: {}", values.join(", ")));
            }
        }
        sink.push(ContractViolation {
            file: obligation.site.file_id,
            site: obligation.site.value.clone(),
            message: obligation.message(),
            notes,
        });
    }
}
