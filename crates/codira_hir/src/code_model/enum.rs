//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - The code model of an `enum` declaration (`spec/LANGUAGE_SPEC.md`, "Enums &
//!   pattern matching"): its variants, their payload types, and the value
//!   layout code generation and the runtime share.
//!
//! An enum is a value type. It is laid out as an ordinary value struct: a
//! `u32` tag (the variant's index in declaration order) followed by one
//! typed *slot* per payload field, where fields of the same type in
//! different variants share a slot -- `Circle(radius: f64)` and
//! `Rectangle(width: f64, height: f64)` need two `f64` slots, not three.
//! Every slot has a real type, so a GC reference in a payload has a slot of
//! its own that the collector traces; the slots of the variants a value is
//! not are zero (null for references). See [`EnumLayout`].
use std::{fmt, iter::once, sync::Arc};

use codira_hir_input::FileId;
use codira_syntax::{
    ast::{self, AstNode, GenericParamsOwner, NameOwner, TypeAscriptionOwner},
    SyntaxNodePtr,
};
use la_arena::{Arena, Idx};
use rustc_hash::FxHashSet;

use super::Module;
use crate::{
    diagnostics::ExprError,
    has_module::HasModule,
    ids::{EnumId, GenericDefId, Lookup, TypeParamId},
    name::AsName,
    resolve::HasResolver,
    ty::{
        lower::{lower_types, LowerTyMap},
        substitute_type_params, Substitution, TypeWalk,
    },

    type_ref::{LocalTypeRefId, TypeRefMap, TypeRefSourceMap},
    visibility::RawVisibility,
    DefDatabase, DiagnosticSink, HasSource, HasVisibility, HirDatabase, HirDisplay, Name,
    StructMemoryKind, Ty, TyKind, Visibility,
};

/// An `enum` declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct Enum {
    pub(crate) id: EnumId,
}

impl From<EnumId> for Enum {
    fn from(id: EnumId) -> Self {
        Enum { id }
    }
}

/// One variant of an [`Enum`]: `Circle(radius: f64)` or `Point`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct EnumVariant {
    pub(crate) parent: Enum,
    pub(crate) id: LocalVariantId,
}

/// An identifier for a variant within its enum.
pub type LocalVariantId = Idx<VariantData>;

/// How a variant's payload is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariantKind {
    /// `Point`
    Unit,
    /// `Circle(radius: f64)` or `Circle(f64)`
    Tuple,
    /// `Circle { radius: f64 }` -- parsed, but not supported yet.
    Record,
}

/// One payload field of a variant. The label (`radius` in
/// `Circle(radius: f64)`) is documentation: a payload is constructed and
/// matched positionally.
#[derive(Debug, PartialEq, Eq)]
pub struct VariantFieldData {
    pub label: Option<Name>,
    pub type_ref: LocalTypeRefId,
}

#[derive(Debug, PartialEq, Eq)]
pub struct VariantData {
    pub name: Name,
    pub kind: VariantKind,
    pub fields: Vec<VariantFieldData>,
}

/// The data of an enum declaration, read from its source.
#[derive(Debug, PartialEq, Eq)]
pub struct EnumData {
    pub name: Name,
    pub visibility: RawVisibility,
    pub variants: Arena<VariantData>,
    /// The names of the enum's generic parameters, in declaration order:
    /// the index into this list is the parameter's `TypeParamId::index`.
    /// A generic enum is laid out per instantiation, like a generic struct
    /// (see [`EnumLayout`] and `crate::mono`).
    pub generic_params: Box<[Name]>,
    type_ref_map: TypeRefMap,
    type_ref_source_map: TypeRefSourceMap,
}

