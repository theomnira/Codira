//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{any::Any, fmt};

use codira_hir_input::FileId;
use codira_syntax::{ast, AstPtr, SmolStr, SyntaxNode, SyntaxNodePtr, TextRange};

use crate::{
    code_model::StructKind, ids::FunctionId, in_file::InFile, ty::cast::InvalidCastReason,
    HirDatabase, IntTy, Name, Ty,
};

/// Diagnostic defines `codira_hir` API for errors and warnings.
///
/// It is used as a `dyn` object, which you can downcast to concrete
/// diagnostics. [`DiagnosticSink`] are structured, meaning that they include
/// rich information which can be used by IDE to create fixes.
///
/// Internally, various subsystems of HIR produce diagnostics specific to a
/// subsystem (typically, an `enum`), which are safe to store in salsa but do
/// not include source locations. Such internal diagnostics are transformed into
/// an instance of `Diagnostic` on demand.
pub trait Diagnostic: Any + Send + Sync + fmt::Debug + 'static {
    fn message(&self) -> String;
    fn source(&self) -> InFile<SyntaxNodePtr>;
    fn highlight_range(&self) -> TextRange {
        self.source().value.range()
    }
    fn as_any(&self) -> &(dyn Any + Send + 'static);
}

pub trait AstDiagnostic {
    type AST;
    fn ast(&self, db: &dyn HirDatabase) -> Self::AST;
}

impl dyn Diagnostic {
    pub fn syntax_node(&self, db: &dyn HirDatabase) -> SyntaxNode {
        let node = db.parse(self.source().file_id).syntax_node();
        self.source().value.to_node(&node)
    }

    pub fn downcast_ref<D: Diagnostic>(&self) -> Option<&D> {
        self.as_any().downcast_ref()
    }
}

type DiagnosticCallback<'a> = Box<dyn FnMut(&dyn Diagnostic) -> Result<(), ()> + 'a>;
type DefaultDiagnosticsCallback<'a> = Box<dyn FnMut(&dyn Diagnostic) + 'a>;

pub struct DiagnosticSink<'a> {
    callbacks: Vec<DiagnosticCallback<'a>>,
    default_callback: DefaultDiagnosticsCallback<'a>,
}

