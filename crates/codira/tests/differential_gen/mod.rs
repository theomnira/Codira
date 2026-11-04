//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - A seeded generator of random, well-typed Codira programs inside the
//!   scalar core that both code generators compile: the LLVM backend
//!   (`codira run`) and the Cranelift backend (`codira run --jit`).
//! - Every program is free of undefined behaviour by construction, so any
//!   difference in what the two backends print is a bug in one of them (or in
//!   a stage they share only partly), never a licence the language gives them:
//!   - integer `+`, `-`, `*`, unary `-` and casts wrap or truncate, which the
//!     language defines and both backends implement;
//!   - a shift amount is masked to `0..width`, because shifting by the width
//!     or more is undefined natively (`agents/USING_CODIRA.md`);
//!   - a divisor is forced odd, and for a signed type also non-negative
//!     (`(d & MAX) | 1`), so it is never zero and never `-1`: integer division
//!     is not checked by the generated code and `0` or `MIN / -1` would be
//!     undefined behaviour;
//!   - float-to-integer casts saturate (`NaN` gives 0) in both backends, as
//!     the language defines them;
//!   - functions only call functions generated before them, so there is no
//!     recursion, and every loop runs a bounded counter that its body never
//!     assigns, so every program terminates. A cost estimate bounds the
//!     dynamic work of each function, so no program runs for long.
//! - The subset is exactly what `crates/codira_cranelift/src/lib.rs` compiles:
//!   integers of 8 to 64 bits, `f32`/`f64`, `bool`; `let`/`var`, assignment
//!   and compound assignment; arithmetic, bitwise, shift, comparison and
//!   logical operators; casts; `if`/`else` as a statement and as a value;
//!   `while`; `loop` with `break` and with `break value`; `return`; calls; and
//!   `println` of scalars and string literals. `for` and `continue` are not
//!   generated: the Cranelift backend refuses both (they reach the catch-all
//!   "this kind of expression"), and a refused program falls back to LLVM,
//!   which would compare LLVM with itself.
//! - Floats are never printed directly: the two backends format them
//!   differently today (see `known_divergence_float_printing` in
//!   `differential.rs`).
//!   Generated programs print `(x as i64)` and `((x * 4096.0) as i64)` of
//!   each float result instead, which still compares the float arithmetic.
//! - Three more constructs are left out by default because of known
//!   divergences, each behind a `Config` switch so they can be turned back on
//!   once fixed: float-to-8/16-bit casts (`float_to_narrow_int`), calls in
//!   `println` arguments (`calls_in_print_args`), and printing from a
//!   function without parameters (`print_in_constant_functions`).

use std::fmt::Write as _;

/// `SplitMix64`: deterministic, no dependencies, and good enough to drive a
/// generator.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`; `n` must not be 0.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// True with probability `percent` / 100.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[usize::try_from(self.below(items.len() as u64)).unwrap()]
    }
}

/// A scalar type of the generated programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Bool,
}

const INTS: [Ty; 8] = [
    Ty::I8,
    Ty::I16,
    Ty::I32,
    Ty::I64,
    Ty::U8,
    Ty::U16,
    Ty::U32,
    Ty::U64,
];
const FLOATS: [Ty; 2] = [Ty::F32, Ty::F64];

impl Ty {
    pub fn name(self) -> &'static str {
        match self {
            Ty::I8 => "i8",
            Ty::I16 => "i16",
            Ty::I32 => "i32",
            Ty::I64 => "i64",
            Ty::U8 => "u8",
            Ty::U16 => "u16",
            Ty::U32 => "u32",
            Ty::U64 => "u64",
            Ty::F32 => "f32",
            Ty::F64 => "f64",
            Ty::Bool => "bool",
        }
    }

    fn bits(self) -> u32 {
        match self {
            Ty::I8 | Ty::U8 => 8,
            Ty::I16 | Ty::U16 => 16,
            Ty::I32 | Ty::U32 | Ty::F32 => 32,
            Ty::I64 | Ty::U64 | Ty::F64 => 64,
            Ty::Bool => 1,
        }
    }

    fn is_signed(self) -> bool {
        matches!(self, Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64)
    }

    fn is_int(self) -> bool {
        INTS.contains(&self)
    }

    fn is_float(self) -> bool {
        FLOATS.contains(&self)
    }

    /// The largest value of a signed integer type, `2^(bits-1) - 1`.
    fn signed_max(self) -> u64 {
        (1u64 << (self.bits() - 1)) - 1
    }

    /// The largest value of an unsigned integer type.
    fn unsigned_max(self) -> u64 {
        if self.bits() == 64 {
            u64::MAX
        } else {
            (1u64 << self.bits()) - 1
        }
    }
}

