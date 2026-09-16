//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::sync::Arc;

use codira_syntax::{AstNode, SyntaxNodePtr};

use crate::{
    code_model::src::HasSource,
    diagnostics::{
        CyclicType, DiagnosticSink, ExportedPrivate, ExternCannotHaveBody, ExternNonPrimitiveParam,
        FreeTypeAliasWithoutTypeRef, PrivateTypeAlias,
    },
    expr::BodySourceMap,
    in_file::InFile,
    refinement_check::{check_satisfiable, RefinementCheckResult},
    resolve::HasResolver,
    Body, Expr, Function, HasVisibility, HirDatabase, InferenceResult, Ty, TyKind, TypeAlias,
    Visibility,
};

mod effect_obligation;
mod literal_out_of_range;
mod move_check;
mod uninitialized_access;

#[cfg(test)]
mod tests;

pub struct ExprValidator<'a> {
    func: Function,
    infer: Arc<InferenceResult>,
    body: Arc<Body>,
    body_source_map: Arc<BodySourceMap>,
    db: &'a dyn HirDatabase,
}

impl<'a> ExprValidator<'a> {
    pub fn new(func: Function, db: &'a dyn HirDatabase) -> Self {
        let (body, body_source_map) = db.body_with_source_map(func.id.into());
        ExprValidator {
            func,
            db,
            infer: db.infer(func.id.into()),
            body,
            body_source_map,
        }
    }

    pub fn validate_body(&self, sink: &mut DiagnosticSink<'_>) {
        self.validate_literal_ranges(sink);
        self.validate_uninitialized_access(sink);
        self.validate_extern(sink);
        self.validate_privacy(sink);
        self.validate_effect_obligations(sink);
        self.validate_healing_contract(sink);
        self.validate_move_checking(sink);
    }

    pub fn validate_privacy(&self, sink: &mut DiagnosticSink<'_>) {
        let resolver = self.func.id.resolver(self.db);
        let fn_data = self.func.data(self.db);
        let ret_type_ref = fn_data.ret_type();
        let param_types = fn_data
            .params()
            .iter()
            .chain(std::iter::once(ret_type_ref))
            .map(|type_ref| {
                let (ty, _) = Ty::from_hir(self.db, &resolver, fn_data.type_ref_map(), *type_ref);
                (ty, type_ref)
            });

        let fn_visibility = self.func.visibility(self.db);
        let type_is_allowed = |ty: &Ty| match fn_visibility {
            Visibility::Module(module_id) => {
                ty.visibility(self.db).is_visible_from(self.db, module_id)
            }
            Visibility::Public => ty.visibility(self.db).is_externally_visible(),
        };

        let file_id = self.func.source(self.db).file_id;
        param_types
            .filter(|(ty, _)| !type_is_allowed(ty))
            .for_each(|(_, type_ref)| {
                sink.push(ExportedPrivate {
                    file: file_id,
                    type_ref: fn_data
                        .type_ref_source_map()
                        .type_ref_syntax(*type_ref)
                        .unwrap(),
                });
            });
    }

    /// Validates a `@heal(...)` healing contract.
    ///
    /// Two complementary checks, kept separate because they answer
    /// different questions:
    ///
    /// * `crate::heal_check` asks whether the `postcondition:` is satisfiable
    ///   by *any* return value -- a well-formedness property of the predicate
    ///   itself.
    /// * `crate::heal_contract` resolves and validates the contract's `on:` and
    ///   `strategies:` lists, which spec section 2.2 requires to be checked
    ///   statically and which were previously read by nothing at all.
    pub fn validate_healing_contract(&self, sink: &mut DiagnosticSink<'_>) {
        let src = self.func.source(self.db);
        if crate::heal_check::check_heal_postcondition(&src.value)
            == Some(crate::refinement_check::RefinementCheckResult::Unsatisfiable)
        {
            sink.push(crate::diagnostics::UnsatisfiableHealingPostcondition {
                file: src.file_id,
                func: SyntaxNodePtr::new(src.value.syntax()),
            });
        }

        crate::heal_contract::heal_contract_diagnostics(self.db, self.func, sink);
    }

    pub fn validate_extern(&self, sink: &mut DiagnosticSink<'_>) {
        if !self.func.is_extern(self.db) {
            return;
        }

        // Validate that there is no body
        match self.body[self.func.body(self.db).body_expr] {
            Expr::Missing => {}
            _ => sink.push(ExternCannotHaveBody {
                func: self
                    .func
                    .source(self.db)
                    .map(|f| SyntaxNodePtr::new(f.syntax())),
            }),
        }

        if let Some(sig) = self.func.ty(self.db).callable_sig(self.db) {
            let fn_data = self.func.data(self.db);
            for (arg_ty, ty_ref) in sig.params().iter().zip(fn_data.params()) {
                if arg_ty.as_struct().is_some() {
                    let arg_ptr = fn_data
                        .type_ref_source_map()
                        .type_ref_syntax(*ty_ref)
                        .map(|ptr| ptr.syntax_node_ptr())
                        .unwrap();
                    sink.push(ExternNonPrimitiveParam {
                        param: InFile::new(self.func.source(self.db).file_id, arg_ptr),
                    });
                }
            }

            let return_ty = sig.ret();
            if return_ty.as_struct().is_some() {
                let arg_ptr = fn_data
                    .type_ref_source_map()
                    .type_ref_syntax(*fn_data.ret_type())
                    .map(|ptr| ptr.syntax_node_ptr())
                    .unwrap();
                sink.push(ExternNonPrimitiveParam {
                    param: InFile::new(self.func.source(self.db).file_id, arg_ptr),
                });
            }
        }
    }
}

