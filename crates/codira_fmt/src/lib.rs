//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - The official formatting standard for Codira sources (`codira fmt`).
//!
//! The formatter works on the lossless syntax tree of `codira_syntax`. It
//! rewrites *only* whitespace: every token, comment and node of the tree is
//! kept, in order. Because the Codira grammar is newline-sensitive (a line
//! break can end a statement), the formatter also keeps every line break
//! where the author put it; what it changes is
//!
//! - indentation: four spaces per open bracket group, plus one level for a
//!   continuation line (a line that starts with, or follows a line ending in, a
//!   binary operator, `=`, `->`, a `.` of a method chain, or a
//!   `requires`/`ensures`/`where`/`uses` clause);
//! - spacing inside a line: one space around binary operators, `=` and `->`,
//!   after `,`, `;` and `:`, inside `{ }`, and none inside `( )` and `[ ]`,
//!   before `,`/`;`/`:`, around `.`, after a prefix operator or `@`, and
//!   between a callee and its argument list. Pairs no rule covers keep "no
//!   space" or become one space, whichever they had;
//! - blank lines: at most one in a row, none right after an opening or before a
//!   closing bracket, none at the start of the file;
//! - trailing whitespace (also at the end of `//` comments) is removed, the
//!   file ends with exactly one newline, and line endings become `\n`.
//!
//! Every result is checked before it is returned: it must parse without
//! errors to the same tree (ignoring whitespace) with a line break before
//! exactly the same tokens. A formatting that fails that check is an error,
//! never a silently changed file. A file with syntax errors is not
//! formatted at all.

mod diff;

use std::fmt;

use codira_syntax::{tokenize, SourceFile, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};
pub use diff::{line_diff, DiffLine};

/// Why a source could not be formatted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// The source does not parse; the formatter never guesses at the
    /// structure of broken code. Each entry is `line:column: message`.
    Syntax(Vec<String>),
    /// The formatted text did not parse back to the same tree. This is a
    /// bug in the formatter; the source is left as it is.
    Verification(String),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Syntax(errors) => {
                write!(f, "not formatted: the file has syntax errors")?;
                for error in errors.iter().take(5) {
                    write!(f, "\n  {error}")?;
                }
                if errors.len() > 5 {
                    write!(f, "\n  ... and {} more", errors.len() - 5)?;
                }
                Ok(())
            }
            FormatError::Verification(message) => write!(
                f,
                "not formatted: the formatted text would not parse to the same tree ({message}); \
                 this is a formatter bug, please report it with the file"
            ),
        }
    }
}

impl std::error::Error for FormatError {}

/// Formats a Codira source file according to the official standard.
///
/// The result is idempotent (`format_source(format_source(x)) ==
/// format_source(x)`) and is verified to parse to the same tree as `text`
/// before it is returned.
pub fn format_source(text: &str) -> Result<String, FormatError> {
    let parse = SourceFile::parse(text);
    if !parse.errors().is_empty() {
        return Err(FormatError::Syntax(
            parse
                .errors()
                .iter()
                .map(|error| {
                    let offset = error.location().offset();
                    let (line, column) = line_column(text, u32::from(offset) as usize);
                    format!("{line}:{column}: {}", error.kind())
                })
                .collect(),
        ));
    }
    let root = parse.syntax_node();
    let formatted = Formatter::new(&root).run();
    check_equivalent(text, &formatted).map_err(FormatError::Verification)?;
    Ok(formatted)
}

/// Checks that `after` parses to the same tree as `before` up to
/// whitespace: the same nodes, the same tokens with the same text (`//`
/// comments compared without trailing whitespace), and a line break before
/// the same tokens. `Err` describes the first difference.
pub fn check_equivalent(before: &str, after: &str) -> Result<(), String> {
    let a = Shape::of(before);
    let b = Shape::of(after);
    if b.errors > a.errors {
        return Err(format!(
            "{} syntax error(s) before, {} after",
            a.errors, b.errors
        ));
    }
    if let Some(index) = a
        .events
        .iter()
        .zip(&b.events)
        .position(|(x, y)| x != y)
        .or_else(|| (a.events.len() != b.events.len()).then(|| a.events.len().min(b.events.len())))
    {
        return Err(format!(
            "trees differ at event {index}: {:?} before, {:?} after",
            a.events.get(index),
            b.events.get(index)
        ));
    }
    if let Some(index) = a
        .newline_before
        .iter()
        .zip(&b.newline_before)
        .position(|(x, y)| x != y)
    {
        return Err(format!(
            "line break before token {index} ({:?}) {} by formatting",
            a.tokens.get(index),
            if a.newline_before[index] {
                "removed"
            } else {
                "added"
            }
        ));
    }
    Ok(())
}

fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Enter(SyntaxKind),
    Token(SyntaxKind, String),
    Exit,
}

/// The whitespace-insensitive shape of a parse tree.
struct Shape {
    events: Vec<Event>,
    /// For each non-trivia token: whether a line break precedes it (what
    /// the parser's newline sensitivity sees).
    newline_before: Vec<bool>,
    tokens: Vec<String>,
    errors: usize,
}

impl Shape {
    fn of(text: &str) -> Shape {
        let parse = SourceFile::parse(text);
        let root = parse.syntax_node();
        let mut events = Vec::new();
        let mut newline_before = Vec::new();
        let mut tokens = Vec::new();
        let mut saw_newline = true;
        for event in root.preorder_with_tokens() {
            match event {
                codira_syntax::WalkEvent::Enter(SyntaxElement::Node(node)) => {
                    events.push(Event::Enter(node.kind()))
                }
                codira_syntax::WalkEvent::Leave(SyntaxElement::Node(_)) => events.push(Event::Exit),
                codira_syntax::WalkEvent::Enter(SyntaxElement::Token(token)) => {
                    let kind = token.kind();
                    if kind == SyntaxKind::WHITESPACE {
                        saw_newline |= token.text().contains('\n');
                        continue;
                    }
                    events.push(Event::Token(kind, comparable_text(&token).to_string()));
                    if kind != SyntaxKind::COMMENT {
                        newline_before.push(saw_newline);
                        tokens.push(token.text().to_string());
                        saw_newline = false;
                    }
                }
                codira_syntax::WalkEvent::Leave(SyntaxElement::Token(_)) => {}
            }
        }
        Shape {
            events,
            newline_before,
            tokens,
            errors: parse.errors().len(),
        }
    }
}

/// A token's text as the formatter writes it: `//` comments lose their
/// trailing whitespace, everything else is verbatim.
fn comparable_text(token: &SyntaxToken) -> &str {
    let text = token.text();
    if token.kind() == SyntaxKind::COMMENT && text.starts_with("//") {
        text.trim_end()
    } else {
        text
    }
}

const INDENT: &str = "    ";

/// One non-whitespace token with the whitespace that preceded it.
struct Item {
    token: SyntaxToken,
    gap: String,
}

/// An open bracket group: the brackets opened on one line and not yet
/// closed, which indent the lines after it by one level.
struct Group {
    open: usize,
    /// Indentation of the lines inside the group.
    indent: usize,
}

struct Formatter {
    items: Vec<Item>,
    /// Start offsets of the tokens that begin a line.
    line_starts: std::collections::HashSet<codira_syntax::TextSize>,
}

impl Formatter {
    fn new(root: &SyntaxNode) -> Formatter {
        let mut items = Vec::new();
        let mut gap = String::new();
        for element in root.descendants_with_tokens() {
            let SyntaxElement::Token(token) = element else {
                continue;
            };
            if token.kind() == SyntaxKind::WHITESPACE {
                gap.push_str(token.text());
            } else {
                items.push(Item {
                    token,
                    gap: std::mem::take(&mut gap),
                });
            }
        }
        let line_starts = items
            .iter()
            .enumerate()
            .filter(|(i, item)| *i == 0 || item.gap.contains('\n'))
            .map(|(_, item)| item.token.text_range().start())
            .collect();
        Formatter { items, line_starts }
    }

