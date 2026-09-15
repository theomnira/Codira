//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::fmt::Write;

fn dump_tokens(tokens: &[crate::Token], text: &str) -> String {
    let mut acc = String::new();
    let mut offset = 0;
    for token in tokens {
        let len: u32 = token.len.into();
        let len = len as usize;
        let token_text = &text[offset..offset + len];
        offset += len;
        writeln!(acc, "{:?} {} {:?}", token.kind, len, token_text).unwrap();
    }
    acc
}

fn dump_text_tokens(text: &str) -> String {
    let tokens = crate::tokenize(text);
    dump_tokens(&tokens, text)
}

#[test]
fn numbers() {
    insta::assert_snapshot!(dump_text_tokens(
        r#"
    1.34
    0x3Af
    1e-3
    100_000
    0x3a_u32
    1f32
    0o71234"#), @r#"
    WHITESPACE 5 "\n    "
    FLOAT_NUMBER 4 "1.34"
    WHITESPACE 5 "\n    "
    INT_NUMBER 5 "0x3Af"
    WHITESPACE 5 "\n    "
    FLOAT_NUMBER 4 "1e-3"
    WHITESPACE 5 "\n    "
    INT_NUMBER 7 "100_000"
    WHITESPACE 5 "\n    "
    INT_NUMBER 8 "0x3a_u32"
    WHITESPACE 5 "\n    "
    INT_NUMBER 4 "1f32"
    WHITESPACE 5 "\n    "
    INT_NUMBER 7 "0o71234"
    "#);
}

#[test]
fn comments() {
    insta::assert_snapshot!(dump_text_tokens(
        r#"
    // hello, world!
    /**/
    /* block comment */
    /* multi
       line
       comment */
    /* /* nested */ */
    /* unclosed comment"#), @r#"
    WHITESPACE 5 "\n    "
    COMMENT 16 "// hello, world!"
    WHITESPACE 5 "\n    "
    COMMENT 4 "/**/"
    WHITESPACE 5 "\n    "
    COMMENT 19 "/* block comment */"
    WHITESPACE 5 "\n    "
    COMMENT 38 "/* multi\n       line\n       comment */"
    WHITESPACE 5 "\n    "
    COMMENT 18 "/* /* nested */ */"
    WHITESPACE 5 "\n    "
    COMMENT 19 "/* unclosed comment"
    "#);
}

#[test]
fn whitespace() {
    insta::assert_snapshot!(dump_text_tokens(
        r#"
    h e ll  o
    w

    o r     l   d"#), @r#"
    WHITESPACE 5 "\n    "
    IDENT 1 "h"
    WHITESPACE 1 " "
    IDENT 1 "e"
    WHITESPACE 1 " "
    IDENT 2 "ll"
    WHITESPACE 2 "  "
    IDENT 1 "o"
    WHITESPACE 5 "\n    "
    IDENT 1 "w"
    WHITESPACE 6 "\n\n    "
    IDENT 1 "o"
    WHITESPACE 1 " "
    IDENT 1 "r"
    WHITESPACE 5 "     "
    IDENT 1 "l"
    WHITESPACE 3 "   "
    IDENT 1 "d"
    "#);
}

#[test]
fn ident() {
    insta::assert_snapshot!(dump_text_tokens(
        r#"
    hello world_ _a2 _ __ x 即可编著课程
    "#), @r#"
    WHITESPACE 5 "\n    "
    IDENT 5 "hello"
    WHITESPACE 1 " "
    IDENT 6 "world_"
    WHITESPACE 1 " "
    IDENT 3 "_a2"
    WHITESPACE 1 " "
    UNDERSCORE 1 "_"
    WHITESPACE 1 " "
    IDENT 2 "__"
    WHITESPACE 1 " "
    IDENT 1 "x"
    WHITESPACE 1 " "
    IDENT 18 "即可编著课程"
    WHITESPACE 5 "\n    "
    "#);
}

#[test]
fn symbols() {
    insta::assert_snapshot!(dump_text_tokens(
        r#"
    # ( ) { } [ ] ; ,
    = ==
    !=
    < <=
    > >=
    . .. ... ..=
    + +=
    - -=
    * *=
    / /=
    % %=
    << <<=
    >> >>=
    && & &=
    || | |=
    ^ ^=
    : ?
    ->
    @
    "#), @r##"
    WHITESPACE 5 "\n    "
    HASH 1 "#"
    WHITESPACE 1 " "
    L_PAREN 1 "("
    WHITESPACE 1 " "
    R_PAREN 1 ")"
    WHITESPACE 1 " "
    L_CURLY 1 "{"
    WHITESPACE 1 " "
    R_CURLY 1 "}"
    WHITESPACE 1 " "
    L_BRACKET 1 "["
    WHITESPACE 1 " "
    R_BRACKET 1 "]"
    WHITESPACE 1 " "
    SEMI 1 ";"
    WHITESPACE 1 " "
    COMMA 1 ","
    WHITESPACE 5 "\n    "
    EQ 1 "="
    WHITESPACE 1 " "
    EQ 1 "="
    EQ 1 "="
    WHITESPACE 5 "\n    "
    EXCLAMATION 1 "!"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    LT 1 "<"
    WHITESPACE 1 " "
    LT 1 "<"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    GT 1 ">"
    WHITESPACE 1 " "
    GT 1 ">"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    DOT 1 "."
    WHITESPACE 1 " "
    DOT 1 "."
    DOT 1 "."
    WHITESPACE 1 " "
    DOT 1 "."
    DOT 1 "."
    DOT 1 "."
    WHITESPACE 1 " "
    DOT 1 "."
    DOT 1 "."
    EQ 1 "="
    WHITESPACE 5 "\n    "
    PLUS 1 "+"
    WHITESPACE 1 " "
    PLUS 1 "+"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    MINUS 1 "-"
    WHITESPACE 1 " "
    MINUS 1 "-"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    STAR 1 "*"
    WHITESPACE 1 " "
    STAR 1 "*"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    SLASH 1 "/"
    WHITESPACE 1 " "
    SLASH 1 "/"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    PERCENT 1 "%"
    WHITESPACE 1 " "
    PERCENT 1 "%"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    LT 1 "<"
    LT 1 "<"
    WHITESPACE 1 " "
    LT 1 "<"
    LT 1 "<"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    GT 1 ">"
    GT 1 ">"
    WHITESPACE 1 " "
    GT 1 ">"
    GT 1 ">"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    AMP 1 "&"
    AMP 1 "&"
    WHITESPACE 1 " "
    AMP 1 "&"
    WHITESPACE 1 " "
    AMP 1 "&"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    PIPE 1 "|"
    PIPE 1 "|"
    WHITESPACE 1 " "
    PIPE 1 "|"
    WHITESPACE 1 " "
    PIPE 1 "|"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    CARET 1 "^"
    WHITESPACE 1 " "
    CARET 1 "^"
    EQ 1 "="
    WHITESPACE 5 "\n    "
    COLON 1 ":"
    WHITESPACE 1 " "
    QUESTION 1 "?"
    WHITESPACE 5 "\n    "
    MINUS 1 "-"
    GT 1 ">"
    WHITESPACE 5 "\n    "
    AT 1 "@"
    WHITESPACE 5 "\n    "
    "##);
}

#[test]
fn strings() {
    insta::assert_snapshot!(dump_text_tokens(
        r#"
    "Hello, world!"
    'Hello, world!'
    "\n"
    "\"\\"
    "multi
    line"
    "#), @r#"
    WHITESPACE 5 "\n    "
    STRING 15 "\"Hello, world!\""
    WHITESPACE 5 "\n    "
    STRING 15 "'Hello, world!'"
    WHITESPACE 5 "\n    "
    STRING 4 "\"\\n\""
    WHITESPACE 5 "\n    "
    STRING 6 "\"\\\"\\\\\""
    WHITESPACE 5 "\n    "
    STRING 16 "\"multi\n    line\""
    WHITESPACE 5 "\n    "
    "#);
}

#[test]
fn keywords() {
    insta::assert_snapshot!(dump_text_tokens(
        r#"
    break do else false for func if in nil
    return true while let var struct class
    never loop public internal super self root type
    extend trait enum match comptime macro
    effect uses perform handle resume where
    override init static
    "#), @r###"
    WHITESPACE 5 "\n    "
    BREAK_KW 5 "break"
    WHITESPACE 1 " "
    DO_KW 2 "do"
    WHITESPACE 1 " "
    ELSE_KW 4 "else"
    WHITESPACE 1 " "
    FALSE_KW 5 "false"
    WHITESPACE 1 " "
    FOR_KW 3 "for"
    WHITESPACE 1 " "
    FUNC_KW 4 "func"
    WHITESPACE 1 " "
    IF_KW 2 "if"
    WHITESPACE 1 " "
    IN_KW 2 "in"
    WHITESPACE 1 " "
    NIL_KW 3 "nil"
    WHITESPACE 5 "\n    "
    RETURN_KW 6 "return"
    WHITESPACE 1 " "
    TRUE_KW 4 "true"
    WHITESPACE 1 " "
    WHILE_KW 5 "while"
    WHITESPACE 1 " "
    LET_KW 3 "let"
    WHITESPACE 1 " "
    VAR_KW 3 "var"
    WHITESPACE 1 " "
    STRUCT_KW 6 "struct"
    WHITESPACE 1 " "
    CLASS_KW 5 "class"
    WHITESPACE 5 "\n    "
    NEVER_KW 5 "never"
    WHITESPACE 1 " "
    LOOP_KW 4 "loop"
    WHITESPACE 1 " "
    PUBLIC_KW 6 "public"
    WHITESPACE 1 " "
    INTERNAL_KW 8 "internal"
    WHITESPACE 1 " "
    SUPER_KW 5 "super"
    WHITESPACE 1 " "
    SELF_KW 4 "self"
    WHITESPACE 1 " "
    ROOT_KW 4 "root"
    WHITESPACE 1 " "
    TYPE_KW 4 "type"
    WHITESPACE 5 "\n    "
    EXTEND_KW 6 "extend"
    WHITESPACE 1 " "
    TRAIT_KW 5 "trait"
    WHITESPACE 1 " "
    ENUM_KW 4 "enum"
    WHITESPACE 1 " "
    MATCH_KW 5 "match"
    WHITESPACE 1 " "
    COMPTIME_KW 8 "comptime"
    WHITESPACE 1 " "
    MACRO_KW 5 "macro"
    WHITESPACE 5 "\n    "
    EFFECT_KW 6 "effect"
    WHITESPACE 1 " "
    USES_KW 4 "uses"
    WHITESPACE 1 " "
    PERFORM_KW 7 "perform"
    WHITESPACE 1 " "
    HANDLE_KW 6 "handle"
    WHITESPACE 1 " "
    RESUME_KW 6 "resume"
    WHITESPACE 1 " "
    WHERE_KW 5 "where"
    WHITESPACE 5 "\n    "
    OVERRIDE_KW 8 "override"
    WHITESPACE 1 " "
    INIT_KW 4 "init"
    WHITESPACE 1 " "
    STATIC_KW 6 "static"
    WHITESPACE 5 "\n    "
    "###);
}

#[test]
fn unclosed_string() {
    insta::assert_snapshot!(dump_text_tokens(
        r#"
    "test
    "#), @r#"
    WHITESPACE 5 "\n    "
    STRING 10 "\"test\n    "
    "#);
}

#[test]
fn binary_cmp() {
    insta::assert_snapshot!(dump_text_tokens(
        r#"
    a==b
    a ==b
    a== b
    a == b
    "#), @r#"
    WHITESPACE 5 "\n    "
    IDENT 1 "a"
    EQ 1 "="
    EQ 1 "="
    IDENT 1 "b"
    WHITESPACE 5 "\n    "
    IDENT 1 "a"
    WHITESPACE 1 " "
    EQ 1 "="
    EQ 1 "="
    IDENT 1 "b"
    WHITESPACE 5 "\n    "
    IDENT 1 "a"
    EQ 1 "="
    EQ 1 "="
    WHITESPACE 1 " "
    IDENT 1 "b"
    WHITESPACE 5 "\n    "
    IDENT 1 "a"
    WHITESPACE 1 " "
    EQ 1 "="
    EQ 1 "="
    WHITESPACE 1 " "
    IDENT 1 "b"
    WHITESPACE 5 "\n    "
    "#);
}
