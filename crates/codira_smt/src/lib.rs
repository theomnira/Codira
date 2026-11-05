//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module doc-comment restored below.
//!
//! SMT-based feasibility/proof checking, backed by a real Z3 solver.
//!
//! This is the "SMT-based feasibility/proof checking" piece described in
//! `spec/self_healing_programming_language.md`: given Codira's refinement
//! types (`Type { binder | predicate }`, see `spec/LANGUAGE_SPEC.md` §7)
//! and healing-strategy preconditions, this crate can actually ask a real
//! SMT solver "is this predicate satisfiable?" and "does this predicate
//! imply that one?" over linear integer arithmetic, rather than faking an
//! answer or hand-rolling a bespoke (and much weaker) decision procedure.
//!
//! It binds directly to Z3's C API (`libz3.dll`) via hand-written FFI
//! declarations rather than a bindgen-based crate: the API surface actually
//! needed here (contexts, integer/boolean expressions, a handful of
//! arithmetic/comparison/boolean operators, and a solver) is small and
//! stable enough that hand-writing it avoids depending on libclang being
//! discoverable at build time. See `build.rs` for how the real Z3 install
//! is located and linked.

mod ffi;

use std::ffi::{c_int, CStr, CString};

/// An owned Z3 context: the arena every expression, sort, and solver
/// created through this crate's API lives in. Expressions and solvers
/// borrow from their `Context` and cannot outlive it.
pub struct Context {
    raw: ffi::Z3_context,
    config: ffi::Z3_config,
    /// The deterministic resource budget (Z3 `rlimit`) every solver made
    /// from this context starts with. See [`DEFAULT_RLIMIT`].
    rlimit: std::cell::Cell<u32>,
}

/// The deterministic per-check budget every [`Solver`] starts with, in Z3
/// resource units (`rlimit`).
///
/// Compilation decisions must not depend on wall-clock time (spec
/// `KGEN_SUPERSET_STATUS.md` section 32.5): a timeout makes the same program
/// verify on a fast machine and fail on a loaded one. `rlimit` counts the
/// solver's own work instead, so a query either fits the budget everywhere
/// or nowhere. A check that exhausts it reports [`SatResult::Unknown`] and
/// [`Solver::check_detailed`] says why -- it is never reported as a proof.
///
/// The value is generous for the linear and bit-vector queries the front end
/// issues (the slowest test query here uses well under a tenth of it); a
/// caller that knows better sets its own with [`Context::set_rlimit`] or
/// [`Solver::set_rlimit`].
pub const DEFAULT_RLIMIT: u32 = 10_000_000;

// Z3 contexts are safe to send between threads as long as they aren't
// accessed concurrently, which the `&`/`&mut` borrows below already
// enforce; there's no thread-local state involved.
unsafe impl Send for Context {}

impl Context {
    /// Creates a fresh Z3 context.
    pub fn new() -> Self {
        // Safety: `Z3_mk_config`/`Z3_mk_context` are always safe to call and
        // never return null on any Z3 build actually shipped.
        unsafe {
            let config = ffi::Z3_mk_config();
            let raw = ffi::Z3_mk_context(config);
            Context {
                raw,
                config,
                rlimit: std::cell::Cell::new(DEFAULT_RLIMIT),
            }
        }
    }

    fn int_sort(&self) -> ffi::Z3_sort {
        unsafe { ffi::Z3_mk_int_sort(self.raw) }
    }

    fn bool_sort(&self) -> ffi::Z3_sort {
        unsafe { ffi::Z3_mk_bool_sort(self.raw) }
    }