    fn run(&self) -> String {
        let mut out = String::new();
        let mut groups: Vec<Group> = Vec::new();
        // For each open bracket: the index of the group it belongs to.
        let mut brackets: Vec<usize> = Vec::new();
        let mut line_group: Option<usize> = None;
        // Indentation of the current line.
        let mut line_indent = 0usize;

        for (i, item) in self.items.iter().enumerate() {
            let kind = item.token.kind();
            let newlines = item.gap.matches('\n').count();
            if i == 0 {
                // No whitespace before the first token of the file.
                line_indent = 0;
                line_group = None;
            } else if newlines > 0 {
                let prev = &self.items[i - 1].token;
                // The last token of the previous line that is not a comment
                // trailing it.
                let line_end = if prev.kind() == SyntaxKind::COMMENT
                    && i >= 2
                    && !self.items[i - 1].gap.contains('\n')
                {
                    &self.items[i - 2].token
                } else {
                    prev
                };
                let keep_blank = newlines >= 2
                    && !(is_opener(line_end.kind()) && kind != SyntaxKind::COMMENT)
                    && !is_closer(kind)
                    && !is_attached_doc_gap(prev, &item.token);
                out.push('\n');
                if keep_blank {
                    out.push('\n');
                }
                let base = if is_closer(kind) {
                    // A line starting with a closer lines up with the line
                    // that opened its bracket.
                    brackets
                        .last()
                        .map_or(0, |&g| groups[g].indent.saturating_sub(1))
                } else {
                    groups.last().map_or(0, |g| g.indent)
                };
                line_indent = base + usize::from(self.is_continuation(i));
                for _ in 0..line_indent {
                    out.push_str(INDENT);
                }
                line_group = None;
            } else {
                out.push_str(self.space(i));
            }

            out.push_str(comparable_text(&item.token));

            if is_opener(kind) {
                let g = match line_group {
                    Some(g) => g,
                    None => {
                        groups.push(Group {
                            open: 0,
                            indent: line_indent + 1,
                        });
                        line_group = Some(groups.len() - 1);
                        groups.len() - 1
                    }
                };
                groups[g].open += 1;
                brackets.push(g);
            } else if is_closer(kind) {
                if let Some(g) = brackets.pop() {
                    groups[g].open -= 1;
                }
                while groups.last().is_some_and(|g| g.open == 0) {
                    groups.pop();
                }
                if line_group.is_some_and(|g| g >= groups.len()) {
                    line_group = None;
                }
            }
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// Whether the line starting at item `i` continues the previous one and
    /// is indented one more level.
    fn is_continuation(&self, i: usize) -> bool {
        let token = &self.items[i].token;
        let kind = token.kind();
        if kind == SyntaxKind::L_CURLY || kind == SyntaxKind::COMMENT {
            return false;
        }
        let parent = parent_kind(token);
        if kind == SyntaxKind::ELSE_KW {
            // `else` on its own line continues an `if` that is itself in
            // the middle of a line (`let x = if c { 1 }` / `else { 2 }`);
            // under an `if` that starts a line it lines up with the `if`.
            let outermost_if = token
                .parent_ancestors()
                .take_while(|n| n.kind() == SyntaxKind::IF_EXPR)
                .last();
            return outermost_if
                .and_then(|n| first_token(&n))
                .is_some_and(|t| !self.line_starts.contains(&t.text_range().start()));
        }
        if is_binary_operator(token) {
            return true;
        }
        if kind == SyntaxKind::DOT
            && matches!(
                parent,
                Some(SyntaxKind::FIELD_EXPR | SyntaxKind::METHOD_CALL_EXPR)
            )
        {
            return true;
        }
        if let Some(parent) = token.parent() {
            if matches!(
                parent.kind(),
                SyntaxKind::REQUIRES_CLAUSE
                    | SyntaxKind::ENSURES_CLAUSE
                    | SyntaxKind::WHERE_CLAUSE
                    | SyntaxKind::USES_CLAUSE
                    | SyntaxKind::RET_TYPE
                    | SyntaxKind::INVARIANT_CLAUSE
                    | SyntaxKind::DECREASES_CLAUSE
            ) && first_token(&parent).as_ref() == Some(token)
            {
                return true;
            }
        }
        // The previous line ends in an operator that needs a right operand.
        let prev = &self.items[i - 1].token;
        if prev.kind() == SyntaxKind::COMMENT {
            return false;
        }
        is_binary_operator(prev)
            || (is_assignment(prev.kind()) && parent_kind(prev) != Some(SyntaxKind::ATTRIBUTE))
            || prev.kind() == SyntaxKind::THIN_ARROW
    }

    /// The whitespace between items `i - 1` and `i` on the same line.
    fn space(&self, i: usize) -> &'static str {
        let a = &self.items[i - 1].token;
        let b = &self.items[i].token;
        let gap = self.items[i].gap.as_str();
        let (ka, kb) = (a.kind(), b.kind());
        let had_space = !gap.is_empty();
        let keep = if had_space { " " } else { "" };

        if kb == SyntaxKind::COMMENT {
            // Keep the author's alignment of trailing comments; tabs and
            // other whitespace become one space.
            if gap.bytes().all(|b| b == b' ') {
                return spaces(gap.len());
            }
            return " ";
        }
        if ka == SyntaxKind::COMMENT {
            return keep;
        }
        // Never join two tokens that would lex or parse as one.
        let can_join = can_join(a.text(), b.text());
        let none = if can_join { "" } else { keep };

        if matches!(kb, SyntaxKind::COMMA | SyntaxKind::SEMI) {
            return none;
        }
        if ka == SyntaxKind::COMMA {
            return if matches!(kb, SyntaxKind::R_PAREN | SyntaxKind::R_BRACKET) {
                none
            } else {
                " "
            };
        }
        if ka == SyntaxKind::SEMI {
            return " ";
        }
        if matches!(ka, SyntaxKind::L_PAREN | SyntaxKind::L_BRACKET)
            || matches!(kb, SyntaxKind::R_PAREN | SyntaxKind::R_BRACKET)
        {
            return none;
        }
        if ka == SyntaxKind::L_CURLY && kb == SyntaxKind::R_CURLY {
            return none;
        }
        if kb == SyntaxKind::L_CURLY && ka == SyntaxKind::DOT {
            // `import root.module.{ a, b }`
            return none;
        }
        if ka == SyntaxKind::L_CURLY || kb == SyntaxKind::R_CURLY || kb == SyntaxKind::L_CURLY {
            return " ";
        }
        if kb == SyntaxKind::COLON && is_typed_colon(b) {
            return none;
        }
        if ka == SyntaxKind::COLON && is_typed_colon(a) {
            return " ";
        }
        if ka == SyntaxKind::THIN_ARROW || kb == SyntaxKind::THIN_ARROW {
            return " ";
        }
        let in_attribute = |t: &SyntaxToken| parent_kind(t) == Some(SyntaxKind::ATTRIBUTE);
        if (is_assignment(ka) && !in_attribute(a)) || (is_assignment(kb) && !in_attribute(b)) {
            return " ";
        }
        if is_binary_operator(a) || is_binary_operator(b) {
            return " ";
        }
        if parent_kind(a) == Some(SyntaxKind::PREFIX_EXPR) && is_operator_token(ka) {
            return none;
        }
        if (ka == SyntaxKind::DOT || kb == SyntaxKind::DOT)
            && matches!(
                parent_kind(if ka == SyntaxKind::DOT { a } else { b }),
                Some(SyntaxKind::FIELD_EXPR | SyntaxKind::METHOD_CALL_EXPR | SyntaxKind::PATH)
            )
        {
            return none;
        }
        if kb == SyntaxKind::L_PAREN
            && matches!(
                parent_kind(b),
                Some(SyntaxKind::ARG_LIST | SyntaxKind::PARAM_LIST)
            )
            && !a.kind().is_keyword()
        {
            return none;
        }
        if kb == SyntaxKind::L_BRACKET
            && matches!(
                parent_kind(b),
                Some(
                    SyntaxKind::GENERIC_ARG_LIST
                        | SyntaxKind::GENERIC_PARAM_LIST
                        | SyntaxKind::INDEX_EXPR
                )
            )
        {
            return none;
        }
        if ka == SyntaxKind::AT {
            return none;
        }
        if kb == SyntaxKind::QUESTION && parent_kind(b) == Some(SyntaxKind::TRY_EXPR) {
            return none;
        }
        keep
    }
}

fn spaces(n: usize) -> &'static str {
    const SPACES: &str = "                                                                ";
    &SPACES[..n.clamp(1, SPACES.len())]
}

