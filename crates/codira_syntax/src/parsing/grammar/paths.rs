//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use super::{declarations, name_ref, Parser, TokenSet, IDENT, PATH, PATH_SEGMENT};

pub(super) const PATH_FIRST: TokenSet =
    TokenSet::new(&[IDENT, T![super], T![self], T![root], T![resume]])
        // A value may be *named* with one of the keywords that stay usable as names
        // (`init`, `extend`, `type`), so a path can start with one. `root` is
        // already here for its own meaning, and is deliberately absent from the
        // value-name set -- see `KEYWORDS_USABLE_AS_VALUE_NAMES`.
        .union(super::KEYWORDS_USABLE_AS_VALUE_NAMES);

pub(super) fn is_path_start(p: &Parser<'_>) -> bool {
    matches!(
        p.current(),
        IDENT | T![self] | T![super] | T![root] | T![resume]
    ) || super::KEYWORDS_USABLE_AS_VALUE_NAMES.contains(p.current())
}

/// Whether a leading `root` here is the *package-root* path keyword rather
/// than an ordinary binding called `root`.
///
/// `root` only means "the current package" as the first segment of a path,
/// so it is followed by `.`. A bare `root` with anything else after it --
/// `root * 2 + 1`, as `std/builtin/sort.code` writes it -- can only be a
/// value, and a package root is not a value.
///
/// One token of lookahead settles it, which is why `root` can be usable as a
/// name without giving up the keyword.
pub(super) fn root_is_package_keyword(p: &Parser<'_>) -> bool {
    p.at(T![root]) && p.nth(1) == T![.]
}

pub(super) fn is_use_path_start(p: &Parser<'_>, top_level: bool) -> bool {
    if top_level {
        matches!(p.current(), IDENT | T![self] | T![super] | T![root])
    } else {
        matches!(p.current(), IDENT | T![self])
    }
}

/// Parses a (potentially multi-segment, `.`-separated) path in type
/// position, e.g. `geometry.Point` in `let p: geometry.Point`, or a
/// qualified enum-variant pattern like `Shape.Circle`.
pub(super) fn type_path(p: &mut Parser<'_>) {
    path(p);
}

/// Parses a single-segment path in expression position: a bare identifier,
/// or `self` / `super` / `root`. Multi-segment qualified access in
/// expression position (module member access, `Enum.Variant`, associated
/// members) is *not* handled here -- it goes through the ordinary postfix
/// `.field` / `.method()` grammar just like any other member access, and is
/// disambiguated from real field access during name resolution. This keeps
/// `self.field` (and every other value-dot-member expression) parsing
/// exactly as it always has, with zero new ambiguity.
pub(super) fn expr_path(p: &mut Parser<'_>) {
    let path = p.start();
    path_segment(p);
    path.complete(p, PATH);
}

/// Parses a (potentially multi-segment) path in an `import` statement.
pub(super) fn use_path(p: &mut Parser<'_>, _top_level: bool) {
    path(p);
}

fn path(p: &mut Parser<'_>) {
    let path = p.start();
    path_segment(p);
    let mut qualifier = path.complete(p, PATH);
    loop {
        // `.{` or `.*` after a path introduces a use-tree list/glob, not a
        // further path segment.
        let use_tree = matches!(p.nth(1), T![*] | T!['{']);
        if p.at(T![.]) && !use_tree {
            let path = qualifier.precede(p);
            p.bump(T![.]);
            path_segment(p);
            qualifier = path.complete(p, PATH);
        } else {
            break;
        }
    }
}

fn path_segment(p: &mut Parser<'_>) {
    let m = p.start();
    match p.current() {
        IDENT => {
            name_ref(p);
        }
        // A segment named with a keyword that stays usable as a name;
        // `name_ref` remaps it to an identifier.
        k if super::KEYWORDS_USABLE_AS_VALUE_NAMES.contains(k) => {
            name_ref(p);
        }
        // A `root` that is not starting a package path is an ordinary name.
        T![root] if !root_is_package_keyword(p) => name_ref(p),
        T![super] | T![root] | T![self] | T![resume] => p.bump_any(),
        _ => p.error_recover(
            "expected identifier",
            declarations::DECLARATION_RECOVERY_SET,
        ),
    }
    m.complete(p, PATH_SEGMENT);
}
