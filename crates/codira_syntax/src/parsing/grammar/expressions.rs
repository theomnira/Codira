//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use super::{
    error_block, expressions, name_ref, name_ref_or_index, paths, patterns, types, BlockLike,
    CompletedMarker, Marker, Parser, SyntaxKind, TokenSet, ARG_LIST, ARRAY_EXPR, BIN_EXPR,
    BLOCK_EXPR, BREAK_EXPR, CALL_EXPR, CAST_EXPR, CHANNEL_RECV_EXPR, CHANNEL_SEND_EXPR,
    COMPTIME_EXPR, CONDITION, EOF, ERROR, EXPR_STMT, FIELD_EXPR, FLOAT_NUMBER, HANDLER_ARM,
    HANDLER_ARM_LIST, HANDLE_EXPR, IDENT, IF_EXPR, INDEX, INDEX_EXPR, INT_NUMBER, LET_STMT,
    LITERAL, LOOP_EXPR, MATCH_ARM, MATCH_ARM_LIST, MATCH_EXPR, PARAM, PARAM_LIST, PAREN_EXPR,
    PATH_EXPR, PATH_TYPE, PERFORM_EXPR, PREFIX_EXPR, RECORD_FIELD, RECORD_FIELD_LIST, RECORD_LIT,
    RETURN_EXPR, SPAWN_EXPR, STRING, TRANSFER_EXPR, TRY_EXPR, WHILE_EXPR,
};
use crate::{parsing::grammar::paths::PATH_FIRST, SyntaxKind::METHOD_CALL_EXPR};

pub(crate) const LITERAL_FIRST: TokenSet = TokenSet::new(&[
    T![true],
    T![false],
    T![nil],
    INT_NUMBER,
    FLOAT_NUMBER,
    STRING,
]);

const EXPR_RECOVERY_SET: TokenSet = TokenSet::new(&[T![let]]);

const ATOM_EXPR_FIRST: TokenSet = LITERAL_FIRST.union(PATH_FIRST).union(TokenSet::new(&[
    IDENT,
    T!['('],
    T!['{'],
    T!['['],
    T![if],
    T![loop],
    T![return],
    T![break],
    T![while],
    T![comptime],
    T![match],
    T![perform],
    T![handle],
    T![spawn],
]));

const LHS_FIRST: TokenSet = ATOM_EXPR_FIRST.union(TokenSet::new(&[T![!], T![-], T![~], T![<-]]));

const EXPR_FIRST: TokenSet = LHS_FIRST;

#[derive(Clone, Copy)]
struct Restrictions {
    /// Indicates that parsing of structs is not valid in the current context.
    /// For instance:
    ///
    /// ```codira
    /// if break { 3 }
    /// if break 4 { 3 }
    /// ```
    ///
    /// In the first if expression we do not want the `break` expression to
    /// capture the block as an expression. However, in the second statement
    /// we do want the break to capture the 4.
    forbid_structs: bool,
}

pub(crate) fn expr_block_contents(p: &mut Parser<'_>) {
    while !p.at(EOF) && !p.at(T!['}']) {
        if p.eat(T![;]) {
            continue;
        }

        stmt(p);
    }
}

/// Parses a block statement
pub(crate) fn block(p: &mut Parser<'_>) {
    if !p.at(T!['{']) {
        p.error("expected a block");
        return;
    }
    block_expr(p);
}

fn block_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    expr_block_contents(p);
    p.expect(T!['}']);
    m.complete(p, BLOCK_EXPR)
}

/// Parses a general statement: (let, expr, etc.)
pub(super) fn stmt(p: &mut Parser<'_>) {
    let m = p.start();

    // `let` (immutable) and `var` (mutable) both produce a LET_STMT; which
    // keyword was used is recorded as the leading token.
    if p.at(T![let]) || p.at(T![var]) {
        let_stmt(p, m);
        return;
    }

    let (cm, _blocklike) = expr_stmt(p);
    let kind = cm.as_ref().map_or(ERROR, CompletedMarker::kind);

    if p.at(T!['}']) {
        if let Some(cm) = cm {
            cm.undo_completion(p).abandon(p);
            m.complete(p, kind);
        } else {
            m.abandon(p);
        }
    } else {
        p.eat(T![;]);
        m.complete(p, EXPR_STMT);
    }
}

