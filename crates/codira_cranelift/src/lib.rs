//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - A second backend: Codira functions compiled to machine code by Cranelift
//!   and run in-process, for development builds where the time from an edit to
//!   a running program matters more than the last few percent of run-time
//!   speed.
//!
//! LLVM's optimization pipeline is most of what a Codira build costs (about
//! 4.4 ms warm for a small package on the development machine, before
//! linking). Cranelift was built for the opposite trade -- fast compilation
//! of reasonable code -- and needs no linker here: the JIT writes machine
//! code into memory and hands back a function pointer.
//!
//! # What it compiles
//!
//! The scalar core of the language: integers up to 64 bits, floats, `bool`
//! and unit; `let` bindings and assignment (compound too); arithmetic,
//! comparisons, logical and bitwise operators; casts between numeric types;
//! `if`/`else`, `loop`, `while`, `break` with and without a value, and
//! `return`; calls to other functions of the program; and `print`/`println`
//! of scalars and string literals.
//!
//! Anything else -- structs, classes, arrays, strings beyond literals,
//! externs, intrinsics, `@asm`, methods -- is reported as [`Unsupported`],
//! naming the construct and the function, and the caller falls back to the
//! LLVM backend for the whole program. Nothing is half-compiled: either every
//! function reachable from the entry point compiles here, or none runs here.

use std::{collections::VecDeque, io::Write as _, sync::Arc};

use codira_hir::{
    ArithOp, BinaryOp, BuiltinFn, CallableDef, CmpOp, Expr, ExprId, FloatBitness, Function,
    HirDatabase, IntBitness, Literal, LogicOp, Ordering, Package, Pat, PatId, ResolveBitness,
    Statement, Ty, TyKind, ValueNs,
};
use cranelift_codegen::{
    ir::{
        condcodes::{FloatCC, IntCC},
        types, AbiParam, Block, InstBuilder, Signature, TrapCode, Type, Value,
    },
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use rustc_hash::FxHashMap;

/// Why a program does not run on this backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// The function the construct is in.
    pub function: String,
    /// What was written that this backend does not compile.
    pub what: String,
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (in `{}`)", self.what, self.function)
    }
}

/// What the entry point returned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Returned {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
}

/// A program compiled to machine code, ready to run.
pub struct Program {
    module: JITModule,
    entry: FuncId,
    entry_return: Option<ScalarTy>,
}

impl Program {
    /// Calls the entry point, which takes no arguments.
    pub fn run(&self) -> Returned {
        let code = self.module.get_finalized_function(self.entry);
        // SAFETY: `entry` was compiled from a function with no parameters
        // returning `entry_return`, with the host's default calling
        // convention, which is what each cast below names.
        let returned = unsafe {
            macro_rules! call {
                ($ty:ty) => {
                    std::mem::transmute::<*const u8, extern "C" fn() -> $ty>(code)()
                };
            }
            match self.entry_return {
                None => {
                    std::mem::transmute::<*const u8, extern "C" fn()>(code)();
                    Returned::Unit
                }
                Some(ScalarTy::Bool) => Returned::Bool(call!(u8) != 0),
                Some(ScalarTy::Int { bits, signed }) => Returned::Int(match (bits, signed) {
                    (8, true) => i64::from(call!(i8)),
                    (8, false) => i64::from(call!(u8)),
                    (16, true) => i64::from(call!(i16)),
                    (16, false) => i64::from(call!(u16)),
                    (32, true) => i64::from(call!(i32)),
                    (32, false) => i64::from(call!(u32)),
                    (_, true) => call!(i64),
                    (_, false) => i64::try_from(call!(u64)).unwrap_or(i64::MAX),
                }),
                Some(ScalarTy::Float { bits: 32 }) => Returned::Float(f64::from(call!(f32))),
                Some(ScalarTy::Float { .. }) => Returned::Float(call!(f64)),
            }
        };
        let _ = std::io::stdout().flush();
        returned
    }
}

// The host side of `print`/`println`: one value each, formatted as the LLVM
// backend's runtime formats it.
extern "C" fn codira_jit_print_i64(value: i64) {
    print!("{value}");
}
extern "C" fn codira_jit_print_u64(value: u64) {
    print!("{value}");
}
extern "C" fn codira_jit_print_f64(value: f64) {
    print!("{value}");
}
extern "C" fn codira_jit_print_bool(value: u8) {
    print!("{}", value != 0);
}
/// # Safety
/// `data` points at `len` bytes of a string literal the JIT emitted.
unsafe extern "C" fn codira_jit_print_str(data: *const u8, len: usize) {
    // SAFETY: see the function's contract.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    print!("{}", String::from_utf8_lossy(bytes));
}

