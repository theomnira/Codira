//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - Extracts the documented items of a Codira source file from its syntax
//!   tree: kind, name, signature, doc comment, attributes, contracts and
//!   methods.

use codira_syntax::{SourceFile, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken, TextSize};

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Effect,
    Session,
    Const,
    Function,
    Method,
    Macro,
    Supervisor,
}

impl Kind {
    /// The word used in anchors and headings.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Trait => "trait",
            Kind::TypeAlias => "type",
            Kind::Effect => "effect",
            Kind::Session => "session",
            Kind::Const => "const",
            Kind::Function => "func",
            Kind::Method => "method",
            Kind::Macro => "macro",
            Kind::Supervisor => "supervisor",
        }
    }

    /// The section heading for items of this kind.
    pub fn plural(self) -> &'static str {
        match self {
            Kind::Struct => "Structs",
            Kind::Enum => "Enums",
            Kind::Trait => "Traits",
            Kind::TypeAlias => "Type aliases",
            Kind::Effect => "Effects",
            Kind::Session => "Session types",
            Kind::Const => "Constants",
            Kind::Function => "Functions",
            Kind::Method => "Methods",
            Kind::Macro => "Macros",
            Kind::Supervisor => "Supervisors",
        }
    }

    /// Whether a name of this kind can appear in a type position, and so is
    /// linked from signatures.
    pub fn is_type(self) -> bool {
        matches!(
            self,
            Kind::Struct
                | Kind::Enum
                | Kind::Trait
                | Kind::TypeAlias
                | Kind::Effect
                | Kind::Session
        )
    }
}

/// One documented item.
#[derive(Debug, Clone)]
pub struct Item {
    pub kind: Kind,
    pub name: String,
    /// For a method, the type its `extend` or `trait` block names.
    pub owner: Option<String>,
    /// The declaration as written, without its doc comment, attributes or
    /// body (for a function), or in full (for a type).
    pub signature: String,
    /// The doc comment, as Markdown (`///` markers removed).
    pub docs: String,
    /// `@noalloc`, `@envelope(..)`, ... as written.
    pub attributes: Vec<String>,
    /// `requires ..` and `ensures ..` clauses as written.
    pub contracts: Vec<String>,
    /// Whether the declaration is `public`.
    pub public: bool,
    /// Whether it is declared in an `extern` block, or has no body and is
    /// marked `extern`.
    pub is_extern: bool,
    /// 1-based line of the declaration keyword.
    pub line: usize,
    /// Byte offset of the item's syntax node, which is how the compiler's
    /// envelope report identifies a function.
    pub offset: u32,
    pub methods: Vec<Item>,
}

impl Item {
    /// The anchor of the item within its module page.
    pub fn anchor(&self) -> String {
        match &self.owner {
            Some(owner) => format!("method.{owner}.{}", self.name),
            None => format!("{}.{}", self.kind.word(), self.name),
        }
    }
}

/// The documented content of one source file.
#[derive(Debug, Clone, Default)]
pub struct FileDocs {
    /// `//!` comments, as Markdown; or, when there are none, the comment
    /// block at the very top of the file.
    pub docs: String,
    pub items: Vec<Item>,
}

