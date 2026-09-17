//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{iter::once, sync::Arc};

use codira_hir_input::FileId;
use codira_syntax::{
    ast,
    ast::{AstNode, AttributeOwner, GenericParamsOwner, NameOwner, TypeAscriptionOwner},
};

use super::Module;
use crate::{
    expr::{validator::ExprValidator, BodySourceMap},
    has_module::HasModule,
    ids::{FunctionId, Lookup},
    item_tree::FunctionFlags,
    name::AsName,
    name_resolution::Namespace,
    resolve::HasResolver,
    type_ref::{LocalTypeRefId, TypeRefMap, TypeRefSourceMap},
    visibility::RawVisibility,
    Body, DefDatabase, DiagnosticSink, HasSource, HasVisibility, HirDatabase, InFile,
    InferenceResult, Name, Pat, Ty, Visibility,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct Function {
    pub(crate) id: FunctionId,
}

impl From<FunctionId> for Function {
    fn from(id: FunctionId) -> Self {
        Function { id }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FunctionData {
    name: Name,
    params: Vec<LocalTypeRefId>,
    visibility: RawVisibility,
    ret_type: LocalTypeRefId,
    type_ref_map: TypeRefMap,
    type_ref_source_map: TypeRefSourceMap,
    flags: FunctionFlags,
    /// Names of this function's own `[T, N: usize]`-style generic
    /// parameters, in declaration order. Unlike `item_tree::Function`
    /// (which also records each parameter's bound), only the *name* is
    /// kept here -- that's all `codira_hir::mir_lower` needs to recognize a
    /// `Expr::Path` reference to one of them and lower it to
    /// `codira_mir::OpKind::ParamRef`. Re-derived directly from the AST
    /// (like the rest of this query) rather than threaded from
    /// `item_tree::Function.generic_params`, since `FunctionData` already
    /// builds its own independent `TypeRefMap` from source instead of
    /// reusing the item tree's.
    generic_params: Box<[Name]>,
    /// This function's declared `uses Effect, ...` clause -- see
    /// `item_tree::Function::effects`'s doc comment for the same
    /// single-segment-name restriction. This is the copy
    /// `expr::validator::effect_obligation` actually queries (via
    /// `Function::data`), the same relationship `generic_params` has to
    /// `codira_hir::mir_lower`.
    effects: Box<[Name]>,
    /// Parallel to `params`: whether each ordinary (non-`self`) parameter
    /// carries the `consuming` ownership-convention keyword. Consumed by
    /// `expr::validator::move_check`'s `@strict` checker (see
    /// `spec/LANGUAGE_SPEC.md` section 17).
    consuming_params: Vec<bool>,
    /// Whether this function carries the `@strict` attribute, opting it
    /// into `expr::validator::move_check`'s use-after-consume checking.
    is_strict: bool,
    /// The ABI named by an `@export("C")` attribute, if present.
    ///
    /// `spec/LANGUAGE_SPEC.md` section 10: "`@export(\"C\")` on a Codira
    /// function emits it with C linkage/calling convention and a stable,
    /// unmangled symbol name, so C/C++ code can call back into Codira."
    /// Codegen turns this into `Linkage::DLLExport`; without it the symbol
    /// exists in the object file but is not exported from the assembly, so
    /// no external caller can find it.
    export_abi: Option<String>,
    /// The receiver's type, for a method declared with a `self` parameter.
    ///
    /// Deliberately kept *out* of `params`, which stays the list of ordinary
    /// value parameters. Inference relies on that: `infer_method_call` passes
    /// only the written-out arguments to `infer_call_arguments_and_return`,
    /// which arity-checks them against `FnSig::params()`, so folding the
    /// receiver in there would make every method call look off by one.
    ///
    /// Codegen consumes this separately via [`Function::self_param_ty`] and
    /// materialises the receiver as LLVM parameter 0, ahead of the value
    /// parameters.
    ///
    /// An un-ascribed `self` lowers to `Self`, which the function's own
    /// resolver binds to the enclosing `extend`'s type. An explicit
    /// ascription (`self: BitSet`) is honoured as written.
    self_param: Option<LocalTypeRefId>,
}

impl FunctionData {
    pub(crate) fn fn_data_query(db: &dyn DefDatabase, func: FunctionId) -> Arc<FunctionData> {
        let loc = func.lookup(db);
        let item_tree = db.item_tree(loc.id.file_id);
        let func = &item_tree[loc.id.value];
        let src = item_tree.source(db, loc.id.value);

        let mut type_ref_builder = TypeRefMap::builder();

        let mut params = Vec::new();
        let mut consuming_params = Vec::new();
        let mut self_param = None;
        if let Some(param_list) = src.param_list() {
            // The receiver is recorded on its own rather than pushed into
            // `params` -- see `FunctionData::self_param`'s doc comment for
            // why folding it in would break method-call arity checking.
            if let Some(self_param_src) = param_list.self_param() {
                self_param = Some(match self_param_src.ascribed_type().as_ref() {
                    Some(type_ref) => type_ref_builder.alloc_from_node(type_ref),
                    None => type_ref_builder.alloc_self(),
                });
            }

            for param in param_list.params() {
                let type_ref = type_ref_builder.alloc_from_node_opt(param.ascribed_type().as_ref());
                params.push(type_ref);
                consuming_params.push(param.is_consuming());
            }
        }

        // `@export("C")` -- the ABI string is captured rather than just a
        // flag, so a future `@export("C++")` (which needs mangled-linkage
        // metadata, see LANGUAGE_SPEC section 10) is a change here and not a
        // change to the shape of the data.
        let export_abi = src.attribute_list().and_then(|attrs| {
            attrs.attributes().find_map(|attr| {
                let is_export = attr
                    .path()
                    .and_then(|p| p.segment())
                    .is_some_and(|s| matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == "export"));
                if !is_export {
                    return None;
                }
                let arg = attr.arg_list()?.args().next()?;
                Some(arg.syntax().text().to_string().trim_matches('"').to_string())
            })
        });

        let is_strict = src.attribute_list().is_some_and(|attrs| {
            attrs.attributes().any(|attr| {
                attr.path()
                    .and_then(|p| p.segment())
                    .is_some_and(|s| matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == "strict"))
            })
        });

        let ret_type = if let Some(type_ref) = src.ret_type().and_then(|rt| rt.type_ref()) {
            type_ref_builder.alloc_from_node(&type_ref)
        } else {
            type_ref_builder.unit()
        };

        let (type_ref_map, type_ref_source_map) = type_ref_builder.finish();

        let generic_params = src
            .generic_param_list()
            .map(|list| {
                list.generic_params()
                    .filter_map(|param| Some(param.name()?.as_name()))
                    .collect()
            })
            .unwrap_or_default();

        Arc::new(FunctionData {
            name: func.name.clone(),
            params,
            ret_type,
            type_ref_map,
            type_ref_source_map,
            flags: func.flags,
            visibility: item_tree[func.visibility].clone(),
            generic_params,
            effects: func.effects.clone(),
            consuming_params,
            is_strict,
            export_abi,
            self_param,
        })
    }

    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn params(&self) -> &[LocalTypeRefId] {
        &self.params
    }

    /// The receiver's type reference, or `None` for a free/associated
    /// function. See the field's doc comment for why this is separate from
    /// [`FunctionData::params`].
    pub fn self_param(&self) -> Option<LocalTypeRefId> {
        self.self_param
    }

    pub fn visibility(&self) -> &RawVisibility {
        &self.visibility
    }

    pub fn ret_type(&self) -> &LocalTypeRefId {
        &self.ret_type
    }

    pub fn type_ref_source_map(&self) -> &TypeRefSourceMap {
        &self.type_ref_source_map
    }

    pub fn type_ref_map(&self) -> &TypeRefMap {
        &self.type_ref_map
    }

    /// Returns true if this function is an extern function.
    pub fn is_extern(&self) -> bool {
        self.flags.is_extern()
    }

    /// Returns true if this function is a compiler intrinsic, declared in an
    /// `extern "codira-intrinsic"` block.
    pub fn is_intrinsic(&self) -> bool {
        self.flags.is_intrinsic()
    }

    /// Returns true if the first param is `self`. This is relevant to decide
    /// whether this can be called as a method as opposed to an associated
    /// function.
    ///
    /// An associated function is a function that is associated with a type but
    /// doesn't "act" on an instance. E.g. in Rust terms you can call
    /// `String::from("foo")` but you can't call `String::len()`.
    ///
    /// A method on the other hand is a function that is associated with a type
    /// and does "act" on an instance. E.g. in Rust terms you can call
    /// `foo.len()` but you can't call `foo.new()`.
    pub fn has_self_param(&self) -> bool {
        self.flags.has_self_param()
    }

    /// Names of this function's own generic parameters, in declaration
    /// order (e.g. `[N]` for `func add[N](x: i64) -> i64 { x + N }`).
    pub fn generic_params(&self) -> &[Name] {
        &self.generic_params
    }

    /// This function's declared `uses Effect, ...` clause (see
    /// `spec/LANGUAGE_SPEC.md` section 6), in source order.
    pub fn effects(&self) -> &[Name] {
        &self.effects
    }

    /// A function is *declared-pure* when it has no `uses` clause of its
    /// own. This is a purely syntactic notion (what the signature says),
    /// not a proof the body performs no effects -- `perform`/`handle`
    /// still lower to `Expr::Missing` (see `expr.rs`), so nothing yet
    /// checks a function's *body* against this claim the way
    /// `expr::validator::effect_obligation` checks its *calls*. Still
    /// real and useful on its own: hot-reload-safety analysis and the
    /// healing engine's retry/idempotency decisions (see this session's
    /// KGEN-superset status doc) can already consult a function's
    /// declared effect row today, where previously it was silently
    /// dropped during lowering.
    pub fn is_declared_pure(&self) -> bool {
        self.effects.is_empty()
    }

    /// Parallel to `params()`: whether each ordinary parameter carries the
    /// `consuming` ownership-convention keyword.
    pub fn consuming_params(&self) -> &[bool] {
        &self.consuming_params
    }

    /// Whether this function opts into `expr::validator::move_check`'s
    /// use-after-consume checking via `@strict` (see
    /// `spec/LANGUAGE_SPEC.md` section 17).
    pub fn is_strict(&self) -> bool {
        self.is_strict
    }

    /// The ABI named by this function's `@export("...")` attribute, if any.
    pub fn export_abi(&self) -> Option<&str> {
        self.export_abi.as_deref()
    }
}

impl Function {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.module(db).into()
    }

    /// Returns the full name of the function including all module specifiers
    /// (e.g: `foo::bar`).
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

    pub fn file_id(self, db: &dyn HirDatabase) -> FileId {
        self.id.lookup(db).id.file_id
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        self.data(db).name.clone()
    }

    pub fn data(self, db: &dyn DefDatabase) -> Arc<FunctionData> {
        db.fn_data(self.id)
    }

    pub fn body(self, db: &dyn HirDatabase) -> Arc<Body> {
        db.body(self.id.into())
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Ty {
        db.type_for_def(self.into(), Namespace::Values)
    }

    /// Returns the parameters of the function.
    pub fn params(self, db: &dyn HirDatabase) -> Vec<Param> {
        db.callable_sig(self.into())
            .params()
            .iter()
            .enumerate()
            .map(|(idx, ty)| Param {
                func: self,
                ty: ty.clone(),
                idx,
            })
            .collect()
    }

    /// The ABI named by this function's `@export("...")` attribute, if any.
    ///
    /// `Some("C")` means the function must be exported from the assembly
    /// under its own unmangled name, so C, Rust, Python (`ctypes`) and Node
    /// (`ffi`) callers can all reach it through the one mechanism every
    /// platform already understands.
    pub fn export_abi(self, db: &dyn HirDatabase) -> Option<String> {
        self.data(db).export_abi().map(ToString::to_string)
    }

    pub fn ret_type(self, db: &dyn HirDatabase) -> Ty {
        let resolver = self.id.resolver(db);
        let data = self.data(db);
        Ty::from_hir(db, &resolver, &data.type_ref_map, data.ret_type).0
    }

    /// The resolved type of this function's `self` receiver, or `None` if it
    /// has none.
    ///
    /// Resolution goes through the function's own resolver, which is what
    /// binds a bare `self`'s `Self` type to the enclosing `extend` block's
    /// type. Codegen uses this to prepend the receiver to the LLVM parameter
    /// list; it is deliberately absent from [`Function::params`], which
    /// reports only the value parameters inference arity-checks against.
    pub fn self_param_ty(self, db: &dyn HirDatabase) -> Option<Ty> {
        let data = self.data(db);
        let self_param = data.self_param()?;
        let resolver = self.id.resolver(db);
        Some(Ty::from_hir(db, &resolver, &data.type_ref_map, self_param).0)
    }

    pub fn infer(self, db: &dyn HirDatabase) -> Arc<InferenceResult> {
        db.infer(self.id.into())
    }

    pub fn is_extern(self, db: &dyn HirDatabase) -> bool {
        db.fn_data(self.id).flags.is_extern()
    }

    /// Whether this function is a compiler intrinsic, which codegen emits
    /// inline at the call site instead of calling as a symbol.
    pub fn is_intrinsic(self, db: &dyn HirDatabase) -> bool {
        db.fn_data(self.id).flags.is_intrinsic()
    }

    pub(crate) fn body_source_map(self, db: &dyn HirDatabase) -> Arc<BodySourceMap> {
        db.body_with_source_map(self.id.into()).1
    }

    pub fn diagnostics(self, db: &dyn HirDatabase, sink: &mut DiagnosticSink<'_>) {
        let body = self.body(db);
        body.add_diagnostics(db, self.into(), sink);
        let infer = self.infer(db);
        infer.add_diagnostics(db, self, sink);
        let validator = ExprValidator::new(self, db);
        validator.validate_body(sink);
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Param {
    func: Function,
    /// The index in parameter list, including self parameter.
    idx: usize,
    ty: Ty,
}

impl Param {
    /// Returns the function to which this parameter belongs
    pub fn parent_fn(&self) -> Function {
        self.func
    }

    /// Returns the index of this parameter in the parameter list (including
    /// self)
    pub fn index(&self) -> usize {
        self.idx
    }

    /// Returns the type of this parameter.
    pub fn ty(&self) -> &Ty {
        &self.ty
    }

    /// Returns the source of the parameter.
    pub fn source(&self, db: &dyn HirDatabase) -> Option<InFile<ast::Param>> {
        let InFile { file_id, value } = self.func.source(db);
        let params = value.param_list()?;
        params
            .params()
            .nth(self.idx)
            .map(|value| InFile { file_id, value })
    }

    /// Returns the name of the parameter.
    ///
    /// Only if the parameter is a named binding will this function return a
    /// name. If the function parameter is a wildcard for instance then this
    /// function will return `None`.
    pub fn name(&self, db: &dyn HirDatabase) -> Option<Name> {
        let body = self.func.body(db);
        let pat_id = body.params().get(self.idx)?.0;
        let pat = &body[pat_id];
        if let Pat::Bind { name, .. } = pat {
            Some(name.clone())
        } else {
            None
        }
    }
}

impl HasVisibility for Function {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.function_visibility(self.id)
    }
}
