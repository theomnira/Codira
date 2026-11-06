//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Compile-time elaboration of `@target(qpu)` Codira functions into a
//!   [`Circuit`].
//!
//! # The surface
//!
//! A quantum kernel is an ordinary Codira function carrying the
//! `@target(qpu)` attribute (an ordinary attribute under the existing
//! `@name(args)` grammar, spec section 14 -- no new syntax). Quantum
//! operations are declared once per program in an
//! `extern "codira-quantum" { .. }` block; [`PRELUDE`] is the full block.
//!
//! Why an `extern` block rather than a std module: the HIR already lowers
//! `extern "..." { .. }` blocks into ordinary, name-resolvable, type-checked
//! function items (that is how `extern "codira-intrinsic"` works), so the
//! gates type-check like any other call -- `h(0.5)` is a type error before
//! elaboration ever runs -- and nothing in `codira_hir` needed to change to
//! recognise them: the block's ABI string is read off the syntax tree here.
//! A std module would need cross-package imports of a module that has no
//! codegen-able bodies, which is more machinery for the same declarations.
//!
//! # What elaboration evaluates
//!
//! The kernel's body is *interpreted at compile time* over HIR
//! (`codira_hir::Body`, reading `Expr`/`Statement`/`Pat` and the inference
//! result exactly as `codira_hir::mir_lower` does). Every value is known:
//! integers, floats and booleans, `let` and assignment, unary and binary
//! operators (checked: overflow and division by zero are errors), `as`
//! casts, `if`/`else` on compile-time conditions, `loop`/`while` with
//! `break`/`return` (unrolled, under a fuel limit), and calls -- to other
//! kernels (which may emit gates) and to ordinary Codira functions (which
//! may compute, but not emit gates). Each call of a quantum intrinsic
//! appends one operation to the circuit.
//!
//! What is *not* known at compile time is a measurement result: `measure`
//! returns a value that may be discarded or returned, but branching on it,
//! computing with it or passing it on is rejected, because the QIR base
//! profile has no measurement feedback. Kernel parameters must be supplied
//! as compile-time values (the CLI's `--param name=value`).
//!
//! The direction of dependencies: this module reads `codira_hir`;
//! `codira_hir` knows nothing of quantum.

use std::{collections::HashMap, fmt, sync::Arc};

use codira_hir::{
    ArithOp, BinaryOp, Body, CallableDef, CmpOp, Expr, ExprId, Function, HasSource, HirDatabase,
    InferenceResult, Literal, LogicOp, Name, Ordering, Pat, Statement, TyKind, UnaryOp,
};
use codira_syntax::{
    ast::{self, AttributeOwner},
    AstNode, SyntaxKind,
};

use crate::circuit::{Circuit, Gate, Op, Register};

/// The ABI string that marks an `extern` block as declaring quantum
/// operations.
pub const QUANTUM_ABI: &str = "codira-quantum";

/// The complete quantum intrinsic block. Paste it into a program (or let
/// `codira quantum import` write it) to use the gates by name.
pub const PRELUDE: &str = r#"extern "codira-quantum" {
    // Registers grow to the highest index used; these force a minimum size.
    func alloc_qubits(n: i64);
    func alloc_bits(n: i64);
    func pi() -> f64;

    func id(q: i64);
    func h(q: i64);
    func x(q: i64);
    func y(q: i64);
    func z(q: i64);
    func s(q: i64);
    func sdg(q: i64);
    func t(q: i64);
    func tdg(q: i64);
    func sx(q: i64);
    func rx(theta: f64, q: i64);
    func ry(theta: f64, q: i64);
    func rz(theta: f64, q: i64);
    func p(lambda: f64, q: i64);
    func u(theta: f64, phi: f64, lambda: f64, q: i64);

    func cx(control: i64, target: i64);
    func cy(control: i64, target: i64);
    func cz(control: i64, target: i64);
    func ch(control: i64, target: i64);
    func swap(a: i64, b: i64);
    func cp(lambda: f64, control: i64, target: i64);
    func crx(theta: f64, control: i64, target: i64);
    func cry(theta: f64, control: i64, target: i64);
    func crz(theta: f64, control: i64, target: i64);
    func ccx(control1: i64, control2: i64, target: i64);
    func cswap(control: i64, a: i64, b: i64);

    // Measures qubit `q` into classical bit `c`. The result can be returned
    // but not branched on: the base profile has no measurement feedback.
    func measure(q: i64, c: i64) -> bool;
    func reset(q: i64);
    func barrier();
}
"#;

