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
//!   - a shift amount is taken modulo the operand's width, which the language
//!     defines (`agents/USING_CODIRA.md`, "Integer arithmetic"); half the
//!     shifts mask it to `0..width` in the source, the others shift by any
//!     value, negative and too-wide amounts included;
//!   - a divisor is forced odd, and for a signed type also non-negative
//!     (`(d & MAX) | 1`), so it is never zero and never `-1`: such a division
//!     panics (on both backends), and a program that panics is not one whose
//!     output can be compared;
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
//! - Floats are printed directly: both backends print them as the runtime's
//!   formatter does (regression test `float_printing_agrees` in
//!   `differential.rs`).
//! - Three constructs were once left out because of divergences since fixed,
//!   and are behind `Config` switches, all on by default: float-to-8/16-bit
//!   casts (`float_to_narrow_int`), calls in `println` arguments
//!   (`calls_in_print_args`) and printing from a function without parameters
//!   (`print_in_constant_functions`). Turning one off reproduces a run from
//!   before its fix.

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
    /// Generate casts from a float to an 8- or 16-bit integer. On by
    /// default since the Cranelift backend stopped crashing on them
    /// (regression test `float_to_narrow_int_cast_agrees`). Off, a 32- or
    /// 64-bit target is still generated, and so is a narrowing of its result.
    pub float_to_narrow_int: bool,
    /// Allow calls inside the arguments of `println`. On by default since
    /// both backends evaluate every argument before printing any
    /// (regression test `print_arguments_evaluated_first`).
    pub calls_in_print_args: bool,
    /// Let a function without parameters print (itself or through a call).
    /// On by default since the LLVM path's Eidos constant fold stopped
    /// dropping such output (regression test `eidos_fold_keeps_output`).
    pub print_in_constant_functions: bool,
    /// Generate the heap: arrays of every scalar type (literals, indexing,
    /// assignment through an index, `push`, `len`, `for` loops over them),
    /// instances of a class `Cell` (record literals naming the fields out of
    /// order, field reads and assignments, an array field) and calls passing
    /// them. Indices are reduced modulo the length, and arrays never
    /// shrink, so no access is out of range.
    pub heap: bool,
    /// Generate `String`s: literals (converted from `str`), `to_string()` of
    /// numbers and `bool`s, `+`, `+=` with short pieces, comparisons, `len`,
    /// `char_count`, `contains`, `starts_with`, `to_upper`, `repeat`,
    /// `replace`, and printing them. No string grows by more than a piece at
    /// a time, so lengths stay small.
    pub strings: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            floats: true,
            max_functions: 7,
            cost_budget: 20_000,
            float_to_narrow_int: true,
            calls_in_print_args: true,
            print_in_constant_functions: true,
            heap: true,
            strings: true,
        }
    }
}

/// What every program with the heap starts with: the class and the helpers
/// that take heap values.
const HEAP_PROLOGUE: &str = "class Cell { a: i64, b: u8, c: bool, d: f64, xs: [i64] }

func sum_xs(xs: [i64]) -> i64 {
    var t: i64 = 0i64
    for x in xs {
        t = (t * 31i64) ^ x
    }
    t
}

func sum_cell(c: Cell) -> i64 {
    var t: i64 = c.a + (c.b as i64)
    if c.c {
        t += 1i64
    }
    t ^ sum_xs(c.xs) ^ (c.d as i64)
}

";

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
    /// Declarations every function may use (the heap's class and helpers).
    pub prologue: String,
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
        out.push_str(&self.prologue);
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
    /// The scalar type, or an array's element type.
    ty: Ty,
    /// Assignable: a `var` that is not a loop counter.
    assignable: bool,
    shape: Shape,
}

