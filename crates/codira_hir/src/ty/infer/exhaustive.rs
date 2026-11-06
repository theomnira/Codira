//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Exhaustiveness checking for `match`: finds a value no arm covers, and
//!   names it in the diagnostic.
//!
//! This is the usefulness algorithm (Maranget, "Warnings for pattern
//! matching", 2007) in its witness-producing form. Patterns are first
//! *deconstructed* against the scrutinee's type into constructors with
//! sub-patterns -- `true`, `nil`, a present optional, a tuple, an enum
//! variant, or a literal of a type with too many values to list -- and a
//! wildcard. A matrix of rows (one per arm) is then searched column by
//! column: where the arms' constructors cover every constructor of the
//! column's type, each one is tried in turn on the rows that start with it;
//! otherwise the rows that accept anything there decide, and the missing
//! constructor becomes part of the witness. Nested patterns inside tuples,
//! optionals and payloads are therefore handled exactly, not
//! conservatively.
//!
//! Against an optional, `nil` and `_` match the optional itself and every
//! other pattern matches the payload of a present value (see
//! `infer_match_pat`), so a name is a "present" constructor around a
//! wildcard, not a wildcard.

use super::InferenceResultBuilder;
use crate::{
    expr::{Literal, LiteralFloat, LiteralInt},
    HirDisplay, Pat, PatId, Ty, TyKind,
};

/// A constructor: what a pattern requires of the outermost shape of a value.
#[derive(Debug, Clone, PartialEq)]
enum Ctor {
    Bool(bool),
    Nil,
    /// A present optional; one field, the payload.
    Some,
    /// The only constructor of a tuple type; one field per element.
    Tuple,
    /// An enum variant, by tag; one field per payload field.
    Variant(u32),
    /// A literal of a type whose values are not listed (integers, floats,
    /// strings). No set of these is ever complete.
    Literal(String),
}

/// A pattern reduced to what matching depends on.
#[derive(Debug, Clone)]
enum DPat {
    Wild,
    Ctor(Ctor, Vec<DPat>),
}

/// A value, described by its shape, that no arm matches.
#[derive(Debug, Clone)]
enum Witness {
    /// Anything at all: a sub-value that does not matter.
    Any,
    /// A value of an unlisted type (an integer, say) that none of the
    /// literal arms matches.
    Other(Ty),
    Ctor(Ctor, Ty, Vec<Witness>),
}