impl EnumData {
    pub(crate) fn enum_data_query(db: &dyn DefDatabase, id: EnumId) -> Arc<EnumData> {
        let loc = id.lookup(db);
        let item_tree = db.item_tree(loc.id.file_id);
        let item = &item_tree[loc.id.value];
        let src = item_tree.source(db, loc.id.value);

        let mut types = TypeRefMap::builder();
        let mut variants = Arena::default();
        for variant in source_variants(&src) {
            let Some(name) = variant.name() else {
                continue;
            };
            let (kind, fields) = if let Some(list) = variant.tuple_field_def_list() {
                let fields = list
                    .fields()
                    .map(|field| VariantFieldData {
                        label: field.label().map(|label| Name::new(label.text())),
                        type_ref: types.alloc_from_node_opt(field.type_ref().as_ref()),
                    })
                    .collect();
                (VariantKind::Tuple, fields)
            } else if let Some(list) = variant.record_field_def_list() {
                let fields = list
                    .fields()
                    .map(|field| VariantFieldData {
                        label: field.name().map(|n| n.as_name()),
                        type_ref: types.alloc_from_node_opt(field.ascribed_type().as_ref()),
                    })
                    .collect();
                (VariantKind::Record, fields)
            } else {
                (VariantKind::Unit, Vec::new())
            };
            variants.alloc(VariantData {
                name: name.as_name(),
                kind,
                fields,
            });
        }
        let (type_ref_map, type_ref_source_map) = types.finish();

        Arc::new(EnumData {
            name: item.name.clone(),
            visibility: item_tree[item.visibility].clone(),
            variants,
            generic_params: item.generic_params.clone(),
            type_ref_map,
            type_ref_source_map,
        })
    }

    pub fn type_ref_map(&self) -> &TypeRefMap {
        &self.type_ref_map
    }

    pub fn type_ref_source_map(&self) -> &TypeRefSourceMap {
        &self.type_ref_source_map
    }
}

/// The variants of `src` that have a name, in order -- exactly the ones
/// [`EnumData`] records, so the n-th of these is the n-th variant.
fn source_variants(src: &ast::EnumDef) -> impl Iterator<Item = ast::EnumVariant> {
    src.enum_variant_list()
        .into_iter()
        .flat_map(|list| list.variants())
        .filter(|variant| variant.name().is_some())
}

/// One payload slot of an enum's value layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumSlot {
    /// The field name the runtime sees: the slot type's name and its
    /// ordinal among slots of that type (`f64.0`, `f64.1`). Deterministic,
    /// so hot reload maps a slot to the slot of the same name.
    pub name: String,
    pub ty: Ty,
}

/// How an enum value is laid out: a `u32` tag, then [`EnumLayout::slots`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumLayout {
    pub slots: Vec<EnumSlot>,
    /// For every variant, by index, the slot each payload field lives in.
    pub variant_slots: Vec<Vec<usize>>,
}

impl EnumLayout {
    /// The name of the tag field.
    pub const TAG: &'static str = "tag";

    /// The layout of `e` at the generic arguments `args` (empty for a
    /// non-generic enum, and for the declared layout of a generic one,
    /// whose slots are then typed by its parameters). Each instantiation of
    /// a generic enum has a layout of its own: `Result[i64, i64]` needs one
    /// `i64` slot, `Result[i64, f64]` an `i64` and an `f64` one.
    pub(crate) fn enum_layout_query(
        db: &dyn HirDatabase,
        e: Enum,
        args: Substitution,
    ) -> Arc<EnumLayout> {
        let mut slots: Vec<EnumSlot> = Vec::new();
        let mut variant_slots = Vec::new();
        for variant in e.variants(db) {
            // The k-th field of type `T` in this variant takes the k-th slot
            // of type `T`, adding one when no earlier variant needed as many.
            let mut used = FxHashSet::default();
            let mut assigned = Vec::new();
            for ty in variant.field_types_at(db, &args) {
                let existing = slots
                    .iter()
                    .enumerate()
                    .position(|(idx, slot)| slot.ty == ty && !used.contains(&idx));
                let idx = existing.unwrap_or_else(|| {
                    let base = ty.display(db).to_string();
                    let ordinal = slots.iter().filter(|slot| slot.ty == ty).count();
                    let mut name = format!("{base}.{ordinal}");
                    // Two different types can print alike (two structs of
                    // the same name in different modules); keep the names
                    // distinct.
                    while slots.iter().any(|slot| slot.name == name) {
                        name.push('\'');
                    }
                    slots.push(EnumSlot { name, ty });
                    slots.len() - 1
                });
                used.insert(idx);
                assigned.push(idx);
            }
            variant_slots.push(assigned);
        }
        Arc::new(EnumLayout {
            slots,
            variant_slots,
        })
    }
}

