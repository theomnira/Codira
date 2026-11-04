//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::borrow::Cow;

use codira_abi::StructMemoryKind;
use rowan::{GreenNodeData, GreenTokenData, NodeOrToken};
use text_size::TextRange;

use crate::{
    ast::{self, child_opt, AstNode, NameOwner},
    SyntaxKind, SyntaxNode, TokenText,
};

impl ast::Name {
    pub fn text(&self) -> TokenText<'_> {
        text_of_first_token(self.syntax())
    }
}

impl ast::NameRef {
    pub fn text(&self) -> TokenText<'_> {
        text_of_first_token(self.syntax())
    }

    pub fn as_tuple_field(&self) -> Option<usize> {
        self.syntax().children_with_tokens().find_map(|c| {
            if c.kind() == SyntaxKind::INT_NUMBER {
                c.as_token().and_then(|tok| tok.text().parse().ok())
            } else {
                None
            }
        })
    }
}

impl ast::FunctionDef {
    /// Returns the signature range.
    ///
    /// ```rust, ignore
    /// func foo_bar() {
    /// ^^^^^^^^^^^^^^___ this part
    ///     // ...
    /// }
    /// ```
    pub fn signature_range(&self) -> TextRange {
        let fn_kw = self
            .syntax()
            .children_with_tokens()
            .find(|p| p.kind() == T![func] || p.kind() == T![def])
            .map(|kw| kw.text_range());
        let name = self.name().map(|n| n.syntax.text_range());
        let param_list = self.param_list().map(|p| p.syntax.text_range());
        let ret_type = self.ret_type().map(|r| r.syntax.text_range());

        let start = fn_kw.map_or_else(|| self.syntax.text_range().start(), rowan::TextRange::start);

        let end = ret_type
            .map(rowan::TextRange::end)
            .or_else(|| param_list.map(rowan::TextRange::end))
            .or_else(|| name.map(rowan::TextRange::end))
            .or_else(|| fn_kw.map(rowan::TextRange::end))
            .unwrap_or_else(|| self.syntax().text_range().end());

        TextRange::new(start, end)
    }
}

fn text_of_first_token(node: &SyntaxNode) -> TokenText<'_> {
    fn first_token(green_ref: &GreenNodeData) -> &GreenTokenData {
        green_ref
            .children()
            .next()
            .and_then(NodeOrToken::into_token)
            .unwrap()
    }

    match node.green() {
        Cow::Borrowed(green_ref) => TokenText::borrowed(first_token(green_ref).text()),
        Cow::Owned(green) => TokenText::owned(first_token(&green).to_owned()),
    }
}

