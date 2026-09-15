//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! This module implements the logic to convert an AST to an `ItemTree`.

use std::{collections::HashMap, convert::TryInto, marker::PhantomData, sync::Arc};

use codira_hir_input::FileId;
use codira_syntax::{
    ast::{
        self, ExternOwner, GenericParamsOwner, ModuleItemOwner, NameOwner, StructKind,
        TypeAscriptionOwner,
    },
    AstNode,
};
use la_arena::{Idx, RawIdx};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::{
    diagnostics, AssociatedItem, Const, Field, Fields, Function, FunctionFlags, GenericParamData,
    IdRange, Impl, ItemTree, ItemTreeData, ItemTreeNode, ItemVisibilities, LocalItemTreeId,
    ModItem, Param, ParamAstId, RawVisibilityId, Struct, TypeAlias,
};
use crate::{
    item_tree::Import,
    name::AsName,
    source_id::AstIdMap,
    type_ref::{TypeRefMap, TypeRefMapBuilder},
    visibility::RawVisibility,
    DefDatabase, Name, Path,
};

struct ModItems(SmallVec<[ModItem; 1]>);

impl<T> From<T> for ModItems
where
    T: Into<ModItem>,
{
    fn from(t: T) -> Self {
        ModItems(SmallVec::from_buf([t.into(); 1]))
    }
}

impl<N: ItemTreeNode> From<Idx<N>> for LocalItemTreeId<N> {
    fn from(index: Idx<N>) -> Self {
        LocalItemTreeId {
            index,
            _p: PhantomData,
        }
    }
}

pub(super) struct Context {
    file: FileId,
    source_ast_id_map: Arc<AstIdMap>,
    data: ItemTreeData,
    diagnostics: Vec<diagnostics::ItemTreeDiagnostic>,
}

impl Context {
    /// Constructs a new `Context` for the specified file
    pub(super) fn new(db: &dyn DefDatabase, file: FileId) -> Self {
        Self {
            file,
            source_ast_id_map: db.ast_id_map(file),
            data: ItemTreeData::default(),
            diagnostics: Vec::new(),
        }
    }

    /// Lowers all the items in the specified `ModuleItemOwner` and returns an
    /// `ItemTree`
    pub(super) fn lower_module_items(mut self, item_owner: &impl ModuleItemOwner) -> ItemTree {
        let top_level = item_owner
            .items()
            .filter_map(|item| self.lower_mod_item(&item))
            .flat_map(|items| items.0)
            .collect::<Vec<_>>();

        // Check duplicates
        let mut set = HashMap::<Name, &ModItem>::new();
        for item in top_level.iter() {
            let name = match item {
                ModItem::Function(item) => Some(&self.data.functions[item.index].name),
                ModItem::Struct(item) => Some(&self.data.structs[item.index].name),
                ModItem::TypeAlias(item) => Some(&self.data.type_aliases[item.index].name),
                ModItem::Const(item) => Some(&self.data.consts[item.index].name),
                ModItem::Import(item) => {
                    let import = &self.data.imports[item.index];
                    if import.is_glob {
                        None
                    } else {
                        import
                            .alias
                            .as_ref()
                            .map_or_else(|| import.path.last_segment(), |alias| alias.as_name())
                    }
                }
                ModItem::Impl(_) => None,
            };
            if let Some(name) = name {
                if let Some(first_item) = set.get(name) {
                    self.diagnostics
                        .push(diagnostics::ItemTreeDiagnostic::DuplicateDefinition {
                            name: name.clone(),
                            first: **first_item,
                            second: *item,
                        });
                } else {
                    set.insert(name.clone(), item);
                }
            }
        }

        // Module-level bindings may refer to one another, so a cycle is
        // possible and must be rejected *here* -- before any body is
        // lowered, evaluated, or fed to the e-graph. See
        // `detect_const_cycles`.
        detect_const_cycles(&top_level, &self.data, &mut self.diagnostics);

        ItemTree {
            file_id: self.file,
            top_level,
            data: self.data,
            diagnostics: self.diagnostics,
        }
    }