impl Enum {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.module(db).into()
    }

    pub fn file_id(self, db: &dyn HirDatabase) -> FileId {
        self.id.lookup(db).id.file_id
    }

    pub fn data(self, db: &dyn DefDatabase) -> Arc<EnumData> {
        db.enum_data(self.id)
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        self.data(db).name.clone()
    }

    /// The name including the module path (`shapes::Shape`), as a struct's
    /// `full_name` is.
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

    /// The variants, in declaration order (which is tag order).
    pub fn variants(self, db: &dyn HirDatabase) -> Vec<EnumVariant> {
        self.data(db)
            .variants
            .iter()
            .map(|(id, _)| EnumVariant { parent: self, id })
            .collect()
    }

    /// The variant called `name`, if there is one. With duplicate names
    /// (reported by [`Enum::diagnostics`]) the first wins.
    pub fn variant(self, db: &dyn HirDatabase, name: &Name) -> Option<EnumVariant> {
        self.data(db)
            .variants
            .iter()
            .find(|(_, data)| &data.name == name)
            .map(|(id, _)| EnumVariant { parent: self, id })
    }

    /// The enum as a type, with its own parameters standing in for its
    /// generic arguments (`Result[T, E]` inside its declaration), as a
    /// bare struct name is. Empty arguments for a non-generic enum.
    pub fn ty(self, db: &dyn HirDatabase) -> Ty {
        TyKind::Enum(self, self.identity_substitution(db)).intern()
    }

    /// Whether the enum has generic parameters.
    pub fn is_generic(self, db: &dyn HirDatabase) -> bool {
        !self.data(db).generic_params.is_empty()
    }

    /// A substitution mapping each of the enum's parameters to itself.
    pub fn identity_substitution(self, db: &dyn HirDatabase) -> Substitution {
        self.data(db)
            .generic_params
            .iter()
            .enumerate()
            .map(|(index, name)| {
                TyKind::TypeParam(
                    TypeParamId {
                        owner: GenericDefId::EnumId(self.id),
                        index: index as u32,
                    },
                    name.clone(),
                )
                .intern()
            })
            .collect()
    }

    /// Resolves every type the variants mention.
    pub fn lower(self, db: &dyn HirDatabase) -> Arc<LowerTyMap> {
        db.lower_enum(self)
    }

    /// The value layout (see [`EnumLayout`]) of a non-generic enum, or the
    /// declared layout of a generic one (slots typed by its parameters).
    pub fn layout(self, db: &dyn HirDatabase) -> Arc<EnumLayout> {
        db.enum_layout(self, Substitution::empty())
    }

    /// The value layout at the generic arguments `args`.
    pub fn layout_at(self, db: &dyn HirDatabase, args: &Substitution) -> Arc<EnumLayout> {
        db.enum_layout(self, args.clone())
    }

    /// Whether `==`/`!=` can compare two values of this enum at `args`:
    /// every payload is a number or a `bool`, so comparing the tag and
    /// every slot (the unused ones are zero on both sides) is the same as
    /// comparing the variant and its payload. `numeric` says whether a
    /// slot type still being inferred is known to be a number.
    pub fn is_comparable(
        self,
        db: &dyn HirDatabase,
        args: &Substitution,
        numeric: impl Fn(&Ty) -> bool,
    ) -> bool {
        self.layout_at(db, args).slots.iter().all(|slot| {
            matches!(
                slot.ty.interned(),
                TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool
            ) || numeric(&slot.ty)
        })
    }

    /// Reports unresolved payload types, and the enum shapes code
    /// generation cannot lay out: bounds on the enum's own parameters, a
    /// payload naming a generic type without its arguments, variants
    /// written with braces, duplicate variant names, and an enum that
    /// contains itself by value (it would have no finite size).
    pub fn diagnostics(self, db: &dyn HirDatabase, sink: &mut DiagnosticSink<'_>) {
        let data = self.data(db);
        let lower = self.lower(db);
        let file = self.file_id(db);
        lower.add_diagnostics(db, file, data.type_ref_source_map(), sink);

        let src = self.source(db).value;
        let enum_node = SyntaxNodePtr::new(src.syntax());
        let name = data.name.clone();

        // As for a struct, a bound belongs on the `extend` block that needs
        // it: the enum itself holds any `T`.
        for param in src
            .generic_param_list()
            .into_iter()
            .flat_map(|list| list.generic_params())
        {
            if let Some(bound) = param.bounds().next() {
                sink.push(ExprError {
                    file,
                    node: SyntaxNodePtr::new(bound.syntax()),
                    message: format!(
                        "bounds on an enum's own parameters are not supported; bound the \
                         `extend` block that needs them instead (`extend {name}[T: Trait]`)"
                    ),
                });
            }
        }

        // A payload may mention the enum's own parameters; any other one can
        // only come from a generic type written without its arguments
        // (`Box` for `Box[T]`), which nothing ever instantiates.
        let own = GenericDefId::EnumId(self.id);
        let foreign = self.variants(db).into_iter().find_map(|variant| {
            variant.field_types(db).into_iter().find_map(|ty| {
                let mut found = None;
                ty.walk(&mut |inner| {
                    if let TyKind::TypeParam(id, param) = inner.interned() {
                        if found.is_none() && id.owner != own {
                            found = Some(param.clone());
                        }
                    }
                });
                found.map(|param| (ty, param))
            })
        });
        if let Some((ty, param)) = foreign {
            sink.push(ExprError {
                file,
                node: enum_node.clone(),
                message: format!(
                    "the payload type `{}` mentions the generic parameter `{param}`, which \
                     `{name}` does not declare: a generic type needs its arguments written out, \
                     e.g. `Box[i32]` rather than `Box`",
                    ty.display(db),
                ),
            });
        }

        let mut seen = FxHashSet::default();
        for (variant, (_, variant_data)) in source_variants(&src).zip(data.variants.iter()) {
            let node = SyntaxNodePtr::new(variant.syntax());
            if !seen.insert(variant_data.name.clone()) {
                sink.push(ExprError {
                    file,
                    node: node.clone(),
                    message: format!(
                        "`{name}` already has a variant called `{}`",
                        variant_data.name
                    ),
                });
            }
            if variant_data.kind == VariantKind::Record {
                sink.push(ExprError {
                    file,
                    node,
                    message: format!(
                        "a variant with braced fields is not supported yet; write \
                         `{}(label: Type, ..)` instead",
                        variant_data.name
                    ),
                });
            }
        }

        let contains_self = self
            .variants(db)
            .into_iter()
            .flat_map(|variant| variant.field_types(db))
            .any(|ty| contains_by_value(db, &ty, self, &mut FxHashSet::default()));
        if contains_self {
            sink.push(ExprError {
                file,
                node: enum_node,
                message: format!(
                    "`{name}` contains itself by value, so it would have no finite size; \
                     hold the recursive part through a `class` or an array"
                ),
            });
        }
    }
}