    /// Declares a free integer-sorted variable with the given name.
    pub fn int_var(&self, name: &str) -> IntExpr<'_> {
        let c_name = CString::new(name).expect("variable name must not contain a NUL byte");
        // Safety: all arguments are valid, live for the duration of the
        // call, and `self.raw` outlives the returned `Z3_ast` (which is
        // freed only when `self.raw` itself is destroyed).
        let raw = unsafe {
            let symbol = ffi::Z3_mk_string_symbol(self.raw, c_name.as_ptr());
            ffi::Z3_mk_const(self.raw, symbol, self.int_sort())
        };
        IntExpr { ctx: self, raw }
    }

    /// An integer literal.
    pub fn int_lit(&self, value: i64) -> IntExpr<'_> {
        let c_value = CString::new(value.to_string()).unwrap();
        let raw = unsafe { ffi::Z3_mk_numeral(self.raw, c_value.as_ptr(), self.int_sort()) };
        IntExpr { ctx: self, raw }
    }

    /// A free boolean-sorted variable with the given name.
    pub fn bool_var(&self, name: &str) -> BoolExpr<'_> {
        let c_name = CString::new(name).expect("variable name must not contain a NUL byte");
        let raw = unsafe {
            let symbol = ffi::Z3_mk_string_symbol(self.raw, c_name.as_ptr());
            ffi::Z3_mk_const(self.raw, symbol, self.bool_sort())
        };
        BoolExpr { ctx: self, raw }
    }

    /// The boolean literal `true`.
    pub fn bool_true(&self) -> BoolExpr<'_> {
        BoolExpr {
            ctx: self,
            raw: unsafe { ffi::Z3_mk_true(self.raw) },
        }
    }

    /// The boolean literal `false`.
    pub fn bool_false(&self) -> BoolExpr<'_> {
        BoolExpr {
            ctx: self,
            raw: unsafe { ffi::Z3_mk_false(self.raw) },
        }
    }

    /// Creates a new solver bound to this context. It starts with this
    /// context's resource budget (see [`Context::set_rlimit`]).
    pub fn solver(&self) -> Solver<'_> {
        let raw = unsafe {
            let solver = ffi::Z3_mk_solver(self.raw);
            ffi::Z3_solver_inc_ref(self.raw, solver);
            solver
        };
        let solver = Solver {
            ctx: self,
            raw,
            timeout_set: std::cell::Cell::new(false),
        };
        let rlimit = self.rlimit.get();
        if rlimit != 0 {
            solver.set_rlimit(rlimit);
        }
        solver
    }

    /// Sets the deterministic resource budget (Z3 `rlimit`) for every solver
    /// created from this context afterwards. `0` removes the bound, which no
    /// compiler-facing caller should do.
    pub fn set_rlimit(&self, rlimit: u32) {
        self.rlimit.set(rlimit);
    }

    /// The budget solvers created from this context start with.
    pub fn rlimit(&self) -> u32 {
        self.rlimit.get()
    }

    /// Writes `assumptions` followed by `formula` as a self-contained
    /// SMT-LIB2 benchmark: a declaration for every free constant, one
    /// `(assert ..)` per assumption and one for `formula`, then
    /// `(check-sat)`.
    ///
    /// Any SMT-LIB2 solver can decide the text without this crate or the
    /// code that built the terms; [`Context::parse_smtlib2`] reads it back.
    /// That is what a proof certificate stores for each obligation: the
    /// premise as the assumption and the negated goal as the formula, so
    /// `unsat` means the obligation holds.
    pub fn to_smtlib2(&self, assumptions: &[BoolExpr<'_>], formula: BoolExpr<'_>) -> String {
        let raws: Vec<ffi::Z3_ast> = assumptions.iter().map(|a| a.raw).collect();
        let empty = CString::new("").unwrap();
        let status = CString::new("unknown").unwrap();
        // Safety: every string is NUL-terminated and outlives the call, and
        // `raws` holds `raws.len()` live ASTs of this context. The returned
        // buffer belongs to the context and is copied out immediately.
        unsafe {
            let text = ffi::Z3_benchmark_to_smtlib_string(
                self.raw,
                empty.as_ptr(),
                empty.as_ptr(),
                status.as_ptr(),
                empty.as_ptr(),
                raws.len() as std::ffi::c_uint,
                raws.as_ptr(),
                formula.raw,
            );
            if text.is_null() {
                String::new()
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            }
        }
    }

    /// Reads the assertions of an SMT-LIB2 script, in order. Commands
    /// other than declarations and assertions (`check-sat`, options) are
    /// not executed: the caller decides what to do with the formulas, under
    /// its own resource budget.
    ///
    /// The text may come from anywhere, so a parse error is returned as
    /// `Err` with Z3's message instead of reaching Z3's default error
    /// handler, which would end the process. To that end this switches the
    /// context to report errors through `Z3_get_error_code`, for the rest of
    /// its life.
    pub fn parse_smtlib2(&self, text: &str) -> Result<Vec<BoolExpr<'_>>, String> {
        let source = CString::new(text)
            .map_err(|e| format!("the SMT-LIB2 text contains a NUL byte: {e}"))?;
        // Safety: a null handler is Z3's documented way to opt out of the
        // default one; `source` is NUL-terminated and outlives the call; the
        // vector is reference-counted and released below on every path.
        unsafe {
            ffi::Z3_set_error_handler(self.raw, None);
            let vector = ffi::Z3_parse_smtlib2_string(
                self.raw,
                source.as_ptr(),
                0,
                std::ptr::null(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            );
            let code = ffi::Z3_get_error_code(self.raw);
            if code != ffi::Z3_OK || vector.0.is_null() {
                let message = ffi::Z3_get_error_msg(self.raw, code);
                let message = if message.is_null() {
                    "the SMT-LIB2 text could not be parsed".to_string()
                } else {
                    CStr::from_ptr(message).to_string_lossy().trim().to_string()
                };
                return Err(message);
            }
            ffi::Z3_ast_vector_inc_ref(self.raw, vector);
            let count = ffi::Z3_ast_vector_size(self.raw, vector);
            let mut assertions = Vec::with_capacity(count as usize);
            for i in 0..count {
                let raw = ffi::Z3_ast_vector_get(self.raw, vector, i);
                let sort = ffi::Z3_get_sort(self.raw, raw);
                if ffi::Z3_get_sort_kind(self.raw, sort) != ffi::Z3_BOOL_SORT {
                    ffi::Z3_ast_vector_dec_ref(self.raw, vector);
                    return Err("an assertion is not a boolean formula".to_string());
                }
                // The vector holds the only reference to what it returns:
                // in a context without user reference counting, an AST is
                // kept alive by being the result of a constructor call, so
                // each assertion is rebuilt as the one-argument conjunction
                // (which is itself) while the vector still owns it.
                let kept = ffi::Z3_mk_and(self.raw, 1, &raw);
                assertions.push(BoolExpr {
                    ctx: self,
                    raw: kept,
                });
            }
            ffi::Z3_ast_vector_dec_ref(self.raw, vector);
            Ok(assertions)
        }
    }

    fn bv_sort(&self, width: u32) -> ffi::Z3_sort {
        assert!(width > 0, "a bitvector must be at least one bit wide");
        unsafe { ffi::Z3_mk_bv_sort(self.raw, width) }
    }

    fn raw_sort(&self, sort: &Sort) -> ffi::Z3_sort {
        match sort {
            Sort::Bool => self.bool_sort(),
            Sort::Bv(width) => self.bv_sort(*width),
            Sort::Array(index, elem) => unsafe {
                ffi::Z3_mk_array_sort(self.raw, self.raw_sort(index), self.raw_sort(elem))
            },
        }
    }

    /// A free constant of any modelled sort.
    pub fn var(&self, name: &str, sort: &Sort) -> Term<'_> {
        let c_name = CString::new(name).expect("variable name must not contain a NUL byte");
        let raw = unsafe {
            let symbol = ffi::Z3_mk_string_symbol(self.raw, c_name.as_ptr());
            ffi::Z3_mk_const(self.raw, symbol, self.raw_sort(sort))
        };
        Term::from_raw(self, raw).expect("a modelled sort")
    }

    /// A free array constant from `index` to `elem`.
    pub fn array_var(&self, name: &str, index: &Sort, elem: &Sort) -> ArrExpr<'_> {
        match self.var(
            name,
            &Sort::Array(Box::new(index.clone()), Box::new(elem.clone())),
        ) {
            Term::Array(a) => a,
            _ => unreachable!("an array sort gives an array term"),
        }
    }

    /// The array from `index` that holds `value` at every index.
    pub fn const_array<'a>(&'a self, index: &Sort, value: Term<'a>) -> ArrExpr<'a> {
        ArrExpr {
            ctx: self,
            raw: unsafe { ffi::Z3_mk_const_array(self.raw, self.raw_sort(index), value.raw()) },
        }
    }

    /// Declares the uninterpreted function `name : domain -> range`.
    /// Declaring the same name and sorts again gives the same function.
    pub fn func(&self, name: &str, domain: &[Sort], range: &Sort) -> FuncDecl<'_> {
        let c_name = CString::new(name).expect("function name must not contain a NUL byte");
        let sorts: Vec<ffi::Z3_sort> = domain.iter().map(|s| self.raw_sort(s)).collect();
        let raw = unsafe {
            let symbol = ffi::Z3_mk_string_symbol(self.raw, c_name.as_ptr());
            ffi::Z3_mk_func_decl(
                self.raw,
                symbol,
                sorts.len() as std::ffi::c_uint,
                sorts.as_ptr(),
                self.raw_sort(range),
            )
        };
        FuncDecl {
            ctx: self,
            raw,
            range: range.clone(),
        }
    }

    /// Declares a free bitvector variable `width` bits wide: the exact
    /// model of a fixed-width machine integer of that width. Signedness is
    /// not part of the sort; it is chosen per operation (`slt` vs `ult`,
    /// `sdiv` vs `udiv`, sign vs zero extension), exactly as in LLVM IR.
    pub fn bv_var(&self, name: &str, width: u32) -> BvExpr<'_> {
        let c_name = CString::new(name).expect("variable name must not contain a NUL byte");
        let raw = unsafe {
            let symbol = ffi::Z3_mk_string_symbol(self.raw, c_name.as_ptr());
            ffi::Z3_mk_const(self.raw, symbol, self.bv_sort(width))
        };
        BvExpr { ctx: self, raw }
    }

    /// A `width`-bit literal whose bit pattern is the low `width` bits of
    /// `bits`. Pass a two's-complement value cast to `u128` for a negative
    /// signed literal: `bv_lit(-1i128 as u128, 8)` is `0xFF`.
    pub fn bv_lit(&self, bits: u128, width: u32) -> BvExpr<'_> {
        let masked = if width >= 128 {
            bits
        } else {
            bits & ((1u128 << width) - 1)
        };
        let c_value = CString::new(masked.to_string()).unwrap();
        let raw = unsafe { ffi::Z3_mk_numeral(self.raw, c_value.as_ptr(), self.bv_sort(width)) };
        BvExpr { ctx: self, raw }
    }

    fn bv64_sort(&self) -> ffi::Z3_sort {
        unsafe { ffi::Z3_mk_bv_sort(self.raw, 64) }
    }

    /// Declares a free 64-bit bitvector variable: exact two's-complement
    /// machine-integer semantics, the theory that models
    /// `codira_mir::fold_op`'s wrapping `i64` arithmetic with *no*
    /// unbounded-integer approximation (unlike [`Context::int_var`]).
    pub fn bv64_var(&self, name: &str) -> BvExpr<'_> {
        let c_name = CString::new(name).expect("variable name must not contain a NUL byte");
        let raw = unsafe {
            let symbol = ffi::Z3_mk_string_symbol(self.raw, c_name.as_ptr());
            ffi::Z3_mk_const(self.raw, symbol, self.bv64_sort())
        };
        BvExpr { ctx: self, raw }
    }

    /// A 64-bit bitvector literal (two's-complement encoding of `value`).
    pub fn bv64_lit(&self, value: i64) -> BvExpr<'_> {
        let raw = unsafe { ffi::Z3_mk_int64(self.raw, value, self.bv64_sort()) };
        BvExpr { ctx: self, raw }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // Safety: every `Z3_ast`/`Z3_sort` created through this context is
        // owned by the context itself (this crate never uses the
        // reference-counted `Z3_mk_context_rc` variant), so it's correct to
        // free them all at once by deleting the context.
        unsafe {
            ffi::Z3_del_context(self.raw);
            ffi::Z3_del_config(self.config);
        }
    }
}

/// An integer-sorted Z3 expression, borrowed from the [`Context`] that
/// created it.
#[derive(Clone, Copy)]
pub struct IntExpr<'ctx> {
    ctx: &'ctx Context,
    raw: ffi::Z3_ast,
}

impl<'ctx> IntExpr<'ctx> {
    // clippy::should_implement_trait fires on `add`/`sub`/`mul`/`bitand`
    // and friends. Implementing `std::ops` here would be wrong: these do
    // not compute a value, they *build an SMT term* in a solver context,
    // and they take `self` by value while borrowing the context. The
    // names deliberately mirror the operations they construct.
    #![allow(clippy::should_implement_trait)]

