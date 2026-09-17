//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
mod lower;
mod pretty;
#[cfg(test)]
mod tests;

use std::{
    any::type_name,
    fmt,
    fmt::Formatter,
    hash::{Hash, Hasher},
    marker::PhantomData,
    ops::{Index, Range},
    sync::Arc,
};

use codira_hir_input::FileId;
use codira_syntax::ast;
use la_arena::{Arena, Idx};

use crate::{
    path::ImportAlias,
    source_id::{AstIdNode, FileAstId},
    type_ref::{LocalTypeRefId, TypeRefMap},
    visibility::RawVisibility,
    DefDatabase, InFile, Name, Path,
};

#[derive(Copy, Clone, Eq, PartialEq)]
pub struct RawVisibilityId(u32);

impl RawVisibilityId {
    pub const PUB: Self = RawVisibilityId(u32::MAX);
    pub const PRIV: Self = RawVisibilityId(u32::MAX - 1);
    pub const PUB_PACKAGE: Self = RawVisibilityId(u32::MAX - 2);
    pub const PUB_SUPER: Self = RawVisibilityId(u32::MAX - 3);
}

impl fmt::Debug for RawVisibilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_tuple("RawVisibilityId");
        match *self {
            Self::PUB => f.field(&"pub"),
            Self::PRIV => f.field(&"pub(self)"),
            Self::PUB_PACKAGE => f.field(&"pub(package)"),
            Self::PUB_SUPER => f.field(&"pub(super)"),
            _ => f.field(&self.0),
        };
        f.finish()
    }
}

/// An `ItemTree` is a derivative of an AST that only contains the items defined
/// in the AST.
///
/// Examples of items are: functions, structs, use statements.
#[derive(Debug, Eq, PartialEq)]
pub struct ItemTree {
    file_id: FileId,
    top_level: Vec<ModItem>,
    data: ItemTreeData,

    pub diagnostics: Vec<diagnostics::ItemTreeDiagnostic>,
}

impl ItemTree {
    /// Constructs a new `ItemTree` for the specified `file_id`
    pub fn item_tree_query(db: &dyn DefDatabase, file_id: FileId) -> Arc<ItemTree> {
        let syntax = db.parse(file_id);
        let item_tree = lower::Context::new(db, file_id).lower_module_items(&syntax.tree());
        Arc::new(item_tree)
    }

    /// Returns a slice over all items located at the top level of the `FileId`
    /// for which this `ItemTree` was constructed.
    pub fn top_level_items(&self) -> &[ModItem] {
        &self.top_level
    }

    /// Returns the source location of the specified item. Note that the
    /// `file_id` of the item must be the same `file_id` that was used to
    /// create this `ItemTree`.
    pub fn source<S: ItemTreeNode>(
        &self,
        db: &dyn DefDatabase,
        item: LocalItemTreeId<S>,
    ) -> S::Source {
        let root = db.parse(self.file_id);

        let id = self[item].ast_id();
        let map = db.ast_id_map(self.file_id);
        let ptr = map.get(id);
        ptr.to_node(&root.syntax_node())
    }
}

#[derive(Default, Debug, Eq, PartialEq)]
struct ItemVisibilities {
    arena: Arena<RawVisibility>,
}

impl ItemVisibilities {
    fn alloc(vis: RawVisibility) -> RawVisibilityId {
        match &vis {
            RawVisibility::Public => RawVisibilityId::PUB,
            RawVisibility::This => RawVisibilityId::PRIV,
            RawVisibility::Package => RawVisibilityId::PUB_PACKAGE,
            RawVisibility::Super => RawVisibilityId::PUB_SUPER,
        }
    }
}

#[derive(Default, Debug, Eq, PartialEq)]
struct ItemTreeData {
    imports: Arena<Import>,
    functions: Arena<Function>,
    params: Arena<Param>,
    structs: Arena<Struct>,
    fields: Arena<Field>,
    type_aliases: Arena<TypeAlias>,
    consts: Arena<Const>,
    impls: Arena<Impl>,

    visibilities: ItemVisibilities,
}

/// Trait implemented by all item nodes in the item tree.
pub trait ItemTreeNode: Clone {
    type Source: AstIdNode + Into<ast::ModuleItem>;