/// Extracts the items of `text`. Methods of an `extend` block for a type
/// declared in the same file are attached to that type; the others are
/// listed under their block's type name as items of kind `Method`.
pub fn extract(text: &str) -> FileDocs {
    let root = SourceFile::parse(text).syntax_node();
    let lines = LineIndex::new(text);
    let mut items = Vec::new();
    let mut loose_methods: Vec<Item> = Vec::new();

    for node in root.children() {
        match node.kind() {
            SyntaxKind::EXTEND => {
                let owner = node
                    .children()
                    .find(|c| is_type_node(c.kind()))
                    .map(|t| type_head(&t))
                    .unwrap_or_default();
                for list in node
                    .children()
                    .filter(|c| c.kind() == SyntaxKind::EXTEND_ITEM_LIST)
                {
                    for f in list
                        .descendants()
                        .filter(|d| d.kind() == SyntaxKind::FUNCTION_DEF)
                    {
                        if let Some(mut method) = item_of(&f, &lines, false) {
                            method.kind = Kind::Method;
                            method.owner = Some(owner.clone());
                            loose_methods.push(method);
                        }
                    }
                }
            }
            SyntaxKind::EXTERN_BLOCK => {
                for f in node
                    .descendants()
                    .filter(|d| d.kind() == SyntaxKind::FUNCTION_DEF)
                {
                    if let Some(item) = item_of(&f, &lines, true) {
                        items.push(item);
                    }
                }
            }
            _ => {
                if let Some(item) = item_of(&node, &lines, false) {
                    items.push(item);
                }
            }
        }
    }

    for method in loose_methods {
        let owner = method.owner.clone().unwrap_or_default();
        match items
            .iter_mut()
            .find(|i| i.kind.is_type() && i.name == owner)
        {
            Some(item) => item.methods.push(method),
            None => items.push(method),
        }
    }

    FileDocs {
        docs: module_docs(&root),
        items,
    }
}

fn item_of(node: &SyntaxNode, lines: &LineIndex, in_extern: bool) -> Option<Item> {
    let kind = match node.kind() {
        SyntaxKind::FUNCTION_DEF => Kind::Function,
        SyntaxKind::STRUCT_DEF => Kind::Struct,
        SyntaxKind::ENUM_DEF => Kind::Enum,
        SyntaxKind::TRAIT_DEF => Kind::Trait,
        SyntaxKind::TYPE_ALIAS_DEF => Kind::TypeAlias,
        SyntaxKind::CONST_DEF => Kind::Const,
        SyntaxKind::EFFECT_DEF => Kind::Effect,
        SyntaxKind::SESSION_DEF => Kind::Session,
        SyntaxKind::MACRO_DEF => Kind::Macro,
        SyntaxKind::SUPERVISOR_DEF => Kind::Supervisor,
        _ => return None,
    };
    let name = node
        .children()
        .find(|c| c.kind() == SyntaxKind::NAME)
        .map(|n| n.text().to_string())?;
    let public = node
        .children()
        .any(|c| c.kind() == SyntaxKind::VISIBILITY && c.text().to_string().trim() == "public");
    let attributes: Vec<String> = node
        .children()
        .filter(|c| c.kind() == SyntaxKind::ATTRIBUTE_LIST)
        .flat_map(|list| list.children().collect::<Vec<_>>())
        .map(|a| collapse(&a))
        .collect();
    let contracts: Vec<String> = node
        .children()
        .filter(|c| {
            matches!(
                c.kind(),
                SyntaxKind::REQUIRES_CLAUSE | SyntaxKind::ENSURES_CLAUSE
            )
        })
        .map(|c| collapse(&c))
        .collect();
    let is_extern = in_extern
        || node.children().any(|c| c.kind() == SyntaxKind::EXTERN)
        || node
            .children_with_tokens()
            .any(|c| c.kind() == SyntaxKind::EXTERN_KW);

    let first = first_significant(node);
    let line = first
        .as_ref()
        .map_or(1, |e| lines.line_of(e.text_range().start()));
    let signature = match kind {
        Kind::Function | Kind::Const | Kind::TypeAlias | Kind::Macro => signature_of(node, |k| {
            matches!(
                k,
                SyntaxKind::ATTRIBUTE_LIST
                    | SyntaxKind::BLOCK_EXPR
                    | SyntaxKind::REQUIRES_CLAUSE
                    | SyntaxKind::ENSURES_CLAUSE
                    | SyntaxKind::SEMI
            )
        }),
        Kind::Trait => signature_of(node, |k| {
            matches!(k, SyntaxKind::ATTRIBUTE_LIST | SyntaxKind::EXTEND_ITEM_LIST)
        }),
        _ => declaration_text(node),
    };

    let mut methods = Vec::new();
    if kind == Kind::Trait {
        for f in node
            .descendants()
            .filter(|d| d.kind() == SyntaxKind::FUNCTION_DEF)
        {
            if let Some(mut method) = item_of(&f, lines, false) {
                method.kind = Kind::Method;
                method.owner = Some(name.clone());
                // A trait's methods are as visible as the trait.
                method.public = public;
                methods.push(method);
            }
        }
    }

    Some(Item {
        kind,
        name,
        owner: None,
        signature,
        docs: doc_comment(node),
        attributes,
        contracts,
        public,
        is_extern,
        line,
        offset: u32::from(node.text_range().start()),
        methods,
    })
}