/// Whether `a` followed directly by `b` still lexes as the same tokens and
/// cannot be read as one composite operator (`<` `=` as `<=`).
fn can_join(a: &str, b: &str) -> bool {
    let (Some(last), Some(first)) = (a.chars().last(), b.chars().next()) else {
        return true;
    };
    let separate = |c: char| matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';');
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let punct = |c: char| !word(c) && !separate(c) && !c.is_whitespace();
    if punct(last) && punct(first) {
        return false;
    }
    let joined = format!("{a}{b}");
    let together: Vec<_> = tokenize(&joined)
        .into_iter()
        .map(|t| (t.kind, t.len))
        .collect();
    let apart: Vec<_> = tokenize(a)
        .into_iter()
        .chain(tokenize(b))
        .map(|t| (t.kind, t.len))
        .collect();
    together == apart
}

fn parent_kind(token: &SyntaxToken) -> Option<SyntaxKind> {
    token.parent().map(|p| p.kind())
}

fn first_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| !t.kind().is_trivia())
}

fn is_opener(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::L_PAREN | SyntaxKind::L_CURLY | SyntaxKind::L_BRACKET
    )
}

fn is_closer(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::R_PAREN | SyntaxKind::R_CURLY | SyntaxKind::R_BRACKET
    )
}

