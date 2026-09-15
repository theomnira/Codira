//! Concrete compile-time values and the evaluation environment.

use rustc_hash::FxHashMap;
use smol_str::SmolStr;

/// A concrete value produced by interpreting a `codira_mir` region.
///
/// Distinct from `codira_mir::Attr`: an `Attr` may still contain an
/// unresolved `ParamRef`; a `Value` is always fully concrete. Elaboration
/// (see `Env`) is exactly the process of turning `Attr`s into `Value`s.
/// One extra shape exists here that `Attr` deliberately lacks:
/// [`Value::Tuple`], the interpreter-side form of `core.tuple` -- tuples
/// are aggregates of SSA values, not attribute-level constants, so they
/// have no `Attr` counterpart (see `codira_mir::fold`'s `NotFoldable`
/// convention and `elaborate`'s "tuples stay unfolded" note).
///
/// # Equality and hashing
///
/// `Value` is a salsa query key in `codira_hir` (`elaborate_generator`'s
/// `bindings` argument), so it must be `Eq + Hash` even though it now
/// carries an `f64`. The implementations below therefore use **bitwise**
/// float identity (`f64::to_bits`), mirroring `codira_mir::Attr::Float`'s
/// documented rationale: bitwise identity is the right notion for IR value
/// numbering and cache keys (two NaNs with the same bit pattern are the
/// same *value*, and `-0.0`/`0.0` are different ones). IEEE `==` semantics
/// (`NaN != NaN`) belong to the interpreted `core.eq` op -- see
/// `codira_mir::fold::fold_op` -- never to Rust-level `Value == Value`.
#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Bool(bool),
    Float(f64),
    Str(SmolStr),
    Tuple(Vec<Value>),
    Unit,
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            // Bitwise, not IEEE -- see the type-level doc comment.
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Tuple(a), Value::Tuple(b)) => a == b,
            (Value::Unit, Value::Unit) => true,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl std::hash::Hash for Value {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
        match self {
            Value::Int(v) => v.hash(state),
            Value::Bool(v) => v.hash(state),
            // Consistent with the bitwise `PartialEq` above (`a == b`
            // implies `hash(a) == hash(b)` holds by construction).
            Value::Float(v) => v.to_bits().hash(state),
            Value::Str(v) => v.hash(state),
            Value::Tuple(v) => v.hash(state),
            Value::Unit => {}
        }
    }
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Bool(_) => "bool",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Tuple(_) => "tuple",
            Value::Unit => "unit",
        }
    }

    pub fn as_int(&self) -> Result<i64, EvalError> {
        match self {
            Value::Int(v) => Ok(*v),
            other => Err(EvalError::TypeMismatch {
                expected: "int",
                found: other.type_name(),
            }),
        }
    }

    pub fn as_bool(&self) -> Result<bool, EvalError> {
        match self {
            Value::Bool(v) => Ok(*v),
            other => Err(EvalError::TypeMismatch {
                expected: "bool",
                found: other.type_name(),
            }),
        }
    }
}

/// Bindings for `param.ref`/`ParamRef` names during elaboration -- e.g. `N
/// -> Value::Int(4)` when elaborating `SIMD[f32, 4]`'s generator with `N`
/// bound to `4`. Empty for plain `comptime { .. }` blocks, which reference
/// no generator parameters.
///
/// One `Env` is shared across `core.call` boundaries during
/// interpretation: generator parameter bindings are module-level comptime
/// bindings (KGEN's parameters are resolved per-specialization, but a
/// callee's *runtime* inputs travel as `core.arg` values, never through
/// the env) -- see `interp`'s `OpKind::Call` handling.
#[derive(Debug, Clone, Default)]
pub struct Env {
    bindings: FxHashMap<SmolStr, Value>,
}

impl Env {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&mut self, name: impl Into<SmolStr>, value: Value) -> &mut Self {
        self.bindings.insert(name.into(), value);
        self
    }

    pub fn lookup(&self, name: &str) -> Result<Value, EvalError> {
        self.bindings
            .get(name)
            .cloned()
            .ok_or_else(|| EvalError::UnresolvedParam(name.into()))
    }
}

/// Everything that can go wrong evaluating a `codira_mir` region at compile
/// time. Deliberately does not have a catch-all `Other(String)` variant --
/// every failure mode this interpreter can hit is enumerated, so callers
/// (and tests) can match exhaustively instead of string-matching messages.
///
/// Pure-op failures (division by zero, shift out of range, operand type
/// mismatches *inside* `fold_op`) arrive wrapped in [`EvalError::Fold`]:
/// the interpreter delegates all pure-op semantics to
/// `codira_mir::fold::fold_op` (the single source of truth -- see that
/// module's doc), so its error type is embedded rather than re-enumerated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvalError {
    #[error("type mismatch: expected {expected}, found {found}")]
    TypeMismatch {
        expected: &'static str,
        found: &'static str,
    },
    #[error("unresolved parameter reference: {0}")]
    UnresolvedParam(SmolStr),
    #[error(transparent)]
    Fold(#[from] codira_mir::FoldError),
    #[error("empty region has no result value")]
    EmptyRegion,
    #[error("`cf.if` requires exactly one condition operand and two regions (then, else)")]
    MalformedIf,
    #[error("malformed loop: {0}")]
    MalformedLoop(&'static str),
    #[error("tuple projection out of range or applied to a non-tuple")]
    MalformedTuple,
    #[error("comptime evaluation fuel exhausted (runaway loop or recursion?)")]
    FuelExhausted,
    #[error("comptime call depth exceeded the limit of {limit} nested calls")]
    CallDepthExceeded { limit: usize },
    #[error("`core.call` references unknown generator `{0}` (or no GeneratorStore was provided)")]
    UnknownCallee(SmolStr),
    #[error("argument #{0} is a genuine runtime value, not a compile-time constant")]
    RuntimeArgument(u32),
}
