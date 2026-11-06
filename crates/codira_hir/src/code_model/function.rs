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
    /// Whether this function carries the `@test` attribute: a test the
    /// `codira test` runner calls (see `expr::validator::test_fn` for what
    /// shape a test must have).
    is_test: bool,
    /// Whether this function carries the `@error_conversion` attribute: an
    /// associated function of a type `F` that `?` calls to pass an error of
    /// another type on as an `F` (see `ty::infer::propagate`).
    is_error_conversion: bool,
    /// Whether this function carries the `@inline` attribute: every call is
    /// to be inlined (LLVM's `alwaysinline`), for a small function on a hot
    /// path whose callers the optimizer would otherwise leave calling it.
    is_inline: bool,
    /// The ABI named by an `@export("C")` attribute, if present.
    ///
    /// `spec/LANGUAGE_SPEC.md` section 10: "`@export(\"C\")` on a Codira
    /// function emits it with C linkage/calling convention and a stable,
    /// unmangled symbol name, so C/C++ code can call back into Codira."
    /// Codegen turns this into `Linkage::DLLExport`; without it the symbol
    /// exists in the object file but is not exported from the assembly, so
    /// no external caller can find it.
    export_abi: Option<String>,
    /// The well-formed `@asm(...)` variants on this function, in source order.
    /// Malformed ones are left out here and reported by
    /// `ExprValidator::validate_inline_asm`, which reads the attributes again.
    asm_variants: Box<[crate::inline_asm::AsmVariant]>,
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

        let asm_variants = crate::inline_asm::asm_variants(&src).0.into_boxed_slice();

        let is_strict = src.attribute_list().is_some_and(|attrs| {
            attrs.attributes().any(|attr| {
                attr.path()
                    .and_then(|p| p.segment())
                    .is_some_and(|s| matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == "strict"))
            })
        });

        let is_test = src.attribute_list().is_some_and(|attrs| {
            attrs.attributes().any(|attr| {
                attr.path()
                    .and_then(|p| p.segment())
                    .is_some_and(|s| matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == "test"))
            })
        });

        let is_error_conversion = src.attribute_list().is_some_and(|attrs| {
            attrs.attributes().any(|attr| {
                attr.path().and_then(|p| p.segment()).is_some_and(|s| {
                    matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == "error_conversion")
                })
            })
        });

        let is_inline = src.attribute_list().is_some_and(|attrs| {
            attrs.attributes().any(|attr| {
                attr.path()
                    .and_then(|p| p.segment())
                    .is_some_and(|s| matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == "inline"))
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
            is_test,
            is_error_conversion,
            is_inline,
            export_abi,
            asm_variants,
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

    /// Returns true if the function was declared with a body. A required
    /// method of a trait (`func eq(self, other: Self) -> bool;`) has none.
    pub fn has_body(&self) -> bool {
        self.flags.has_body()
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

    /// The well-formed `@asm(...)` variants on this function.
    pub fn asm_variants(&self) -> &[crate::inline_asm::AsmVariant] {
        &self.asm_variants
    }

    /// Whether this function is a `@test`.
    pub fn is_test(&self) -> bool {
        self.is_test
    }

    /// Whether this function is an `@error_conversion`.
    pub fn is_error_conversion(&self) -> bool {
        self.is_error_conversion
    }

    /// Whether this function is marked `@inline`.
    pub fn is_inline(&self) -> bool {
        self.is_inline
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

    /// The name the runtime links this function by: its [`full_name`]
    /// (`foo::bar`), with the type of the `extend` block between the
    /// module path and the name for a method (`foo::Point::length`).
    ///
    /// A module's dispatch table finds another module's function by this
    /// name, and two methods of one module with the same name (`Json::len`
    /// and `JsonObject::len`), or a method and a free function
    /// (`Json::equals` and `equals`), would otherwise be the same name, and
    /// the runtime could not tell which one a call means.
    ///
    /// [`full_name`]: Function::full_name
    pub fn link_name(self, db: &dyn HirDatabase) -> String {
        use crate::{ids::ItemContainerId, HirDisplay};
        match self.id.lookup(db).container {
            ItemContainerId::ImplId(impl_id) => {
                let self_ty = db.type_for_impl_self(impl_id);
                itertools::Itertools::intersperse(
                    self.module(db)
                        .path_to_root(db)
                        .into_iter()
                        .filter_map(|module| module.name(db))
                        .chain(once(self_ty.display(db).to_string()))
                        .chain(once(self.name(db).to_string())),
                    String::from("::"),
                )
                .collect()
            }
            _ => self.full_name(db),
        }
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

    /// Whether this function is a `@test` the `codira test` runner calls.
    pub fn is_test(self, db: &dyn HirDatabase) -> bool {
        self.data(db).is_test()
    }

    /// Whether this function is an `@error_conversion`, which `?` calls to
    /// pass an error on as the type it belongs to.
    pub fn is_error_conversion(self, db: &dyn HirDatabase) -> bool {
        self.data(db).is_error_conversion()
    }

    /// Whether this function is marked `@inline`: code generation asks
    /// LLVM to inline every call of it (`alwaysinline`).
    pub fn is_inline(self, db: &dyn HirDatabase) -> bool {
        self.data(db).is_inline()
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

    /// Every generic parameter this function's signature and body are
    /// written in terms of, in a fixed order: first those of the type its
    /// `extend` block is about (the `T` of `extend Box[T]`), then its own
    /// (`func map[U]`).
    ///
    /// An instance of the function gives one type per entry, in this order
    /// (see [`crate::mono::Instance`]); a function with no entries is not
    /// generic and has exactly one body to generate. `extend Box[i32]`
    /// contributes nothing: its `Box[i32]` is already concrete.
    pub fn generic_environment(self, db: &dyn HirDatabase) -> Vec<crate::ids::TypeParamId> {
        use crate::{
            ids::{GenericDefId, ItemContainerId, TypeParamId},
            ty::TypeWalk,
            TyKind,
        };

        let mut environment = Vec::new();
        match self.id.lookup(db).container {
            ItemContainerId::ImplId(impl_id) => {
                db.type_for_impl_self(impl_id).walk(&mut |ty| {
                    if let TyKind::TypeParam(id, _) = ty.interned() {
                        if !environment.contains(id) {
                            environment.push(*id);
                        }
                    }
                });
            }
            // A method of a trait is written in terms of the trait's `Self`,
            // so even a default body has one instance per implementing type.
            ItemContainerId::TraitId(trait_id) => environment.push(TypeParamId {
                owner: GenericDefId::TraitId(trait_id),
                index: 0,
            }),
            ItemContainerId::ModuleId(_) => {}
        }
        let own = self.data(db).generic_params().len();
        environment.extend((0..own).map(|index| TypeParamId {
            owner: GenericDefId::FunctionId(self.id),
            index: index as u32,
        }));
        environment
    }

    /// Whether this function is generic: whether it has a body per
    /// instantiation rather than one body. See
    /// [`Function::generic_environment`].
    pub fn is_generic(self, db: &dyn HirDatabase) -> bool {
        !self.generic_environment(db).is_empty()
    }

    pub fn is_extern(self, db: &dyn HirDatabase) -> bool {
        db.fn_data(self.id).flags.is_extern()
    }

    /// Whether this function is a compiler intrinsic, which codegen emits
    /// inline at the call site instead of calling as a symbol.
    pub fn is_intrinsic(self, db: &dyn HirDatabase) -> bool {
        db.fn_data(self.id).flags.is_intrinsic()
    }

    /// Whether this function is declared with `@asm(...)`: its calls are
    /// replaced by its inline assembly, so it never becomes a symbol either.
    pub fn is_inline_asm(self, db: &dyn HirDatabase) -> bool {
        !db.fn_data(self.id).asm_variants().is_empty()
    }

    /// The `@asm(...)` variant that applies on the target being compiled
    /// for, if the function has one for it.
    pub fn inline_asm(self, db: &dyn HirDatabase) -> Option<crate::inline_asm::AsmVariant> {
        let target = db.target();
        crate::inline_asm::select_variant(db.fn_data(self.id).asm_variants(), &target.arch).cloned()
    }

    pub(crate) fn body_source_map(self, db: &dyn HirDatabase) -> Arc<BodySourceMap> {
        db.body_with_source_map(self.id.into()).1
    }

    /// The source range of expression `expr` in this function's body, if it
    /// came from source.
    ///
    /// For consumers outside this crate that report their own diagnostics
    /// against HIR expressions -- `codira_quantum`'s compile-time elaborator
    /// points at the line where a kernel branches on a measurement -- and so
    /// need a location without access to the crate-private source map.
    pub fn expr_text_range(
        self,
        db: &dyn HirDatabase,
        expr: crate::ExprId,
    ) -> Option<codira_syntax::TextRange> {
        let source = self.body_source_map(db).expr_syntax(expr)?;
        Some(match source.value {
            either::Either::Left(ptr) => ptr.syntax_node_ptr().range(),
            either::Either::Right(ptr) => ptr.syntax_node_ptr().range(),
        })
    }

    /// The file and text range an expression of this function's body was
    /// written at, or `None` for an expression the compiler made up.
    ///
    /// Code generation uses this to give a runtime panic (an out-of-range
    /// index, say) the source location the language promises it.
    pub fn expr_source_range(
        self,
        db: &dyn HirDatabase,
        expr: crate::ExprId,
    ) -> Option<(FileId, codira_syntax::TextRange)> {
        let source = self.body_source_map(db).expr_syntax(expr)?;
        let range = source
            .value
            .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
            .range();
        Some((source.file_id, range))
    }

    pub fn diagnostics(self, db: &dyn HirDatabase, sink: &mut DiagnosticSink<'_>) {
        // Only a declaration of a foreign function (in an `extern` block)
        // or a required method of a trait may leave out the body. Anywhere
        // else there is nothing to generate code from: code generation used
        // to reach the missing body and panic.
        let data = db.fn_data(self.id);
        let in_trait = matches!(
            self.id.lookup(db).container,
            crate::ids::ItemContainerId::TraitId(_)
        );
        let source = self.source(db);
        // A declaration that ends in `;`: one cut short by a syntax error
        // already has that error.
        let declared = source
            .value
            .syntax()
            .last_token()
            .is_some_and(|token| token.kind() == codira_syntax::SyntaxKind::SEMI);
        if !data.has_body() && !data.is_extern() && !in_trait && declared {
            sink.push(crate::diagnostics::ExprError {
                file: source.file_id,
                node: codira_syntax::SyntaxNodePtr::new(source.value.syntax()),
                message: "a function needs a body: only a function in an `extern` block or a \
                          required method of a trait is declared without one"
                    .to_string(),
            });
        }
        db.function_bounds(self.id).add_diagnostics(sink);
        let body = self.body(db);
        body.add_diagnostics(db, self.into(), sink);
        let infer = self.infer(db);
        infer.add_diagnostics(db, self, sink);
        let validator = ExprValidator::new(self, db);
        validator.validate_body(sink);
        crate::escape::region_diagnostics(db, self, sink);
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