    fn binop(
        self,
        other: IntExpr<'ctx>,
        f: unsafe extern "C" fn(ffi::Z3_context, ffi::Z3_ast, ffi::Z3_ast) -> ffi::Z3_ast,
    ) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { f(self.ctx.raw, self.raw, other.raw) },
        }
    }

    fn variadic(
        self,
        other: IntExpr<'ctx>,
        f: unsafe extern "C" fn(ffi::Z3_context, c_int, *const ffi::Z3_ast) -> ffi::Z3_ast,
    ) -> IntExpr<'ctx> {
        let args = [self.raw, other.raw];
        IntExpr {
            ctx: self.ctx,
            raw: unsafe { f(self.ctx.raw, 2, args.as_ptr()) },
        }
    }

    pub fn lt(self, other: IntExpr<'ctx>) -> BoolExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_lt)
    }

    pub fn le(self, other: IntExpr<'ctx>) -> BoolExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_le)
    }

    pub fn gt(self, other: IntExpr<'ctx>) -> BoolExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_gt)
    }

    pub fn ge(self, other: IntExpr<'ctx>) -> BoolExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_ge)
    }

    pub fn eq(self, other: IntExpr<'ctx>) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_eq(self.ctx.raw, self.raw, other.raw) },
        }
    }

    pub fn add(self, other: IntExpr<'ctx>) -> IntExpr<'ctx> {
        self.variadic(other, ffi::Z3_mk_add)
    }

    pub fn sub(self, other: IntExpr<'ctx>) -> IntExpr<'ctx> {
        self.variadic(other, ffi::Z3_mk_sub)
    }

    pub fn mul(self, other: IntExpr<'ctx>) -> IntExpr<'ctx> {
        self.variadic(other, ffi::Z3_mk_mul)
    }
}

/// A 64-bit bitvector-sorted Z3 expression: exact two's-complement machine
/// arithmetic. `sdiv`/`srem` truncate toward zero, matching Rust's (and
/// `codira_mir::fold_op`'s) `/`/`%`; `shl`/`ashr`/`lshr` with a shift
/// amount `>= 64` produce Z3's defined total-function results (0 or the
/// sign-fill), *not* an error -- callers modeling `fold_op`'s checked
/// shifts must constrain the amount themselves.
#[derive(Clone, Copy)]
pub struct BvExpr<'ctx> {
    ctx: &'ctx Context,
    raw: ffi::Z3_ast,
}

impl<'ctx> BvExpr<'ctx> {
    // clippy::should_implement_trait fires on `add`/`sub`/`mul`/`bitand`
    // and friends. Implementing `std::ops` here would be wrong: these do
    // not compute a value, they *build an SMT term* in a solver context,
    // and they take `self` by value while borrowing the context. The
    // names deliberately mirror the operations they construct.
    #![allow(clippy::should_implement_trait)]

    fn binop(
        self,
        other: BvExpr<'ctx>,
        f: unsafe extern "C" fn(ffi::Z3_context, ffi::Z3_ast, ffi::Z3_ast) -> ffi::Z3_ast,
    ) -> BvExpr<'ctx> {
        BvExpr {
            ctx: self.ctx,
            raw: unsafe { f(self.ctx.raw, self.raw, other.raw) },
        }
    }

    fn cmp(
        self,
        other: BvExpr<'ctx>,
        f: unsafe extern "C" fn(ffi::Z3_context, ffi::Z3_ast, ffi::Z3_ast) -> ffi::Z3_ast,
    ) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { f(self.ctx.raw, self.raw, other.raw) },
        }
    }

    pub fn add(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvadd)
    }

    pub fn sub(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvsub)
    }

    pub fn mul(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvmul)
    }

    /// Signed division, truncating toward zero. Division by zero is Z3's
    /// defined total-function result (an unconstrained value), not an
    /// error -- see the type-level doc for the caller's obligations.
    pub fn sdiv(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvsdiv)
    }

    /// Signed remainder (sign follows the dividend), matching Rust `%`.
    pub fn srem(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvsrem)
    }

    pub fn neg(self) -> BvExpr<'ctx> {
        BvExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_bvneg(self.ctx.raw, self.raw) },
        }
    }

    pub fn bitand(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvand)
    }

    pub fn bitor(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvor)
    }

    pub fn bitxor(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvxor)
    }

    pub fn bitnot(self) -> BvExpr<'ctx> {
        BvExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_bvnot(self.ctx.raw, self.raw) },
        }
    }

    pub fn shl(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvshl)
    }

    /// Arithmetic (sign-propagating) right shift, matching `i64 >>`.
    pub fn ashr(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvashr)
    }

    /// Logical (zero-filling) right shift.
    pub fn lshr(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvlshr)
    }

    pub fn slt(self, other: BvExpr<'ctx>) -> BoolExpr<'ctx> {
        self.cmp(other, ffi::Z3_mk_bvslt)
    }

    pub fn sle(self, other: BvExpr<'ctx>) -> BoolExpr<'ctx> {
        self.cmp(other, ffi::Z3_mk_bvsle)
    }

    pub fn sgt(self, other: BvExpr<'ctx>) -> BoolExpr<'ctx> {
        self.cmp(other, ffi::Z3_mk_bvsgt)
    }

    pub fn sge(self, other: BvExpr<'ctx>) -> BoolExpr<'ctx> {
        self.cmp(other, ffi::Z3_mk_bvsge)
    }

    pub fn eq(self, other: BvExpr<'ctx>) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_eq(self.ctx.raw, self.raw, other.raw) },
        }
    }

    /// Whether `self` and `other` are the same term. Z3 hash-conses its
    /// terms, so structural equality is pointer equality; this is a cheap
    /// syntactic test, not a semantic one.
    pub fn is_identical(self, other: BvExpr<'ctx>) -> bool {
        self.raw == other.raw
    }

    /// The width of this bitvector, in bits.
    pub fn width(self) -> u32 {
        unsafe {
            let sort = ffi::Z3_get_sort(self.ctx.raw, self.raw);
            ffi::Z3_get_bv_sort_size(self.ctx.raw, sort)
        }
    }

    pub fn ult(self, other: BvExpr<'ctx>) -> BoolExpr<'ctx> {
        self.cmp(other, ffi::Z3_mk_bvult)
    }

    pub fn ule(self, other: BvExpr<'ctx>) -> BoolExpr<'ctx> {
        self.cmp(other, ffi::Z3_mk_bvule)
    }

    pub fn ugt(self, other: BvExpr<'ctx>) -> BoolExpr<'ctx> {
        self.cmp(other, ffi::Z3_mk_bvugt)
    }

    pub fn uge(self, other: BvExpr<'ctx>) -> BoolExpr<'ctx> {
        self.cmp(other, ffi::Z3_mk_bvuge)
    }

    /// Unsigned division. As with [`BvExpr::sdiv`], a zero divisor yields
    /// Z3's total-function result, not an error.
    pub fn udiv(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvudiv)
    }

    /// Unsigned remainder.
    pub fn urem(self, other: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(other, ffi::Z3_mk_bvurem)
    }

    /// Widens by `extra` bits, copying the sign bit.
    pub fn sign_ext(self, extra: u32) -> BvExpr<'ctx> {
        BvExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_sign_ext(self.ctx.raw, extra, self.raw) },
        }
    }

    /// Widens by `extra` bits of zeros.
    pub fn zero_ext(self, extra: u32) -> BvExpr<'ctx> {
        BvExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_zero_ext(self.ctx.raw, extra, self.raw) },
        }
    }

    /// `self` followed by `low`: a bit-vector as wide as both together, with
    /// `self` in the high bits.
    pub fn concat(self, low: BvExpr<'ctx>) -> BvExpr<'ctx> {
        self.binop(low, ffi::Z3_mk_concat)
    }

    /// Bits `high..=low` of this bitvector, as a `high - low + 1`-bit value.
    pub fn extract(self, high: u32, low: u32) -> BvExpr<'ctx> {
        BvExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_extract(self.ctx.raw, high, low, self.raw) },
        }
    }

    /// Converts to `width` bits the way an integer cast does: truncation
    /// keeps the low bits, widening sign- or zero-extends according to the
    /// *source* type's signedness (`source_signed`).
    pub fn resize(self, width: u32, source_signed: bool) -> BvExpr<'ctx> {
        let current = self.width();
        match current.cmp(&width) {
            std::cmp::Ordering::Equal => self,
            std::cmp::Ordering::Greater => self.extract(width - 1, 0),
            std::cmp::Ordering::Less if source_signed => self.sign_ext(width - current),
            std::cmp::Ordering::Less => self.zero_ext(width - current),
        }
    }

    /// True when `self + other` does not overflow (signed or unsigned).
    pub fn add_no_overflow(self, other: BvExpr<'ctx>, signed: bool) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_bvadd_no_overflow(self.ctx.raw, self.raw, other.raw, signed) },
        }
    }

    /// True when `self - other` does not underflow (signed or unsigned).
    pub fn sub_no_underflow(self, other: BvExpr<'ctx>, signed: bool) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe {
                ffi::Z3_mk_bvsub_no_underflow(self.ctx.raw, self.raw, other.raw, signed)
            },
        }
    }

    /// True when `self * other` does not overflow (signed or unsigned).
    pub fn mul_no_overflow(self, other: BvExpr<'ctx>, signed: bool) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_bvmul_no_overflow(self.ctx.raw, self.raw, other.raw, signed) },
        }
    }
}

