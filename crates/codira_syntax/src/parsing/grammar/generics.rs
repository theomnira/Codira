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
        bound_list(p);
    }
    m.complete(p, GENERIC_PARAM);
}

/// Parses one or more `+`-separated bounds: `T: EqualityComparable +
/// Stringable`.
///
/// A single bound was already accepted; the `+` form is what
/// `std/sys/terminate.code` writes and is the ordinary way to require more
/// than one conformance. Bounds are parsed but not yet checked -- nothing
/// resolves a conformance -- so this makes the signature readable rather
/// than enforced.
fn bound_list(p: &mut Parser<'_>) {
    types::type_(p);
    while p.eat(T![+]) {
        types::type_(p);
    }
}

/// Parses an optional generic argument list in type position: `Box[i32]`,
/// `SIMD[f32, 4]`.
///
/// An argument is normally a type, but a *const* generic parameter takes a
/// value, so an integer literal is accepted too. `generic_param` already
/// allows the declaration side (`[T, N: usize]`); without the matching
/// argument form, `SIMD[T, N]` parsed while `SIMD[f32, 4]` -- which is how
/// `std/gpu` and `std/builtin/simd.code` actually write it -- did not.
///
/// The literal is wrapped in its own `LITERAL` node so it stays
/// distinguishable from a type named by a path; nothing lowers const
/// arguments yet, and mixing the two would make that later work harder to
/// get right rather than easier.
pub(super) fn opt_generic_arg_list(p: &mut Parser<'_>) {
    if !p.at(T!['[']) {
        return;
    }
    let m = p.start();
    p.bump(T!['[']);
    while !p.at(T![']']) && !p.at(crate::SyntaxKind::EOF) {
        generic_arg(p);
        if !p.at(T![']']) && !p.expect(T![,]) {
            break;
        }
    }
    p.expect(T![']']);
    m.complete(p, GENERIC_ARG_LIST);
}

/// One generic argument: a type, an integer literal for a const parameter,
/// or -- in an `extend` -- a *binding* occurrence with a bound.
///
/// `extend OwnedPointer[T: Defaultable] { .. }` reuses the argument-list
/// production because an `extend` has no separate parameter list: its `[T]`
/// is syntactically an argument on the extended type while semantically
/// introducing `T` (see `resolve::impl_generic_param`). Accepting `: Bound`
/// here is what lets that spelling parse; the bound is recorded and not yet
/// checked, exactly as on a declaration.
fn generic_arg(p: &mut Parser<'_>) {
    if p.at(crate::SyntaxKind::INT_NUMBER) {
        let m = p.start();
        p.bump(crate::SyntaxKind::INT_NUMBER);
        m.complete(p, crate::SyntaxKind::LITERAL);
        return;
    }
    types::type_(p);
    if p.eat(T![:]) {
        bound_list(p);
    }
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