fn is_assignment(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::EQ
            | SyntaxKind::PLUSEQ
            | SyntaxKind::MINUSEQ
            | SyntaxKind::STAREQ
            | SyntaxKind::SLASHEQ
            | SyntaxKind::PERCENTEQ
            | SyntaxKind::SHLEQ
            | SyntaxKind::SHREQ
            | SyntaxKind::AMPEQ
            | SyntaxKind::PIPEEQ
            | SyntaxKind::CARETEQ
    )
}

fn is_operator_token(kind: SyntaxKind) -> bool {
    kind.is_symbol() && !is_opener(kind) && !is_closer(kind)
}

/// The operator token of a binary expression (operands are nodes, so a
/// non-trivia token directly under `BIN_EXPR` is its operator).
fn is_binary_operator(token: &SyntaxToken) -> bool {
    parent_kind(token) == Some(SyntaxKind::BIN_EXPR) && !token.kind().is_trivia()
}

/// A `:` that separates a name from its type or value (`x: i32`, `T: Ord`,
/// `field: value`), which is written `name: type`.
fn is_typed_colon(token: &SyntaxToken) -> bool {
    matches!(
        parent_kind(token),
        Some(
            SyntaxKind::PARAM
                | SyntaxKind::SELF_PARAM
                | SyntaxKind::RECORD_FIELD_DEF
                | SyntaxKind::RECORD_FIELD
                | SyntaxKind::LET_STMT
                | SyntaxKind::CONST_DEF
                | SyntaxKind::GENERIC_PARAM
                | SyntaxKind::WHERE_PRED
                | SyntaxKind::INHERITANCE_LIST
                | SyntaxKind::STRUCT_DEF
                | SyntaxKind::TRAIT_DEF
                | SyntaxKind::ENUM_DEF
        )
    )
}

/// Whether the gap between `prev` and `next` lies between a function's
/// attached leading comments and the function itself. The parser attaches
/// comments to a function only while no blank line separates them, so such
/// a gap must not gain one.
fn is_attached_doc_gap(prev: &SyntaxToken, next: &SyntaxToken) -> bool {
    if prev.kind() != SyntaxKind::COMMENT {
        return false;
    }
    let Some(func) = prev
        .parent()
        .filter(|p| p.kind() == SyntaxKind::FUNCTION_DEF)
    else {
        return false;
    };
    let leading = func
        .children_with_tokens()
        .take_while(|e| e.as_token().is_some_and(|t| t.kind().is_trivia()))
        .any(|e| e.as_token() == Some(prev));
    leading && next.parent_ancestors().any(|a| a == func)
}

#[cfg(test)]
mod tests;