/// A boolean-sorted Z3 expression, borrowed from the [`Context`] that
/// created it.
#[derive(Clone, Copy)]
pub struct BoolExpr<'ctx> {
    ctx: &'ctx Context,
    raw: ffi::Z3_ast,
}

impl<'ctx> BoolExpr<'ctx> {
    // clippy::should_implement_trait fires on `add`/`sub`/`mul`/`bitand`
    // and friends. Implementing `std::ops` here would be wrong: these do
    // not compute a value, they *build an SMT term* in a solver context,
    // and they take `self` by value while borrowing the context. The
    // names deliberately mirror the operations they construct.
    #![allow(clippy::should_implement_trait)]

    /// Whether `self` and `other` are the same term (see
    /// [`BvExpr::is_identical`]).
    pub fn is_identical(self, other: BoolExpr<'ctx>) -> bool {
        self.raw == other.raw
    }

    /// This formula with each constant on the left of a pair replaced by the
    /// term on the right, simultaneously.
    pub fn substitute(self, pairs: &[(Term<'ctx>, Term<'ctx>)]) -> BoolExpr<'ctx> {
        match Term::Bool(self).substitute(pairs) {
            Term::Bool(b) => b,
            _ => unreachable!("substitution keeps the sort"),
        }
    }

    pub fn not(self) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_not(self.ctx.raw, self.raw) },
        }
    }

    pub fn and(self, other: BoolExpr<'ctx>) -> BoolExpr<'ctx> {
        let args = [self.raw, other.raw];
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_and(self.ctx.raw, 2, args.as_ptr()) },
        }
    }

    pub fn or(self, other: BoolExpr<'ctx>) -> BoolExpr<'ctx> {
        let args = [self.raw, other.raw];
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_or(self.ctx.raw, 2, args.as_ptr()) },
        }
    }

    pub fn implies(self, other: BoolExpr<'ctx>) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_implies(self.ctx.raw, self.raw, other.raw) },
        }
    }

    pub fn xor(self, other: BoolExpr<'ctx>) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_xor(self.ctx.raw, self.raw, other.raw) },
        }
    }

    /// Bidirectional implication (`<=>`): boolean equality as a single
    /// binder instead of two `implies` calls.
    pub fn iff(self, other: BoolExpr<'ctx>) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_iff(self.ctx.raw, self.raw, other.raw) },
        }
    }

    /// `if self then a else b` over integer expressions.
    pub fn ite_int(self, then: IntExpr<'ctx>, otherwise: IntExpr<'ctx>) -> IntExpr<'ctx> {
        IntExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_ite(self.ctx.raw, self.raw, then.raw, otherwise.raw) },
        }
    }

    /// `if self then a else b` over boolean expressions.
    pub fn ite_bool(self, then: BoolExpr<'ctx>, otherwise: BoolExpr<'ctx>) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_ite(self.ctx.raw, self.raw, then.raw, otherwise.raw) },
        }
    }

    /// `if self then a else b` over 64-bit bitvector expressions.
    pub fn ite_bv(self, then: BvExpr<'ctx>, otherwise: BvExpr<'ctx>) -> BvExpr<'ctx> {
        BvExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_ite(self.ctx.raw, self.raw, then.raw, otherwise.raw) },
        }
    }

    /// Combines a slice of boolean expressions with `&&`; `true` for an
    /// empty slice (the identity for conjunction).
    pub fn conjunction(ctx: &'ctx Context, exprs: &[BoolExpr<'ctx>]) -> BoolExpr<'ctx> {
        if exprs.is_empty() {
            return ctx.bool_true();
        }
        let raws: Vec<ffi::Z3_ast> = exprs.iter().map(|e| e.raw).collect();
        BoolExpr {
            ctx,
            raw: unsafe { ffi::Z3_mk_and(ctx.raw, raws.len() as c_int, raws.as_ptr()) },
        }
    }
}

/// The sort of a value a [`Term`] stands for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Sort {
    Bool,
    /// A bit-vector this many bits wide.
    Bv(u32),
    /// An array from the first sort to the second.
    Array(Box<Sort>, Box<Sort>),
}

impl Sort {
    /// An array from `width`-bit indices to `elem`.
    pub fn array_from_bv(width: u32, elem: Sort) -> Sort {
        Sort::Array(Box::new(Sort::Bv(width)), Box::new(elem))
    }
}

/// An expression of any of the sorts in [`Sort`]: what arrays hold and
/// uninterpreted functions take and return.
#[derive(Clone, Copy)]
pub enum Term<'ctx> {
    Bool(BoolExpr<'ctx>),
    Bv(BvExpr<'ctx>),
    Array(ArrExpr<'ctx>),
}

impl<'ctx> From<BoolExpr<'ctx>> for Term<'ctx> {
    fn from(value: BoolExpr<'ctx>) -> Self {
        Term::Bool(value)
    }
}

impl<'ctx> From<BvExpr<'ctx>> for Term<'ctx> {
    fn from(value: BvExpr<'ctx>) -> Self {
        Term::Bv(value)
    }
}

impl<'ctx> From<ArrExpr<'ctx>> for Term<'ctx> {
    fn from(value: ArrExpr<'ctx>) -> Self {
        Term::Array(value)
    }
}

impl<'ctx> Term<'ctx> {
    fn raw(self) -> ffi::Z3_ast {
        match self {
            Term::Bool(b) => b.raw,
            Term::Bv(b) => b.raw,
            Term::Array(a) => a.raw,
        }
    }

    fn ctx(self) -> &'ctx Context {
        match self {
            Term::Bool(b) => b.ctx,
            Term::Bv(b) => b.ctx,
            Term::Array(a) => a.ctx,
        }
    }

    /// Wraps a raw term of a sort this crate models; `None` for any other.
    fn from_raw(ctx: &'ctx Context, raw: ffi::Z3_ast) -> Option<Term<'ctx>> {
        let kind = unsafe {
            let sort = ffi::Z3_get_sort(ctx.raw, raw);
            ffi::Z3_get_sort_kind(ctx.raw, sort)
        };
        match kind {
            ffi::Z3_BOOL_SORT => Some(Term::Bool(BoolExpr { ctx, raw })),
            ffi::Z3_BV_SORT => Some(Term::Bv(BvExpr { ctx, raw })),
            ffi::Z3_ARRAY_SORT => Some(Term::Array(ArrExpr { ctx, raw })),
            _ => None,
        }
    }

    /// The sort of this term.
    pub fn sort(self) -> Sort {
        unsafe fn read(c: ffi::Z3_context, sort: ffi::Z3_sort) -> Sort {
            match ffi::Z3_get_sort_kind(c, sort) {
                ffi::Z3_BOOL_SORT => Sort::Bool,
                ffi::Z3_BV_SORT => Sort::Bv(ffi::Z3_get_bv_sort_size(c, sort)),
                ffi::Z3_ARRAY_SORT => Sort::Array(
                    Box::new(read(c, ffi::Z3_get_array_sort_domain(c, sort))),
                    Box::new(read(c, ffi::Z3_get_array_sort_range(c, sort))),
                ),
                other => panic!("a term of unmodelled sort kind {other}"),
            }
        }
        let c = self.ctx().raw;
        unsafe { read(c, ffi::Z3_get_sort(c, self.raw())) }
    }

    /// This term with each constant on the left of a pair replaced by the
    /// term on the right (of the same sort), simultaneously.
    pub fn substitute(self, pairs: &[(Term<'ctx>, Term<'ctx>)]) -> Term<'ctx> {
        let from: Vec<ffi::Z3_ast> = pairs.iter().map(|(f, _)| f.raw()).collect();
        let to: Vec<ffi::Z3_ast> = pairs.iter().map(|(_, t)| t.raw()).collect();
        let raw = unsafe {
            ffi::Z3_substitute(
                self.ctx().raw,
                self.raw(),
                pairs.len() as std::ffi::c_uint,
                from.as_ptr(),
                to.as_ptr(),
            )
        };
        Term::from_raw(self.ctx(), raw).expect("substitution keeps the sort")
    }

    /// `self = other`. Both must have the same sort.
    pub fn eq(self, other: Term<'ctx>) -> BoolExpr<'ctx> {
        BoolExpr {
            ctx: self.ctx(),
            raw: unsafe { ffi::Z3_mk_eq(self.ctx().raw, self.raw(), other.raw()) },
        }
    }

    /// Whether `self` and `other` are the same term.
    pub fn is_identical(self, other: Term<'ctx>) -> bool {
        self.raw() == other.raw()
    }

    /// `if cond then then else otherwise`. Both branches must have the same
    /// sort.
    pub fn ite(cond: BoolExpr<'ctx>, then: Term<'ctx>, otherwise: Term<'ctx>) -> Term<'ctx> {
        let raw = unsafe { ffi::Z3_mk_ite(cond.ctx.raw, cond.raw, then.raw(), otherwise.raw()) };
        Term::from_raw(cond.ctx, raw).expect("an `ite` has its branches' sort")
    }
}

/// An array-sorted Z3 expression: a total map from indices to elements, with
/// Z3's extensional theory of arrays (`select`, `store`, equality of whole
/// arrays).
#[derive(Clone, Copy)]
pub struct ArrExpr<'ctx> {
    ctx: &'ctx Context,
    raw: ffi::Z3_ast,
}