    /// Lowers a single module item
    fn lower_mod_item(&mut self, item: &ast::ModuleItem) -> Option<ModItems> {
        match item.kind() {
            ast::ModuleItemKind::FunctionDef(ast) => self.lower_function(&ast).map(Into::into),
            ast::ModuleItemKind::StructDef(ast) => self.lower_struct(&ast).map(Into::into),
            ast::ModuleItemKind::TypeAliasDef(ast) => self.lower_type_alias(&ast).map(Into::into),
            ast::ModuleItemKind::ConstDef(ast) => self.lower_const(&ast).map(Into::into),
            ast::ModuleItemKind::Use(ast) => Some(ModItems(
                self.lower_use(&ast).into_iter().map(Into::into).collect(),
            )),
            ast::ModuleItemKind::Extend(ast) => self.lower_impl(&ast).map(Into::into),
            // `trait`, `enum`, `effect`, `macro`, and `extern "C" { .. }` blocks are
            // fully parsed (see `codira_syntax`) but not yet lowered into name-resolvable
            // HIR items -- that requires their own item-tree/code-model support (akin
            // to what exists for `struct`/`func` today), which is tracked as follow-up
            // work rather than implemented here without a way to verify it end-to-end.
            ast::ModuleItemKind::TraitDef(_)
            | ast::ModuleItemKind::EnumDef(_)
            | ast::ModuleItemKind::EffectDef(_)
            | ast::ModuleItemKind::MacroDef(_)
            | ast::ModuleItemKind::ExternBlock(_)
            | ast::ModuleItemKind::SupervisorDef(_) => None,
        }
    }

    /// Lowers a `use` statement
    fn lower_use(&mut self, use_item: &ast::Use) -> Vec<LocalItemTreeId<Import>> {
        let visibility = lower_visibility(use_item);
        let ast_id = self.source_ast_id_map.ast_id(use_item);

        // Every use item can expand to many `Import`s.
        let mut imports = Vec::new();
        let tree = &mut self.data;
        Path::expand_use_item(use_item, |path, _use_tree, is_glob, alias| {
            imports.push(
                tree.imports
                    .alloc(Import {
                        path,
                        alias,
                        visibility,
                        is_glob,
                        ast_id,
                        index: imports.len(),
                    })
                    .into(),
            );
        });

        imports
    }

    /// Lowers a function
    /// Lowers a `[T, U: Bound]`-style generic parameter list, if present.
    ///
    /// Bounds are allocated into `types` so callers can resolve them the
    /// same way they resolve param/field/return types (see
    /// [`GenericParamData`]).
    // Keeps `&mut self` for symmetry with the sibling `lower_*` methods.
    #[allow(clippy::unused_self)]
    fn lower_generic_params(
        &mut self,
        owner: &impl GenericParamsOwner,
        types: &mut TypeRefMapBuilder,
    ) -> Box<[GenericParamData]> {
        let Some(list) = owner.generic_param_list() else {
            return Box::new([]);
        };
        list.generic_params()
            .filter_map(|param| {
                let name = param.name()?.as_name();
                let bound = param.bound().map(|bound| types.alloc_from_node(&bound));
                Some(GenericParamData { name, bound })
            })
            .collect()
    }

    /// Lowers a function's `uses Effect, ...` clause (see
    /// `spec/LANGUAGE_SPEC.md` section 6) to the simple names of the
    /// effects it declares. Only single-segment paths are recognized
    /// (`uses Logger`, not `uses some.module.Logger`) -- see the doc
    /// comment on `item_tree::Function::effects` for why that's an
    /// intentional restriction for now, not an oversight.
    // Keeps `&self` for symmetry with the sibling `lower_*` methods,
    // which all take the collector; making this one associated would
    // make the call sites inconsistent.
    #[allow(clippy::unused_self)]
    fn lower_uses_clause(&self, func: &ast::FunctionDef) -> Box<[crate::name::Name]> {
        let Some(uses_clause) = func.uses_clause() else {
            return Box::new([]);
        };
        uses_clause
            .effects()
            .filter_map(|path| match path.segment()?.kind()? {
                ast::PathSegmentKind::Name(name_ref) => Some(name_ref.as_name()),
                _ => None,
            })
            .collect()
    }