/// Compiles the program in `db` whose entry point is the function named
/// `entry`, and every function it can reach.
pub fn compile(db: &dyn HirDatabase, entry: &str) -> Result<Program, Unsupported> {
    let fail = |what: String| Unsupported {
        function: entry.to_owned(),
        what,
    };
    let entry_fn = Package::all(db)
        .into_iter()
        .flat_map(|package| package.modules(db))
        .flat_map(|module| module.all_functions(db))
        .find(|function| function.name(db).to_string() == entry)
        .ok_or_else(|| fail(format!("there is no function `{entry}`")))?;

    let mut flags = settings::builder();
    flags
        .set("opt_level", "speed")
        .map_err(|e| fail(format!("Cranelift setting: {e}")))?;
    let isa = cranelift_native::builder()
        .map_err(|e| fail(format!("this host is not one Cranelift supports: {e}")))?
        .finish(settings::Flags::new(flags))
        .map_err(|e| fail(format!("Cranelift target: {e}")))?;
    let mut jit = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    jit.symbol("codira_jit_print_i64", codira_jit_print_i64 as *const u8);
    jit.symbol("codira_jit_print_u64", codira_jit_print_u64 as *const u8);
    jit.symbol("codira_jit_print_f64", codira_jit_print_f64 as *const u8);
    jit.symbol("codira_jit_print_bool", codira_jit_print_bool as *const u8);
    jit.symbol("codira_jit_print_str", codira_jit_print_str as *const u8);
    let mut module = JITModule::new(jit);
    let pointer = module.target_config().pointer_type();

    let mut print_ids = FxHashMap::default();
    for (name, params) in [
        ("codira_jit_print_i64", vec![AbiParam::new(types::I64)]),
        ("codira_jit_print_u64", vec![AbiParam::new(types::I64)]),
        ("codira_jit_print_f64", vec![AbiParam::new(types::F64)]),
        // A Rust `u8` parameter: the caller zero-extends it (see
        // `ScalarTy::abi_param`).
        ("codira_jit_print_bool", vec![ScalarTy::Bool.abi_param()]),
        (
            "codira_jit_print_str",
            vec![AbiParam::new(pointer), AbiParam::new(pointer)],
        ),
    ] {
        let mut sig = module.make_signature();
        sig.params.extend(params);
        let id = module
            .declare_function(name, Linkage::Import, &sig)
            .map_err(|e| fail(e.to_string()))?;
        print_ids.insert(name, id);
    }

    let mut state = State {
        db,
        layout: db.target_data_layout(),
        module,
        declared: FxHashMap::default(),
        queue: VecDeque::new(),
        print_ids,
        strings: FxHashMap::default(),
        pointer,
    };
    let (entry_params, entry_return) = state.signature_types(entry_fn)?;
    if !entry_params.is_empty() {
        return Err(fail("an entry point with parameters".to_owned()));
    }
    let entry_id = state.declare(entry_fn)?;

    let mut ctx = state.module.make_context();
    let mut builder_ctx = FunctionBuilderContext::new();
    while let Some((function, id)) = state.queue.pop_front() {
        ctx.func.signature = state.signature(function)?;
        {
            let builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
            Translator::translate(&mut state, function, builder)?;
        }
        state
            .module
            .define_function(id, &mut ctx)
            .map_err(|e| Unsupported {
                function: function.name(db).to_string(),
                what: format!("Cranelift rejected the function: {e:?}"),
            })?;
        state.module.clear_context(&mut ctx);
    }
    state
        .module
        .finalize_definitions()
        .map_err(|e| fail(e.to_string()))?;

    Ok(Program {
        module: state.module,
        entry: entry_id,
        entry_return,
    })
}

/// A Codira scalar type, as this backend represents it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarTy {
    Bool,
    Int { bits: u8, signed: bool },
    Float { bits: u8 },
}

impl ScalarTy {
    /// The value as a parameter or result of a call, extended as the
    /// System V and `AArch64` C conventions that LLVM (and so Rust) implement
    /// expect: a `bool` or 8/16-bit integer occupies a full 32-bit register,
    /// zero- or sign-extended. Without the extension the upper bits of the
    /// register are whatever the computation left there, and an optimised
    /// Rust callee or caller (`codira_jit_print_bool`, the host reading the
    /// entry point's result) reads them: `(672u32 + 7u32) == 515u32` once
    /// printed `true` with a release-built compiler.
    fn abi_param(self) -> AbiParam {
        let param = AbiParam::new(self.clif());
        match self {
            ScalarTy::Bool
            | ScalarTy::Int {
                bits: 8 | 16,
                signed: false,
            } => param.uext(),
            ScalarTy::Int {
                bits: 8 | 16,
                signed: true,
            } => param.sext(),
            _ => param,
        }
    }

    fn clif(self) -> Type {
        match self {
            ScalarTy::Bool | ScalarTy::Int { bits: 8, .. } => types::I8,
            ScalarTy::Int { bits: 16, .. } => types::I16,
            ScalarTy::Int { bits: 32, .. } => types::I32,
            ScalarTy::Int { .. } => types::I64,
            ScalarTy::Float { bits: 32 } => types::F32,
            ScalarTy::Float { .. } => types::F64,
        }
    }
}