impl<'ctx> ArrExpr<'ctx> {
    /// The element at `index`. `index` must have the array's index sort.
    pub fn select(self, index: Term<'ctx>) -> Term<'ctx> {
        let raw = unsafe { ffi::Z3_mk_select(self.ctx.raw, self.raw, index.raw()) };
        Term::from_raw(self.ctx, raw).expect("an array holds a sort this crate models")
    }

    /// This array with the element at `index` replaced by `value`.
    pub fn store(self, index: Term<'ctx>, value: Term<'ctx>) -> ArrExpr<'ctx> {
        ArrExpr {
            ctx: self.ctx,
            raw: unsafe { ffi::Z3_mk_store(self.ctx.raw, self.raw, index.raw(), value.raw()) },
        }
    }

    /// `self = other` as whole arrays (every index holds the same element).
    pub fn eq(self, other: ArrExpr<'ctx>) -> BoolExpr<'ctx> {
        Term::Array(self).eq(Term::Array(other))
    }
}

/// An uninterpreted function: Z3 knows only that equal arguments give equal
/// results. Two applications to the same arguments are the same value, which
/// is how an unchanged callee is made to behave identically in two versions
/// of a program.
#[derive(Clone)]
pub struct FuncDecl<'ctx> {
    ctx: &'ctx Context,
    raw: ffi::Z3_func_decl,
    range: Sort,
}

impl<'ctx> FuncDecl<'ctx> {
    /// `self(args)`. The arguments must have the declared sorts.
    pub fn apply(&self, args: &[Term<'ctx>]) -> Term<'ctx> {
        let raws: Vec<ffi::Z3_ast> = args.iter().map(|a| a.raw()).collect();
        let raw = unsafe {
            ffi::Z3_mk_app(
                self.ctx.raw,
                self.raw,
                raws.len() as std::ffi::c_uint,
                raws.as_ptr(),
            )
        };
        Term::from_raw(self.ctx, raw).expect("a function returns a sort this crate models")
    }

    /// The declared result sort.
    pub fn range(&self) -> &Sort {
        &self.range
    }
}

/// The linked Z3's own version string, e.g. `Z3 4.13.0.0`.
pub fn z3_version() -> String {
    // Safety: a static string owned by the library.
    unsafe {
        let text = ffi::Z3_get_full_version();
        if text.is_null() {
            String::new()
        } else {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        }
    }
}

/// The result of asking a [`Solver`] whether its current assertions are
/// satisfiable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SatResult {
    /// A satisfying assignment exists.
    Sat,
    /// No satisfying assignment exists -- the assertions are contradictory.
    Unsat,
    /// Z3 could not determine satisfiability (e.g. it timed out, or the
    /// formula falls outside the theories it can decide).
    Unknown,
}

/// Why a check came back [`SatResult::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnknownReason {
    /// The deterministic resource budget (`rlimit`) ran out before Z3
    /// reached an answer. The same query with the same budget gives the same
    /// verdict on every machine.
    ResourceLimit,
    /// Anything else Z3 reports: incomplete theory combination, a
    /// quantifier it gave up on, an interrupted search. Carries Z3's own
    /// explanation.
    Other(String),
}

impl std::fmt::Display for UnknownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnknownReason::ResourceLimit => f.write_str("resource limit"),
            UnknownReason::Other(reason) => f.write_str(reason),
        }
    }
}

/// A satisfiability verdict that, unlike [`SatResult`], says why an
/// `unknown` happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    Sat,
    Unsat,
    Unknown(UnknownReason),
}

/// A Z3 solver: an incremental set of boolean assertions that can be
/// checked for satisfiability.
pub struct Solver<'ctx> {
    ctx: &'ctx Context,
    raw: ffi::Z3_solver,
    /// Whether a wall-clock timeout was set, which makes a "canceled"
    /// verdict ambiguous (see [`Solver::reason_unknown`]).
    timeout_set: std::cell::Cell<bool>,
}

impl<'ctx> Solver<'ctx> {
    /// Adds `expr` to the solver's set of assertions.
    pub fn assert(&self, expr: BoolExpr<'ctx>) {
        unsafe { ffi::Z3_solver_assert(self.ctx.raw, self.raw, expr.raw) };
    }

    /// Checks whether the current set of assertions is satisfiable.
    pub fn check(&self) -> SatResult {
        match unsafe { ffi::Z3_solver_check(self.ctx.raw, self.raw) } {
            ffi::Z3_L_TRUE => SatResult::Sat,
            ffi::Z3_L_FALSE => SatResult::Unsat,
            _ => SatResult::Unknown,
        }
    }

    /// A human-readable dump of the solver's current assertions, useful for
    /// debugging a feasibility check.
    pub fn to_debug_string(&self) -> String {
        unsafe {
            let s = ffi::Z3_solver_to_string(self.ctx.raw, self.raw);
            if s.is_null() {
                String::new()
            } else {
                CStr::from_ptr(s).to_string_lossy().into_owned()
            }
        }
    }

    /// Pushes a backtracking point: assertions added after this call are
    /// retracted by the matching [`Solver::pop`]. Makes counterexample
    /// probing incremental instead of solver-per-probe.
    pub fn push(&self) {
        unsafe { ffi::Z3_solver_push(self.ctx.raw, self.raw) };
    }

    /// Pops `n` backtracking points, retracting the assertions made since
    /// the matching pushes.
    pub fn pop(&self, n: u32) {
        unsafe { ffi::Z3_solver_pop(self.ctx.raw, self.raw, n) };
    }

    /// Bounds each subsequent `check` to at most `ms` milliseconds; a
    /// check that exceeds it comes back [`SatResult::Unknown`], so callers
    /// can budget validation cost instead of risking an unbounded solve.
    pub fn set_timeout_ms(&self, ms: u32) {
        self.timeout_set.set(true);
        let key = CString::new("timeout").unwrap();
        unsafe {
            let params = ffi::Z3_mk_params(self.ctx.raw);
            ffi::Z3_params_inc_ref(self.ctx.raw, params);
            let symbol = ffi::Z3_mk_string_symbol(self.ctx.raw, key.as_ptr());
            ffi::Z3_params_set_uint(self.ctx.raw, params, symbol, ms);
            ffi::Z3_solver_set_params(self.ctx.raw, self.raw, params);
            ffi::Z3_params_dec_ref(self.ctx.raw, params);
        }
    }

    /// Bounds each subsequent `check` to `rlimit` Z3 resource units -- the
    /// deterministic budget (see [`DEFAULT_RLIMIT`]). Exhausting it yields
    /// [`SatResult::Unknown`] with [`UnknownReason::ResourceLimit`].
    pub fn set_rlimit(&self, rlimit: u32) {
        let key = CString::new("rlimit").unwrap();
        unsafe {
            let params = ffi::Z3_mk_params(self.ctx.raw);
            ffi::Z3_params_inc_ref(self.ctx.raw, params);
            let symbol = ffi::Z3_mk_string_symbol(self.ctx.raw, key.as_ptr());
            ffi::Z3_params_set_uint(self.ctx.raw, params, symbol, rlimit);
            ffi::Z3_solver_set_params(self.ctx.raw, self.raw, params);
            ffi::Z3_params_dec_ref(self.ctx.raw, params);
        }
    }