    /// Returns the AST id for this instance
    fn ast_id(&self) -> FileAstId<Self::Source>;

    /// Looks up an instance of `Self` in an item tree.
    fn lookup(tree: &ItemTree, index: Idx<Self>) -> &Self;

    /// Downcasts a `ModItem` to a `FileItemTreeId` specific to this type
    fn id_from_mod_item(mod_item: ModItem) -> Option<LocalItemTreeId<Self>>;

    /// Upcasts a `FileItemTreeId` to a generic [`ModItem`].
    fn id_to_mod_item(id: LocalItemTreeId<Self>) -> ModItem;
}

/// The typed Id of an item in an `ItemTree`
#[derive(Clone)]
pub struct LocalItemTreeId<N: ItemTreeNode> {
    index: Idx<N>,
    _p: PhantomData<N>,
}

impl<N: ItemTreeNode> Copy for LocalItemTreeId<N> {}

impl<N: ItemTreeNode> PartialEq for LocalItemTreeId<N> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl<N: ItemTreeNode> Eq for LocalItemTreeId<N> {}

impl<N: ItemTreeNode> Hash for LocalItemTreeId<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl<N: ItemTreeNode> fmt::Debug for LocalItemTreeId<N> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.index.fmt(f)
    }
}

/// Represents the Id of an item in the [`ItemTree`] of a file.
pub type ItemTreeId<N> = InFile<LocalItemTreeId<N>>;

macro_rules! mod_items {
    ( $( $typ:ident in $fld:ident -> $ast:ty ),+ $(,)?) => {
        #[derive(Debug,Copy,Clone,Eq,PartialEq,Hash)]
        pub enum ModItem {
            $(
                $typ(LocalItemTreeId<$typ>),
            )+
        }

        $(
            impl From<LocalItemTreeId<$typ>> for ModItem {
                fn from(id: LocalItemTreeId<$typ>) -> ModItem {
                    ModItem::$typ(id)
                }
            }
        )+

        $(
            impl ItemTreeNode for $typ {
                type Source = $ast;

                fn ast_id(&self) -> FileAstId<Self::Source> {
                    self.ast_id
                }

                fn lookup(tree: &ItemTree, index: Idx<Self>) -> &Self {
                    &tree.data.$fld[index]
                }

                fn id_from_mod_item(mod_item: ModItem) -> Option<LocalItemTreeId<Self>> {
                    if let ModItem::$typ(id) = mod_item {
                        Some(id)
                    } else {
                        None
                    }
                }

                fn id_to_mod_item(id: LocalItemTreeId<Self>) -> ModItem {
                    ModItem::$typ(id)
                }
            }

            impl Index<Idx<$typ>> for ItemTree {
                type Output = $typ;

                fn index(&self, index: Idx<$typ>) -> &Self::Output {
                    &self.data.$fld[index]
                }
            }
        )+
    };
}

mod_items! {
    Function in functions -> ast::FunctionDef,
    Struct in structs -> ast::StructDef,
    TypeAlias in type_aliases -> ast::TypeAliasDef,
    Const in consts -> ast::ConstDef,
    Import in imports -> ast::Use,
    Impl in impls -> ast::Extend,
}

macro_rules! impl_index {
    ( $($fld:ident: $t:ty),+ $(,)? ) => {
        $(
            impl Index<Idx<$t>> for ItemTree {
                type Output = $t;

                fn index(&self, index: Idx<$t>) -> &Self::Output {
                    &self.data.$fld[index]
                }
            }
        )+
    };
}

impl_index!(fields: Field, params: Param);

static VIS_PUB: RawVisibility = RawVisibility::Public;
static VIS_PRIV: RawVisibility = RawVisibility::This;
static VIS_PUB_PACKAGE: RawVisibility = RawVisibility::Package;
static VIS_PUB_SUPER: RawVisibility = RawVisibility::Super;

