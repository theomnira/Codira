//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Compile-time elaboration of a Codira function into a combinational
//!   [`Circuit`]: `oblivious` functions for `codira attest` (zero-knowledge
//!   proofs) and `@target(fpga)` functions for Verilog.
//!
//! # How
//!
//! The function's HIR body is interpreted symbolically (the same `Body`,
//! `Expr`, `Statement`, `Pat` and inference result `mir_lower` and the
//! quantum elaborator read; the dependency points from this crate to HIR,
//! never back). Parameters become circuit inputs. Every value is a circuit
//! [`Wire`]; a value that does not depend on an input is a constant node, so
//! the same interpreter both computes at compile time and builds gates:
//!
//! * integer and `bool` literals, parameters, `let`/`var` and assignment (plain
//!   and compound);
//! * `+ - *` (wrapping), `& | ^ ~ !`, `<< >>` (arithmetic on signed types), `==
//!   != < <= > >=` (signed or unsigned by type), unary `-`, integer casts;
//! * `ct_select(c, a, b)` (a multiplexer) and `declassify(e)` (the identity: in
//!   a circuit the revealed value is the output, chosen by the caller);
//! * calls to other Codira functions, inlined;
//! * `if` and `&&`/`||`: on a compile-time condition, only the branch taken is
//!   elaborated; on a run-time condition (possible in an `@target(fpga)`
//!   function, never in an `oblivious` one, whose secrets may not steer a
//!   branch) both branches are elaborated and every local they assign is merged
//!   with a multiplexer;
//! * `while`, `loop` and `for i in a..b` / `a..=b` with compile-time bounds,
//!   unrolled, with `break` and `continue`; `return`, also from inside a loop,
//!   as long as no run-time condition guards it.
//!
//! `/` and `%` are computed when both operands are compile-time values and
//! refused otherwise. Everything else is refused with the source location:
//! floats, 128-bit integers, strings, arrays, tuples, structs, classes,
//! methods, effects, `print`, concurrency, external functions, recursion
//! deeper than the call limit, and loops without compile-time bounds (a fuel
//! limit stops those).
//!
//! # `attest` and `oblivious`
//!
//! [`Mode::Attest`] accepts only an `oblivious func` as the entry, and in it
//! passes a run-time value only to functions that are themselves `oblivious`
//! or `@constant_time`, mirroring `codira_hir::verify::flow`. The flow
//! checker itself runs as part of the ordinary diagnostics, which every
//! driver (the CLI included) runs before elaborating; this module does not
//! repeat it. What it does guarantee by construction is the property a
//! circuit needs: no branch, loop bound, index or division depends on an
//! input.

use std::{collections::HashMap, fmt, sync::Arc};

use codira_hir::{
    ArithOp, BinaryOp, Body, BuiltinFn, CallableDef, CmpOp, Expr, ExprId, Function, HasSource,
    HirDatabase, HirDisplay, InferenceResult, IntBitness, Literal, LogicOp, Name, Ordering, Pat,
    PatId, ResolveBitness, Signedness, Statement, TyKind, UnaryOp,
};
use codira_syntax::ast::{self, AttributeOwner};

use crate::ir::{Builder, Circuit, Op, Ty, Wire};

/// What the entry function must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `codira attest`: an `oblivious func`.
    Attest,
    /// Verilog: a function marked `@target(fpga)`.
    Fpga,
}

/// Limits that keep elaboration finite and deterministic (no wall clock).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElabOptions {
    /// Loop iterations plus function calls, across the whole elaboration.
    pub fuel: u64,
    /// The deepest call nesting.
    pub max_depth: usize,
    /// The most circuit nodes.
    pub max_nodes: usize,
}

impl Default for ElabOptions {
    fn default() -> Self {
        ElabOptions {
            fuel: 1_000_000,
            max_depth: 128,
            max_nodes: 2_000_000,
        }
    }
}

/// Where an elaboration error points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub file: String,
    /// 1-based.
    pub line: u32,
    /// 1-based, in characters.
    pub column: u32,
    pub function: String,
}

/// An elaboration failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElabError {
    pub message: String,
    pub location: Option<SourceLocation>,
}

