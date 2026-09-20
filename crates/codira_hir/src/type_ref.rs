//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! HIR for references to types. These paths are not yet resolved. They can be
//! directly created from an `ast::TypeRef`, without further queries.

use std::ops::Index;

use codira_syntax::{ast, AstPtr};
use la_arena::{Arena, ArenaMap, Idx};
use rustc_hash::FxHashMap;

use crate::{name, Path};

/// The ID of a `TypeRef` in a `TypeRefMap`
pub type LocalTypeRefId = Idx<TypeRef>;

/// Compare [`ty::Ty`]
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TypeRef {
    /// A named type, with the generic arguments applied to it.
    ///
    /// The arguments were previously dropped here, which made `Box[i32]`
    /// and `Box[f64]` the same type reference -- and so the same `Ty` --
    /// all the way down. Keeping them is what lets a generic type be
    /// instantiated at all.
    Path(Path, Vec<LocalTypeRefId>),
    Array(LocalTypeRefId),
    /// `Type?` -- an optional, i.e. a type that may additionally be `nil`.
    Optional(LocalTypeRefId),
    /// `Type { x | predicate }` -- a refinement type (see
    /// `spec/self_healing_programming_language.md` section 3.1). The predicate
    /// itself is parsed but not yet checked by inference; like `Optional`,
    /// this lowers transparently to its base type for now.
    Refinement(LocalTypeRefId),
    Never,
    Tuple(Vec<LocalTypeRefId>),
    Error,
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct TypeRefSourceMap {
    type_ref_map: FxHashMap<AstPtr<ast::TypeRef>, LocalTypeRefId>,
    type_ref_map_back: ArenaMap<LocalTypeRefId, AstPtr<ast::TypeRef>>,
}

impl TypeRefSourceMap {
    /// Returns the syntax node of the specified `LocalTypeRefId` or `None` if
    /// it doesnt exist in this instance.
    pub(crate) fn type_ref_syntax(&self, expr: LocalTypeRefId) -> Option<AstPtr<ast::TypeRef>> {
        self.type_ref_map_back.get(expr).cloned()
    }

    /// Returns the `LocalTypeRefId` references at the given location or `None`
    /// if no such Id exists.
    pub(crate) fn syntax_type_ref(&self, ptr: AstPtr<ast::TypeRef>) -> Option<LocalTypeRefId> {
        self.type_ref_map.get(&ptr).cloned()
    }
}

/// Holds all type references from a specific region in the source code
/// (depending on the use of this struct). This struct is often used in
/// conjunction with a `TypeRefSourceMap` which maps `LocalTypeRefId`s to
/// location in the syntax tree and back.
#[derive(Default, Debug, Eq, PartialEq, Clone)]
pub struct TypeRefMap {
    type_refs: Arena<TypeRef>,
}

impl TypeRefMap {
    pub(crate) fn builder() -> TypeRefMapBuilder {
        TypeRefMapBuilder::default()
    }

    /// Returns an iterator over all types in this instance
    pub fn iter(&self) -> impl Iterator<Item = (LocalTypeRefId, &TypeRef)> {
        self.type_refs.iter()
    }
}

impl Index<LocalTypeRefId> for TypeRefMap {
    type Output = TypeRef;

    fn index(&self, pat: LocalTypeRefId) -> &Self::Output {
        &self.type_refs[pat]
    }
}

/// A builder object to lower type references from syntax to a more abstract
/// representation.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct TypeRefMapBuilder {
    map: TypeRefMap,
    source_map: TypeRefSourceMap,
}

impl TypeRefMapBuilder {
    /// Allocates a new `LocalTypeRefId` for the specified `TypeRef`. The passed
    /// `ptr` marks where the `TypeRef` is located in the AST.
    fn alloc_type_ref(&mut self, type_ref: TypeRef, ptr: AstPtr<ast::TypeRef>) -> LocalTypeRefId {
        let id = self.map.type_refs.alloc(type_ref);
        self.source_map.type_ref_map.insert(ptr.clone(), id);
        self.source_map.type_ref_map_back.insert(id, ptr);
        id
    }

    /// Lowers the given optional AST type references and returns the Id of the
    /// resulting `TypeRef`. If the node is None an error is created
    /// indicating a missing `TypeRef` in the AST.
    pub fn alloc_from_node_opt(&mut self, node: Option<&ast::TypeRef>) -> LocalTypeRefId {
        if let Some(node) = node {
            self.alloc_from_node(node)
        } else {
            self.error()
        }
    }

