//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: September 17, 2026
//!
//! Lowering and static validation of `@heal(...)` healing contracts.
//!
//! `spec/HERACLES_Codira_Implementation.md` §1 states that Codira "makes
//! self-healing a first-class language feature rather than a library
//! retrofit", and §2.2 that "strategy feasibility is checked statically
//! (`ReturnDefault` requires a default value; `RetryWithBackoff` requires
//! idempotence proofs; `PropagateToParent` requires a non-root node in the
//! supervisor tree)".
//!
//! Before this module, none of that happened. `@heal(...)` parsed, its
//! `postcondition:` was checked for satisfiability (`crate::heal_check`),
//! and then the whole annotation was **discarded** -- `on:` and
//! `strategies:` were never even read. A contract naming a misspelled fault
//! class, an empty strategy list, or a strategy the function cannot possibly
//! support compiled without complaint and then silently failed to heal
//! anything at runtime. For a feature whose entire purpose is that a fault
//! *is* recovered, a silently inert contract is the worst available
//! outcome, so every check here exists to turn one of those silent failures
//! into a diagnostic.
//!
//! # Relationship to the runtime
//!
//! `codira_healing::contract::HealingContract` is the runtime-side
//! representation of the same (F, S, ψ) triple, and its doc comment says
//! "the compiler resolves these from source annotations and the engine
//! consumes them at runtime". [`HealContract`] is that compiler-side
//! resolution: the structured, validated form a backend can hand to the
//! engine instead of the engine's contracts having to be written by hand in
//! Rust.
//!
//! The two vocabularies are deliberately separate types rather than a
//! shared crate. `codira_hir` does not depend on `codira_healing` (the
//! dependency runs the other way through `codira_runtime`), and the runtime
//! type carries `&'static str`s and boxed closures that only make sense
//! once there is a loaded assembly to point into.
//!
//! # What is checked, and what is not
//!
//! Checked here:
//!
//! * `on:` names one or more *known* fault classes. An unrecognised bare name
//!   is an error with a nearest-match suggestion, because the alternative --
//!   silently treating it as a user-defined class -- means a typo produces a
//!   contract that never fires. `Custom("name")` is the explicit escape hatch
//!   for a genuinely user-defined class.
//! * `strategies:` is present, non-empty, and free of duplicates.
//! * Per-strategy feasibility, to the extent the type system can support it
//!   today (see [`Strategy::feasibility_requirement`]).
//!
//! Not checked here, and deliberately so:
//!
//! * Whether the function body *establishes* the postcondition. That is full
//!   Hoare-logic verification; `crate::heal_check` checks the weaker and
//!   still-useful property that the postcondition is satisfiable at all.
//! * Whether `SubstituteAlternate`'s target is *refinement-equivalent* to the
//!   original. Signature compatibility is checked; equivalence needs the
//!   SMT-backed proof obligation described in §2.2's "Type safety: Dependent
//!   (SMT-checked)" row, which is its own piece of work.
//! * `ReturnDefault`'s "requires a default value" beyond rejecting `Never`.
//!   There is no `Defaultable` conformance in the type system yet, so a deeper
//!   claim here would be a check in name only.

use codira_syntax::{
    ast::{self, ArgListOwner, AstNode, AttributeOwner},
    SyntaxNodePtr,
};

use crate::{
    code_model::src::HasSource,
    diagnostics::{
        DiagnosticSink, HealContractEmptyStrategies, HealContractInfeasibleStrategy,
        HealContractUnknownFaultClass, HealContractUnknownStrategy,
    },
    Function, HirDatabase,
};

/// A fault class a guarded site may encounter.
///
/// Mirrors `codira_healing::contract::FaultClass`. Kept as its own type for
/// the layering reason in the module doc comment; the names are identical so
/// a backend translation is a one-to-one match rather than a mapping table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultClass {
    NullDereference,
    OutOfBounds,
    Timeout,
    ConnectionRefused,
    OutOfMemory,
    MemoryLeak,
    AccessViolation,
    ArithmeticFault,
    /// A user-defined class, written explicitly as `Custom("name")`.
    Custom(String),
}

