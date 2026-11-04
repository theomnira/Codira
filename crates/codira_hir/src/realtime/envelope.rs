//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The Codira Semantic Envelope (CSE), version 0: `@envelope(..)` on a
//!   function declares what it may do, the effect analysis checks the
//!   declaration against everything the function calls, and
//!   [`envelope_report`] gives every function's *inferred* envelope as
//!   data, for `codira envelope`.
//! - Version 1: a package envelope (`[envelope]` in `codira.toml`,
//!   [`Envelope::from_pairs`]) that every `public` function must fit
//!   ([`package_envelope_violations`]); each function's inferred values for
//!   proof certificates ([`inferred_values`]); and the direction a value
//!   moved in between two versions, for `codira api-diff`
//!   ([`compare_values`]).
//!
//! Tier B (experimental) of the Frontier Systems Laboratory; the design,
//! prior art and limits are in `lab/rfcs/0001-semantic-envelope.md`.
//!
//! # What a clause means
//!
//! Each clause rules one [`Effect`] out (or, with the permissive value,
//! states that it is allowed, which checks nothing and documents intent):
//!
//! | clause                  | rules out                                     |
//! |-------------------------|-----------------------------------------------|
//! | `allocation: none`      | [`Effect::Alloc`] -- as `@noalloc`            |
//! | `blocking: false`       | [`Effect::Block`] -- as `@noblock`            |
//! | `panic: false`          | [`Effect::Panic`] -- as `@nopanic`            |
//! | `termination: bounded`  | [`Effect::Unbounded`]                         |
//! | `foreign: none`         | [`Effect::Foreign`]: `extern`, `@asm`, unknown calls |
//! | `concurrency: none`     | [`Effect::Spawn`]: `scope` and `spawn`        |
//! | `effects: none`         | [`Effect::Perform`]: `perform`                |
//! | `secrecy: none`         | [`Effect::Secret`]: any `secret` data         |
//!
//! The guarantee is the effect analysis's, with its documented limits
//! (stack overflow is not ruled out; `extern` declarations are trusted; a
//! host calling in through the runtime is trusted to meet `requires`). An
//! envelope is checked over *source*: it says nothing about what LLVM or the
//! linker produce, and nothing is enforced at run time.
//!
//! Keys from the CSE design that the compiler cannot check yet (`reads`,
//! `writes`, `persistence`, `capability`, `memory`, `fuel`, `latency`,
//! `energy`, `network`, `targets`) are *rejected* with an error, never
//! accepted and ignored: an unchecked promise in a checked position is worse
//! than none.

use codira_syntax::{
    ast::{self, AstNode},
    SyntaxKind,
};

use super::{Effect, EffectSet};
use crate::{code_model::Function, HasSource, HirDatabase, Package};

/// One checkable envelope key.
struct Key {
    name: &'static str,
    effect: Effect,
    /// The value that rules the effect out.
    forbid: &'static str,
    /// The value that allows it.
    allow: &'static str,
}

const KEYS: &[Key] = &[
    Key {
        name: "allocation",
        effect: Effect::Alloc,
        forbid: "none",
        allow: "any",
    },
    Key {
        name: "blocking",
        effect: Effect::Block,
        forbid: "false",
        allow: "true",
    },
    Key {
        name: "panic",
        effect: Effect::Panic,
        forbid: "false",
        allow: "true",
    },
    Key {
        name: "termination",
        effect: Effect::Unbounded,
        forbid: "bounded",
        allow: "any",
    },
    Key {
        name: "foreign",
        effect: Effect::Foreign,
        forbid: "none",
        allow: "any",
    },
    Key {
        name: "concurrency",
        effect: Effect::Spawn,
        forbid: "none",
        allow: "any",
    },
    Key {
        name: "effects",
        effect: Effect::Perform,
        forbid: "none",
        allow: "any",
    },
    Key {
        name: "secrecy",
        effect: Effect::Secret,
        forbid: "none",
        allow: "any",
    },
];

