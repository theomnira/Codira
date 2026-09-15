//! Copyright (c) 2026 Omnira CJSC
//!
//! The core IR data model: [`Op`], [`OpKind`], [`Attr`], [`Body`], [`Region`].
//!
//! This mirrors MLIR's generic `Operation`/`Region`/`Attribute` model (and,
//! more specifically, the shape of KGEN's `kgen`/`pop`/`hlcf` dialects --
//! see `spec/EIDOS_ARCHITECTURE.md` §2) without a TableGen/C++
//! dialect-registration mechanism: op kinds are grouped into "dialects" by
//! naming convention on the [`OpKind`] variants (`core.*`, `param.*`,
//! `cf.*`) rather than by separate Rust crates or types, per §7.2's "one
//! IR, not four dialects" simplification.
//!
//! # Structured control flow, not a flat CFG
//!
//! KGEN itself keeps *structured* control flow (`hlcf.for`/`hlcf.loop` with
//! `hlcf.yield`) all the way through elaboration and only flattens to a
//! basic-block CFG in `LowerLoops`, immediately before LLVM
//! (`modular/KGEN/lib/Compiler/Pipeline/Pipeline.cpp`, late-opt pipeline).
//! MLIR's `scf` dialect makes the same choice. This IR follows suit:
//! loops are region ops carrying **loop-carried values** ("iter args", the
//! region's *block arguments*, referenced by [`OpKind::BlockArg`]) and an
//! explicit multi-value [`OpKind::Yield`] terminator, rather than blocks,
//! branches, and phi nodes. SSA within a region is by construction (the
//! arena is append-only and operands can only point backwards); dataflow
//! *across* iterations is exactly the iter-arg/yield pair.

use la_arena::{Arena, Idx};
use smallvec::SmallVec;
use smol_str::SmolStr;

use crate::ty::TypeId;

/// A compile-time-known value attached to an [`Op`].
///
/// KGEN's terminology (`DesignOverview.md`, "Generator Parameter
/// Arguments"): attributes are *not* SSA values -- they are the meta-program
/// data a generator acts on at elaboration time, as opposed to [`OpId`]
/// operands, which are ordinary SSA values computed at (kernel) runtime.
/// Both distinctions are preserved here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Attr {
    Int(i64),
    Bool(bool),
    /// An `f64` stored as its raw bit pattern so `Attr` stays `Eq + Hash`
    /// (required for e-graph hashconsing and CSE). Construct with
    /// [`Attr::float`], read with [`Attr::as_float`]. Two NaNs with the
    /// same bit pattern compare equal here -- that is the *right* identity
    /// for IR value numbering (it is bitwise, not IEEE, equality).
    Float(u64),
    Str(SmolStr),
    Unit,
    /// An unresolved reference to a generator parameter (e.g. `N` in
    /// `SIMD[T, N: usize]`) -- only becomes a concrete `Int`/`Bool`/etc.
    /// during elaboration (`codira_comptime`). See architecture doc §3.
    ParamRef(SmolStr),
}

impl Attr {
    pub fn float(value: f64) -> Attr {
        Attr::Float(value.to_bits())
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Attr::Float(bits) => Some(f64::from_bits(*bits)),
            _ => None,
        }
    }
}

/// How a conversion behaves when the value does not fit the target type
/// (RFC-002 SS3.3).
///
/// `as` semantics are a **language design decision**, not an
/// implementation detail, and this axis is the reason the cast ops carry
/// a mode rather than being fixed to C-style truncation. A language that
/// is adding refinement types should not default to silent wrapping.
///
/// * [`CastMode::Wrapping`] -- two's-complement truncation / IEEE
///   round-to-nearest. Total: always produces a value. Foldable.
/// * [`CastMode::Checked`] -- emits a proof obligation. When `codira_smt`
///   proves the value is in range (e.g. `0 <= x <= 255` for `x as u8`), the
///   check **erases completely**; otherwise a runtime check is emitted. This
///   makes `as` the first real customer of refinement types instead of
///   something retrofitted around them.
///
/// Only `Wrapping` casts are foldable and e-graph-rewritable; `Checked`
/// casts carry an obligation and must not be reassociated away before it
/// is discharged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CastMode {
    Wrapping,
    Checked,
}

/// Which conversion a cast op performs. Separate variants rather than one
/// polymorphic `cast` because both e-graph pattern matching and SMT
/// encoding dispatch on the exact conversion (RFC-001 SS1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CastKind {
    /// Narrow an integer. SMT: `extract`.
    Trunc,
    /// Widen an integer, zero-filling. SMT: `zero_extend`.
    Zext,
    /// Widen an integer, sign-filling. SMT: `sign_extend`.
    Sext,
    /// Narrow a float. SMT: FP theory (`z3_fpa.h`).
    FpTrunc,
    /// Widen a float. SMT: FP theory.
    FpExt,
    /// Signed integer to float.
    SiToFp,
    /// Unsigned integer to float.
    UiToFp,
    /// Float to signed integer.
    FpToSi,
    /// Float to unsigned integer.
    FpToUi,
    /// Reinterpret the bits; operand and result must have equal width.
    Bitcast,
}

