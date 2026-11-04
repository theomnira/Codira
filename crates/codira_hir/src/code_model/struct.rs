//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{fmt, iter::once, sync::Arc};

pub use ast::StructMemoryKind;
use codira_hir_input::FileId;
use codira_syntax::{
    ast,
    ast::{NameOwner, TypeAscriptionOwner, VisibilityOwner},
};
use la_arena::{Arena, Idx};

use super::Module;
use crate::{
    has_module::HasModule,
    ids::{Lookup, StructId},
    name::AsName,
    name_resolution::Namespace,
    resolve::HasResolver,
    ty::lower::LowerTyMap,
    type_ref::{LocalTypeRefId, TypeRefMap, TypeRefSourceMap},
    visibility::RawVisibility,
    DefDatabase, DiagnosticSink, HasVisibility, HirDatabase, Name, Ty, Visibility,
};

pub(crate) mod validator;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Struct {
    pub(crate) id: StructId,
}

impl From<StructId> for Struct {
    fn from(id: StructId) -> Self {
        Struct { id }
    }
}

/// A field of a [`Struct`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Field {
    pub(crate) parent: Struct,
    pub(crate) id: LocalFieldId,
}

impl Field {
    /// Returns the type of the field
    pub fn ty(self, db: &dyn HirDatabase) -> Ty {
        let data = self.parent.data(db);
        let type_ref_id = data.fields[self.id].type_ref;
        let lower = self.parent.lower(db);
        lower[type_ref_id].clone()
    }

    /// Returns the name of the field
    pub fn name(self, db: &dyn HirDatabase) -> Name {
        self.parent.data(db).fields[self.id].name.clone()
    }

    /// Returns the name given by `@renamed(from: "old")`, if any.
    pub fn renamed_from(self, db: &dyn HirDatabase) -> Option<String> {
        self.parent.data(db).fields[self.id].renamed_from.clone()
    }

    /// Returns the index of this field in the parent
    pub fn index(self, _db: &dyn HirDatabase) -> u32 {
        self.id.into_raw().into()
    }

    /// Returns the ID of the field with relation to the parent struct
    pub(crate) fn id(self) -> LocalFieldId {
        self.id
    }
}

impl Struct {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.module(db).into()
    }

    pub fn file_id(self, db: &dyn HirDatabase) -> FileId {
        self.id.lookup(db).id.file_id
    }

    pub fn data(self, db: &dyn DefDatabase) -> Arc<StructData> {
        db.struct_data(self.id)
    }

    /// Returns the name of the struct non including any module specifiers (e.g:
    /// `Bar`).
    pub fn name(self, db: &dyn HirDatabase) -> Name {
        self.data(db).name.clone()
    }

    /// Returns the full name of the struct including all module specifiers
    /// (e.g: `foo::Bar`).
    pub fn full_name(self, db: &dyn HirDatabase) -> String {
        itertools::Itertools::intersperse(
            self.module(db)
                .path_to_root(db)
                .into_iter()
                .filter_map(|module| module.name(db))
                .chain(once(self.name(db).to_string())),
            String::from("::"),
        )
        .collect()
    }

    /// Whether the struct declares generic parameters, and so has a layout
    /// per instantiation rather than one of its own.
    pub fn is_generic(self, db: &dyn HirDatabase) -> bool {
        !db.struct_data(self.id).generic_params.is_empty()
    }

    pub fn fields(self, db: &dyn HirDatabase) -> Vec<Field> {
        self.data(db)
            .fields
            .iter()
            .map(|(id, _)| Field { parent: self, id })
            .collect()
    }

    pub fn field(self, db: &dyn HirDatabase, name: &Name) -> Option<Field> {
        self.data(db)
            .fields
            .iter()
            .find(|(_, data)| data.name == *name)
            .map(|(id, _)| Field { parent: self, id })
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Ty {
        db.type_for_def(self.into(), Namespace::Types)
    }

    pub fn lower(self, db: &dyn HirDatabase) -> Arc<LowerTyMap> {
        db.lower_struct(self)
    }

    pub fn diagnostics(self, db: &dyn HirDatabase, sink: &mut DiagnosticSink<'_>) {
        let data = self.data(db);
        let lower = self.lower(db);
        lower.add_diagnostics(db, self.file_id(db), data.type_ref_source_map(), sink);
        let validator = validator::StructValidator::new(self, db, self.file_id(db));
        validator.validate_privacy(sink);
        self.validate_codegen_support(db, sink);
        crate::traits::struct_bound_diagnostics(db, self, sink);
    }

    /// Reports a field whose type mentions a generic parameter that is not
    /// one of this struct's own.
    ///
    /// A generic struct has a layout per instantiation (see `crate::mono`),
    /// read from its fields through the instantiation's arguments, so a
    /// field typed by one of the struct's own parameters is fine. Any other
    /// parameter can only come from a generic type written without its
    /// arguments -- `b: Box` for `struct Box[T]` means `Box[T]` with
    /// `Box`'s own `T` -- and nothing ever says what that `T` is, so the
    /// field would have no layout.
    fn validate_codegen_support(self, db: &dyn HirDatabase, sink: &mut DiagnosticSink<'_>) {
        use codira_syntax::{ast::AstNode, SyntaxNodePtr};

        use crate::{
            code_model::src::HasSource,
            diagnostics::ExprError,
            ids::GenericDefId,
            ty::{TyKind, TypeWalk},
            HirDisplay,
        };

        let own = GenericDefId::from(self.id);
        let Some((ty, name)) = self.fields(db).into_iter().find_map(|field| {
            let ty = field.ty(db);
            let mut foreign = None;
            ty.walk(&mut |inner| {
                if let TyKind::TypeParam(id, name) = inner.interned() {
                    if foreign.is_none() && id.owner != own {
                        foreign = Some(name.clone());
                    }
                }
            });
            foreign.map(|name| (ty, name))
        }) else {
            return;
        };

        let src = self.source(db);
        sink.push(ExprError {
            file: src.file_id,
            node: SyntaxNodePtr::new(src.value.syntax()),
            message: format!(
                "the field type `{}` mentions the generic parameter `{name}`, which `{}` does not \
                 declare: a generic type needs its arguments written out, e.g. `Box[i32]` rather \
                 than `Box`",
                ty.display(db),
                self.name(db),
            ),
        });
    }
}