/// The names of the checked keys, in the order reports list them.
pub const KEY_NAMES: [&str; 8] = [
    "allocation",
    "blocking",
    "panic",
    "termination",
    "foreign",
    "concurrency",
    "effects",
    "secrecy",
];

/// Keys of the CSE design that have no checker yet, and what each would
/// need.
const UNCHECKED_KEYS: &[(&str, &str)] = &[
    ("reads", "a model of which state a function reads (no heap-region analysis exists)"),
    ("writes", "a model of which state a function writes (no heap-region analysis exists)"),
    ("persistence", "a persistence layer (there is no `std.db` yet)"),
    ("capability", "a capability model (the CHERI-style mode is experimental and unchecked)"),
    ("memory", "a stack and heap bound (the analysis does not bound stack use)"),
    ("fuel", "a static cost analysis (`--fuel` meters at run time only)"),
    ("latency", "a cycle model of the target CPU (see why `@deadline` is rejected)"),
    ("energy", "an energy model of the target, which the compiler does not have"),
    ("network", "a notion of network access (it is not distinguished from other `extern` calls; use `foreign: none`)"),
    ("targets", "target-set checking, which is not implemented"),
];

/// One clause of a parsed `@envelope`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clause {
    pub key: &'static str,
    pub value: String,
    pub effect: Effect,
    /// Whether the clause rules its effect out (as opposed to allowing it).
    pub forbids: bool,
}

/// A parsed `@envelope(..)` attribute: its clauses, and what was wrong with
/// the ones that could not be accepted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Envelope {
    pub clauses: Vec<Clause>,
    pub errors: Vec<String>,
}

impl Envelope {
    /// The effects the envelope rules out.
    pub fn forbidden(&self) -> EffectSet {
        self.clauses
            .iter()
            .filter(|c| c.forbids)
            .fold(EffectSet::NONE, |set, c| {
                set.union(EffectSet::of(&[c.effect]))
            })
    }

    /// "`@envelope(allocation: none, blocking: false)`": the clauses that
    /// rule out any of `effects`, or `None` when none do.
    pub fn name_forbidding(&self, effects: EffectSet) -> Option<String> {
        let clauses: Vec<String> = self
            .clauses
            .iter()
            .filter(|c| c.forbids && effects.contains(c.effect))
            .map(|c| format!("{}: {}", c.key, c.value))
            .collect();
        (!clauses.is_empty()).then(|| format!("@envelope({})", clauses.join(", ")))
    }

    /// Parses the arguments of an `@envelope` attribute.
    pub fn parse(attr: &ast::Attribute) -> Envelope {
        let mut envelope = Envelope::default();
        let Some(args) = attr.arg_list() else {
            envelope
                .errors
                .push("`@envelope` needs clauses, e.g. `@envelope(allocation: none)`".to_string());
            return envelope;
        };
        // The parser records `name:` as a loose IDENT and `:` ahead of each
        // argument expression (see `attribute_arg_list`).
        let mut pending: Option<String> = None;
        let mut seen: Vec<&'static str> = Vec::new();
        for element in args.syntax().children_with_tokens() {
            match element {
                codira_syntax::SyntaxElement::Token(token) => {
                    if token.kind() == SyntaxKind::IDENT {
                        pending = Some(token.text().to_string());
                    }
                }
                codira_syntax::SyntaxElement::Node(node) => {
                    let value: String = node
                        .text()
                        .to_string()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let Some(name) = pending.take() else {
                        envelope.errors.push(format!(
                            "`@envelope` clauses are written `key: value`; `{value}` has no key"
                        ));
                        continue;
                    };
                    envelope.clause(&name, value, &mut seen);
                }
            }
        }
        if envelope.clauses.is_empty() && envelope.errors.is_empty() {
            envelope
                .errors
                .push("`@envelope` needs clauses, e.g. `@envelope(allocation: none)`".to_string());
        }
        envelope
    }