impl fmt::Display for ElabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.location {
            Some(loc) => write!(
                f,
                "{}:{}:{}: in `{}`: {}",
                loc.file, loc.line, loc.column, loc.function, self.message
            ),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ElabError {}

fn plain(message: impl Into<String>) -> ElabError {
    ElabError {
        message: message.into(),
        location: None,
    }
}

/// Whether `func` is declared `oblivious func`.
pub fn is_oblivious(db: &dyn HirDatabase, func: Function) -> bool {
    func.source(db).value.is_oblivious()
}

fn has_attribute(db: &dyn HirDatabase, func: Function, name: &str, arg: Option<&str>) -> bool {
    let Some(attrs) = func.source(db).value.attribute_list() else {
        return false;
    };
    attrs.attributes().any(|attr| {
        let named = attr.path().and_then(|p| p.segment()).is_some_and(
            |s| matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == name),
        );
        named
            && arg.is_none_or(|arg| {
                attr.arg_list().is_some_and(|args| {
                    args.args().next().is_some_and(|a| {
                        codira_syntax::AstNode::syntax(&a).text().to_string().trim() == arg
                    })
                })
            })
    })
}

/// Whether `func` carries `@target(fpga)`.
pub fn is_fpga(db: &dyn HirDatabase, func: Function) -> bool {
    has_attribute(db, func, "target", Some("fpga"))
}

/// Whether `func` is constant-time: `oblivious` or `@constant_time`.
pub fn is_constant_time(db: &dyn HirDatabase, func: Function) -> bool {
    is_oblivious(db, func) || has_attribute(db, func, "constant_time", None)
}

/// The functions `wanted` selects among `functions`, in declaration order.
fn all_functions(db: &dyn HirDatabase, wanted: impl Fn(Function) -> bool) -> Vec<Function> {
    let mut found = Vec::new();
    for package in codira_hir::Package::all(db) {
        for module in package.modules(db) {
            for func in module.all_functions(db) {
                if wanted(func) {
                    found.push(func);
                }
            }
        }
    }
    found
}

/// Every `oblivious func` in the database, in declaration order.
pub fn find_oblivious(db: &dyn HirDatabase) -> Vec<Function> {
    all_functions(db, |f| is_oblivious(db, f))
}

/// Every `@target(fpga)` function in the database, in declaration order.
pub fn find_fpga(db: &dyn HirDatabase) -> Vec<Function> {
    all_functions(db, |f| is_fpga(db, f))
}

/// Elaborates `func`, which must suit `mode`, into a circuit whose inputs
/// are its parameters and whose output is its result.
pub fn elaborate(
    db: &dyn HirDatabase,
    func: Function,
    mode: Mode,
    options: &ElabOptions,
) -> Result<Circuit, ElabError> {
    let name = func.name(db).to_string();
    match mode {
        Mode::Attest if !is_oblivious(db, func) => {
            return Err(plain(format!(
                "`{name}` is not an `oblivious func`; `attest` proves only oblivious functions, \
                 whose control flow and memory accesses do not depend on their inputs"
            )))
        }
        Mode::Fpga if !is_fpga(db, func) => {
            return Err(plain(format!("`{name}` is not marked `@target(fpga)`")))
        }
        _ => {}
    }
    if func.is_extern(db) {
        return Err(plain(format!(
            "`{name}` is external; it has no body to elaborate"
        )));
    }

    let mut elab = Elaborator {
        db,
        options,
        mode,
        builder: Builder::new(),
        fuel: options.fuel,
        depth: 0,
        layout: db.target_data_layout(),
    };

    // The parameters are the circuit's inputs.
    let body = func.body(db);
    let infer = func.infer(db);
    let mut args = Vec::new();
    for (pat, _) in body.params() {
        let Pat::Bind { name: pname } = &body[*pat] else {
            return Err(plain(format!(
                "`{name}` has a parameter that is not a plain name; circuit inputs need names"
            )));
        };
        let ty = elab
            .circuit_ty(&infer[*pat])
            .map_err(|why| plain(format!("parameter `{pname}` of `{name}`: {why}")))?;
        args.push(Value::Word(elab.builder.input(pname.to_string(), ty)));
    }
    let ret = func.ret_type(db);
    if ret.is_empty() {
        return Err(plain(format!(
            "`{name}` returns nothing; a circuit's output is the function's result"
        )));
    }
    elab.circuit_ty(&ret)
        .map_err(|why| plain(format!("the result of `{name}`: {why}")))?;

    let result = elab.call(func, &args, None)?;
    let Value::Word(output) = result else {
        return Err(plain(format!("`{name}` produced no value")));
    };
    Ok(elab.builder.finish(name, output))
}

/// A value during elaboration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value {
    Word(Wire),
    Unit,
    /// A `let` with no initializer, not assigned yet.
    Uninit,
}

/// Non-local control flow, and errors, while interpreting.
enum Flow {
    Break(Value),
    Continue,
    Return(Value),
    Error(ElabError),
}

impl From<ElabError> for Flow {
    fn from(e: ElabError) -> Self {
        Flow::Error(e)
    }
}