impl ast::Path {
    pub fn parent_path(&self) -> Option<ast::Path> {
        self.syntax().parent().and_then(ast::Path::cast)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegmentKind {
    Name(ast::NameRef),
    SelfKw,
    SuperKw,
    RootKw,
}

impl ast::PathSegment {
    pub fn parent_path(&self) -> ast::Path {
        self.syntax()
            .parent()
            .and_then(ast::Path::cast)
            .expect("segments are always nested in paths")
    }

    pub fn kind(&self) -> Option<PathSegmentKind> {
        let res = if let Some(name_ref) = self.name_ref() {
            PathSegmentKind::Name(name_ref)
        } else {
            match self.syntax().first_child_or_token()?.kind() {
                T![self] => PathSegmentKind::SelfKw,
                T![super] => PathSegmentKind::SuperKw,
                T![root] => PathSegmentKind::RootKw,
                _ => return None,
            }
        };
        Some(res)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructKind {
    Record(ast::RecordFieldDefList),
    Tuple(ast::TupleFieldDefList),
    Unit,
}

impl StructKind {
    fn from_node<N: AstNode>(node: &N) -> StructKind {
        if let Some(r) = child_opt::<_, ast::RecordFieldDefList>(node) {
            StructKind::Record(r)
        } else if let Some(t) = child_opt::<_, ast::TupleFieldDefList>(node) {
            StructKind::Tuple(t)
        } else {
            StructKind::Unit
        }
    }
}

impl ast::StructDef {
    pub fn kind(&self) -> StructKind {
        StructKind::from_node(self)
    }

    /// The memory kind is no longer a parenthetical annotation; it's encoded
    /// directly in which keyword introduced the declaration: `class` is a
    /// heap-allocated, garbage-collected, shared-mutability reference type,
    /// while `struct` is a copied value type.
    pub fn memory_kind(&self) -> StructMemoryKind {
        if self.is_class() {
            StructMemoryKind::Gc
        } else {
            StructMemoryKind::Value
        }
    }

    pub fn is_class(&self) -> bool {
        self.syntax()
            .children_with_tokens()
            .any(|it| it.kind() == T![class])
    }

    /// Whether this declaration carries the `data` modifier (Kotlin-style
    /// `data class`/`data struct`; see `spec/LANGUAGE_SPEC.md` section 15).
    /// `data` derives structural equality, hashing, and a description
    /// (Debug/toString-analog) from the struct's fields.
    pub fn is_data(&self) -> bool {
        self.syntax()
            .children_with_tokens()
            .any(|it| it.kind() == T![data])
    }

    /// Returns the signature range.
    ///
    /// ```rust, ignore
    /// class Foo {
    /// ^^^^^^^^^^___ this part
    ///     // ...
    /// }
    /// ```
    pub fn signature_range(&self) -> TextRange {
        let struct_kw = self
            .syntax()
            .children_with_tokens()
            .find(|p| p.kind() == T![struct] || p.kind() == T![class])
            .map(|kw| kw.text_range());
        let name = self.name().map(|n| n.syntax.text_range());

        let start =
            struct_kw.map_or_else(|| self.syntax.text_range().start(), rowan::TextRange::start);

        let end = name
            .map(rowan::TextRange::end)
            .or_else(|| struct_kw.map(rowan::TextRange::end))
            .unwrap_or_else(|| self.syntax().text_range().end());

        TextRange::new(start, end)
    }
}

impl ast::TupleFieldDef {
    /// The optional `name:` label in front of the field's type, as in an
    /// enum variant's `Circle(radius: f64)`. Documentation only: tuple
    /// fields are positional.
    pub fn label(&self) -> Option<crate::SyntaxToken> {
        let mut tokens = self
            .syntax()
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .filter(|token| !token.kind().is_trivia());
        let label = tokens.find(|token| token.kind() == SyntaxKind::IDENT)?;
        let colon = tokens.next()?;
        (colon.kind() == T![:]).then_some(label)
    }
}

impl ast::Param {
    /// Whether this parameter carries the `consuming` ownership-convention
    /// keyword (see `spec/LANGUAGE_SPEC.md` section 14 and
    /// `parsing/grammar/params.rs`'s `eat_contextual_kw`) -- the callee
    /// takes ownership; the caller's binding is no longer valid after the
    /// call. Consumed by `codira_hir`'s `@strict` move-checker (see
    /// `spec/LANGUAGE_SPEC.md` section 17).
    pub fn is_consuming(&self) -> bool {
        self.syntax()
            .children_with_tokens()
            .any(|it| it.kind() == T![consuming])
    }
}

impl ast::FunctionDef {
    /// Whether this is an `oblivious func`: constant-time, with every
    /// parameter and the result secret.
    pub fn is_oblivious(&self) -> bool {
        self.syntax()
            .children_with_tokens()
            .any(|it| it.kind() == T![oblivious])
    }
}

impl ast::LetStmt {
    /// Whether this is a `ghost let`: a binding that exists only for
    /// verification. `codira_hir` keeps ghost bindings out of the body that
    /// code generation sees, and rejects any use of one from executable code.
    pub fn is_ghost(&self) -> bool {
        self.syntax()
            .children_with_tokens()
            .any(|it| it.kind() == T![ghost])
    }

    /// Whether the binding may be assigned to after its initialisation:
    /// `var x` (or `let mut x`, the Rust-style spelling of it). A plain
    /// `let x` binds once.
    pub fn is_mutable(&self) -> bool {
        self.syntax()
            .children_with_tokens()
            .any(|it| it.kind() == T![var] || it.kind() == T![mut])
    }
}

pub enum VisibilityKind {
    Public,
    Internal,
}

impl ast::Visibility {
    pub fn kind(&self) -> VisibilityKind {
        if self.is_internal() {
            VisibilityKind::Internal
        } else {
            VisibilityKind::Public
        }
    }

    fn is_internal(&self) -> bool {
        self.syntax()
            .children_with_tokens()
            .any(|it| it.kind() == T![internal])
    }
}

impl ast::UseTree {
    pub fn has_star_token(&self) -> bool {
        self.syntax()
            .children_with_tokens()
            .any(|it| it.kind() == T![*])
    }
}

impl ast::TypeAliasDef {
    /// Returns the signature range.
    ///
    /// ```rust, ignore
    /// type FooBar = i32
    /// ^^^^^^^^^^^___ this part
    /// ```
    pub fn signature_range(&self) -> TextRange {
        let type_kw = self
            .syntax()
            .children_with_tokens()
            .find(|p| p.kind() == T![type])
            .map(|kw| kw.text_range());
        let name = self.name().map(|n| n.syntax.text_range());

        let start =
            type_kw.map_or_else(|| self.syntax.text_range().start(), rowan::TextRange::start);

        let end = name
            .map(rowan::TextRange::end)
            .or_else(|| type_kw.map(rowan::TextRange::end))
            .unwrap_or_else(|| self.syntax().text_range().end());

        TextRange::new(start, end)
    }
}

impl ast::GenericParam {
    /// Every bound of the parameter, in order: `A` and `B` of `T: A + B`.
    ///
    /// `bound()` only returns the first; a `+`-separated list is a sequence
    /// of sibling `TYPE_REF`s.
    pub fn bounds(&self) -> impl Iterator<Item = ast::TypeRef> {
        self.syntax().children().filter_map(ast::TypeRef::cast)
    }
}

impl ast::WherePred {
    /// The constrained type: `T` of `where T: A + B`.
    pub fn subject(&self) -> Option<ast::TypeRef> {
        self.syntax().children().find_map(ast::TypeRef::cast)
    }

    /// The bounds of the predicate: `A` and `B` of `where T: A + B`.
    ///
    /// The generated `bound()` returns the first `TYPE_REF` child, which is
    /// the subject, not a bound.
    pub fn bounds(&self) -> impl Iterator<Item = ast::TypeRef> {
        self.syntax()
            .children()
            .filter_map(ast::TypeRef::cast)
            .skip(1)
    }
}

impl ast::GenericArgList {
    /// The arguments, each with the bounds written after it.
    ///
    /// In `extend Box[T: Show + Eq, U]` the argument list holds the type
    /// references `T`, `Show`, `Eq` and `U` as siblings, separated by `:`,
    /// `+` and `,` tokens. The generated `generic_args()` returns all four;
    /// this tells the arguments (`T`, `U`) from their bounds (`Show`, `Eq`
    /// for `T`, none for `U`).
    pub fn args_with_bounds(&self) -> Vec<(ast::TypeRef, Vec<ast::TypeRef>)> {
        let mut args: Vec<(ast::TypeRef, Vec<ast::TypeRef>)> = Vec::new();
        let mut in_bounds = false;
        for child in self.syntax().children_with_tokens() {
            match child {
                NodeOrToken::Token(token) => match token.kind() {
                    T![:] => in_bounds = true,
                    T![,] => in_bounds = false,
                    _ => {}
                },
                NodeOrToken::Node(node) => {
                    let Some(type_ref) = ast::TypeRef::cast(node) else {
                        continue;
                    };
                    match args.last_mut() {
                        Some((_, bounds)) if in_bounds => bounds.push(type_ref),
                        _ => args.push((type_ref, Vec::new())),
                    }
                }
            }
        }
        args
    }

    /// The arguments without their bounds: `T` and `U` of
    /// `Box[T: Show, U]`. See [`ast::GenericArgList::args_with_bounds`].
    pub fn args(&self) -> impl Iterator<Item = ast::TypeRef> {
        self.args_with_bounds().into_iter().map(|(arg, _)| arg)
    }
}