    /// Z3's explanation of the last `unknown` verdict.
    pub fn reason_unknown(&self) -> UnknownReason {
        let text = unsafe {
            let s = ffi::Z3_solver_get_reason_unknown(self.ctx.raw, self.raw);
            if s.is_null() {
                String::new()
            } else {
                CStr::from_ptr(s).to_string_lossy().into_owned()
            }
        };
        // Z3 spells this "max. resource limit exceeded" or "(resource
        // limits reached)" depending on which component gave up first, and
        // plain "canceled" when the limit trips inside a tactic that only
        // sees the cancellation. Without a wall-clock timeout (which the
        // compiler never sets) nothing else cancels a check, so "canceled"
        // is the budget too.
        let budget = text.contains("resource limit")
            || text.contains("rlimit")
            || (text.contains("canceled") && !self.timeout_set.get());
        if budget {
            UnknownReason::ResourceLimit
        } else {
            UnknownReason::Other(text)
        }
    }

    /// Checks the current assertions, saying why if the answer is unknown.
    pub fn check_detailed(&self) -> CheckOutcome {
        match self.check() {
            SatResult::Sat => CheckOutcome::Sat,
            SatResult::Unsat => CheckOutcome::Unsat,
            SatResult::Unknown => CheckOutcome::Unknown(self.reason_unknown()),
        }
    }

    /// Retrieves the satisfying model after a [`SatResult::Sat`] check.
    /// Returns `None` if no model is available (the last check was not
    /// SAT, or nothing was checked yet).
    pub fn model(&self) -> Option<Model<'ctx>> {
        let raw = unsafe { ffi::Z3_solver_get_model(self.ctx.raw, self.raw) };
        if raw.0.is_null() {
            return None;
        }
        unsafe { ffi::Z3_model_inc_ref(self.ctx.raw, raw) };
        Some(Model { ctx: self.ctx, raw })
    }
}

impl Drop for Solver<'_> {
    fn drop(&mut self) {
        unsafe { ffi::Z3_solver_dec_ref(self.ctx.raw, self.raw) };
    }
}

/// A satisfying assignment extracted from a SAT [`Solver`] -- the direct
/// counterexample a translation-validation refutation reports, replacing
/// bounded probing.
pub struct Model<'ctx> {
    ctx: &'ctx Context,
    raw: ffi::Z3_model,
}

impl<'ctx> Model<'ctx> {
    fn eval_raw(&self, ast: ffi::Z3_ast) -> Option<ffi::Z3_ast> {
        let mut out = ffi::Z3_ast(std::ptr::null_mut());
        // `model_completion = true`: variables the model doesn't constrain
        // get an arbitrary-but-consistent value instead of failing, which
        // is exactly what a counterexample witness wants.
        let ok = unsafe { ffi::Z3_model_eval(self.ctx.raw, self.raw, ast, true, &mut out) };
        (ok && !out.0.is_null()).then_some(out)
    }

    /// The model's value for an integer expression, if it fits in `i64`.
    pub fn eval_int(&self, expr: IntExpr<'ctx>) -> Option<i64> {
        let value = self.eval_raw(expr.raw)?;
        let mut out: i64 = 0;
        unsafe { ffi::Z3_get_numeral_int64(self.ctx.raw, value, &mut out) }.then_some(out)
    }

    /// The model's value for a 64-bit bitvector expression, as the
    /// two's-complement `i64` it encodes.
    pub fn eval_bv64(&self, expr: BvExpr<'ctx>) -> Option<i64> {
        let value = self.eval_raw(expr.raw)?;
        // BV numerals are unsigned in Z3's view; a value with the top bit
        // set only extracts via the u64 accessor.
        let mut out: u64 = 0;
        unsafe { ffi::Z3_get_numeral_uint64(self.ctx.raw, value, &mut out) }.then_some(out as i64)
    }

    /// The model's value for a bitvector of any width up to 128 bits, as its
    /// raw (unsigned) bit pattern. Callers reinterpret it as signed when the
    /// source type is.
    pub fn eval_bv_bits(&self, expr: BvExpr<'ctx>) -> Option<u128> {
        let value = self.eval_raw(expr.raw)?;
        let mut out: u64 = 0;
        if unsafe { ffi::Z3_get_numeral_uint64(self.ctx.raw, value, &mut out) } {
            return Some(u128::from(out));
        }
        // Wider than 64 bits: only the decimal rendering reaches it.
        let text = unsafe {
            let s = ffi::Z3_get_numeral_string(self.ctx.raw, value);
            if s.is_null() {
                return None;
            }
            CStr::from_ptr(s).to_string_lossy().into_owned()
        };
        text.parse().ok()
    }

    /// The model's value for a boolean expression.
    pub fn eval_bool(&self, expr: BoolExpr<'ctx>) -> Option<bool> {
        let value = self.eval_raw(expr.raw)?;
        match unsafe { ffi::Z3_get_bool_value(self.ctx.raw, value) } {
            ffi::Z3_L_TRUE => Some(true),
            ffi::Z3_L_FALSE => Some(false),
            _ => None,
        }
    }
}

impl Drop for Model<'_> {
    fn drop(&mut self) {
        unsafe { ffi::Z3_model_dec_ref(self.ctx.raw, self.raw) };
    }
}

/// Checks whether `predicate` is satisfiable: whether there exists *some*
/// assignment to its free variables that makes it true.
///
/// This is exactly the check a refinement type `Type { binder | predicate }`
/// needs to be well-formed in the first place: an *unsatisfiable*
/// refinement (e.g. `i32 { x | x > 0 && x < 0 }`) describes an empty type
/// nothing can ever inhabit, which is almost certainly a mistake at the
/// declaration site rather than an intentional "no valid values" type.
pub fn is_satisfiable<'ctx>(ctx: &'ctx Context, predicate: BoolExpr<'ctx>) -> SatResult {
    let solver = ctx.solver();
    solver.assert(predicate);
    solver.check()
}

/// The verdict of [`implies_detailed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Implication {
    /// Every assignment satisfying the premise satisfies the conclusion.
    Proven,
    /// Some assignment satisfies the premise but not the conclusion.
    Refuted,
    /// Z3 could not decide (see [`UnknownReason`]).
    Unknown(UnknownReason),
}

/// [`implies`], keeping "could not decide" apart from "refuted".
pub fn implies_detailed<'ctx>(
    ctx: &'ctx Context,
    premise: BoolExpr<'ctx>,
    conclusion: BoolExpr<'ctx>,
) -> Implication {
    let solver = ctx.solver();
    solver.assert(premise);
    solver.assert(conclusion.not());
    match solver.check_detailed() {
        CheckOutcome::Unsat => Implication::Proven,
        CheckOutcome::Sat => Implication::Refuted,
        CheckOutcome::Unknown(reason) => Implication::Unknown(reason),
    }
}