    /// An envelope given as `key = value` pairs: the manifest's `[envelope]`
    /// table. Keys and values are exactly those of the attribute, and an
    /// unknown, unchecked, repeated or ill-valued key is an error in the
    /// same words, naming `[envelope]` instead of `@envelope`.
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Envelope {
        let mut envelope = Envelope::default();
        let mut seen: Vec<&'static str> = Vec::new();
        for (name, value) in pairs {
            envelope.clause_in("`[envelope]`", name, value.trim().to_string(), &mut seen);
        }
        if envelope.clauses.is_empty() && envelope.errors.is_empty() {
            envelope.errors.push(
                "`[envelope]` in codira.toml has no keys; give it some, e.g. \
                 `allocation = \"none\"`, or remove it"
                    .to_string(),
            );
        }
        envelope
    }

    fn clause(&mut self, name: &str, value: String, seen: &mut Vec<&'static str>) {
        self.clause_in("`@envelope`", name, value, seen);
    }

    /// Accepts or rejects one clause; `what` names where it was written.
    fn clause_in(&mut self, what: &str, name: &str, value: String, seen: &mut Vec<&'static str>) {
        if let Some(key) = KEYS.iter().find(|k| k.name == name) {
            if seen.contains(&key.name) {
                self.errors
                    .push(format!("{what} key `{name}` is given more than once"));
                return;
            }
            seen.push(key.name);
            let forbids = if value == key.forbid {
                true
            } else if value == key.allow {
                false
            } else {
                self.errors.push(format!(
                    "{what} key `{name}` takes `{}` or `{}`, not `{value}`",
                    key.forbid, key.allow
                ));
                return;
            };
            self.clauses.push(Clause {
                key: key.name,
                value,
                effect: key.effect,
                forbids,
            });
        } else if let Some((_, needs)) = UNCHECKED_KEYS.iter().find(|(k, _)| *k == name) {
            self.errors.push(format!(
                "{what} key `{name}` is part of the semantic-envelope design but cannot be \
                 checked yet: it needs {needs}. It is rejected rather than accepted unchecked"
            ));
        } else {
            self.errors.push(format!(
                "unknown {what} key `{name}`; the checked keys are {}",
                KEY_NAMES
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
}

/// Whether `attr` is `@envelope`.
pub(super) fn is_envelope(attr: &ast::Attribute) -> bool {
    attr.path().and_then(|p| p.segment()).is_some_and(
        |s| matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == "envelope"),
    )
}

// ---- the machine-readable report ----------------------------------------

/// One dimension of an inferred envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dimension {
    pub key: &'static str,
    /// The strongest value the analysis establishes: the key's forbidding
    /// value when the effect is absent, its allowing value otherwise.
    pub value: &'static str,
    /// When the effect is present: why, as the chain of calls down to its
    /// source.
    pub because: Option<String>,
}

/// A function's declared and inferred envelopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionEnvelope {
    pub function: Function,
    /// `file.code:12`, the line of the signature.
    pub location: String,
    pub name: String,
    pub is_extern: bool,
    /// The clauses of its `@envelope`, if it has one.
    pub declared: Option<Vec<Clause>>,
    /// Declared clauses the inferred envelope does not meet (empty for an
    /// `extern` function, whose declaration is trusted).
    pub violated: Vec<&'static str>,
    pub inferred: Vec<Dimension>,
}

/// The envelope of every function in every package of `db`, in source
/// order.
pub fn envelope_report(db: &dyn HirDatabase) -> Vec<FunctionEnvelope> {
    let mut functions: Vec<Function> = Package::all(db)
        .into_iter()
        .flat_map(|package| package.modules(db))
        .flat_map(|module| module.all_functions(db))
        .collect();
    functions.sort_by_key(|f| {
        (
            f.file_id(db),
            f.source(db).value.syntax().text_range().start(),
        )
    });
    functions
        .into_iter()
        .map(|function| function_envelope(db, function))
        .collect()
}

fn function_envelope(db: &dyn HirDatabase, function: Function) -> FunctionEnvelope {
    let src = function.source(db);
    let home = src.file_id;
    let line = db
        .line_index(home)
        .line_col(src.value.signature_range().start())
        .line;
    let location = format!("{}:{}", db.file_relative_path(home), line + 1);
    let summary = db.effect_summary(function);
    let inferred = KEYS
        .iter()
        .map(|key| {
            let present = summary.effects.contains(key.effect);
            Dimension {
                key: key.name,
                value: if present { key.allow } else { key.forbid },
                because: present.then(|| explain(db, home, &summary, key.effect)),
            }
        })
        .collect();
    let declared = super::Attributes::of(db, function)
        .envelope
        .map(|(e, _)| e.clauses);
    let is_extern = function.is_extern(db);
    let violated = match &declared {
        Some(clauses) if !is_extern => clauses
            .iter()
            .filter(|c| c.forbids && summary.effects.contains(c.effect))
            .map(|c| c.key)
            .collect(),
        _ => Vec::new(),
    };
    FunctionEnvelope {
        function,
        location,
        name: function.name(db).to_string(),
        is_extern,
        declared,
        violated,
        inferred,
    }
}

/// "calls `g` (line 4); `println` formats .. at line 9", or the source alone
/// when it is in the function itself.
fn explain(
    db: &dyn HirDatabase,
    home: codira_hir_input::FileId,
    summary: &super::EffectSummary,
    effect: Effect,
) -> String {
    let witness = summary.witness(effect);
    let source = summary
        .recursion
        .as_ref()
        .filter(|_| witness.is_none() && effect == Effect::Unbounded);
    match (witness, source) {
        (Some(w), _) => {
            let mut text = String::new();
            for call in &w.calls {
                text.push_str(&format!(
                    "calls `{}` ({}); ",
                    call.callee.name(db),
                    super::location(db, home, &call.site)
                ));
            }
            text.push_str(&format!(
                "{} at {}",
                w.source.what,
                super::location(db, home, &w.source.site)
            ));
            text
        }
        (None, Some(rec)) => format!("{} at {}", rec.what, super::location(db, home, &rec.site)),
        (None, None) => "no witness recorded".to_string(),
    }
}

// ---- version 1: values, package envelopes, comparisons --------------------

/// `function`'s inferred envelope as `(key, value)` pairs, one per checked
/// key in [`KEY_NAMES`] order: the value a proof certificate records and
/// `codira api-diff` compares.
pub fn inferred_values(
    db: &dyn HirDatabase,
    function: Function,
) -> Vec<(&'static str, &'static str)> {
    let summary = db.effect_summary(function);
    KEYS.iter()
        .map(|key| {
            let value = if summary.effects.contains(key.effect) {
                key.allow
            } else {
                key.forbid
            };
            (key.name, value)
        })
        .collect()
}

/// How a key's value moved between two envelopes of one function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Movement {
    /// From the forbidding value to the allowing one: the function may now
    /// do something it could not before.
    Widened,
    /// From the allowing value to the forbidding one.
    Narrowed,
}