/// What to generate.
#[derive(Debug, Clone)]
pub struct Config {
    /// Generate `f32`/`f64` values and arithmetic.
    pub floats: bool,
    /// Upper bound on the number of functions besides `main`.
    pub max_functions: usize,
    /// Upper bound on the estimated dynamic work of one function (statements
    /// executed, counting callees).
    pub cost_budget: u64,
    /// Generate casts from a float to an 8- or 16-bit integer. Off by
    /// default: the Cranelift backend crashes on them today (known
    /// the float-to-narrow-integer divergence, pinned by
    /// `known_divergence_float_to_narrow_int_cast`). A 32- or 64-bit target
    /// is still generated, and so is a narrowing of its result.
    pub float_to_narrow_int: bool,
    /// Allow calls inside the arguments of `println`. Off by default: when a
    /// called function prints too, the backends interleave the output
    /// differently (known divergence 4, pinned by
    /// `known_divergence_print_argument_order`).
    pub calls_in_print_args: bool,
    /// Let a function without parameters print (itself or through a call).
    /// Off by default: the LLVM path's Eidos constant fold replaces such a
    /// function by its constant result and drops the printing (known
    /// divergence 5, pinned by `known_divergence_eidos_fold_drops_output`).
    /// Functions without parameters are still generated, so the fold itself
    /// stays under test.
    pub print_in_constant_functions: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            floats: true,
            max_functions: 7,
            cost_budget: 20_000,
            float_to_narrow_int: false,
            calls_in_print_args: false,
            print_in_constant_functions: false,
        }
    }
}

/// One generated function, kept in pieces so a reducer can drop statements.
#[derive(Debug, Clone)]
pub struct GenFn {
    pub header: String,
    /// Top-level statements of the body, each a complete (possibly
    /// multi-line) statement.
    pub stmts: Vec<String>,
    pub tail: String,
}

/// One call from `main`: which function it calls, and its lines.
#[derive(Debug, Clone)]
pub struct MainCall {
    pub function: usize,
    pub lines: String,
}

/// A generated program.
#[derive(Debug, Clone)]
pub struct Program {
    pub seed: u64,
    pub functions: Vec<GenFn>,
    pub calls: Vec<MainCall>,
}

impl Program {
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "// Generated by crates/codira/tests/differential_gen, seed {}.\n",
            self.seed
        );
        for function in &self.functions {
            out.push_str(&function.header);
            out.push_str(" {\n");
            for stmt in &function.stmts {
                out.push_str(stmt);
            }
            let _ = writeln!(out, "    {}\n}}\n", function.tail);
        }
        out.push_str("public func main() {\n    var h: u64 = 14695981039346656037u64\n");
        for call in &self.calls {
            out.push_str(&call.lines);
        }
        out.push_str("    println(\"checksum \", h)\n}\n");
        out
    }
}

#[derive(Debug, Clone)]
struct Sig {
    name: String,
    params: Vec<Ty>,
    ret: Ty,
    cost: u64,
    /// Prints, itself or through a function it calls.
    prints: bool,
}

#[derive(Debug, Clone)]
struct Var {
    name: String,
    ty: Ty,
    /// Assignable: a `var` that is not a loop counter.
    assignable: bool,
}

struct Gen<'c> {
    rng: Rng,
    cfg: &'c Config,
    sigs: Vec<Sig>,
    scopes: Vec<Vec<Var>>,
    next_id: usize,
    loop_depth: usize,
    /// Product of the trip counts of the enclosing loops.
    mult: u64,
    /// Estimated dynamic work of the function so far.
    cost: u64,
    ret: Ty,
    /// Nesting depth of blocks, for indentation and to bound nesting.
    depth: usize,
    /// Generating the arguments of a `println` (see
    /// `Config::calls_in_print_args`).
    in_print: bool,
    /// The current function may print (see
    /// `Config::print_in_constant_functions`).
    may_print: bool,
    /// The current function prints, itself or through a call.
    prints: bool,
}