fn let_stmt(p: &mut Parser<'_>, m: Marker) {
    assert!(p.at(T![let]) || p.at(T![var]));
    p.bump_any();
    // `let mut x` is accepted as the Rust-style spelling of `var x`.
    // Both forms produce the same LET_STMT: the language uses the shared
    // mutability model of spec/LANGUAGE_SPEC.md section 8 (no borrow
    // checker), so the binding form carries no semantics HIR tracks today.
    // Accepting it here is what lets idiomatic stdlib code parse; if
    // mutability ever becomes checked, this is the one place that has to
    // start recording which form was written.
    // `mut` is a *contextual* keyword (grammar.ron section on contextual
    // keywords): the lexer emits IDENT and the parser promotes it, so
    // `p.eat(T![mut])` would never match.
    if p.at_contextual_kw("mut") {
        p.bump_remap(T![mut]);
    }
    patterns::pattern(p);
    if p.at(T![:]) {
        types::ascription(p);
    }
    if p.eat(T![=]) {
        expressions::expr(p);
    }

    p.eat(T![;]); // Semicolon at the end of statement belongs to the statement
    m.complete(p, LET_STMT);
}

pub(super) fn expr(p: &mut Parser<'_>) {
    let r = Restrictions {
        forbid_structs: false,
    };
    expr_bp(p, r, 1);
}

fn expr_no_struct(p: &mut Parser<'_>) {
    let r = Restrictions {
        forbid_structs: true,
    };
    expr_bp(p, r, 1);
}

fn expr_stmt(p: &mut Parser<'_>) -> (Option<CompletedMarker>, BlockLike) {
    let r = Restrictions {
        forbid_structs: false,
    };
    expr_bp(p, r, 1)
}

fn expr_bp(p: &mut Parser<'_>, r: Restrictions, bp: u8) -> (Option<CompletedMarker>, BlockLike) {
    // Parse left hand side of the expression
    let mut lhs = match lhs(p, r) {
        Some((lhs, blocklike)) => {
            if blocklike.is_block() {
                return (Some(lhs), BlockLike::Block);
            }
            lhs
        }
        None => return (None, BlockLike::NotBlock),
    };

    loop {
        let (op_bp, op) = current_op(p);
        if op_bp < bp {
            break;
        }

        let m = lhs.precede(p);

        // `as` is the one infix operator whose right operand is a *type*, not
        // an expression, so it cannot go through the generic `expr_bp`
        // recursion below.
        //
        // Associativity falls out of this shape for free: because nothing
        // recurses, control returns straight to the top of this loop with the
        // freshly completed `CAST_EXPR` as the new `lhs`, so `x as i32 as i64`
        // nests left as `((x as i32) as i64)`. Chained casts matter -- the
        // stdlib leans on them (`b as u32 as u64`).
        if op == T![as] {
            p.bump(T![as]);
            types::cast_type(p);
            lhs = m.complete(p, CAST_EXPR);
            continue;
        }

        p.bump(op);

        expr_bp(p, r, op_bp + 1);
        let kind = if op == T![<-] {
            CHANNEL_SEND_EXPR
        } else {
            BIN_EXPR
        };
        lhs = m.complete(p, kind);
    }

    (Some(lhs), BlockLike::NotBlock)
}

