//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use super::{
    declarations, error_block, generics, name, name_recovery, opt_visibility, traits, types,
    Marker, Parser, ENUM_DEF, ENUM_VARIANT, ENUM_VARIANT_LIST, IDENT, RECORD_FIELD_DEF,
    RECORD_FIELD_DEF_LIST, STRUCT_DEF, TUPLE_FIELD_DEF, TUPLE_FIELD_DEF_LIST, TYPE_ALIAS_DEF,
    VISIBILITY_FIRST,
};
use crate::{
    parsing::{grammar::types::TYPE_FIRST, token_set::TokenSet},
    SyntaxKind::ERROR,
};

const TUPLE_FIELD_FIRST: TokenSet = types::TYPE_FIRST.union(VISIBILITY_FIRST);

/// Parses `struct Foo { .. }` (a copied value type) or `class Foo { .. }` (a
/// heap-allocated, garbage-collected reference type with shared mutability).
/// The memory kind used to be a `(gc)`/`(value)` parenthetical annotation on
/// `struct`; it is now simply which of the two keywords introduced the type.
pub(super) fn struct_def(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![struct]) || p.at(T![class]));
    p.bump_any();
    name_recovery(p, declarations::DECLARATION_RECOVERY_SET);
    generics::opt_generic_param_list(p);
    traits::opt_inheritance_list(p);
    generics::opt_where_clause(p);
    match p.current() {
        T![;] => {
            p.bump(T![;]);
        }
        T!['{'] => record_field_def_list(p),
        T!['('] => tuple_field_def_list(p),
        _ => {
            p.error("expected a ';', '{', or '('");
        }
    }
    m.complete(p, STRUCT_DEF);
}

pub(super) fn type_alias_def(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![type]));
    p.bump(T![type]);
    name(p);
    if p.eat(T![=]) {
        types::type_(p);
    }
    p.eat(T![;]);
    m.complete(p, TYPE_ALIAS_DEF);
}

/// Parses `enum Name { Variant, Variant(Type, ..), Variant { field: Type, .. }
/// }`.
pub(super) fn enum_def(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![enum]));
    p.bump(T![enum]);
    name_recovery(p, declarations::DECLARATION_RECOVERY_SET);
    generics::opt_generic_param_list(p);
    generics::opt_where_clause(p);
    if p.at(T!['{']) {
        enum_variant_list(p);
    } else {
        p.error("expected `{`");
    }
    m.complete(p, ENUM_DEF);
}

fn enum_variant_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    while !p.at(T!['}']) && !p.at(crate::SyntaxKind::EOF) {
        if p.at(T!['{']) {
            error_block(p, "expected an enum variant");
            continue;
        }
        enum_variant(p);
        // The trailing comma between variants is optional (variants are also
        // separated by the newline convention shown in the language spec).
        if !p.at(T!['}']) {
            p.eat(T![,]);
        }
    }
    p.expect(T!['}']);
    m.complete(p, ENUM_VARIANT_LIST);
}

fn enum_variant(p: &mut Parser<'_>) {
    let m = p.start();
    if p.at(IDENT) {
        name(p);
        match p.current() {
            T!['{'] => record_field_def_list(p),
            T!['('] => tuple_field_def_list(p),
            _ => (),
        }
        m.complete(p, ENUM_VARIANT);
    } else {
        m.abandon(p);
        p.error_and_bump("expected an enum variant");
    }
}

pub(super) fn record_field_def_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    while !p.at(T!['}']) && !p.at(crate::SyntaxKind::EOF) {
        if p.at(T!['{']) {
            error_block(p, "expected a field");
            continue;
        }
        record_field_def(p);
        if !p.at(T!['}']) {
            p.expect(T![,]);
        }
    }
    p.expect(T!['}']);
    p.eat(T![;]);
    m.complete(p, RECORD_FIELD_DEF_LIST);
}

pub(super) fn tuple_field_def_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['(']));
    let m = p.start();
    p.bump(T!['(']);
    while !p.at(T![')']) && !p.at(crate::SyntaxKind::EOF) {
        let m = p.start();
        if !p.at_ts(TUPLE_FIELD_FIRST) {
            m.abandon(p);
            p.error_and_bump("expected a tuple field");
            break;
        }
        let has_vis = opt_visibility(p);
        // Tuple fields (including enum-variant payloads) may optionally carry a
        // `name:` label purely for readability, e.g. `Circle(radius: f64)`; the
        // label itself isn't yet a distinct addressable field name in the HIR.
        if p.at(IDENT) && p.nth(1) == T![:] {
            p.bump(IDENT);
            p.bump(T![:]);
        }
        if !p.at_ts(TYPE_FIRST) {
            p.error("expected a type");
            if has_vis {
                m.complete(p, ERROR);
            } else {
                m.abandon(p);
            }
            break;
        }
        types::type_(p);
        m.complete(p, TUPLE_FIELD_DEF);

        if !p.at(T![')']) {
            p.expect(T![,]);
        }
    }
    p.expect(T![')']);
    p.eat(T![;]);
    m.complete(p, TUPLE_FIELD_DEF_LIST);
}

fn record_field_def(p: &mut Parser<'_>) {
    let m = p.start();
    opt_visibility(p);
    // `let`/`var` mirror local-binding mutability: a `let` field can only be
    // set from `init`, a `var` field can be reassigned any time. Neither
    // keyword is required (fields default to `let`-like, immutable-after-init
    // semantics); this is purely a syntax-level marker for now.
    p.eat(T![let]);
    p.eat(T![var]);
    // A field may also be named with one of the keywords that stay usable as
    // names (`std/os/path.code` has a field called `root`); `name` does the
    // remapping.
    if p.at(IDENT) || p.at_ts(super::KEYWORDS_USABLE_AS_NAMES) {
        name(p);
        p.expect(T![:]);
        types::type_(p);
        m.complete(p, RECORD_FIELD_DEF);
    } else {
        m.abandon(p);
        p.error_and_bump("expected a field declaration");
    }
}