impl InferenceResultBuilder<'_> {
    /// Describes a value of type `ty` that none of `pats` matches, or
    /// returns `None` when together they cover every value (or when the
    /// type is unknown, which has been reported already).
    pub(super) fn missing_pattern(&mut self, pats: &[PatId], ty: &Ty) -> Option<String> {
        let ty = self.normalize(ty);
        if ty.is_unknown() {
            return None;
        }
        let rows: Vec<Vec<DPat>> = pats
            .iter()
            .map(|&pat| vec![self.deconstruct(pat, &ty)])
            .collect();
        let mut witness = self.witness(&rows, std::slice::from_ref(&ty))?;
        let witness = witness.pop()?;
        Some(self.describe(&witness))
    }

    /// Reduces `pat`, matched against a value of type `ty`, to a [`DPat`].
    /// A pattern that is wrong for its type has been reported by
    /// `infer_match_pat`; it counts as a wildcard here, so the mistake is not
    /// reported twice.
    fn deconstruct(&mut self, pat: PatId, ty: &Ty) -> DPat {
        let ty = self.normalize(ty);
        let pattern = self.body[pat].clone();
        if let TyKind::Optional(payload) = ty.interned() {
            return match pattern {
                Pat::Wild | Pat::Missing => DPat::Wild,
                Pat::Literal(Literal::Nil) => DPat::Ctor(Ctor::Nil, Vec::new()),
                _ => {
                    let payload = payload.clone();
                    DPat::Ctor(Ctor::Some, vec![self.deconstruct(pat, &payload)])
                }
            };
        }
        match pattern {
            Pat::Wild | Pat::Missing | Pat::Bind { .. } => DPat::Wild,
            Pat::Literal(Literal::Bool(value)) => DPat::Ctor(Ctor::Bool(value), Vec::new()),
            Pat::Literal(Literal::Int(LiteralInt { value, .. })) => {
                DPat::Ctor(Ctor::Literal(value.to_string()), Vec::new())
            }
            Pat::Literal(Literal::Float(LiteralFloat { value, .. })) => {
                DPat::Ctor(Ctor::Literal(value.to_bits().to_string()), Vec::new())
            }
            Pat::Literal(Literal::String(text)) => {
                DPat::Ctor(Ctor::Literal(format!("{text:?}")), Vec::new())
            }
            // `nil` against a non-optional matches nothing.
            Pat::Literal(Literal::Nil) => DPat::Ctor(Ctor::Literal("nil".into()), Vec::new()),
            Pat::Tuple(args) => match ty.interned() {
                TyKind::Tuple(arity, elements) if *arity == args.len() => {
                    let elements = elements.clone();
                    let fields = args
                        .iter()
                        .zip(elements.iter())
                        .map(|(&arg, element)| self.deconstruct(arg, element))
                        .collect();
                    DPat::Ctor(Ctor::Tuple, fields)
                }
                _ => DPat::Wild,
            },
            Pat::Path(_) | Pat::TupleStruct { .. } => {
                let Some(variant) = self.pat_variants.get(&pat).copied() else {
                    return DPat::Wild;
                };
                let args = match &pattern {
                    Pat::TupleStruct { args, .. } => args.clone(),
                    _ => Vec::new(),
                };
                let enum_args = crate::mono::enum_args(&self.normalize(&ty));
                let field_tys = variant.field_types_at(self.db, &enum_args);
                let fields = field_tys
                    .iter()
                    .enumerate()
                    .map(|(idx, field_ty)| match args.get(idx) {
                        Some(&arg) => self.deconstruct(arg, field_ty),
                        None => DPat::Wild,
                    })
                    .collect();
                DPat::Ctor(Ctor::Variant(variant.index()), fields)
            }
        }
    }

    /// Every constructor of `ty`, or `None` when its values are not listed.
    fn all_ctors(&mut self, ty: &Ty) -> Option<Vec<Ctor>> {
        match self.normalize(ty).interned() {
            TyKind::Bool => Some(vec![Ctor::Bool(true), Ctor::Bool(false)]),
            TyKind::Optional(_) => Some(vec![Ctor::Nil, Ctor::Some]),
            TyKind::Tuple(..) => Some(vec![Ctor::Tuple]),
            TyKind::Enum(e, _) => Some(
                e.variants(self.db)
                    .into_iter()
                    .map(|variant| Ctor::Variant(variant.index()))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// The types of the fields of `ctor` applied to a value of type `ty`.
    fn ctor_fields(&mut self, ctor: &Ctor, ty: &Ty) -> Vec<Ty> {
        match (ctor, self.normalize(ty).interned()) {
            (Ctor::Some, TyKind::Optional(payload)) => vec![payload.clone()],
            (Ctor::Tuple, TyKind::Tuple(_, elements)) => elements.iter().cloned().collect(),
            (Ctor::Variant(tag), TyKind::Enum(e, args)) => e
                .variants(self.db)
                .get(*tag as usize)
                .map(|variant| variant.field_types_at(self.db, args))
                .unwrap_or_default(),

            _ => Vec::new(),
        }
    }

    /// Finds values for the columns `tys` that no row of `rows` matches,
    /// one witness per column, or `None` when the rows cover everything.
    fn witness(&mut self, rows: &[Vec<DPat>], tys: &[Ty]) -> Option<Vec<Witness>> {
        let Some((ty, rest)) = tys.split_first() else {
            return rows.is_empty().then(Vec::new);
        };
        let heads: Vec<Ctor> = rows
            .iter()
            .filter_map(|row| match &row[0] {
                DPat::Ctor(ctor, _) => Some(ctor.clone()),
                DPat::Wild => None,
            })
            .collect();
        let all = self.all_ctors(ty);
        let missing: Option<Vec<Ctor>> = all
            .clone()
            .map(|all| all.into_iter().filter(|c| !heads.contains(c)).collect());

        if let (Some(all), Some(missing)) = (all, &missing) {
            if missing.is_empty() {
                // The arms name every constructor: a gap can only be inside
                // one of them.
                for ctor in all {
                    let fields = self.ctor_fields(&ctor, ty);
                    let specialized = specialize(rows, &ctor, fields.len());
                    let columns: Vec<Ty> = fields.iter().chain(rest).cloned().collect();
                    if let Some(mut found) = self.witness(&specialized, &columns) {
                        let args = found.drain(..fields.len()).collect();
                        found.insert(0, Witness::Ctor(ctor, ty.clone(), args));
                        return Some(found);
                    }
                }
                return None;
            }
        }

        // Some constructor is named by no arm (or the type's values are not
        // listed): only the rows that accept anything here can cover it.
        let defaults: Vec<Vec<DPat>> = rows
            .iter()
            .filter(|row| matches!(row[0], DPat::Wild))
            .map(|row| row[1..].to_vec())
            .collect();
        let mut found = self.witness(&defaults, rest)?;
        let head = match missing.and_then(|missing| missing.into_iter().next()) {
            Some(ctor) => {
                let arity = self.ctor_fields(&ctor, ty).len();
                Witness::Ctor(ctor, ty.clone(), vec![Witness::Any; arity])
            }
            None => Witness::Other(ty.clone()),
        };
        found.insert(0, head);
        Some(found)
    }

    /// The diagnostic's description of an uncovered value.
    fn describe(&mut self, witness: &Witness) -> String {
        let (text, other) = match witness {
            Witness::Other(ty) => {
                return format!("any `{}` no literal arm matches", ty.display(self.db))
            }
            Witness::Ctor(Ctor::Some, _, args) => match args.first() {
                Some(Witness::Other(ty)) => {
                    return format!("a present `{}` no literal arm matches", ty.display(self.db))
                }
                Some(Witness::Any) | None => return "a present value".to_string(),
                Some(inner) => {
                    let mut other = None;
                    let text = self.render(inner, &mut other);
                    (format!("a present value matching `{text}`"), other)
                }
            },
            _ => {
                let mut other = None;
                let text = self.render(witness, &mut other);
                (format!("`{text}`"), other)
            }
        };
        match other {
            Some(ty) => format!(
                "{text} (where that `_` is any `{}` no literal arm matches)",
                ty.display(self.db)
            ),
            None => text,
        }
    }

    /// Writes `witness` as a pattern. The first `_` that stands for "a value
    /// no literal matches" is reported through `other`, so the message can
    /// say which type it is.
    fn render(&mut self, witness: &Witness, other: &mut Option<Ty>) -> String {
        match witness {
            Witness::Any => "_".to_string(),
            Witness::Other(ty) => {
                other.get_or_insert_with(|| ty.clone());
                "_".to_string()
            }
            Witness::Ctor(ctor, ty, args) => match ctor {
                Ctor::Bool(value) => value.to_string(),
                Ctor::Nil => "nil".to_string(),
                // A pattern against an optional matches the payload.
                Ctor::Some => args
                    .first()
                    .map_or_else(|| "_".to_string(), |inner| self.render(inner, other)),
                Ctor::Tuple => {
                    let elements: Vec<String> =
                        args.iter().map(|arg| self.render(arg, other)).collect();
                    if elements.len() == 1 {
                        format!("({},)", elements[0])
                    } else {
                        format!("({})", elements.join(", "))
                    }
                }
                Ctor::Variant(tag) => {
                    let TyKind::Enum(e, _) = self.normalize(ty).interned().clone() else {
                        return "_".to_string();
                    };
                    let name = e
                        .variants(self.db)
                        .get(*tag as usize)
                        .map_or_else(|| "_".to_string(), |v| v.display(self.db).to_string());
                    if args.is_empty() {
                        name
                    } else {
                        let fields: Vec<String> =
                            args.iter().map(|arg| self.render(arg, other)).collect();
                        format!("{name}({})", fields.join(", "))
                    }
                }
                Ctor::Literal(text) => text.clone(),
            },
        }
    }
}

/// The rows of `rows` that can match a value built with `ctor`, with the
/// constructor's `arity` fields in place of the first column.
fn specialize(rows: &[Vec<DPat>], ctor: &Ctor, arity: usize) -> Vec<Vec<DPat>> {
    rows.iter()
        .filter_map(|row| {
            let fields = match &row[0] {
                DPat::Wild => vec![DPat::Wild; arity],
                DPat::Ctor(head, fields) if head == ctor => fields.clone(),
                DPat::Ctor(..) => return None,
            };
            Some(fields.into_iter().chain(row[1..].iter().cloned()).collect())
        })
        .collect()
}