/// The binding power and normalized token of the infix operator the parser is
/// sitting on, or `(0, T![_])` when it is not sitting on one at all.
///
/// Binding powers in use, loosest to tightest:
///
/// |  bp | operators                     |
/// |-----|-------------------------------|
/// |   1 | `=` and compound assignments  |
/// |   2 | `<-` (channel send)           |
/// |   3 | `\|\|`                        |
/// |   4 | `&&`                          |
/// |   5 | `==` `!=` `<` `>` `<=` `>=`   |
/// |   6 | `\|`                          |
/// |   7 | `^`                           |
/// |   8 | `&`                           |
/// |   9 | `<<` `>>`                     |
/// |  10 | `+` `-`                       |
/// |  11 | `*` `/` `%`                   |
/// |  12 | `as`                          |
fn current_op(p: &Parser<'_>) -> (u8, SyntaxKind) {
    match p.current() {
        // `expr as Type` (see `expr_bp`). `as` sits at 12, one above `*`/`/`/`%`
        // at 11 and therefore above *every* binary operator, so a cast claims
        // exactly its adjacent operand and nothing more: `a as i32 + b` is
        // `(a as i32) + b`, never `a as (i32 + b)`, and `a + b as i32` is
        // `a + (b as i32)`.
        //
        // It is deliberately *below* the 255 that `lhs` passes down for the
        // operand of a prefix operator, which leaves `as` looser than the
        // unary and postfix operators: `-x as i32` is `(-x) as i32`, and
        // `f() as u8` casts the call's result rather than the callee. That is
        // the layering Rust uses, and the one the stdlib's `as` uses assume.
        T![as] => (12, T![as]),
        T![+] if p.at(T![+=]) => (1, T![+=]),
        T![+] => (10, T![+]),
        T![-] if p.at(T![-=]) => (1, T![-=]),
        T![-] => (10, T![-]),
        T![*] if p.at(T![*=]) => (1, T![*=]),
        T![*] => (11, T![*]),
        T![/] if p.at(T![/=]) => (1, T![/=]),
        T![/] => (11, T![/]),
        T![%] if p.at(T![%=]) => (1, T![%=]),
        T![%] => (11, T![%]),
        T![&] if p.at(T![&=]) => (1, T![&=]),
        T![&] if p.at(T![&&]) => (4, T![&&]),
        T![&] => (8, T![&]),
        T![|] if p.at(T![||]) => (3, T![||]),
        T![|] if p.at(T![|=]) => (1, T![|=]),
        T![|] => (6, T![|]),
        T![^] if p.at(T![^=]) => (1, T![^=]),
        T![^] => (7, T![^]),
        T![=] if p.at(T![==]) => (5, T![==]),
        T![=] => (1, T![=]),
        T![!] if p.at(T![!=]) => (5, T![!=]),
        T![>] if p.at(T![>>=]) => (1, T![>>=]),
        T![>] if p.at(T![>>]) => (9, T![>>]),
        T![>] if p.at(T![>=]) => (5, T![>=]),
        T![>] => (5, T![>]),
        T![<] if p.at(T![<=]) => (5, T![<=]),
        // `<-` channel send: binds looser than the arithmetic/logical
        // operators (so `ch <- x + 1` sends the whole sum) but tighter than
        // assignment, letting `ch <- x` appear as a standalone statement.
        T![<] if p.at(T![<-]) => (2, T![<-]),
        T![<] if p.at(T![<<=]) => (1, T![<<=]),
        T![<] if p.at(T![<<]) => (9, T![<<]),
        T![<] => (5, T![<]),
        _ => (0, T![_]),
    }
}

fn lhs(p: &mut Parser<'_>, r: Restrictions) -> Option<(CompletedMarker, BlockLike)> {
    let m;
    let kind = match p.current() {
        T![-] | T![!] | T![~] => {
            m = p.start();
            p.bump_any();
            PREFIX_EXPR
        }
        // `<-chan` receives the next value from a channel (Go-style, see
        // spec/LANGUAGE_SPEC.md section 14). Parse-level scaffolding only.
        T![<] if p.at(T![<-]) => {
            m = p.start();
            p.bump(T![<-]);
            CHANNEL_RECV_EXPR
        }
        _ => {
            let (lhs, blocklike) = atom_expr(p, r)?;
            return Some(postfix_expr(p, lhs, blocklike, !blocklike.is_block()));
        }
    };
    expr_bp(p, r, 255);
    Some((m.complete(p, kind), BlockLike::NotBlock))
}

fn postfix_expr(
    p: &mut Parser<'_>,
    mut lhs: CompletedMarker,
    // Calls are disallowed if the type is a block and we prefer statements because the call cannot
    // be disambiguated from a tuple E.g. `while true {break}();` is parsed as
    // `while true {break}; ();`
    mut blocklike: BlockLike,
    mut allow_calls: bool,
) -> (CompletedMarker, BlockLike) {
    loop {
        lhs = match p.current() {
            T!['('] if allow_calls => call_expr(p, lhs),
            T!['['] if allow_calls => index_expr(p, lhs),
            T![.] => postfix_dot_expr(p, lhs),
            INDEX => field_expr(p, lhs),
            T![!] if !p.at(T![!=]) => force_unwrap_expr(p, lhs),
            // Postfix `x^` transfers the value. Disambiguated from binary XOR
            // (`a ^ b`) by lookahead: a postfix transfer only fires when the
            // `^` is NOT followed by another expression-start token (and is
            // not the start of `^=`). Parse-level scaffolding only.
            T![^] if !p.at(T![^=]) && !EXPR_FIRST.contains(p.nth(1)) => transfer_expr(p, lhs),
            _ => break,
        };
        allow_calls = true;
        blocklike = BlockLike::NotBlock;
    }
    (lhs, blocklike)
}