/// Whether a value of type `ty` holds an `target` inline: directly, or
/// through a tuple, an optional, a value struct's fields or another enum's
/// payload. A `class` or an array is a reference and ends the search.
fn contains_by_value(
    db: &dyn HirDatabase,
    ty: &Ty,
    target: Enum,
    visited: &mut FxHashSet<Ty>,
) -> bool {
    if !visited.insert(ty.clone()) {
        return false;
    }
    match ty.interned() {
        TyKind::Enum(e, args) => {
            *e == target
                || e.variants(db)
                    .into_iter()
                    .flat_map(|variant| variant.field_types_at(db, args))
                    .any(|field| contains_by_value(db, &field, target, visited))
        }
        TyKind::Tuple(_, elements) => elements
            .iter()
            .any(|element| contains_by_value(db, element, target, visited)),
        TyKind::Optional(inner) => contains_by_value(db, inner, target, visited),
        TyKind::Struct(s, _) if s.data(db).memory_kind == StructMemoryKind::Value => {
            crate::mono::struct_field_types(db, ty)
                .into_iter()
                .any(|field| contains_by_value(db, &field, target, visited))
        }
        _ => false,
    }
}

impl EnumVariant {
    pub fn parent(self) -> Enum {
        self.parent
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        self.parent.data(db).variants[self.id].name.clone()
    }

