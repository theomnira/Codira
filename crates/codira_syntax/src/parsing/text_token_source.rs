//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.

use crate::{
    parsing::{lexer::Token, Token as PToken, TokenSource},
    SyntaxKind::EOF,
    TextRange, TextSize,
};

/// An implementation of `TokenSource` for text.
pub(crate) struct TextTokenSource<'t> {
    #[allow(dead_code)]
    text: &'t str,
    /// start position of each token(expect whitespace and comment)
    /// ```non-rust
    ///  struct Foo;
    /// ^------^---
    /// |      |  ^-
    /// 0      7  10
    /// ```
    /// (token, `start_offset)`: `[(struct, 0), (Foo, 7), (;, 10)]`
    start_offsets: Vec<TextSize>,
    /// non-whitespace/comment tokens
    /// ```non-rust
    /// struct Foo {}
    /// ^^^^^^ ^^^ ^^
    /// ```
    /// tokens: `[struct, Foo, {, }]`
    tokens: Vec<Token>,

    /// Parallel to `tokens`: whether the trivia skipped before each token
    /// contained a line break. Precomputed here rather than re-scanned per
    /// lookahead, since the parser asks about it on every postfix operator.
    /// See `parsing::Token::has_newline_before` for what it is for.
    preceded_by_newline: Vec<bool>,

    /// Current token and position
    curr: (PToken, usize),
}

impl TokenSource for TextTokenSource<'_> {
    fn lookahead_nth(&self, n: usize) -> PToken {
        mk_token(
            self.curr.1 + n,
            &self.start_offsets,
            &self.tokens,
            &self.preceded_by_newline,
        )
    }

    fn bump(&mut self) {
        if self.curr.0.kind == EOF {
            return;
        }

        let pos = self.curr.1 + 1;
        self.curr = (
            mk_token(
                pos,
                &self.start_offsets,
                &self.tokens,
                &self.preceded_by_newline,
            ),
            pos,
        );
    }

    fn is_keyword(&self, kw: &str) -> bool {
        let pos = self.curr.1;
        if pos >= self.tokens.len() {
            return false;
        }
        let range = TextRange::at(self.start_offsets[pos], self.tokens[pos].len);
        self.text[range] == *kw
    }
}

fn mk_token(
    pos: usize,
    start_offsets: &[TextSize],
    tokens: &[Token],
    preceded_by_newline: &[bool],
) -> PToken {
    let kind = tokens.get(pos).map_or(EOF, |t| t.kind);
    let is_jointed_to_next = if pos + 1 < start_offsets.len() {
        start_offsets[pos] + tokens[pos].len == start_offsets[pos + 1]
    } else {
        false
    };

    PToken {
        kind,
        is_jointed_to_next,
        // Past the end of input counts as newline-separated: EOF never
        // continues the previous line's expression.
        has_newline_before: preceded_by_newline.get(pos).copied().unwrap_or(true),
    }
}

impl<'t> TextTokenSource<'t> {
    /// Generate input from tokens(expect comment and whitespace).
    pub fn new(text: &'t str, raw_tokens: &'t [Token]) -> TextTokenSource<'t> {
        let mut tokens = Vec::new();
        let mut start_offsets = Vec::new();
        let mut preceded_by_newline = Vec::new();
        let mut len: TextSize = 0.into();
        // Set for the first real token and after any trivia containing a
        // line break; a comment spanning lines counts, since what matters is
        // whether the source moved to a new line, not what put it there.
        let mut saw_newline = true;
        for &token in raw_tokens.iter() {
            if token.kind.is_trivia() {
                let range = TextRange::at(len, token.len);
                if text[range].contains('\n') {
                    saw_newline = true;
                }
            } else {
                tokens.push(token);
                start_offsets.push(len);
                preceded_by_newline.push(saw_newline);
                saw_newline = false;
            }
            len += token.len;
        }

        let first = mk_token(0, &start_offsets, &tokens, &preceded_by_newline);
        TextTokenSource {
            text,
            start_offsets,
            tokens,
            preceded_by_newline,
            curr: (first, 0),
        }
    }
}
