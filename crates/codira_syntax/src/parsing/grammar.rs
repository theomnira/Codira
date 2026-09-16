//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
mod adt;
mod declarations;
mod expressions;
mod generics;
mod params;
mod paths;
mod patterns;
mod traits;
mod types;

use super::{
    parser::{CompletedMarker, Marker, Parser},
    token_set::TokenSet,
    SyntaxKind::{
        self, ARG_LIST, ARRAY_EXPR, ARRAY_TYPE, ATTRIBUTE, ATTRIBUTE_LIST, BIND_PAT, BIN_EXPR,
        BLOCK_EXPR, BREAK_EXPR, CALL_EXPR, CAST_EXPR, CHANNEL_RECV_EXPR, CHANNEL_SEND_EXPR,
        COMPTIME_EXPR, CONDITION, CONST_DEF, EFFECT_DEF, EFFECT_OP, EFFECT_OP_LIST, ENUM_DEF,
        ENUM_VARIANT, ENUM_VARIANT_LIST, EOF, ERROR, EXPR_STMT, EXTEND, EXTEND_ITEM_LIST, EXTERN,
        EXTERN_BLOCK, EXTERN_ITEM_LIST, FIELD_EXPR, FLOAT_NUMBER, FUNCTION_DEF, GENERIC_ARG_LIST,
        GENERIC_PARAM, GENERIC_PARAM_LIST, HANDLER_ARM, HANDLER_ARM_LIST, HANDLE_EXPR, IDENT,
        IF_EXPR, INDEX, INDEX_EXPR, INHERITANCE_LIST, INT_NUMBER, LET_STMT, LITERAL, LITERAL_PAT,
        LOOP_EXPR, MACRO_DEF, MATCH_ARM, MATCH_ARM_LIST, MATCH_EXPR, NAME, NAME_REF, NEVER_TYPE,
        OPTIONAL_TYPE, PARAM, PARAM_LIST, PAREN_EXPR, PAREN_PAT, PAREN_TYPE, PATH, PATH_EXPR,
        PATH_PAT, PATH_SEGMENT, PATH_TYPE, PERFORM_EXPR, PLACEHOLDER_PAT, PREFIX_EXPR,
        RECORD_FIELD, RECORD_FIELD_DEF, RECORD_FIELD_DEF_LIST, RECORD_FIELD_LIST, RECORD_LIT,
        REFINEMENT_TYPE, RENAME, RETURN_EXPR, RET_TYPE, SELF_PARAM, SOURCE_FILE, SPAWN_EXPR,
        STRING, STRUCT_DEF, TRAIT_DEF, TRANSFER_EXPR, TRY_EXPR, TUPLE_EXPR, TUPLE_FIELD_DEF,
        TUPLE_FIELD_DEF_LIST, TUPLE_PAT, TUPLE_STRUCT_PAT, TUPLE_TYPE, TYPE_ALIAS_DEF, USE,
        USES_CLAUSE, USE_TREE, USE_TREE_LIST, VISIBILITY, WHERE_CLAUSE, WHERE_PRED, WHILE_EXPR,
    },
};

const VISIBILITY_FIRST: TokenSet = TokenSet::new(&[T![public], T![internal]]);

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockLike {
    Block,
    NotBlock,
}

impl BlockLike {
    fn is_block(self) -> bool {
        self == BlockLike::Block
    }
}

pub(crate) fn root(p: &mut Parser<'_>) {
    let m = p.start();
    declarations::mod_contents(p);
    m.complete(p, SOURCE_FILE);
}

//pub(crate) fn pattern(p: &mut Parser<'_>) {
//    patterns::pattern(p)
//}
//
//pub(crate) fn expr(p: &mut Parser<'_>) {
//    expressions::expr(p);
//}
//
//pub(crate) fn type_(p: &mut Parser<'_>) {
//    types::type_(p)
//}

fn name_recovery(p: &mut Parser<'_>, recovery: TokenSet) {
    if p.at(IDENT) {
        let m = p.start();
        p.bump(IDENT);
        m.complete(p, NAME);
    } else {
        p.error_recover("expected a name", recovery);
    }
}

fn name(p: &mut Parser<'_>) {
    name_recovery(p, TokenSet::empty());
}

fn name_ref(p: &mut Parser<'_>) {
    if p.at(IDENT) {
        let m = p.start();
        p.bump(IDENT);
        m.complete(p, NAME_REF);
    } else {
        p.error_and_bump("expected identifier");
    }
}

fn name_ref_or_index(p: &mut Parser<'_>) {
    assert!(p.at(IDENT) || p.at(INT_NUMBER));
    let m = p.start();
    p.bump_any();
    m.complete(p, NAME_REF);
}

fn opt_visibility(p: &mut Parser<'_>) -> bool {
    match p.current() {
        T![public] | T![internal] => {
            let m = p.start();
            p.bump_any();
            m.complete(p, VISIBILITY);
            true
        }
        _ => false,
    }
}

fn opt_attribute_list(p: &mut Parser<'_>) {
    if p.at(T![@]) {
        let m = p.start();
        while p.at(T![@]) {
            attribute(p);
        }
        m.complete(p, ATTRIBUTE_LIST);
    }
}

fn attribute(p: &mut Parser<'_>) {
    assert!(p.at(T![@]));
    let m = p.start();
    p.bump(T![@]);
    paths::type_path(p);
    if p.at(T!['(']) {
        expressions::attribute_arg_list(p);
    }
    m.complete(p, ATTRIBUTE);
}

fn error_block(p: &mut Parser<'_>, message: &str) {
    assert!(p.at(T!['{']));
    let m = p.start();
    p.error(message);
    p.bump(T!['{']);
    expressions::expr_block_contents(p);
    p.eat(T!['}']);
    m.complete(p, ERROR);
}