fn call_expr(p: &mut Parser<'_>, lhs: CompletedMarker) -> CompletedMarker {
    assert!(p.at(T!['(']));
    let m = lhs.precede(p);
    arg_list(p);
    m.complete(p, CALL_EXPR)
}

fn method_call_expr(p: &mut Parser<'_>, lhs: CompletedMarker) -> CompletedMarker {
    let m = lhs.precede(p);
    p.bump(T![.]);
    name_ref(p);
    if p.at(T!['(']) {
        arg_list(p);
    } else {
        p.error("expected argument list");
    }

    m.complete(p, METHOD_CALL_EXPR)
}

/// `expr!` force-unwraps an optional (`Type?`), panicking with a clear
/// message on `nil` rather than invoking undefined behavior.
fn force_unwrap_expr(p: &mut Parser<'_>, lhs: CompletedMarker) -> CompletedMarker {
    assert!(p.at(T![!]));
    let m = lhs.precede(p);
    p.bump(T![!]);
    m.complete(p, TRY_EXPR)
}

/// `expr^` transfers a value into a new temporary in Mojo style: the source
/// binding is moved into the result and no longer usable. See
/// `spec/LANGUAGE_SPEC.md` section 14. Parse-level scaffolding only -- there is
/// no ownership/move analysis yet, so this produces the same tree shape as an
/// infix XOR would, distinguished only by the surrounding tokens.
fn transfer_expr(p: &mut Parser<'_>, lhs: CompletedMarker) -> CompletedMarker {
    assert!(p.at(T![^]));
    let m = lhs.precede(p);
    p.bump(T![^]);
    m.complete(p, TRANSFER_EXPR)
}

fn index_expr(p: &mut Parser<'_>, lhs: CompletedMarker) -> CompletedMarker {
    assert!(p.at(T!['[']));
    let m = lhs.precede(p);
    p.bump(T!['[']);
    expr(p);
    p.expect(T![']']);
    m.complete(p, INDEX_EXPR)
}

fn arg_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['(']));
    let m = p.start();
    p.bump(T!['(']);
    while !p.at(T![')']) && !p.at(EOF) {
        if !p.at_ts(EXPR_FIRST) {
            p.error("expected expression");
            break;
        }

        expr(p);
        if !p.at(T![')']) && !p.expect(T![,]) {
            break;
        }
    }
    p.eat(T![')']);
    m.complete(p, ARG_LIST);
}

/// Parses an attribute's argument list, e.g. `@heal(on: [Timeout], strategies:
/// [Retry])`. Arguments may optionally be prefixed by `name:`, matching
/// healing-contract-style named attribute arguments; the name is recorded as a
/// loose token pair ahead of the argument expression rather than a dedicated
/// named-argument node.
pub(super) fn attribute_arg_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['(']));
    let m = p.start();
    p.bump(T!['(']);
    while !p.at(T![')']) && !p.at(EOF) {
        if p.at(IDENT) && p.nth(1) == T![:] {
            p.bump(IDENT);
            p.bump(T![:]);
        }
        if !p.at_ts(EXPR_FIRST) {
            p.error("expected expression");
            break;
        }

        expr(p);
        if !p.at(T![')']) && !p.expect(T![,]) {
            break;
        }
    }
    p.eat(T![')']);
    m.complete(p, ARG_LIST);
}

fn postfix_dot_expr(p: &mut Parser<'_>, lhs: CompletedMarker) -> CompletedMarker {
    assert!(p.at(T![.]));
    if p.nth(1) == IDENT && p.nth(2) == T!['('] {
        return method_call_expr(p, lhs);
    }

    field_expr(p, lhs)
}

