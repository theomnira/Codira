//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use super::{
    expressions, patterns, types, Parser, TokenSet, EOF, NAME, PARAM, PARAM_LIST, SELF_PARAM,
};

pub(super) fn param_list(p: &mut Parser<'_>) {
    list(p, false);
}

/// The parameter list of a Mojo-style dynamic `def` function (Python `def`
/// style): parameters are allowed to omit their type ascription, leaving the
/// parameter dynamically typed. Parse-level scaffolding only -- the HIR has
/// no dynamic-typing model yet, so untyped `def` parameters lower as missing
/// types.
pub(super) fn param_list_untyped(p: &mut Parser<'_>) {
    list(p, true);
}

fn list(p: &mut Parser<'_>, optional_types: bool) {
    assert!(p.at(T!['(']));

    let m = p.start();
    p.bump(T!['(']);

    opt_self_param(p);

    while !p.at(EOF) && !p.at(T![')']) {
        if !p.at_ts(VALUE_PARAMETER_FIRST) {
            p.error("expected value parameter");
            break;
        }
        param(p, optional_types);
        if !p.at(T![')']) {
            p.expect(T![,]);
        }
    }
    p.expect(T![')']);
    m.complete(p, PARAM_LIST);
}

const VALUE_PARAMETER_FIRST: TokenSet = patterns::PATTERN_FIRST.union(TokenSet::new(&[
    T![consuming],
    T![borrowing],
    T![inout],
    T![read],
    T![out],
    T![mut],
]));

/// A parameter may optionally be prefixed by one (mutually exclusive)
/// ownership-convention keyword (see `spec/LANGUAGE_SPEC.md` section 14). All
/// of these are parse-level scaffolding: the HIR currently treats every
/// parameter as an ordinary by-value copy, and none of the borrow/move rules
/// are enforced yet (no move-checking or use-after-consume diagnostics).
///
/// Established conventions:
///   - `var`  : mutable binding (the caller's copy stays valid).
///   - `consuming` / `owned` : the callee takes ownership; the caller's binding
///     is no longer valid after the call.
///   - `borrowing` / `read`: an explicit, read-only borrow -- the same as the
///     unmarked default, spelled out for clarity.
///   - `inout` / `mut`: in-place mutable borrowing without copying.
///   - `out`: the callee initializes the caller's uninitialized memory.
///
/// Ownership-convention keywords are contextual: the lexer emits a plain
/// `IDENT` for `consuming`/`borrowing`/`inout`/`read`/`out`/`mut` (so those
/// words stay usable as ordinary binding/path names elsewhere), and the
/// parser promotes one to its keyword `SyntaxKind` here via `bump_remap` --
/// `p.eat(T![consuming])` does not work for a contextual keyword, since
/// `Parser::at` compares the *already-lexed* token kind (`IDENT`) against
/// the target kind and always fails. See grammar.ron's `contextual_keywords`
/// doc comment.
fn eat_contextual_kw(p: &mut Parser<'_>, kw: &str, kind: crate::SyntaxKind) -> bool {
    if p.at_contextual_kw(kw) {
        p.bump_remap(kind);
        true
    } else {
        false
    }
}

fn param(p: &mut Parser<'_>, optional_types: bool) {
    let m = p.start();
    if !p.eat(T![var]) {
        let _ = eat_contextual_kw(p, "consuming", T![consuming])
            || eat_contextual_kw(p, "borrowing", T![borrowing])
            || eat_contextual_kw(p, "inout", T![inout])
            || eat_contextual_kw(p, "read", T![read])
            || eat_contextual_kw(p, "out", T![out])
            || eat_contextual_kw(p, "mut", T![mut]);
    }
    patterns::pattern(p);
    if optional_types {
        if p.at(T![:]) {
            types::ascription(p);
        }
    } else {
        types::ascription(p);
    }
    if p.eat(T![=]) {
        expressions::expr(p);
    }
    m.complete(p, PARAM);
}

fn opt_self_param(p: &mut Parser<'_>) {
    if p.at(T![self]) || (p.at(T![var]) && p.nth(1) == T![self]) {
        let m = p.start();
        p.eat(T![var]);
        self_as_name(p);
        m.complete(p, SELF_PARAM);

        if !p.at(T![')']) {
            p.expect(T![,]);
        }
    }
}

fn self_as_name(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(T![self]);
    m.complete(p, NAME);
}