/// Every key whose value differs between `old` and `new` (both as
/// [`inferred_values`] gives them), with the direction it moved in. A key
/// present on one side only, or a value that is neither of its key's two,
/// counts as widened: a value that cannot be compared is never assumed
/// harmless.
pub fn compare_values(old: &[(&str, &str)], new: &[(&str, &str)]) -> Vec<(&'static str, Movement)> {
    KEYS.iter()
        .filter_map(|key| {
            let was = old.iter().find_map(|(k, v)| (*k == key.name).then_some(*v));
            let now = new.iter().find_map(|(k, v)| (*k == key.name).then_some(*v));
            if was == now {
                return None;
            }
            let movement = if was == Some(key.allow) && now == Some(key.forbid) {
                Movement::Narrowed
            } else {
                Movement::Widened
            };
            Some((key.name, movement))
        })
        .collect()
}

/// The forbidding value of `key`, if it is a checked key.
pub fn forbidding_value(key: &str) -> Option<&'static str> {
    KEYS.iter().find(|k| k.name == key).map(|k| k.forbid)
}

/// The allowing value of `key`, if it is a checked key.
pub fn allowing_value(key: &str) -> Option<&'static str> {
    KEYS.iter().find(|k| k.name == key).map(|k| k.allow)
}

/// Why `function` has the effect of the checked `key`, as the chain of
/// calls down to its source; `None` when it does not have it.
pub fn explain_key(db: &dyn HirDatabase, function: Function, key: &str) -> Option<String> {
    let key = KEYS.iter().find(|k| k.name == key)?;
    let summary = db.effect_summary(function);
    summary
        .effects
        .contains(key.effect)
        .then(|| explain(db, function.source(db).file_id, &summary, key.effect))
}