    fn lower_function(&mut self, func: &ast::FunctionDef) -> Option<LocalItemTreeId<Function>> {
        let name = func.name()?.as_name();
        let visibility = lower_visibility(func);
        let mut types = TypeRefMap::builder();
        let generic_params = self.lower_generic_params(func, &mut types);
        let effects = self.lower_uses_clause(func);

        // Lower all the params
        let start_param_idx = self.next_param_idx();
        let mut has_self_param = false;
        if let Some(param_list) = func.param_list() {
            if let Some(self_param) = param_list.self_param() {
                let ast_id = self.source_ast_id_map.ast_id(&self_param);
                let type_ref = match self_param.ascribed_type().as_ref() {
                    Some(type_ref) => types.alloc_from_node(type_ref),
                    None => types.alloc_self(),
                };
                self.data.params.alloc(Param {
                    type_ref,
                    ast_id: ParamAstId::SelfParam(ast_id),
                });
                has_self_param = true;
            }

            for param in param_list.params() {
                let ast_id = self.source_ast_id_map.ast_id(&param);
                let type_ref = types.alloc_from_node_opt(param.ascribed_type().as_ref());
                self.data.params.alloc(Param {
                    type_ref,
                    ast_id: ParamAstId::Param(ast_id),
                });
            }
        }
        let end_param_idx = self.next_param_idx();
        let params = IdRange::new(start_param_idx..end_param_idx);

        // Lowers the return type
        let ret_type = match func.ret_type().and_then(|rt| rt.type_ref()) {
            None => types.unit(),
            Some(ty) => types.alloc_from_node(&ty),
        };

        let (types, _types_source_map) = types.finish();
        let ast_id = self.source_ast_id_map.ast_id(func);

        let mut flags = FunctionFlags::default();
        if func.is_extern() {
            flags |= FunctionFlags::IS_EXTERN;
        }
        if func.body().is_some() {
            flags |= FunctionFlags::HAS_BODY;
        }
        if has_self_param {
            flags |= FunctionFlags::HAS_SELF_PARAM;
        }

        let res = Function {
            name,
            visibility,
            types,
            generic_params,
            params,
            ret_type,
            effects,
            ast_id,
            flags,
        };

        Some(self.data.functions.alloc(res).into())
    }

    /// Lowers a struct
    fn lower_struct(&mut self, strukt: &ast::StructDef) -> Option<LocalItemTreeId<Struct>> {
        let name = strukt.name()?.as_name();
        let visibility = lower_visibility(strukt);
        let mut types = TypeRefMap::builder();
        let generic_params = self.lower_generic_params(strukt, &mut types);
        let fields = self.lower_fields(&strukt.kind(), &mut types);
        let is_data = strukt.is_data();
        let ast_id = self.source_ast_id_map.ast_id(strukt);

        let (types, _types_source_map) = types.finish();
        let res = Struct {
            name,
            visibility,
            types,
            generic_params,
            fields,
            is_data,
            ast_id,
        };
        Some(self.data.structs.alloc(res).into())
    }

    /// Lowers the fields of a struct or enum
    fn lower_fields(
        &mut self,
        struct_kind: &ast::StructKind,
        types: &mut TypeRefMapBuilder,
    ) -> Fields {
        match struct_kind {
            StructKind::Record(it) => {
                let range = self.lower_record_fields(it, types);
                Fields::Record(range)
            }
            StructKind::Tuple(it) => {
                let range = self.lower_tuple_fields(it, types);
                Fields::Tuple(range)
            }
            StructKind::Unit => Fields::Unit,
        }
    }

