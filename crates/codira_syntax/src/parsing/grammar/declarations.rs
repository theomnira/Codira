//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use super::{
    adt, error_block, expressions, generics, name, name_recovery, opt_attribute_list,
    opt_visibility, params, paths, traits, types, Marker, Parser, TokenSet, CONST_DEF, EOF, ERROR,
    EXTERN, FUNCTION_DEF, IDENT, MACRO_DEF, RENAME, RET_TYPE, USE, USES_CLAUSE, USE_TREE,
    USE_TREE_LIST,
};
use crate::parsing::grammar::paths::is_use_path_start;

pub(super) const DECLARATION_RECOVERY_SET: TokenSet = TokenSet::new(&[
    T![func],
    T![def],
    T![public],
    T![internal],
    T![struct],
    T![class],
    T![trait],
    T![enum],
    T![effect],
    T![macro],
    T![import],
    T![;],
    T![extend],
]);

pub(super) fn mod_contents(p: &mut Parser<'_>) {
    while !p.at(EOF) {
        declaration(p, false);
    }
}

pub(super) fn declaration(p: &mut Parser<'_>, stop_on_r_curly: bool) {
    let m = p.start();
    let m = match maybe_declaration(p, m) {
        Ok(()) => return,
        Err(m) => m,
    };

    m.abandon(p);
    match p.current() {
        T!['{'] => error_block(p, "expected a declaration"),
        T!['}'] if !stop_on_r_curly => {
            let e = p.start();
            p.error("unmatched }");
            p.bump(T!['}']);
            e.complete(p, ERROR);
        }
        EOF | T!['}'] => p.error("expected a declaration"),
        _ => p.error_and_bump("expected a declaration"),
    }
}

pub(super) fn maybe_declaration(p: &mut Parser<'_>, m: Marker) -> Result<(), Marker> {
    opt_attribute_list(p);
    opt_visibility(p);

    // `data` is a Kotlin-style contextual modifier on `struct`/`class` (see
    // spec/LANGUAGE_SPEC.md section 15): like `def` and the param-ownership
    // keywords, the lexer emits a plain `IDENT` for it, so it must be
    // promoted here via `bump_remap` -- and only when it's actually
    // introducing a struct/class, so `data` stays usable as an ordinary
    // identifier everywhere else (a field, a binding, a function named
    // `data`).
    if p.at_contextual_kw("data") && (p.nth_at(1, T![struct]) || p.nth_at(1, T![class])) {
        p.bump_remap(T![data]);
    }

    let m = match declarations_without_modifiers(p, m) {
        Ok(()) => return Ok(()),
        Err(m) => m,
    };

    if p.at(T![extern]) {
        abi(p);
    }

    // `override` is mandatory when shadowing a superclass method (checked at
    // the HIR level); at the syntax level it's simply an optional modifier.
    p.eat(T![override]);

    let is_comptime = p.at(T![comptime]);
    if is_comptime {
        p.bump(T![comptime]);
    }

    match p.current() {
        T![func] => {
            fn_def(p);
            m.complete(p, FUNCTION_DEF);
        }
        // `def` is a contextual keyword (see params.rs's `eat_contextual_kw`
        // doc comment / grammar.ron): the lexer emits a plain `IDENT` for it,
        // so it can't be matched via `p.current() == T![def]` the way `func`
        // (a real reserved keyword) can.
        IDENT if p.at_contextual_kw("def") => {
            fn_def(p);
            m.complete(p, FUNCTION_DEF);
        }
        _ if is_comptime => {
            p.error("expected `func` after `comptime`");
            return Err(m);
        }
        _ => return Err(m),
    }
    Ok(())
}

fn abi(p: &mut Parser<'_>) {
    assert!(p.at(T![extern]));
    let abi = p.start();
    p.bump(T![extern]);
    abi.complete(p, EXTERN);
}

