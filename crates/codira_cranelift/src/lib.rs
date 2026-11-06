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
//! `if`/`else`, `loop`, `while`, `for` over a range or an array, `break`
//! with and without a value, `continue` and `return`; calls to other
//! functions of the program and to methods; and `print`/`println` of
//! scalars and string literals.
//!
//! And the garbage-collected heap, with the object layout, run-time checks,
//! panic messages and exit statuses of the LLVM backend:
//!
//! * classes: construction (record literal, tuple-style call, unit value),
//!   field reads, and assignment (compound too) to fields;
//! * arrays: literals, indexing and assignment through an index with the
//!   same bounds check and message, `len()`, `push(..)` (growing the buffer
//!   through the runtime), `unchecked_get(..)`, and `for` loops over them.
//!
//! Anything else -- value structs, tuples, enums, optionals, strings beyond
//! literals, closures, pipelines, externs, intrinsics, `@asm` -- is reported
//! as [`Unsupported`], naming the construct and the function, and the caller
//! falls back to the LLVM backend for the whole program. Nothing is
//! half-compiled: either every function reachable from the entry point
//! compiles here, or none runs here.
//!
//! # The runtime
//!
//! Compiled code calls the runtime through the functions of the runtime
//! function catalogue (`codira_abi::runtime_functions`), resolved to the
//! functions `codira_runtime` registers for LLVM-compiled code: allocation,
//! array growth, printing and panics are the same code on both backends.
//! Objects are laid out by `codira_hir::layout`, which is checked against
//! LLVM's layout (`codira_codegen`'s `layout_matches_llvm`), and described to
//! the collector with descriptors built from it (`runtime::Heap`).
//!
//! # Safepoints and stack scanning
//!
//! The collector stops the world at safepoints -- every allocation, every
//! array growth, every blocking runtime call -- and scans each stopped
//! thread's stack conservatively: any word that looks like a handle keeps its
//! object alive. JIT frames are ordinary native frames on the same stack, so
//! they are scanned exactly as AOT frames are, with the same reasoning: a
//! handle live across a call is either in a callee-saved register (captured
//! at the safepoint) or spilled to the frame (scanned). Generated code never
//! keeps only a derived pointer (an object's payload address) live across a
//! call: it reloads the payload address from the handle after every call
//! that may allocate, so the handle is what stays live, and an array's
//! payload, which `push` can move, is never used stale.
//!
//! Precise scanning (stack maps) is being built for the LLVM backend. For
//! JIT frames to take part, the collector needs, for each safepoint (each
//! call site that can reach one), the frame offsets of the live GC handles.
//! Cranelift computes exactly that ("user stack maps") for values declared
//! as needing one: [`Options::precise_stack_maps`] declares every handle the
//! generated code holds (`Translator::gc_value`), and the maps Cranelift
//! produces are counted in [`Program::stack_maps`]. Nothing consumes them
//! yet; the switch is off by default and is the hook where a precise
//! collector would read them (from the compiled code's buffer, keyed by
//! return address) and, if objects start to move, rewrite the slots.

use std::{collections::VecDeque, io::Write as _, sync::Arc};

use codira_hir::{Function, HirDatabase, Package, Ty, TyKind};
use cranelift_codegen::{
    ir::{types, AbiParam, Signature, Type},
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use rustc_hash::FxHashMap;

mod heap;
mod runtime;
mod translate;

use translate::Translator;

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

/// How to compile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Ask Cranelift for a stack map at every call for the GC handles live
    /// across it (see the crate documentation). Off by default: nothing
    /// reads the maps yet, and asking for them makes Cranelift keep those
    /// handles in stack slots across calls.
    pub precise_stack_maps: bool,
    /// The most bytes the program's garbage-collected heap may hold, as
    /// `codira_runtime::RuntimeBuilder::heap_limit` sets for a runtime: at
    /// the limit an allocation collects first. `None` for no limit.
    pub heap_limit: Option<usize>,
}

/// A program compiled to machine code, ready to run.
pub struct Program {
    module: JITModule,
    entry: FuncId,
    entry_return: Option<ScalarTy>,
    stack_maps: usize,
    // Dropped after the code: the type descriptors the code refers to and
    // the collector it allocates in.
    heap: runtime::Heap,
}

