//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use super::{
    expressions, name, paths, CompletedMarker, Parser, TokenSet, BIND_PAT, IDENT, LITERAL_PAT,
    PATH_PAT, PLACEHOLDER_PAT, TUPLE_STRUCT_PAT,
};

pub(super) const PATTERN_FIRST: TokenSet = expressions::LITERAL_FIRST
    .union(paths::PATH_FIRST)
    .union(TokenSet::new(&[T![-], T![_]]));

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

    #[allow(clippy::single_match_else)]
    let m = match p.current() {
        T![_] => placeholder_pat(p),
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
    if p.at(IDENT) && p.nth(1) != T![.] && p.nth(1) != T!['('] {
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

fn bind_pat(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    name(p);
    m.complete(p, BIND_PAT)
}
