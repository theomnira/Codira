//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Compares the public API of two versions of a package and classifies every
//!   difference as a major, minor or patch change (`codira api-diff`), deciding
//!   contract changes by Z3 implication.
//!
//! # The rules
//!
//! An item is public when it is declared `public`. Items are matched by
//! their path (`geometry.area`, `Point`, a method as `Point.len`).
//!
//! A public generic function is API even though it never reaches the
//! library's exports (it has no code of its own): a caller in another module
//! compiles its instances from its source, so its signature and contract are
//! what callers depend on. Its own type parameters are compared by position
//! (renaming `T` to `U` is no change; adding one is a signature change), and a
//! clause about a value of a type-parameter type cannot be encoded, so a
//! change to it is undecided.
//!
//! | change | bump |
//! |---|---|
//! | public item removed | major |
//! | public item added | minor |
//! | parameter or return types, receiver or generic parameters changed | major |
//! | effect added to `uses` | major |
//! | effect removed from `uses` | minor |
//! | precondition strengthened (some input accepted before is refused now) | major |
//! | precondition weakened (accepts more inputs, refuses none it accepted) | minor |
//! | postcondition weakened (guarantees less, for some accepted input) | major |
//! | postcondition strengthened (guarantees more) | minor |
//! | clauses rewritten but equivalent | patch |
//! | inferred semantic envelope widened (a key moved to its allowing value) | major |
//! | inferred semantic envelope narrowed (a key moved to its forbidding value) | minor |
//! | struct fields, kind or value/reference changed | major |
//! | type alias retargeted, or its refinement changed in any way | major |
//! | anything else (bodies, private items) | patch |
//!
//! The semantic envelope (`lab/rfcs/0001-semantic-envelope.md`) is the one
//! the effect analysis *infers* for the function, through everything it
//! calls, private callees included: a body change that makes a public
//! function allocate, block, panic, run unboundedly, call foreign code,
//! start tasks, perform effects or handle `secret` data is a breaking change
//! for a caller that relied on it not doing so, even with the signature
//! untouched. Narrowing is minor, like a strengthened postcondition: the
//! function promises more and no caller can break. A key that changed in
//! both directions at once cannot happen (each key has two values). The
//! comparison is of the source-level analysis result only, with its
//! documented limits.
//!
//! "Precondition" is the conjunction of the `requires` clauses and the
//! parameters' refinement predicates; "postcondition" is the `ensures`
//! clauses and the return type's refinement. Both versions are encoded over
//! the same constants -- parameter `i` is `p<i>` whatever it is named,
//! `result` the return value -- with the verifier's machine-integer
//! semantics, and compared by implication:
//!
//! * the precondition is strengthened unless `old_pre => new_pre` is proven;
//! * the postcondition is weakened unless `old_pre && new_post => old_post` is
//!   proven: only inputs old callers could pass matter.
//!
//! A refuted implication comes with the input that shows it. When a clause
//! cannot be encoded (it mentions a value the verifier does not model) or Z3
//! runs out of its deterministic budget, the change is *undecided*: it is
//! reported as such and never classified by guessing. Contracts are only
//! compared when the signature is unchanged (a changed signature is already
//! major). An alias's refinement is used both by arguments and by results,
//! so any change to it is major; the functions using it are judged on their
//! own as well.

use std::collections::BTreeMap;

use codira_smt::{BoolExpr, CheckOutcome, Context, UnknownReason};
use codira_syntax::AstNode;

use super::{
    exec::{self, Sym},
    refinement, Value,
};
use crate::{
    code_model::{r#struct::StructKind, AssocItem},
    Effect, Function, HasSource, HasVisibility, HirDatabase, HirDisplay, Module, ModuleDef,
    Package, Struct, TypeAlias, Visibility,
};

/// How far a version number has to move for a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

impl std::fmt::Display for Bump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Bump::Patch => "patch",
            Bump::Minor => "minor",
            Bump::Major => "major",
        })
    }
}