fn is_type_node(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::PATH_TYPE
            | SyntaxKind::ARRAY_TYPE
            | SyntaxKind::TUPLE_TYPE
            | SyntaxKind::REFERENCE_TYPE
            | SyntaxKind::OPTIONAL_TYPE
    )
}

/// The name a type is documented under: `Point[T]` -> `Point`.
fn type_head(node: &SyntaxNode) -> String {
    node.descendants_with_tokens()
        .filter_map(SyntaxElement::into_token)
        .find(|t| t.kind() == SyntaxKind::IDENT)
        .map(|t| t.text().to_string())
        .unwrap_or_else(|| collapse(node))
}

fn first_significant(node: &SyntaxNode) -> Option<SyntaxElement> {
    node.children_with_tokens().find(|e| match e {
        SyntaxElement::Token(t) => !t.kind().is_trivia(),
        SyntaxElement::Node(n) => n.kind() != SyntaxKind::ATTRIBUTE_LIST,
    })
}

/// The tokens of `node` on one line: comments dropped, whitespace runs as
/// one space, none just inside brackets or before `,`.
fn collapse(node: &SyntaxNode) -> String {
    let tokens: Vec<SyntaxToken> = node
        .descendants_with_tokens()
        .filter_map(SyntaxElement::into_token)
        .filter(|t| t.kind() != SyntaxKind::COMMENT)
        .collect();
    join_tokens(&tokens)
}

fn join_tokens(tokens: &[SyntaxToken]) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for token in tokens {
        if token.kind() == SyntaxKind::WHITESPACE {
            pending_space = !out.is_empty();
            continue;
        }
        let text = token.text();
        if pending_space && !out.ends_with(['(', '[']) && !matches!(text, ")" | "]" | "," | ";") {
            out.push(' ');
        }
        pending_space = false;
        out.push_str(text);
    }
    out
}

/// The declaration of `node` on one line, without its leading comments and
/// the children `skip` names.
fn signature_of(node: &SyntaxNode, skip: impl Fn(SyntaxKind) -> bool) -> String {
    let mut tokens = Vec::new();
    let mut started = false;
    for element in node.children_with_tokens() {
        match &element {
            SyntaxElement::Token(t) => {
                if t.kind().is_trivia() && !started {
                    continue;
                }
                if skip(t.kind()) {
                    continue;
                }
                started = true;
                if t.kind() != SyntaxKind::COMMENT {
                    tokens.push(t.clone());
                }
            }
            SyntaxElement::Node(n) => {
                if skip(n.kind()) {
                    continue;
                }
                started = true;
                tokens.extend(
                    n.descendants_with_tokens()
                        .filter_map(SyntaxElement::into_token)
                        .filter(|t| t.kind() != SyntaxKind::COMMENT),
                );
            }
        }
    }
    join_tokens(&tokens).trim().to_string()
}

/// The full text of a type declaration from its first keyword, without
/// attributes or leading comments, dedented.
fn declaration_text(node: &SyntaxNode) -> String {
    let Some(first) = first_significant(node) else {
        return String::new();
    };
    let start = first.text_range().start() - node.text_range().start();
    let text = node.text().to_string();
    let text = &text[u32::from(start) as usize..];
    let lines: Vec<&str> = text.lines().collect();
    let indent = lines
        .iter()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !l.trim_start().starts_with('}'))
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    // Keep the body indented by four spaces relative to the first line.
    let shift = indent.saturating_sub(4);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
            let cut = shift.min(line.len() - line.trim_start().len());
            out.push_str(line[cut..].trim_end());
        } else {
            out.push_str(line.trim_end());
        }
    }
    out
}