/// A public function that does not fit the package's `[envelope]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageViolation {
    pub function: Function,
    pub name: String,
    /// `file.code:12`, the line of the signature.
    pub location: String,
    /// The `[envelope]` keys it violates.
    pub keys: Vec<&'static str>,
    /// The whole diagnostic, with the call chain to each cause.
    pub message: String,
}

/// Checks every `public` function of every package in `db` against
/// `envelope`, the package's `[envelope]` table, in source order.
///
/// Only public functions are checked, and that covers the package: a
/// function's summary closes over everything it calls, so every effect
/// reachable from the public API is in some public function's summary. A
/// private function that no public one reaches is not constrained -- no
/// caller outside the package can run it. A public `extern` declaration is
/// checked against what it asserts (its trusted attributes), like any other
/// call to it.
///
/// `envelope.errors` are not reported here; the caller reports them (they
/// are about the manifest, not about any function).
pub fn package_envelope_violations(
    db: &dyn HirDatabase,
    envelope: &Envelope,
) -> Vec<PackageViolation> {
    use crate::{HasVisibility, Visibility};

    let forbidden = envelope.forbidden();
    if forbidden.is_empty() {
        return Vec::new();
    }
    let mut functions: Vec<Function> = Package::all(db)
        .into_iter()
        .flat_map(|package| package.modules(db))
        .flat_map(|module| module.all_functions(db))
        .filter(|f| f.visibility(db) == Visibility::Public)
        .collect();
    functions.sort_by_key(|f| {
        (
            f.file_id(db),
            f.source(db).value.syntax().text_range().start(),
        )
    });

    let mut violations = Vec::new();
    for function in functions {
        let summary = db.effect_summary(function);
        let hit = summary.effects.intersection(forbidden);
        if hit.is_empty() {
            continue;
        }
        let src = function.source(db);
        let home = src.file_id;
        let line = db
            .line_index(home)
            .line_col(src.value.signature_range().start())
            .line;
        let location = format!("{}:{}", db.file_relative_path(home), line + 1);
        let name = function.name(db).to_string();
        let keys: Vec<&'static str> = KEYS
            .iter()
            .filter(|k| hit.contains(k.effect))
            .map(|k| k.name)
            .collect();
        let clauses: Vec<String> = envelope
            .clauses
            .iter()
            .filter(|c| c.forbids && hit.contains(c.effect))
            .map(|c| match c.value.as_str() {
                // As TOML writes them.
                "true" | "false" => format!("{} = {}", c.key, c.value),
                _ => format!("{} = \"{}\"", c.key, c.value),
            })
            .collect();
        // One explanation per distinct cause, naming every effect it
        // explains, as the attribute diagnostics do.
        let mut causes: Vec<(EffectSet, String)> = Vec::new();
        for effect in hit.iter() {
            let because = explain(db, home, &summary, effect);
            match causes.iter_mut().find(|(_, b)| *b == because) {
                Some((set, _)) => *set = set.union(EffectSet::of(&[effect])),
                None => causes.push((EffectSet::of(&[effect]), because)),
            }
        }
        let causes: Vec<String> = causes
            .into_iter()
            .map(|(effects, because)| format!("it {}: {because}", effects.verbs()))
            .collect();
        let message = format!(
            "public function `{name}` ({location}) does not fit the package envelope \
             (`[envelope]` in codira.toml: {}): {}",
            clauses.join(", "),
            causes.join("; ")
        );
        violations.push(PackageViolation {
            function,
            name,
            location,
            keys,
            message,
        });
    }
    violations
}