fn declarations_without_modifiers(p: &mut Parser<'_>, m: Marker) -> Result<(), Marker> {
    match p.current() {
        T![import] => {
            use_(p, m);
        }
        T![struct] | T![class] => {
            adt::struct_def(p, m);
        }
        T![type] => {
            adt::type_alias_def(p, m);
        }
        // Module-level bindings: `let NAME: T = expr;` (and `var`, which
        // differs only in mutability -- a distinction HIR does not track
        // today, see spec/LANGUAGE_SPEC.md section 8's shared mutability
        // model). This is what the standard library actually writes for
        // its constants; `const` is deliberately *not* a keyword, since
        // introducing one would imply comptime-only evaluation semantics
        // that should be decided rather than arrived at incidentally.
        T![let] | T![var] => {
            const_def(p, m);
        }
        T![trait] => {
            traits::trait_def(p, m);
        }
        T![enum] => {
            adt::enum_def(p, m);
        }
        T![effect] => {
            traits::effect_def(p, m);
        }
        T![macro] => {
            macro_def(p, m);
        }
        T![extend] => {
            traits::extend_(p, m);
        }
        T![extern] if matches!(p.nth(1), crate::SyntaxKind::STRING) => {
            traits::extern_block(p, m);
        }
        T![supervisor] => {
            supervisor_def(p, m);
        }
        _ => return Err(m),
    };
    Ok(())
}

/// Parses `supervisor Name { key: value, .. child Name { .. } }` (see
/// `spec/self_healing_programming_language.md` section 3.5). This only covers
/// the declaration surface: parsing and, at the HIR level, registering the
/// supervisor/child hierarchy and its config entries -- not the OTP-style
/// restart-strategy runtime described in the paper, which needs a process
/// supervision model Codira doesn't have.
fn supervisor_def(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![supervisor]));
    p.bump(T![supervisor]);
    name_recovery(p, DECLARATION_RECOVERY_SET);
    if p.at(T!['{']) {
        supervisor_item_list(p);
    } else {
        p.error("expected `{`");
    }
    m.complete(p, crate::SyntaxKind::SUPERVISOR_DEF);
}

fn supervisor_item_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    while !p.at(EOF) && !p.at(T!['}']) {
        if p.at(T![child]) {
            child_def(p);
        } else if p.at(IDENT) {
            supervisor_entry(p);
        } else {
            p.error_and_bump("expected a supervisor entry or `child`");
            continue;
        }
        if !p.at(T!['}']) {
            p.eat(T![,]);
        }
    }
    p.expect(T!['}']);
    m.complete(p, crate::SyntaxKind::SUPERVISOR_ITEM_LIST);
}

fn supervisor_entry(p: &mut Parser<'_>) {
    let m = p.start();
    name(p);
    p.expect(T![:]);
    expressions::expr(p);
    m.complete(p, crate::SyntaxKind::SUPERVISOR_ENTRY);
}

fn child_def(p: &mut Parser<'_>) {
    assert!(p.at(T![child]));
    let m = p.start();
    p.bump(T![child]);
    name_recovery(p, DECLARATION_RECOVERY_SET);
    if p.at(T!['{']) {
        supervisor_item_list(p);
    } else {
        p.error("expected `{`");
    }
    m.complete(p, crate::SyntaxKind::CHILD_DEF);
}

fn macro_def(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![macro]));
    p.bump(T![macro]);
    name_recovery(p, DECLARATION_RECOVERY_SET.union(TokenSet::new(&[T![')']])));
    if p.at(T!['(']) {
        params::param_list(p);
    } else {
        p.error("expected macro arguments");
    }
    opt_fn_ret_type(p);
    if p.at(T![;]) {
        p.bump(T![;]);
    } else {
        expressions::block(p);
    }
    m.complete(p, MACRO_DEF);
}