/// The `///` comments directly before `node`: the ones the parser attached
/// inside it, and the ones before it with no blank line in between.
pub fn doc_comment(node: &SyntaxNode) -> String {
    let mut before = Vec::new();
    let mut element = node.prev_sibling_or_token();
    while let Some(e) = element {
        match &e {
            SyntaxElement::Token(t) if t.kind() == SyntaxKind::WHITESPACE => {
                if t.text().matches('\n').count() > 1 {
                    break;
                }
            }
            SyntaxElement::Token(t) if t.kind() == SyntaxKind::COMMENT => {
                before.push(t.text().to_string())
            }
            _ => break,
        }
        element = e.prev_sibling_or_token();
    }
    before.reverse();
    let inside = node
        .children_with_tokens()
        .take_while(|e| e.as_token().is_some_and(|t| t.kind().is_trivia()))
        .filter_map(SyntaxElement::into_token)
        .filter(|t| t.kind() == SyntaxKind::COMMENT)
        .map(|t| t.text().to_string());
    let comments: Vec<String> = before.into_iter().chain(inside).collect();
    let docs = strip_markers(comments.iter().map(String::as_str), "///");
    if !docs.is_empty() || comments.iter().any(|c| c.starts_with("///")) {
        return docs;
    }
    // No `///`: a plain `//` block directly above the item documents it
    // (the convention of much of the standard library). Rules made only of
    // punctuation or box-drawing characters are decoration.
    let plain: Vec<&str> = comments
        .iter()
        .map(String::as_str)
        .filter(|c| c.starts_with("//") && !c.starts_with("//!"))
        .filter(|c| c.chars().any(char::is_alphanumeric))
        .collect();
    strip_markers(plain.into_iter(), "//")
}

fn strip_markers<'a>(comments: impl Iterator<Item = &'a str>, marker: &str) -> String {
    let mut out = String::new();
    for comment in comments {
        let Some(rest) = comment.strip_prefix(marker) else {
            continue;
        };
        // `////...` is a separator, not documentation.
        if rest.starts_with('/') {
            continue;
        }
        out.push_str(rest.strip_prefix(' ').unwrap_or(rest).trim_end());
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// `//!` comments of the file, or else the comment block it starts with.
fn module_docs(root: &SyntaxNode) -> String {
    let comments: Vec<String> = root
        .descendants_with_tokens()
        .filter_map(SyntaxElement::into_token)
        .filter(|t| t.kind() == SyntaxKind::COMMENT && t.text().starts_with("//!"))
        .map(|t| t.text().to_string())
        .collect();
    if !comments.is_empty() {
        return strip_markers(comments.iter().map(String::as_str), "//!");
    }
    // The leading block: comments before the first item, up to the first
    // blank line, if the block is not an item's doc comment.
    let mut block = Vec::new();
    for element in root.children_with_tokens() {
        match &element {
            SyntaxElement::Token(t) if t.kind() == SyntaxKind::COMMENT => {
                if t.text().starts_with("///") || !t.text().starts_with("//") {
                    return String::new();
                }
                block.push(t.text().to_string());
            }
            SyntaxElement::Token(t) if t.kind() == SyntaxKind::WHITESPACE => {
                if t.text().matches('\n').count() > 1 {
                    break;
                }
            }
            _ => break,
        }
    }
    let lines: Vec<String> = block
        .iter()
        .map(|c| c.trim_start_matches('/').trim().to_string())
        // Rules made of box-drawing or punctuation characters are
        // decoration, not text.
        .filter(|l| l.chars().any(char::is_alphanumeric))
        // A line that only names the file (`std/collections/array.code`).
        .filter(|l| !(l.ends_with(".code") && !l.contains(' ')))
        .collect();
    lines.join("\n")
}

/// Byte offset to 1-based line.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> LineIndex {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { starts }
    }

    pub fn line_of(&self, offset: TextSize) -> usize {
        let offset = u32::from(offset) as usize;
        match self.starts.binary_search(&offset) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    }
}