/// A single field of a record
/// ```codira
/// struct Foo {
///     a: int, // <- this
/// }
/// ```
/// or
/// ```codira
/// struct Foo(
///     int, // <- this
/// )
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldData {
    pub name: Name,
    pub type_ref: LocalTypeRefId,
    pub visibility: RawVisibility,
    /// The name this field had before, from `@renamed(from: "old")`. Hot
    /// reload carries the old field's value over to this one instead of
    /// treating the change as a deletion and an insertion.
    pub renamed_from: Option<String>,
}

/// A struct's fields' data (record, tuple, or unit struct)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructKind {
    Record,
    Tuple,
    Unit,
}

impl fmt::Display for StructKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StructKind::Record => write!(f, "record"),
            StructKind::Tuple => write!(f, "tuple"),
            StructKind::Unit => write!(f, "unit struct"),
        }
    }
}

/// An identifier for a struct's or tuple's field
pub type LocalFieldId = Idx<FieldData>;

#[derive(Debug, PartialEq, Eq)]
pub struct StructData {
    pub name: Name,
    pub visibility: RawVisibility,
    pub fields: Arena<FieldData>,
    pub kind: StructKind,
    pub memory_kind: StructMemoryKind,

    /// The names of this struct's generic parameters, in declaration order.
    ///
    /// The index into this list *is* the `TypeParamId::index` used to refer
    /// to the parameter, so the order is load-bearing rather than cosmetic.
    /// Read from the item tree, the same source `Resolver` uses, so that a
    /// parameter resolves to the same id from both directions.
    pub generic_params: Box<[Name]>,

    type_ref_map: TypeRefMap,
    type_ref_source_map: TypeRefSourceMap,
}

/// Reads `@renamed(from: "old")` (or `@renamed("old")`) from a record field.
fn renamed_from(field: &ast::RecordFieldDef) -> Option<String> {
    use ast::{AstNode, AttributeOwner};
    field.attribute_list()?.attributes().find_map(|attr| {
        let is_renamed = attr.path().and_then(|p| p.segment()).is_some_and(
            |s| matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == "renamed"),
        );
        if !is_renamed {
            return None;
        }
        let arg = attr.arg_list()?.args().next()?;
        let text = arg.syntax().text().to_string();
        let name = text.trim().trim_matches('"');
        (!name.is_empty()).then(|| name.to_owned())
    })
}

impl StructData {
    pub(crate) fn struct_data_query(db: &dyn DefDatabase, id: StructId) -> Arc<StructData> {
        let loc = id.lookup(db);
        let item_tree = db.item_tree(loc.id.file_id);
        let strukt = &item_tree[loc.id.value];
        let src = item_tree.source(db, loc.id.value);

        let memory_kind = src.memory_kind();

        let mut type_ref_builder = TypeRefMap::builder();
        let (fields, kind) = match src.kind() {
            ast::StructKind::Record(r) => {
                let fields = r
                    .fields()
                    .map(|fd| FieldData {
                        name: fd.name().map_or_else(Name::missing, |n| n.as_name()),
                        type_ref: type_ref_builder.alloc_from_node_opt(fd.ascribed_type().as_ref()),
                        visibility: RawVisibility::from_ast(fd.visibility()),
                        renamed_from: renamed_from(&fd),
                    })
                    .collect();
                (fields, StructKind::Record)
            }
            ast::StructKind::Tuple(t) => {
                let fields = t
                    .fields()
                    .enumerate()
                    .map(|(index, fd)| FieldData {
                        name: Name::new_tuple_field(index),
                        type_ref: type_ref_builder.alloc_from_node_opt(fd.type_ref().as_ref()),
                        visibility: RawVisibility::from_ast(fd.visibility()),
                        renamed_from: None,
                    })
                    .collect();
                (fields, StructKind::Tuple)
            }
            ast::StructKind::Unit => (Arena::default(), StructKind::Unit),
        };

        let visibility = item_tree[strukt.visibility].clone();
        let generic_params = strukt
            .generic_params
            .iter()
            .map(|param| param.name.clone())
            .collect();

        let (type_ref_map, type_ref_source_map) = type_ref_builder.finish();
        Arc::new(StructData {
            name: strukt.name.clone(),
            visibility,
            fields,
            kind,
            memory_kind,
            generic_params,
            type_ref_map,
            type_ref_source_map,
        })
    }

    pub fn type_ref_source_map(&self) -> &TypeRefSourceMap {
        &self.type_ref_source_map
    }

    pub fn type_ref_map(&self) -> &TypeRefMap {
        &self.type_ref_map
    }

    /// Returns the index of the field with the specified name.
    pub fn find_field(&self, name: &Name) -> Option<LocalFieldId> {
        self.fields
            .iter()
            .find_map(|(idx, data)| (data.name == *name).then_some(idx))
    }
}

impl HasVisibility for Struct {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        self.data(db).visibility.resolve(db, &self.id.resolver(db))
    }
}
