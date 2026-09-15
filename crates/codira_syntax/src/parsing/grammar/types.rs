//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use super::{
    expressions, generics, name_ref, paths, CompletedMarker, Parser, TokenSet, ARRAY_TYPE,
    NEVER_TYPE, OPTIONAL_TYPE, PATH_TYPE, REFINEMENT_TYPE,
};

pub(super) const TYPE_FIRST: TokenSet =
    paths::PATH_FIRST.union(TokenSet::new(&[T![never], T!['[']]));

pub(super) const TYPE_RECOVERY_SET: TokenSet =
    TokenSet::new(&[T!['('], T![,], T![public], T![internal]]);

pub(super) fn ascription(p: &mut Parser<'_>) {
    p.expect(T![:]);
    type_(p);
}

pub(super) fn type_(p: &mut Parser<'_>) {
    type_inner(p, true);
}

/// Like [`type_`], but never attempts to parse a trailing refinement clause.
/// A function's `-> T` return type is the one position where a bare `{`
/// genuinely can't be disambiguated from a refinement clause: the `{ IDENT |`
/// lookahead below (needed to tell `Type { x | pred }` apart from a following
/// body block) also matches the extremely common case of a body whose first
/// expression is itself a `|`/bitwise-or expression starting with a bare
/// identifier, e.g. `-> bool { a | b }`. Refinement types are only supported
/// via a named `type` alias (see the doc comment below) anyway, so the return
/// type position simply never looks for one.
pub(super) fn return_type(p: &mut Parser<'_>) {
    type_inner(p, false);
}

/// Like [`type_`], but never attempts to parse a trailing refinement clause.
/// This is the target type of an `expr as Type` cast.
///
/// A cast's target type sits in expression position, so a `{` following it is
/// overwhelmingly likely to open a block rather than a refinement clause --
/// `while i as u64 { .. }`, `match x as u8 { .. }`, `if flag as i32 { .. }`.
/// The `{ IDENT |` lookahead [`type_inner`] uses to spot a refinement cannot
/// tell those apart from `i as i32 { x | x > 0 }`, so, exactly as for
/// [`return_type`], the cast position simply never looks for one. A refined
/// cast target remains expressible through a named `type` alias.
///
/// Nothing is lost by this today: `as` only ever produces wrapping/saturating
/// conversions, never a checked one, so a refinement on the target would have
/// nothing to check. Refined casts are reserved for the refinement-types
/// integration in M7.
pub(super) fn cast_type(p: &mut Parser<'_>) {
    type_inner(p, false);
}

fn type_inner(p: &mut Parser<'_>, allow_refinement: bool) {
    let mut inner = match p.current() {
        T!['['] => array_type(p),
        T![never] => never_type(p),
        _ if paths::is_path_start(p) => path_type(p),
        _ => {
            p.error_recover("expected type", TYPE_RECOVERY_SET);
            return;
        }
    };
    if p.at(T![?]) {
        let m = inner.precede(p);
        p.bump(T![?]);
        inner = m.complete(p, OPTIONAL_TYPE);
    }
    // Refinement type: `Type { binder | predicate }`, e.g.
    // `i32 { x | x > 0 && x < MAX_ID }` (see
    // spec/self_healing_programming_language.md's refined type system).
    //
    // A bare `{` after a type is ambiguous with a following body block (e.g.
    // a function's `-> T { ... }`), so this only commits to a refinement
    // clause when the `{` is unambiguously followed by `binder |`; a plain
    // block never starts that way. Refinement types used as a return type
    // therefore need a named type alias, exactly as
    // spec/self_healing_programming_language.md's own examples do (and
    // `allow_refinement` is `false` for that one position, since even the
    // `binder |` lookahead collides with a body like `{ a | b }`).
    if allow_refinement
        && p.at(T!['{'])
        && p.nth(1) == crate::SyntaxKind::IDENT
        && p.nth(2) == T![|]
    {
        let m = inner.precede(p);
        p.bump(T!['{']);
        name_ref(p);
        p.expect(T![|]);
        expressions::expr(p);
        p.expect(T!['}']);
        m.complete(p, REFINEMENT_TYPE);
    }
}

pub(super) fn path_type(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    paths::type_path(p);
    generics::opt_generic_arg_list(p);
    m.complete(p, PATH_TYPE)
}

fn never_type(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![never]));
    let m = p.start();
    p.bump(T![never]);
    m.complete(p, NEVER_TYPE)
}

fn array_type(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T!['[']));
    let m = p.start();
    p.bump(T!['[']);
    type_(p);
    p.expect(T![']']);
    m.complete(p, ARRAY_TYPE)
}