    /// Lowers records fields (e.g. `{ a: i32, b: i32 }`)
    fn lower_record_fields(
        &mut self,
        fields: &ast::RecordFieldDefList,
        types: &mut TypeRefMapBuilder,
    ) -> IdRange<Field> {
        let start = self.next_field_idx();
        for field in fields.fields() {
            if let Some(data) = lower_record_field(&field, types) {
                let _idx = self.data.fields.alloc(data);
            }
        }
        let end = self.next_field_idx();
        IdRange::new(start..end)
    }

    /// Lowers tuple fields (e.g. `(i32, u8)`)
    fn lower_tuple_fields(
        &mut self,
        fields: &ast::TupleFieldDefList,
        types: &mut TypeRefMapBuilder,
    ) -> IdRange<Field> {
        let start = self.next_field_idx();
        for (i, field) in fields.fields().enumerate() {
            let data = lower_tuple_field(i, &field, types);
            let _idx = self.data.fields.alloc(data);
        }
        let end = self.next_field_idx();
        IdRange::new(start..end)
    }

    /// Lowers a type alias (e.g. `type Foo = Bar`)
    fn lower_type_alias(
        &mut self,
        type_alias: &ast::TypeAliasDef,
    ) -> Option<LocalItemTreeId<TypeAlias>> {
        let name = type_alias.name()?.as_name();
        let visibility = lower_visibility(type_alias);
        let mut types = TypeRefMap::builder();
        let type_ref = type_alias.type_ref().map(|ty| types.alloc_from_node(&ty));
        let ast_id = self.source_ast_id_map.ast_id(type_alias);
        let (types, _types_source_map) = types.finish();
        let res = TypeAlias {
            name,
            visibility,
            types,
            type_ref,
            ast_id,
        };
        Some(self.data.type_aliases.alloc(res).into())
    }

    /// Lowers `let NAME: T = expr;` at module level.
    ///
    /// The initializer expression is *not* stored: bodies lower on demand
    /// (`Body::body_query`). What is captured instead is the set of
    /// single-segment names the initializer mentions, which is the edge
    /// set [`detect_const_cycles`] walks. Collecting it here -- straight
    /// from the CST, before name resolution -- is what lets cycle
    /// detection run without lowering a single body.
    fn lower_const(&mut self, konst: &ast::ConstDef) -> Option<LocalItemTreeId<Const>> {
        let name = konst.name()?.as_name();
        let visibility = lower_visibility(konst);

        let mut types = TypeRefMap::builder();
        // The grammar requires the ascription, but a malformed source can
        // still reach here; an error type keeps lowering total.
        let type_ref = match konst.type_ref() {
            Some(ty) => types.alloc_from_node(&ty),
            None => types.error(),
        };

        let references = konst
            .initializer()
            .map(|init| collect_path_references(&init))
            .unwrap_or_default();

        let ast_id = self.source_ast_id_map.ast_id(konst);
        let (types, _types_source_map) = types.finish();
        Some(
            self.data
                .consts
                .alloc(Const {
                    name,
                    visibility,
                    types,
                    type_ref,
                    references,
                    ast_id,
                })
                .into(),
        )
    }

    fn lower_impl(&mut self, impl_def: &ast::Extend) -> Option<LocalItemTreeId<Impl>> {
        let ast_id = self.source_ast_id_map.ast_id(impl_def);
        let mut types = TypeRefMap::builder();
        let self_ty = impl_def.type_ref().map(|ty| types.alloc_from_node(&ty))?;

        let items = impl_def
            .extend_item_list()
            .into_iter()
            .flat_map(|it| it.extend_items())
            .filter_map(|item| self.lower_associated_item(&item))
            .collect();

        let (types, _types_source_map) = types.finish();

        let res = Impl {
            types,
            self_ty,
            items,
            ast_id,
        };

        Some(self.data.impls.alloc(res).into())
    }