/// One difference between the two APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiChange {
    /// The item's path, e.g. `parse` or `geometry.Point`.
    pub item: String,
    /// The bump it needs, or `None` when it could not be decided.
    pub bump: Option<Bump>,
    /// A phrase completing "the new version ...", such as "adds `Net` to
    /// `parse()`".
    pub summary: String,
    /// Supporting detail: the clauses, a witness input, why undecided.
    pub details: Vec<String>,
}

/// Every difference, in item order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiDiff {
    pub changes: Vec<ApiChange>,
}

impl ApiDiff {
    /// The smallest bump that covers every change. An undecided change
    /// might be breaking, so it needs a major bump.
    pub fn required(&self) -> Bump {
        self.changes
            .iter()
            .map(|c| c.bump.unwrap_or(Bump::Major))
            .max()
            .unwrap_or(Bump::Patch)
    }

    /// The changes that could not be decided.
    pub fn undecided(&self) -> impl Iterator<Item = &ApiChange> {
        self.changes.iter().filter(|c| c.bump.is_none())
    }

    /// The changes as one sentence: "adds `Net` to `parse()` and weakens
    /// the postcondition of `decode()`".
    pub fn sentence(&self) -> String {
        let parts: Vec<&str> = self
            .changes
            .iter()
            .filter(|c| c.bump != Some(Bump::Patch))
            .map(|c| c.summary.as_str())
            .collect();
        match parts.as_slice() {
            [] => "changes no public API".to_string(),
            [one] => (*one).to_string(),
            [init @ .., last] => format!("{} and {last}", init.join(", ")),
        }
    }
}

/// A public item of one version.
#[derive(Clone, Copy)]
enum Item {
    Function(Function),
    Struct(Struct),
    Alias(TypeAlias),
    Effect(Effect),
}

impl Item {
    fn noun(self) -> &'static str {
        match self {
            Item::Function(_) => "function",
            Item::Struct(_) => "type",
            Item::Alias(_) => "type alias",
            Item::Effect(_) => "effect",
        }
    }

    fn display(self, path: &str) -> String {
        match self {
            Item::Function(_) => format!("`{path}()`"),
            _ => format!("`{path}`"),
        }
    }
}

