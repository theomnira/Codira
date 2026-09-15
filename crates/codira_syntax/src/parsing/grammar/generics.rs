//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use super::{
    name, types, Parser, GENERIC_ARG_LIST, GENERIC_PARAM, GENERIC_PARAM_LIST, WHERE_CLAUSE,
    WHERE_PRED,
};

/// Parses an optional generic parameter list: `[T, U: Bound]`.
///
/// Square brackets are used instead of angle brackets so that generics never
/// read like markup-language syntax.
pub(super) fn opt_generic_param_list(p: &mut Parser<'_>) {
    if !p.at(T!['[']) {
        return;
    }
    let m = p.start();
    p.bump(T!['[']);
    while !p.at(T![']']) && !p.at(crate::SyntaxKind::EOF) {
        generic_param(p);
        if !p.at(T![']']) && !p.expect(T![,]) {
            break;
        }
    }
    p.expect(T![']']);
    m.complete(p, GENERIC_PARAM_LIST);
}

fn generic_param(p: &mut Parser<'_>) {
    let m = p.start();
    name(p);
    if p.eat(T![:]) {
        types::type_(p);
    }
    m.complete(p, GENERIC_PARAM);
}

/// Parses an optional generic argument list in type position: `Box[i32]`.
pub(super) fn opt_generic_arg_list(p: &mut Parser<'_>) {
    if !p.at(T!['[']) {
        return;
    }
    let m = p.start();
    p.bump(T!['[']);
    while !p.at(T![']']) && !p.at(crate::SyntaxKind::EOF) {
        types::type_(p);
        if !p.at(T![']']) && !p.expect(T![,]) {
            break;
        }
    }
    p.expect(T![']']);
    m.complete(p, GENERIC_ARG_LIST);
}

/// Parses an optional `where` clause: `where T: Comparable, U: Show`.
pub(super) fn opt_where_clause(p: &mut Parser<'_>) {
    if !p.at(T![where]) {
        return;
    }
    let m = p.start();
    p.bump(T![where]);
    loop {
        let pred = p.start();
        types::type_(p);
        if p.eat(T![:]) {
            types::type_(p);
        }
        pred.complete(p, WHERE_PRED);
        if !p.eat(T![,]) {
            break;
        }
    }
    m.complete(p, WHERE_CLAUSE);
}