pub struct TypeAliasValidator<'a> {
    type_alias: TypeAlias,
    db: &'a dyn HirDatabase,
}

impl<'a> TypeAliasValidator<'a> {
    /// Constructs a validator for the provided `TypeAlias`.
    pub fn new(type_alias: TypeAlias, db: &'a dyn HirDatabase) -> Self {
        TypeAliasValidator { type_alias, db }
    }

    /// Validates that the provided `TypeAlias` has a target type of alias.
    pub fn validate_target_type_existence(&self, sink: &mut DiagnosticSink<'_>) {
        let src = self.type_alias.source(self.db);
        if src.value.type_ref().is_none() {
            sink.push(FreeTypeAliasWithoutTypeRef {
                type_alias_def: src.map(|t| SyntaxNodePtr::new(t.syntax())),
            });
        }
    }

    /// Validates that the provided `TypeAlias` is not leaking the privacy of
    /// its target type.
    pub fn validate_target_type_privacy(&self, sink: &mut DiagnosticSink<'_>) {
        let lower = self.type_alias.lower(self.db);
        let data = self.type_alias.data(self.db);
        let target_ty = &lower[data.type_ref_id];

        let target_visibility = target_ty.visibility(self.db);
        let alias_visibility = self.type_alias.visibility(self.db);

        let leaks_privacy = match alias_visibility {
            Visibility::Module(module) => !target_visibility.is_visible_from(self.db, module),
            Visibility::Public => !target_visibility.is_externally_visible(),
        };

        if leaks_privacy {
            let src = self.type_alias.source(self.db);

            let (kind, name) = match target_ty.interned() {
                TyKind::Struct(s) => ("struct", s.name(self.db)),
                TyKind::TypeAlias(a) => ("type alias", a.name(self.db)),
                _ => unreachable!(),
            };

            sink.push(PrivateTypeAlias {
                type_alias_def: src.map(|t| SyntaxNodePtr::new(t.syntax())),
                kind: kind.to_string(),
                name: name.to_string(),
            });
        }
    }

    /// Validates that, if this `TypeAlias`'s target is a refinement type
    /// (`Type { binder | predicate }`), the predicate is satisfiable --
    /// see `crate::refinement_check`. Only the top-level target type is
    /// checked (`type Foo = i32 { x | .. }`, not a refinement nested
    /// inside e.g. an array element type), matching the language spec's
    /// own primary documented use case (a refinement type must be named
    /// via a `type` alias to be spelled unambiguously at all -- see
    /// `spec/LANGUAGE_SPEC.md` §12's refinement-type limitation note).
    pub fn validate_refinement_satisfiability(&self, sink: &mut DiagnosticSink<'_>) {
        let src = self.type_alias.source(self.db);
        let Some(type_ref) = src.value.type_ref() else {
            return;
        };
        let codira_syntax::ast::TypeRefKind::RefinementType(rt) = type_ref.kind() else {
            return;
        };
        let (Some(binder), Some(predicate)) = (rt.binder(), rt.predicate()) else {
            return;
        };

        if check_satisfiable(&binder.text(), &predicate) == RefinementCheckResult::Unsatisfiable {
            sink.push(crate::diagnostics::UnsatisfiableRefinement {
                file: src.file_id,
                type_ref: SyntaxNodePtr::new(type_ref.syntax()),
                predicate_text: predicate.syntax().text().to_string(),
            });
        }
    }

    /// Validates the provided `TypeAlias` is not cyclic.
    pub fn validate_acyclic(&self, sink: &mut DiagnosticSink<'_>) {
        let mut next_alias = Some(self.type_alias);

        let mut ids = Vec::new();
        while let Some(alias) = next_alias.take() {
            let type_ref = alias.type_ref(self.db);

            // Detect cyclic type
            if ids.contains(&alias.id) {
                let src = self.type_alias.source(self.db);
                sink.push(CyclicType {
                    file: src.file_id,
                    type_ref: self
                        .type_alias
                        .data(self.db)
                        .type_ref_source_map()
                        .type_ref_syntax(type_ref)
                        .unwrap(),
                });
                break;
            }

            ids.push(alias.id);

            let ty = Ty::from_hir(
                self.db,
                &alias.id.resolver(self.db),
                alias.data(self.db).type_ref_map(),
                type_ref,
            )
            .0;

            if let TyKind::TypeAlias(alias) = ty.into_inner() {
                next_alias = Some(alias);
            }
        }
    }
}
