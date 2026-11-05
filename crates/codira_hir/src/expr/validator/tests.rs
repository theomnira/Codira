//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
#[cfg(test)]
use crate::utils::tests::*;

#[test]
fn test_uninitialized_access() {
    insta::assert_snapshot!(
        diagnostics(r#"
    func foo() {
        let a:i64;
        let b = a + 3;
    }
    "#), @"40..41: use of possibly-uninitialized variable"
    );
}

#[test]
fn test_uninitialized_access_if() {
    insta::assert_snapshot!(diagnostics(
        r#"
    func foo() {
        var a:i64;
        if true { a = 3; } else { a = 4; }
        let b = a + 4;  // correct, `a` is initialized either way
    }

    func bar() {
        var a:i64;
        if true { a = 3; }
        let b = a + 4;  // `a` is possibly-unitialized
    }

    func baz() {
        var a:i64;
        if true { return } else { a = 4 };
        let b = a + 4;  // correct, `a` is initialized either way
    }

    func foz() {
        var a:i64;
        if true { a = 4 } else { return };
        let b = a + 4;  // correct, `a` is initialized either way
    }

    func boz() {
        var a:i64;
        return;
        let b = a + 4;  // `a` is not initialized but this is dead code anyway
    }
    "#,
    ), @"195..196: use of possibly-uninitialized variable");
}

#[test]
fn test_uninitialized_access_while() {
    insta::assert_snapshot!(diagnostics(
        r#"
    func foo(b:i64) {
        var a:i64;
        while b < 4 { b += 1; a = b; a += 1; }
        let c = a + 4;  // `a` is possibly-unitialized
    }
    "#,
    ), @"88..89: use of possibly-uninitialized variable");
}

#[test]
fn test_free_type_alias_without_type_ref() {
    insta::assert_snapshot!(diagnostics(
        r#"
    type Foo; // `Foo` must have a target type
    "#,
    ), @"0..9: free type alias without type ref");
}

#[test]
fn test_private_leak_function_return() {
    insta::assert_snapshot!(diagnostics(
        r#"
    struct Foo(usize);

    public func bar() -> Foo { // Foo is not public
        Foo(0)
    }

    public func baz(a: usize, b: usize) -> Foo {
        Foo(2)
    }

    public struct FooBar(usize);

    public func FooBaz() -> FooBar {
        FooBar(0)
    }

    func BarBaz() -> FooBar {
        FooBar(1)
    }
    "#,
    ), @"
    41..44: can't leak private type
    121..124: can't leak private type
    ");
}

#[test]
fn test_private_leak_function_args() {
    insta::assert_snapshot!(diagnostics(
        r#"
    struct Foo(usize);

    public func bar(a: Foo, b: isize) -> usize{ // Foo is not public
        0
    }

    public func baz(a: isize, b: Foo) -> isize {
        -1
    }

    public struct FooBar(usize);

    public func FooBaz(a: FooBar) -> FooBar {
        a
    }

    func BarBaz(a: isize, b: FooBar) -> isize {
        a
    }
    "#,
    ), @"
    39..42: can't leak private type
    123..126: can't leak private type
    ");
}

#[test]
fn test_private_leak_function_scoped() {
    insta::assert_snapshot!(diagnostics(
        r#"
    // Illegal, Bar has a smaller scope than this use statement
    public(super) struct Bar;

    // Illegal, Bar has a smaller scope than this function
    public func baz() -> Bar {
        Bar
    }
    "#,
    ), @"
    Offset(66): expected a declaration
    Offset(67): expected a declaration
    Offset(72): expected a declaration
    163..166: can't leak private type
    ");
}

#[test]
fn test_private_leak_alias() {
    insta::assert_snapshot!(diagnostics(
        r#"
    type Bar = usize;

    public func baz() -> Bar {
        0
    }
    "#,
    ), @"40..43: can't leak private type");
}

#[test]
fn test_type_alias_with_private_struct() {
    insta::assert_snapshot!(diagnostics(
        r#"
    struct Foo;

    public type Bar = Foo;
    "#,
    ), @"13..35: struct `Foo` is private");
}

#[test]
fn test_type_alias_with_private_type_alias() {
    insta::assert_snapshot!(diagnostics(
        r#"
    type Foo = i32;

    public type Bar = Foo;
    "#,
    ), @"17..39: type alias `Foo` is private");
}

/// Constructs the front end parses but does not lower become `Expr::Missing`.
/// Each one must surface as a diagnostic naming what was written, so that it
/// stops the build here instead of reaching the code generator's
/// `Expr::Missing` arm and crashing the compiler.
#[test]
fn test_unlowered_constructs_are_reported_not_crashed() {
    insta::assert_snapshot!(diagnostics(
        r#"
    func with_match(x: i32) -> i32 {
        match x { 1 -> 10, _ -> 20 }
    }

    func with_closure() {
        let f = func(a: i32) -> i32 { a };
    }

    func with_force_unwrap(x: i32) -> i32 {
        x!
    }
    "#,
    ), @"
    177..179: postfix `!` unwraps an optional (`T?`), but this is `i32`
    ");
}

/// Misuses of `for`, ranges and `continue` are type errors with a message
/// saying what is allowed.
#[test]
fn test_for_range_and_continue_errors() {
    insta::assert_snapshot!(diagnostics(
        r#"
    func f(x: i32, b: bool) {
        let r = 0..10
        for i in x { }
        for i in 0..b { }
        for 1 in [1, 2] { }
        continue
    }
    "#,
    ), @"
    38..43: a range can only be used as the iterable of a `for` loop for now; there is no range value type yet
    57..58: cannot iterate over a value of type `i32`: a `for` loop iterates over a range (`a..b`, `a..=b`) or an array
    79..80: mismatched type
    76..77: a range's bounds must be integers, found `bool`
    93..94: a `for` loop's pattern must always match: use a name, `_` or a tuple of those
    113..121: `continue` outside of a loop
    ");
}

/// A closure captures by value: assigning to a captured variable, or to a
/// field of a captured value struct, inside the closure is an error. A field
/// of a captured class instance is shared storage and may be assigned, as may
/// the closure's own locals. A parameter type must come from somewhere, and
/// a channel (which lives only as long as its `scope`) cannot be captured.
#[test]
fn test_closure_capture_rules() {
    insta::assert_snapshot!(diagnostics(
        r#"
    struct Point { x: i32, y: i32 }
    class Counter { n: i32 }

    func f() {
        var x = 1
        var p = Point { x: 1, y: 2 }
        let c = Counter { n: 1 }
        let g = func() { x = 2 }
        let h = func() { p.x = 3 }
        let k = func() { c.n = 3 }
        let m = func() { var y = 1; y = 2 }
        let u = func(a) { a }
    }

    func c() {
        scope {
            let ch = channel(1)
            let w = func() { ch <- 1 }
        }
    }
    "#,
    ), @"
    166..171: cannot assign to `x` inside this closure: the closure captured `x` by value when it was created, so the assignment would change only the closure's copy; keep state the closure changes in a class instance and capture that
    195..202: cannot assign to `p` inside this closure: the closure captured `p` by value when it was created, so the assignment would change only the closure's copy; keep state the closure changes in a class instance and capture that
    293..294: cannot infer the type of this closure parameter: write it out (`x: T`), or pass the closure where a function type is expected
    372..390: this closure cannot capture `ch` of type `Channel[i32]`: a channel is a handle that lives only as long as its `scope`, so a closure cannot keep it
    ");
}

/// Function values are typed by signature; using one at the wrong type,
/// with the wrong arguments, or with `==` is a type error, and a generic
/// function cannot become a value.
#[test]
fn test_function_value_type_errors() {
    insta::assert_snapshot!(diagnostics(
        r#"
    func square(x: i32) -> i32 { x * x }
    func id[T](x: T) -> T { x }

    func f() -> bool {
        let a = square
        let b: func(i32) -> bool = square
        let c = id
        a(1, 2)
        a == a
    }
    "#,
    ), @"
    135..141: mismatched type
    154..156: the generic function `id` cannot be used as a function value yet: a value names no instantiation of it; wrap the call in a closure instead
    161..168: this function takes 1 parameters but 2 parameters was supplied
    173..179: cannot apply binary operator
    ");
}

/// Closures carry no effect handlers and are not tasks: they cannot perform,
/// a function that uses effects cannot become a value, `spawn` cannot run a
/// function value, and `break` does not leave the closure.
#[test]
fn test_closure_effects_tasks_and_loops() {
    insta::assert_snapshot!(diagnostics(
        r#"
    effect Fail {
        func fail(code: i32) -> i32
    }

    func risky() uses Fail -> i32 { perform Fail.fail(1) }

    func g() uses Fail -> i32 {
        let f = func() -> i32 { perform Fail.fail(2) }
        let h = risky
        let k = func() -> i32 { risky() }
        1
    }

    func work() {}

    func t() {
        scope {
            let f = work
            spawn f()
        }
    }

    func l() {
        loop {
            let f = func() { break }
        }
    }
    "#,
    ), @"
    196..201: `risky` uses effects, so it cannot be used as a function value: a function value carries no effect handlers
    161..181: a closure cannot `perform` an effect: a function value carries no effect handlers
    230..237: a closure cannot call `risky`, which uses effects: a function value carries no effect handlers
    323..326: `spawn` cannot run a function value: a closure may carry references to shared objects, and nothing proves them safe to use from another task; spawn a named function of this module instead
    383..388: `break` outside of a loop
    ");
}

/// A closure that captures a secret is secret; a constant-time function
/// neither creates nor calls function values; a verified function's closure
/// bodies are reported as unverified.
#[test]
fn test_closure_secrets_and_contracts() {
    insta::assert_snapshot!(diagnostics(
        r#"
    func leak(key: secret u64) -> func() -> u64 {
        func() -> u64 { key }
    }

    @constant_time
    func ct(a: secret u64) -> secret u64 {
        let f = func(x: u64) -> u64 { x }
        f(a)
    }

    func verified(x: i64) -> i64
        requires x > 0
    {
        let f = func(y: i64) -> i64 { y / x }
        f(1)
    }
    "#,
    ), @"
    50..71: secret value returned from a function whose result is not `secret` (wrap it in `declassify(..)` if revealing it is intended)
    141..166: a constant-time function cannot create a closure: whatever calls it is not known to be constant-time
    171..175: a constant-time function cannot call a function value: the callee is not known to be constant-time
    173..174: secret value passed to a function value, whose parameters are never `secret`
    241..270: this closure's body is not verified yet: the obligations in it (divisions, calls to functions with contracts) would go unchecked; move that code into a named function
    ");
}

/// A function-typed parameter of a generic function is passed on to another
/// generic function: the instantiated parameter unifies with the caller's
/// own `T`.
#[test]
fn test_function_parameter_passed_between_generic_functions() {
    insta::assert_snapshot!(diagnostics(
        r#"
    func g[T](a: T, cmp: func(T, T) -> i32) -> i32 { cmp(a, a) }
    func f[T](a: T, cmp: func(T, T) -> i32) -> i32 {
        g(a, cmp)
    }
    "#,
    ), @"");
}

/// A `match` must cover every value: a missing `bool` literal, a missing
/// `nil`, or no catch-all for an integer are each reported, naming what is
/// not covered. Patterns are checked against the scrutinee's type.
#[test]
fn test_match_exhaustiveness_and_pattern_errors() {
    insta::assert_snapshot!(diagnostics(
        r#"
    func f(n: i32, b: bool, o: i32?) {
        let a = match n { 0 -> 1, 1 -> 2 };
        let c = match b { true -> 1 };
        let d = match o { 5 -> 1, v -> v };
        let e = match o { nil -> 0, 5 -> 1 };
        let g = match n { "x" -> 1, _ -> 2 };
        let h = match n { nil -> 1, _ -> 2 };
        let i = n!;
        let ok1 = match b { true -> 1, false -> 2 };
        let ok2 = match o { nil -> 0, v -> v };
        let ok3 = match (n, b) { (0, true) -> 1, (_, _) -> 2 };
    }
    "#,
    ), @"
    47..73: `match` is not exhaustive: any `i32` no literal arm matches is not covered; add an arm for it, or a `_ ->` arm
    87..108: `match` is not exhaustive: `false` is not covered; add an arm for it, or a `_ ->` arm
    122..148: `match` is not exhaustive: `nil` is not covered; add an arm for it, or a `_ ->` arm
    162..190: `match` is not exhaustive: a present `i32` no literal arm matches is not covered; add an arm for it, or a `_ ->` arm
    214..217: this pattern is a `str`, but the value it matches is a `i32`
    256..259: `nil` only matches an optional (`T?`), not `i32`
    288..290: postfix `!` unwraps an optional (`T?`), but this is `i32`
    ");
}

/// Enum construction and variant patterns are checked against the
/// variants' payloads, and a `match` over an enum must cover every variant
/// -- through nested payload patterns too. The error names a value that is
/// not covered.
#[test]
fn test_enum_typing_and_exhaustiveness() {
    insta::assert_snapshot!(diagnostics(
        r#"
    enum Shape {
        Circle(radius: f64)
        Rectangle(width: f64, height: f64)
        Point
    }
    enum Wrap {
        One(Shape?)
        Two(bool, Shape)
    }
    func f(s: Shape, w: Wrap) {
        let a = match s { Shape.Circle(r) -> r, Shape.Point -> 0.0 };
        let b = match w {
            Wrap.One(nil) -> 0,
            Wrap.One(_) -> 1,
            Wrap.Two(true, _) -> 2,
            Wrap.Two(false, Shape.Circle(_)) -> 3,
            Wrap.Two(false, Shape.Point) -> 4,
        };
        let c = match w { Wrap.One(Shape.Point) -> 0, Wrap.Two(_, _) -> 1 };
        let d = match w { Wrap.One(Shape.Point) -> 0, Wrap.One(nil) -> 1, Wrap.Two(_, _) -> 2 };
        let ok = match w {
            Wrap.One(nil) -> 0,
            Wrap.One(Shape.Circle(_)) -> 1,
            Wrap.One(_) -> 2,
            Wrap.Two(true, _) -> 3,
            Wrap.Two(false, x) -> 4,
        };
        let e = Shape.Circle;
        let g = Shape.Point(1.0);
        let h = Shape.Circle(1.0, 2.0);
        let i = Shape.Square;
        let j = Shape.Circle(true);
        let k = match s {
            Shape.Circle -> 1,
            Shape.Point(1) -> 2,
            Shape.Rectangle(x) -> 3,
            Shape.Nope -> 4,
            Wrap.One(_) -> 5,
            _ -> 6,
        };
        let l = s == s;
        let m = w == w;
        let n = s < s;
        let Shape.Circle(z) = s;
    }
    "#,
    ), @"
    179..231: `match` is not exhaustive: `Shape.Rectangle(_, _)` is not covered; add an arm for it, or a `_ ->` arm
    245..436: `match` is not exhaustive: `Wrap.Two(false, Shape.Rectangle(_, _))` is not covered; add an arm for it, or a `_ ->` arm
    450..509: `match` is not exhaustive: `Wrap.One(nil)` is not covered; add an arm for it, or a `_ ->` arm
    523..602: `match` is not exhaustive: `Wrap.One(Shape.Circle(_))` is not covered; add an arm for it, or a `_ ->` arm
    805..817: `Shape.Circle` carries a payload: construct it with `Shape.Circle(_)` (variants are not function values)
    831..847: `Shape.Point` has no payload: write it without parentheses
    861..883: `Shape.Circle` takes 1 payload value, but 2 were given
    897..909: enum `Shape` has no variant `Square`
    936..940: mismatched type
    973..985: `Shape.Circle` carries a payload: match it with `Shape.Circle(_)`
    1000..1014: `Shape.Point` has no payload: match it without parentheses
    1029..1047: `Shape.Rectangle` has 2 payload fields, but this pattern has 1
    1062..1072: enum `Shape` has no variant `Nope`
    1087..1098: this pattern is a `Wrap` variant, but the value it matches is a `Shape`
    1160..1166: `==` compares only enums whose payloads are all numbers or `bool`s, and `Wrap` has others; `match` on it instead
    1180..1185: enums (`Shape`) are not ordered; compare them with `==`/`!=` or `match` on them
    1195..1210: a `let` pattern must always match: use a name, `_` or a tuple of those, and `if let` or `match` for a pattern that may not
    ");
}

/// A generic enum is instantiated at every use: construction learns its
/// arguments from the payload and the expected type, a pattern reads the
/// payload types through the matched value's arguments, and `match` over
/// an instantiation must cover every variant.
#[test]
fn test_generic_enum_typing_and_exhaustiveness() {
    insta::assert_snapshot!(diagnostics(
        r#"
    enum Result[T, E] { Ok(T), Err(E) }
    enum Maybe[T] { Just(T), Nothing }
    struct P { x: i64 }

    func ok() -> Result[i64, bool] { Result.Ok(1) }
    func err() -> Result[i64, bool] { Result.Err(true) }
    func wrong() -> Result[i64, bool] { Result.Err(1) }
    func nested() -> Maybe[Maybe[i64]] { Maybe.Just(Maybe.Nothing) }
    func f(r: Result[i64, bool], m: Maybe[P]) -> i64 {
        let a: i64 = match r { Result.Ok(v) -> v, Result.Err(_) -> 0 };
        let b: i64 = match r { Result.Ok(v) -> v };
        let c: bool = match r { Result.Ok(v) -> v, Result.Err(e) -> e };
        let d = match m { Maybe.Just(p) -> p.x, Maybe.Nothing -> 0 };
        let e = Result.Ok(1) == Result.Err(2);
        let g = m == m;
        let h: Result[i64, bool] = Maybe.Nothing;
        if let Result.Err(flag) = r { let k: bool = flag; }
        a + b + d
    }
    "#,
    ), @"
    240..241: mismatched type
    446..475: `match` is not exhaustive: `Result.Err(_)` is not covered; add an arm for it, or a `_ ->` arm
    521..522: mismatched type
    541..542: `match` arms have incompatible types: `i64` and `bool`
    495..544: mismatched type
    667..673: `==` compares only enums whose payloads are all numbers or `bool`s, and `Maybe` has others; `match` on it instead
    706..719: mismatched type
    ");
}

/// `x?` on a `Result` is its `Ok` payload and needs the enclosing function
/// to return a `Result` of the same declaration with the same error type;
/// on an optional it is the payload and needs an optional result. Anything
/// else is an error that says which rule is broken.
#[test]
fn test_propagate_typing() {
    insta::assert_snapshot!(diagnostics(
        r#"
    enum Result[T, E] { Ok(T), Err(E) }
    enum Other[T, E] { Ok(T), Err(E) }
    func get() -> Result[i64, bool] { Result.Ok(1) }
    func opt() -> i64? { 1 }

    func fine() -> Result[str, bool] { let n: i64 = get()?; Result.Ok("x") }
    func fine_opt() -> f64? { let n: i64 = opt()?; 2.0 }
    func closure() -> i64 {
        let f = func() -> Result[i64, bool] { Result.Ok(get()? + 1) };
        0
    }
    func wrong_error() -> Result[i64, str] { Result.Ok(get()?) }
    func not_result() -> i64 { get()? }
    func not_optional() -> i64 { opt()? }
    func other() -> Other[i64, bool] { Other.Ok(get()?) }
    func on_int(x: i64) -> i64? { x? }
    func on_other(o: Other[i64, bool]) -> Other[i64, bool] { o? ; o }
    "#,
    ), @"
    422..428: `?` would return this `Err(bool)` from a function that returns `Result[i64, str]`: the error types must be the same
    459..465: `?` on a `Result[i64, bool]` returns its `Err` early, so the enclosing function must return a `Result[_, bool]`, but it returns `i64`
    497..503: `?` on an optional returns `nil` early, so the enclosing function must return an optional, but it returns `i64`; unwrap with `!`, or test with `if let`
    550..556: `?` on a `Result[i64, bool]` returns its `Err` early, so the enclosing function must return a `Result[_, bool]`, but it returns `Other[i64, bool]`
    590..592: `?` applies to a `Result[T, E]` or an optional `T?`, but this is `i64`
    652..654: `?` applies to a `Result[T, E]` (an enum `Result` with variants `Ok(T)` and `Err(E)`) or an optional `T?`, but this is `Other[i64, bool]`
    ");
}

/// `x?` wraps `x`'s error in the variant of the function's error enum that
/// holds exactly that type; none or two such variants are errors.
#[test]
fn test_propagate_wraps_errors() {
    insta::assert_snapshot!(diagnostics(
        r#"
    enum Result[T, E] { Ok(T), Err(E) }
    enum IoError { Missing, Denied }
    enum Problem { Io(IoError), Parse(i64), Other }
    enum Twice { A(i64), B(i64) }
    func read() -> Result[i64, IoError] { Result.Err(IoError.Missing) }
    func parse() -> Result[i64, i64] { Result.Err(3) }

    func both() -> Result[i64, Problem] { Result.Ok(read()? + parse()?) }
    func twice() -> Result[i64, Twice] { Result.Ok(parse()?) }
    func none() -> Result[i64, Problem] { let b: Result[i64, bool] = Result.Ok(1); Result.Ok(b?) }
    "#,
    ), @"
    384..392: `?` cannot tell which variant of `Twice` to wrap this `i64` error in: `Twice.A`, `Twice.B` all hold one; `match` and build the error
    485..487: `?` would return this `Err(bool)` from a function that returns `Result[i64, Problem]`: the error types must be the same, or `Problem` must have exactly one variant holding a `bool`, which `?` wraps the error in
    ");
}

/// Declarations an enum cannot have yet, or ever: bounds on its
/// parameters, a generic type without its arguments in a payload, braced
/// variants, a variant name used twice, and containing itself by value --
/// a generic one included.
#[test]
fn test_enum_declaration_errors() {
    insta::assert_snapshot!(diagnostics(
        r#"
    enum Option[T] { Some(T), Nothing }
    enum Bounded[T: Shape] { One(T) }
    trait Shape { func area(self) -> i64; }
    enum Bare { One(Option) }
    enum Tree[T] { Leaf, Node(Tree[T], T, Tree[T]) }
    enum Forest[T] { Leaf(T), Node([Forest[T]]) }
    enum Dup { A, B, A }
    enum Braced { A { x: i32 } }
    enum List { Empty, More(i32, List) }
    enum Fine { Empty, More(i32, Holder) }
    class Holder { list: Fine }
    "#,
    ), @"
    52..57: bounds on an enum's own parameters are not supported; bound the `extend` block that needs them instead (`extend Bounded[T: Trait]`)
    110..135: the payload type `Option[T]` mentions the generic parameter `T`, which `Bare` does not declare: a generic type needs its arguments written out, e.g. `Box[i32]` rather than `Box`
    136..184: `Tree` contains itself by value, so it would have no finite size; hold the recursive part through a `class` or an array
    248..249: `Dup` already has a variant called `A`
    266..278: a variant with braced fields is not supported yet; write `A(label: Type, ..)` instead
    281..317: `List` contains itself by value, so it would have no finite size; hold the recursive part through a `class` or an array
    ");
}

/// `T` converts to `T?` where an optional is expected; `nil` takes the
/// optional type it is used as; optionals compare with `==`/`!=` but are
/// not ordered.
#[test]
fn test_optional_typing() {
    insta::assert_snapshot!(diagnostics(
        r#"
    struct P { x: i32 }

    func f() -> i32? {
        let a: i32? = 5;
        let b: i32? = nil;
        let c = a == b;
        let d = a < b;
        let p: P? = nil;
        let q = p == p;
        let r = p == nil;
        let unknown = nil;
        if let x = a { return x }
        nil
    }
    "#,
    ), @"
    116..121: optionals (`i32?`) are not ordered; compare them with `==`/`!=` or unwrap them first
    156..162: `P` values cannot be compared with `==`; compare the optional with `nil` instead
    204..207: type annotations needed: the type of this expression is ambiguous
    194..201: type annotations needed: the type of this expression is ambiguous
    ");
}

/// The binding named by every "cannot assign to `x`: it is bound with
/// `let`" diagnostic of `source`, in order.
fn let_assignments(source: &str) -> Vec<String> {
    diagnostics(source)
        .lines()
        .filter(|line| line.contains("it is bound with `let`"))
        .map(|line| {
            let name = line.split("cannot assign to `").nth(1).unwrap();
            name.split('`').next().unwrap().to_owned()
        })
        .collect()
}

/// `let` binds once: assigning to the binding, compound assignment, and
/// assigning to a field of a value struct or a tuple element it holds are
/// errors, as is assigning a first value to a `let` declared without one.
/// `var`, `let mut`, parameters, the fields of a class and the elements of
/// an array (storage of a shared object, not of the binding) stay
/// assignable.
#[test]
fn test_let_bindings_cannot_be_assigned() {
    let source = r#"
    struct Point { x: i32, y: i32 }
    class Counter { n: i32 }

    func f(q: i32) {
        let a = 1
        a = 2
        a += 1
        let p = Point { x: 1, y: 2 }
        p.x = 3
        let (u, v) = (1, 2)
        u = 5
        let d: i32
        d = 1
        let c = Counter { n: 1 }
        c.n = 3
        let arr = [1, 2]
        arr[0] = 5
        var b = 1
        b = 2
        let mut m = 1
        m += 2
        q = 4
        var w = Point { x: 1, y: 2 }
        w.y = 4
    }
    "#;
    assert_eq!(let_assignments(source), ["a", "a", "p", "u", "d"]);
}