fn field_expr(p: &mut Parser<'_>, lhs: CompletedMarker) -> CompletedMarker {
    assert!(p.at(T![.]) || p.at(INDEX));
    let m = lhs.precede(p);
    if p.at(T![.]) {
        p.bump(T![.]);
        if p.at(IDENT) || p.at(INT_NUMBER) {
            name_ref_or_index(p);
        } else {
            p.error("expected field name or number");
        }
    } else if p.at(INDEX) {
        p.bump(INDEX);
    } else {
        p.error("expected field name or number");
    }
    m.complete(p, FIELD_EXPR)
}

fn atom_expr(p: &mut Parser<'_>, r: Restrictions) -> Option<(CompletedMarker, BlockLike)> {
    if let Some(m) = literal(p) {
        return Some((m, BlockLike::NotBlock));
    }

    if paths::is_path_start(p) {
        return Some(path_expr(p, r));
    }

    let marker = match p.current() {
        T!['('] => paren_expr(p),
        T!['{'] => block_expr(p),
        T!['['] => array_expr(p),
        T![if] => if_expr(p),
        T![loop] => loop_expr(p),
        T![return] => ret_expr(p),
        T![while] => while_expr(p),
        T![break] => break_expr(p, r),
        T![comptime] => comptime_expr(p),
        T![match] => match_expr(p),
        T![perform] => perform_expr(p),
        T![handle] => handle_expr(p),
        T![spawn] => spawn_expr(p),
        _ => {
            p.error_recover("expected expression", EXPR_RECOVERY_SET);
            return None;
        }
    };
    let blocklike = match marker.kind() {
        IF_EXPR | WHILE_EXPR | LOOP_EXPR | BLOCK_EXPR | COMPTIME_EXPR | MATCH_EXPR
        | HANDLE_EXPR => BlockLike::Block,
        _ => BlockLike::NotBlock,
    };
    Some((marker, blocklike))
}

/// `comptime { .. }`: the block must be fully evaluable at compile time.
fn comptime_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![comptime]));
    let m = p.start();
    p.bump(T![comptime]);
    block(p);
    m.complete(p, COMPTIME_EXPR)
}

/// `match expr { pat -> expr, .. }`.
fn match_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![match]));
    let m = p.start();
    p.bump(T![match]);
    expr_no_struct(p);
    if p.at(T!['{']) {
        match_arm_list(p);
    } else {
        p.error("expected `{`");
    }
    m.complete(p, MATCH_EXPR)
}

fn match_arm_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    while !p.at(EOF) && !p.at(T!['}']) {
        let arm = p.start();
        patterns::pattern(p);
        if p.expect(T![->]) {
            expr(p);
        }
        if !p.at(T!['}']) {
            p.eat(T![,]);
        }
        arm.complete(p, MATCH_ARM);
    }
    p.expect(T!['}']);
    m.complete(p, MATCH_ARM_LIST);
}

/// `spawn <expr>`: launches `expr` (typically a call or block) as a
/// concurrently-scheduled task, Go-`go`-statement style (see
/// `spec/LANGUAGE_SPEC.md` section 14). Parse-level scaffolding only: there
/// is no green-thread/work-stealing scheduler backing this yet, so it does
/// not actually run anything concurrently.
fn spawn_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![spawn]));
    let m = p.start();
    p.bump(T![spawn]);
    expr(p);
    m.complete(p, SPAWN_EXPR)
}

/// `perform Effect.op(args)`: invokes an algebraic effect operation.
fn perform_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![perform]));
    let m = p.start();
    p.bump(T![perform]);
    paths::type_path(p);
    if p.at(T!['(']) {
        arg_list(p);
    } else {
        p.error("expected argument list");
    }
    m.complete(p, PERFORM_EXPR)
}

/// `handle expr { Effect.op(args) -> body, .. }`: installs handlers for the
/// effects `expr` may perform.
fn handle_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![handle]));
    let m = p.start();
    p.bump(T![handle]);
    expr_no_struct(p);
    if p.at(T!['{']) {
        handler_arm_list(p);
    } else {
        p.error("expected `{`");
    }
    m.complete(p, HANDLE_EXPR)
}

fn handler_arm_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    while !p.at(EOF) && !p.at(T!['}']) {
        handler_arm(p);
    }
    p.expect(T!['}']);
    m.complete(p, HANDLER_ARM_LIST);
}