impl Program {
    /// Calls the entry point, which takes no arguments, on this thread,
    /// registered with the collector as running Codira code for the call
    /// (so a collection scans this thread's stack).
    pub fn run(&self) -> Returned {
        let code = self.module.get_finalized_function(self.entry);
        let _running = codira_runtime::enter_codira();
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

    /// How many stack maps Cranelift produced: zero unless
    /// [`Options::precise_stack_maps`] was set.
    pub fn stack_maps(&self) -> usize {
        self.stack_maps
    }

    /// Runs a full collection of this program's heap (for tests: the
    /// program's objects must survive it while referenced from its frames).
    pub fn collect_garbage(&self) -> bool {
        self.heap.collector().collect()
    }

    /// How many collections allocations had to run because the heap was at
    /// [`Options::heap_limit`].
    pub fn emergency_collections(&self) -> usize {
        self.heap.collector().emergency_collections()
    }
}

/// Compiles the program in `db` whose entry point is the function named
/// `entry`, and every function it can reach.
pub fn compile(db: &dyn HirDatabase, entry: &str) -> Result<Program, Unsupported> {
    compile_with(db, entry, Options::default())
}

/// [`compile`], with [`Options`].
pub fn compile_with(
    db: &dyn HirDatabase,
    entry: &str,
    options: Options,
) -> Result<Program, Unsupported> {
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
    for (name, value) in [
        ("opt_level", "speed"),
        // Results that do not fit the return registers (a `String` is
        // three words) go through a hidden return area.
        ("enable_multi_ret_implicit_sret", "true"),
    ] {
        flags
            .set(name, value)
            .map_err(|e| fail(format!("Cranelift setting: {e}")))?;
    }
    let isa = cranelift_native::builder()
        .map_err(|e| fail(format!("this host is not one Cranelift supports: {e}")))?
        .finish(settings::Flags::new(flags))
        .map_err(|e| fail(format!("Cranelift target: {e}")))?;
    let mut jit = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    runtime::register_symbols(&mut jit);
    let module = JITModule::new(jit);
    let pointer = module.target_config().pointer_type();

    let mut state = State {
        db,
        layout: db.target_data_layout(),
        module,
        declared: FxHashMap::default(),
        queue: VecDeque::new(),
        imports: runtime::Imports::default(),
        heap: runtime::Heap::new(),
        strings: FxHashMap::default(),
        pointer,
        options: options.clone(),
    };
    state.heap.collector().set_heap_limit(options.heap_limit);
    let (entry_params, entry_ret) = state.signature_types(entry_fn)?;
    if !entry_params.is_empty() {
        return Err(fail("an entry point with parameters".to_owned()));
    }
    let entry_return = state.scalar(&entry_ret).map_err(|what| {
        fail(format!(
            "an entry point returning {what} (the host reads only numbers and `bool`s)"
        ))
    })?;
    let entry_id = state.declare(entry_fn)?;

    let mut ctx = state.module.make_context();
    let mut builder_ctx = FunctionBuilderContext::new();
    let mut stack_maps = 0;
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
        if let Some(code) = ctx.compiled_code() {
            stack_maps += code.buffer.user_stack_maps().len();
        }
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
        stack_maps,
        heap: state.heap,
    })
}

/// A Codira scalar type, as this backend represents it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScalarTy {
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
    /// Rust callee or caller (`codira.print_bool`, the host reading the
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

    pub(crate) fn clif(self) -> Type {
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

/// Program-wide state: the JIT module, which functions are declared, the
/// runtime functions imported and the heap the program allocates in.
pub(crate) struct State<'db> {
    pub(crate) db: &'db dyn HirDatabase,
    pub(crate) layout: Arc<codira_target::abi::TargetDataLayout>,
    pub(crate) module: JITModule,
    declared: FxHashMap<Function, FuncId>,
    queue: VecDeque<(Function, FuncId)>,
    pub(crate) imports: runtime::Imports,
    pub(crate) heap: runtime::Heap,
    strings: FxHashMap<String, DataId>,
    pub(crate) pointer: Type,
    pub(crate) options: Options,
}

