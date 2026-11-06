//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The operations of lazy pipelines: `xs.iter()`, a range, or an object
//!   with `next(self) -> T?` as the source; `map`, `filter`, `take`, `skip`,
//!   `take_while`, `skip_while`, `enumerate`, `zip`, `chain` and `rev` as
//!   adapters; `sum`, `fold`, `count`, `any`, `all`, `find`, `position`,
//!   `min`, `max` and `collect` as terminals (and `for x in pipeline`).
//!
//! # A pipeline exists only inside one expression
//!
//! A pipeline has the type `Iter[T]`, which has no run-time representation:
//! code generation compiles the whole chain, from its source to its
//! terminal (or the `for` loop it feeds), into one loop in which every
//! closure literal is generated in place (see
//! `codira_codegen::ir::body::pipelines`). So an `Iter[T]` value cannot be
//! stored in a variable or a field, returned, passed to a function, or put
//! in an array, a tuple or an `if`: inference reports each of those (see
//! `ty::infer::pipelines`). What it gives in exchange is a guarantee rather
//! than an optimization that may or may not happen: no iterator object, no
//! allocation and no call per element for literal closures.
//!
//! The order of evaluation and of side effects is Rust's: the source, the
//! arguments of every stage (left to right) and the terminal's arguments
//! are evaluated once, before the first element; then each element goes
//! through all the stages in order before the next element is pulled, and
//! a stage that ends the stream (`take`, `take_while`, `zip`, or a terminal
//! that has its answer) stops pulling at once.

/// An operation of a lazy pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PipelineOp {
    /// `xs.iter()` on an array, a range or an iterator object: the start of
    /// a pipeline (on a pipeline, it changes nothing).
    Iter,
    /// `.map(f)`: `f(x)` for each element.
    Map,
    /// `.filter(f)`: the elements `f` accepts.
    Filter,
    /// `.take(n)`: the first `n` elements (then stops pulling).
    Take,
    /// `.skip(n)`: all but the first `n` elements.
    Skip,
    /// `.take_while(f)`: the elements before the first one `f` rejects.
    TakeWhile,
    /// `.skip_while(f)`: the elements from the first one `f` rejects on.
    SkipWhile,
    /// `.enumerate()`: `(index, element)` pairs, the index a `usize`.
    Enumerate,
    /// `.zip(ys)`: `(x, y)` pairs with an array's or a range's elements,
    /// ending with the shorter.
    Zip,
    /// `.chain(ys)`: these elements, then those of an array, a range or
    /// another pipeline.
    Chain,
    /// `.rev()`: the elements in reverse order (an array or a range source,
    /// with only `map` and `filter` before it).
    Rev,
    /// `.sum()`: the sum (wrapping for integers, from 0 for floats).
    Sum,
    /// `.fold(init, f)`.
    Fold,
    /// `.count()`: the number of elements, a `usize`.
    Count,
    /// `.any(f)`: whether `f` accepts some element (stops at the first).
    Any,
    /// `.all(f)`: whether `f` accepts every element (stops at the first it
    /// rejects).
    All,
    /// `.find(f)`: the first element `f` accepts, `T?`.
    Find,
    /// `.position(f)`: the index of the first element `f` accepts, `usize?`.
    Position,
    /// `.min()`: the smallest element, `nil` when there is none.
    Min,
    /// `.max()`: the largest element, `nil` when there is none.
    Max,
    /// `.collect()`: a new array of the elements.
    Collect,
}