pub(super) fn fn_def(p: &mut Parser<'_>) {
    assert!(p.at(T![func]) || p.at_contextual_kw("def"));

    // `def` declares a Mojo/Python-style dynamically-typed function (see
    // spec/LANGUAGE_SPEC.md section 14): parameters may omit their types, and
    // the body is parsed exactly like a `func` body. Both keywords lower to
    // the same `FunctionDef` node; the leading keyword records which style was
    // written. Parse-level scaffolding only.
    let is_def = p.at_contextual_kw("def");
    if is_def {
        p.bump_remap(T![def]);
    } else {
        p.bump_any();
    }

    name_recovery(p, DECLARATION_RECOVERY_SET.union(TokenSet::new(&[T![')']])));

    generics::opt_generic_param_list(p);

    if p.at(T!['(']) {
        if is_def {
            params::param_list_untyped(p);
        } else {
            params::param_list(p);
        }
    } else {
        p.error("expected function arguments");
    }

    opt_uses_clause(p);
    opt_fn_ret_type(p);
    generics::opt_where_clause(p);

    if p.at(T![;]) {
        p.bump(T![;]);
    } else {
        expressions::block(p);
    }
}

fn opt_fn_ret_type(p: &mut Parser<'_>) -> bool {
    if p.at(T![->]) {
        let m = p.start();
        p.bump(T![->]);
        types::return_type(p);
        m.complete(p, RET_TYPE);
        true
    } else {
        false
    }
}

fn opt_uses_clause(p: &mut Parser<'_>) {
    if p.at(T![uses]) {
        let m = p.start();
        p.bump(T![uses]);
        paths::type_path(p);
        while p.at(T![,]) {
            p.bump(T![,]);
            if !paths::is_path_start(p) {
                break;
            }
            paths::type_path(p);
        }
        m.complete(p, USES_CLAUSE);
    }
}

fn use_(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![import]));
    p.bump(T![import]);
    use_tree(p, true);
    p.eat(T![;]);
    m.complete(p, USE);
}

/// Parses a use "tree", such as `foo.bar` in `import foo.bar`.
fn use_tree(p: &mut Parser<'_>, top_level: bool) {
    let m = p.start();

    match p.current() {
        T![*] if !top_level => p.bump(T![*]),
        _ if is_use_path_start(p, top_level) => {
            paths::use_path(p, top_level);
            match p.current() {
                T![as] => {
                    opt_rename(p);
                }
                T![.] if matches!(p.nth(1), T![*] | T!['{']) => {
                    p.bump(T![.]);
                    match p.current() {
                        T![*] => {
                            p.bump(T![*]);
                        }
                        T!['{'] => use_tree_list(p),
                        _ => {
                            p.error("expected `{` or `*`");
                        }
                    }
                }
                _ => (),
            }
        }
        _ => {
            m.abandon(p);
            let msg = "expected one of `self`, `super`, `root` or an identifier";
            if top_level {
                p.error_recover(msg, DECLARATION_RECOVERY_SET);
            } else {
                // if we are parsing a nested tree, we have to eat a token to remain balanced
                // `{}`
                p.error_and_bump(msg);
            }
            return;
        }
    }

    m.complete(p, USE_TREE);
}

fn use_tree_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    while !p.at(EOF) && !p.at(T!['}']) {
        use_tree(p, false);
        if !p.at(T!['}']) {
            p.expect(T![,]);
        }
    }
    p.expect(T!['}']);
    m.complete(p, USE_TREE_LIST);
}

fn opt_rename(p: &mut Parser<'_>) {
    if p.at(T![as]) {
        let m = p.start();
        p.bump(T![as]);
        if !p.eat(T![_]) {
            name(p);
        }
        m.complete(p, RENAME);
    }
}

/// Parses a module-level binding: `let NAME: T = expr;`.
///
/// Deliberately requires both the type ascription and the initializer. A
/// module-level binding has no enclosing scope to infer a type from and no
/// later assignment to take a value from, so either omission is a hard
/// error rather than something inference could recover.
fn const_def(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![let]) || p.at(T![var]));
    p.bump_any();
    name(p);
    if p.at(T![:]) {
        types::ascription(p);
    } else {
        p.error("a module-level binding must declare its type");
    }
    if p.eat(T![=]) {
        expressions::expr(p);
    } else {
        p.error("a module-level binding must have an initializer");
    }
    p.eat(T![;]);
    m.complete(p, CONST_DEF);
}