/// Limits that keep elaboration finite and deterministic (no wall clock).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElabOptions {
    /// Loop iterations plus function calls, across the whole elaboration.
    pub fuel: u64,
    /// The deepest call nesting.
    pub max_depth: usize,
    /// The most operations the circuit may contain.
    pub max_ops: usize,
    /// The highest qubit or classical-bit index accepted.
    pub max_index: u32,
}

impl Default for ElabOptions {
    fn default() -> Self {
        ElabOptions {
            fuel: 10_000_000,
            max_depth: 256,
            max_ops: 10_000_000,
            max_index: 1 << 20,
        }
    }
}

/// A compile-time value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Unit,
    /// The result of `measure(qubit, clbit)`: known only at run time.
    Measured {
        qubit: u32,
        clbit: u32,
    },
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(v) => write!(f, "{v}"),
            Value::Float(v) => write!(f, "{v}"),
            Value::Bool(v) => write!(f, "{v}"),
            Value::Unit => write!(f, "()"),
            Value::Measured { qubit, clbit } => {
                write!(f, "<measurement of qubit {qubit} into bit {clbit}>")
            }
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

/// Whether `func` carries `@target(qpu)`.
pub fn is_kernel(db: &dyn HirDatabase, func: Function) -> bool {
    let source = func.source(db);
    let Some(attrs) = source.value.attribute_list() else {
        return false;
    };
    attrs.attributes().any(|attr| {
        let named_target = attr.path().and_then(|p| p.segment()).is_some_and(
            |s| matches!(s.kind(), Some(ast::PathSegmentKind::Name(n)) if n.text() == "target"),
        );
        named_target
            && attr.arg_list().is_some_and(|args| {
                args.args()
                    .next()
                    .is_some_and(|a| a.syntax().text().to_string().trim() == "qpu")
            })
    })
}

/// Whether `func` is declared in an `extern "codira-quantum"` block.
pub fn is_quantum_intrinsic(db: &dyn HirDatabase, func: Function) -> bool {
    if !func.is_extern(db) {
        return false;
    }
    let source = func.source(db);
    source
        .value
        .syntax()
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::EXTERN_BLOCK)
        .and_then(|block| {
            block
                .children_with_tokens()
                .filter_map(codira_syntax::SyntaxElement::into_token)
                .find(|t| t.kind() == SyntaxKind::STRING)
        })
        .is_some_and(|abi| abi.text().trim_matches('"') == QUANTUM_ABI)
}

/// Every `@target(qpu)` function in the database, in declaration order.
pub fn find_kernels(db: &dyn HirDatabase) -> Vec<Function> {
    let mut kernels = Vec::new();
    for package in codira_hir::Package::all(db) {
        for module in package.modules(db) {
            for func in module.all_functions(db) {
                if is_kernel(db, func) {
                    kernels.push(func);
                }
            }
        }
    }
    kernels
}