/// One IR operation. Grouped into "dialects" by variant name prefix in the
/// doc comment below, matching the table in the architecture doc §2.1.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OpKind {
    // ---- core.* (KGEN analog: concrete `kgen.*`/`pop.*` ops) ---------------
    /// `core.const` -- materializes a literal [`Attr`] as a value.
    Const(Attr),
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Neg,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Not,
    /// `core.bitnot` -- bitwise complement. Integers only; the logical
    /// negation of a `bool` is [`OpKind::Not`]. Keeping them distinct is
    /// what lets the verifier type-check both: one requires `Bool`, the
    /// other requires an integer type.
    BitNot,
    BitAnd,
    BitOr,
    BitXor,
    /// Left shift. Shift amounts >= 64 are an evaluation error, not UB --
    /// the comptime interpreter is the semantics reference.
    Shl,
    /// Arithmetic (sign-propagating) right shift, matching the `i64`
    /// interpretation of `Attr::Int`.
    Shr,

    /// `core.cast` -- an explicit conversion. One operand, result type on
    /// the op ([`Op::ty`]). See [`CastKind`] and [`CastMode`].
    ///
    /// Only `Wrapping` casts participate in folding and rewriting; a
    /// `Checked` cast carries an unmet proof obligation and is opaque
    /// until refinement checking discharges it.
    Cast(CastKind, CastMode),

    /// `core.tuple` -- packs its N operands into one multi-value. The only
    /// aggregate in the IR; loops with more than one loop-carried value
    /// produce one of these as their result (see [`OpKind::While`]).
    Tuple,
    /// `core.tuple_get(i)` -- projects element `i` out of a `Tuple` value.
    TupleGet(u32),

    /// `core.call(symbol)` -- calls another generator by symbol name, in
    /// the manner of KGEN's `#kgen.genref` symbol references: resolution
    /// happens against a [`crate::GeneratorStore`] at interpretation/
    /// elaboration/codegen time, not at IR-construction time. Operands are
    /// the runtime arguments (`OpKind::Arg` values in the callee).
    Call(SmolStr),

    // ---- param.* (KGEN analog: `kgen.param.*`) -----------------------------
    /// `param.ref` -- reference to a not-yet-resolved generator parameter
    /// (compile-time; substituted away by elaboration, see
    /// `codira_comptime`'s `elaborate` module and architecture doc §4).
    ParamRef(SmolStr),
    /// `core.arg` -- reference to the generator's Nth *runtime* argument
    /// (KGEN's distinction, `DesignOverview.md` "Generator/Function
    /// Arguments" vs. "Generator Parameter Arguments": arguments are SSA
    /// values computed at call time, parameters are compile-time-known).
    /// Elaboration passes these through unchanged -- specializing a
    /// generator's parameters must not, and cannot, eliminate genuine
    /// runtime inputs.
    Arg(u32),
    /// `cf.block_arg(i)` -- reference to argument `i` of the *innermost
    /// enclosing region* (see [`Region::num_args`]). This is MLIR's block
    /// argument: loop bodies receive the induction variable and the
    /// loop-carried values this way. Only valid inside a region whose
    /// `num_args > i`.
    BlockArg(u32),

    // ---- cf.* (KGEN analog: `hlcf.*`) --------------------------------------
    /// `cf.if` -- one operand (the condition), two regions (`then`, `else`).
    /// Both regions must be present; a missing `else` is represented as an
    /// empty region yielding `Attr::Unit`. Neither region takes block
    /// arguments. Each region's result is its last op (the original
    /// single-block-region convention), kept for compatibility with every
    /// existing consumer -- `If` predates `Yield` and does not require it.
    If,
    /// `cf.while` -- structured while-loop with loop-carried values, shaped
    /// like MLIR's `scf.while` collapsed to the common case:
    ///
    /// * operands: the initial values of the N loop-carried values
    /// * region 0 (`cond`): `num_args == N` (the current carried values); its
    ///   result op must evaluate to a bool
    /// * region 1 (`body`): `num_args == N`; its last op must be a
    ///   [`OpKind::Yield`] with exactly N operands (the next carried values)
    ///
    /// Result: the final carried values -- the value itself if N == 1, a
    /// `Tuple`-shaped multi-value if N > 1, `Unit` if N == 0.
    While,
    /// `cf.for` -- structured counted loop, shaped like MLIR's `scf.for`:
    ///
    /// * operands: `[start, end, step, init_0, .., init_{N-1}]` (so
    ///   `operands.len() == 3 + N`); iterates `iv` from `start` while `step > 0
    ///   ? iv < end : iv > end`, advancing by `step`
    /// * region 0 (`body`): `num_args == N + 1` -- block arg 0 is the induction
    ///   variable, args `1..=N` are the carried values; its last op must be a
    ///   [`OpKind::Yield`] with exactly N operands
    ///
    /// Result: as [`OpKind::While`] -- final carried values.
    For,
    /// `cf.yield` -- terminator of a loop body region, naming the next
    /// iteration's carried values as its operands. Only valid as the last
    /// op of a `While` body / `For` body region. Produces no value usable
    /// by later ops (there are none: it is last).
    Yield,
}