/// Generates the program for `seed`.
pub fn generate(seed: u64, cfg: &Config) -> Program {
    let mut gen = Gen {
        rng: Rng::new(seed),
        cfg,
        sigs: Vec::new(),
        scopes: Vec::new(),
        next_id: 0,
        loop_depth: 0,
        mult: 1,
        cost: 0,
        ret: Ty::I32,
        depth: 1,
        in_print: false,
        may_print: true,
        prints: false,
    };
    let count = 1 + usize::try_from(gen.rng.below(cfg.max_functions as u64)).unwrap();
    let mut functions = Vec::with_capacity(count);
    for index in 0..count {
        functions.push(gen.function(index));
    }
    let mut calls = Vec::new();
    for index in 0..count {
        let times = 1 + gen.rng.below(2);
        for _ in 0..times {
            calls.push(gen.main_call(index));
        }
    }
    Program {
        seed,
        functions,
        calls,
    }
}

impl Gen<'_> {
    fn indent(&self) -> String {
        "    ".repeat(self.depth)
    }

    fn fresh(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    fn any_ty(&mut self) -> Ty {
        if self.cfg.floats && self.rng.chance(15) {
            *self.rng.pick(&FLOATS)
        } else if self.rng.chance(8) {
            Ty::Bool
        } else {
            *self.rng.pick(&INTS)
        }
    }

    fn vars_of(&self, ty: Ty, assignable_only: bool) -> Vec<Var> {
        self.scopes
            .iter()
            .flatten()
            .filter(|v| v.ty == ty && (!assignable_only || v.assignable))
            .cloned()
            .collect()
    }

    fn function(&mut self, index: usize) -> GenFn {
        let params: Vec<Ty> = (0..self.rng.below(5)).map(|_| self.any_ty()).collect();
        let ret = self.any_ty();
        let name = format!("f{index}");
        let mut header = format!("func {name}(");
        let mut scope = Vec::new();
        for (i, ty) in params.iter().enumerate() {
            if i > 0 {
                header.push_str(", ");
            }
            let _ = write!(header, "p{i}: {}", ty.name());
            scope.push(Var {
                name: format!("p{i}"),
                ty: *ty,
                assignable: false,
            });
        }
        let _ = write!(header, ") -> {}", ret.name());

        self.scopes = vec![scope];
        self.loop_depth = 0;
        self.mult = 1;
        self.cost = 1;
        self.ret = ret;
        self.depth = 1;
        self.may_print = !params.is_empty() || self.cfg.print_in_constant_functions;
        self.prints = false;
        let count = 2 + self.rng.below(8);
        let mut stmts = Vec::new();
        for _ in 0..count {
            stmts.push(self.stmt());
        }
        let tail = self.expr(ret, 3);
        self.sigs.push(Sig {
            name,
            params,
            ret,
            cost: self.cost,
            prints: self.prints,
        });
        GenFn {
            header,
            stmts,
            tail,
        }
    }

    fn main_call(&mut self, index: usize) -> MainCall {
        let sig = self.sigs[index].clone();
        self.scopes = vec![Vec::new()];
        self.loop_depth = 0;
        self.mult = 1;
        self.cost = 0;
        let args: Vec<String> = sig.params.iter().map(|&ty| self.literal(ty)).collect();
        let r = self.fresh("r");
        let mut lines = format!(
            "    let {r}: {} = {}({})\n",
            sig.ret.name(),
            sig.name,
            args.join(", ")
        );
        let fold = |lines: &mut String, value: &str| {
            let _ = writeln!(lines, "    h = (h ^ {value}) * 1099511628211u64");
        };
        if sig.ret.is_float() {
            let _ = writeln!(
                lines,
                "    println(\"{} \", ({r} as i64), \" \", (({r} * 4096.0{}) as i64))",
                sig.name,
                sig.ret.name()
            );
            fold(&mut lines, &format!("(({r} as i64) as u64)"));
            fold(
                &mut lines,
                &format!("((({r} * 4096.0{}) as i64) as u64)", sig.ret.name()),
            );
        } else {
            let _ = writeln!(lines, "    println(\"{} \", {r})", sig.name);
            fold(&mut lines, &format!("({r} as u64)"));
        }
        MainCall {
            function: index,
            lines,
        }
    }

    // ---- statements ---------------------------------------------------

    fn block(&mut self, count: u64) -> String {
        self.scopes.push(Vec::new());
        self.depth += 1;
        let mut out = String::new();
        for _ in 0..count {
            out.push_str(&self.stmt());
        }
        self.depth -= 1;
        self.scopes.pop();
        out
    }

    fn stmt(&mut self) -> String {
        self.cost += self.mult;
        let ind = self.indent();
        let nest_ok = self.depth < 4;
        loop {
            match self.rng.below(20) {
                0..=4 => {
                    let ty = self.any_ty();
                    let mutable = self.rng.chance(60);
                    let name = self.fresh("v");
                    let init = self.expr(ty, 3);
                    self.scopes.last_mut().unwrap().push(Var {
                        name: name.clone(),
                        ty,
                        assignable: mutable,
                    });
                    let kw = if mutable { "var" } else { "let" };
                    return format!("{ind}{kw} {name}: {} = {init}\n", ty.name());
                }
                5..=7 => {
                    let ty = self.any_ty();
                    let targets = self.vars_of(ty, true);
                    if targets.is_empty() {
                        continue;
                    }
                    let target = self.rng.pick(&targets).name.clone();
                    let value = self.expr(ty, 3);
                    if ty.is_int() && self.rng.chance(50) {
                        let op = *self.rng.pick(&["+=", "-=", "*=", "&=", "|=", "^="]);
                        return format!("{ind}{target} {op} {value}\n");
                    }
                    if ty.is_int() && self.rng.chance(15) {
                        let op = *self.rng.pick(&["<<=", ">>="]);
                        let mask = Self::int_lit(ty, u64::from(ty.bits() - 1));
                        return format!("{ind}{target} {op} ({value} & {mask})\n");
                    }
                    if ty.is_float() && self.rng.chance(40) {
                        let op = *self.rng.pick(&["+=", "-=", "*="]);
                        return format!("{ind}{target} {op} {value}\n");
                    }
                    return format!("{ind}{target} = {value}\n");
                }
                8..=9 if nest_ok => {
                    let cond = self.expr(Ty::Bool, 3);
                    let n = 1 + self.rng.below(3);
                    let then = self.block(n);
                    if self.rng.chance(50) {
                        let n = 1 + self.rng.below(3);
                        let other = self.block(n);
                        return format!(
                            "{ind}if {cond} {{\n{then}{ind}}} else {{\n{other}{ind}}}\n"
                        );
                    }
                    return format!("{ind}if {cond} {{\n{then}{ind}}}\n");
                }
                10..=11 if nest_ok && self.loop_depth < 2 => return self.counted_loop(),
                12..=13 if self.loop_depth > 0 => {
                    let cond = self.expr(Ty::Bool, 2);
                    return format!("{ind}if {cond} {{\n{ind}    break\n{ind}}}\n");
                }
                14 => {
                    let cond = self.expr(Ty::Bool, 2);
                    let value = self.expr(self.ret, 2);
                    return format!("{ind}if {cond} {{\n{ind}    return {value}\n{ind}}}\n");
                }
                15..=16 if self.may_print => {
                    self.prints = true;
                    let ty = self.any_ty();
                    self.in_print = !self.cfg.calls_in_print_args;
                    let mut value = self.expr(ty, 2);
                    self.in_print = false;
                    if ty.is_float() {
                        // Not printed directly: see the module documentation.
                        value = format!("({value} as i64)");
                    }
                    return format!("{ind}println(\"{} \", {value})\n", self.fresh("s"));
                }
                17 => {
                    // `loop` used as a value, left by `break value`.
                    let ty = self.any_ty();
                    let name = self.fresh("w");
                    let counter = self.fresh("j");
                    let trips = self.rng.below(6);
                    let saved = self.mult;
                    self.mult *= trips.max(1);
                    let found = self.expr(Ty::Bool, 2);
                    let early = self.expr(ty, 2);
                    let late = self.expr(ty, 2);
                    self.mult = saved;
                    self.scopes.last_mut().unwrap().push(Var {
                        name: name.clone(),
                        ty,
                        assignable: false,
                    });
                    return format!(
                        "{ind}var {counter}: i32 = 0i32\n\
                         {ind}let {name}: {t} = loop {{\n\
                         {ind}    if {counter} >= {trips}i32 {{\n\
                         {ind}        break {late}\n\
                         {ind}    }}\n\
                         {ind}    if {found} {{\n\
                         {ind}        break {early}\n\
                         {ind}    }}\n\
                         {ind}    {counter} += 1i32\n\
                         {ind}}}\n",
                        t = ty.name()
                    );
                }
                _ => {}
            }
        }
    }

    /// A `while` or `loop` over a counter the body cannot assign.
    fn counted_loop(&mut self) -> String {
        let ind = self.indent();
        let counter = self.fresh("i");
        let trips = self.rng.below(9);
        let saved = self.mult;
        self.mult *= trips.max(1);
        self.loop_depth += 1;
        self.scopes.last_mut().unwrap().push(Var {
            name: counter.clone(),
            ty: Ty::I32,
            assignable: false,
        });
        let n = 1 + self.rng.below(4);
        let body = self.block(n);
        self.loop_depth -= 1;
        self.mult = saved;
        if self.rng.chance(60) {
            format!(
                "{ind}var {counter}: i32 = 0i32\n\
                 {ind}while {counter} < {trips}i32 {{\n\
                 {body}{ind}    {counter} += 1i32\n\
                 {ind}}}\n"
            )
        } else {
            format!(
                "{ind}var {counter}: i32 = 0i32\n\
                 {ind}loop {{\n\
                 {ind}    if {counter} >= {trips}i32 {{\n\
                 {ind}        break\n\
                 {ind}    }}\n\
                 {body}{ind}    {counter} += 1i32\n\
                 {ind}}}\n"
            )
        }
    }

    // ---- expressions --------------------------------------------------

    fn int_lit(ty: Ty, value: u64) -> String {
        format!("{value}{}", ty.name())
    }

    fn literal(&mut self, ty: Ty) -> String {
        match ty {
            Ty::Bool => (if self.rng.chance(50) { "true" } else { "false" }).to_owned(),
            Ty::F32 | Ty::F64 => {
                let range = if self.rng.chance(10) { 100_000 } else { 20 };
                let whole = self.rng.below(range);
                let frac = *self.rng.pick(&["0", "5", "25", "125", "75", "1", "3"]);
                let sign = if self.rng.chance(30) { "-" } else { "" };
                format!("({sign}{whole}.{frac}{})", ty.name())
            }
            _ => {
                // Mostly small values, sometimes values near the type's edges
                // so wrapping is exercised.
                let max = if ty.is_signed() {
                    ty.signed_max()
                } else {
                    ty.unsigned_max()
                };
                let value = match self.rng.below(10) {
                    0..=5 => self.rng.below(16.min(max)),
                    6..=7 => self.rng.below(max.min(1000) + 1),
                    _ => max - self.rng.below(4).min(max),
                };
                if ty.is_signed() && self.rng.chance(35) {
                    format!("(-{})", Self::int_lit(ty, value))
                } else {
                    Self::int_lit(ty, value)
                }
            }
        }
    }

    fn leaf(&mut self, ty: Ty) -> String {
        let vars = self.vars_of(ty, false);
        if !vars.is_empty() && self.rng.chance(65) {
            return self.rng.pick(&vars).name.clone();
        }
        self.literal(ty)
    }

    /// A call to an earlier function returning `ty`, if one fits the budget.
    fn call(&mut self, ty: Ty, depth: u32) -> Option<String> {
        if self.in_print {
            return None;
        }
        let room = self.cfg.cost_budget.saturating_sub(self.cost);
        let candidates: Vec<Sig> = self
            .sigs
            .iter()
            .filter(|s| {
                s.ret == ty
                    && (self.may_print || !s.prints)
                    && self.mult.saturating_mul(s.cost + 1) <= room
            })
            .cloned()
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let sig = self.rng.pick(&candidates).clone();
        self.prints |= sig.prints;
        self.cost += self.mult * (sig.cost + 1);
        let args: Vec<String> = sig
            .params
            .iter()
            .map(|&p| self.expr(p, depth.saturating_sub(1)))
            .collect();
        Some(format!("{}({})", sig.name, args.join(", ")))
    }

    fn expr(&mut self, ty: Ty, depth: u32) -> String {
        if depth == 0 || self.rng.chance(25) {
            return self.leaf(ty);
        }
        let d = depth - 1;
        match ty {
            Ty::Bool => self.bool_expr(d),
            Ty::F32 | Ty::F64 => self.float_expr(ty, d),
            _ => self.int_expr(ty, d),
        }
    }

    fn int_expr(&mut self, ty: Ty, d: u32) -> String {
        loop {
            match self.rng.below(13) {
                0..=2 => {
                    let op = *self.rng.pick(&["+", "-", "*"]);
                    return format!("({} {op} {})", self.expr(ty, d), self.expr(ty, d));
                }
                3..=4 => {
                    let op = *self.rng.pick(&["&", "|", "^"]);
                    return format!("({} {op} {})", self.expr(ty, d), self.expr(ty, d));
                }
                5 => {
                    let op = *self.rng.pick(&["<<", ">>"]);
                    let mask = Self::int_lit(ty, u64::from(ty.bits() - 1));
                    return format!(
                        "({} {op} ({} & {mask}))",
                        self.expr(ty, d),
                        self.expr(ty, d)
                    );
                }
                6 => {
                    let op = *self.rng.pick(&["/", "%"]);
                    let one = Self::int_lit(ty, 1);
                    let divisor = if ty.is_signed() {
                        let max = Self::int_lit(ty, ty.signed_max());
                        format!("(({} & {max}) | {one})", self.expr(ty, d))
                    } else {
                        format!("({} | {one})", self.expr(ty, d))
                    };
                    return format!("({} {op} {divisor})", self.expr(ty, d));
                }
                7 => {
                    if ty.is_signed() && self.rng.chance(50) {
                        return format!("(-{})", self.expr(ty, d));
                    }
                    return format!("(~{})", self.expr(ty, d));
                }
                8 => {
                    let float_ok =
                        self.cfg.floats && (ty.bits() >= 32 || self.cfg.float_to_narrow_int);
                    let from = if float_ok && self.rng.chance(25) {
                        *self.rng.pick(&FLOATS)
                    } else {
                        *self.rng.pick(&INTS)
                    };
                    return format!("({} as {})", self.expr(from, d), ty.name());
                }
                9 => {
                    if let Some(call) = self.call(ty, d) {
                        return call;
                    }
                }
                10 => {
                    return format!(
                        "(if {} {{ {} }} else {{ {} }})",
                        self.expr(Ty::Bool, d),
                        self.expr(ty, d),
                        self.expr(ty, d)
                    );
                }
                11 => return format!("({} as {})", self.expr(Ty::Bool, d), ty.name()),
                _ => return self.leaf(ty),
            }
        }
    }

    fn float_expr(&mut self, ty: Ty, d: u32) -> String {
        loop {
            match self.rng.below(9) {
                0..=3 => {
                    let op = *self.rng.pick(&["+", "-", "*", "/"]);
                    return format!("({} {op} {})", self.expr(ty, d), self.expr(ty, d));
                }
                4 => return format!("(-{})", self.expr(ty, d)),
                5 => {
                    let from = if self.rng.chance(70) {
                        *self.rng.pick(&INTS)
                    } else {
                        *self.rng.pick(&FLOATS)
                    };
                    return format!("({} as {})", self.expr(from, d), ty.name());
                }
                6 => {
                    if let Some(call) = self.call(ty, d) {
                        return call;
                    }
                }
                7 => {
                    return format!(
                        "(if {} {{ {} }} else {{ {} }})",
                        self.expr(Ty::Bool, d),
                        self.expr(ty, d),
                        self.expr(ty, d)
                    );
                }
                _ => return self.leaf(ty),
            }
        }
    }

    fn bool_expr(&mut self, d: u32) -> String {
        loop {
            match self.rng.below(8) {
                0..=3 => {
                    let ty = if self.cfg.floats && self.rng.chance(20) {
                        *self.rng.pick(&FLOATS)
                    } else {
                        *self.rng.pick(&INTS)
                    };
                    let op = *self.rng.pick(&["==", "!=", "<", "<=", ">", ">="]);
                    return format!("({} {op} {})", self.expr(ty, d), self.expr(ty, d));
                }
                4 => return format!("(!{})", self.expr(Ty::Bool, d)),
                5 => {
                    let op = *self.rng.pick(&["&&", "||"]);
                    return format!(
                        "({} {op} {})",
                        self.expr(Ty::Bool, d),
                        self.expr(Ty::Bool, d)
                    );
                }
                6 => {
                    if let Some(call) = self.call(Ty::Bool, d) {
                        return call;
                    }
                }
                _ => return self.leaf(Ty::Bool),
            }
        }
    }
}