    /// Lowers the given AST type references and returns the Id of the resulting
    /// `TypeRef`.
    pub fn alloc_from_node(&mut self, node: &ast::TypeRef) -> LocalTypeRefId {
        use codira_syntax::ast::TypeRefKind::{
            ArrayType, FunctionType, NeverType, OptionalType, ParenType, PathType, ReferenceType,
            RefinementType, TupleType, VariadicType,
        };

        let ptr = AstPtr::new(node);
        let type_ref = match node.kind() {
            PathType(path) => {
                // Lower the arguments before the path so that their ids are
                // allocated in source order, which keeps the arena readable
                // when a lowering bug has to be traced back to a source
                // position.
                let generic_args: Vec<LocalTypeRefId> = path
                    .generic_arg_list()
                    .map(|list| {
                        list.generic_args()
                            .map(|arg| self.alloc_from_node(&arg))
                            .collect()
                    })
                    .unwrap_or_default();

                path.path()
                    .and_then(Path::from_ast)
                    .map_or(TypeRef::Error, |p| TypeRef::Path(p, generic_args))
            }
            NeverType(_) => TypeRef::Never,
            ArrayType(inner) => TypeRef::Array(self.alloc_from_node_opt(inner.type_ref().as_ref())),
            // `(A, B)`. `()` lowers to `Tuple(vec![])`, which is exactly what
            // `TypeRefMapBuilder::unit` already produces, so the unit type
            // written explicitly and the unit type inferred for a
            // no-return-type function are the same `TypeRef`.
            TupleType(inner) => {
                TypeRef::Tuple(inner.fields().map(|f| self.alloc_from_node(&f)).collect())
            }
            // `(A)` is grouping and nothing else: lower straight through to
            // `A` so no later stage has to know parentheses existed.
            ParenType(inner) => {
                return self.alloc_from_node_opt(inner.type_ref().as_ref());
            }
            // `&T` / `mut T` lower transparently to `T`, the same treatment
            // the parameter ownership keywords get (`spec/LANGUAGE_SPEC.md`
            // section 14): the spelling is recorded in the syntax tree, but
            // there is no borrow model for the type system to enforce, so
            // inventing a distinct `Ty` would mean claiming a check that
            // does not happen.
            ReferenceType(inner) => {
                return self.alloc_from_node_opt(inner.type_ref().as_ref());
            }
            // `func(A, B) -> R` and `...` both parse but have nothing to
            // lower to: there are no function *values* yet (LANGUAGE_SPEC
            // section 12 lists closures as unimplemented) and no
            // argument-pack model. `Error` rather than a silent stand-in, so
            // a signature mentioning one is readable while any *use* reports
            // instead of quietly type-checking against a type that does not
            // exist.
            FunctionType(_) | VariadicType(_) => TypeRef::Error,
            OptionalType(inner) => {
                TypeRef::Optional(self.alloc_from_node_opt(inner.type_ref().as_ref()))
            }
            RefinementType(inner) => {
                TypeRef::Refinement(self.alloc_from_node_opt(inner.base().as_ref()))
            }
        };
        self.alloc_type_ref(type_ref, ptr)
    }

    /// Constructs a new instance for a `Self` type. Returns the Id of the newly
    /// created `TypeRef`.
    pub fn alloc_self(&mut self) -> LocalTypeRefId {
        self.map
            .type_refs
            .alloc(TypeRef::Path(name![Self].into(), Vec::new()))
    }

    /// Constructs a new `TypeRef` for the empty tuple type. Returns the Id of
    /// the newly create `TypeRef`.
    pub fn unit(&mut self) -> LocalTypeRefId {
        self.map.type_refs.alloc(TypeRef::Tuple(vec![]))
    }

    /// Constructs a new error `TypeRef` which marks an error in the AST.
    pub fn error(&mut self) -> LocalTypeRefId {
        self.map.type_refs.alloc(TypeRef::Error)
    }

    /// Finish building type references, returning the `TypeRefMap` which
    /// contains all the `TypeRef`s and a `TypeRefSourceMap` which converts
    /// [`LocalTypeRefIds`] back to source location.
    pub fn finish(self) -> (TypeRefMap, TypeRefSourceMap) {
        (self.map, self.source_map)
    }
}