/// Checks whether `premise` *implies* `conclusion`: whether every
/// assignment satisfying `premise` also satisfies `conclusion`.
///
/// This is the check a healing strategy's precondition needs: e.g. "is it
/// always safe to retry with `x` halved?" reduces to proving the
/// original refinement predicate implies the retried call's precondition.
/// Implication is checked the standard way for a decision procedure that
/// only directly answers satisfiability queries: `premise => conclusion`
/// is valid exactly when `premise && !conclusion` is unsatisfiable.
///
/// Returns `false` when Z3 cannot decide within the budget: "not proven",
/// never "proven". Callers that must tell the two apart use
/// [`implies_detailed`].
pub fn implies<'ctx>(
    ctx: &'ctx Context,
    premise: BoolExpr<'ctx>,
    conclusion: BoolExpr<'ctx>,
) -> bool {
    let solver = ctx.solver();
    solver.assert(premise);
    solver.assert(conclusion.not());
    solver.check() == SatResult::Unsat
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn satisfiable_predicate_is_sat() {
        let ctx = Context::new();
        let x = ctx.int_var("x");
        // x > 0 && x < 100 -- clearly satisfiable (e.g. x = 1).
        let pred = x.gt(ctx.int_lit(0)).and(x.lt(ctx.int_lit(100)));
        assert_eq!(is_satisfiable(&ctx, pred), SatResult::Sat);
    }

    #[test]
    fn contradictory_predicate_is_unsat() {
        let ctx = Context::new();
        let x = ctx.int_var("x");
        // x > 0 && x < 0 -- an empty refinement type, must be UNSAT.
        let pred = x.gt(ctx.int_lit(0)).and(x.lt(ctx.int_lit(0)));
        assert_eq!(is_satisfiable(&ctx, pred), SatResult::Unsat);
    }

    #[test]
    fn tighter_bound_implies_looser_bound() {
        let ctx = Context::new();
        let x = ctx.int_var("x");
        // x > 10 && x < 20  =>  x > 0
        let premise = x.gt(ctx.int_lit(10)).and(x.lt(ctx.int_lit(20)));
        let conclusion = x.gt(ctx.int_lit(0));
        assert!(implies(&ctx, premise, conclusion));
    }

    #[test]
    fn unrelated_bound_does_not_imply() {
        let ctx = Context::new();
        let x = ctx.int_var("x");
        // x > 0  does NOT imply  x > 100 (e.g. x = 1 is a counterexample)
        let premise = x.gt(ctx.int_lit(0));
        let conclusion = x.gt(ctx.int_lit(100));
        assert!(!implies(&ctx, premise, conclusion));
    }

    #[test]
    fn arithmetic_reasoning() {
        let ctx = Context::new();
        let x = ctx.int_var("x");
        let y = ctx.int_var("y");
        // x > 0 && y > 0  =>  x + y > 0
        let premise = x.gt(ctx.int_lit(0)).and(y.gt(ctx.int_lit(0)));
        let conclusion = x.add(y).gt(ctx.int_lit(0));
        assert!(implies(&ctx, premise, conclusion));
    }

    #[test]
    fn bitvector_arithmetic_wraps_exactly() {
        let ctx = Context::new();
        // i64::MAX + 1 == i64::MIN under two's complement -- the exact
        // wrapping semantics `codira_mir::fold_op` defines, which the
        // unbounded Int theory cannot express.
        let max = ctx.bv64_lit(i64::MAX);
        let one = ctx.bv64_lit(1);
        let min = ctx.bv64_lit(i64::MIN);
        assert_eq!(is_satisfiable(&ctx, max.add(one).eq(min)), SatResult::Sat);
        // ...and it's not just satisfiable, it's forced:
        let solver = ctx.solver();
        solver.assert(max.add(one).eq(min).not());
        assert_eq!(solver.check(), SatResult::Unsat);
    }

    #[test]
    fn bitvector_division_truncates_toward_zero() {
        let ctx = Context::new();
        // -7 / 2 == -3 (truncation, Rust semantics), not -4 (floor).
        let solver = ctx.solver();
        solver.assert(
            ctx.bv64_lit(-7)
                .sdiv(ctx.bv64_lit(2))
                .eq(ctx.bv64_lit(-3))
                .not(),
        );
        assert_eq!(solver.check(), SatResult::Unsat);
    }

    #[test]
    fn bitvector_shift_matches_mul() {
        let ctx = Context::new();
        // forall x: x << 3 == x * 8 -- exact even where x*8 overflows.
        let x = ctx.bv64_var("x");
        let solver = ctx.solver();
        solver.assert(x.shl(ctx.bv64_lit(3)).eq(x.mul(ctx.bv64_lit(8))).not());
        assert_eq!(solver.check(), SatResult::Unsat);
    }

    #[test]
    fn ite_selects_branches() {
        let ctx = Context::new();
        let c = ctx.bool_var("c");
        let picked = c.ite_int(ctx.int_lit(1), ctx.int_lit(2));
        // c => picked == 1
        assert!(implies(&ctx, c, picked.eq(ctx.int_lit(1))));
        // !c => picked == 2
        assert!(implies(&ctx, c.not(), picked.eq(ctx.int_lit(2))));
    }

    #[test]
    fn model_extraction_yields_witness() {
        let ctx = Context::new();
        let x = ctx.int_var("x");
        let solver = ctx.solver();
        // x > 41 && x < 43 has the unique witness x = 42.
        solver.assert(x.gt(ctx.int_lit(41)).and(x.lt(ctx.int_lit(43))));
        assert_eq!(solver.check(), SatResult::Sat);
        let model = solver.model().expect("SAT check must produce a model");
        assert_eq!(model.eval_int(x), Some(42));
    }

    #[test]
    fn model_extraction_bv_negative_witness() {
        let ctx = Context::new();
        let x = ctx.bv64_var("x");
        let solver = ctx.solver();
        // Unique witness x = -1 (top bit set: exercises the u64 accessor).
        solver.assert(x.eq(ctx.bv64_lit(-1)));
        assert_eq!(solver.check(), SatResult::Sat);
        let model = solver.model().expect("SAT check must produce a model");
        assert_eq!(model.eval_bv64(x), Some(-1));
        assert_eq!(
            model.eval_bool(x.slt(ctx.bv64_lit(0))),
            Some(true),
            "model evaluation works on derived expressions too"
        );
    }

    #[test]
    fn push_pop_retracts_assertions() {
        let ctx = Context::new();
        let x = ctx.int_var("x");
        let solver = ctx.solver();
        solver.assert(x.gt(ctx.int_lit(0)));
        solver.push();
        solver.assert(x.lt(ctx.int_lit(0)));
        assert_eq!(solver.check(), SatResult::Unsat);
        solver.pop(1);
        assert_eq!(solver.check(), SatResult::Sat, "popped back to x > 0 alone");
    }

    #[test]
    fn timeout_is_settable() {
        // Only checks the parameter plumbing doesn't crash and a trivial
        // query still solves inside a generous budget -- forcing an actual
        // timeout would make the test slow and flaky by design.
        let ctx = Context::new();
        let solver = ctx.solver();
        solver.set_timeout_ms(10_000);
        solver.assert(ctx.bool_true());
        assert_eq!(solver.check(), SatResult::Sat);
    }

    #[test]
    fn narrow_bitvectors_wrap_at_their_own_width() {
        let ctx = Context::new();
        // i8: 127 + 1 == -128.
        let solver = ctx.solver();
        let max = ctx.bv_lit(127, 8);
        let min = ctx.bv_lit(-128i128 as u128, 8);
        solver.assert(max.add(ctx.bv_lit(1, 8)).eq(min).not());
        assert_eq!(solver.check(), SatResult::Unsat);
        assert_eq!(max.width(), 8);
    }

    #[test]
    fn unsigned_and_signed_comparisons_differ() {
        let ctx = Context::new();
        let all_ones = ctx.bv_lit(u128::MAX, 16);
        let zero = ctx.bv_lit(0, 16);
        // 0xFFFF is -1 signed (< 0) but 65535 unsigned (> 0).
        assert!(implies(&ctx, ctx.bool_true(), all_ones.slt(zero)));
        assert!(implies(&ctx, ctx.bool_true(), all_ones.ugt(zero)));
    }

    #[test]
    fn resize_follows_source_signedness() {
        let ctx = Context::new();
        let minus_one = ctx.bv_lit(0xFF, 8);
        let solver = ctx.solver();
        assert_eq!(solver.check(), SatResult::Sat);
        let model = solver.model().unwrap();
        assert_eq!(
            model.eval_bv_bits(minus_one.resize(32, true)),
            Some(0xFFFF_FFFF)
        );
        assert_eq!(model.eval_bv_bits(minus_one.resize(32, false)), Some(0xFF));
        assert_eq!(
            model.eval_bv_bits(ctx.bv_lit(0x1234, 16).resize(8, false)),
            Some(0x34)
        );
    }

    #[test]
    fn wide_bitvector_model_values_survive_extraction() {
        let ctx = Context::new();
        let x = ctx.bv_var("x", 128);
        let solver = ctx.solver();
        let big = 1u128 << 100;
        solver.assert(x.eq(ctx.bv_lit(big, 128)));
        assert_eq!(solver.check(), SatResult::Sat);
        assert_eq!(solver.model().unwrap().eval_bv_bits(x), Some(big));
    }

    #[test]
    fn overflow_predicates() {
        let ctx = Context::new();
        let x = ctx.bv_var("x", 32);
        // x + 1 overflows (signed) exactly when x == i32::MAX.
        let solver = ctx.solver();
        solver.assert(x.add_no_overflow(ctx.bv_lit(1, 32), true).not());
        assert_eq!(solver.check(), SatResult::Sat);
        let model = solver.model().unwrap();
        assert_eq!(model.eval_bv_bits(x), Some(i32::MAX as u128));
    }

    /// The deterministic budget: a query that cannot be answered inside it
    /// is `unknown` with the resource limit named as the reason -- never a
    /// proof -- and asking again with the same budget gives the same answer.
    #[test]
    fn rlimit_exhaustion_is_unknown_not_a_proof() {
        let ctx = Context::new();
        ctx.set_rlimit(200);
        // Factoring a 64-bit semiprime with bit-vector multiplication is far
        // beyond 200 resource units.
        let x = ctx.bv_var("x", 64);
        let y = ctx.bv_var("y", 64);
        let n = ctx.bv_lit(4_294_967_291u128 * 4_294_967_279u128, 64);
        let one = ctx.bv_lit(1, 64);
        let query = x
            .mul(y)
            .eq(n)
            .and(x.ugt(one))
            .and(y.ugt(one))
            .and(x.mul_no_overflow(y, false));
        for _ in 0..2 {
            let solver = ctx.solver();
            solver.assert(query);
            assert_eq!(
                solver.check_detailed(),
                CheckOutcome::Unknown(UnknownReason::ResourceLimit)
            );
        }
    }

    /// A query written as SMT-LIB2 and read back into a *different*
    /// context decides the same way, and names survive the trip.
    #[test]
    fn smtlib2_text_round_trips_into_a_fresh_context() {
        let ctx = Context::new();
        let x = ctx.bv_var("x!1", 64);
        let zero = ctx.bv_lit(0, 64);
        let premise = x.sgt(zero);
        // x > 0 implies x + 0 > 0: the negation is unsat.
        let text = ctx.to_smtlib2(&[premise], x.add(zero).sgt(zero).not());
        assert!(text.contains("(check-sat)"), "{text}");

        let other = Context::new();
        let assertions = other.parse_smtlib2(&text).unwrap();
        assert_eq!(assertions.len(), 2);
        let solver = other.solver();
        for a in &assertions {
            solver.assert(*a);
        }
        assert_eq!(solver.check_detailed(), CheckOutcome::Unsat);
        // The premise alone is satisfiable.
        let premise_only = other.solver();
        premise_only.assert(assertions[0]);
        assert_eq!(premise_only.check_detailed(), CheckOutcome::Sat);
    }

    /// Malformed text is an error value, not a call to Z3's default error
    /// handler (which would end the process).
    #[test]
    fn malformed_smtlib2_is_an_error_not_an_exit() {
        let ctx = Context::new();
        assert!(ctx.parse_smtlib2("(assert (> x 0))").is_err());
        assert!(ctx.parse_smtlib2("(assert").is_err());
        // The context is still usable afterwards.
        let ok = ctx
            .parse_smtlib2("(declare-fun b () Bool)(assert b)")
            .unwrap();
        assert_eq!(ok.len(), 1);
        // Z3 4.8 gives its full version as "4.8.12.0", later releases as
        // "Z3 4.13.0.0"; either way it carries the release's numbers.
        let version = z3_version();
        let numbers = version.trim_start_matches("Z3 ");
        assert!(
            numbers.starts_with(|c: char| c.is_ascii_digit()),
            "{version}"
        );
    }

    #[test]
    fn default_budget_is_applied_to_every_solver() {
        let ctx = Context::new();
        assert_eq!(ctx.rlimit(), DEFAULT_RLIMIT);
        let solver = ctx.solver();
        solver.assert(ctx.bool_true());
        assert_eq!(solver.check_detailed(), CheckOutcome::Sat);
    }

    #[test]
    fn division_by_zero_precondition_is_checkable() {
        // Models the exact kind of check a healing strategy would need:
        // "given what we know about `divisor` at the fault site, would a
        // retry with `divisor` clamped to at least 1 actually avoid the
        // division-by-zero that just happened?"
        let ctx = Context::new();
        let divisor = ctx.int_var("divisor");
        let clamped = ctx.int_var("clamped");
        // Precondition of the retry strategy: clamped = max(divisor, 1).
        // Encode `max` as a disjunction rather than needing an `ite`
        // builder in this crate's small API surface.
        let is_divisor = clamped.eq(divisor).and(divisor.ge(ctx.int_lit(1)));
        let is_one = clamped.eq(ctx.int_lit(1)).and(divisor.lt(ctx.int_lit(1)));
        let clamp_def = is_divisor.or(is_one);

        let premise = clamp_def;
        let conclusion = clamped.ge(ctx.int_lit(1));
        assert!(implies(&ctx, premise, conclusion));
    }

    #[test]
    fn arrays_obey_read_over_write_and_extensionality() {
        let ctx = Context::new();
        let index = Sort::Bv(8);
        let elem = Sort::Bv(16);
        let heap = ctx.array_var("heap", &index, &elem);
        let (i, j) = (ctx.bv_var("i", 8), ctx.bv_var("j", 8));
        let v = ctx.bv_var("v", 16);
        let stored = heap.store(i.into(), v.into());
        // A store is visible at its own index ...
        let Term::Bv(read) = stored.select(i.into()) else {
            panic!("a bit-vector array holds bit-vectors")
        };
        assert!(implies(&ctx, ctx.bool_true(), read.eq(v)));
        // ... and invisible elsewhere ...
        let Term::Bv(other) = stored.select(j.into()) else {
            panic!("a bit-vector array holds bit-vectors")
        };
        let Term::Bv(before) = heap.select(j.into()) else {
            panic!("a bit-vector array holds bit-vectors")
        };
        assert!(implies(&ctx, i.eq(j).not(), other.eq(before)));
        // ... and overwriting with the old value changes nothing.
        let Term::Bv(old) = heap.select(i.into()) else {
            panic!("a bit-vector array holds bit-vectors")
        };
        let same = heap.store(i.into(), old.into());
        assert!(implies(&ctx, ctx.bool_true(), same.eq(heap)));
        // Two different writes are different arrays.
        let other_write = heap.store(i.into(), v.add(ctx.bv_lit(1, 16)).into());
        assert!(!implies(&ctx, ctx.bool_true(), stored.eq(other_write)));
    }

    #[test]
    fn uninterpreted_functions_are_functions_and_nothing_more() {
        let ctx = Context::new();
        let f = ctx.func("f", &[Sort::Bv(32), Sort::Bool], &Sort::Bv(32));
        let (x, y) = (ctx.bv_var("x", 32), ctx.bv_var("y", 32));
        let b = ctx.bool_var("b");
        let (Term::Bv(fx), Term::Bv(fy)) = (
            f.apply(&[x.into(), b.into()]),
            f.apply(&[y.into(), b.into()]),
        ) else {
            panic!("f returns a bit-vector")
        };
        // Equal arguments, equal results ...
        assert!(implies(&ctx, x.eq(y), fx.eq(fy)));
        // ... and different arguments prove nothing.
        assert!(!implies(&ctx, ctx.bool_true(), fx.eq(fy)));
        // Declaring it again gives the same function.
        let again = ctx.func("f", &[Sort::Bv(32), Sort::Bool], &Sort::Bv(32));
        let Term::Bv(fx2) = again.apply(&[x.into(), b.into()]) else {
            panic!("f returns a bit-vector")
        };
        assert!(implies(&ctx, ctx.bool_true(), fx.eq(fx2)));
    }

    #[test]
    fn concatenation_and_scalar_terms_round_trip_through_smtlib2() {
        let ctx = Context::new();
        let hi = ctx.bv_var("hi", 8);
        let lo = ctx.bv_var("lo", 8);
        let pair = hi.concat(lo);
        assert_eq!(pair.width(), 16);
        assert!(implies(
            &ctx,
            ctx.bool_true(),
            pair.extract(15, 8).eq(hi).and(pair.extract(7, 0).eq(lo))
        ));
        // A query over an array and an uninterpreted function is plain
        // SMT-LIB2 that a fresh context decides the same way.
        let heap = ctx.array_var("heap", &Sort::Bv(8), &Sort::Bv(8));
        let f = ctx.func(
            "g",
            &[Sort::Array(Box::new(Sort::Bv(8)), Box::new(Sort::Bv(8)))],
            &Sort::Bv(8),
        );
        let (Term::Bv(a), Term::Bv(b)) = (
            f.apply(&[heap.into()]),
            f.apply(&[heap
                .store(hi.into(), lo.into())
                .store(hi.into(), heap.select(hi.into()))
                .into()]),
        ) else {
            panic!("g returns a bit-vector")
        };
        let text = ctx.to_smtlib2(&[ctx.bool_true()], a.eq(b).not());
        let fresh = Context::new();
        let parsed = fresh.parse_smtlib2(&text).unwrap();
        let solver = fresh.solver();
        for assertion in parsed {
            solver.assert(assertion);
        }
        assert_eq!(solver.check(), SatResult::Unsat);
    }
}