/// Program-wide state: the JIT module and which functions are declared.
struct State<'db> {
    db: &'db dyn HirDatabase,
    layout: Arc<codira_target::abi::TargetDataLayout>,
    module: JITModule,
    declared: FxHashMap<Function, FuncId>,
    queue: VecDeque<(Function, FuncId)>,
    print_ids: FxHashMap<&'static str, FuncId>,
    strings: FxHashMap<String, DataId>,
    pointer: Type,
}

impl State<'_> {
    /// The scalar type of `ty`, `None` for unit and `never`, or why it is
    /// not one this backend handles.
    fn scalar(&self, ty: &Ty) -> Result<Option<ScalarTy>, String> {
        Ok(Some(match ty.interned() {
            TyKind::Bool => ScalarTy::Bool,
            TyKind::Int(int_ty) => {
                let resolved = int_ty.resolve(&self.layout);
                let bits = match resolved.bitness {
                    IntBitness::X8 => 8,
                    IntBitness::X16 => 16,
                    IntBitness::X32 => 32,
                    IntBitness::X64 => 64,
                    IntBitness::X128 | IntBitness::Xsize => {
                        return Err("a 128-bit integer".to_owned())
                    }
                };
                ScalarTy::Int {
                    bits,
                    signed: resolved.signedness.is_signed(),
                }
            }
            TyKind::Float(float_ty) => ScalarTy::Float {
                bits: match float_ty.resolve(&self.layout).bitness {
                    FloatBitness::X32 => 32,
                    FloatBitness::X64 => 64,
                },
            },
            TyKind::Tuple(0, _) | TyKind::Never => return Ok(None),
            _ => {
                return Err(format!(
                    "a value of type `{}`",
                    codira_hir::HirDisplay::display(ty, self.db)
                ))
            }
        }))
    }

    fn signature_types(
        &self,
        function: Function,
    ) -> Result<(Vec<ScalarTy>, Option<ScalarTy>), Unsupported> {
        let unsupported = |what: String| Unsupported {
            function: function.name(self.db).to_string(),
            what,
        };
        if function.is_extern(self.db) || function.is_intrinsic(self.db) {
            return Err(unsupported("an `extern` function".to_owned()));
        }
        if function.self_param_ty(self.db).is_some() {
            return Err(unsupported("a method with `self`".to_owned()));
        }
        // Monomorphisation lives in the LLVM backend: this one compiles one
        // body per function, and a generic function has one per
        // instantiation.
        if function.is_generic(self.db) {
            return Err(unsupported("a generic function".to_owned()));
        }
        let sig = function
            .ty(self.db)
            .callable_sig(self.db)
            .ok_or_else(|| unsupported("a function without a signature".to_owned()))?;
        let mut params = Vec::new();
        for param in sig.params() {
            match self.scalar(param).map_err(&unsupported)? {
                Some(ty) => params.push(ty),
                None => return Err(unsupported("a unit parameter".to_owned())),
            }
        }
        let ret = self.scalar(sig.ret()).map_err(&unsupported)?;
        Ok((params, ret))
    }

    fn signature(&self, function: Function) -> Result<Signature, Unsupported> {
        let (params, ret) = self.signature_types(function)?;
        let mut sig = self.module.make_signature();
        sig.params.extend(params.iter().map(|ty| ty.abi_param()));
        if let Some(ret) = ret {
            sig.returns.push(ret.abi_param());
        }
        Ok(sig)
    }

    /// Declares `function` and queues its body, once.
    fn declare(&mut self, function: Function) -> Result<FuncId, Unsupported> {
        if let Some(&id) = self.declared.get(&function) {
            return Ok(id);
        }
        let sig = self.signature(function)?;
        // Named after the module path, so two modules' `helper`s do not
        // collide in the JIT's symbol table.
        let symbol = format!("codira${}", function.full_name(self.db));
        let id = self
            .module
            .declare_function(&symbol, Linkage::Local, &sig)
            .map_err(|e| Unsupported {
                function: function.name(self.db).to_string(),
                what: e.to_string(),
            })?;
        self.declared.insert(function, id);
        self.queue.push_back((function, id));
        Ok(id)
    }

    /// The data object holding `text`'s bytes, once per distinct literal.
    fn string(&mut self, text: &str) -> Result<DataId, String> {
        if let Some(&id) = self.strings.get(text) {
            return Ok(id);
        }
        let id = self
            .module
            .declare_anonymous_data(false, false)
            .map_err(|e| e.to_string())?;
        let mut data = DataDescription::new();
        data.define(text.as_bytes().to_vec().into_boxed_slice());
        self.module
            .define_data(id, &data)
            .map_err(|e| e.to_string())?;
        self.strings.insert(text.to_owned(), id);
        Ok(id)
    }
}