impl FaultClass {
    /// The built-in class names, in the order
    /// `codira_healing::contract::FaultClass::ALL` uses.
    pub const BUILTIN_NAMES: &'static [&'static str] = &[
        "NullDereference",
        "OutOfBounds",
        "Timeout",
        "ConnectionRefused",
        "OutOfMemory",
        "MemoryLeak",
        "AccessViolation",
        "ArithmeticFault",
    ];

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "NullDereference" => FaultClass::NullDereference,
            "OutOfBounds" => FaultClass::OutOfBounds,
            "Timeout" => FaultClass::Timeout,
            "ConnectionRefused" => FaultClass::ConnectionRefused,
            "OutOfMemory" => FaultClass::OutOfMemory,
            "MemoryLeak" => FaultClass::MemoryLeak,
            "AccessViolation" => FaultClass::AccessViolation,
            "ArithmeticFault" => FaultClass::ArithmeticFault,
            _ => return None,
        })
    }
}

/// A recovery strategy, mirroring
/// `codira_healing::contract::HealingStrategy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strategy {
    /// `ReturnDefault` returns the return type's zero value;
    /// `ReturnDefault(n)` returns `n`, which is how
    /// `spec/HERACLES_Codira_Implementation.md` section 2.2's own example
    /// writes it. The explicit form fills
    /// `codira_healing::contract::HealingContract::default_value`.
    ReturnDefault(Option<i64>),
    ReturnCached,
    RetryWithBackoff(u32),
    SubstituteAlternate(String),
    DegradeGracefully,
    IsolateAndRestart,
    PropagateToParent,
}

/// What a strategy needs from its guarded function in order to be usable.
///
/// This is the §2.2 feasibility table made explicit. Each variant names a
/// concrete, checkable obligation; a strategy whose obligation cannot be
/// discharged is rejected at compile time rather than failing at the moment
/// a fault actually occurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Nothing beyond being written down.
    None,
    /// The return type must have a default value, so `Never` is rejected.
    DefaultableReturn,
    /// Needs `@memoizable`, so the engine may cache the last good result.
    Memoizable,
    /// Needs `@idempotent(n)` with `n` at least the retry count, since a
    /// retry re-runs observable effects.
    Idempotent(u32),
    /// Needs `@snapshot`, so module state can be restored on restart.
    Snapshot,
    /// Needs a `supervisor` block to escalate into.
    Supervisor,
    /// Needs the named alternate to resolve to a signature-compatible
    /// function.
    ResolvableAlternate,
}

impl Strategy {
    /// The strategy names accepted in a `strategies:` list.
    pub const NAMES: &'static [&'static str] = &[
        "ReturnDefault",
        "ReturnCached",
        "RetryWithBackoff",
        "SubstituteAlternate",
        "DegradeGracefully",
        "IsolateAndRestart",
        "PropagateToParent",
    ];

    /// This strategy's feasibility obligation, per spec §2.2.
    ///
    /// The `match` is a *table*: two strategies needing nothing from the
    /// function reach `Requirement::None` for entirely different reasons
    /// (an explicit default is already in the contract; degrading just
    /// stops calling the function), and merging the arms would erase which
    /// case is which. Same reasoning as
    /// `codira_healing::contract`'s own `match_same_arms` allow.
    #[allow(clippy::match_same_arms)]
    pub fn feasibility_requirement(&self) -> Requirement {
        match self {
            // An explicitly written default needs nothing from the return
            // type: the value is right there in the contract.
            Strategy::ReturnDefault(Some(_)) => Requirement::None,
            Strategy::ReturnDefault(None) => Requirement::DefaultableReturn,
            Strategy::ReturnCached => Requirement::Memoizable,
            Strategy::RetryWithBackoff(n) => Requirement::Idempotent(*n),
            Strategy::SubstituteAlternate(_) => Requirement::ResolvableAlternate,
            // Disabling a feature needs no capability from the function; the
            // engine simply stops calling it.
            Strategy::DegradeGracefully => Requirement::None,
            Strategy::IsolateAndRestart => Requirement::Snapshot,
            Strategy::PropagateToParent => Requirement::Supervisor,
        }
    }

    /// The name as written, for diagnostics.
    pub fn name(&self) -> &'static str {
        match self {
            Strategy::ReturnDefault(_) => "ReturnDefault",
            Strategy::ReturnCached => "ReturnCached",
            Strategy::RetryWithBackoff(_) => "RetryWithBackoff",
            Strategy::SubstituteAlternate(_) => "SubstituteAlternate",
            Strategy::DegradeGracefully => "DegradeGracefully",
            Strategy::IsolateAndRestart => "IsolateAndRestart",
            Strategy::PropagateToParent => "PropagateToParent",
        }
    }
}