/// Elaborates kernel `kernel` with its parameters bound to `args`, given as
/// `(name, text)` pairs and parsed according to each parameter's type.
pub fn elaborate(
    db: &dyn HirDatabase,
    kernel: Function,
    args: &[(String, String)],
    options: &ElabOptions,
) -> Result<Circuit, ElabError> {
    let name = kernel.name(db).to_string();
    if !is_kernel(db, kernel) {
        return Err(plain(format!(
            "`{name}` is not a quantum kernel; mark it `@target(qpu)`"
        )));
    }

    // Bind the kernel's parameters from the textual arguments.
    let body = kernel.body(db);
    let params = kernel.params(db);
    let mut bound: Vec<Value> = Vec::with_capacity(params.len());
    let mut used = vec![false; args.len()];
    for (param, (pat, _)) in params.iter().zip(body.params()) {
        let Pat::Bind { name: pname } = &body[*pat] else {
            return Err(plain(format!(
                "kernel `{name}` has a parameter that is not a plain name"
            )));
        };
        let pname = pname.to_string();
        let ty = param.ty();
        let Some(index) = args.iter().position(|(n, _)| *n == pname) else {
            return Err(plain(format!(
                "kernel `{name}` needs a compile-time value for parameter `{pname}`: pass \
                 --param {pname}=<value>"
            )));
        };
        used[index] = true;
        let text = args[index].1.trim();
        let value = match ty.interned() {
            TyKind::Int(_) => text.parse::<i64>().ok().map(Value::Int),
            TyKind::Float(_) => text.parse::<f64>().ok().map(Value::Float),
            TyKind::Bool => text.parse::<bool>().ok().map(Value::Bool),
            _ => {
                return Err(plain(format!(
                    "kernel parameter `{pname}` must be an integer, float or bool to be bound \
                     at compile time"
                )))
            }
        }
        .ok_or_else(|| {
            plain(format!(
                "`{text}` is not a valid value for kernel parameter `{pname}`"
            ))
        })?;
        bound.push(value);
    }
    if let Some(extra) = used.iter().position(|u| !u) {
        return Err(plain(format!(
            "kernel `{name}` has no parameter `{}`",
            args[extra].0
        )));
    }

    let mut elab = Elaborator {
        db,
        options,
        ops: Vec::new(),
        min_qubits: 0,
        min_clbits: 0,
        fuel: options.fuel,
        depth: 0,
    };
    elab.call(kernel, &bound, None)?;

    let num_qubits = elab.ops.iter().fold(elab.min_qubits, |acc, op| {
        acc.max(match op {
            Op::Gate { qubits, .. } | Op::Barrier { qubits } => {
                qubits.iter().map(|q| q + 1).max().unwrap_or(0)
            }
            Op::Measure { qubit, .. } | Op::Reset { qubit } => qubit + 1,
        })
    });
    let num_clbits = elab.ops.iter().fold(elab.min_clbits, |acc, op| match op {
        Op::Measure { clbit, .. } => acc.max(clbit + 1),
        _ => acc,
    });
    let circuit = Circuit {
        name,
        qregs: if num_qubits > 0 {
            vec![Register::new("q", num_qubits)]
        } else {
            vec![]
        },
        cregs: if num_clbits > 0 {
            vec![Register::new("c", num_clbits)]
        } else {
            vec![]
        },
        ops: elab.ops,
    };
    circuit
        .validate()
        .map_err(|e| plain(format!("the elaborated circuit is invalid: {e}")))?;
    Ok(circuit)
}

fn plain(message: String) -> ElabError {
    ElabError {
        message,
        location: None,
    }
}

/// Non-local control flow, and errors, while interpreting.
enum Flow {
    Break(Value),
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
    ops: Vec<Op>,
    min_qubits: u32,
    min_clbits: u32,
    fuel: u64,
    depth: usize,
}

/// One function activation.
struct Frame {
    func: Function,
    body: Arc<Body>,
    infer: Arc<InferenceResult>,
    is_kernel: bool,
    scopes: Vec<HashMap<Name, Value>>,
}