    /// The variant's index in declaration order: its tag at run time.
    pub fn index(self) -> u32 {
        u32::from(self.id.into_raw())
    }

    pub fn kind(self, db: &dyn HirDatabase) -> VariantKind {
        self.parent.data(db).variants[self.id].kind
    }

    /// The resolved payload types, in order. A type alias is seen through,
    /// as in a function signature.
    pub fn field_types(self, db: &dyn HirDatabase) -> Vec<Ty> {
        let data = self.parent.data(db);
        let lower = self.parent.lower(db);
        data.variants[self.id]
            .fields
            .iter()
            .map(|field| {
                let ty = lower[field.type_ref].clone();
                match ty.interned() {
                    TyKind::TypeAlias(alias) => alias.target_type(db),
                    _ => ty,
                }
            })
            .collect()
    }

    /// The payload types at the parent enum's generic arguments `args`
    /// (the declared types when `args` is empty): `Result.Ok`'s payload is
    /// `i64` in a `Result[i64, str]`.
    pub fn field_types_at(self, db: &dyn HirDatabase, args: &Substitution) -> Vec<Ty> {
        let owner = GenericDefId::EnumId(self.parent.id);
        self.field_types(db)
            .into_iter()
            .map(|ty| substitute_type_params(ty, &owner, args))
            .collect()
    }

    /// The slot each payload field lives in (see [`EnumLayout`]).
    pub fn slots(self, db: &dyn HirDatabase) -> Vec<usize> {
        self.parent.layout(db).variant_slots[self.index() as usize].clone()
    }

    /// The slot each payload field lives in at the generic arguments `args`.
    pub fn slots_at(self, db: &dyn HirDatabase, args: &Substitution) -> Vec<usize> {
        self.parent.layout_at(db, args).variant_slots[self.index() as usize].clone()
    }

    /// `Shape.Circle`, as the variant is written in code.
    pub fn display(self, db: &dyn HirDatabase) -> impl fmt::Display {
        format!("{}.{}", self.parent.name(db), self.name(db))
    }
}

pub(crate) fn lower_enum_query(db: &dyn HirDatabase, e: Enum) -> Arc<LowerTyMap> {
    let data = e.data(db);
    lower_types(db, &e.id.resolver(db), data.type_ref_map())
}

impl HasVisibility for Enum {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        self.data(db).visibility.resolve(db, &self.id.resolver(db))
    }
}

impl crate::HasSource for Enum {
    type Ast = ast::EnumDef;
    fn source(&self, db: &dyn DefDatabase) -> crate::InFile<Self::Ast> {
        self.id.lookup(db).source(db)
    }
}