type Eval = Result<Value, Flow>;

struct Elaborator<'a> {
    db: &'a dyn HirDatabase,
    options: &'a ElabOptions,
    mode: Mode,
    builder: Builder,
    fuel: u64,
    depth: usize,
    layout: Arc<codira_target::abi::TargetDataLayout>,
}

/// One function activation.
struct Frame {
    func: Function,
    body: Arc<Body>,
    infer: Arc<InferenceResult>,
    scopes: Vec<HashMap<Name, Value>>,
}

impl Frame {
    fn lookup(&self, name: &Name) -> Option<Value> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn assign(&mut self, name: &Name, value: Value) -> bool {
        match self.scopes.iter_mut().rev().find(|s| s.contains_key(name)) {
            Some(scope) => {
                scope.insert(name.clone(), value);
                true
            }
            None => false,
        }
    }
}

impl Elaborator<'_> {
    /// The circuit type for a HIR type, or why there is none.
    fn circuit_ty(&self, ty: &codira_hir::Ty) -> Result<Ty, String> {
        match ty.interned() {
            TyKind::Bool => Ok(Ty::Bool),
            TyKind::Int(int) => {
                let width = match int.bitness.resolve(&self.layout) {
                    IntBitness::X8 => 8,
                    IntBitness::X16 => 16,
                    IntBitness::X32 => 32,
                    IntBitness::X64 => 64,
                    IntBitness::X128 | IntBitness::Xsize => {
                        return Err(format!(
                            "`{}` is not supported in circuits yet (integers up to 64 bits are)",
                            ty.display(self.db)
                        ))
                    }
                };
                Ok(Ty::Int {
                    width,
                    signed: int.signedness == Signedness::Signed,
                })
            }
            TyKind::Float(_) => Err(format!(
                "`{}` is a floating-point type; circuits compute exactly over integers and \
                 booleans, and floats are not supported",
                ty.display(self.db)
            )),
            _ => Err(format!(
                "values of type `{}` are not supported in circuits (integers of 8 to 64 bits and \
                 `bool` are; arrays, tuples, structs and objects are not)",
                ty.display(self.db)
            )),
        }
    }

    fn burn(&mut self, frame: &Frame, at: ExprId) -> Result<(), Flow> {
        if self.fuel == 0 {
            return Err(self.error(
                frame,
                at,
                format!(
                    "elaboration ran out of fuel ({} loop iterations and calls); a circuit's \
                     loops must have compile-time bounds",
                    self.options.fuel
                ),
            ));
        }
        self.fuel -= 1;
        if self.builder.len() > self.options.max_nodes {
            return Err(self.error(
                frame,
                at,
                format!("the circuit exceeds {} nodes", self.options.max_nodes),
            ));
        }
        Ok(())
    }

    fn error(&self, frame: &Frame, at: ExprId, message: impl Into<String>) -> Flow {
        Flow::Error(ElabError {
            message: message.into(),
            location: self.locate(frame.func, at),
        })
    }

    fn locate(&self, func: Function, at: ExprId) -> Option<SourceLocation> {
        let range = func.expr_text_range(self.db, at)?;
        let file_id = func.file_id(self.db);
        let text = self.db.file_text(file_id);
        let offset = usize::from(range.start()).min(text.len());
        let before = &text[..offset];
        let line = u32::try_from(before.matches('\n').count()).unwrap_or(u32::MAX) + 1;
        let column = u32::try_from(before.rsplit('\n').next().map_or(0, |l| l.chars().count()))
            .unwrap_or(u32::MAX)
            + 1;
        Some(SourceLocation {
            file: self.db.file_relative_path(file_id).to_string(),
            line,
            column,
            function: func.name(self.db).to_string(),
        })
    }

    /// Calls `func` with `args`. `site` is the call in the caller, for error
    /// locations.
    fn call(
        &mut self,
        func: Function,
        args: &[Value],
        site: Option<(&Frame, ExprId)>,
    ) -> Result<Value, ElabError> {
        let at_site = |this: &Self, message: String| match site {
            Some((frame, at)) => match this.error(frame, at, message) {
                Flow::Error(e) => e,
                _ => unreachable!("error() makes an error"),
            },
            None => plain(message),
        };
        if self.depth >= self.options.max_depth {
            let message = format!(
                "calls nest deeper than {} (recursion is unrolled at compile time, so it needs a \
                 compile-time bound)",
                self.options.max_depth
            );
            return Err(at_site(self, message));
        }
        let body = func.body(self.db);
        if body.params().len() != args.len() {
            let message = format!(
                "`{}` takes {} arguments",
                func.name(self.db),
                body.params().len()
            );
            return Err(at_site(self, message));
        }
        let mut scope = HashMap::new();
        for ((pat, _), value) in body.params().iter().zip(args) {
            match &body[*pat] {
                Pat::Bind { name } => {
                    scope.insert(name.clone(), *value);
                }
                Pat::Wild => {}
                _ => {
                    let message = format!(
                        "`{}` has a destructuring parameter; circuits support plain names only",
                        func.name(self.db)
                    );
                    return Err(at_site(self, message));
                }
            }
        }
        let mut frame = Frame {
            func,
            infer: func.infer(self.db),
            scopes: vec![scope],
            body,
        };
        self.depth += 1;
        let root = frame.body.body_expr();
        let result = self.eval(&mut frame, root);
        self.depth -= 1;
        match result {
            Ok(v) | Err(Flow::Return(v)) => Ok(v),
            Err(Flow::Break(_) | Flow::Continue) => {
                Err(plain("`break` or `continue` outside of a loop"))
            }
            Err(Flow::Error(e)) => Err(e),
        }
    }

    /// Evaluates `id`, growing the native stack on the heap (`stacker`) as
    /// the source nests: the depth limit is a language rule, not a function
    /// of the host thread's stack size.
    fn eval(&mut self, frame: &mut Frame, id: ExprId) -> Eval {
        stacker::maybe_grow(256 * 1024, 8 * 1024 * 1024, || self.eval_inner(frame, id))
    }

    /// Evaluates `id` to a word (an integer or boolean wire).
    fn word(&mut self, frame: &mut Frame, id: ExprId) -> Result<Wire, Flow> {
        match self.eval(frame, id)? {
            Value::Word(w) => Ok(w),
            Value::Unit => Err(self.error(frame, id, "this expression has no value")),
            Value::Uninit => Err(self.error(frame, id, "this local is read before it is assigned")),
        }
    }

    /// The circuit type of expression `id`.
    fn ty_of(&self, frame: &Frame, id: ExprId) -> Result<Ty, Flow> {
        self.circuit_ty(&frame.infer[id])
            .map_err(|why| self.error(frame, id, why))
    }

    #[allow(clippy::too_many_lines)]
    fn eval_inner(&mut self, frame: &mut Frame, id: ExprId) -> Eval {
        let body = Arc::clone(&frame.body);
        match &body[id] {
            Expr::Missing => Err(self.error(frame, id, "incomplete expression")),
            Expr::Literal(lit) => match lit {
                Literal::Int(i) => {
                    let ty = self.ty_of(frame, id)?;
                    #[allow(clippy::cast_possible_truncation)]
                    let bits = i.value as u64;
                    if i.value > u128::from(ty.mask()) {
                        return Err(self.error(
                            frame,
                            id,
                            format!("literal {} does not fit `{ty}`", i.value),
                        ));
                    }
                    Ok(Value::Word(self.builder.constant(ty, bits)))
                }
                Literal::Bool(b) => Ok(Value::Word(self.builder.bool(*b))),
                Literal::Float(_) => Err(self.error(
                    frame,
                    id,
                    "floating-point values are not supported in circuits",
                )),
                Literal::String(_) | Literal::Nil => {
                    Err(self.error(frame, id, "strings and `nil` are not supported in circuits"))
                }
            },
            Expr::Path(path) => {
                let found = path.as_ident().and_then(|n| frame.lookup(n));
                found.ok_or_else(|| {
                    self.error(
                        frame,
                        id,
                        "only parameters and locals can be read in a circuit (globals and \
                         function values are not supported)",
                    )
                })
            }
            Expr::Block { statements, tail } => {
                frame.scopes.push(HashMap::new());
                let result = self.eval_block(frame, statements, *tail);
                frame.scopes.pop();
                result
            }
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.word(frame, *condition)?;
                match self.builder.as_const(cond) {
                    Some(1) => self.eval(frame, *then_branch),
                    Some(_) => match else_branch {
                        Some(e) => self.eval(frame, *e),
                        None => Ok(Value::Unit),
                    },
                    None => self.if_convert(frame, id, cond, *then_branch, *else_branch),
                }
            }
            Expr::Loop { body: loop_body } => loop {
                self.burn(frame, id)?;
                match self.eval(frame, *loop_body) {
                    Ok(_) | Err(Flow::Continue) => {}
                    Err(Flow::Break(v)) => return Ok(v),
                    Err(other) => return Err(other),
                }
            },
            Expr::While {
                condition,
                body: loop_body,
            } => {
                loop {
                    self.burn(frame, id)?;
                    let cond = self.word(frame, *condition)?;
                    match self.builder.as_const(cond) {
                        Some(1) => {}
                        Some(_) => return Ok(Value::Unit),
                        None => return Err(self.error(
                            frame,
                            *condition,
                            "this loop condition depends on a run-time value; a circuit unrolls \
                             its loops, so their bounds must be known at compile time",
                        )),
                    }
                    match self.eval(frame, *loop_body) {
                        Ok(_) | Err(Flow::Continue) => {}
                        Err(Flow::Break(_)) => return Ok(Value::Unit),
                        Err(other) => return Err(other),
                    }
                }
            }
            Expr::For {
                pat,
                iterable,
                body: loop_body,
            } => self.eval_for(frame, id, *pat, *iterable, *loop_body),
            Expr::Break { expr } => {
                let v = match expr {
                    Some(e) => self.eval(frame, *e)?,
                    None => Value::Unit,
                };
                Err(Flow::Break(v))
            }
            Expr::Continue => Err(Flow::Continue),
            Expr::Return { expr } => {
                let v = match expr {
                    Some(e) => self.eval(frame, *e)?,
                    None => Value::Unit,
                };
                Err(Flow::Return(v))
            }
            Expr::UnaryOp { expr, op } => {
                let a = self.word(frame, *expr)?;
                let ty = self.builder.ty(a);
                let op = match op {
                    UnaryOp::Neg => Op::Neg(a),
                    UnaryOp::Not | UnaryOp::BitNot => Op::Not(a),
                };
                Ok(Value::Word(self.builder.op(op, ty)))
            }
            Expr::BinaryOp { lhs, rhs, op } => {
                let Some(op) = op else {
                    return Err(self.error(frame, id, "incomplete binary expression"));
                };
                self.eval_binary(frame, id, *lhs, *rhs, *op)
            }
            Expr::Cast { expr, .. } => {
                let a = self.word(frame, *expr)?;
                let target = self.ty_of(frame, id)?;
                self.cast(frame, id, a, target).map(Value::Word)
            }
            Expr::Call { callee, args } => self.eval_call(frame, id, *callee, args),
            Expr::Range { .. } => Err(self.error(
                frame,
                id,
                "a range is only meaningful as a `for` loop's iterable",
            )),
            Expr::MethodCall { .. } => Err(self.error(
                frame,
                id,
                "method calls are not supported in circuits yet; call a function instead",
            )),
            Expr::Index { .. } | Expr::Array(_) => Err(self.error(
                frame,
                id,
                "arrays are heap objects and are not supported in circuits",
            )),
            Expr::Tuple(_) | Expr::RecordLit { .. } | Expr::Field { .. } => Err(self.error(
                frame,
                id,
                "tuples, structs and objects are not supported in circuits (integers and \
                 booleans are)",
            )),
            Expr::Scope { .. }
            | Expr::Group { .. }
            | Expr::Spawn { .. }
            | Expr::ChannelSend { .. }
            | Expr::ChannelReceive { .. } => Err(self.error(
                frame,
                id,
                "`scope`, `group`, `spawn` and channels are not supported in circuits",
            )),
            Expr::Perform { .. } | Expr::Handle { .. } | Expr::Resume { .. } => Err(self.error(
                frame,
                id,
                "algebraic effects (`perform`, `handle`, `resume`) are not supported in \
                 circuits: a circuit has no handler to call",
            )),
            #[allow(unreachable_patterns)]
            _ => Err(self.error(
                frame,
                id,
                "this kind of expression is not supported in circuits",
            )),
        }
    }

    fn eval_block(
        &mut self,
        frame: &mut Frame,
        statements: &[Statement],
        tail: Option<ExprId>,
    ) -> Eval {
        for stmt in statements {
            match stmt {
                Statement::Let {
                    pat, initializer, ..
                } => {
                    let value = match initializer {
                        Some(init) => self.eval(frame, *init)?,
                        None => Value::Uninit,
                    };
                    self.bind(
                        frame,
                        *pat,
                        value,
                        initializer.unwrap_or(frame.body.body_expr()),
                    )?;
                }
                Statement::Expr(e) => {
                    self.eval(frame, *e)?;
                }
            }
        }
        match tail {
            Some(t) => self.eval(frame, t),
            None => Ok(Value::Unit),
        }
    }

    /// Binds `pat` to `value` in the innermost scope.
    fn bind(
        &mut self,
        frame: &mut Frame,
        pat: PatId,
        value: Value,
        at: ExprId,
    ) -> Result<(), Flow> {
        let body = Arc::clone(&frame.body);
        match &body[pat] {
            Pat::Bind { name } => {
                frame
                    .scopes
                    .last_mut()
                    .expect("a block pushed a scope")
                    .insert(name.clone(), value);
                Ok(())
            }
            Pat::Wild => Ok(()),
            _ => Err(self.error(
                frame,
                at,
                "only plain `let name = ...` bindings are supported in circuits",
            )),
        }
    }

    /// `if` on a run-time condition: both branches are elaborated from the
    /// same state, and every local either assigns is merged with a
    /// multiplexer on the condition.
    fn if_convert(
        &mut self,
        frame: &mut Frame,
        id: ExprId,
        cond: Wire,
        then_branch: ExprId,
        else_branch: Option<ExprId>,
    ) -> Eval {
        if self.mode == Mode::Attest {
            return Err(self.error(
                frame,
                id,
                "this `if` branches on a run-time (secret) value; an oblivious function chooses \
                 with `ct_select(c, a, b)` instead",
            ));
        }
        let before = frame.scopes.clone();
        let escaped = |this: &Self, frame: &Frame| {
            this.error(
                frame,
                id,
                "`return`, `break` or `continue` under a condition known only at run time; a \
                 circuit has no control flow, so restructure it to compute a value",
            )
        };
        let then_value = match self.eval(frame, then_branch) {
            Ok(v) => v,
            Err(Flow::Error(e)) => return Err(Flow::Error(e)),
            Err(_) => return Err(escaped(self, frame)),
        };
        let after_then = std::mem::replace(&mut frame.scopes, before);
        let else_value = match else_branch {
            Some(e) => match self.eval(frame, e) {
                Ok(v) => v,
                Err(Flow::Error(e)) => return Err(Flow::Error(e)),
                Err(_) => return Err(escaped(self, frame)),
            },
            None => Value::Unit,
        };
        // Merge every local that differs between the two branches.
        for (level, then_scope) in after_then.into_iter().enumerate() {
            for (name, then_v) in then_scope {
                let Some(else_v) = frame.scopes[level].get(&name).copied() else {
                    continue;
                };
                if then_v == else_v {
                    continue;
                }
                let merged = self.merge(frame, id, cond, then_v, else_v)?;
                frame.scopes[level].insert(name, merged);
            }
        }
        if else_branch.is_none() {
            return Ok(Value::Unit);
        }
        self.merge(frame, id, cond, then_value, else_value)
    }

    fn merge(&mut self, frame: &Frame, id: ExprId, cond: Wire, a: Value, b: Value) -> Eval {
        match (a, b) {
            (Value::Word(a), Value::Word(b)) => {
                let ty = self.builder.ty(a);
                Ok(Value::Word(self.builder.op(Op::Mux(cond, a, b), ty)))
            }
            (Value::Unit, Value::Unit) => Ok(Value::Unit),
            _ => Err(self.error(
                frame,
                id,
                "a local is assigned in only one branch of an `if` on a run-time condition \
                 and was not assigned before it; give it a value first",
            )),
        }
    }

    fn eval_for(
        &mut self,
        frame: &mut Frame,
        id: ExprId,
        pat: PatId,
        iterable: ExprId,
        loop_body: ExprId,
    ) -> Eval {
        let body = Arc::clone(&frame.body);
        let Expr::Range {
            start,
            end,
            inclusive,
        } = &body[iterable]
        else {
            return Err(self.error(
                frame,
                iterable,
                "circuits support `for` over ranges only (arrays are not supported)",
            ));
        };
        let start_w = self.word(frame, *start)?;
        let end_w = self.word(frame, *end)?;
        let (Some(first), Some(last)) =
            (self.builder.as_const(start_w), self.builder.as_const(end_w))
        else {
            return Err(self.error(
                frame,
                iterable,
                "this range's bounds depend on a run-time value; a circuit unrolls its loops, \
                 so their bounds must be known at compile time",
            ));
        };
        let ty = self.builder.ty(start_w);
        let (first, last) = (ty.to_i128(first), ty.to_i128(last));
        let mut i = first;
        loop {
            if (*inclusive && i > last) || (!*inclusive && i >= last) {
                return Ok(Value::Unit);
            }
            self.burn(frame, id)?;
            frame.scopes.push(HashMap::new());
            let index = self.builder.constant(ty, ty.from_i128(i));
            let bound = self.bind(frame, pat, Value::Word(index), iterable);
            let result = bound.and_then(|()| self.eval(frame, loop_body));
            frame.scopes.pop();
            match result {
                Ok(_) | Err(Flow::Continue) => {}
                Err(Flow::Break(_)) => return Ok(Value::Unit),
                Err(other) => return Err(other),
            }
            // `a..=MAX` stops at MAX instead of wrapping.
            if *inclusive && i == last {
                return Ok(Value::Unit);
            }
            i += 1;
        }
    }

    fn eval_binary(
        &mut self,
        frame: &mut Frame,
        id: ExprId,
        lhs: ExprId,
        rhs: ExprId,
        op: BinaryOp,
    ) -> Eval {
        match op {
            BinaryOp::Assignment { op: compound } => {
                let body = Arc::clone(&frame.body);
                let Some(name) = (match &body[lhs] {
                    Expr::Path(p) => p.as_ident().cloned(),
                    _ => None,
                }) else {
                    return Err(self.error(
                        frame,
                        lhs,
                        "only locals can be assigned in a circuit (no fields or array elements)",
                    ));
                };
                let new = match compound {
                    None => self.eval(frame, rhs)?,
                    Some(arith) => {
                        let old = match frame.lookup(&name) {
                            Some(Value::Word(w)) => w,
                            _ => {
                                return Err(self.error(
                                    frame,
                                    lhs,
                                    "this local has no value to update",
                                ))
                            }
                        };
                        let b = self.word(frame, rhs)?;
                        Value::Word(self.arith(frame, id, arith, old, b)?)
                    }
                };
                if !frame.assign(&name, new) {
                    return Err(self.error(frame, lhs, "assignment to an unknown local"));
                }
                Ok(Value::Unit)
            }
            BinaryOp::LogicOp(logic) => {
                let a = self.word(frame, lhs)?;
                match (logic, self.builder.as_const(a)) {
                    // Short-circuit, as at run time.
                    (LogicOp::And, Some(0)) => Ok(Value::Word(self.builder.bool(false))),
                    (LogicOp::Or, Some(1)) => Ok(Value::Word(self.builder.bool(true))),
                    (_, Some(_)) => self.eval(frame, rhs),
                    (_, None) if self.mode == Mode::Attest => Err(self.error(
                        frame,
                        lhs,
                        "`&&`/`||` on a run-time (secret) value short-circuits, which is a branch; \
                         use `&`/`|` on booleans",
                    )),
                    // The right operand has no side effects in a circuit, so
                    // evaluating it unconditionally computes the same value.
                    (_, None) => {
                        let b = self.word(frame, rhs)?;
                        let op = match logic {
                            LogicOp::And => Op::And(a, b),
                            LogicOp::Or => Op::Or(a, b),
                        };
                        Ok(Value::Word(self.builder.op(op, Ty::Bool)))
                    }
                }
            }
            BinaryOp::ArithOp(arith) => {
                let a = self.word(frame, lhs)?;
                let b = self.word(frame, rhs)?;
                self.arith(frame, id, arith, a, b).map(Value::Word)
            }
            BinaryOp::CmpOp(cmp) => {
                let a = self.word(frame, lhs)?;
                let b = self.word(frame, rhs)?;
                let ty = self.builder.ty(a);
                let lt = |this: &mut Self, x: Wire, y: Wire| {
                    let op = if ty.is_signed() {
                        Op::Slt(x, y)
                    } else {
                        Op::Ult(x, y)
                    };
                    this.builder.op(op, Ty::Bool)
                };
                let (wire, negate) = match cmp {
                    CmpOp::Eq { negated } => (self.builder.op(Op::Eq(a, b), Ty::Bool), negated),
                    CmpOp::Ord {
                        ordering: Ordering::Less,
                        strict: true,
                    } => (lt(self, a, b), false),
                    CmpOp::Ord {
                        ordering: Ordering::Less,
                        strict: false,
                    } => (lt(self, b, a), true),
                    CmpOp::Ord {
                        ordering: Ordering::Greater,
                        strict: true,
                    } => (lt(self, b, a), false),
                    CmpOp::Ord {
                        ordering: Ordering::Greater,
                        strict: false,
                    } => (lt(self, a, b), true),
                };
                let wire = if negate {
                    self.builder.op(Op::Not(wire), Ty::Bool)
                } else {
                    wire
                };
                Ok(Value::Word(wire))
            }
        }
    }

    fn arith(
        &mut self,
        frame: &Frame,
        id: ExprId,
        op: ArithOp,
        a: Wire,
        b: Wire,
    ) -> Result<Wire, Flow> {
        let ty = self.builder.ty(a);
        let op = match op {
            ArithOp::Add => Op::Add(a, b),
            ArithOp::Subtract => Op::Sub(a, b),
            ArithOp::Multiply => Op::Mul(a, b),
            ArithOp::BitAnd => Op::And(a, b),
            ArithOp::BitOr => Op::Or(a, b),
            ArithOp::BitXor => Op::Xor(a, b),
            ArithOp::LeftShift => Op::Shl(a, b),
            ArithOp::RightShift if ty.is_signed() => Op::AShr(a, b),
            ArithOp::RightShift => Op::LShr(a, b),
            ArithOp::Divide | ArithOp::Remainder => {
                return self.divide(frame, id, op == ArithOp::Divide, a, b);
            }
        };
        Ok(self.builder.op(op, ty))
    }

    /// `/` and `%` on compile-time values, with native semantics (truncating
    /// toward zero); anything the machine would trap on is an error.
    fn divide(
        &mut self,
        frame: &Frame,
        id: ExprId,
        quotient: bool,
        a: Wire,
        b: Wire,
    ) -> Result<Wire, Flow> {
        let ty = self.builder.ty(a);
        let (Some(x), Some(y)) = (self.builder.as_const(a), self.builder.as_const(b)) else {
            return Err(self.error(
                frame,
                id,
                "division and remainder are supported only on compile-time values: a division \
                 circuit is not implemented yet, and an oblivious function may not divide a \
                 secret",
            ));
        };
        let (x, y) = (ty.to_i128(x), ty.to_i128(y));
        if y == 0 {
            return Err(self.error(frame, id, "division by zero"));
        }
        // `MIN / -1` (and `MIN % -1`) overflow: the machine traps.
        if ty.is_signed() && x == -(1i128 << (ty.width() - 1)) && y == -1 {
            return Err(self.error(frame, id, format!("this division overflows `{ty}`")));
        }
        let r = if quotient { x / y } else { x % y };
        Ok(self.builder.constant(ty, ty.from_i128(r)))
    }

    fn cast(&mut self, frame: &Frame, id: ExprId, a: Wire, target: Ty) -> Result<Wire, Flow> {
        let source = self.builder.ty(a);
        let op = match (source, target) {
            (s, t) if s == t => return Ok(a),
            (Ty::Bool, Ty::Int { .. }) => Op::Zext(a),
            (Ty::Int { width: sw, signed }, Ty::Int { width: tw, .. }) => {
                if tw < sw {
                    Op::Trunc(a)
                } else if tw == sw || !signed {
                    Op::Zext(a)
                } else {
                    Op::Sext(a)
                }
            }
            _ => {
                return Err(self.error(
                    frame,
                    id,
                    format!("a cast from `{source}` to `{target}` is not supported in circuits"),
                ))
            }
        };
        Ok(self.builder.op(op, target))
    }

    fn eval_call(
        &mut self,
        frame: &mut Frame,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
    ) -> Eval {
        if let Some(builtin) = frame.infer.builtin_call(id) {
            return match builtin {
                BuiltinFn::CtSelect | BuiltinFn::Select if args.len() == 3 => {
                    let c = self.word(frame, args[0])?;
                    let a = self.word(frame, args[1])?;
                    let b = self.word(frame, args[2])?;
                    let ty = self.builder.ty(a);
                    Ok(Value::Word(self.builder.op(Op::Mux(c, a, b), ty)))
                }
                // In a circuit the revealed value is the output, which the
                // prover's caller chooses; inside, `declassify` changes
                // nothing.
                BuiltinFn::Declassify if args.len() == 1 => self.eval(frame, args[0]),
                BuiltinFn::Print | BuiltinFn::Println => Err(self.error(
                    frame,
                    id,
                    "`print` and `println` have effects, which a circuit cannot perform",
                )),
                _ => Err(self.error(frame, id, "this builtin is not supported in circuits")),
            };
        }
        let Some(CallableDef::Function(func)) = frame.infer[callee].as_callable_def() else {
            return Err(self.error(
                frame,
                callee,
                "only direct calls to named functions are supported in circuits",
            ));
        };
        if func.is_extern(self.db) {
            return Err(self.error(
                frame,
                id,
                format!(
                    "`{}` is an external function; a circuit can only contain Codira code",
                    func.name(self.db)
                ),
            ));
        }
        let mut values = Vec::with_capacity(args.len());
        for &arg in args {
            values.push(self.eval(frame, arg)?);
        }
        let runtime_arg = values
            .iter()
            .any(|v| matches!(v, Value::Word(w) if self.builder.as_const(*w).is_none()));
        if self.mode == Mode::Attest && runtime_arg && !is_constant_time(self.db, func) {
            return Err(self.error(
                frame,
                id,
                format!(
                    "a secret is passed to `{}`, which is not `oblivious` or `@constant_time`",
                    func.name(self.db)
                ),
            ));
        }
        self.burn(frame, id)?;
        self.call(func, &values, Some((frame, id)))
            .map_err(Flow::Error)
    }
}

#[cfg(test)]
mod tests;