impl State<'_> {
    pub(crate) fn display(&self, ty: &Ty) -> String {
        codira_hir::HirDisplay::display(ty, self.db).to_string()
    }

    /// The scalar type of `ty`, `None` for unit and `never`, or why it is
    /// not a scalar this backend handles.
    pub(crate) fn scalar(&self, ty: &Ty) -> Result<Option<ScalarTy>, String> {
        use codira_hir::{FloatBitness, IntBitness, ResolveBitness};
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
            TyKind::String => {
                return Err("a `String`".to_owned());
            }
            _ => return Err(format!("a value of type `{}`", self.display(ty))),
        }))
    }

    /// Whether this backend represents values of `ty`: a scalar, unit, a
    /// class instance or an array of such values.
    pub(crate) fn check(&self, ty: &Ty) -> Result<(), String> {
        match ty.interned() {
            TyKind::Struct(s, _)
                if s.data(self.db).memory_kind == codira_hir::StructMemoryKind::Gc =>
            {
                if codira_hir::layout::object_layout(self.db, ty).is_none() {
                    return Err(format!(
                        "a class with a field the backend cannot lay out (`{}`)",
                        self.display(ty)
                    ));
                }
                Ok(())
            }
            TyKind::Array(element) => self.check(element),
            _ => self.scalar(ty).map(|_| ()),
        }
    }

    /// The Cranelift types of the scalars a value of `ty` is made of, in
    /// memory order: none for unit, one for a scalar or a reference.
    pub(crate) fn clif_types(&self, ty: &Ty) -> Result<Vec<Type>, String> {
        self.check(ty)?;
        if let Some(scalar) = self.scalar(ty).ok().flatten() {
            return Ok(vec![scalar.clif()]);
        }
        let layout = codira_hir::layout::value_layout(self.db, ty)
            .ok_or_else(|| format!("a value of type `{}`", self.display(ty)))?;
        Ok(layout
            .leaves
            .iter()
            .map(|leaf| self.leaf_type(leaf.scalar))
            .collect())
    }

    pub(crate) fn leaf_type(&self, scalar: codira_hir::layout::Scalar) -> Type {
        use codira_hir::layout::Scalar;
        match scalar {
            Scalar::Bool | Scalar::Int(8) => types::I8,
            Scalar::Int(16) => types::I16,
            Scalar::Int(32) => types::I32,
            Scalar::Int(64) => types::I64,
            Scalar::Int(_) => types::I128,
            Scalar::F32 => types::F32,
            Scalar::F64 => types::F64,
            Scalar::Ptr { .. } => self.pointer,
        }
    }

    /// How a value of `ty` is passed to or returned from a function of the
    /// program: one parameter per scalar, a number or `bool` extended as the
    /// C convention does.
    fn abi_params(&self, ty: &Ty) -> Result<Vec<AbiParam>, String> {
        if let Some(scalar) = self.scalar(ty).ok().flatten() {
            return Ok(vec![scalar.abi_param()]);
        }
        Ok(self
            .clif_types(ty)?
            .into_iter()
            .map(AbiParam::new)
            .collect())
    }

    /// The parameter types of `function` (its receiver first) and its
    /// result type.
    pub(crate) fn signature_types(&self, function: Function) -> Result<(Vec<Ty>, Ty), Unsupported> {
        let unsupported = |what: String| Unsupported {
            function: function.name(self.db).to_string(),
            what,
        };
        if function.is_extern(self.db) || function.is_intrinsic(self.db) {
            return Err(unsupported("an `extern` function".to_owned()));
        }
        // Monomorphisation lives in the LLVM backend: this one compiles one
        // body per function, and a generic function has one per
        // instantiation.
        if function.is_generic(self.db) {
            return Err(unsupported("a generic function".to_owned()));
        }
        if !function.effects(self.db).is_empty() {
            return Err(unsupported("a function with a `uses` clause".to_owned()));
        }
        let sig = function
            .ty(self.db)
            .callable_sig(self.db)
            .ok_or_else(|| unsupported("a function without a signature".to_owned()))?;
        let params: Vec<Ty> = function
            .self_param_ty(self.db)
            .into_iter()
            .chain(sig.params().iter().cloned())
            .collect();
        for param in &params {
            if param.is_empty() {
                return Err(unsupported("a unit parameter".to_owned()));
            }
            self.check(param).map_err(&unsupported)?;
        }
        let ret = sig.ret().clone();
        self.check(&ret).map_err(&unsupported)?;
        Ok((params, ret))
    }

    fn signature(&self, function: Function) -> Result<Signature, Unsupported> {
        let (params, ret) = self.signature_types(function)?;
        let unsupported = |what: String| Unsupported {
            function: function.name(self.db).to_string(),
            what,
        };
        let mut sig = self.module.make_signature();
        for param in &params {
            sig.params
                .extend(self.abi_params(param).map_err(&unsupported)?);
        }
        if !ret.is_never() {
            sig.returns
                .extend(self.abi_params(&ret).map_err(&unsupported)?);
        }
        Ok(sig)
    }

    /// Declares `function` and queues its body, once.
    pub(crate) fn declare(&mut self, function: Function) -> Result<FuncId, Unsupported> {
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
    pub(crate) fn string(&mut self, text: &str) -> Result<DataId, String> {
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