impl<'a> DiagnosticSink<'a> {
    pub fn new(cb: impl FnMut(&dyn Diagnostic) + 'a) -> DiagnosticSink<'a> {
        DiagnosticSink {
            callbacks: Vec::new(),
            default_callback: Box::new(cb),
        }
    }

    pub fn on<D: Diagnostic, F: FnMut(&D) + 'a>(mut self, mut cb: F) -> DiagnosticSink<'a> {
        let cb = move |diag: &dyn Diagnostic| match diag.downcast_ref::<D>() {
            Some(d) => {
                cb(d);
                Ok(())
            }
            None => Err(()),
        };
        self.callbacks.push(Box::new(cb));
        self
    }

    pub(crate) fn push(&mut self, d: impl Diagnostic) {
        let d: &dyn Diagnostic = &d;
        for cb in self.callbacks.iter_mut() {
            if cb(d).is_ok() {
                return;
            }
        }
        (self.default_callback)(d);
    }
}

#[derive(Debug)]
pub struct UnresolvedValue {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
}

impl Diagnostic for UnresolvedValue {
    fn message(&self) -> String {
        "undefined value".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct UnresolvedType {
    pub file: FileId,
    pub type_ref: AstPtr<ast::TypeRef>,
}

impl Diagnostic for UnresolvedType {
    fn message(&self) -> String {
        "undefined type".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.type_ref.syntax_node_ptr())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct CyclicType {
    pub file: FileId,
    pub type_ref: AstPtr<ast::TypeRef>,
}

impl Diagnostic for CyclicType {
    fn message(&self) -> String {
        "cyclic type".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.type_ref.syntax_node_ptr())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct PrivateAccess {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
}

impl Diagnostic for PrivateAccess {
    fn message(&self) -> String {
        "access of private type".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct ExpectedFunction {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
    pub found: Ty,
}

impl Diagnostic for ExpectedFunction {
    fn message(&self) -> String {
        "expected function type".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct ExportedPrivate {
    pub file: FileId,
    pub type_ref: AstPtr<ast::TypeRef>,
}

impl Diagnostic for ExportedPrivate {
    fn message(&self) -> String {
        "can't leak private type".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.type_ref.syntax_node_ptr())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct ParameterCountMismatch {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
    pub expected: usize,
    pub found: usize,
}

impl Diagnostic for ParameterCountMismatch {
    fn message(&self) -> String {
        format!(
            "this function takes {} parameters but {} parameters was supplied",
            self.expected, self.found
        )
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct MismatchedType {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
    pub expected: Ty,
    pub found: Ty,
}

impl Diagnostic for MismatchedType {
    fn message(&self) -> String {
        "mismatched type".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct IncompatibleBranch {
    pub file: FileId,
    pub if_expr: SyntaxNodePtr,
    pub expected: Ty,
    pub found: Ty,
}

impl Diagnostic for IncompatibleBranch {
    fn message(&self) -> String {
        "mismatched branches".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.if_expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct InvalidLhs {
    /// The file that contains the expressions
    pub file: FileId,

    /// The expression containing the `lhs`
    pub expr: SyntaxNodePtr,

    /// The left-hand side of the expression.
    pub lhs: SyntaxNodePtr,
}

impl Diagnostic for InvalidLhs {
    fn message(&self) -> String {
        "invalid left hand side of expression".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.lhs.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct MissingElseBranch {
    pub file: FileId,
    pub if_expr: SyntaxNodePtr,
    pub found: Ty,
}

impl Diagnostic for MissingElseBranch {
    fn message(&self) -> String {
        "missing else branch".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.if_expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct CannotApplyBinaryOp {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
    pub lhs: Ty,
    pub rhs: Ty,
}

impl Diagnostic for CannotApplyBinaryOp {
    fn message(&self) -> String {
        "cannot apply binary operator".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct CannotApplyUnaryOp {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
    pub ty: Ty,
}

impl Diagnostic for CannotApplyUnaryOp {
    fn message(&self) -> String {
        "cannot apply unary operator".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// An `expr as Type` cast that the legality matrix in [`crate::ty::cast`]
/// rejects. `as` never silently reinterprets: a cast it cannot perform is a
/// type error.
#[derive(Debug)]
pub struct InvalidCast {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
    /// The inferred type of the operand.
    pub from: Ty,
    /// The resolved target type that was written after `as`.
    pub to: Ty,
    pub reason: InvalidCastReason,
}

impl Diagnostic for InvalidCast {
    fn message(&self) -> String {
        self.reason.message().to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct DuplicateDefinition {
    pub name: String,
    pub first_definition: InFile<SyntaxNodePtr>,
    pub definition: InFile<SyntaxNodePtr>,
}

impl Diagnostic for DuplicateDefinition {
    fn message(&self) -> String {
        format!("the name `{}` is defined multiple times", self.name)
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.definition.clone()
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct ReturnMissingExpression {
    pub file: FileId,
    pub return_expr: SyntaxNodePtr,
}

impl Diagnostic for ReturnMissingExpression {
    fn message(&self) -> String {
        "`return;` in a function whose return type is not `()`".to_owned()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.return_expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct BreakOutsideLoop {
    pub file: FileId,
    pub break_expr: SyntaxNodePtr,
}

impl Diagnostic for BreakOutsideLoop {
    fn message(&self) -> String {
        "`break` outside of a loop".to_owned()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.break_expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct BreakWithValueOutsideLoop {
    pub file: FileId,
    pub break_expr: SyntaxNodePtr,
}

impl Diagnostic for BreakWithValueOutsideLoop {
    fn message(&self) -> String {
        "`break` with value can only appear in a `loop`".to_owned()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.break_expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct AccessUnknownField {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
    pub receiver_ty: Ty,
    pub name: Name,
}

impl Diagnostic for AccessUnknownField {
    fn message(&self) -> String {
        "attempted to access a non-existent field in a struct.".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct FieldCountMismatch {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
    pub expected: usize,
    pub found: usize,
}

impl Diagnostic for FieldCountMismatch {
    fn message(&self) -> String {
        format!(
            "this tuple struct literal has {} field{} but {} field{} supplied",
            self.expected,
            if self.expected == 1 { "" } else { "s" },
            self.found,
            if self.found == 1 { " was" } else { "s were" },
        )
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct MissingFields {
    pub file: FileId,
    pub fields: SyntaxNodePtr,
    pub struct_ty: Ty,
    pub field_names: Vec<Name>,
}

impl Diagnostic for MissingFields {
    fn message(&self) -> String {
        use std::fmt::Write;
        let mut message = "missing record fields:\n".to_string();
        for field in &self.field_names {
            writeln!(message, "- {field}").unwrap();
        }
        message
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.fields.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct MismatchedStructLit {
    pub file: FileId,
    pub expr: SyntaxNodePtr,
    pub expected: StructKind,
    pub found: StructKind,
}

impl Diagnostic for MismatchedStructLit {
    fn message(&self) -> String {
        format!(
            "mismatched struct literal kind. expected `{}`, found `{}`",
            self.expected, self.found
        )
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct NoFields {
    pub file: FileId,
    pub receiver_expr: SyntaxNodePtr,
    pub found: Ty,
}

impl Diagnostic for NoFields {
    fn message(&self) -> String {
        "attempted to access a field on a primitive type.".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.receiver_expr.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}
#[derive(Debug)]
pub struct NoSuchField {
    pub file: FileId,
    pub field: SyntaxNodePtr,
}

impl Diagnostic for NoSuchField {
    fn message(&self) -> String {
        "no such field".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.field.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct PossiblyUninitializedVariable {
    pub file: FileId,
    pub pat: SyntaxNodePtr,
}

impl Diagnostic for PossiblyUninitializedVariable {
    fn message(&self) -> String {
        "use of possibly-uninitialized variable".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.pat.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// A call to a function declaring `uses Effect, ...` (see
/// `spec/LANGUAGE_SPEC.md` section 6) from a caller that doesn't itself
/// declare `Effect` in its own `uses` clause. Emitted by
/// `expr::validator::effect_obligation` -- the "declare `uses` and
/// propagate the obligation" half of the spec's effect-obligation rule
/// (the "or be inside a matching `handle` block" half is not checked yet,
/// since `handle` still lowers to `Expr::Missing`; see that validator's
/// module doc for the precise scope).
#[derive(Debug)]
pub struct UndeclaredEffect {
    pub file: FileId,
    pub call: SyntaxNodePtr,
    pub effect: Name,
    pub callee: Name,
}

impl Diagnostic for UndeclaredEffect {
    fn message(&self) -> String {
        format!(
            "call to `{}` requires effect `{}`, which is not declared in this function's `uses` clause",
            self.callee, self.effect
        )
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.call.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// A refinement type (`Type { binder | predicate }`, see
/// `spec/LANGUAGE_SPEC.md` §7) whose predicate `codira_smt` (a real Z3
/// solver -- see `crate::refinement_check`) has proven unsatisfiable: no
/// value can ever inhabit this type. Emitted for `type` alias declarations
/// only (see `refinement_check`'s module doc for why).
#[derive(Debug)]
pub struct UnsatisfiableRefinement {
    pub file: FileId,
    pub type_ref: SyntaxNodePtr,
    pub predicate_text: String,
}

impl Diagnostic for UnsatisfiableRefinement {
    fn message(&self) -> String {
        format!(
            "unsatisfiable refinement type: no value can ever satisfy `{}` (proven by SMT solver, not just a heuristic)",
            self.predicate_text
        )
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.type_ref.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// A `@heal(..., postcondition: <expr>)` healing contract (see
/// `crate::heal_check`) whose postcondition `codira_smt` has proven
/// unsatisfiable: no return value could ever discharge it, so the contract
/// can never be honored by any recovery strategy.
#[derive(Debug)]
pub struct UnsatisfiableHealingPostcondition {
    pub file: FileId,
    pub func: SyntaxNodePtr,
}

impl Diagnostic for UnsatisfiableHealingPostcondition {
    fn message(&self) -> String {
        "unsatisfiable healing-contract postcondition: no return value can ever satisfy it (proven by SMT solver, not just a heuristic)".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.func.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// A `consuming` parameter (see `spec/LANGUAGE_SPEC.md` section 14) used
/// more than once in an `@strict` function's body (section 17). The first
/// use is the move; any further use is a use-after-consume, the same class
/// of bug a real borrow checker rejects -- see
/// `expr::validator::move_check`'s module doc for exactly what this v1
/// does and does not catch.
#[derive(Debug)]
pub struct UseAfterConsume {
    pub file: FileId,
    /// The second (or later) use -- the first use is not itself an error.
    pub use_site: SyntaxNodePtr,
    pub binding: Name,
}

impl Diagnostic for UseAfterConsume {
    fn message(&self) -> String {
        format!(
            "use of `{}` after it was consumed -- `consuming` parameters may only be used once in an `@strict` function",
            self.binding
        )
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.use_site.clone())
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct ExternCannotHaveBody {
    pub func: InFile<SyntaxNodePtr>,
}

impl Diagnostic for ExternCannotHaveBody {
    fn message(&self) -> String {
        "extern functions cannot have bodies".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.func.clone()
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct ExternNonPrimitiveParam {
    pub param: InFile<SyntaxNodePtr>,
}

impl Diagnostic for ExternNonPrimitiveParam {
    fn message(&self) -> String {
        "extern functions can only have primitives as parameter- and return types".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.param.clone()
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// An error that is emitted if a literal is too large to even parse
#[derive(Debug)]
pub struct IntLiteralTooLarge {
    pub literal: InFile<AstPtr<ast::Literal>>,
}

impl Diagnostic for IntLiteralTooLarge {
    fn message(&self) -> String {
        "int literal is too large".to_owned()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.literal.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// An error that is emitted if a literal is too large for its suffix
#[derive(Debug)]
pub struct LiteralOutOfRange {
    pub literal: InFile<AstPtr<ast::Literal>>,
    pub int_ty: IntTy,
}

impl Diagnostic for LiteralOutOfRange {
    fn message(&self) -> String {
        format!("literal out of range for `{}`", self.int_ty.as_str())
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.literal.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// An error that is emitted for a literal with an invalid suffix (e.g.
/// `123_foo`)
#[derive(Debug)]
pub struct InvalidLiteralSuffix {
    pub literal: InFile<AstPtr<ast::Literal>>,
    pub suffix: SmolStr,
}

impl Diagnostic for InvalidLiteralSuffix {
    fn message(&self) -> String {
        format!("invalid suffix `{}`", self.suffix)
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.literal.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// An error that is emitted for a literal with a floating point suffix with a
/// non 10 base (e.g. `0x123_f32`)
#[derive(Debug)]
pub struct InvalidFloatingPointLiteral {
    pub literal: InFile<AstPtr<ast::Literal>>,
    pub base: u32,
}

impl Diagnostic for InvalidFloatingPointLiteral {
    fn message(&self) -> String {
        match self.base {
            2 => "binary float literal is not supported".to_owned(),
            8 => "octal float literal is not supported".to_owned(),
            16 => "hexadecimal float literal is not supported".to_owned(),
            _ => "unsupported base for floating pointer literal".to_owned(),
        }
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.literal.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// An error that is emitted for a malformed literal (e.g. `0b22222`)
#[derive(Debug)]
pub struct InvalidLiteral {
    pub literal: InFile<AstPtr<ast::Literal>>,
}

impl Diagnostic for InvalidLiteral {
    fn message(&self) -> String {
        "invalid literal value".to_owned()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.literal.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct FreeTypeAliasWithoutTypeRef {
    pub type_alias_def: InFile<SyntaxNodePtr>,
}

impl Diagnostic for FreeTypeAliasWithoutTypeRef {
    fn message(&self) -> String {
        "free type alias without type ref".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.type_alias_def.clone()
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct UnresolvedImport {
    pub use_tree: InFile<AstPtr<ast::UseTree>>,
}

impl Diagnostic for UnresolvedImport {
    fn message(&self) -> String {
        "unresolved import".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.use_tree.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send) {
        self
    }
}

#[derive(Debug)]
pub struct ImportDuplicateDefinition {
    pub use_tree: InFile<AstPtr<ast::UseTree>>,
}

impl Diagnostic for ImportDuplicateDefinition {
    fn message(&self) -> String {
        "a second item with the same name imported. Try to use an alias.".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.use_tree.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send) {
        self
    }
}

#[derive(Debug)]
pub struct PrivateTypeAlias {
    pub type_alias_def: InFile<SyntaxNodePtr>,
    pub kind: String,
    pub name: String,
}

impl Diagnostic for PrivateTypeAlias {
    fn message(&self) -> String {
        format!("{} `{}` is private", self.kind, self.name)
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.type_alias_def.clone()
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct ImplForForeignType {
    pub impl_: InFile<AstPtr<ast::Extend>>,
}

impl Diagnostic for ImplForForeignType {
    fn message(&self) -> String {
        String::from("cannot define inherent `extend` for foreign type")
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.impl_.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

#[derive(Debug)]
pub struct InvalidSelfTyImpl {
    pub impl_: InFile<AstPtr<ast::Extend>>,
}

impl Diagnostic for InvalidSelfTyImpl {
    fn message(&self) -> String {
        String::from("inherent `extend` blocks can only be added for structs")
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.impl_.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// An error that is emitted if a method is called that is not visible from the
/// current scope
#[derive(Debug)]
pub struct MethodNotInScope {
    pub method_call: InFile<AstPtr<ast::MethodCallExpr>>,
    pub receiver_ty: Ty,
}

impl Diagnostic for MethodNotInScope {
    fn message(&self) -> String {
        "method not in scope for type".to_string()
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.method_call.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// An error that is emitted if an unknown method is called
#[derive(Debug)]
pub struct MethodNotFound {
    pub method_call: InFile<AstPtr<ast::MethodCallExpr>>,
    pub receiver_ty: Ty,
    pub method_name: Name,
    pub field_with_same_name: Option<Ty>,
    pub associated_function_with_same_name: Option<FunctionId>,
}

impl Diagnostic for MethodNotFound {
    fn message(&self) -> String {
        format!("method `{}` does not exist", self.method_name)
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.method_call.clone().map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// An error emitted when a `supervisor` block declares two `child` entries
/// with the same name (see `spec/self_healing_programming_language.md`
/// section 3.5). Structural validation only: `supervisor`/`child` blocks
/// parse into a full syntax tree (`codira_syntax`) but are not lowered into
/// name-resolvable HIR items, so this walks the raw syntax tree directly
/// rather than going through the usual item-diagnostic machinery.
#[derive(Debug)]
pub struct DuplicateSupervisorChild {
    pub file: FileId,
    pub duplicate: AstPtr<ast::ChildDef>,
    pub name: Name,
}

impl Diagnostic for DuplicateSupervisorChild {
    fn message(&self) -> String {
        format!(
            "a child named `{}` is already declared in this supervisor",
            self.name
        )
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.duplicate.clone()).map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// An error emitted when a `supervisor` or `child` block declares the same
/// configuration key (e.g. `strategy`) more than once at the same level.
#[derive(Debug)]
pub struct DuplicateSupervisorEntry {
    pub file: FileId,
    pub duplicate: AstPtr<ast::SupervisorEntry>,
    pub name: Name,
}

impl Diagnostic for DuplicateSupervisorEntry {
    fn message(&self) -> String {
        format!("the key `{}` is already set in this block", self.name)
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        InFile::new(self.file, self.duplicate.clone()).map(Into::into)
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}

/// A set of module-level bindings that depend on one another in a cycle,
/// so none of them has a value.
///
/// Emitted once per participating binding: each is an equally valid place
/// to break the cycle, and singling one out would be arbitrary.
#[derive(Debug)]
pub struct CyclicConstDefinition {
    /// The cycle rendered as `A -> B -> C`, for the message.
    pub cycle: String,
    pub definition: InFile<SyntaxNodePtr>,
}

impl Diagnostic for CyclicConstDefinition {
    fn message(&self) -> String {
        format!(
            "module-level bindings form a dependency cycle: {}",
            self.cycle
        )
    }

    fn source(&self) -> InFile<SyntaxNodePtr> {
        self.definition.clone()
    }

    fn as_any(&self) -> &(dyn Any + Send + 'static) {
        self
    }
}