impl PipelineOp {
    const ALL: &'static [PipelineOp] = &[
        PipelineOp::Iter,
        PipelineOp::Map,
        PipelineOp::Filter,
        PipelineOp::Take,
        PipelineOp::Skip,
        PipelineOp::TakeWhile,
        PipelineOp::SkipWhile,
        PipelineOp::Enumerate,
        PipelineOp::Zip,
        PipelineOp::Chain,
        PipelineOp::Rev,
        PipelineOp::Sum,
        PipelineOp::Fold,
        PipelineOp::Count,
        PipelineOp::Any,
        PipelineOp::All,
        PipelineOp::Find,
        PipelineOp::Position,
        PipelineOp::Min,
        PipelineOp::Max,
        PipelineOp::Collect,
    ];

    /// The name a program calls this operation by.
    pub fn name(self) -> &'static str {
        match self {
            PipelineOp::Iter => "iter",
            PipelineOp::Map => "map",
            PipelineOp::Filter => "filter",
            PipelineOp::Take => "take",
            PipelineOp::Skip => "skip",
            PipelineOp::TakeWhile => "take_while",
            PipelineOp::SkipWhile => "skip_while",
            PipelineOp::Enumerate => "enumerate",
            PipelineOp::Zip => "zip",
            PipelineOp::Chain => "chain",
            PipelineOp::Rev => "rev",
            PipelineOp::Sum => "sum",
            PipelineOp::Fold => "fold",
            PipelineOp::Count => "count",
            PipelineOp::Any => "any",
            PipelineOp::All => "all",
            PipelineOp::Find => "find",
            PipelineOp::Position => "position",
            PipelineOp::Min => "min",
            PipelineOp::Max => "max",
            PipelineOp::Collect => "collect",
        }
    }

    /// The pipeline operation called `name`, if there is one.
    pub fn from_name(name: &str) -> Option<PipelineOp> {
        Self::ALL.iter().copied().find(|it| it.name() == name)
    }

    /// Whether the operation ends a pipeline with a value (`sum`, ...),
    /// rather than making a longer pipeline.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            PipelineOp::Sum
                | PipelineOp::Fold
                | PipelineOp::Count
                | PipelineOp::Any
                | PipelineOp::All
                | PipelineOp::Find
                | PipelineOp::Position
                | PipelineOp::Min
                | PipelineOp::Max
                | PipelineOp::Collect
        )
    }

    /// The number of arguments the operation takes.
    pub fn arity(self) -> usize {
        match self {
            PipelineOp::Fold => 2,
            PipelineOp::Map
            | PipelineOp::Filter
            | PipelineOp::Take
            | PipelineOp::Skip
            | PipelineOp::TakeWhile
            | PipelineOp::SkipWhile
            | PipelineOp::Zip
            | PipelineOp::Chain
            | PipelineOp::Any
            | PipelineOp::All
            | PipelineOp::Find
            | PipelineOp::Position => 1,
            _ => 0,
        }
    }

    /// The position of the closure argument, for the operations that take
    /// one.
    pub fn closure_arg(self) -> Option<usize> {
        match self {
            PipelineOp::Map
            | PipelineOp::Filter
            | PipelineOp::TakeWhile
            | PipelineOp::SkipWhile
            | PipelineOp::Any
            | PipelineOp::All
            | PipelineOp::Find
            | PipelineOp::Position => Some(0),
            PipelineOp::Fold => Some(1),
            _ => None,
        }
    }

    /// Whether the operation allocates: only `collect`, which makes the
    /// array it returns.
    pub fn allocates(self) -> bool {
        self == PipelineOp::Collect
    }

    /// What the element type must support: an order (`min`, `max`) or `+`
    /// (`sum`).
    pub fn element_requirement(self) -> Option<super::ElementRequirement> {
        match self {
            PipelineOp::Min | PipelineOp::Max => {
                Some(super::ElementRequirement::Order { str_allowed: false })
            }
            PipelineOp::Sum => Some(super::ElementRequirement::Number),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::tests::diagnostics;

    #[test]
    fn pipelines_type_check() {
        let diags = diagnostics(
            r#"
    class Countdown { n: i64 }
    extend Countdown {
        func next(self) -> i64? {
            if self.n == 0 { return nil }
            self.n -= 1
            self.n
        }
    }
    class Cycle[T] { items: [T], at: usize }
    extend Cycle[T] {
        func next(self) -> T? {
            if self.at >= self.items.len() { return nil }
            self.at += 1
            self.items[self.at - 1]
        }
    }
    struct P { x: i64 }
    func ok(xs: [i64], ws: [f64], ps: [P], n: i64) -> i64 {
        let a: i64 = xs.iter().map(func(x) { x * 2 }).filter(func(x) { x > 1 }).sum()
        let b: usize = xs.iter().enumerate().filter(func(p) { p.0 > 1 }).count()
        let c: f64 = ws.iter().zip(ws).fold(0.0, func(acc, p) { acc + p.0 * p.1 })
        let d: i64? = xs.iter().take_while(func(x) { x < 10 }).skip_while(func(x) { x < 3 }).max()
        let e: [f64] = xs.iter().take(3).skip(1).map(func(x) { x as f64 }).collect()
        let f: i64? = (0..n).chain(xs).chain(xs.iter().rev()).find(func(x) { x == 4 })
        let g: usize? = (0..=n).rev().map(func(i) { i * i }).position(func(x) { x > 9 })
        let h: bool = ps.iter().any(func(p) { p.x > 0 }) && ps.iter().all(func(p) { p.x < 9 })
        let i: [(usize, i64)] = (1..4).iter().zip(xs.iter()).map(func(p) { (p.0 as usize, p.1) }).collect()
        let down = Countdown { n: 3 }
        let j: i64? = down.iter().min()
        let cycle = Cycle { items: ["a", "b"], at: 0 }
        let k: str? = cycle.iter().find(func(s) { s == "b" })
        for (idx, x) in xs.iter().enumerate() { }
        for x in down { }
        for p in ps.iter().filter(func(p) { p.x > 1 }) { }
        a + b as i64 + n
    }
    "#,
        );
        assert_eq!(diags, "", "{diags}");
    }

    #[test]
    fn pipeline_misuse_is_reported() {
        let diags = diagnostics(
            r#"
    struct Steps { n: i64 }
    extend Steps { func next(self) -> i64? { nil } }
    class Wrong { n: i64 }
    extend Wrong { func next(self, by: i64) -> i64? { nil } }
    class Plain { n: i64 }
    extend Plain { func next(self) -> i64 { 0 } }
    class Ticks { n: i64 }
    extend Ticks { func next(self) -> i64? { nil } }
    func keep[T](x: T) -> T { x }
    func a(xs: [i64]) -> i64 {
        let p = xs.iter().map(func(x) { x + 1 })
        p.sum()
    }
    func b(xs: [i64]) -> i64 { keep(xs.iter()).sum() }
    func c(xs: [i64]) { xs.iter().filter(func(x) { x > 1 }); }
    func d(xs: [i64]) -> usize { xs.iter().take(2).rev().count() }
    func e(t: Ticks) -> usize { t.iter().rev().count() }
    func f(xs: [i64]) -> usize { xs.iter().zip(xs.iter().map(func(x) { x })).count() }
    func g(xs: [i64], ws: [f64]) -> usize { xs.iter().chain(ws).count() }
    func h(ws: [str]) -> str { ws.iter().sum() }
    func i(ws: [str]) -> str? { ws.iter().max() }
    func j(s: Steps) { for x in s { } }
    func k(w: Wrong) { for x in w { } }
    func l(w: Plain) { for x in w { } }
    func m(xs: [i64]) -> i64 { xs.iter().fold(0) }
    func n(xs: [i64]) {
        var k = 3
        for x in xs.iter().map(func(x) { x + k }) { k = x }
    }
    func o(xs: [i64]) -> usize { xs.iter().enumerate().rev().count() }
    func p(n: i64) -> i64 { (0..n).len() }
    func q(xs: [i64]) -> i64 { (if xs.len() > 2 { xs.iter() } else { xs.iter() }).sum() }
    "#,
        );
        let count = |needle: &str| diags.matches(needle).count();
        let escape = "exists only inside the expression that builds it";
        // `a` (the `let`), `b` (the argument), `c` (left unused) and `q` (both
        // branches of the `if`); then `b`'s `keep(..)` and `q`'s `if` where
        // they are used as pipelines.
        assert_eq!(count(escape), 5, "{diags}");
        assert_eq!(
            count("a pipeline must be one chain of calls written out in place"),
            2,
            "{diags}"
        );
        assert_eq!(
            count("`rev()` walks its source from the back"),
            3,
            "{diags}"
        );
        assert_eq!(
            count("`zip` takes an array, a range or a bare `xs.iter()`"),
            1,
            "{diags}"
        );
        assert_eq!(
            count("`chain` needs elements of the same type: this pipeline yields `i64`, the argument `f64`"),
            1,
            "{diags}"
        );
        assert_eq!(
            count("`sum` needs a pipeline of integers or floats, not of `str`"),
            1,
            "{diags}"
        );
        assert_eq!(
            count("`max` needs a pipeline of integers, floats or `bool`, not of `str`"),
            1,
            "{diags}"
        );
        assert_eq!(
            count("`Steps` is a value struct, so it cannot be an iterator"),
            1,
            "{diags}"
        );
        assert_eq!(count("`Wrong.next` takes arguments"), 1, "{diags}");
        assert_eq!(count("`Plain.next` returns `i64`"), 1, "{diags}");
        assert_eq!(
            count("cannot assign to `k` inside this loop: a closure of the pipeline it iterates reads `k`"),
            1,
            "{diags}"
        );
        assert_eq!(
            count("a range can only be used as the iterable of a `for` loop or as the source of a pipeline"),
            1,
            "{diags}"
        );
        // `m`: `fold` takes two arguments.
        assert_eq!(count("takes 2 parameters but 1"), 1, "{diags}");
    }

    #[test]
    fn effects_of_pipelines() {
        let diags = diagnostics(
            r#"
    class Ticks { n: i64 }
    extend Ticks {
        func next(self) -> i64? {
            if self.n == 0 { return nil }
            self.n -= 1
            self.n
        }
    }
    @noalloc @nopanic
    func fused(xs: [i64], ys: [i64], limit: i64) -> i64 {
        let a = xs.iter().map(func(x) { x * 2 }).filter(func(x) { x % 3 == 0 }).sum()
        let b = xs.iter().enumerate().filter(func(p) { p.1 > limit }).count() as i64
        let c = xs.iter().zip(ys).fold(0, func(acc, p) { acc + p.0 * p.1 })
        let d = xs.iter().take_while(func(x) { x < limit }).chain(0..10).skip(2).take(5).sum()
        var e: i64 = 0
        for (i, x) in (0..100).rev().zip(xs.iter()) { e += i + x }
        let f = if let m = xs.iter().rev().max() { m } else { 0 }
        let g = if let at = xs.iter().position(func(x) { x == limit }) { at as i64 } else { -1 }
        a + b + c + d + e + f + g
    }
    @noalloc
    func collects(xs: [i64]) -> [i64] { xs.iter().map(func(x) { x + 1 }).collect() }
    @noalloc
    func through_value(xs: [i64], f: func(i64) -> bool) -> usize { xs.iter().filter(f).count() }
    @realtime
    func pulls(t: Ticks) -> i64 { t.iter().sum() }
    @realtime
    func loops(t: Ticks) -> i64 {
        var s: i64 = 0
        for x in t { s += x }
        s
    }
    "#,
        );
        let count = |needle: &str| diags.matches(needle).count();
        assert_eq!(count("`fused`"), 0, "{diags}");
        assert_eq!(
            count("`collects` is `@noalloc` but allocates here: `collect` allocates the array it returns"),
            1,
            "{diags}"
        );
        assert_eq!(
            count(
                "`through_value` is `@noalloc` but allocates here: `filter` calls a function value"
            ),
            1,
            "{diags}"
        );
        assert_eq!(
            count("a loop over an iterator object runs until its `next` returns `nil`"),
            2,
            "{diags}"
        );
    }
}