/// Where a `break` goes, and the value it carries.
struct LoopTarget {
    exit: Block,
    value: Option<ScalarTy>,
}

/// Translates one function body.
struct Translator<'a, 'db> {
    state: &'a mut State<'db>,
    function: Function,
    body: Arc<codira_hir::Body>,
    infer: Arc<codira_hir::InferenceResult>,
    builder: FunctionBuilder<'a>,
    variables: FxHashMap<PatId, (Variable, ScalarTy)>,
    loops: Vec<LoopTarget>,
    /// Whether the current block already ends in a terminator.
    terminated: bool,
}

type Lowered = Result<Option<Value>, Unsupported>;

impl<'a, 'db> Translator<'a, 'db> {
    fn translate(
        state: &'a mut State<'db>,
        function: Function,
        builder: FunctionBuilder<'a>,
    ) -> Result<(), Unsupported> {
        let body = function.body(state.db);
        let infer = function.infer(state.db);
        let (param_types, ret) = state.signature_types(function)?;
        let mut this = Translator {
            state,
            function,
            body: body.clone(),
            infer,
            builder,
            variables: FxHashMap::default(),
            loops: Vec::new(),
            terminated: false,
        };

        let entry = this.builder.create_block();
        this.builder.append_block_params_for_function_params(entry);
        this.builder.switch_to_block(entry);
        let params: Vec<Value> = this.builder.block_params(entry).to_vec();
        for (((pat, _), value), ty) in body.params().iter().zip(params).zip(param_types) {
            let var = this.builder.declare_var(ty.clif());
            this.builder.def_var(var, value);
            if matches!(body[*pat], Pat::Bind { .. }) {
                this.variables.insert(*pat, (var, ty));
            }
        }

        let result = this.expr(body.body_expr())?;
        if !this.terminated {
            match (ret, result) {
                (Some(_), Some(value)) => {
                    this.builder.ins().return_(&[value]);
                }
                (Some(ty), None) => {
                    let zero = this.zero(ty);
                    this.builder.ins().return_(&[zero]);
                }
                (None, _) => {
                    this.builder.ins().return_(&[]);
                }
            }
        }
        this.builder.seal_all_blocks();
        let frontend = this.state.module.target_config();
        this.builder.finalize(frontend);
        Ok(())
    }

    fn unsupported<T>(&self, what: impl Into<String>) -> Result<T, Unsupported> {
        Err(Unsupported {
            function: self.function.name(self.state.db).to_string(),
            what: what.into(),
        })
    }

    fn scalar_of(&self, ty: &Ty) -> Result<Option<ScalarTy>, Unsupported> {
        self.state.scalar(ty).or_else(|what| self.unsupported(what))
    }

    fn zero(&mut self, ty: ScalarTy) -> Value {
        match ty {
            ScalarTy::Float { bits: 32 } => self.builder.ins().f32const(0.0),
            ScalarTy::Float { .. } => self.builder.ins().f64const(0.0),
            other => self.builder.ins().iconst(other.clif(), 0),
        }
    }

    /// The value of an operand, which type checking guarantees has one.
    fn operand(&mut self, id: ExprId) -> Result<Value, Unsupported> {
        match self.expr(id)? {
            Some(value) => Ok(value),
            None => self.unsupported("an operand with no value"),
        }
    }

    /// Continues in a fresh block after a terminator: code that follows a
    /// `return` or `break` is unreachable but still has to be somewhere.
    fn after_terminator(&mut self) {
        let dead = self.builder.create_block();
        self.builder.switch_to_block(dead);
        self.terminated = false;
    }

    /// Jumps to `target` with `value`, unless the current block already
    /// ended.
    fn jump(&mut self, target: Block, value: Option<Value>) {
        if self.terminated {
            return;
        }
        match value {
            Some(value) => self.builder.ins().jump(target, &[value.into()]),
            None => self.builder.ins().jump(target, &[]),
        };
        self.terminated = true;
    }

    fn switch_to(&mut self, block: Block) {
        self.builder.switch_to_block(block);
        self.terminated = false;
    }

    fn local(&self, id: ExprId) -> Option<(Variable, ScalarTy)> {
        let Expr::Path(path) = &self.body[id] else {
            return None;
        };
        let resolver = codira_hir::resolver_for_expr(self.state.db, self.body.owner(), id);
        match resolver.resolve_path_as_value_fully(self.state.db, path) {
            Some((ValueNs::LocalBinding(pat), _)) => self.variables.get(&pat).copied(),
            _ => None,
        }
    }

