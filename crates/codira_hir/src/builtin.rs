//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Functions the compiler provides itself, with no declaration in any source
//!   file: `print`, `println` and `channel`, and the `len`/`unchecked_get`
//!   methods of arrays and `str`.
//!
//! # Why these are built in rather than library functions
//!
//! `println("x = ", x)` takes any number of arguments of any printable type.
//! Codira has neither variadic parameters nor overloading yet, so no
//! declaration written in Codira can have that signature -- the standard
//! library cannot provide it, only the compiler can. (The same is true of
//! Swift's `print(_ items: Any...)` and of Rust's `println!`, which is a
//! macro for exactly this reason.)
//!
//! # Resolution
//!
//! A builtin is what a bare call to its name means *when nothing in scope
//! defines that name*. Any user definition -- a local, a function, an
//! import -- wins, so existing code that declares its own `print` (the
//! standard library's testing suite does) keeps calling its own.
//!
//! # Lowering
//!
//! Inference records each builtin call in
//! [`InferenceResult::builtin_call`](crate::InferenceResult::builtin_call);
//! the code generator lowers every argument to one call to a runtime
//! function chosen by the argument's type (`codira.print_str`,
//! `codira.print_i64`, ...), which the runtime registers in its dispatch
//! table alongside `new` and `new_array`.

use crate::name::Name;

mod array;

pub use array::{
    is_inlined_closure_arg, is_inlined_closure_literal, ArrayMethod, ElementRequirement,
};
mod string;

pub use string::{StringArg, StringOp};

/// A function provided by the compiler rather than declared in source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinFn {
    /// `print(a, b, ..)` writes each argument in turn to standard output,
    /// with no separator and no trailing newline.
    Print,
    /// `println(a, b, ..)` is `print` followed by a newline. With no
    /// arguments it writes just the newline.
    Println,
    /// `declassify(e)` returns `e` unchanged and marks it public for the
    /// information-flow checker (`verify::flow`). No code of its own.
    Declassify,
    /// `ct_select(c, a, b)` is `if c { a } else { b }` evaluated without a
    /// branch: both operands are computed and code generation emits one
    /// LLVM `select` marked `!unpredictable`. The way to choose on a
    /// `secret` in a constant-time function.
    CtSelect,
    /// `channel(capacity)` creates a bounded `Channel[T]`, owned by the
    /// enclosing `scope` (see `expr::validator::tasks`).
    Channel,
    /// `array_filled(value, count)` creates a `[T]` of `count` copies of
    /// `value`, an integer, float or `bool`. The only way a program can make
    /// an array whose length is not written out in the source.
    ArrayFilled,
    /// `select(c, a, b)` is `if c { a } else { b }` for integers, floats and
    /// `bool`s: both operands are evaluated and code generation emits one
    /// LLVM `select`, which the backend lowers to a conditional move or a
    /// blend. Unlike `ct_select` it carries no `!unpredictable` promise and
    /// accepts floats.
    Select,
    /// `likely(c)` returns `c` and tells the optimizer it is usually true.
    Likely,
    /// `unlikely(c)` returns `c` and tells the optimizer it is usually false.
    Unlikely,
    /// `fma(a, b, c)` is `a * b + c` rounded once, for `f32` and `f64`.
    Fma,
    /// `array.len()`: the number of elements, as `usize`.
    ArrayLen,
    /// `array.unchecked_get(i)`: the element at `i` with no bounds check.
    ///
    /// Indexing (`array[i]`) checks the index and panics with the source
    /// location when it is out of range. This is the escape hatch for a hot
    /// loop whose index is already known to be in range; an out-of-range
    /// index here reads arbitrary memory.
    ArrayUncheckedGet,
    /// `s.len()` on a `str`: its length in bytes, as `usize`.
    StrLen,
    /// A method of `[T]` that grows, searches, orders or iterates it
    /// (`push`, `sort`, `map`, ...; see [`ArrayMethod`]).
    Array(ArrayMethod),
    /// A method of `String`, `to_string()` on a primitive, or
    /// `string_from_utf8` (see [`StringOp`]).
    String(StringOp),
}

impl BuiltinFn {
    /// Every builtin called as a free function, for lookup by name.
    const FREE_FUNCTIONS: &'static [BuiltinFn] = &[
        BuiltinFn::Print,
        BuiltinFn::Println,
        BuiltinFn::Declassify,
        BuiltinFn::CtSelect,
        BuiltinFn::Channel,
        BuiltinFn::ArrayFilled,
        BuiltinFn::Select,
        BuiltinFn::Likely,
        BuiltinFn::Unlikely,
        BuiltinFn::Fma,
        BuiltinFn::String(StringOp::FromUtf8),
    ];

    /// The name a program calls this builtin by.
    pub fn name(self) -> &'static str {
        match self {
            BuiltinFn::Print => "print",
            BuiltinFn::Println => "println",
            BuiltinFn::Declassify => "declassify",
            BuiltinFn::CtSelect => "ct_select",
            BuiltinFn::Channel => "channel",
            BuiltinFn::ArrayFilled => "array_filled",
            BuiltinFn::Select => "select",
            BuiltinFn::Likely => "likely",
            BuiltinFn::Unlikely => "unlikely",
            BuiltinFn::Fma => "fma",
            BuiltinFn::ArrayLen | BuiltinFn::StrLen => "len",
            BuiltinFn::ArrayUncheckedGet => "unchecked_get",
            BuiltinFn::Array(method) => method.name(),
            BuiltinFn::String(op) => op.name(),
        }
    }

    /// The builtin free function called `name`, if there is one.
    pub fn from_name(name: &Name) -> Option<BuiltinFn> {
        let text = name.as_str()?;
        Self::FREE_FUNCTIONS
            .iter()
            .copied()
            .find(|it| it.name() == text)
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::tests::diagnostics;

    #[test]
    fn the_new_builtins_check_their_arguments() {
        let diags = diagnostics(
            r#"
    func a(n: usize) -> [i64] { array_filled(0i64, n) }
    func b(n: usize) -> [f64] { array_filled(1.5, n) }
    func c(n: usize) -> [i64] { array_filled(0i64) }
    func d(n: usize) -> [i64] { array_filled("x", n) }
    func e(c: bool, x: f64, y: f64) -> f64 { select(c, x, y) }
    func f(c: i64, x: f64, y: f64) -> f64 { select(c, x, y) }
    func g(x: f64, y: f64) -> f64 { select(true, x) }
    func h(x: f64, y: f64, z: f64) -> f64 { fma(x, y, z) }
    func i(x: i64, y: i64, z: i64) -> i64 { fma(x, y, z) }
    func j(x: f64, y: f32, z: f64) -> f64 { fma(x, y, z) }
    func k(c: bool) -> bool { likely(c) && unlikely(c) }
    func l(c: i64) -> bool { likely(c) }
    "#,
        );
        let count = |needle: &str| diags.matches(needle).count();
        assert_eq!(
            count("`array_filled` takes a value and a count, found 1 argument(s)"),
            1,
            "{diags}"
        );
        assert_eq!(
            count("`array_filled` makes an array of integers, floats or `bool`s"),
            1,
            "{diags}"
        );
        assert_eq!(
            count("`select` takes a condition and two values, found 2 argument(s)"),
            1,
            "{diags}"
        );
        assert_eq!(
            count("`fma` takes `f32` or `f64` operands, not `i64`"),
            1,
            "{diags}"
        );
        // `f` (integer condition), `j` (mixed widths) and `l` (integer hint)
        // are mismatches the unifier reports.
        assert!(diags.lines().count() >= 7, "{diags}");
    }
}