    fn lower_associated_item(&mut self, item: &ast::ExtendItem) -> Option<AssociatedItem> {
        let item: AssociatedItem = match item.kind() {
            ast::ExtendItemKind::FunctionDef(ast) => self.lower_function(&ast).map(Into::into),
        }?;
        Some(item)
    }

    /// Returns the `Idx` of the next `Field`
    fn next_field_idx(&self) -> Idx<Field> {
        let idx: u32 = self.data.fields.len().try_into().expect("too many fields");
        Idx::from_raw(RawIdx::from(idx))
    }

    /// Returns the `Idx` of the next `Param`
    fn next_param_idx(&self) -> Idx<Param> {
        let idx: u32 = self.data.params.len().try_into().expect("too many params");
        Idx::from_raw(RawIdx::from(idx))
    }
}

/// Lowers a record field (e.g. `a:i32`)
fn lower_record_field(field: &ast::RecordFieldDef, types: &mut TypeRefMapBuilder) -> Option<Field> {
    let name = field.name()?.as_name();
    let type_ref = types.alloc_from_node_opt(field.ascribed_type().as_ref());
    let res = Field { name, type_ref };
    Some(res)
}

/// Lowers a tuple field (e.g. `i32`)
fn lower_tuple_field(
    idx: usize,
    field: &ast::TupleFieldDef,
    types: &mut TypeRefMapBuilder,
) -> Field {
    let name = Name::new_tuple_field(idx);
    let type_ref = types.alloc_from_node_opt(field.type_ref().as_ref());
    Field { name, type_ref }
}

/// Lowers an `ast::VisibilityOwner`
fn lower_visibility(item: &impl ast::VisibilityOwner) -> RawVisibilityId {
    let vis = RawVisibility::from_ast(item.visibility());
    ItemVisibilities::alloc(vis)
}