    fn expr(&mut self, id: ExprId) -> Lowered {
        // A `T` used where a `T?` is expected changes representation, and
        // this backend has no optional representation to change it into.
        if self.infer.optional_wrap(id).is_some() {
            return self.unsupported("an optional (`T?`) value");
        }
        let body = self.body.clone();
        match &body[id] {
            Expr::Literal(literal) => self.literal(id, literal),
            Expr::Path(_) if self.infer.function_value(id).is_some() => {
                self.unsupported("a function used as a value")
            }
            Expr::Path(_) => match self.local(id) {
                Some((var, _)) => Ok(Some(self.builder.use_var(var))),
                None => self.unsupported("a path that is not a scalar local variable"),
            },
            Expr::Block { statements, tail } => {
                for statement in statements {
                    match statement {
                        Statement::Let {
                            pat,
                            initializer: Some(init),
                            ..
                        } => {
                            let value = self.expr(*init)?;
                            let Some(ty) = self.scalar_of(&self.infer[*pat])? else {
                                continue;
                            };
                            if !matches!(body[*pat], Pat::Bind { .. }) {
                                return self.unsupported("a `let` with a pattern");
                            }
                            let var = self.builder.declare_var(ty.clif());
                            let value = match value {
                                Some(value) => value,
                                None => self.zero(ty),
                            };
                            self.builder.def_var(var, value);
                            self.variables.insert(*pat, (var, ty));
                        }
                        Statement::Let {
                            initializer: None, ..
                        } => return self.unsupported("a `let` without an initializer"),
                        Statement::Expr(expr) => {
                            self.expr(*expr)?;
                        }
                    }
                }
                match tail {
                    Some(tail) => self.expr(*tail),
                    None => Ok(None),
                }
            }
            Expr::UnaryOp { expr, op } => {
                let ty = self.scalar_of(&self.infer[*expr])?;
                let value = self.operand(*expr)?;
                Ok(Some(match (op, ty) {
                    (codira_hir::UnaryOp::Neg, Some(ScalarTy::Float { .. })) => {
                        self.builder.ins().fneg(value)
                    }
                    (codira_hir::UnaryOp::Neg, _) => self.builder.ins().ineg(value),
                    (codira_hir::UnaryOp::Not, Some(ScalarTy::Bool)) => {
                        let one = self.builder.ins().iconst(types::I8, 1);
                        self.builder.ins().bxor(value, one)
                    }
                    (codira_hir::UnaryOp::Not | codira_hir::UnaryOp::BitNot, _) => {
                        self.builder.ins().bnot(value)
                    }
                }))
            }
            Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(op),
            } => self.binary(*lhs, *rhs, *op),
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => self.if_expr(id, *condition, *then_branch, *else_branch),
            Expr::Loop { body: loop_body } => {
                let header = self.builder.create_block();
                let exit = self.builder.create_block();
                let value = self.scalar_of(&self.infer[id])?;
                if let Some(ty) = value {
                    self.builder.append_block_param(exit, ty.clif());
                }
                self.jump(header, None);
                self.switch_to(header);
                self.loops.push(LoopTarget { exit, value });
                self.expr(*loop_body)?;
                self.loops.pop();
                self.jump(header, None);
                self.switch_to(exit);
                Ok(value.map(|_| self.builder.block_params(exit)[0]))
            }
            Expr::While {
                condition,
                body: loop_body,
            } => {
                let header = self.builder.create_block();
                let inside = self.builder.create_block();
                let exit = self.builder.create_block();
                self.jump(header, None);
                self.switch_to(header);
                let condition = self.operand(*condition)?;
                self.builder.ins().brif(condition, inside, &[], exit, &[]);
                self.switch_to(inside);
                self.loops.push(LoopTarget { exit, value: None });
                self.expr(*loop_body)?;
                self.loops.pop();
                self.jump(header, None);
                self.switch_to(exit);
                Ok(None)
            }
            Expr::Break { expr } => {
                let Some(target) = self.loops.last() else {
                    return self.unsupported("`break` outside a loop");
                };
                let (exit, value_ty) = (target.exit, target.value);
                let value = match expr {
                    Some(expr) => self.expr(*expr)?,
                    None => None,
                };
                let value = match (value_ty, value) {
                    (Some(_), Some(value)) => Some(value),
                    (Some(ty), None) => Some(self.zero(ty)),
                    (None, _) => None,
                };
                self.jump(exit, value);
                self.after_terminator();
                Ok(None)
            }
            Expr::Return { expr } => {
                let value = match expr {
                    Some(expr) => self.expr(*expr)?,
                    None => None,
                };
                if !self.terminated {
                    match value {
                        Some(value) => self.builder.ins().return_(&[value]),
                        None => self.builder.ins().return_(&[]),
                    };
                }
                self.after_terminator();
                Ok(None)
            }
            Expr::Call { callee, args } => self.call(id, *callee, args),
            Expr::Cast { expr, .. } => self.cast(id, *expr),
            Expr::Tuple(elements) if elements.is_empty() => Ok(None),
            Expr::Missing => self.unsupported("an expression the front end could not lower"),
            Expr::MethodCall { .. } => self.unsupported("a method call"),
            Expr::Index { .. } => self.unsupported("indexing"),
            Expr::RecordLit { .. } => self.unsupported("a struct literal"),
            Expr::Field { .. } => self.unsupported("a field access"),
            Expr::Array(_) => self.unsupported("an array"),
            // Structured concurrency lowers through the LLVM backend only.
            Expr::Select { .. } => self.unsupported("`select`"),
            Expr::Offer { .. } => self.unsupported("`offer`"),
            Expr::Closure { .. } => self.unsupported("a closure"),
            _ => self.unsupported("this kind of expression"),
        }
    }

    fn literal(&mut self, id: ExprId, literal: &Literal) -> Lowered {
        let ty = self.scalar_of(&self.infer[id])?;
        Ok(Some(match (literal, ty) {
            (Literal::Bool(b), _) => self.builder.ins().iconst(types::I8, i64::from(*b)),
            (Literal::Int(int), Some(ScalarTy::Float { bits })) => {
                // An integer literal can be inferred as a float.
                #[allow(clippy::cast_precision_loss)]
                let value = int.value as f64;
                if bits == 32 {
                    #[allow(clippy::cast_possible_truncation)]
                    let value = value as f32;
                    self.builder.ins().f32const(value)
                } else {
                    self.builder.ins().f64const(value)
                }
            }
            (Literal::Int(int), Some(ty)) => {
                // The literal's two's-complement bits, which is how `iconst`
                // takes them.
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let bits = int.value as u64 as i64;
                self.builder.ins().iconst(ty.clif(), bits)
            }
            (Literal::Float(float), Some(ScalarTy::Float { bits: 32 })) => {
                #[allow(clippy::cast_possible_truncation)]
                let value = float.value as f32;
                self.builder.ins().f32const(value)
            }
            (Literal::Float(float), _) => self.builder.ins().f64const(float.value),
            (Literal::String(_), _) => {
                return self.unsupported("a string outside `print`/`println`")
            }
            _ => return self.unsupported("this literal"),
        }))
    }

    fn binary(&mut self, lhs: ExprId, rhs: ExprId, op: BinaryOp) -> Lowered {
        match op {
            BinaryOp::Assignment { op } => {
                let Some((var, ty)) = self.local(lhs) else {
                    return self.unsupported("assignment to something other than a variable");
                };
                let value = self.operand(rhs)?;
                let value = match op {
                    None => value,
                    Some(arith) => {
                        let old = self.builder.use_var(var);
                        self.arith(arith, ty, old, value)?
                    }
                };
                self.builder.def_var(var, value);
                Ok(None)
            }
            BinaryOp::LogicOp(logic) => {
                let left = self.operand(lhs)?;
                let right_block = self.builder.create_block();
                let merge = self.builder.create_block();
                let result = self.builder.append_block_param(merge, types::I8);
                match logic {
                    LogicOp::And => {
                        self.builder
                            .ins()
                            .brif(left, right_block, &[], merge, &[left.into()]);
                    }
                    LogicOp::Or => {
                        self.builder
                            .ins()
                            .brif(left, merge, &[left.into()], right_block, &[]);
                    }
                }
                self.switch_to(right_block);
                let right = self.operand(rhs)?;
                self.jump(merge, Some(right));
                self.switch_to(merge);
                Ok(Some(result))
            }
            BinaryOp::ArithOp(arith) => {
                let Some(ty) = self.scalar_of(&self.infer[lhs])? else {
                    return self.unsupported("arithmetic on unit");
                };
                let left = self.operand(lhs)?;
                let right = self.operand(rhs)?;
                Ok(Some(self.arith(arith, ty, left, right)?))
            }
            BinaryOp::CmpOp(cmp) => {
                let Some(ty) = self.scalar_of(&self.infer[lhs])? else {
                    return self.unsupported("comparison of unit");
                };
                let left = self.operand(lhs)?;
                let right = self.operand(rhs)?;
                Ok(Some(self.compare(cmp, ty, left, right)))
            }
        }
    }

    fn arith(
        &mut self,
        op: ArithOp,
        ty: ScalarTy,
        a: Value,
        b: Value,
    ) -> Result<Value, Unsupported> {
        let signed = matches!(ty, ScalarTy::Int { signed: true, .. });
        if let ScalarTy::Float { .. } = ty {
            let ins = self.builder.ins();
            return match op {
                ArithOp::Add => Ok(ins.fadd(a, b)),
                ArithOp::Subtract => Ok(ins.fsub(a, b)),
                ArithOp::Multiply => Ok(ins.fmul(a, b)),
                ArithOp::Divide => Ok(ins.fdiv(a, b)),
                other => self.unsupported(format!("`{other:?}` on a float")),
            };
        }
        let ins = self.builder.ins();
        Ok(match op {
            ArithOp::Add => ins.iadd(a, b),
            ArithOp::Subtract => ins.isub(a, b),
            ArithOp::Multiply => ins.imul(a, b),
            ArithOp::Divide if signed => ins.sdiv(a, b),
            ArithOp::Divide => ins.udiv(a, b),
            ArithOp::Remainder if signed => ins.srem(a, b),
            ArithOp::Remainder => ins.urem(a, b),
            ArithOp::LeftShift => ins.ishl(a, b),
            ArithOp::RightShift if signed => ins.sshr(a, b),
            ArithOp::RightShift => ins.ushr(a, b),
            ArithOp::BitAnd => ins.band(a, b),
            ArithOp::BitOr => ins.bor(a, b),
            ArithOp::BitXor => ins.bxor(a, b),
        })
    }

    fn compare(&mut self, op: CmpOp, ty: ScalarTy, a: Value, b: Value) -> Value {
        let ordered = |ordering: Ordering, strict: bool| match (ordering, strict) {
            (Ordering::Less, true) => 0,
            (Ordering::Less, false) => 1,
            (Ordering::Greater, true) => 2,
            (Ordering::Greater, false) => 3,
        };
        if let ScalarTy::Float { .. } = ty {
            let cc = match op {
                CmpOp::Eq { negated: false } => FloatCC::Equal,
                // `!=` is true when an operand is NaN.
                CmpOp::Eq { negated: true } => FloatCC::NotEqual,
                CmpOp::Ord { ordering, strict } => [
                    FloatCC::LessThan,
                    FloatCC::LessThanOrEqual,
                    FloatCC::GreaterThan,
                    FloatCC::GreaterThanOrEqual,
                ][ordered(ordering, strict)],
            };
            return self.builder.ins().fcmp(cc, a, b);
        }
        let signed = matches!(ty, ScalarTy::Int { signed: true, .. });
        let cc = match op {
            CmpOp::Eq { negated: false } => IntCC::Equal,
            CmpOp::Eq { negated: true } => IntCC::NotEqual,
            CmpOp::Ord { ordering, strict } if signed => [
                IntCC::SignedLessThan,
                IntCC::SignedLessThanOrEqual,
                IntCC::SignedGreaterThan,
                IntCC::SignedGreaterThanOrEqual,
            ][ordered(ordering, strict)],
            CmpOp::Ord { ordering, strict } => [
                IntCC::UnsignedLessThan,
                IntCC::UnsignedLessThanOrEqual,
                IntCC::UnsignedGreaterThan,
                IntCC::UnsignedGreaterThanOrEqual,
            ][ordered(ordering, strict)],
        };
        self.builder.ins().icmp(cc, a, b)
    }

    fn if_expr(
        &mut self,
        id: ExprId,
        condition: ExprId,
        then_branch: ExprId,
        else_branch: Option<ExprId>,
    ) -> Lowered {
        let result_ty = if else_branch.is_some() {
            self.scalar_of(&self.infer[id])?
        } else {
            None
        };
        let condition = self.operand(condition)?;
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge = self.builder.create_block();
        let result = result_ty.map(|ty| self.builder.append_block_param(merge, ty.clif()));
        self.builder
            .ins()
            .brif(condition, then_block, &[], else_block, &[]);

        for (block, branch) in [(then_block, Some(then_branch)), (else_block, else_branch)] {
            self.switch_to(block);
            let value = match branch {
                Some(branch) => self.expr(branch)?,
                None => None,
            };
            let value = match (result_ty, value) {
                (Some(_), Some(value)) => Some(value),
                (Some(ty), None) => Some(self.zero(ty)),
                (None, _) => None,
            };
            self.jump(merge, value);
        }
        self.switch_to(merge);
        Ok(result)
    }

    fn call(&mut self, id: ExprId, callee: ExprId, args: &[ExprId]) -> Lowered {
        if let Some(builtin) = self.infer.builtin_call(id) {
            // Only the printing builtins compile here. Anything else --
            // `channel`, `declassify`, `ct_select`, and builtins added later
            // -- is declined by name rather than guessed at: treating an
            // unknown builtin as `print` would compile it into the wrong
            // program, which is worse than not compiling it.
            if !matches!(builtin, BuiltinFn::Print | BuiltinFn::Println) {
                return self.unsupported(format!("the builtin `{}`", builtin.name()));
            }
            for &arg in args {
                self.print(arg)?;
            }
            if builtin == BuiltinFn::Println {
                self.print_text("\n")?;
            }
            return Ok(None);
        }
        if self.infer.is_indirect_call(id) {
            return self.unsupported("a call through a function value");
        }
        let Some(CallableDef::Function(function)) = self.infer[callee].as_callable_def() else {
            return self.unsupported("a call to something other than a function");
        };
        if function.is_inline_asm(self.state.db) {
            return self.unsupported("a call to an `@asm` function");
        }
        let callee_id = self.state.declare(function)?;
        let mut values = Vec::with_capacity(args.len());
        for &arg in args {
            values.push(self.operand(arg)?);
        }
        let callee_ref = self
            .state
            .module
            .declare_func_in_func(callee_id, self.builder.func);
        let call = self.builder.ins().call(callee_ref, &values);
        // A function returning `never` has no result here either; nothing
        // after a call to it can run, so the block ends in a trap and what
        // follows goes to a dead block, as after a `return`.
        let never = function
            .ty(self.state.db)
            .callable_sig(self.state.db)
            .is_some_and(|sig| sig.ret().is_never());
        if never {
            self.builder.ins().trap(TrapCode::unwrap_user(1));
            self.terminated = true;
            self.after_terminator();
            return Ok(None);
        }
        Ok(self.builder.inst_results(call).first().copied())
    }

    fn call_print(&mut self, name: &'static str, args: &[Value]) {
        let print = self.state.print_ids[name];
        let print = self
            .state
            .module
            .declare_func_in_func(print, self.builder.func);
        self.builder.ins().call(print, args);
    }

    fn print_text(&mut self, text: &str) -> Result<(), Unsupported> {
        let data = self
            .state
            .string(text)
            .or_else(|what| self.unsupported(what))?;
        let global = self
            .state
            .module
            .declare_data_in_func(data, self.builder.func);
        let pointer = self.state.pointer;
        let address = self.builder.ins().symbol_value(pointer, global);
        let len = self
            .builder
            .ins()
            .iconst(pointer, i64::try_from(text.len()).unwrap_or(i64::MAX));
        self.call_print("codira_jit_print_str", &[address, len]);
        Ok(())
    }

    fn print(&mut self, arg: ExprId) -> Result<(), Unsupported> {
        if let Expr::Literal(Literal::String(text)) = &self.body.clone()[arg] {
            return self.print_text(text);
        }
        let ty = self.scalar_of(&self.infer[arg])?;
        let value = self.expr(arg)?;
        match (ty, value) {
            (Some(ScalarTy::Bool), Some(value)) => {
                self.call_print("codira_jit_print_bool", &[value]);
            }
            (Some(ScalarTy::Float { bits }), Some(value)) => {
                let value = if bits == 32 {
                    self.builder.ins().fpromote(types::F64, value)
                } else {
                    value
                };
                self.call_print("codira_jit_print_f64", &[value]);
            }
            (Some(ScalarTy::Int { bits, signed }), Some(value)) => {
                let value = match (bits, signed) {
                    (64, _) => value,
                    (_, true) => self.builder.ins().sextend(types::I64, value),
                    (_, false) => self.builder.ins().uextend(types::I64, value),
                };
                let name = if signed {
                    "codira_jit_print_i64"
                } else {
                    "codira_jit_print_u64"
                };
                self.call_print(name, &[value]);
            }
            _ => self.print_text("()")?,
        }
        Ok(())
    }

    fn cast(&mut self, id: ExprId, expr: ExprId) -> Lowered {
        let (Some(from), Some(to)) = (
            self.scalar_of(&self.infer[expr])?,
            self.scalar_of(&self.infer[id])?,
        ) else {
            return self.unsupported("a cast to or from unit");
        };
        let value = self.operand(expr)?;
        let target = to.clif();
        let ins = self.builder.ins();
        Ok(Some(match (from, to) {
            _ if from == to => value,
            (ScalarTy::Bool, ScalarTy::Int { bits: 8, .. }) => value,
            (ScalarTy::Bool, ScalarTy::Int { .. }) => ins.uextend(target, value),
            (
                ScalarTy::Int {
                    bits: from_bits,
                    signed,
                },
                ScalarTy::Int { bits: to_bits, .. },
            ) => match from_bits.cmp(&to_bits) {
                std::cmp::Ordering::Equal => value,
                std::cmp::Ordering::Greater => ins.ireduce(target, value),
                std::cmp::Ordering::Less if signed => ins.sextend(target, value),
                std::cmp::Ordering::Less => ins.uextend(target, value),
            },
            (ScalarTy::Int { signed: true, .. }, ScalarTy::Float { .. }) => {
                ins.fcvt_from_sint(target, value)
            }
            (ScalarTy::Int { signed: false, .. }, ScalarTy::Float { .. }) => {
                ins.fcvt_from_uint(target, value)
            }
            // Float to integer saturates, as the language defines it.
            (ScalarTy::Float { .. }, ScalarTy::Int { signed: true, .. }) => {
                ins.fcvt_to_sint_sat(target, value)
            }
            (ScalarTy::Float { .. }, ScalarTy::Int { signed: false, .. }) => {
                ins.fcvt_to_uint_sat(target, value)
            }
            (ScalarTy::Float { bits: 32 }, ScalarTy::Float { .. }) => ins.fpromote(target, value),
            (ScalarTy::Float { .. }, ScalarTy::Float { .. }) => ins.fdemote(target, value),
            _ => return self.unsupported("this cast"),
        }))
    }
}