/// A `@heal(...)` contract, resolved from source and validated.
///
/// This is the (F, S, ψ) triple of `spec/self_healing_programming_language.md`
/// §3.2, in the form a backend can consume.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HealContract {
    /// The fault classes this site guards. Empty means "every class", which
    /// matches `codira_healing::contract::HealingContract::guards`.
    pub fault_classes: Vec<FaultClass>,
    /// The ordered recovery strategies, preferred first. Validated
    /// non-empty.
    pub strategies: Vec<Strategy>,
    /// Whether a `postcondition:` was written. The predicate itself stays in
    /// the syntax tree; `crate::heal_check` is what evaluates it.
    pub has_postcondition: bool,
}

/// The capabilities a guarded function declares through companion
/// attributes, used to discharge strategy obligations.
///
/// These are read from the same attribute list as `@heal` itself, so a
/// contract and the proofs it relies on sit together at the declaration
/// rather than being configured elsewhere.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Capabilities {
    memoizable: bool,
    snapshot: bool,
    /// From `@idempotent(n)`; `0` means not declared idempotent at all.
    idempotent_up_to: u32,
}

/// Lowers and validates the `@heal(...)` contract on `func`, pushing a
/// diagnostic for every problem found.
///
/// Returns the contract when one is present and well-formed enough to
/// describe, so a caller that wants the contract (rather than just its
/// diagnostics) gets the best available reading. Returns `None` when there
/// is no `@heal` attribute at all.
pub(crate) fn heal_contract_diagnostics(
    db: &dyn HirDatabase,
    func: Function,
    sink: &mut DiagnosticSink<'_>,
) -> Option<HealContract> {
    let src = func.source(db);
    let file = src.file_id;
    let ast_func = &src.value;

    let heal_attr = ast_func
        .attribute_list()?
        .attributes()
        .find(|attr| attribute_name(attr).as_deref() == Some("heal"))?;

    let capabilities = read_capabilities(ast_func);
    let has_supervisor = file_declares_supervisor(db, func);

    let mut contract = HealContract::default();
    let Some(arg_list) = heal_attr.arg_list() else {
        // `@heal` with no arguments at all: no strategies, which is the
        // same defect as an empty list and gets the same diagnostic.
        sink.push(HealContractEmptyStrategies {
            file,
            attr: SyntaxNodePtr::new(heal_attr.syntax()),
        });
        return Some(contract);
    };

    contract.has_postcondition = named_arg(&arg_list, "postcondition").is_some();

    // --- on: ------------------------------------------------------------
    if let Some(on) = named_arg(&arg_list, "on") {
        for element in list_elements(&on) {
            let ptr = SyntaxNodePtr::new(element.syntax());
            match parse_fault_class(&element) {
                Some(class) => contract.fault_classes.push(class),
                None => sink.push(HealContractUnknownFaultClass {
                    file,
                    expr: ptr,
                    written: element.syntax().text().to_string(),
                    suggestion: nearest(
                        &element.syntax().text().to_string(),
                        FaultClass::BUILTIN_NAMES,
                    ),
                }),
            }
        }
    }

    // --- strategies: ----------------------------------------------------
    let strategies_arg = named_arg(&arg_list, "strategies");
    let Some(strategies) = strategies_arg else {
        sink.push(HealContractEmptyStrategies {
            file,
            attr: SyntaxNodePtr::new(heal_attr.syntax()),
        });
        return Some(contract);
    };

    let mut seen: Vec<&'static str> = Vec::new();
    let mut wrote_any = false;
    for element in list_elements(&strategies) {
        wrote_any = true;
        let ptr = SyntaxNodePtr::new(element.syntax());
        let written = element.syntax().text().to_string();
        let Some(strategy) = parse_strategy(&element) else {
            sink.push(HealContractUnknownStrategy {
                file,
                expr: ptr.clone(),
                written: written.clone(),
                suggestion: nearest(&written, Strategy::NAMES),
            });
            continue;
        };

        // A repeated strategy is never useful: the engine tries each in
        // order, so the second attempt does exactly what the first did.
        if seen.contains(&strategy.name()) {
            sink.push(HealContractInfeasibleStrategy {
                file,
                expr: ptr.clone(),
                strategy: strategy.name(),
                reason: format!(
                    "`{}` is already listed earlier in this contract",
                    strategy.name()
                ),
            });
            continue;
        }
        seen.push(strategy.name());

        if let Some(reason) = infeasible_reason(db, func, &strategy, capabilities, has_supervisor) {
            sink.push(HealContractInfeasibleStrategy {
                file,
                expr: ptr.clone(),
                strategy: strategy.name(),
                reason,
            });
            continue;
        }

        contract.strategies.push(strategy);
    }

    // Only report "no usable strategy" when nothing was *written*. If every
    // entry was written and individually rejected, the specific reasons are
    // already on screen and this would just double the error count -- for a
    // single-strategy contract it would be pure repetition.
    if contract.strategies.is_empty() && !wrote_any {
        sink.push(HealContractEmptyStrategies {
            file,
            attr: SyntaxNodePtr::new(heal_attr.syntax()),
        });
    }

    Some(contract)
}