/// Collects every single-segment path name appearing in `expr`, in source
/// order and deduplicated.
///
/// Walks the CST directly rather than HIR: cycle detection has to run
/// before bodies are lowered, and a cycle among initializers would
/// otherwise be discovered only by whatever tries to *evaluate* them.
///
/// This deliberately **over-approximates**. A name here may turn out to be
/// a function, a local, or nothing at all; such a name simply is not a
/// vertex in the dependency graph and contributes no edge. Missing a real
/// edge would be unsound (a cycle would slip through); including a
/// spurious one is merely conservative, and cannot create a false cycle
/// because a non-binding name has no outgoing edges of its own.
fn collect_path_references(expr: &ast::Expr) -> Box<[Name]> {
    let mut names: Vec<Name> = Vec::new();
    for node in expr.syntax().descendants() {
        let Some(path) = ast::Path::cast(node) else {
            continue;
        };
        // Only unqualified single-segment paths can name a binding in this
        // module; a qualified path resolves elsewhere.
        if path.qualifier().is_some() {
            continue;
        }
        let Some(name) = path
            .segment()
            .and_then(|segment| segment.name_ref())
            .map(|name_ref| name_ref.as_name())
        else {
            continue;
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.into_boxed_slice()
}
// Tarjan state. `usize::MAX` stands in for "unvisited" so the arrays
// can be flat and allocation-free after this point.
const UNVISITED: usize = usize::MAX;

/// Rejects cyclic module-level bindings, e.g. `let A: i32 = B;` together
/// with `let B: i32 = A;`.
///
/// # Why this is a graph pass and not an SMT query
///
/// It is tempting to let the downstream machinery discover the cycle: feed
/// every binding to the e-graph and let evaluation or the solver notice
/// that no value exists. That does not work, and the failure mode is bad.
/// An e-graph represents equalities, not recursion -- `A = B` and `B = A`
/// simply merge into one e-class with no base case, and evaluation
/// recurses until it exhausts fuel. An SMT encoding fares no better: the
/// constraint system is satisfiable by *any* value (nothing pins it), so
/// the solver either returns an arbitrary model or grinds. Neither
/// produces a diagnostic a user can act on.
///
/// Cycles are a property of the dependency *graph*, so they are decided
/// with a graph algorithm. Tarjan's strongly-connected-components runs in
/// O(V + E), reports every cycle in one pass, and names the participating
/// bindings -- which is exactly what the error message needs.
///
/// Implemented iteratively rather than recursively: the recursion depth of
/// the natural formulation is the length of a dependency chain, which is
/// attacker-controlled (a generated source file with ten thousand chained
/// bindings would overflow the stack). An explicit stack makes the pass
/// depth-independent.
fn detect_const_cycles(
    top_level: &[ModItem],
    data: &ItemTreeData,
    diagnostics: &mut Vec<diagnostics::ItemTreeDiagnostic>,
) {
    // Vertices are exactly the module-level bindings. Anything else a
    // reference names is not a vertex and so contributes no edge.
    let consts: Vec<LocalItemTreeId<Const>> = top_level
        .iter()
        .filter_map(|item| match item {
            ModItem::Const(id) => Some(*id),
            _ => None,
        })
        .collect();
    if consts.len() < 2
        && consts.first().is_none_or(|id| {
            let konst = &data.consts[id.index];
            !konst.references.contains(&konst.name)
        })
    {
        // Fewer than two bindings and no self-reference: no cycle is
        // possible, so skip building the index entirely.
        return;
    }

    let index_of: FxHashMap<&Name, usize> = consts
        .iter()
        .enumerate()
        .map(|(i, id)| (&data.consts[id.index].name, i))
        .collect();

    let edges: Vec<Vec<usize>> = consts
        .iter()
        .map(|id| {
            data.consts[id.index]
                .references
                .iter()
                .filter_map(|name| index_of.get(name).copied())
                .collect()
        })
        .collect();

    let n = consts.len();
    let mut index = vec![UNVISITED; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut scc_stack: Vec<usize> = Vec::with_capacity(n);
    let mut next_index = 0usize;

    // Explicit DFS stack: (vertex, position in its edge list).
    let mut work: Vec<(usize, usize)> = Vec::with_capacity(n);

    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        work.push((root, 0));
        index[root] = next_index;
        lowlink[root] = next_index;
        next_index += 1;
        scc_stack.push(root);
        on_stack[root] = true;

        while let Some((v, edge_pos)) = work.pop() {
            if edge_pos < edges[v].len() {
                // Resume `v` after this child returns.
                work.push((v, edge_pos + 1));
                let w = edges[v][edge_pos];
                if index[w] == UNVISITED {
                    index[w] = next_index;
                    lowlink[w] = next_index;
                    next_index += 1;
                    scc_stack.push(w);
                    on_stack[w] = true;
                    work.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w]);
                }
                continue;
            }

            // `v` is exhausted: propagate its lowlink to the parent and,
            // if it roots an SCC, pop that component.
            if let Some(&(parent, _)) = work.last() {
                lowlink[parent] = lowlink[parent].min(lowlink[v]);
            }
            if lowlink[v] == index[v] {
                let start = scc_stack
                    .iter()
                    .rposition(|&x| x == v)
                    .expect("the SCC root is on the stack");
                let component: Vec<usize> = scc_stack.split_off(start);
                for &member in &component {
                    on_stack[member] = false;
                }
                // A component of one vertex is only a cycle if that vertex
                // refers to itself (`let A: i32 = A;`).
                let is_cycle = component.len() > 1 || edges[v].contains(&v);
                if is_cycle {
                    let names: Box<[Name]> = component
                        .iter()
                        .map(|&i| data.consts[consts[i].index].name.clone())
                        .collect();
                    diagnostics.push(diagnostics::ItemTreeDiagnostic::CyclicConstDefinition {
                        items: component
                            .iter()
                            .map(|&i| ModItem::Const(consts[i]))
                            .collect(),
                        names,
                    });
                }
            }
        }
    }
}
