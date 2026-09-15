//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use crate::{
    ast::{self, child_opt, children, AstChildren, AstNode, AstToken},
    syntax_node::SyntaxElementChildren,
    SyntaxKind,
};

pub trait ModuleItemOwner: AstNode {
    fn items(&self) -> AstChildren<ast::ModuleItem> {
        children(self)
    }
}

pub trait FunctionDefOwner: AstNode {
    fn functions(&self) -> AstChildren<ast::FunctionDef> {
        children(self)
    }
}

pub trait NameOwner: AstNode {
    fn name(&self) -> Option<ast::Name> {
        child_opt(self)
    }
}

pub trait TypeAscriptionOwner: AstNode {
    fn ascribed_type(&self) -> Option<ast::TypeRef> {
        child_opt(self)
    }
}

pub trait VisibilityOwner: AstNode {
    fn visibility(&self) -> Option<ast::Visibility> {
        child_opt(self)
    }
}

pub trait LoopBodyOwner: AstNode {
    fn loop_body(&self) -> Option<ast::BlockExpr> {
        child_opt(self)
    }
}

pub trait ArgListOwner: AstNode {
    fn arg_list(&self) -> Option<ast::ArgList> {
        child_opt(self)
    }
}

pub trait DocCommentsOwner: AstNode {
    fn doc_comments(&self) -> CommentIter {
        CommentIter {
            iter: self.syntax().children_with_tokens(),
        }
    }
}

pub struct CommentIter {
    iter: SyntaxElementChildren,
}

impl Iterator for CommentIter {
    type Item = ast::Comment;
    fn next(&mut self) -> Option<ast::Comment> {
        self.iter
            .by_ref()
            .find_map(|el| el.into_token().and_then(ast::Comment::cast))
    }
}

pub trait DefaultTypeParamOwner: AstNode {
    fn default_type(&self) -> Option<ast::PathType> {
        child_opt(self)
    }
}

pub trait ExternOwner: AstNode {
    fn is_extern(&self) -> bool {
        self.syntax()
            .children()
            .any(|p| p.kind() == SyntaxKind::EXTERN)
    }
}

pub trait AttributeOwner: AstNode {
    fn attribute_list(&self) -> Option<ast::AttributeList> {
        child_opt(self)
    }
}

pub trait GenericParamsOwner: AstNode {
    fn generic_param_list(&self) -> Option<ast::GenericParamList> {
        child_opt(self)
    }

    fn where_clause(&self) -> Option<ast::WhereClause> {
        child_opt(self)
    }
}
