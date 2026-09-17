//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use super::{
    expressions, name, paths, CompletedMarker, Parser, TokenSet, BIND_PAT, IDENT, LITERAL_PAT,
    PAREN_PAT, PATH_PAT, PLACEHOLDER_PAT, TUPLE_PAT, TUPLE_STRUCT_PAT,
};

pub(super) const PATTERN_FIRST: TokenSet = expressions::LITERAL_FIRST
    .union(paths::PATH_FIRST)
    .union(super::KEYWORDS_USABLE_AS_NAMES)
    .union(TokenSet::new(&[T![-], T![_], T!['(']]));

pub(super) fn pattern(p: &mut Parser<'_>) {
    pattern_r(p, PATTERN_FIRST);
}

pub(super) fn pattern_r(p: &mut Parser<'_>, recovery_set: TokenSet) -> Option<CompletedMarker> {
    atom_pat(p, recovery_set)
}

fn atom_pat(p: &mut Parser<'_>, recovery_set: TokenSet) -> Option<CompletedMarker> {
    if p.at_ts(expressions::LITERAL_FIRST) || p.at(T![-]) {
        return Some(literal_pat(p));
    }

    if paths::is_path_start(p) {
        return Some(path_like_pat(p));
    }

    // A binding may be named with one of the keywords that stay usable as
    // names (`init`, `root`, ...). `is_path_start` cannot see those -- they
    // are not `IDENT` at this point -- so they are dispatched here, and
    // `bind_pat`'s call to `name` does the remapping. This is what lets
    // `func reduce(.., init: U, ..)` declare a parameter called `init`.
    if p.at_ts(super::KEYWORDS_USABLE_AS_NAMES) {
        return Some(bind_pat(p));
    }

    #[allow(clippy::single_match_else)]
    let m = match p.current() {
        T![_] => placeholder_pat(p),
        T!['('] => tuple_pat(p),
        _ => {
            p.error_recover("expected pattern", recovery_set);
            return None;
        }
    };
    Some(m)
}

fn placeholder_pat(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![_]));
    let m = p.start();
    p.bump(T![_]);
    m.complete(p, PLACEHOLDER_PAT)
}

fn literal_pat(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    if p.at(T![-]) {
        p.bump(T![-]);
    }
    expressions::literal(p);
    m.complete(p, LITERAL_PAT)
}

/// A single bare identifier (`x`) is a binding pattern; a path with more than
/// one segment, or one immediately followed by `(...)`, refers to an enum
/// variant (`Shape.Point`, `Shape.Circle(radius)`).
fn path_like_pat(p: &mut Parser<'_>) -> CompletedMarker {
    // A name-able keyword binds exactly as an identifier does. Without this
    // `let root: usize = start` became a *path* pattern rather than a
    // binding, because `is_path_start` accepts `root` for its own
    // package-path meaning and this check only looked for `IDENT`.
    let starts_a_name = p.at(IDENT) || p.at_ts(super::KEYWORDS_USABLE_AS_NAMES);
    if starts_a_name && p.nth(1) != T![.] && p.nth(1) != T!['('] {
        return bind_pat(p);
    }

    let m = p.start();
    paths::type_path(p);
    if p.at(T!['(']) {
        p.bump(T!['(']);
        while !p.at(T![')']) && !p.at(crate::SyntaxKind::EOF) {
            pattern(p);
            if !p.at(T![')']) && !p.expect(T![,]) {
                break;
            }
        }
        p.expect(T![')']);
        m.complete(p, TUPLE_STRUCT_PAT)
    } else {
        m.complete(p, PATH_PAT)
    }
}

/// Parses `(a, b)` -- a tuple pattern, the destructuring counterpart of the
/// `(a, b)` tuple expression and the `(A, B)` tuple type.
///
/// The comma rule matches those two exactly, so all three positions agree:
/// `()` binds nothing (the unit value), `(a)` is grouping and yields the
/// inner pattern unchanged, and `(a,)` destructures a 1-tuple.
///
/// Grouping produces a `PAREN_PAT`, which HIR lowers straight through to the
/// inner pattern. Keeping it a distinct node is what makes `(a,)` -- a
/// genuine 1-tuple -- expressible at all: without it both spellings would
/// arrive as a one-element `TUPLE_PAT` and become indistinguishable.
fn tuple_pat(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T!['(']));
    let m = p.start();
    p.bump(T!['(']);

    let mut saw_comma = false;
    let mut element_count = 0usize;
    while !p.at(T![')']) && !p.at(crate::SyntaxKind::EOF) {
        pattern(p);
        element_count += 1;
        if p.at(T![,]) {
            p.bump(T![,]);
            saw_comma = true;
        } else {
            break;
        }
    }
    p.expect(T![')']);

    if element_count == 1 && !saw_comma {
        m.complete(p, PAREN_PAT)
    } else {
        m.complete(p, TUPLE_PAT)
    }
}

fn bind_pat(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    name(p);
    m.complete(p, BIND_PAT)
}