/// Why `strategy` cannot be used on `func`, or `None` if it can.
fn infeasible_reason(
    db: &dyn HirDatabase,
    func: Function,
    strategy: &Strategy,
    capabilities: Capabilities,
    has_supervisor: bool,
) -> Option<String> {
    match strategy.feasibility_requirement() {
        Requirement::None => None,
        Requirement::DefaultableReturn => func.ret_type(db).is_never().then(|| {
            "the function returns `Never`, so there is no default value to return instead"
                .to_string()
        }),
        Requirement::Memoizable => (!capabilities.memoizable).then(|| {
            "caching the last good result needs `@memoizable` on this function, so the compiler \
             knows the result may be reused"
                .to_string()
        }),
        Requirement::Idempotent(n) => {
            if capabilities.idempotent_up_to == 0 {
                Some(format!(
                    "retrying re-runs this function's effects, so it needs `@idempotent({n})` to \
                     declare that calling it {n} times is equivalent to calling it once"
                ))
            } else if capabilities.idempotent_up_to < n {
                Some(format!(
                    "this function declares `@idempotent({})`, which is fewer than the {n} \
                     attempts `RetryWithBackoff({n})` would make",
                    capabilities.idempotent_up_to
                ))
            } else {
                None
            }
        }
        Requirement::Snapshot => (!capabilities.snapshot).then(|| {
            "restarting needs `@snapshot` on this function, so there is committed state to \
             restore from"
                .to_string()
        }),
        Requirement::Supervisor => (!has_supervisor).then(|| {
            "there is no `supervisor` block in this file to escalate to, so this contract is the \
             root and has no parent"
                .to_string()
        }),
        Requirement::ResolvableAlternate => {
            let Strategy::SubstituteAlternate(name) = strategy else {
                return None;
            };
            resolve_alternate(db, func, name)
        }
    }
}

/// Checks that `name` refers to a function in the same module whose
/// signature matches `func`'s, returning the reason it does not.
///
/// Signature compatibility is the part that is genuinely checkable today.
/// Refinement-equivalence -- §2.2's "Type safety: Dependent (SMT-checked)"
/// -- is not attempted, and this function's diagnostic does not claim it is.
fn resolve_alternate(db: &dyn HirDatabase, func: Function, name: &str) -> Option<String> {
    let module = func.module(db);
    let candidate = module.all_functions(db).into_iter().find(|f| {
        f.name(db)
            .as_str()
            .is_some_and(|candidate_name| candidate_name == name)
    })?;

    if candidate == func {
        return Some(format!(
            "`{name}` is the guarded function itself, so substituting it would re-run the call \
             that just faulted"
        ));
    }

    let original = db.callable_sig(func.into());
    let alternate = db.callable_sig(candidate.into());
    if original.params() != alternate.params() || original.ret() != alternate.ret() {
        return Some(format!(
            "`{name}` does not have the same signature as this function, so it cannot stand in \
             for it"
        ));
    }

    None
}