fn module_prefix(db: &dyn HirDatabase, module: Module) -> String {
    let mut names: Vec<String> = module
        .path_to_root(db)
        .into_iter()
        .filter_map(|m| m.name(db))
        .collect();
    names.reverse();
    names.join(".")
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn public_items(db: &dyn HirDatabase) -> BTreeMap<String, Item> {
    let mut items = BTreeMap::new();
    for package in Package::all(db) {
        for module in package.modules(db) {
            let prefix = module_prefix(db, module);
            for def in module.declarations(db) {
                let (name, item, public) = match def {
                    ModuleDef::Function(f) => (
                        f.name(db).to_string(),
                        Item::Function(f),
                        f.visibility(db) == Visibility::Public,
                    ),
                    ModuleDef::Struct(s) => (
                        s.name(db).to_string(),
                        Item::Struct(s),
                        s.visibility(db) == Visibility::Public,
                    ),
                    ModuleDef::TypeAlias(t) => (
                        t.name(db).to_string(),
                        Item::Alias(t),
                        t.visibility(db) == Visibility::Public,
                    ),
                    ModuleDef::Effect(e) => (
                        e.name(db).to_string(),
                        Item::Effect(e),
                        e.visibility(db) == Visibility::Public,
                    ),
                    _ => continue,
                };
                if public {
                    items.insert(join(&prefix, &name), item);
                }
            }
            for implementation in module.impls(db) {
                let owner = implementation.self_ty(db).display(db).to_string();
                for assoc in implementation.items(db) {
                    let AssocItem::Function(f) = assoc;
                    if f.visibility(db) == Visibility::Public {
                        let path = join(&prefix, &format!("{owner}.{}", f.name(db)));
                        items.insert(path, Item::Function(f));
                    }
                }
            }
        }
    }
    items
}

/// Compares the public API of `old` with that of `new`.
pub fn api_diff(old: &dyn HirDatabase, new: &dyn HirDatabase) -> ApiDiff {
    let old_items = public_items(old);
    let new_items = public_items(new);
    let mut changes = Vec::new();

    for (path, &item) in &old_items {
        let Some(&now) = new_items.get(path) else {
            changes.push(ApiChange {
                item: path.clone(),
                bump: Some(Bump::Major),
                summary: format!("removes the {} {}", item.noun(), item.display(path)),
                details: Vec::new(),
            });
            continue;
        };
        match (item, now) {
            (Item::Function(a), Item::Function(b)) => {
                compare_functions(old, a, new, b, path, &mut changes);
            }
            (Item::Struct(a), Item::Struct(b)) => {
                compare_structs(old, a, new, b, path, &mut changes);
            }
            (Item::Alias(a), Item::Alias(b)) => {
                compare_aliases(old, a, new, b, path, &mut changes);
            }
            (Item::Effect(a), Item::Effect(b)) => {
                let ops = |db: &dyn HirDatabase, e: Effect| {
                    let src = e.source(db);
                    normalize(&src.value.syntax().text().to_string())
                };
                if ops(old, a) != ops(new, b) {
                    changes.push(ApiChange {
                        item: path.clone(),
                        bump: Some(Bump::Major),
                        summary: format!("changes the operations of the effect `{path}`"),
                        details: Vec::new(),
                    });
                }
            }
            _ => changes.push(ApiChange {
                item: path.clone(),
                bump: Some(Bump::Major),
                summary: format!("turns the {} `{path}` into a {}", item.noun(), now.noun()),
                details: Vec::new(),
            }),
        }
    }
    for (path, &item) in &new_items {
        if !old_items.contains_key(path) {
            changes.push(ApiChange {
                item: path.clone(),
                bump: Some(Bump::Minor),
                summary: format!("adds the {} {}", item.noun(), item.display(path)),
                details: Vec::new(),
            });
        }
    }
    changes.sort_by(|a, b| a.item.cmp(&b.item));
    ApiDiff { changes }
}

/// Collapses whitespace, so a reformatted clause is the same clause.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` with each whole-word occurrence of `names[i]` replaced by `$i`, so
/// that renaming a type parameter changes nothing.
fn by_position(text: &str, names: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        match names.iter().position(|n| n == word) {
            Some(i) => out.push_str(&format!("${i}")),
            None => out.push_str(word),
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The parts of a signature a caller depends on. A generic function's own
/// type parameters are compared by position.
fn signature_shape(db: &dyn HirDatabase, f: Function) -> (Vec<String>, String, bool, usize) {
    let data = f.data(db);
    let names: Vec<String> = data
        .generic_params()
        .iter()
        .map(ToString::to_string)
        .collect();
    let params = f
        .params(db)
        .iter()
        .map(|p| by_position(&p.ty().display(db).to_string(), &names))
        .collect();
    (
        params,
        by_position(&f.ret_type(db).display(db).to_string(), &names),
        data.has_self_param(),
        names.len(),
    )
}

/// The signature as written, whitespace collapsed.
fn signature_text(db: &dyn HirDatabase, f: Function) -> String {
    let src = f.source(db);
    let range = src.value.signature_range();
    let start = src.value.syntax().text_range().start();
    let text = src.value.syntax().text().to_string();
    let from = usize::from(range.start() - start);
    let to = usize::from(range.end() - start).min(text.len());
    normalize(&text[from..to])
}

fn compare_functions(
    old: &dyn HirDatabase,
    a: Function,
    new: &dyn HirDatabase,
    b: Function,
    path: &str,
    changes: &mut Vec<ApiChange>,
) {
    let shown = format!("`{path}()`");
    if signature_shape(old, a) != signature_shape(new, b) {
        changes.push(ApiChange {
            item: path.to_string(),
            bump: Some(Bump::Major),
            summary: format!("changes the signature of {shown}"),
            details: vec![
                format!("was: {}", signature_text(old, a)),
                format!("now: {}", signature_text(new, b)),
            ],
        });
        return;
    }

    let effects = |db: &dyn HirDatabase, f: Function| -> Vec<String> {
        f.data(db)
            .effects()
            .iter()
            .map(ToString::to_string)
            .collect()
    };
    let (was, now) = (effects(old, a), effects(new, b));
    for effect in now.iter().filter(|e| !was.contains(e)) {
        changes.push(ApiChange {
            item: path.to_string(),
            bump: Some(Bump::Major),
            summary: format!("adds `{effect}` to {shown}"),
            details: vec!["callers must now handle or declare the effect".to_string()],
        });
    }
    for effect in was.iter().filter(|e| !now.contains(e)) {
        changes.push(ApiChange {
            item: path.to_string(),
            bump: Some(Bump::Minor),
            summary: format!("removes `{effect}` from {shown}"),
            details: Vec::new(),
        });
    }

    compare_envelopes(old, a, new, b, path, changes);
    compare_contracts(old, a, new, b, path, changes);
}

/// The inferred semantic envelopes of the two versions: widened keys are
/// one major change, narrowed keys one minor change.
fn compare_envelopes(
    old: &dyn HirDatabase,
    a: Function,
    new: &dyn HirDatabase,
    b: Function,
    path: &str,
    changes: &mut Vec<ApiChange>,
) {
    use crate::realtime::envelope::{compare_values, explain_key, inferred_values, Movement};

    let was = inferred_values(old, a);
    let now = inferred_values(new, b);
    let value = |side: &[(&'static str, &'static str)], key: &str| {
        side.iter()
            .find_map(|(k, v)| (*k == key).then_some(*v))
            .unwrap_or("(none)")
    };
    let moved = compare_values(&was, &now);
    let shown = format!("`{path}()`");
    for direction in [Movement::Widened, Movement::Narrowed] {
        let keys: Vec<&str> = moved
            .iter()
            .filter(|(_, m)| *m == direction)
            .map(|(k, _)| *k)
            .collect();
        if keys.is_empty() {
            continue;
        }
        let moves: Vec<String> = keys
            .iter()
            .map(|k| format!("{k}: {} -> {}", value(&was, k), value(&now, k)))
            .collect();
        let (bump, verb, mut details) = match direction {
            Movement::Widened => (
                Bump::Major,
                "widens",
                vec![
                    "callers that relied on the old envelope may break: the function may now \
                     do what it could not before"
                        .to_string(),
                ],
            ),
            Movement::Narrowed => (Bump::Minor, "narrows", Vec::new()),
        };
        if direction == Movement::Widened {
            for key in &keys {
                if let Some(because) = explain_key(new, b, key) {
                    details.push(format!("{key} is {} now: {because}", value(&now, key)));
                }
            }
        } else {
            for key in &keys {
                if let Some(because) = explain_key(old, a, key) {
                    details.push(format!("{key} was {}: {because}", value(&was, key)));
                }
            }
        }
        changes.push(ApiChange {
            item: path.to_string(),
            bump: Some(bump),
            summary: format!(
                "{verb} the semantic envelope of {shown} ({})",
                moves.join(", ")
            ),
            details,
        });
    }
}

/// The outcome of one implication check.
enum Decision {
    Proven,
    /// Refuted, with the inputs that show it.
    Refuted(String),
    Unknown(String),
}

fn decide<'ctx>(
    ctx: &'ctx Context,
    premise: BoolExpr<'ctx>,
    conclusion: BoolExpr<'ctx>,
    inputs: &[(String, Sym<'ctx>)],
) -> Decision {
    let solver = ctx.solver();
    solver.assert(premise);
    solver.assert(conclusion.not());
    match solver.check_detailed() {
        CheckOutcome::Unsat => Decision::Proven,
        CheckOutcome::Sat => {
            let witness = solver.model().map(|model| {
                inputs
                    .iter()
                    .filter_map(|(name, sym)| {
                        let value = match *sym {
                            Sym::Int(bv, ty) => Value::Int {
                                bits: model.eval_bv_bits(bv)?,
                                signed: ty.signedness.is_signed(),
                                width: bv.width(),
                            },
                            Sym::Bool(b) => Value::Bool(model.eval_bool(b)?),
                            Sym::Unit | Sym::Opaque => return None,
                        };
                        Some(format!("{name} = {value}"))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            });
            Decision::Refuted(witness.unwrap_or_default())
        }
        CheckOutcome::Unknown(UnknownReason::ResourceLimit) => {
            Decision::Unknown("Z3 ran out of its resource budget".to_string())
        }
        CheckOutcome::Unknown(UnknownReason::Other(reason)) => Decision::Unknown(reason),
    }
}

/// A list of clauses as one formula, or why it cannot be one.
fn conjunction<'ctx>(
    ctx: &'ctx Context,
    clauses: &[(String, Result<BoolExpr<'ctx>, String>)],
) -> Result<BoolExpr<'ctx>, String> {
    let mut all = Vec::with_capacity(clauses.len());
    for (text, clause) in clauses {
        match clause {
            Ok(b) => all.push(*b),
            Err(reason) => return Err(format!("`{text}`: {reason}")),
        }
    }
    Ok(BoolExpr::conjunction(ctx, &all))
}

fn texts(clauses: &[(String, Result<BoolExpr<'_>, String>)]) -> Vec<String> {
    let mut texts: Vec<String> = clauses.iter().map(|(t, _)| normalize(t)).collect();
    texts.sort();
    texts
}

fn shown_clauses(clauses: &[(String, Result<BoolExpr<'_>, String>)]) -> String {
    if clauses.is_empty() {
        "(none)".to_string()
    } else {
        clauses
            .iter()
            .map(|(t, _)| format!("`{}`", normalize(t)))
            .collect::<Vec<_>>()
            .join(" && ")
    }
}

fn compare_contracts(
    old: &dyn HirDatabase,
    a: Function,
    new: &dyn HirDatabase,
    b: Function,
    path: &str,
    changes: &mut Vec<ApiChange>,
) {
    let ctx = Context::new();
    let was = exec::contract_terms(old, a, &ctx);
    let now = exec::contract_terms(new, b, &ctx);
    let shown = format!("`{path}()`");
    let mut push = |bump: Option<Bump>, summary: String, details: Vec<String>| {
        changes.push(ApiChange {
            item: path.to_string(),
            bump,
            summary,
            details,
        });
    };

    // The parameters, named as the new version names them, over the shared
    // interface constants.
    let inputs: Vec<(String, Sym<'_>)> = b
        .params(new)
        .iter()
        .enumerate()
        .map(|(idx, param)| {
            let name = param
                .name(new)
                .map_or_else(|| format!("#{idx}"), |n| n.to_string());
            (
                name,
                exec::interface_value(new, &ctx, param.ty(), &format!("p{idx}")),
            )
        })
        .collect();
    let clause_detail = |label: &str, old_list, new_list| {
        vec![
            format!("{label} was: {}", shown_clauses(old_list)),
            format!("{label} now: {}", shown_clauses(new_list)),
        ]
    };

    // Preconditions.
    let old_pre = conjunction(&ctx, &was.requires);
    if texts(&was.requires) != texts(&now.requires) {
        let details = clause_detail("requires", &was.requires, &now.requires);
        match (old_pre.clone(), conjunction(&ctx, &now.requires)) {
            (Err(reason), _) | (_, Err(reason)) => push(
                None,
                format!("changes the precondition of {shown} (undecided)"),
                [details, vec![format!("cannot be compared: {reason}")]].concat(),
            ),
            (Ok(old_pre), Ok(new_pre)) => match decide(&ctx, old_pre, new_pre, &inputs) {
                Decision::Refuted(witness) => push(
                    Some(Bump::Major),
                    format!("strengthens the precondition of {shown}"),
                    [
                        details,
                        vec![format!("accepted before, refused now: {witness}")],
                    ]
                    .concat(),
                ),
                Decision::Unknown(reason) => push(
                    None,
                    format!("changes the precondition of {shown} (undecided)"),
                    [details, vec![format!("cannot be compared: {reason}")]].concat(),
                ),
                Decision::Proven => match decide(&ctx, new_pre, old_pre, &inputs) {
                    Decision::Proven => push(
                        Some(Bump::Patch),
                        format!("rewrites the precondition of {shown} equivalently"),
                        details,
                    ),
                    Decision::Refuted(witness) => push(
                        Some(Bump::Minor),
                        format!("weakens the precondition of {shown}"),
                        [details, vec![format!("newly accepted: {witness}")]].concat(),
                    ),
                    Decision::Unknown(reason) => push(
                        Some(Bump::Minor),
                        format!("weakens or rewrites the precondition of {shown}"),
                        [
                            details,
                            vec![format!(
                                "not strengthened (proven); whether it is weaker is unknown: \
                                 {reason}"
                            )],
                        ]
                        .concat(),
                    ),
                },
            },
        }
    }

    // Postconditions, for the inputs old callers can pass.
    if texts(&was.ensures) != texts(&now.ensures) {
        let details = clause_detail("ensures", &was.ensures, &now.ensures);
        let encoded = old_pre.clone().and_then(|pre| {
            Ok((
                pre,
                conjunction(&ctx, &was.ensures)?,
                conjunction(&ctx, &now.ensures)?,
            ))
        });
        let mut inputs = inputs.clone();
        inputs.push((
            "result".to_string(),
            exec::interface_value(new, &ctx, &b.ret_type(new), "result"),
        ));
        match encoded {
            Err(reason) => push(
                None,
                format!("changes the postcondition of {shown} (undecided)"),
                [details, vec![format!("cannot be compared: {reason}")]].concat(),
            ),
            Ok((pre, old_post, new_post)) => {
                match decide(&ctx, pre.and(new_post), old_post, &inputs) {
                    Decision::Refuted(witness) => push(
                        Some(Bump::Major),
                        format!("weakens the postcondition of {shown}"),
                        [
                            details,
                            vec![format!("allowed now, ruled out before: {witness}")],
                        ]
                        .concat(),
                    ),
                    Decision::Unknown(reason) => push(
                        None,
                        format!("changes the postcondition of {shown} (undecided)"),
                        [details, vec![format!("cannot be compared: {reason}")]].concat(),
                    ),
                    Decision::Proven => match decide(&ctx, pre.and(old_post), new_post, &inputs) {
                        Decision::Proven => push(
                            Some(Bump::Patch),
                            format!("rewrites the postcondition of {shown} equivalently"),
                            details,
                        ),
                        Decision::Refuted(_) => push(
                            Some(Bump::Minor),
                            format!("strengthens the postcondition of {shown}"),
                            details,
                        ),
                        Decision::Unknown(reason) => push(
                            Some(Bump::Minor),
                            format!("strengthens or rewrites the postcondition of {shown}"),
                            [
                                details,
                                vec![format!(
                                    "not weakened (proven); whether it is stronger is unknown: \
                                     {reason}"
                                )],
                            ]
                            .concat(),
                        ),
                    },
                }
            }
        }
    }
}

fn compare_structs(
    old: &dyn HirDatabase,
    a: Struct,
    new: &dyn HirDatabase,
    b: Struct,
    path: &str,
    changes: &mut Vec<ApiChange>,
) {
    let shape = |db: &dyn HirDatabase, s: Struct| {
        let data = s.data(db);
        let kind = match data.kind {
            StructKind::Record => "record",
            StructKind::Tuple => "tuple",
            StructKind::Unit => "unit",
        };
        let fields: Vec<String> = s
            .fields(db)
            .into_iter()
            .map(|f| format!("{}: {}", f.name(db), f.ty(db).display(db)))
            .collect();
        (
            kind,
            format!("{:?}", data.memory_kind),
            data.generic_params.len(),
            fields,
        )
    };
    let (was, now) = (shape(old, a), shape(new, b));
    if was != now {
        changes.push(ApiChange {
            item: path.to_string(),
            bump: Some(Bump::Major),
            summary: format!("changes the layout of `{path}`"),
            details: vec![
                format!("fields were: {}", was.3.join(", ")),
                format!("fields now: {}", now.3.join(", ")),
            ],
        });
    }
}

fn compare_aliases(
    old: &dyn HirDatabase,
    a: TypeAlias,
    new: &dyn HirDatabase,
    b: TypeAlias,
    path: &str,
    changes: &mut Vec<ApiChange>,
) {
    let (old_ty, new_ty) = (a.target_type(old), b.target_type(new));
    let (old_target, new_target) = (
        old_ty.display(old).to_string(),
        new_ty.display(new).to_string(),
    );
    if old_target != new_target {
        changes.push(ApiChange {
            item: path.to_string(),
            bump: Some(Bump::Major),
            summary: format!("retargets the type alias `{path}`"),
            details: vec![format!("was `{old_target}`, now `{new_target}`")],
        });
        return;
    }
    let old_ref = refinement::alias_refinement(old, a, 0);
    let new_ref = refinement::alias_refinement(new, b, 0);
    let text = |r: &Option<refinement::Refinement>| r.as_ref().map(|r| normalize(&r.text));
    if text(&old_ref) == text(&new_ref) {
        return;
    }
    let details = vec![
        format!(
            "refinement was: {}",
            text(&old_ref).unwrap_or_else(|| "(none)".to_string())
        ),
        format!(
            "refinement now: {}",
            text(&new_ref).unwrap_or_else(|| "(none)".to_string())
        ),
    ];
    let ctx = Context::new();
    let value = exec::interface_value(new, &ctx, &new_ty, "v");
    let encode = |r: &Option<refinement::Refinement>| match r {
        None => Ok(ctx.bool_true()),
        Some(r) => refinement::encode(&ctx, r, value),
    };
    match (encode(&old_ref), encode(&new_ref)) {
        (Ok(was), Ok(now)) => {
            let inputs = [("v".to_string(), value)];
            let both = (
                decide(&ctx, was, now, &inputs),
                decide(&ctx, now, was, &inputs),
            );
            match both {
                (Decision::Proven, Decision::Proven) => changes.push(ApiChange {
                    item: path.to_string(),
                    bump: Some(Bump::Patch),
                    summary: format!("rewrites the refinement of `{path}` equivalently"),
                    details,
                }),
                (Decision::Unknown(reason), _) | (_, Decision::Unknown(reason)) => {
                    changes.push(ApiChange {
                        item: path.to_string(),
                        bump: None,
                        summary: format!("changes the refinement of `{path}` (undecided)"),
                        details: [details, vec![format!("cannot be compared: {reason}")]].concat(),
                    });
                }
                _ => changes.push(ApiChange {
                    item: path.to_string(),
                    bump: Some(Bump::Major),
                    summary: format!("changes the refinement of `{path}`"),
                    details: [
                        details,
                        vec![
                            "an alias constrains arguments and results alike, so any change \
                             can break a caller"
                                .to_string(),
                        ],
                    ]
                    .concat(),
                }),
            }
        }
        (Err(reason), _) | (_, Err(reason)) => changes.push(ApiChange {
            item: path.to_string(),
            bump: None,
            summary: format!("changes the refinement of `{path}` (undecided)"),
            details: [details, vec![format!("cannot be compared: {reason}")]].concat(),
        }),
    }
}

#[cfg(test)]
mod tests;