impl Index<RawVisibilityId> for ItemTree {
    type Output = RawVisibility;
    fn index(&self, index: RawVisibilityId) -> &Self::Output {
        match index {
            RawVisibilityId::PRIV => &VIS_PRIV,
            RawVisibilityId::PUB => &VIS_PUB,
            RawVisibilityId::PUB_PACKAGE => &VIS_PUB_PACKAGE,
            RawVisibilityId::PUB_SUPER => &VIS_PUB_SUPER,
            _ => &self.data.visibilities.arena[Idx::from_raw(index.0.into())],
        }
    }
}

impl<N: ItemTreeNode> Index<LocalItemTreeId<N>> for ItemTree {
    type Output = N;
    fn index(&self, id: LocalItemTreeId<N>) -> &N {
        N::lookup(self, id.index)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Import {
    /// The path of the import (e.g. `foo::Bar`). Note that group imports have
    /// been desugared, each item in the import tree is a seperate import.
    pub path: Path,

    /// An optional alias for this import statement (e.g. `use foo as bar`)
    pub alias: Option<ImportAlias>,

    /// The visibility of the import statement as seen from the file that
    /// contains the import statement.
    pub visibility: RawVisibilityId,

    /// Whether or not this is a wildcard import.
    pub is_glob: bool,

    /// AST Id of the `use` item this import was derived from. Note that
    /// multiple `Import`s can map to the same `use` item.
    pub ast_id: FileAstId<ast::Use>,

    /// Index of this `Import` when the containing `Use` is visited with
    /// `Path::expand_use_item`.
    pub index: usize,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Function {
    pub name: Name,
    pub visibility: RawVisibilityId,
    pub types: TypeRefMap,
    pub generic_params: Box<[GenericParamData]>,
    pub params: IdRange<Param>,
    pub ret_type: LocalTypeRefId,
    /// This function's declared `uses Effect, ...` clause (see
    /// `spec/LANGUAGE_SPEC.md` section 6), in source order. Simple
    /// single-segment effect names only for now -- a qualified
    /// `uses some.module.Effect` is not yet resolved to just `Effect`
    /// here, matching this field's only consumer
    /// (`expr::validator::effect_obligation`) not yet needing cross-module
    /// effect resolution.
    pub effects: Box<[Name]>,
    pub ast_id: FileAstId<ast::FunctionDef>,
    pub(crate) flags: FunctionFlags,
}

/// A single entry of a `[T, N: usize]`-style generic parameter list, as
/// captured by the item tree.
///
/// `bound` is resolved against the *same* [`TypeRefMap`] as the rest of the
/// owning item (`Function::types` / `Struct::types`) — it is not its own
/// map. This mirrors how `Param::type_ref` and `Field::type_ref` work.
///
/// The grammar does not distinguish a trait-bounded type parameter
/// (`T: Comparable`) from a value/const parameter (`N: usize`) — both parse
/// as `name (":" type)?`. Telling them apart is a name-resolution concern
/// (does `bound` resolve to a trait or to a concrete non-trait type?), left
/// to the consumer of this data (see `codira_comptime`'s specialization
/// logic), not something the item tree itself decides.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GenericParamData {
    pub name: Name,
    pub bound: Option<LocalTypeRefId>,
}

/// The ABI string that marks an `extern` block as declaring compiler
/// intrinsics rather than foreign symbols.
///
/// Spelled with a `codira-` prefix so it cannot collide with a real platform
/// ABI (`"C"`, `"C++"`, `"system"`), which are the names a linker would
/// recognise.
pub const INTRINSIC_ABI: &str = "codira-intrinsic";

bitflags::bitflags! {
    #[doc = "Flags that are used to store additional information about a function"]
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
    pub(crate) struct FunctionFlags: u8 {
        const HAS_SELF_PARAM = 1 << 0;
        const HAS_BODY = 1 << 1;
        const IS_EXTERN = 1 << 2;
        const IS_INTRINSIC = 1 << 3;
    }
}

impl FunctionFlags {
    /// Whether the function has a self parameter.
    pub fn has_self_param(self) -> bool {
        self.contains(Self::HAS_SELF_PARAM)
    }

    /// Whether the function has a body.
    pub fn has_body(self) -> bool {
        self.contains(Self::HAS_BODY)
    }

    /// Whether the function is extern.
    pub fn is_extern(self) -> bool {
        self.contains(Self::IS_EXTERN)
    }

    /// Whether the function is a compiler intrinsic -- declared in an
    /// `extern "codira-intrinsic"` block and lowered to inline IR at the call
    /// site rather than called as a symbol.
    pub fn is_intrinsic(self) -> bool {
        self.contains(Self::IS_INTRINSIC)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Param {
    pub type_ref: LocalTypeRefId,
    pub ast_id: ParamAstId,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ParamAstId {
    Param(FileAstId<ast::Param>),
    SelfParam(FileAstId<ast::SelfParam>),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Struct {
    pub name: Name,
    pub visibility: RawVisibilityId,
    pub types: TypeRefMap,
    pub generic_params: Box<[GenericParamData]>,
    pub fields: Fields,
    /// Whether this struct carries the Kotlin-style `data` modifier (see
    /// `spec/LANGUAGE_SPEC.md` section 16). Downstream consumers use this to
    /// derive structural equality/hashing/description from `fields` --
    /// nothing else in the item tree changes shape based on it.
    pub is_data: bool,
    pub ast_id: FileAstId<ast::StructDef>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Impl {
    pub types: TypeRefMap,
    pub self_ty: LocalTypeRefId,
    pub items: Box<[AssociatedItem]>,
    pub ast_id: FileAstId<ast::Extend>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeAlias {
    pub name: Name,
    pub visibility: RawVisibilityId,
    pub types: TypeRefMap,
    pub type_ref: Option<LocalTypeRefId>,
    pub ast_id: FileAstId<ast::TypeAliasDef>,
}

/// A module-level binding: `let NAME: T = expr;`.
///
/// Both the type and the initializer are mandatory at the grammar level
/// (see `declarations::const_def`), so neither is optional here -- a
/// module-level binding has no enclosing scope to infer a type from and no
/// later assignment to take a value from.
///
/// The initializer expression is **not** stored in the item tree: bodies
/// are lowered separately and on demand (`Body::body_query`), and holding
/// an expression here would make every item-tree consumer depend on body
/// lowering. What *is* stored is [`Const::references`] -- the set of
/// module-level names the initializer mentions -- because cycle detection
/// must run before any body is lowered. See
/// [`lower::detect_const_cycles`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Const {
    pub name: Name,
    pub visibility: RawVisibilityId,
    pub types: TypeRefMap,
    pub type_ref: LocalTypeRefId,
    /// Single-segment names the initializer refers to, in source order and
    /// deduplicated. This is the edge set of the dependency graph that
    /// cycle detection walks; it deliberately over-approximates (a name
    /// that turns out to be a local or a function is simply not a vertex,
    /// so it contributes no edge).
    pub references: Box<[Name]>,
    pub ast_id: FileAstId<ast::ConstDef>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AssociatedItem {
    Function(LocalItemTreeId<Function>),
}

impl From<LocalItemTreeId<Function>> for AssociatedItem {
    fn from(value: LocalItemTreeId<Function>) -> Self {
        AssociatedItem::Function(value)
    }
}

impl From<AssociatedItem> for ModItem {
    fn from(item: AssociatedItem) -> Self {
        match item {
            AssociatedItem::Function(it) => it.into(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum StructDefKind {
    /// `struct S { ... }` - type namespace only.
    Record,
    /// `struct S(...);`
    Tuple,
    /// `struct S;`
    Unit,
}

/// A set of fields
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fields {
    Record(IdRange<Field>),
    Tuple(IdRange<Field>),
    Unit,
}

/// A single field of an enum variant or struct
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: Name,
    pub type_ref: LocalTypeRefId,
}

/// A range of Ids
pub struct IdRange<T> {
    range: Range<u32>,
    _p: PhantomData<T>,
}

impl<T> IdRange<T> {
    fn new(range: Range<Idx<T>>) -> Self {
        Self {
            range: range.start.into_raw().into()..range.end.into_raw().into(),
            _p: PhantomData,
        }
    }

    /// Returns true if the index range is empty
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
}

impl<T> Iterator for IdRange<T> {
    type Item = Idx<T>;
    fn next(&mut self) -> Option<Self::Item> {
        self.range.next().map(|raw| Idx::from_raw(raw.into()))
    }
}

impl<T> fmt::Debug for IdRange<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple(&format!("IdRange::<{}>", type_name::<T>()))
            .field(&self.range)
            .finish()
    }
}

impl<T> Clone for IdRange<T> {
    fn clone(&self) -> Self {
        Self {
            range: self.range.clone(),
            _p: PhantomData,
        }
    }
}

impl<T> PartialEq for IdRange<T> {
    fn eq(&self, other: &Self) -> bool {
        self.range == other.range
    }
}

impl<T> Eq for IdRange<T> {}

mod diagnostics {
    use codira_syntax::{AstNode, SyntaxNodePtr};

    use super::{ItemTree, ModItem};
    use crate::{
        diagnostics::{CyclicConstDefinition, DuplicateDefinition},
        DefDatabase, DiagnosticSink, HirDatabase, InFile, Name, Path,
    };

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum ItemTreeDiagnostic {
        DuplicateDefinition {
            name: Name,
            first: ModItem,
            second: ModItem,
        },
        /// Module-level bindings that depend on one another in a cycle,
        /// e.g. `let A: i32 = B;` with `let B: i32 = A;`. Detected by
        /// Tarjan's SCC in `lower::detect_const_cycles`; see that
        /// function's doc comment for why this is a graph pass rather
        /// than something the evaluator or the solver discovers.
        CyclicConstDefinition {
            /// Every binding in the cycle, so each one can be pointed at.
            items: Box<[ModItem]>,
            /// Their names, in the same order, for the message.
            names: Box<[Name]>,
        },
    }

    impl ItemTreeDiagnostic {
        pub(crate) fn add_to(
            &self,
            db: &dyn HirDatabase,
            item_tree: &ItemTree,
            sink: &mut DiagnosticSink<'_>,
        ) {
            fn ast_ptr_from_mod(
                db: &dyn DefDatabase,
                item_tree: &ItemTree,
                item: ModItem,
            ) -> InFile<SyntaxNodePtr> {
                match item {
                    ModItem::Function(item) => InFile::new(
                        item_tree.file_id,
                        SyntaxNodePtr::new(item_tree.source(db, item).syntax()),
                    ),
                    ModItem::Struct(item) => InFile::new(
                        item_tree.file_id,
                        SyntaxNodePtr::new(item_tree.source(db, item).syntax()),
                    ),
                    ModItem::TypeAlias(item) => InFile::new(
                        item_tree.file_id,
                        SyntaxNodePtr::new(item_tree.source(db, item).syntax()),
                    ),
                    ModItem::Import(it) => {
                        let import = &item_tree[it];
                        let import_src = item_tree.source(db, it);
                        let mut use_item = None;
                        let mut index = 0;
                        Path::expand_use_item(&import_src, |_, tree, _, _| {
                            if index == import.index {
                                use_item = Some(tree.clone());
                            }
                            index += 1;
                        });
                        InFile::new(
                            item_tree.file_id,
                            SyntaxNodePtr::new(use_item.expect("cannot find use item").syntax()),
                        )
                    }
                    ModItem::Const(item) => InFile::new(
                        item_tree.file_id,
                        SyntaxNodePtr::new(item_tree.source(db, item).syntax()),
                    ),
                    ModItem::Impl(_) => unreachable!("impls cannot be duplicated"),
                }
            }

            match self {
                ItemTreeDiagnostic::DuplicateDefinition {
                    name,
                    first,
                    second,
                } => sink.push(DuplicateDefinition {
                    name: name.to_string(),
                    first_definition: ast_ptr_from_mod(db, item_tree, *first),
                    definition: ast_ptr_from_mod(db, item_tree, *second),
                }),
                ItemTreeDiagnostic::CyclicConstDefinition { items, names } => {
                    let cycle = names
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    // Reported against every participant: each is an
                    // equally valid place to break the cycle, so singling
                    // one out would be arbitrary.
                    for item in items.iter() {
                        sink.push(CyclicConstDefinition {
                            cycle: cycle.clone(),
                            definition: ast_ptr_from_mod(db, item_tree, *item),
                        });
                    }
                }
            };
        }
    }
}