impl Elaborator<'_> {
    fn burn(&mut self, frame: &Frame, at: ExprId) -> Result<(), Flow> {
        if self.fuel == 0 {
            return Err(self.error(
                frame,
                at,
                format!(
                    "elaboration ran out of fuel ({} loop iterations and calls); a kernel's \
                     loops must have compile-time bounds",
                    self.options.fuel
                ),
            ));
        }
        self.fuel -= 1;
        Ok(())
    }

    fn error(&self, frame: &Frame, at: ExprId, message: String) -> Flow {
        Flow::Error(ElabError {
            message,
            location: self.locate(frame.func, at),
        })
    }

    fn locate(&self, func: Function, at: ExprId) -> Option<SourceLocation> {
        let range = func.expr_text_range(self.db, at)?;
        let file_id = func.file_id(self.db);
        let text = self.db.file_text(file_id);
        let offset = usize::from(range.start()).min(text.len());
        let before = &text[..offset];
        let line = before.matches('\n').count() as u32 + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) as u32 + 1;
        Some(SourceLocation {
            file: self.db.file_relative_path(file_id).to_string(),
            line,
            column,
            function: func.name(self.db).to_string(),
        })
    }

    /// Calls `func` with `args`. `site` is the call expression in the
    /// caller, for error locations.
    fn call(
        &mut self,
        func: Function,
        args: &[Value],
        site: Option<(&Frame, ExprId)>,
    ) -> Result<Value, ElabError> {
        let unwrap = |flow: Flow| match flow {
            Flow::Error(e) => e,
            Flow::Break(_) | Flow::Return(_) => unreachable!("handled by the caller"),
        };
        if self.depth >= self.options.max_depth {
            let message = format!(
                "calls nest deeper than {} (unbounded recursion?)",
                self.options.max_depth
            );
            return Err(match site {
                Some((frame, at)) => unwrap(self.error(frame, at, message)),
                None => plain(message),
            });
        }
        let body = func.body(self.db);
        if body.params().len() != args.len() {
            return Err(plain(format!(
                "`{}` takes {} arguments",
                func.name(self.db),
                body.params().len()
            )));
        }
        let mut scope = HashMap::new();
        for ((pat, _), value) in body.params().iter().zip(args) {
            match &body[*pat] {
                Pat::Bind { name } => {
                    scope.insert(name.clone(), *value);
                }
                Pat::Wild => {}
                _ => {
                    return Err(plain(format!(
                        "`{}` has a destructuring parameter; only plain names are supported in \
                         quantum elaboration",
                        func.name(self.db)
                    )))
                }
            }
        }
        let mut frame = Frame {
            func,
            infer: func.infer(self.db),
            is_kernel: is_kernel(self.db, func),
            scopes: vec![scope],
            body,
        };
        self.depth += 1;
        let root = frame.body.body_expr();
        let result = self.eval(&mut frame, root);
        self.depth -= 1;
        match result {
            Ok(v) | Err(Flow::Return(v)) => Ok(v),
            Err(Flow::Break(_)) => Err(plain("`break` outside of a loop".into())),
            Err(Flow::Error(e)) => Err(e),
        }
    }

    fn lookup(frame: &Frame, name: &Name) -> Option<Value> {
        frame
            .scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    /// Rejects a measurement result where a compile-time value is needed.
    fn known(&self, frame: &Frame, at: ExprId, v: Value, what: &str) -> Result<Value, Flow> {
        if let Value::Measured { qubit, clbit } = v {
            return Err(self.error(
                frame,
                at,
                format!(
                    "{what} depends on the measurement of qubit {qubit} (into bit {clbit}), which \
                     is only known at run time; the QIR base profile has no measurement feedback, \
                     so kernels cannot branch on, compute with, or pass on measurement results"
                ),
            ));
        }
        Ok(v)
    }

    /// Evaluates `id`. Every recursion of the interpreter passes through
    /// here, so this is where the native stack is grown (on the heap, by
    /// `stacker`) instead of overflowing: kernels nest calls and
    /// expressions as deeply as their source does, and the depth limit is a
    /// language rule, not a function of the host thread's stack size.
    fn eval(&mut self, frame: &mut Frame, id: ExprId) -> Eval {
        stacker::maybe_grow(256 * 1024, 8 * 1024 * 1024, || self.eval_inner(frame, id))
    }

    #[allow(clippy::too_many_lines)]
    fn eval_inner(&mut self, frame: &mut Frame, id: ExprId) -> Eval {
        let body = Arc::clone(&frame.body);
        match &body[id] {
            Expr::Missing => Err(self.error(frame, id, "incomplete expression".into())),
            Expr::Literal(lit) => match lit {
                Literal::Int(i) => i64::try_from(i.value)
                    .map(Value::Int)
                    .map_err(|_e| self.error(frame, id, "integer literal exceeds i64".into())),
                Literal::Float(f) => Ok(Value::Float(f.value)),
                Literal::Bool(b) => Ok(Value::Bool(*b)),
                Literal::String(_) | Literal::Nil => Err(self.error(
                    frame,
                    id,
                    "strings and `nil` have no meaning in a quantum kernel".into(),
                )),
            },
            Expr::Path(path) => {
                let found = path.as_ident().and_then(|n| Self::lookup(frame, n));
                found.ok_or_else(|| {
                    self.error(
                        frame,
                        id,
                        "this name is not a compile-time value in scope (only parameters and \
                         locals can be read during elaboration)"
                            .into(),
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
                let cond = self.eval(frame, *condition)?;
                let cond = self.known(frame, *condition, cond, "this `if` condition")?;
                match cond {
                    Value::Bool(true) => self.eval(frame, *then_branch),
                    Value::Bool(false) => match else_branch {
                        Some(e) => self.eval(frame, *e),
                        None => Ok(Value::Unit),
                    },
                    other => Err(self.error(
                        frame,
                        *condition,
                        format!("an `if` condition must be a bool, got {other}"),
                    )),
                }
            }
            Expr::Loop { body: loop_body } => loop {
                self.burn(frame, id)?;
                match self.eval(frame, *loop_body) {
                    Ok(_) => {}
                    Err(Flow::Break(v)) => return Ok(v),
                    Err(other) => return Err(other),
                }
            },
            Expr::While {
                condition,
                body: loop_body,
            } => loop {
                self.burn(frame, id)?;
                let cond = self.eval(frame, *condition)?;
                match self.known(frame, *condition, cond, "this `while` condition")? {
                    Value::Bool(true) => {}
                    Value::Bool(false) => return Ok(Value::Unit),
                    other => {
                        return Err(self.error(
                            frame,
                            *condition,
                            format!("a `while` condition must be a bool, got {other}"),
                        ))
                    }
                }
                match self.eval(frame, *loop_body) {
                    Ok(_) => {}
                    Err(Flow::Break(_)) => return Ok(Value::Unit),
                    Err(other) => return Err(other),
                }
            },
            Expr::Break { expr } => {
                let v = match expr {
                    Some(e) => self.eval(frame, *e)?,
                    None => Value::Unit,
                };
                Err(Flow::Break(v))
            }
            Expr::Return { expr } => {
                let v = match expr {
                    Some(e) => self.eval(frame, *e)?,
                    None => Value::Unit,
                };
                Err(Flow::Return(v))
            }
            Expr::UnaryOp { expr, op } => {
                let v = self.eval(frame, *expr)?;
                let v = self.known(frame, *expr, v, "this operand")?;
                match (op, v) {
                    (UnaryOp::Neg, Value::Int(i)) => i
                        .checked_neg()
                        .map(Value::Int)
                        .ok_or_else(|| self.error(frame, id, "negation overflows i64".into())),
                    (UnaryOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
                    (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                    (UnaryOp::Not | UnaryOp::BitNot, Value::Int(i)) => Ok(Value::Int(!i)),
                    (_, other) => Err(self.error(
                        frame,
                        id,
                        format!("operator {op:?} cannot be applied to {other}"),
                    )),
                }
            }
            Expr::BinaryOp { lhs, rhs, op } => {
                let Some(op) = op else {
                    return Err(self.error(frame, id, "incomplete binary expression".into()));
                };
                self.eval_binary(frame, id, *lhs, *rhs, *op)
            }
            Expr::Cast { expr, .. } => {
                let v = self.eval(frame, *expr)?;
                let v = self.known(frame, *expr, v, "this cast")?;
                let target = frame.infer[id].clone();
                match (target.interned(), v) {
                    (TyKind::Int(_), Value::Int(i)) => Ok(Value::Int(i)),
                    // Rust's `as`: saturating, NaN to 0 -- the same rule
                    // the language spec gives float-to-int casts.
                    #[allow(clippy::cast_possible_truncation)]
                    (TyKind::Int(_), Value::Float(f)) => Ok(Value::Int(f as i64)),
                    (TyKind::Int(_), Value::Bool(b)) => Ok(Value::Int(i64::from(b))),
                    #[allow(clippy::cast_precision_loss)]
                    (TyKind::Float(_), Value::Int(i)) => Ok(Value::Float(i as f64)),
                    (TyKind::Float(_), Value::Float(f)) => Ok(Value::Float(f)),
                    (TyKind::Bool, Value::Bool(b)) => Ok(Value::Bool(b)),
                    (_, other) => {
                        Err(self.error(frame, id, format!("cannot cast {other} at compile time")))
                    }
                }
            }
            Expr::Call { callee, args } => self.eval_call(frame, id, *callee, args),
            // `for`/`continue` (and ranges, which exist only as `for`
            // iterables) are not modelled by the elaborator yet.
            Expr::For { .. } | Expr::Continue | Expr::Range { .. } => Err(self.error(
                frame,
                id,
                "quantum elaboration does not support `for` loops or `continue` yet; use `while`"
                    .into(),
            )),
            Expr::MethodCall { .. }
            | Expr::Match { .. }
            | Expr::Unwrap { .. }
            | Expr::Propagate { .. }
            | Expr::Format { .. }
            | Expr::Index { .. }
            | Expr::Array(_)
            | Expr::Tuple(_)
            | Expr::RecordLit { .. }
            | Expr::Field { .. } => Err(self.error(
                frame,
                id,
                "quantum elaboration handles integers, floats, booleans, let/assignment, if, \
                 loop/while and function calls; methods, arrays, tuples and structs are not \
                 supported in kernels yet"
                    .into(),
            )),
            Expr::Scope { .. }
            | Expr::Spawn { .. }
            | Expr::ChannelSend { .. }
            | Expr::ChannelReceive { .. }
            | Expr::Select { .. }
            | Expr::Offer { .. } => Err(self.error(
                frame,
                id,
                "`scope`, `spawn`, channels, `select` and sessions are not supported in quantum \
                 kernels"
                    .into(),
            )),
            Expr::Closure { .. } => Err(self.error(
                frame,
                id,
                "closures are not supported in quantum kernels: a circuit has no function values"
                    .into(),
            )),
            Expr::Perform { .. } | Expr::Handle { .. } | Expr::Resume { .. } => Err(self.error(
                frame,
                id,
                "algebraic effects (`perform`, `handle`, `resume`) are not supported in quantum                  kernels: a circuit has no handler to call"
                    .into(),
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
                        None => Value::Unit,
                    };
                    let body = Arc::clone(&frame.body);
                    match &body[*pat] {
                        Pat::Bind { name } => {
                            frame
                                .scopes
                                .last_mut()
                                .expect("a block pushed a scope")
                                .insert(name.clone(), value);
                        }
                        Pat::Wild => {}
                        _ => {
                            let at = initializer.unwrap_or_else(|| frame.body.body_expr());
                            return Err(self.error(
                                frame,
                                at,
                                "only plain `let name = ...` bindings are supported in kernels"
                                    .into(),
                            ));
                        }
                    }
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
                        "only local variables can be assigned in kernels".into(),
                    ));
                };
                let rhs_v = self.eval(frame, rhs)?;
                let new = match compound {
                    None => rhs_v,
                    Some(arith) => {
                        let old = Self::lookup(frame, &name).ok_or_else(|| {
                            self.error(frame, lhs, "assignment to an unknown local".into())
                        })?;
                        self.arith(frame, id, lhs, rhs, arith, old, rhs_v)?
                    }
                };
                let Some(scope) = frame
                    .scopes
                    .iter_mut()
                    .rev()
                    .find(|s| s.contains_key(&name))
                else {
                    return Err(self.error(frame, lhs, "assignment to an unknown local".into()));
                };
                scope.insert(name, new);
                Ok(Value::Unit)
            }
            BinaryOp::LogicOp(logic) => {
                let a = self.eval(frame, lhs)?;
                let a = self.known(frame, lhs, a, "this condition")?;
                let Value::Bool(a) = a else {
                    return Err(self.error(frame, lhs, "`&&`/`||` need bools".into()));
                };
                // Short-circuit, as at run time.
                match (logic, a) {
                    (LogicOp::And, false) => return Ok(Value::Bool(false)),
                    (LogicOp::Or, true) => return Ok(Value::Bool(true)),
                    _ => {}
                }
                let b = self.eval(frame, rhs)?;
                match self.known(frame, rhs, b, "this condition")? {
                    Value::Bool(b) => Ok(Value::Bool(b)),
                    _ => Err(self.error(frame, rhs, "`&&`/`||` need bools".into())),
                }
            }
            BinaryOp::ArithOp(arith) => {
                let a = self.eval(frame, lhs)?;
                let b = self.eval(frame, rhs)?;
                self.arith(frame, id, lhs, rhs, arith, a, b)
            }
            BinaryOp::CmpOp(cmp) => {
                let a = self.eval(frame, lhs)?;
                let a = self.known(frame, lhs, a, "this comparison")?;
                let b = self.eval(frame, rhs)?;
                let b = self.known(frame, rhs, b, "this comparison")?;
                let ordering = match (a, b) {
                    (Value::Int(x), Value::Int(y)) => Some(x.cmp(&y)),
                    (Value::Float(x), Value::Float(y)) => x.partial_cmp(&y),
                    (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(&y)),
                    _ => return Err(self.error(frame, id, format!("cannot compare {a} with {b}"))),
                };
                let result = match (cmp, ordering) {
                    (CmpOp::Eq { negated }, o) => (o == Some(std::cmp::Ordering::Equal)) != negated,
                    (_, None) => false,
                    (
                        CmpOp::Ord {
                            ordering: Ordering::Less,
                            strict,
                        },
                        Some(o),
                    ) => o.is_lt() || (!strict && o.is_eq()),
                    (
                        CmpOp::Ord {
                            ordering: Ordering::Greater,
                            strict,
                        },
                        Some(o),
                    ) => o.is_gt() || (!strict && o.is_eq()),
                };
                Ok(Value::Bool(result))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn arith(
        &self,
        frame: &Frame,
        id: ExprId,
        lhs: ExprId,
        rhs: ExprId,
        op: ArithOp,
        a: Value,
        b: Value,
    ) -> Eval {
        let a = self.known(frame, lhs, a, "this arithmetic")?;
        let b = self.known(frame, rhs, b, "this arithmetic")?;
        let overflow = || self.error(frame, id, format!("{op:?} overflows or divides by zero"));
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                let shift = || u32::try_from(y).ok();
                let r = match op {
                    ArithOp::Add => x.checked_add(y),
                    ArithOp::Subtract => x.checked_sub(y),
                    ArithOp::Multiply => x.checked_mul(y),
                    ArithOp::Divide => x.checked_div(y),
                    ArithOp::Remainder => x.checked_rem(y),
                    ArithOp::LeftShift => shift().and_then(|s| x.checked_shl(s)),
                    ArithOp::RightShift => shift().and_then(|s| x.checked_shr(s)),
                    ArithOp::BitAnd => Some(x & y),
                    ArithOp::BitOr => Some(x | y),
                    ArithOp::BitXor => Some(x ^ y),
                };
                r.map(Value::Int).ok_or_else(overflow)
            }
            (Value::Float(x), Value::Float(y)) => match op {
                ArithOp::Add => Ok(Value::Float(x + y)),
                ArithOp::Subtract => Ok(Value::Float(x - y)),
                ArithOp::Multiply => Ok(Value::Float(x * y)),
                ArithOp::Divide => Ok(Value::Float(x / y)),
                ArithOp::Remainder => Ok(Value::Float(x % y)),
                _ => Err(self.error(frame, id, format!("{op:?} is not defined on floats"))),
            },
            (Value::Bool(x), Value::Bool(y)) => match op {
                ArithOp::BitAnd => Ok(Value::Bool(x & y)),
                ArithOp::BitOr => Ok(Value::Bool(x | y)),
                ArithOp::BitXor => Ok(Value::Bool(x ^ y)),
                _ => Err(self.error(frame, id, format!("{op:?} is not defined on bools"))),
            },
            _ => Err(self.error(frame, id, format!("{op:?} cannot combine {a} and {b}"))),
        }
    }

    fn eval_call(
        &mut self,
        frame: &mut Frame,
        id: ExprId,
        callee: ExprId,
        args: &[ExprId],
    ) -> Eval {
        let Some(CallableDef::Function(func)) = frame.infer[callee].as_callable_def() else {
            return Err(self.error(
                frame,
                callee,
                "only direct calls to named functions are supported in kernels".into(),
            ));
        };
        let mut values = Vec::with_capacity(args.len());
        for &arg in args {
            values.push(self.eval(frame, arg)?);
        }

        if is_quantum_intrinsic(self.db, func) {
            if !frame.is_kernel {
                return Err(self.error(
                    frame,
                    id,
                    format!(
                        "quantum operation `{}` used outside a `@target(qpu)` function",
                        func.name(self.db)
                    ),
                ));
            }
            return self.intrinsic(frame, id, &func.name(self.db).to_string(), &values);
        }
        if func.is_extern(self.db) {
            return Err(self.error(
                frame,
                id,
                format!(
                    "`{}` is an external function; its result is not known at compile time",
                    func.name(self.db)
                ),
            ));
        }
        for (&arg, v) in args.iter().zip(&values) {
            self.known(frame, arg, *v, "this argument")?;
        }
        self.burn(frame, id)?;
        self.call(func, &values, Some((frame, id)))
            .map_err(Flow::Error)
    }

    fn index(&self, frame: &Frame, at: ExprId, v: Value, what: &str) -> Result<u32, Flow> {
        match v {
            Value::Int(i) if i >= 0 && i <= i64::from(self.options.max_index) => Ok(i as u32),
            Value::Int(i) => Err(self.error(
                frame,
                at,
                format!(
                    "{what} index {i} is out of range (0..={})",
                    self.options.max_index
                ),
            )),
            other => Err(self.error(
                frame,
                at,
                format!("{what} index must be an integer, got {other}"),
            )),
        }
    }

    fn push(&mut self, frame: &Frame, at: ExprId, op: Op) -> Result<(), Flow> {
        if self.ops.len() >= self.options.max_ops {
            return Err(self.error(
                frame,
                at,
                format!("the circuit exceeds {} operations", self.options.max_ops),
            ));
        }
        self.ops.push(op);
        Ok(())
    }

    fn intrinsic(&mut self, frame: &Frame, id: ExprId, name: &str, args: &[Value]) -> Eval {
        for v in args {
            self.known(frame, id, *v, "this quantum operation's argument")?;
        }
        match name {
            "pi" => return Ok(Value::Float(std::f64::consts::PI)),
            "alloc_qubits" | "alloc_bits" => {
                let n = self.index(frame, id, args[0], "register size")?;
                if name == "alloc_qubits" {
                    self.min_qubits = self.min_qubits.max(n);
                } else {
                    self.min_clbits = self.min_clbits.max(n);
                }
                return Ok(Value::Unit);
            }
            "measure" => {
                let qubit = self.index(frame, id, args[0], "qubit")?;
                let clbit = self.index(frame, id, args[1], "classical bit")?;
                self.push(frame, id, Op::Measure { qubit, clbit })?;
                return Ok(Value::Measured { qubit, clbit });
            }
            "reset" => {
                let qubit = self.index(frame, id, args[0], "qubit")?;
                self.push(frame, id, Op::Reset { qubit })?;
                return Ok(Value::Unit);
            }
            "barrier" => {
                self.push(frame, id, Op::Barrier { qubits: vec![] })?;
                return Ok(Value::Unit);
            }
            _ => {}
        }
        // A gate: angle arguments (floats) first, then qubits.
        let split = args
            .iter()
            .position(|v| !matches!(v, Value::Float(_)))
            .unwrap_or(args.len());
        let angles: Vec<f64> = args[..split]
            .iter()
            .map(|v| match v {
                Value::Float(f) => *f,
                _ => unreachable!("split at the first non-float"),
            })
            .collect();
        let Some(gate) = Gate::from_name(name, &angles) else {
            return Err(self.error(
                frame,
                id,
                format!(
                    "`{name}` is declared in an extern \"{QUANTUM_ABI}\" block but is not a \
                     quantum operation this compiler knows (see the prelude for the list)"
                ),
            ));
        };
        let mut qubits = Vec::with_capacity(args.len() - split);
        for v in &args[split..] {
            qubits.push(self.index(frame, id, *v, "qubit")?);
        }
        if qubits.len() != gate.num_qubits() {
            return Err(self.error(
                frame,
                id,
                format!(
                    "`{name}` acts on {} qubits, given {}",
                    gate.num_qubits(),
                    qubits.len()
                ),
            ));
        }
        if let Some(q) = qubits
            .iter()
            .enumerate()
            .find_map(|(i, q)| qubits[..i].contains(q).then_some(*q))
        {
            return Err(self.error(frame, id, format!("`{name}` is given qubit {q} twice")));
        }
        if angles.iter().any(|a| !a.is_finite()) {
            return Err(self.error(frame, id, format!("`{name}` has a non-finite angle")));
        }
        self.push(frame, id, Op::Gate { gate, qubits })?;
        Ok(Value::Unit)
    }
}

/// Writes `circuit` as a Codira source file: the [`PRELUDE`] and one
/// `@target(qpu)` function whose body replays the circuit. Register names
/// are recorded in a comment; the body uses flat indices.
pub fn circuit_to_codira(circuit: &Circuit, origin: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "// Generated by `codira quantum import {origin}`.");
    if !circuit.qregs.is_empty() || !circuit.cregs.is_empty() {
        out.push_str("// Flat indices used below, by original register:\n");
        let mut start = 0;
        for reg in &circuit.qregs {
            let _ = writeln!(
                out,
                "//   qubits {}..{}  = qreg {}[{}]",
                start,
                start + reg.size,
                reg.name,
                reg.size
            );
            start += reg.size;
        }
        let mut start = 0;
        for reg in &circuit.cregs {
            let _ = writeln!(
                out,
                "//   bits   {}..{}  = creg {}[{}]",
                start,
                start + reg.size,
                reg.name,
                reg.size
            );
            start += reg.size;
        }
    }
    out.push('\n');
    out.push_str(PRELUDE);
    out.push('\n');
    let _ = writeln!(out, "@target(qpu)\npublic func {}() {{", circuit.name);
    if circuit.num_qubits() > 0 {
        let _ = writeln!(out, "    alloc_qubits({});", circuit.num_qubits());
    }
    if circuit.num_clbits() > 0 {
        let _ = writeln!(out, "    alloc_bits({});", circuit.num_clbits());
    }
    for op in &circuit.ops {
        match op {
            Op::Gate { gate, qubits } => {
                let mut args: Vec<String> = gate
                    .params()
                    .into_iter()
                    .map(crate::qasm::format_angle)
                    .collect();
                args.extend(qubits.iter().map(ToString::to_string));
                let _ = writeln!(out, "    {}({});", gate.name(), args.join(", "));
            }
            Op::Measure { qubit, clbit } => {
                let _ = writeln!(out, "    measure({qubit}, {clbit});");
            }
            Op::Reset { qubit } => {
                let _ = writeln!(out, "    reset({qubit});");
            }
            Op::Barrier { .. } => out.push_str("    barrier();\n"),
        }
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests;