/// Reads `@memoizable`, `@snapshot` and `@idempotent(n)` off the same
/// attribute list `@heal` is on.
fn read_capabilities(func: &ast::FunctionDef) -> Capabilities {
    let mut caps = Capabilities::default();
    let Some(list) = func.attribute_list() else {
        return caps;
    };

    for attr in list.attributes() {
        match attribute_name(&attr).as_deref() {
            Some("memoizable") => caps.memoizable = true,
            Some("snapshot") => caps.snapshot = true,
            Some("idempotent") => {
                // `@idempotent(n)`. A bare `@idempotent` declares no bound,
                // so it stays 0 and `RetryWithBackoff` remains infeasible --
                // which is the safe reading: an unbounded idempotence claim
                // is exactly the one not to take on faith.
                caps.idempotent_up_to = attr
                    .arg_list()
                    .and_then(|args| args.args().next())
                    .and_then(|arg| literal_u32(&arg))
                    .unwrap_or(0);
            }
            _ => {}
        }
    }
    caps
}

/// Whether the file declaring `func` contains a `supervisor` block.
///
/// File-scoped because that is the granularity `supervisor` blocks actually
/// have today -- they parse but are not lowered into HIR items (see
/// `crate::supervisor_validator`), so there is no supervisor *tree* to ask
/// about ancestry. This is the honest available approximation of §2.2's
/// "requires a non-root node in the supervisor tree": it catches the case
/// that matters in practice, a `PropagateToParent` with no supervisor
/// anywhere to propagate to.
fn file_declares_supervisor(db: &dyn HirDatabase, func: Function) -> bool {
    use codira_syntax::ast::ModuleItemOwner;

    db.parse(func.file_id(db))
        .tree()
        .items()
        .any(|item| matches!(item.kind(), ast::ModuleItemKind::SupervisorDef(_)))
}

/// The single-segment name of an attribute (`heal` for `@heal(..)`).
fn attribute_name(attr: &ast::Attribute) -> Option<String> {
    match attr.path()?.segment()?.kind()? {
        ast::PathSegmentKind::Name(name_ref) => Some(name_ref.text().to_string()),
        _ => None,
    }
}

/// The elements of a `[a, b, c]` array expression, or the single expression
/// itself when it is not an array.
///
/// Accepting a bare expression matters for ergonomics: `on: Timeout` reads
/// better than `on: [Timeout]` for the common single-class case, and there
/// is no ambiguity to resolve.
fn list_elements(expr: &ast::Expr) -> Vec<ast::Expr> {
    match expr.kind() {
        ast::ExprKind::ArrayExpr(array) => array.exprs().collect(),
        _ => vec![expr.clone()],
    }
}

/// Parses one `on:` element into a fault class.
fn parse_fault_class(expr: &ast::Expr) -> Option<FaultClass> {
    // `Custom("name")` -- the explicit escape hatch.
    if let ast::ExprKind::CallExpr(call) = expr.kind() {
        if callee_name(&call).as_deref() != Some("Custom") {
            return None;
        }
        let name = call
            .arg_list()?
            .args()
            .next()
            .and_then(|arg| string_literal(&arg))?;
        return Some(FaultClass::Custom(name));
    }
    FaultClass::from_name(&path_name(expr)?)
}

/// Parses one `strategies:` element into a strategy.
fn parse_strategy(expr: &ast::Expr) -> Option<Strategy> {
    if let ast::ExprKind::CallExpr(call) = expr.kind() {
        let name = callee_name(&call)?;
        let mut args = call.arg_list()?.args();
        return match name.as_str() {
            // `ReturnDefault(0)` -- the spec section 2.2 spelling.
            "ReturnDefault" => Some(Strategy::ReturnDefault(Some(literal_i64(&args.next()?)?))),
            "RetryWithBackoff" => Some(Strategy::RetryWithBackoff(literal_u32(&args.next()?)?)),
            "SubstituteAlternate" => {
                let arg = args.next()?;
                // Accepts both `SubstituteAlternate(fallback)` (a path, as
                // the spec's examples write it) and
                // `SubstituteAlternate("fallback")`.
                let name = path_name(&arg).or_else(|| string_literal(&arg))?;
                Some(Strategy::SubstituteAlternate(name))
            }
            _ => None,
        };
    }

    Some(match path_name(expr)?.as_str() {
        "ReturnDefault" => Strategy::ReturnDefault(None),
        "ReturnCached" => Strategy::ReturnCached,
        "DegradeGracefully" => Strategy::DegradeGracefully,
        "IsolateAndRestart" => Strategy::IsolateAndRestart,
        "PropagateToParent" => Strategy::PropagateToParent,
        // The parameterized strategies are only meaningful with their
        // argument, so a bare mention is a mistake worth reporting rather
        // than silently defaulting.
        _ => return None,
    })
}