/// What a variable holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Scalar,
    /// An array of `ty`, never empty.
    Array,
    /// An instance of the program's class `Cell`.
    Cell,
    /// A `String` (assignable when a `var`).
    Text,
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
    /// Heap reads being generated inside one another (an index that reads
    /// an array), bounded so generation terminates quickly.
    heap_nesting: u32,
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
        heap_nesting: 0,
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
        prologue: if cfg.heap {
            HEAP_PROLOGUE.to_owned()
        } else {
            String::new()
        },
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
            .filter(|v| {
                v.shape == Shape::Scalar && v.ty == ty && (!assignable_only || v.assignable)
            })
            .cloned()
            .collect()
    }

    fn heap_vars(&self, shape: Shape, ty: Option<Ty>) -> Vec<Var> {
        self.scopes
            .iter()
            .flatten()
            .filter(|v| v.shape == shape && ty.is_none_or(|ty| v.ty == ty))
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
                shape: Shape::Scalar,
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
            let sides = if self.cfg.heap { 27 } else { 20 };
            let sides = if self.cfg.strings { sides + 4 } else { sides };
            match self.rng.below(sides) {
                27..=30 => {
                    if let Some(stmt) = self.text_stmt(&ind) {
                        return stmt;
                    }
                }
                20..=26 if self.cfg.heap => {
                    if let Some(stmt) = self.heap_stmt(&ind, nest_ok) {
                        return stmt;
                    }
                }
                0..=4 => {
                    let ty = self.any_ty();
                    let mutable = self.rng.chance(60);
                    let name = self.fresh("v");
                    let init = self.expr(ty, 3);
                    self.scopes.last_mut().unwrap().push(Var {
                        name: name.clone(),
                        ty,
                        assignable: mutable,
                        shape: Shape::Scalar,
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
                    let value = self.expr(ty, 2);
                    self.in_print = false;
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
                        shape: Shape::Scalar,
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

    /// An index into the array `name`, always in range: an integer reduced
    /// modulo the length (never 0).
    fn index_into(&mut self, name: &str) -> String {
        let ty = *self.rng.pick(&INTS);
        let value = self.expr(ty, 1);
        format!("(({value} as usize) % {name}.len())")
    }

    fn declare(&mut self, name: &str, ty: Ty, shape: Shape) {
        self.scopes.last_mut().unwrap().push(Var {
            name: name.to_owned(),
            ty,
            assignable: false,
            shape,
        });
    }

    /// The field assignments of a `Cell` record literal, in a random order.
    fn cell_literal(&mut self) -> String {
        let mut fields = vec![
            format!("a: {}", self.expr(Ty::I64, 2)),
            format!("b: {}", self.expr(Ty::U8, 2)),
            format!("c: {}", self.expr(Ty::Bool, 2)),
            format!("d: {}", self.expr(Ty::F64, 2)),
        ];
        let n = 1 + self.rng.below(3);
        let xs: Vec<String> = (0..n).map(|_| self.expr(Ty::I64, 1)).collect();
        fields.push(format!("xs: [{}]", xs.join(", ")));
        for i in (1..fields.len()).rev() {
            let j = usize::try_from(self.rng.below(i as u64 + 1)).unwrap();
            fields.swap(i, j);
        }
        format!("Cell {{ {} }}", fields.join(", "))
    }

    /// One statement on the heap, or `None` when there is nothing to use.
    fn heap_stmt(&mut self, ind: &str, nest_ok: bool) -> Option<String> {
        match self.rng.below(7) {
            0 => {
                let ty = self.any_ty();
                let name = self.fresh("a");
                let n = 1 + self.rng.below(4);
                let elements: Vec<String> = (0..n).map(|_| self.expr(ty, 2)).collect();
                self.declare(&name, ty, Shape::Array);
                Some(format!(
                    "{ind}let {name}: [{}] = [{}]\n",
                    ty.name(),
                    elements.join(", ")
                ))
            }
            1 => {
                let arrays = self.heap_vars(Shape::Array, None);
                if arrays.is_empty() {
                    return None;
                }
                let array = self.rng.pick(&arrays).clone();
                let value = self.expr(array.ty, 2);
                Some(format!("{ind}{}.push({value})\n", array.name))
            }
            2 => {
                let arrays = self.heap_vars(Shape::Array, None);
                if arrays.is_empty() {
                    return None;
                }
                let array = self.rng.pick(&arrays).clone();
                let index = self.index_into(&array.name);
                let value = self.expr(array.ty, 2);
                let ty = array.ty;
                let op = if ty.is_int() && self.rng.chance(50) {
                    *self.rng.pick(&["+=", "-=", "*=", "^="])
                } else if ty.is_float() && self.rng.chance(40) {
                    *self.rng.pick(&["+=", "*="])
                } else {
                    "="
                };
                Some(format!("{ind}{}[{index}] {op} {value}\n", array.name))
            }
            3 => {
                let name = self.fresh("c");
                let literal = self.cell_literal();
                self.declare(&name, Ty::I64, Shape::Cell);
                Some(format!("{ind}let {name} = {literal}\n"))
            }
            4 => {
                let cells = self.heap_vars(Shape::Cell, None);
                if cells.is_empty() {
                    return None;
                }
                let cell = self.rng.pick(&cells).name.clone();
                Some(match self.rng.below(6) {
                    0 => format!("{ind}{cell}.a += {}\n", self.expr(Ty::I64, 2)),
                    1 => format!("{ind}{cell}.b = {}\n", self.expr(Ty::U8, 2)),
                    2 => format!("{ind}{cell}.c = {}\n", self.expr(Ty::Bool, 2)),
                    3 => format!("{ind}{cell}.d *= {}\n", self.expr(Ty::F64, 2)),
                    4 => format!("{ind}{cell}.xs.push({})\n", self.expr(Ty::I64, 2)),
                    _ => {
                        let xs = format!("{cell}.xs");
                        let index = self.index_into(&xs);
                        format!("{ind}{xs}[{index}] = {}\n", self.expr(Ty::I64, 2))
                    }
                })
            }
            5 if nest_ok && self.loop_depth == 0 => {
                let arrays = self.heap_vars(Shape::Array, None);
                if arrays.is_empty() {
                    return None;
                }
                let array = self.rng.pick(&arrays).clone();
                let element = self.fresh("x");
                let saved = self.mult;
                // The length is not tracked: assume a long array.
                self.mult *= 64;
                self.loop_depth += 1;
                self.scopes.push(vec![Var {
                    name: element.clone(),
                    ty: array.ty,
                    assignable: false,
                    shape: Shape::Scalar,
                }]);
                let n = 1 + self.rng.below(3);
                let body = self.block(n);
                self.scopes.pop();
                self.loop_depth -= 1;
                self.mult = saved;
                Some(format!(
                    "{ind}for {element} in {} {{\n{body}{ind}}}\n",
                    array.name
                ))
            }
            _ => None,
        }
    }

    /// A short `str` literal, sometimes with a multi-byte character.
    fn text_literal(&mut self) -> String {
        let piece = *self
            .rng
            .pick(&["", "a", "ab", "b-a", "é", "xé!", "a b", "Zz", "-"]);
        format!("\"{piece}\"")
    }

    /// A `String` expression whose length is small.
    fn text_expr(&mut self, depth: u32) -> String {
        let texts = self.heap_vars(Shape::Text, None);
        let pick_text = |gen: &mut Self| -> String {
            if !texts.is_empty() && gen.rng.chance(60) {
                gen.rng.pick(&texts).name.clone()
            } else {
                gen.text_literal()
            }
        };
        match self.rng.below(if depth == 0 { 2 } else { 8 }) {
            0 => pick_text(self),
            1 => {
                let ty = self.any_ty();
                format!("({}).to_string()", self.expr(ty, 1))
            }
            2 | 3 => {
                let left = pick_text(self);
                let right = pick_text(self);
                format!("({left} + {right})")
            }
            4 => format!("{}.to_upper()", pick_text(self)),
            5 => format!("{}.to_lower()", pick_text(self)),
            6 => format!("{}.repeat({}i64)", pick_text(self), self.rng.below(3)),
            _ => format!("{}.replace(\"a\", \"<>\")", pick_text(self)),
        }
    }

    /// One statement on strings, or `None` when there is nothing to use.
    fn text_stmt(&mut self, ind: &str) -> Option<String> {
        match self.rng.below(4) {
            0 | 1 => {
                let name = self.fresh("t");
                let value = self.text_expr(2);
                self.scopes.last_mut().unwrap().push(Var {
                    name: name.clone(),
                    ty: Ty::I64,
                    assignable: true,
                    shape: Shape::Text,
                });
                Some(format!("{ind}var {name}: String = {value}\n"))
            }
            2 => {
                let texts = self.heap_vars(Shape::Text, None);
                if texts.is_empty() {
                    return None;
                }
                let name = self.rng.pick(&texts).name.clone();
                let piece = if self.rng.chance(50) {
                    self.text_literal()
                } else {
                    let ty = *self.rng.pick(&INTS);
                    format!("({}).to_string()", self.expr(ty, 1))
                };
                Some(format!("{ind}{name} += {piece}\n"))
            }
            _ if self.may_print => {
                let texts = self.heap_vars(Shape::Text, None);
                if texts.is_empty() {
                    return None;
                }
                self.prints = true;
                let name = self.rng.pick(&texts).name.clone();
                Some(format!("{ind}println(\"{} \", {name})\n", self.fresh("s")))
            }
            _ => None,
        }
    }

    /// A value read from a `String`, of type `ty`.
    fn text_leaf(&mut self, ty: Ty) -> Option<String> {
        let texts = self.heap_vars(Shape::Text, None);
        if texts.is_empty() {
            return None;
        }
        let name = self.rng.pick(&texts).name.clone();
        let other = if self.rng.chance(50) {
            self.text_literal()
        } else {
            self.rng.pick(&texts).name.clone()
        };
        match ty {
            Ty::Bool => Some(match self.rng.below(5) {
                0 => format!("({name} == {other})"),
                1 => format!("({name} < {other})"),
                2 => format!("({name} >= {other})"),
                3 => format!("{name}.contains({other})"),
                _ => format!("{name}.starts_with({other})"),
            }),
            _ if ty.is_int() => Some(if self.rng.chance(50) {
                format!("({name}.len() as {})", ty.name())
            } else {
                format!("({name}.char_count() as {})", ty.name())
            }),
            _ => None,
        }
    }

    /// A value read from the heap, of type `ty`, or `None` when there is
    /// nothing of that type to read.
    fn heap_leaf(&mut self, ty: Ty) -> Option<String> {
        match self.rng.below(4) {
            0 => {
                let arrays = self.heap_vars(Shape::Array, Some(ty));
                if arrays.is_empty() {
                    return None;
                }
                let name = self.rng.pick(&arrays).name.clone();
                let index = self.index_into(&name);
                Some(format!("({name}[{index}])"))
            }
            1 if ty.is_int() => {
                let arrays = self.heap_vars(Shape::Array, None);
                if arrays.is_empty() {
                    return None;
                }
                let name = self.rng.pick(&arrays).name.clone();
                Some(format!("({name}.len() as {})", ty.name()))
            }
            2 => {
                let cells = self.heap_vars(Shape::Cell, None);
                if cells.is_empty() {
                    return None;
                }
                let cell = self.rng.pick(&cells).name.clone();
                Some(match ty {
                    // Parenthesised, like every other compound read: a
                    // statement ending in `c.a` followed by a line starting
                    // with `(` would read as a method call.
                    Ty::I64 => format!("({cell}.a)"),
                    Ty::U8 => format!("({cell}.b)"),
                    Ty::Bool => format!("({cell}.c)"),
                    Ty::F64 => format!("({cell}.d)"),
                    _ if ty.is_int() => {
                        let xs = format!("{cell}.xs");
                        let index = self.index_into(&xs);
                        format!("({xs}[{index}] as {})", ty.name())
                    }
                    _ => return None,
                })
            }
            _ if ty.is_int() && !self.in_print => {
                // A call that takes the heap: the helpers of the prologue.
                if self.rng.chance(50) {
                    let cells = self.heap_vars(Shape::Cell, None);
                    if cells.is_empty() {
                        return None;
                    }
                    let cell = self.rng.pick(&cells).name.clone();
                    self.cost += self.mult * 70;
                    Some(format!("(sum_cell({cell}) as {})", ty.name()))
                } else {
                    let arrays = self.heap_vars(Shape::Array, Some(Ty::I64));
                    if arrays.is_empty() {
                        return None;
                    }
                    let name = self.rng.pick(&arrays).name.clone();
                    self.cost += self.mult * 70;
                    Some(format!("(sum_xs({name}) as {})", ty.name()))
                }
            }
            _ => None,
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
            shape: Shape::Scalar,
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
        if self.cfg.strings && self.heap_nesting < 2 && self.rng.chance(8) {
            self.heap_nesting += 1;
            let read = self.text_leaf(ty);
            self.heap_nesting -= 1;
            if let Some(read) = read {
                return read;
            }
        }
        if self.cfg.heap && self.heap_nesting < 2 && self.rng.chance(20) {
            self.heap_nesting += 1;
            let read = self.heap_leaf(ty);
            self.heap_nesting -= 1;
            if let Some(read) = read {
                return read;
            }
        }
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
                    if self.rng.chance(50) {
                        // Any amount: the language takes it modulo the width.
                        return format!("({} {op} {})", self.expr(ty, d), self.expr(ty, d));
                    }
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
