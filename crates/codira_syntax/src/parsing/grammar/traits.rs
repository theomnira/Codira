//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use super::{
    declarations::{declaration, fn_def},
    error_block, generics, name, name_recovery, types, Marker, Parser, EFFECT_DEF, EFFECT_OP,
    EFFECT_OP_LIST, EXTEND, EXTEND_ITEM_LIST, EXTERN_BLOCK, EXTERN_ITEM_LIST, INHERITANCE_LIST,
    PARAM_LIST, RET_TYPE, TRAIT_DEF,
};
use crate::SyntaxKind::{EOF, FUNCTION_DEF, STRING};

/// Parses an optional `: Base, Trait1, Trait2` clause on a `struct`, `class`,
/// `trait`, or `extend`, replacing Rust's separate trait-impl syntax with a
/// single Swift/Kotlin-style colon list.
pub(super) fn opt_inheritance_list(p: &mut Parser<'_>) {
    if !p.at(T![:]) {
        return;
    }
    let m = p.start();
    p.bump(T![:]);
    inheritance_entry(p);
    while p.at(T![,]) {
        p.bump(T![,]);
        inheritance_entry(p);
    }
    m.complete(p, INHERITANCE_LIST);
}

/// A single entry in an inheritance/conformance list. Ordinarily just a
/// trait/superclass name, but may be prefixed with `~` to explicitly opt
/// out of an implicit marker trait, e.g. `struct GPUBuffer: ~Copyable { .. }`
/// declaring a linear, non-duplicable resource type (see
/// `spec/LANGUAGE_SPEC.md` section 14). Parse-level scaffolding only: the
/// HIR does not yet have a `Copyable` marker trait to opt out of, so `~`
/// currently parses but has no enforced effect (no move-checking).
fn inheritance_entry(p: &mut Parser<'_>) {
    p.eat(T![~]);
    types::type_(p);
}

/// Parses `extend Type { .. }` or `extend Type: Trait1, Trait2 { .. }`.
/// Replaces Rust's `impl Type { .. }` / `impl Trait for Type { .. }`.
pub(super) fn extend_(p: &mut Parser<'_>, m: Marker) {
    p.bump(T![extend]);
    types::type_(p);
    opt_inheritance_list(p);
    if p.at(T!['{']) {
        extend_item_list(p);
    } else {
        p.error("expected `{`");
    }
    m.complete(p, EXTEND);
}

fn extend_item_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    while !p.at(EOF) && !p.at(T!['}']) {
        if p.at(T!['{']) {
            error_block(p, "expected an extend item");
            continue;
        }
        declaration(p, true);
    }
    p.expect(T!['}']);
    m.complete(p, EXTEND_ITEM_LIST);
}

/// Parses `trait Name { .. }` or `trait Name: Base { .. }`. Trait bodies hold
/// method signatures (terminated by `;`) and/or default implementations.
pub(super) fn trait_def(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![trait]));
    p.bump(T![trait]);
    name_recovery(p, super::declarations::DECLARATION_RECOVERY_SET);
    generics::opt_generic_param_list(p);
    opt_inheritance_list(p);
    generics::opt_where_clause(p);
    if p.at(T!['{']) {
        extend_item_list(p);
    } else {
        p.error("expected `{`");
    }
    m.complete(p, TRAIT_DEF);
}

/// Parses `effect Name { func op(..) -> T }`, an algebraic effect signature.
pub(super) fn effect_def(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![effect]));
    p.bump(T![effect]);
    name_recovery(p, super::declarations::DECLARATION_RECOVERY_SET);
    if p.at(T!['{']) {
        effect_op_list(p);
    } else {
        p.error("expected `{`");
    }
    m.complete(p, EFFECT_DEF);
}

fn effect_op_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    while !p.at(EOF) && !p.at(T!['}']) {
        effect_op(p);
    }
    p.expect(T!['}']);
    m.complete(p, EFFECT_OP_LIST);
}

fn effect_op(p: &mut Parser<'_>) {
    let m = p.start();
    if p.at(T![func]) {
        p.bump(T![func]);
        name_recovery(p, super::declarations::DECLARATION_RECOVERY_SET);
        if p.at(T!['(']) {
            let pm = p.start();
            p.bump(T!['(']);
            while !p.at(T![')']) && !p.at(EOF) {
                let param = p.start();
                name(p);
                if p.eat(T![:]) {
                    types::type_(p);
                }
                param.complete(p, crate::SyntaxKind::PARAM);
                if !p.at(T![')']) && !p.expect(T![,]) {
                    break;
                }
            }
            p.expect(T![')']);
            pm.complete(p, PARAM_LIST);
        } else {
            p.error("expected effect operation parameters");
        }
        if p.at(T![->]) {
            let rm = p.start();
            p.bump(T![->]);
            types::type_(p);
            rm.complete(p, RET_TYPE);
        }
        p.eat(T![;]);
        m.complete(p, EFFECT_OP);
    } else {
        m.abandon(p);
        p.error_and_bump("expected an effect operation");
    }
}

/// Parses `extern "C" { .. }` / `extern "C++" { .. }` interop blocks. Unlike
/// the single-function `extern func` ABI prefix, this form declares a batch
/// of externally-defined symbols under one ABI string.
pub(super) fn extern_block(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![extern]));
    p.bump(T![extern]);
    assert!(p.at(STRING));
    p.bump(STRING);
    if p.at(T!['{']) {
        extern_item_list(p);
    } else {
        p.error("expected `{`");
    }
    m.complete(p, EXTERN_BLOCK);
}

fn extern_item_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    while !p.at(EOF) && !p.at(T!['}']) {
        if p.at(T!['{']) {
            error_block(p, "expected an extern item");
            continue;
        }
        extern_item(p);
    }
    p.expect(T!['}']);
    m.complete(p, EXTERN_ITEM_LIST);
}

fn extern_item(p: &mut Parser<'_>) {
    let m = p.start();
    if p.at(T![func]) {
        fn_def(p);
        m.complete(p, FUNCTION_DEF);
    } else {
        m.abandon(p);
        p.error_and_bump("expected `func`");
    }
}