/// The name of a single-segment path expression.
fn path_name(expr: &ast::Expr) -> Option<String> {
    let ast::ExprKind::PathExpr(path_expr) = expr.kind() else {
        return None;
    };
    match path_expr.path()?.segment()?.kind()? {
        ast::PathSegmentKind::Name(name_ref) => Some(name_ref.text().to_string()),
        _ => None,
    }
}

/// The callee name of a call expression, when the callee is a bare path.
fn callee_name(call: &ast::CallExpr) -> Option<String> {
    path_name(&call.expr()?)
}

/// The value of a signed integer literal expression, allowing a leading
/// `-` so `ReturnDefault(-1)` works.
fn literal_i64(expr: &ast::Expr) -> Option<i64> {
    if let ast::ExprKind::PrefixExpr(prefix) = expr.kind() {
        if prefix.op_kind() == Some(ast::PrefixOp::Neg) {
            return literal_i64(&prefix.expr()?).map(|v| -v);
        }
        return None;
    }
    literal_u32(expr).map(i64::from)
}

/// The value of an unsigned integer literal expression.
fn literal_u32(expr: &ast::Expr) -> Option<u32> {
    let ast::ExprKind::Literal(literal) = expr.kind() else {
        return None;
    };
    let ast::LiteralKind::IntNumber(int) = literal.kind() else {
        return None;
    };
    let (text, _suffix) = int.split_into_parts();
    text.parse().ok()
}

/// The contents of a string literal expression, without its quotes.
fn string_literal(expr: &ast::Expr) -> Option<String> {
    let ast::ExprKind::Literal(literal) = expr.kind() else {
        return None;
    };
    let ast::LiteralKind::String(_) = literal.kind() else {
        return None;
    };
    let text = literal.syntax().text().to_string();
    Some(text.trim_matches('"').to_string())
}

/// The candidate from `options` closest to `written`, if any is close
/// enough to be worth suggesting.
///
/// Suggestions are what make a typo a two-second fix instead of a hunt
/// through the spec, and the whole reason an unknown bare name is an error
/// rather than being waved through as a user-defined class.
fn nearest(written: &str, options: &[&'static str]) -> Option<&'static str> {
    // A bare name (no `(..)`) is what a typo looks like; anything else is
    // structurally different and a name suggestion would be noise.
    let written = written.trim();
    options
        .iter()
        .map(|candidate| (edit_distance(written, candidate), *candidate))
        // Distance 3 keeps single-word typos and transpositions while
        // rejecting genuinely unrelated names.
        .filter(|(distance, _)| *distance <= 3)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance, case-insensitive.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];

    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = prev[j] + usize::from(ca != cb);
            curr[j + 1] = substitution.min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Finds the value `Expr` for a `name: value` labeled argument inside an
/// attribute's argument list.
///
/// `attribute_arg_list` does not wrap `name:` in a dedicated node -- it is a
/// loose `IDENT COLON` token pair immediately before the value expression --
/// so this walks the raw child sequence looking for that exact three-element
/// pattern rather than using a structured accessor that does not exist.
/// (Shared shape with `crate::heal_check::named_arg`; kept separate because
/// that module deliberately depends on nothing but the syntax tree.)
fn named_arg(arg_list: &ast::ArgList, name: &str) -> Option<ast::Expr> {
    let elements: Vec<_> = arg_list
        .syntax()
        .children_with_tokens()
        .filter(|el| !el.kind().is_trivia())
        .collect();

    for i in 0..elements.len() {
        let Some(label) = elements[i].as_token() else {
            continue;
        };
        if label.kind() != codira_syntax::SyntaxKind::IDENT || label.text() != name {
            continue;
        }
        if elements.get(i + 1).and_then(|el| el.as_token())?.kind() != codira_syntax::T![:] {
            continue;
        }
        if let Some(value) = elements.get(i + 2).and_then(|el| el.as_node()) {
            if let Some(expr) = ast::Expr::cast(value.clone()) {
                return Some(expr);
            }
        }
    }
    None
}