fn handler_arm(p: &mut Parser<'_>) {
    let m = p.start();
    paths::type_path(p);
    if p.at(T!['(']) {
        handler_param_list(p);
    }
    if p.expect(T![->]) {
        expr(p);
    }
    if !p.at(T!['}']) {
        p.eat(T![,]);
    }
    m.complete(p, HANDLER_ARM);
}

/// The (untyped) parameter list on a handler arm, e.g. `(message)` in
/// `Logger.log(message) -> { .. }`; types are inferred from the effect's
/// operation signature rather than re-declared at the handler site.
fn handler_param_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['(']));
    let m = p.start();
    p.bump(T!['(']);
    while !p.at(T![')']) && !p.at(EOF) {
        let param = p.start();
        patterns::pattern(p);
        param.complete(p, PARAM);
        if !p.at(T![')']) && !p.expect(T![,]) {
            break;
        }
    }
    p.expect(T![')']);
    m.complete(p, PARAM_LIST);
}

fn path_expr(p: &mut Parser<'_>, r: Restrictions) -> (CompletedMarker, BlockLike) {
    assert!(paths::is_path_start(p));
    let m = p.start();
    paths::expr_path(p);
    match p.current() {
        T!['{'] if !r.forbid_structs => {
            let m = m.complete(p, PATH_TYPE).precede(p);
            record_field_list(p);
            (m.complete(p, RECORD_LIT), BlockLike::NotBlock)
        }
        _ => (m.complete(p, PATH_EXPR), BlockLike::NotBlock),
    }
}

pub(super) fn literal(p: &mut Parser<'_>) -> Option<CompletedMarker> {
    if !p.at_ts(LITERAL_FIRST) {
        return None;
    }
    let m = p.start();
    p.bump_any();
    Some(m.complete(p, LITERAL))
}

fn paren_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T!['(']));
    let m = p.start();
    p.bump(T!['(']);
    expr(p);
    p.expect(T![')']);
    m.complete(p, PAREN_EXPR)
}

fn if_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![if]));
    let m = p.start();
    p.bump(T![if]);
    cond(p);
    block(p);
    if p.at(T![else]) {
        p.bump(T![else]);
        if p.at(T![if]) {
            if_expr(p);
        } else {
            block(p);
        }
    }
    m.complete(p, IF_EXPR)
}

fn loop_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![loop]));
    let m = p.start();
    p.bump(T![loop]);
    block(p);
    m.complete(p, LOOP_EXPR)
}

fn cond(p: &mut Parser<'_>) {
    let m = p.start();
    expr_no_struct(p);
    m.complete(p, CONDITION);
}

fn ret_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![return]));
    let m = p.start();
    p.bump(T![return]);
    if p.at_ts(EXPR_FIRST) {
        expr(p);
    }
    m.complete(p, RETURN_EXPR)
}

fn break_expr(p: &mut Parser<'_>, r: Restrictions) -> CompletedMarker {
    assert!(p.at(T![break]));
    let m = p.start();
    p.bump(T![break]);
    if p.at_ts(EXPR_FIRST) && !(r.forbid_structs && p.at(T!['{'])) {
        expr(p);
    }
    m.complete(p, BREAK_EXPR)
}

fn while_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T![while]));
    let m = p.start();
    p.bump(T![while]);
    cond(p);
    block(p);
    m.complete(p, WHILE_EXPR)
}

fn record_field_list(p: &mut Parser<'_>) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.bump(T!['{']);
    while !p.at(EOF) && !p.at(T!['}']) {
        match p.current() {
            IDENT | INT_NUMBER => {
                let m = p.start();
                name_ref_or_index(p);
                if p.eat(T![:]) {
                    expr(p);
                }
                m.complete(p, RECORD_FIELD);
            }
            T!['{'] => error_block(p, "expected a field"),
            _ => p.error_and_bump("expected an identifier"),
        }
        if !p.at(T!['}']) {
            p.expect(T![,]);
        }
    }
    p.expect(T!['}']);
    m.complete(p, RECORD_FIELD_LIST);
}

fn array_expr(p: &mut Parser<'_>) -> CompletedMarker {
    assert!(p.at(T!['[']));
    let m = p.start();

    p.bump(T!['[']);
    while !p.at(EOF) && !p.at(T![']']) {
        expr(p);

        if !p.at(T![']']) && !p.expect(T![,]) {
            break;
        }
    }
    p.expect(T![']']);

    m.complete(p, ARRAY_EXPR)
}