/// One operation: a kind, its SSA operands (other ops in the same [`Body`]),
/// and any nested [`Region`]s (e.g. `cf.if`'s `then`/`else` bodies).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Op {
    pub kind: OpKind,
    pub operands: SmallVec<[OpId; 2]>,
    pub regions: SmallVec<[Region; 0]>,
    /// The type of this op's *result* (RFC-001 SS1.3).
    ///
    /// Stored redundantly rather than derived, deliberately: the verifier
    /// re-derives it from the op kind and operand types and checks the
    /// two agree, which turns a bad rewrite into a verifier failure
    /// naming the exact op instead of a confusing LLVM-verifier failure
    /// after lowering. Costs 4 bytes per op (see `ty::TypeId`'s packed
    /// representation).
    ///
    /// [`TypeId::UNTYPED`] during the phase-1/2 migration means "not yet
    /// carrying a type"; the verifier skips type checking for those.
    pub ty: TypeId,
}

pub type OpId = Idx<Op>;

/// A straight-line sequence of [`Op`]s in SSA form: the arena is
/// append-only and operands may only reference earlier ops, so dominance
/// holds by construction. Its "result" (matching MLIR's
/// single-block-region-yields-last-value convention) is the value of its
/// last op, if any. Control flow is *structured*: it lives in region ops
/// ([`OpKind::If`]/[`OpKind::While`]/[`OpKind::For`]), never in branches
/// between blocks -- see the module doc.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Body {
    ops: Arena<Op>,
}

impl Body {
    pub fn new() -> Self {
        Self { ops: Arena::new() }
    }

    /// Appends an op with no type yet ([`TypeId::UNTYPED`]).
    ///
    /// Retained with its original signature so the phase-1 migration is
    /// non-breaking; use [`Body::push_typed`] for new code.
    pub fn push(&mut self, kind: OpKind, operands: impl IntoIterator<Item = OpId>) -> OpId {
        self.push_typed(kind, operands, TypeId::UNTYPED)
    }

    /// Appends an op carrying its result type.
    pub fn push_typed(
        &mut self,
        kind: OpKind,
        operands: impl IntoIterator<Item = OpId>,
        ty: TypeId,
    ) -> OpId {
        self.ops.alloc(Op {
            kind,
            operands: operands.into_iter().collect(),
            regions: SmallVec::new(),
            ty,
        })
    }

    pub fn push_with_regions(
        &mut self,
        kind: OpKind,
        operands: impl IntoIterator<Item = OpId>,
        regions: impl IntoIterator<Item = Region>,
    ) -> OpId {
        self.push_with_regions_typed(kind, operands, regions, TypeId::UNTYPED)
    }

    /// Appends a region-carrying op with its result type.
    pub fn push_with_regions_typed(
        &mut self,
        kind: OpKind,
        operands: impl IntoIterator<Item = OpId>,
        regions: impl IntoIterator<Item = Region>,
        ty: TypeId,
    ) -> OpId {
        self.ops.alloc(Op {
            kind,
            operands: operands.into_iter().collect(),
            regions: regions.into_iter().collect(),
            ty,
        })
    }

    /// Overwrites an op's result type. Used by `mir_lower` when the HIR
    /// type of an expression is known only after its operands are built.
    pub fn set_type(&mut self, id: OpId, ty: TypeId) {
        self.ops[id].ty = ty;
    }

    /// How many ops still carry [`TypeId::UNTYPED`] -- the phase-2
    /// migration metric (RFC-001 SS1.8).
    pub fn untyped_count(&self) -> usize {
        self.ops.iter().filter(|(_, op)| op.ty.is_untyped()).count()
    }

    pub fn get(&self, id: OpId) -> &Op {
        &self.ops[id]
    }

    /// The op whose value this body evaluates to: its last op, if any.
    pub fn result(&self) -> Option<OpId> {
        self.ops.iter().last().map(|(id, _)| id)
    }

    pub fn is_empty(&self) -> bool {
        self.ops.iter().next().is_none()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (OpId, &Op)> {
        self.ops.iter()
    }
}

/// A nested region: a [`Body`] plus the number of *block arguments* the
/// region receives from its enclosing op ([`OpKind::BlockArg`] references
/// them by index). `If` regions take 0; a `While`'s `cond`/`body` regions
/// take the number of loop-carried values; a `For` body takes carried
/// values + 1 (the induction variable). See each op's doc for the exact
/// contract, and `verify::verify_body` for the checked rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Region {
    pub num_args: u32,
    pub body: Body,
}

impl Region {
    /// A region with no block arguments (`If` branches, plain nesting).
    pub fn new(body: Body) -> Self {
        Self { num_args: 0, body }
    }

    /// A region receiving `num_args` block arguments (loop regions).
    pub fn with_args(num_args: u32, body: Body) -> Self {
        Self { num_args, body }
    }
}
