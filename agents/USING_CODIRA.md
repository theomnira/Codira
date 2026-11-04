<!--
Copyright (c) 2026 Omnira CJSC
Author: Tunjay Akbarli
Date: August 6, 2026

Functionality: Instructions for AI coding agents — how to USE the Codira
programming language to write projects (NOT how to build the compiler).
 -->

# Using Codira — Agent Instructions

This document teaches an AI coding agent how to **use** the Codira programming
language to write and run programs. It is *not* about building the compiler;
for compiler-internals work see `spec/LANGUAGE_SPEC.md` §12-§15 and the
`crates/**` sources.

Codira is an ahead-of-time (AOT) compiled, statically typed, hot-reloadable
systems language with an LLVM backend. It compiles to natively executable
machine code, cross-compiles, and ships as reusable libraries.

> **Ground truth** — Language details change fast. Always prefer
> `spec/LANGUAGE_SPEC.md` over this guide when they disagree. This guide is a
> friendly summary of the current (v26.8.1) surface.

---

## 1. What Codira is not

- No `null` / undefined reference value. Absence is spelled `Type?` and handled
  explicitly.
- No template angle brackets — generics use **square brackets** `[T]`.
- `;` is *optional* at the end of a line-terminated statement. The parser is
  not indentation-sensitive; it just treats `;` as always-legal.
- No borrow checker / ownership model. `struct` = value (copied on pass),
  `class` = GC-managed reference (shared mutability).
- No standalone executable by default — you produce a Codira "library"
  (`*.codiralib`) and run it via the `codira start` CLI.

---

## 2. Project layout

A Codira project is a directory holding the source plus a manifest. Example:

```
my_program/
├── src/
│   └── mod.code      # module source (must end in `.code`)
└── codira.toml       # manifest
```

- Source files always end in **`.code`**. Multi-word file names use
  underscores. One file = one module; directories nest modules.
- The manifest (`codira.toml`) declares package metadata:
  ```toml
  [package]
  name="my_program"
  authors=[]
  version="0.1.0"
  ```
- Compiling yields `target/mod.codiralib`; `codira start` invokes a chosen
  entry-point function.

### Scaffold a project

```bash
codira new my_program      # creates a new directory + project
codira init .             # initializes the current directory (or [path])
```

The generated `src/mod.code` starts as:

```
public func main() -> f64 {
    3.14159
}
```

---

## 3. Writing Codira source

### Lexical rules

Full reserved keyword list (the modern, non-Rust surface):

```
as break class comptime effect else enum extend extern false for
func handle if import in init internal let loop macro match never
nil override perform private public resume return root self static
struct super true trait uses var where while
```

The path separator is **`.`** (like Python/Swift), not `::`.

### Functions

```codira
func add(a: i32, b: i32) -> i32 {
    a + b
}
```

Visibility: `private` (default — write nothing), `internal` (same package),
`public` (exported):

```codira
public func distance_squared(a: Point, b: Point) -> f64 {
    let dx = a.x - b.x
    let dy = a.y - b.y
    dx * dx + dy * dy
}
```

Implicit return — the last expression is the return value; `return x` also works.

Every argument is passed by copy (a class instance is a reference, so the
callee and the caller share its fields). There are no in-place
parameters yet: `inout`, `mut` and `out` before a parameter, and the
mutable reference types `mut T` and `&mut T` anywhere, are rejected ("...
are not supported yet"). Return the new value instead
(`func bump(s: S) -> S`), or pass a class instance. `consuming`,
`borrowing` and `read` are accepted and mean nothing beyond a copy (no
move checking), and `&T` is accepted as `T`.

A function that never returns declares the result `never` (`Never` in
`std.builtin.type_aliases`): its body must diverge (`loop {}` without a
`break`, or a call to another such function), and a call to it can stand
where any type is expected (`if c { x } else { fail(3) }`):

```codira
extern "C" { func codira_exit(code: i32) -> never; }
public func fail(code: i32) -> never { codira_exit(code) }
```

Compiled as a function returning nothing that LLVM is told does not return
(`noreturn`), with `unreachable` after each call; its reflected signature
names `()` as the result, so the runtime's hooks that do not return
(`codira_abort`, `codira_exit`) are registered with a unit result. A `uses`
function returning `never` can still come back when a handler aborts it,
and is not `noreturn`. The Cranelift `--jit` backend compiles a call to one
as a trap after the call.

### Bindings

```codira
let pi = 3.14159     // immutable
var counter = 0       // mutable
counter += 1
```

`var` replaces `let mut`; there is no standalone `mut` keyword.

A `let` binding cannot be assigned after its declaration: `x = ..`,
`x += ..`, and assigning to a field of a value struct or an element of a
tuple held in it (`p.x = ..`) are errors ("cannot assign to `x`: it is
bound with `let`..."). That includes a `let` declared without an
initializer: there is no deferred initialization of a `let`, so write
`var x: T` and assign it. Through a `let` you may still change what the
binding *refers to* but does not hold: a field of a class instance
(`c.n = 3`) and an element of an array (`a[0] = 5`). Parameters are
mutable local copies.

Bindings exist only inside functions. A module-level `let` or `var`
(`public let LIMIT: i64 = 10;`) is rejected ("module-level `let` is not
supported yet"): write a function returning the value instead, as
`std.math.constants` does (`public func LIMIT() -> i64 { 10 }`).

### Loops and arrays

```codira
for i in 0..10 { }            // 0, 1, ..., 9
for i in 1..=n { }            // 1, ..., n (stops at the type's max, never wraps)
for x in xs { }               // each element of the array `xs`
for (a, b) in pairs { }       // tuple patterns destructure each element
while cond { if skip { continue } }
loop { if done { break } }

let xs = [3, 1, 4]
xs.len()                      // 3, as usize
xs[i]                         // bounds-checked: an out-of-range index panics
xs.unchecked_get(i)           // no check, for hot loops with a proven index
```

A range (`a..b`, `a..=b`) is only valid as a `for` loop's iterable for now --
there is no range value type. `continue` is a contextual keyword: it means
"next iteration" only when it stands alone (before `}`, `;` or a line break).
`&&` and `||` short-circuit.

An out-of-range index prints `panic at mod.code:7:13: index out of bounds: the
length is 3 but the index is -1` to stderr and ends the program with exit
status 101. A panic cannot be caught from Codira yet.

### Hot-loop builtins: `array_filled`, `select`, `fma`, `likely`, `unlikely`

```
let xs = array_filled(0.0, n)        // [f64] of n copies; n is a usize
let m  = select(a > b, a, b)         // both operands evaluated, then one LLVM `select`
let y  = fma(x, w, acc)              // x * w + acc, rounded once (f32 or f64)
if unlikely(i >= limit) { .. }       // branch weights for the optimizer
```

- `array_filled(v, n)` is the only way to make an array whose length is not
  written out in the source. `v` is an integer, float or `bool`; the fill is a
  counted loop that LLVM turns into a `memset` or vector stores. It allocates,
  so `@noalloc` rejects it.
- `select(c, a, b)` takes integers, floats or `bool`s (it is a call; the
  channel statement `select { .. }` is a different construct, told apart by the
  `{`). Unlike `ct_select` it
  makes no promise about the machine code (the backend may emit a branch), so
  a secret condition is an error: use `ct_select` there.
- `fma(a, b, c)` is `llvm.fma`: one instruction on a target that has FMA
  (`--target-cpu haswell`, `znver4`, ...), a libm call on the default x86-64
  target, which is correct but slower than `a * b + c`.
- `likely(c)` / `unlikely(c)` return `c` and become `llvm.expect`, then branch
  weights. The verifier treats them as the identity.
- Float sums are not reassociated, so LLVM will not vectorize a float
  reduction on its own. Write the accumulators out (`a0..a3`, each updated with
  `fma`) to get independent dependency chains; integer reductions vectorize at
  `-O3` without help. There are no vector (`<4 x f64>`) types in the language
  yet: lane-level control and an automatic accumulator split are not done.
- Tests that read the generated IR: `crates/codira/tests/perf_builtins.rs`.

### Records (value) vs classes (reference)

```codira
// Value type: copied on assignment/pass, value semantics.
struct Point {
    x: f64
    y: f64
}

// Reference type: heap-allocated, garbage collected, shared mutability.
class Actor {
    var position: Point
    var health: f64
}
```

### Methods via `extend` (was `impl`)

The type body holds **fields only**; methods and `init` live in an `extend` block:

```codira
extend Actor {
    init(position: Point) {
        self.position = position
    }
    func take_damage(var self, amount: f64) {
        self.health -= amount
    }
}
```

### Enums & pattern matching

```codira
enum Shape {
    Circle(radius: f64)                  // labels are documentation
    Rectangle(width: f64, height: f64)
    Point
}

func area(shape: Shape) -> f64 {
    match shape {
        Shape.Circle(radius) -> 3.14159 * radius * radius
        Shape.Rectangle(width, height) -> width * height
        Shape.Point -> 0.0
    }
}

let s = Shape.Circle(2.0)                // construct: `Enum.Variant(payload..)`
let p = Shape.Point                      // a unit variant has no parentheses
if let Shape.Circle(r) = s { println(r) }
while let Step.Next(n) = current { .. }
let same = Shape.Point == p              // `==`/`!=`: see below
```

- An enum is a **value** type (copied like a `struct`): a `u32` tag plus one
  typed slot per payload field, where variants share slots of the same type
  (`Circle.radius` and `Rectangle.width` are the same `f64` slot). No
  allocation. It can be a field of a `struct` or `class`, an array element,
  a parameter or result (also across modules), a payload of another enum,
  and hold class references (traced by the collector).
- Payloads are positional: `Shape.Rectangle(w, h)` builds and matches.
  Patterns nest: `Token.Wrapped(Shape.Circle(r))`, `Token.Pair(0, y)`,
  `Token.Maybe(nil)`.
- `match` must be exhaustive, through nested payload patterns too; the error
  names a value no arm covers (`` `Wrap.Two(false, Shape.Rectangle(_, _))` is
  not covered ``).
- `==`/`!=` compare enums whose payloads are all numbers or `bool`s (tag and
  payload); others are an error (`match` instead). Enums are not ordered.
- `extend Shape { func area(self) -> f64 { .. } }` adds methods and
  associated functions (`Shape.unit()` when `unit` is not a variant name).
- A `let` or parameter pattern must be irrefutable: use `if let`/`match`.
- Not supported (each a clear error): generic enums (`enum Option[T]`),
  braced variants (`Circle { radius: f64 }`), an enum containing itself by
  value (hold the recursion through a `class`), variants as function values
  (`let f = Shape.Circle`), printing an enum, and enums in
  `codira run --jit`, the MIR/e-graph path and quantum kernels. The verifier
  treats a payload a pattern binds as an unknown value.
- Hot reload: a stored enum keeps its tag and the slots whose names (`f64.0`,
  `i64.0`, ..) survive; new slots start at zero. Append new variants at the
  end: the tag is the declaration index, so reordering variants changes what
  stored values mean, and a value whose variant was removed makes a later
  `match` stop with "no `match` arm matches this value".

### Generics

Generic functions, structs and classes build and run. Each use at concrete
types is compiled separately (monomorphisation): `id(1)` and `id(2.5)` run
two functions, `Box[i32]` and `Box[f64]` are two types with their own
layouts.

```codira
struct Pair[A, B] { a: A, b: B }
class Box[T] { value: T }

extend Box[T] {                          // methods of every Box[T]
    func get(self) -> T { self.value }
    func new(value: T) -> Box[T] { Box { value: value } }
}

func pick[T](a: T, b: T, first: bool) -> T {
    if first { a } else { b }
}

func main() -> f64 {
    let b: Box[i32] = Box { value: 41 }  // explicit arguments, in type position
    let p = Pair { a: 3, b: 0.5 }        // inferred: Pair[i64, f64]
    let n = Box.new(p)                   // Box[Pair[i64, f64]]
    pick(1.5, 2.5, b.get() > 40)
}
```

- Type arguments are written only in type position (`Box[i32]`); at calls
  they are always inferred from the arguments and the expected type. A
  parameter nothing determines (`func size[T]() -> i64` called as `size()`)
  is an error at the call.
- A value of type `T` can be moved, copied, passed, returned and stored,
  and the methods of its bounds called (see "Traits and bounds" below).
  Operators -- `a + b`, `a < b`, `-a` -- are not resolved through traits:
  on a `T` they are a type error saying so; declare the operation as a
  trait method and call that.
- A generic type needs its arguments outside its own declaration: `b: Box`
  is an error, write `b: Box[i32]`.
- Generics cross modules as source: a module calling another module's
  generic function compiles the instance itself (and a private helper the
  function calls, which its module does not export). A generic function has
  no code of its own, so it is never exported to the host, even when
  `public`; `codira start`/`invoke` can only call non-generic functions.
  `@export`, `@asm` and `@test` cannot be put on a generic function, and a
  generic function cannot have a `uses` clause or be `spawn`ed yet.
- A function calling itself at an ever larger type (`f(Box { v: x })`
  inside `f[T]`) has no finite set of instances and is reported.
- The Cranelift backend (`codira run --jit`) does not compile generic
  functions; a program using them runs on LLVM.

#### Traits and bounds

A `trait` declares methods; a type implements it in an `extend` block that
names it; a generic parameter bounded by it can call those methods. Every
call is resolved at compile time, per instantiation (static dispatch, no
vtables): `total(r)` for a `Rect` calls `Rect`'s `area` directly.

```codira
trait Shape {
    func area(self) -> i64;                          // required
    func describe(self) -> i64 { self.area() * 10 }  // default body
}
trait Named: Shape {                                 // supertrait
    func id(self) -> i64;
}
trait Make {
    func make() -> Self;                             // no `self`: associated
}

struct Rect { w: i64, h: i64 }
struct Box[T] { v: T }

extend Rect: Shape {                                 // the implementation
    func area(self) -> i64 { self.w * self.h }       // `describe`: default
}
extend Rect: Named { func id(self) -> i64 { 7 } }
extend Rect: Make { func make() -> Rect { Rect { w: 2, h: 5 } } }
extend i64: Shape { func area(self) -> i64 { self * self } }
extend Box[T: Shape]: Shape {                        // Box[T] is a Shape
    func area(self) -> i64 { self.v.area() + 1 }     // when T is
}

func total[T: Shape](x: T) -> i64 { x.area() + x.describe() }
func both[T](x: T) -> i64 where T: Named { x.id() + x.area() }
func fresh[T: Shape + Make](x: T) -> i64 { T.make().area() + x.area() }
```

- Bounds: `[T: A + B]`, `where T: A + B` (only on the function's own
  parameters), and on an `extend` block's parameters, `extend Box[T: A]`,
  which hold in all its methods (with or without a trait after the `:`).
  A bound gives `T` its traits' methods and their supertraits' methods;
  `T.make()` calls an associated function of a bound. A method name two
  bounds both declare is an error at the call.
- At every call of a bounded function, the type each parameter stands for
  must implement its bounds: ``the type `Circle` does not implement the
  trait `Shape`, which `total` requires of its parameter `T` ``. An
  integer literal is `i32` unless something says otherwise, so with only
  `extend i64: Shape`, `total(5)` is that error; write `let n: i64 = 5`.
- Implementations are checked: every method without a default present,
  each method's signature equal to the trait's with `Self` read as the
  extended type, no other methods in the block, every supertrait
  implemented, and at most one implementation of a trait per type (two
  generic implementations whose types can overlap, `Box[T]` and
  `Box[i64]`, count as two).
- What can implement a trait: structs, classes, enums, generic structs
  and classes (`extend Box[T: Shape]: Shape`), and the integer and float
  types, `bool` and `str`. A struct, class or enum is extended in its own
  package (as for any `extend`); a primitive type only in the package that
  declares the trait. A trait method is called like any method on a
  concrete type, `r.area()`, including a default one it does not override,
  `r.describe()`; methods of a trait implementation are visible wherever
  the type is.
- A trait and its methods cross modules like other items: `import
  shapes.{Shape, total}`; the program implementing `Shape` and calling
  `total` instantiates it.
- `@noalloc`, `@nopanic`, `@noblock`, `@realtime`: the analysis runs once
  per generic body, before an implementation is chosen, so a trait method
  called on a type parameter -- including inside a default body, which is
  written in terms of `Self` -- is treated as having every effect. A
  promise-keeping function must call the implementation directly
  (`r.area()`), not through a bound. The verifier likewise knows nothing
  about the result of a call through a bound.
- Not supported, each a clear error: generic traits (`trait Iterable[E]`)
  and so a bound with arguments (`T: Iterable[i64]`); generic methods in a
  trait; `requires`/`ensures` on a trait method (put them on the
  implementations); a `where` clause on a trait; bounds on a struct's own
  parameters (`struct Box[T: Shape]`: bound the `extend` instead); a trait
  named on a declaration (`struct P: Shape`: write `extend P: Shape`); a
  trait used as a type (`x: Shape`: there are no trait objects or dynamic
  dispatch); operators on a type parameter. There are no associated types
  or constants, no blanket implementations (`extend T: Shape`), no
  negative or specialised implementations, and one type cannot implement
  two traits that declare a method of the same name (a type's methods
  share one namespace). Generic enums do not exist, so `Option[T]`-style
  traits cannot be written yet.
- `codira run --jit` (Cranelift) does not compile method calls or generic
  functions: it says so, and the program runs on LLVM.

### Optional types

```codira
func find(items: [i32], target: i32) -> i32? {
    for item in items {
        if item == target { return item }   // an `i32` becomes an `i32?` here
    }
    nil
}

if let value = find(xs, 7) {      // binds the value when present
    println(value)
} else {
    println("not found")
}

let forced = find(xs, 7)!         // panics with its location on `nil`
let missing = find(xs, 8) == nil  // `==`/`!=` work; optionals are not ordered
while let v = next() { }          // loops while present
```

`T?` is a value -- a present flag plus the payload, no allocation. A `T` is
accepted wherever a `T?` is expected. `nil` needs an optional type from
context (`let x = nil` alone is an error: annotate it).

### `match`

```codira
match n {
    0 -> "zero",
    1 -> "one",
    _ -> "many",
}

match (a, b) { (0, _) -> 0, (1, y) -> y, (x, y) -> x + y }
match name { "alice" -> 1, _ -> 2 }      // string literals compare bytes
match maybe { nil -> 0, 1 -> 10, v -> v } // other patterns match the value inside
```

Patterns: literals (integer, float, `bool`, string, `nil`), names, `_`,
tuples, and enum variants (`Shape.Circle(r)`, `Shape.Point`), nested. A
`match` must be exhaustive -- both `bool`s, `nil` plus the present values of
an optional, every variant of an enum, and a `_`/name arm for integers,
floats and strings -- and the error names a value that is not covered. A
value no arm matches at run time (only possible for an enum after a hot
reload removed its variant) stops the program with its location.

### Function values and closures

```codira
func square(x: i32) -> i32 { x * x }

func apply(f: func(i32) -> i32, x: i32) -> i32 { f(x) }

func adder(a: i32) -> func(i32) -> i32 {
    func(b: i32) -> i32 { a + b }      // a closure literal; captures `a`
}

class Counter { n: i32 }

func make_counter() -> func() -> i32 {
    let state = Counter { n: 0 }
    func() -> i32 { state.n += 1; state.n }   // state lives in a captured object
}

class Button { on_click: func(i32) -> i32 }

public func main() -> i32 {
    let f = square                     // a named function used as a value
    let steps = [square, adder(1), func(x) { x * 3 }]
    let b = Button { on_click: f }
    b.on_click(2) + apply(func(x) { x + 1 }, 4) + steps[1](5) + make_counter()()
}
```

- `func(A, B) -> R` is the type of a function value (`func()` returns `()`).
  A named function (used anywhere but as the callee of a call) or a closure
  literal `func(a: A) -> R { .. }` is a value of that type: store it in a
  local, a field or an array, pass and return it, call it with `f(x)`. A
  field of function type is called like a method: `obj.on_click(x)`.
- **Captures are by value, at creation.** A closure copies every local it
  mentions when it is created; changing the variable afterwards does not
  change the closure's copy. Assigning to a captured variable (or a field of
  a captured value struct) inside the closure is a compile error. Mutable
  state goes in a class instance: capturing it copies the reference, so the
  closure and its creator share the object (the counter above).
- Parameter types and the result may be left out when the closure is
  checked against a known function type (an argument, a typed `let`, an
  array of functions); otherwise parameter types are required and a missing
  `-> R` means `()`, as for functions.
- Function values cannot be compared (`==` is a type error).
- Generic higher-order functions work: `func fold[T, U](xs: [T], init: U,
  f: func(U, T) -> U) -> U` is instantiated like any generic function, and a
  closure inside a generic function gets code per instance (it may capture
  a `T`). A generic function itself cannot be used as a value yet (`let f =
  id` names no instantiation); wrap it in a closure.
- Not allowed: a generic function, a function that `uses` effects, an
  `extern "codira-intrinsic"` or `@asm` function as a value; `perform`,
  `handle` or calling a `uses` function inside a closure; `spawn` of a
  function value; capturing a channel (it lives only as long as its
  `scope`). Enums, optionals, tuples and `str` are captured by value like
  anything else. `break`/`continue` inside a closure do not reach loops
  outside it; `return` returns from the closure.
- Secrets: a closure that captures a secret is secret (so is its result);
  function-value parameters are never `secret`; `@constant_time` functions
  neither create nor call function values. A function with contracts may
  not contain closures whose obligations would go unchecked (the verifier
  says so).
- Cost: creating a function value allocates one GC object (a cell word plus
  the captures); a call is three dependent loads and an indirect call.
- Hot reload: a function value never holds a raw code pointer. It points at a
  runtime-owned code cell that every reload repoints, so a value created
  before a reload runs the *new* code of the same closure (or named
  function) with the captures it was created with. If the reload removed
  that closure or changed its signature, calling the old value stops the
  program with a message instead.
- `codira run --jit` (Cranelift) does not compile function values; such a
  program runs on the LLVM backend.

### Algebraic effects: `effect`, `uses`, `perform`, `handle`, `resume`

An effect declares operations; a function that may perform them says so
with `uses`; a `handle` expression supplies the implementation.

```codira
effect Fail {
    func fail(code: i32) -> i32
}

effect Ask {
    func ask(hint: i32) -> i32
}

func check(x: i32) uses Fail -> i32 {
    if x < 0 { perform Fail.fail(x) } else { x }
}

func compute(x: i32) uses Ask, Fail -> i32 {
    check(perform Ask.ask(x)) * 2
}

public func run(x: i32) -> i32 {
    handle {
        handle compute(x) {
            Ask.ask(h) -> resume(h + 1)      // resumes: `perform` returns h + 1
        }
    } {
        Fail.fail(code) -> code * 1000       // aborts: this is the value of the handle
    }
}
```

Rules (checked by the compiler):

- A `perform E.op(..)`, or a call to a function with `uses E`, must be inside
  a `handle` with an `E` arm or in a function that itself declares `uses E`.
  An arm is not covered by its own `handle` (it runs where the `handle` is).
- An arm `E.op(a, b) -> body` binds one name (or `_`, or a tuple pattern) per
  parameter of `op`; their types come from the effect. A `handle` implements
  every operation of each effect it names.
- Handlers are one-shot: `resume(v)` returns `v` to the `perform` and must be
  the last thing the arm does (tail position, through blocks and `if`
  branches). An arm that finishes without resuming abandons the handled
  computation -- through any number of call frames -- and its value becomes
  the value of the whole `handle`. `resume()` for an operation without a
  result.
- Innermost handler wins.
- Version-1 restrictions: an arm cannot use local variables or parameters of
  the enclosing function (arms do not capture -- pass values through the
  operation's parameters), cannot `return` or `break` out of the enclosing
  function, and a function with `uses` can only be called from its own
  module and cannot be `@export`ed.
- A `uses` function can still be called from the host (`Runtime::invoke`);
  performing an effect there, with no handler, executes a trap instead of
  continuing.

Cost: evidence passing. Each `uses` effect is one hidden pointer parameter;
`perform` is one load and one indirect call; after a `perform` or a call that
passed evidence, one load and branch per handler in scope. Code that uses no
effects pays nothing.

### Structured concurrency: `scope`, `spawn`, channels

```codira
func square(jobs: Channel[i32], results: Channel[i32]) {
    let x = <-jobs              // receive: waits while the channel is empty
    results <- x * x            // send: waits while the channel is full
}

public func sum_of_squares(n: i32) -> i32 {
    scope {
        let jobs = channel(64)      // bounded; the element type is inferred
        let results = channel(64)
        var i = 0
        while i < n { spawn square(jobs, results); i += 1 }
        i = 0
        while i < n { jobs <- i; i += 1 }
        var sum = 0
        i = 0
        while i < n { sum += <-results; i += 1 }
        sum
    }                               // every task has finished here
}
```

- `scope { .. }` owns tasks: every task `spawn`ed in it has finished before
  control leaves it, by its end, `return`, `break`, or an effect handler
  abandoning the computation. `scope` is a contextual keyword (only before
  `{`; `if scope { .. }` still tests a variable).
- `spawn f(args)` runs a call to a function of the same module on the
  runtime's worker pool. It must be inside a `scope` (of the same function;
  a handler arm counts as its own function); `f` takes only integers,
  floats, `bool` and `Channel[T]` and declares no effects. Arguments are
  evaluated by the spawner; the result is discarded -- results come back
  through channels.
- `let ch = channel(capacity)` creates a bounded blocking `Channel[T]` of
  integers, floats or `bool`, owned by the enclosing `scope` and freed when
  it ends. A channel is only *used*: `ch <- v`, `<-ch`, or passed to a
  function or `spawn`. It cannot be stored, returned, copied into another
  binding or made the value of a block -- that is what keeps it from
  outliving its scope.
- Tasks share nothing but channels, so they cannot race on memory.
- The pool keeps `available_parallelism()` workers running and starts a
  replacement when one blocks on a channel or a nested scope, so blocked
  tasks cannot starve the ones that would unblock them. Mismatched sends and
  receives still block forever, as with any blocking channel.
- Output printed by a task is not captured by `capture_output` (it is
  per-thread); a task's `print` goes to standard output.
- A secret (`secret T`) cannot be sent on a channel: what a channel delivers
  is public.

#### `select`: wait on several channel operations

```codira
func collect(jobs: Channel[i64], results: Channel[i64], quit: Channel[bool]) -> i64 {
    var total = 0
    var sent = 0
    var idle = 0
    let x = 7
    select {
        v = <-jobs -> total += v,       // receive, bind the value in this arm
        <-quit -> {},                   // receive, discard the value
        results <- x -> sent += 1,      // send
        default -> idle += 1,           // optional: makes the select non-blocking
    }
    let first = select {                // a select is an expression
        a = <-jobs -> a,
        b = <-results -> b * 2,
    }
    total + first
}
```

- `select` is a contextual keyword (only before `{`; `if select { .. }`
  tests a variable). Arms are separated by commas (optional after a block)
  and use `->` like `match`. An arm's operation is `name = <-ch`, `<-ch`,
  `ch <- value` or `default`; `name` is a new binding visible only in that
  arm's body.
- Semantics (Go's): every arm's channel and sent value is evaluated first,
  top to bottom. Then the select waits until at least one operation can
  proceed (a receive: the channel has a value; a send: it has room),
  performs exactly one and runs that arm. When several are ready, one is
  chosen uniformly at random, so a ready arm is never starved by the
  others. `default` runs when none is ready, and then the select never
  waits.
- The arms' bodies have one type, which is the select's value.
- Rejected at compile time: a select with no arms (it would wait forever),
  more than one `default`, an arm that is not a channel operation, a binding
  on anything but a receive, arms of different types.
- A waiting select sleeps (no polling) and counts as blocked for the worker
  pool, like a blocked send or receive. The verifier treats the chosen arm
  as unknown and a received value as unconstrained. Cranelift (`--jit`)
  declines `select`, as it declines channels.

#### Session types: protocols checked at compile time

```codira
session Checkout {                  // what the customer's endpoint does
    send i64; recv i64              // send an item, receive its price
    choose { send bool; recv i64 }  // branch 0: pay (or not), get a receipt
    or { send i32 }                 // branch 1: give a reason and stop
}

func shop(ep: dual Checkout) {      // the other side: send<->recv, choose<->offer
    let item = <-ep
    ep <- item * 100
    offer ep {                      // runs the branch the customer chose
        let pay = <-ep
        if pay { ep <- 1 } else { ep <- 0 }
    } or {
        let reason = <-ep
    }
}

public func buy(item: i64, budget: i64) -> i64 {
    scope {
        let (me, them) = Checkout.open()   // me: Checkout, them: dual Checkout
        spawn shop(them)                   // the task owns `them` now
        me <- item
        let price = <-me
        if price <= budget {
            me.choose(0)
            me <- true
            let receipt = <-me
            price * 10 + receipt
        } else {
            me.choose(1)
            me <- 7
            -1
        }
    }
}
```

- `session Name { .. }` (contextual keyword, only before a name and `{`)
  lists the steps one endpoint takes: `send T`, `recv T`,
  `choose { .. } or { .. } ..` (this side picks a branch) and
  `offer { .. } or { .. } ..` (the other side picks). `;` between steps is
  optional. Steps after a `choose`/`offer` continue every branch. Payloads
  are integers, floats or `bool`, as on channels. There is no repetition
  (no recursive protocols).
- Types: `Name` is an endpoint following the protocol as written,
  `dual Name` one following its dual (every `send` a `recv`, every `choose`
  an `offer`, and back).
- `let (a, b) = Name.open()` -- only inside a `scope`, which owns the pair
  and frees it when it ends -- makes a connected pair: `a: Name`,
  `b: dual Name`. Both names must be bound.
- On an endpoint: `ep <- v` sends, `<-ep` receives (its type is the
  payload type at that step), `ep.choose(k)` takes branch `k` (an integer
  literal), `offer ep { .. } or { .. }` runs the block for the branch the
  other side chose (one block per branch, in order; it is an expression
  whose blocks share one type).
- An endpoint is passed to a function or `spawn` whose parameter has its
  exact session type (`Name` or `dual Name`), and only before it has been
  used; the callee then owns it. Two tasks talk by spawning a function
  with one endpoint and keeping the other.
- The compiler follows every endpoint along every path and rejects:
  an operation other than the one the protocol expects next (or a payload
  of the wrong type); any use after the protocol ended, or after the
  endpoint was passed on; branches of an `if`/`select`/`offer` (and the two
  outcomes of `&&`/`||`, and an `if` without `else`) that leave an endpoint
  at different steps; a loop that moves an endpoint (body end, `break`,
  `continue`); a function or the block that opened a pair ending -- by any
  path, `return` included -- before every endpoint reached the end of its
  protocol (or was passed on); a `perform`, or a call to a `uses` function,
  while an endpoint is part-way through its protocol (a handler could
  abandon the function there); a handler arm that moves an endpoint; and
  copying, storing or returning an endpoint (endpoints are linear: used
  only with `<-`, `.choose`, `offer`, or as an argument). A `select` waits
  on channels only, not endpoints.
- What that guarantees: **protocol fidelity** for each pair -- every send
  meets a receive of the same type, every `choose` an `offer` with that
  branch, and neither side stops early. **Deadlock freedom only for a
  single session between two tasks that block on nothing else.** Tasks
  that interleave several sessions or channels can still deadlock each
  other, and one task holding both endpoints of a pair can deadlock itself;
  neither is checked.
- Runtime: a pair is two one-slot channels, one each way; a step's payload
  size is passed with it. A secret cannot be sent on an endpoint (same rule
  as channels). Cranelift and quantum kernels decline sessions; the
  verifier treats `offer`'s branch as unknown.

### Contracts and verification

Every contract is proven at compile time by Z3; a clause that cannot be
proven is a compile error with a counterexample, never a silent pass.

```codira
type Positive = i64 { v | v > 0 }        // refinement type

func div(a: i64, b: i64) -> i64
    requires b != 0                          // proven at every call site
    requires a != -9223372036854775807 - 1 || b != -1
{
    a / b
}

func half(x: Positive) -> i64
    ensures result >= 0 && result < x        // `result` = the return value
{
    x / 2
}

func sum_to(n: i64) -> i64
    requires n >= 0 && n <= 1000
    ensures result >= 0
{
    ghost let limit = n                      // verification-only binding
    var i = 0
    var s = 0
    while i < n
        invariant i >= 0 && i <= limit
        invariant s >= 0 && s <= i * 1000
        decreases n - i                      // proves the loop terminates
    {
        i += 1
        s += i
    }
    s
}
```

- Integers are checked with the semantics the compiled code has: fixed
  width, wrapping. `ensures result > x` for `x + 1` fails at `x = i64::MAX`.
- Inside `ensures`, a parameter means its value on entry.
- Loops are checked from an arbitrary iteration: every local a loop assigns
  is unknown at its head. A `while`/`loop` recovers facts through its
  `invariant`s, which must hold at the end of the body and at every
  `continue`; its `decreases` measure must drop on both paths too. A `for`
  loop takes no clauses: inside it the variable is known to lie in its range
  (an array element is unknown), and after it every local it assigns is
  unknown.
- A function with any contract must also prove its divisions safe (no zero
  divisor, no signed `MIN / -1`).
- `ghost let` bindings and all clauses are erased before code generation;
  executable code may not read a ghost binding, and clauses may not call,
  assign or loop.
- Contract clauses support integer/bool parameters, locals, literals,
  arithmetic, comparisons, `&&`/`||`/`!`, `if`/`else` and `as`. Anything that
  mentions a struct field, array, string, float or a call is reported as
  uncheckable (an error).
- `@runtime_checked` on a function makes its `requires` clauses run on entry
  (a violation traps), so a call site the solver cannot *decide* is accepted.
  A call site it *refutes* is still an error.

### Synthesis: `??` and `choose`

```codira
func inc(x: i64) -> i64
    requires x < 1000
    ensures result == x + 1
{
    ??                          // codira synth writes `(x + 1)` here
}

func complement(n: i64) -> i64
    requires n >= 0 && n <= 10
{
    choose y where y + n == 10  // codira synth writes `(-n + 10)`
}
```

```bash
codira synth                                  # search, prove, rewrite the source
codira synth --function inc --dry-run         # print instead of writing
codira synth --oracle "my-llm-cli --model x"  # ask an external program
```

A hole is a compile error until it is filled. `codira synth` searches small
expression shapes (constants, linear combinations of parameters, `if a < b`
with linear arms, comparisons for `bool` holes) by counterexample-guided
synthesis, and writes a candidate only after Z3 proves every contract
obligation of the function with it. With `--oracle` the command receives the
function and every rejected answer on stdin and prints one expression; each
answer is spliced in and re-verified, and only a proven one is kept
(`--attempts`, default 3). The compiler opens no network connection itself.
Holes are integer or `bool`; templates use only the function's parameters.

### Secrets and constant-time code

```codira
oblivious func ct_max(a: u64, b: u64) -> u64 {   // every value is secret
    ct_select(a > b, a, b)                         // a select, never a branch
}

@constant_time
func ct_equal(a: secret u64, b: secret u64) -> secret bool {
    let diff: secret u64 = a ^ b
    diff == 0
}

public func equal(a: u64, b: u64) -> bool {
    declassify(ct_equal(a, b))                     // the only way out
}
```

- `secret T` marks a parameter, result or `let`. A secret may not reach a
  non-secret result, parameter, `let`, field/array store or `print` without
  `declassify(..)`; values assigned under a secret `if`/`while`/`match` are
  secret too, and so are the names a pattern binds from a secret.
- In `@constant_time` / `oblivious` functions the compiler rejects branching on
  a secret (`if`, `while`, `match`/`if let`/`while let`, the left side of
  `&&`/`||`), indexing by a secret,
  `/` or `%` on a secret, and passing a secret to a function that is not
  constant-time. `ct_select(c, a, b)` chooses without a branch.
- What is guaranteed: those source rules, and that `ct_select` becomes one
  LLVM `select` marked `!unpredictable` with no conditional branch (checked
  in tests at `-O0`, `-O2`, `-O3`). The IR is not the machine code: the LLVM
  backend may still lower a `select` to a branch, and does -- at `-O0` on
  x86-64 a `ct_select` on `u8`/`i8` becomes a conditional jump on the secret
  condition, and a `bool` one a select of two stack addresses followed by a
  load.
- `codira build --check-constant-time` (alias `--check-ct`, off by default)
  checks the machine code. After linking, it disassembles each `oblivious` /
  `@constant_time` function of the library (found by its symbol) and fails
  the build, naming the function and each offending instruction (address and
  disassembly), on: (a) any conditional branch -- even on a public value, such
  as a loop counter at `-O0`; the report says whether the condition was traced
  to a secret; any indirect branch; (b) a variable-time instruction: integer
  and floating-point division and square root, x87 remainder/transcendental
  instructions (`AArch64`: `udiv`, `sdiv`, `fdiv`, `fsqrt`), and a `rep`
  string instruction whose count is secret; (c) a call to anything but
  another function the check covers; (d) x86-64 only: a load or store whose
  address register a taint analysis traces to a secret parameter (through
  registers, flags and stack slots). Rule (d) starts from the registers the
  calling convention passes `secret` parameters in, so it covers functions
  whose parameters and result are integers, `bool`s or floats; a secret
  reached through a pointer (an array, a struct) is not seen there and is
  covered only by the source rule, and the build prints a note for each
  function this applies to. A function the optimizer inlined everywhere and
  removed has no code of its own and is reported as not checked.
- What the machine-code check is not: a proof of constant-time execution.
  It has been run on x86-64 Linux (ELF) builds, and on `aarch64` Linux
  libraries cross-compiled there (checked, never run on `AArch64` hardware),
  where it checks rules (a)-(c) only; Mach-O and PE libraries are read but
  call targets through their stubs are not resolved, so calls there are
  reported.
  Multiplies, shifts, `cmov` and loads at public addresses are assumed
  constant-time; other micro-architectural effects (cache banks, subnormal
  floats, frequency) are not modelled; speculative execution is out of
  scope.

### Units of measure

```codira
type Speed = f64 unit(m / s);

func speed(d: f64 unit(m), t: f64 unit(s)) -> Speed { d / t }

func area(w: f64 unit(m), h: f64 unit(m)) -> f64 unit(m * m) { w * h }

func wrong(d: f64 unit(m), t: f64 unit(s)) -> f64 unit(m) {
    d + t      // error: dimension mismatch: added or subtracted `s` where `m` is required
}
```

- `T unit(<dimension>)` gives a number a dimension: a product of base units
  with integer exponents, written with `*`, `/`, parentheses and `1`
  (`kg * m / (s * s)`). Any identifier is a base unit; units are not declared.
  `unit` is contextual (only directly after a type and before `(`).
- `+`, `-`, comparisons, assignment, `if` arms, arguments and results need
  equal dimensions; `*` adds exponents and `/` subtracts them. An unannotated
  numeric parameter, `let` or result is dimensionless.
- A literal takes the dimension it meets (`x + 1.0` is fine for any `x`) and
  is dimensionless under `*` and `/` (`2.0 * x` keeps `x`'s dimension).
- Values the checker cannot follow (fields, array elements, calls through
  anything but a plain function) are unconstrained, so a mismatch through them
  is not caught.
- The qualifier lowers to its base type: units cost nothing at run time and
  code generation never sees them. There are no unit conversions (`km` and
  `m` are unrelated base units).

### Allocation-, panic- and block-free code: `@noalloc`, `@nopanic`, `@noblock`, `@realtime`

```codira
class Particle {
    x: i64
}

@realtime                                  // = @noalloc @nopanic @noblock + bounded
func mean(total: i64, count: i64) -> i64
    requires count > 0                     // proves the division safe
{
    total / count
}

@noalloc
func tick(n: i64) -> i64 {
    step(n)                                // error, see below
}

func step(n: i64) -> i64 {
    let p = Particle { x: n }
    p.x
}
```

```
error: `tick` is `@noalloc` but calls `step` (line 14), which allocates: class `Particle` constructed at line 18
```

The compiler computes, for every function, whether a call to it can
allocate, block, panic or run unboundedly, through everything it calls
(recursion included), and remembers one chain of calls that explains each.
An attribute forbids effects: `@noalloc` allocation, `@nopanic` panics,
`@noblock` blocking, `@realtime` all three and unbounded running time. Every
place in an annotated function that has a forbidden effect, directly or
through a call, is an error that prints the chain.

What counts, exactly (today's code generator and runtime):

- **Allocates** (and so may also block on the heap's lock and abort when the
  heap limit or memory runs out): constructing a `class`, an array literal,
  `print`/`println`, `channel(n)`, `scope`, `spawn`, a channel send, `select`
  (even with `default`), opening a session (`S.open()`), and a
  call to a function of *another module* that takes or returns a value
  struct (the cross-module calling convention boxes it). String literals,
  value structs, tuples and effect handler records allocate nothing.
- **Blocks**: channel send and receive (also on a session endpoint, and
  `ep.choose(..)` and `offer`), `select`, the end of a `scope`, and the
  allocations above.
- **Panics**: `a[i]` and `a[i] = v` (bounds check; `a.unchecked_get(i)` is
  the unchecked escape hatch), integer `/` and `%` (and `/=`, `%=`), calling a
  `@runtime_checked` function that has `requires`, and every allocation.
  Integer division is not checked by the generated code at all -- a zero
  divisor or `MIN / -1` is undefined behaviour -- so a division only counts
  as safe when its divisor is a positive literal or the function has a
  contract and Z3 proves the divisor non-zero (and, signed, not `MIN / -1`).
- **May not terminate** (`@realtime` only): a `while` or `loop` without a
  `decreases` measure, or whose measure the verifier does not prove, and
  recursion, direct or mutual. `for` loops are bounded by construction.
- **Everything at once**: a call to an `extern` function, a `perform` (its
  handler is chosen at run time), and any call the analysis cannot resolve.
  An `extern` declaration may carry the attributes as a *trusted
  assertion* -- nothing checks it -- e.g. `@noalloc @nopanic extern func
  c_mix(a: i64) -> i64;`, also inside `extern "C" { .. }` blocks.
  `extern "codira-intrinsic"` operations have no effects.

Limits: stack overflow is not ruled out (there is no stack bound). `@realtime`
proves termination, not a time bound: `@deadline(..)` is rejected, because a
worst-case execution time needs a cycle model of the target CPU that the
compiler does not have. `codira build --fuel` lets any call stop when its fuel
runs out, so such a build rejects `@nopanic` and `@realtime` functions.

### Semantic envelopes: `@envelope` (experimental)

`@envelope(..)` declares what a function may do. Each clause rules out one
effect, and the compiler checks it through everything the function calls,
like `@noalloc`:

```codira
@envelope(allocation: none, blocking: false, panic: false, termination: bounded,
          foreign: none, concurrency: none, effects: none, secrecy: none)
func scale(x: i64, k: i64) -> i64 { x * k + 1 }
```

| key | rules out with | allows with | what it rules out |
|-----|----------------|-------------|-------------------|
| `allocation` | `none` | `any` | allocation (as `@noalloc`) |
| `blocking` | `false` | `true` | blocking (as `@noblock`) |
| `panic` | `false` | `true` | panics and unchecked UB (as `@nopanic`) |
| `termination` | `bounded` | `any` | unproven loops, recursion |
| `foreign` | `none` | `any` | `extern`, `@asm`, `perform`, calls through function values |
| `concurrency` | `none` | `any` | `scope`, `spawn` |
| `effects` | `none` | `any` | `perform` |
| `secrecy` | `none` | `any` | `secret` parameters, results and `let`s, `oblivious`, `declassify` |

The allowing value checks nothing; it documents intent. On an `extern`
declaration the envelope is a trusted assertion. Keys from the design that
cannot be checked yet (`reads`, `writes`, `persistence`, `capability`,
`memory`, `fuel`, `latency`, `energy`, `network`, `targets`) are errors, not
silently ignored, and so are unknown keys, repeated keys, values without a
key and a second `@envelope` on the same function.

`codira envelope` prints every function's *inferred* envelope (the strongest
value of each key the analysis proves, with the call chain behind each
permissive value) beside its declaration. `--json` writes the
`codira-envelope/0` schema, and `--check` exits with an error when a
declaration is not met. The guarantee is about the source, under the same
limits as `@noalloc` and the other attributes, not about machine code.
Example:
`examples/envelope`.

**Package envelope.** An `[envelope]` table in `codira.toml` takes the same
keys and values (strings, or `true`/`false` for `blocking` and `panic`):

```toml
[envelope]
allocation = "none"
blocking = false
foreign = "none"
```

Every `public` function of the package must fit it: `codira check` reports
each one that does not, with the call chain to the cause, and `codira build`
refuses to build (it checks before compiling, so no library is written).
Non-public functions are covered exactly as far as a public function reaches
them, because a function's inferred envelope includes everything it calls; a
private function no public one calls is not constrained. Unknown, unchecked
and repeated keys are errors, as for the attribute. `codira envelope` shows
the package envelope and whether it is met (`package_envelope` in the JSON),
and `--check` fails when it is not. Only `check`, `build` and `envelope`
enforce it today (`codira run` and `codira test` do not), and a rebuild under
`codira build --watch` does not re-check it.

**In certificates and `api-diff`.** `codira build --certify` records every
function's inferred envelope in its proof certificate, and `codira certify`
re-derives it from the source (see "Proof certificates"). `codira api-diff`
treats a public function whose inferred envelope *widened* (a key moved to
its allowing value, for instance because a private callee started
allocating) as a major change, and one that narrowed as a minor change.

### Modules & imports

```codira
import math
import collections.list
import geometry.{ Point, Vector as Vec2 }
import root.tools.clamp        // `root` = package root
import super.helper            // parent module
import std.crypto.hash.{sha256_str, SHA256_LEN}   // the standard library
```

Each file is a module (`geometry.code` or `geometry/mod.code` is
`geometry`, `a/b.code` is `a.b`). Paths with more than one segment are
written in `import` and in type position (`let p: root.geometry.Point`); in
an expression, import the name and call it (`clamp(x, 0, 1)`), since
`root.tools.clamp(x, 0, 1)` is read as field accesses and does not resolve.

#### Importing the standard library

A program imports standard-library modules (the `std/` directory of the
Codira distribution) by their path under `std`:

```codira
import std.crypto.hash.{sha256_str, SHA256_LEN}
import std.fs.fs.*
import std.memory.region.{Region, region_new, region_alloc, region_free}
```

- **Only what is imported is loaded**: the modules a program's imports
  name, and the modules those import in turn (`import root.<..>` inside
  `std` means the standard library's root). Nothing else of `std/` is read,
  and there is **no implicit prelude**: `std/prelude.code` is not in scope
  in programs (it would pull in all of `std/`, most of which does not check
  yet). Import what you use, explicitly.
- **Standard-library modules do not use the prelude either.** Each one
  imports what it uses (`import root.sys.terminate.{panic};`), exactly as
  a program does, so that it means the same whether it is imported by a
  program or checked as part of `std/`.
- **Only modules that check, build and load can be used.** Importing a
  module with errors, or one that imports such a module, fails the build
  with the module's own diagnostics, shown against its file
  (`--> std/collections/list.code:10:10`), and a line naming it:
  ``error: the standard-library module `std.collections.list`
  (std/collections/list.code) has 55 errors; ...``. Which modules can be
  imported, and for each one that cannot, the first error and what it is
  waiting for, is recorded in
  `crates/codira_compiler/tests/snapshots/stdlib_import_ratchet__stdlib_import_status.snap`
  (`stdlib_check_ratchet` checks `std/` as one package with its prelude,
  which is not what a program sees).
- **Importable today** (17 of 89 modules), each run from a program in
  `crates/codira/tests/std_imports.rs` against independently computed
  values, except `std.memory.page` and `std.builtin.type_aliases` (check,
  build and load; not run):
  - `std.crypto.hash` (SHA-256, SHA-512, BLAKE3), `std.fs.fs`,
    `std.memory.region`, `std.memory.page`.
  - `std.bit.bit`, `std.bit.mask`: bit counts, reversal, byte swaps,
    rotations, masks and fields on `u64`/`u32`. Exact. `bit_ceil` needs a
    result that fits (`value` at most 2^63).
  - `std.math.constants` (`PI()`, `E()`, `INF()`, `NAN()`, `F64_MAX()`,
    integer limits...: niladic functions, since module-level
    `let`s are not supported) and `std.math.fast` (f32 approximations: `fast_sin`/`fast_cos`
    within 7e-5 absolute, `fast_exp` 0.01 % relative, `fast_log` 5.4e-4
    absolute, `fast_sqrt`/`fast_rsqrt` 0.175 % relative; `fast_pow` and
    `fast_tan` have no stated bound).
  - `std.hashlib.hash`: FNV-1a 64 over `[u8]`, integers, floats and bools,
    hash combiners, and `hash[T: Hashable]`. **Not cryptographic**: for
    hash tables and fingerprints of trusted data only.
  - `std.base64.base64`: base64, base32 and base16 encode and decode over
    `[u8]` (encoded text is its ASCII bytes), standard alphabets with `=`
    padding; decoding skips bytes outside the alphabet and returns an empty
    array for invalid input.
  - `std.random.rng`: xoshiro256** with the state in a `Rng` the caller
    holds, seeded by SplitMix64 (`rng_seeded(seed)`, `rng_next_u64`,
    `rng_next_f64`, `rng_range_i64`, `rng_next_bool`). **Not
    cryptographic**: predictable from its output, and seeded only by the
    caller.
  - `std.builtin.type_aliases` (`Never`).
  - `std.sys.terminate`: `panic` (writes `panic: <message>` to standard
    error and aborts), `exit`, `abort`, `assert`, `unreachable`, `todo`.
  - What calls `panic`: `std.math.math` (libm-backed `sin`...`atan2`,
    rounding, `gcd`, `factorial`, classification; within 2 ulp of Rust's
    libm in the tests), `std.time.time` (durations, monotonic instants,
    wall clock, sleep), `std.complex.complex`, `std.collections.optional`
    (a generic `Optional[T]` struct; `unwrap` of an empty one panics).
- **Waiting for the language**: what is written against `String` (most of
  `std.format`, `std.os`, `std.pathlib`, `std.testing`, `std.sys.info`, and
  the `*_string` halves split off from the modules above:
  `std.base64.base64_string`, `std.hashlib.string_hash`,
  `std.complex.complex_string`); everything built on `Pointer[T]` (every
  collection but `optional`, and through `List`, `std.math.sequence` and
  `std.math.polynomial`); `std.builtin.traits` (generic traits);
  `std.sys.assert_compare` (`assert_eq` and friends: operators on a type
  parameter); `std.random.random` (a global generator: there is no
  module-level state, and a module-level `var` is rejected);
  `std.hashlib.hasher` and `std.atomic.atomic` (they update a struct
  passed as a `mut` parameter, and in-place parameters are rejected; the
  orderings of `std.atomic.atomic` are module-level `let`s).
- **Linking**: each imported standard-library module is compiled with the
  program into a library of its own in the program's `target/` directory
  (`target/hash$crypto$std.codiralib`), exactly like the program's own
  modules, and the runtime loads it with the program (`codira start`,
  `Runtime::builder`, and `codira build --exe`, which bundles every library
  in `target/`). There is no precompiled standard library.
- Names: a program module of its own called `std` takes precedence over
  the standard library (then `std` means that module). The program's
  `fs` module and `std.fs` are different modules with different symbols.
  Private and `internal` items of the standard library cannot be imported.
- **Where the standard library is found**: `CODIRA_STD_PATH` if set (a
  directory holding `prelude.code`); otherwise `std/` next to the `codira`
  executable or one directory above it (`<prefix>/bin/codira` and
  `<prefix>/std`); otherwise the `std/` of the source tree the compiler was
  built from. A program that imports `std` when none is found fails with
  a message saying so; a program that imports nothing from `std` never
  looks.
- Not yet: the language server does not load the standard library (an
  `import std.<..>` is unresolved there); a standard-library module cannot
  be named in an expression without importing it, like any module.

---

## 4. Building and running

```bash
codira run                                       # build, then run `main`
codira run other_entry                           # build, then run `other_entry`
codira build                                     # compile -> target/mod.codiralib
codira start target/mod.codiralib main          # run an already-built library
```

`codira run` takes the same `--manifest-path`, `-O 0..3` and `--color`
options as `build`. If the build reports an error, nothing is run. After the
entry point returns, `run`/`start` print its value if it is a primitive
(`bool`, any integer or float type); a `()` entry point prints nothing.

### How a library is linked

A `.codiralib` has a handful of sections, imports nothing, exports a known
list of functions and needs only absolute-address base relocations (calls
between Codira functions go through the dispatch table), so on Windows the
compiler writes the image itself instead of calling LLD: the `codira_image`
crate turns the object LLVM emitted into the DLL in microseconds. It is the
default for `x86_64-pc-windows-msvc` libraries on a Windows host.

It never guesses. An object with anything it does not model -- an unknown
relocation, a section it does not know (TLS, C runtime initializers, linker
directives), an import, debug information, a package with `[native]` C, C++ or
assembly sources -- is *declined*, and LLD links it exactly as before. Other
targets always use LLD.

- `CODIRA_LINK_MODE=direct` asks for the image writer (the default on Windows
  hosts); `in-process` or `subprocess` forces LLD for everything, and
  `CODIRA_DIRECT_LINK=0` does the same without naming how LLD runs.
- x86-64 Linux shared objects can be written the same way
  (`CODIRA_DIRECT_ELF=1`, with `CODIRA_LINK_MODE=direct`). It is opt-in
  because it is newer: the library is checked by `llvm-readobj` and loaded
  by a real dynamic loader in the tests, but it has not run under the Codira
  runtime on Linux. It declines a program that calls one of its own public
  functions directly (that would need a PLT). Its cross-check against LLD
  compares what a program can observe -- the exported symbols and the
  symbolic relocations -- not the bytes.
- In builds of the compiler with debug assertions (what `cargo test` builds)
  every library is also linked by LLD and the two images must be identical but
  for the header time stamp; a difference stops the build with a report.
  `CODIRA_LINK_VERIFY=0` turns that off, `=1` turns it on in a release build.
  Every test of the workspace that builds a library is therefore a
  differential test of the image writer.
- `CODIRA_LINK_LOG=<file>` appends one line per link (`written`, `verified`,
  `declined <reason>`); `CODIRA_DUMP_OBJECTS=<dir>` keeps the objects.
- The compiler writes a finished library to a temporary file next to its
  destination (`.<name>.<pid>-<n>.codira-tmp`) and renames it over the old one,
  so a running program never loads a half-written file. The runtime ignores
  `.codira-tmp` files and treats a file renamed onto a loaded library as a
  change to it.

### Tests, counterexamples and semantic search

```bash
codira test                       # build src/ + tests/, run every @test
codira test parse                 # only tests whose name contains "parse"
codira check                      # also writes tests/generated/ (see below)
codira query "result < 0"         # functions that can return a negative
codira query "x > 0 && result == 0"
```

A test is a `public` function marked `@test` with no parameters, returning
`bool` (passes when `true`) or `()` (passes unless it traps or aborts). Tests
can live in `src/` or in any `.code` file under `tests/`; each runs in its own
process, so a trap or abort fails only that test.

When `codira check` refutes a contract it writes
`tests/generated/<fn>_counterexample.code`: a `@test` that calls the function
with the counterexample Z3 found (for a postcondition it returns the clause
evaluated on the real result, so it fails exactly while the bug exists). The
file is removed again once the contract is proven. `codira test` runs such
tests even while the contract is unproven -- that is what they are for.

`codira query` answers from the function *bodies* (callees summarised by
their contracts), with a witness input for each match. Values the verifier
does not model (struct fields, arrays, loop variables without an invariant)
are unconstrained, so a match there can be a value the program never
produces; a function is never missed because of that.

Printing is built in -- no import, no `extern` declaration:

```codira
public func main() {
    let x = 5
    println("x = ", x)           // x = 5
    print("no newline ", 1.5)    // arguments are written back to back
    println()                    // just a newline
}
```

`print`/`println` accept any number of integers (any width), floats, `bool`s
and `str`s; anything else is a compile error. A program that defines its own
function named `print` calls its own. See `examples/hello`.

Useful `build` options: `-O 0..3` (default `2`), `--emit-ir`, `--watch`,
`--target <triple>` (cross-compile), `--manifest-path <path>`, and:

- `--mode dev|release` (default `release`). `dev` skips Eidos, the
  mid-level optimizer (inlining, unrolling, equality saturation and
  compile-time evaluation of whole functions), for faster edit-build
  cycles. `-O` still controls LLVM's optimizer separately.
- `--target-cpu <name>`: an LLVM CPU name for the target (`znver4`,
  `skylake`, `apple-m1`, ...) or `native` for the machine running the
  compiler. An unknown name is an error that lists the CPUs the target does
  know. `native` is resolved to the concrete host CPU and its features
  before the build starts, and is refused when cross-compiling.

Eidos's search is bounded per region by an optional `[eidos]` table in
`codira.toml`. Every bound counts work, never time, so the same sources
always build to the same program:

```toml
[eidos]
max_egraph_nodes_per_region = 10000   # hard cap on e-graph nodes (1..=1000000)
max_egraph_iterations = 8             # rewrite rounds (<= 1000)
exact_extraction_budget = 10000       # exact-extraction search steps; 0 = greedy only
```

Unknown keys are errors. `egraph_timeout_ms` is refused because it would
make the output depend on machine speed. `ilp_extractor_fallback` is
refused because there is no ILP solver; use `exact_extraction_budget`
instead.

Hot reload: Codira is designed so a running application can swap function
bodies without a manual stop/restart cycle.

### Fast development runs: `codira run --jit`

`codira run --jit` compiles the program with the Cranelift backend and runs it
in the compiler's own process: no LLVM optimization pipeline, no linker. It
covers the scalar core of the language (integers up to 64 bits, floats,
`bool`; `let`/`var` and assignment; operators and casts; `if`, `loop`,
`while`, `break`, `return`; calls; `print`/`println` of scalars and string
literals). A program that uses anything else runs on the LLVM backend instead,
with a note naming what Cranelift does not compile yet. Measured: a
three-function program compiles in about 2.2 ms (minimum of 25, in a debug
build of the compiler). `CODIRA_JIT_TIMINGS=1` prints the compile time.

### Standalone executables: `codira build --exe`

```bash
codira build --exe -o app.exe      # one file; runs with nothing else installed
./app.exe                          # `main` returning an integer sets the exit code
```

The executable is the Codira runtime (`codira-launcher`, shipped next to
`codira`) with the package's compiled libraries appended. On its first run it
unpacks them once into a temporary directory named after their hash; later
runs load them directly. `main` may return nothing (exit code 0), an integer
(the exit code) or `bool` (true is success). Host target only for now.

### Proof certificates: `codira build --certify`, `codira certify`

```bash
codira build --certify                     # also writes target/mod.codiralib.cert.json
codira certify target/mod.codiralib        # re-checks it (envelopes aside, without the compiler)
codira certify target/                     # every library in a directory
codira build --exe --certify -o app.exe    # the certificates travel inside the executable
codira certify app.exe
codira certify target/mod.codiralib --source-dir src   # also match the source text
codira certify lib.codiralib --require-envelopes        # fail unless envelopes are re-derived
```

The certificate (a JSON sidecar, `<library>.cert.json`) lists every function
of the library with: its symbol; the byte range of its machine code in the
library and a SHA-256 of those bytes; the file, line, byte range and SHA-256
of its source; and every proof obligation the contract verifier generated
for it, each as the self-contained SMT-LIB2 queries Z3 decided (premise
asserted, then the negated goal; `unsat` = proven) or the reason it is not
proven. The whole library file is hashed too.

Apart from re-deriving envelopes (below), `codira certify` uses Z3 and the
certificate reader only -- no parser, type checker, verifier or code
generator. It fails (exit 1) when the library is not
the hashed file, when a function's code bytes or symbol position differ (the
failing function is named), when a query contains anything but declarations
and assertions, or when Z3 under the checker's own `--rlimit` does not answer
`unsat` for a query recorded as proven. It reports, without failing,
obligations the certificate records as unproven (a `@runtime_checked`
`requires` the solver could not decide; `--deny-unproven` makes them fail) and
proofs that hold only because their path condition is unsatisfiable.

**Envelopes.** The format is `codira-certificate/2`: each function also
records its inferred semantic envelope (key to value, e.g. `"allocation":
"none"`; see "Semantic envelopes"), computed from the source at build time.
`codira-certificate/1` files are still read and checked; they carry no
envelopes, and the check says so. Re-deriving an envelope needs the
compiler's front end, so this one step is the exception to "no compiler":
when `codira certify` finds the package source (`--source-dir`,
`--manifest-path`, or the `codira.toml` in the artifact's directory or a
parent), it recomputes every function's envelope and fails if any recorded
one differs, naming the function and key. Without a source it reports the
envelopes as not re-derived and passes; `--require-envelopes` makes that a
failure. What this certifies is the source-level effect analysis result
recorded at build time, and that today's analysis of today's source agrees
with it -- not anything about the machine code, and nothing enforced at
run time.

What a passing check establishes, and what it does not:

- It re-proves the recorded obligations and binds them, by hash, to the
  library's bytes. It does **not** show the obligations are the right ones:
  the verification-condition generator that turned source and contracts into
  queries is trusted, as is Z3. Nothing relates the machine code to the
  source beyond sitting in one certificate: the queries are about the source
  program, so a miscompilation is not detected.
- The certificate is not signed. Whoever can rewrite both the library and its
  certificate can make them agree; keep the certificate somewhere the library
  cannot change it (or sign it by other means) if that matters.
- Translation validation does not run during builds, so the certificate
  contains no translation-validation results (it says so). The Codira runtime
  that loads the library is not covered.
- Code ranges: ELF gives each function's exact size; on PE and Mach-O a
  function's range runs to the next symbol, so it includes alignment padding.
  A certified build keeps every function's symbol (internal instead of
  private linkage, and `/DEBUG:SYMTAB` on Windows) -- the generated code is
  the same. A function the optimizer inlined away has no code range (noted),
  and neither has a generic function: its instances are compiled into the
  libraries that use them and are not given ranges.

Cost: certifying verifies each function with a contract a second time and
writes its queries; it is opt-in for that reason.

### API compatibility: `codira api-diff`

```bash
codira api-diff v1.2.0 .              # a git revision of this package vs the working tree
codira api-diff ../lib-old ../lib-new # two package directories
codira api-diff HEAD~1 . --check      # exit 1 if `version` is not bumped far enough
```

```text
v2.2 (2.2.0) -> . (2.3.0)
      minor: weakens the precondition of `decode()`
             requires was: `x >= 0`
             requires now: `x >= -10`
             newly accepted: x = -10
      major: weakens the postcondition of `decode()`
             ensures was: `result >= 0 && result <= x`
             ensures now: `result >= 0`
             allowed now, ruled out before: x = 17592186044414, result = 17592186044416
      major: adds `Net` to `parse()`
2.3.0 weakens the precondition of `decode()`, weakens the postcondition of `decode()` and adds `Net` to `parse()`
required bump: major
error: 2.2.0 -> 2.3.0 is a minor bump; these changes need a major bump
```

It compares the `public` items (functions, methods as `Type.method`, structs,
type aliases, effects), matched by path:

- removed item, changed parameter/return types, receiver or generic
  parameters, an effect added to `uses`, a struct's fields or kind changed, an
  alias retargeted or its refinement changed: **major**;
- added item, an effect removed from `uses`: **minor**;
- `requires` + parameter refinements: **strengthened** (some input accepted
  before is refused now) is major, **weakened** is minor, an equivalent rewrite
  is a patch;
- `ensures` + return refinement: **weakened** (for an input old callers could
  pass, something ruled out before is allowed now) is major, **strengthened**
  is minor;
- bodies and private items: patch.

Contracts are compared by Z3 implication over the parameters by position
(`p0`, `p1`, ...; a renamed parameter is the same parameter) and `result`,
with the verifier's machine-integer semantics; a refuted implication prints
the input that shows it. A clause that cannot be encoded (floats, fields,
calls) or a check that exhausts the budget makes the change **undecided**:
reported as such, never guessed, and counted as major by `--check`.
A public generic function counts as API although it is never exported (its
callers compile its instances from source); its type parameters are compared
by position, so renaming `T` is no change and adding one is major.
`--check` follows Cargo's reading of versions: below 1.0 the leftmost
non-zero component is the breaking one (`0.4.0 -> 0.4.1` allows additions,
`0.4 -> 0.5` allows breaks), and every `0.0.z` is incompatible. A git
revision is read with `git cat-file` into a temporary directory; the working
tree and index are not touched. Limits: enums and traits are not compared;
`@export`, `@runtime_checked` and other attribute changes are not reported;
there is no `codira publish` -- `--check` is the step a CI job or publish
script runs.

### Targets

`--target` accepts: `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`,
`x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`,
`aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`,
`riscv64gc-unknown-linux-gnu` (lp64d), `x86_64-unknown-freebsd`,
`aarch64-unknown-freebsd`, `x86_64-apple-darwin`, `aarch64-apple-darwin`,
`x86_64-apple-ios`, `aarch64-apple-ios`, `aarch64-apple-ios-sim`. Every one is
tested to produce an object of its own format and architecture, and every
non-Apple target is tested to link into a library from a Windows host; Apple
targets need the platform SDK to link. Running the output is tested on the
host only.

`wasm32-unknown-unknown` and `wasm32-wasip1` build `target/<module>.wasm`, a
freestanding WebAssembly module that imports nothing and exports the
module's `@export("C")` functions (and its linear memory):

```codira
@export("C")
public func add(a: i32, b: i32) -> i32 { a + b }
```

```bash
codira build --target wasm32-unknown-unknown   # -> target/mod.wasm
```

There is no Codira runtime inside a WebAssembly host, so only functions over
integers, floats and booleans that need nothing from it can be built: no
classes or other garbage-collected values, no calls into other modules, no
`extern` functions. A package that needs any of those fails the build with
the reason. Modules are tested by running them in an independent engine
(wasmi) with no imports.

### Inline assembly: `@asm`

Declare a body-less `extern func` with one `@asm` variant per architecture.
Calls are replaced by the assembly itself, so a call costs exactly the
instructions written:

```codira
@asm(arch: "x86_64", "lea ($1,$2), $0", constraints: "=r,r,r", pure: true)
@asm(arch: "aarch64", "add $0, $1, $2", constraints: "=r,r,r", pure: true)
extern func add(a: i64, b: i64) -> i64;

@asm(arch: "x86_64", "rdtsc; shl $$32, %rdx; or %rdx, %rax", constraints: "={rax},~{rdx}")
@asm(arch: "aarch64", "mrs $0, cntvct_el0", constraints: "=r")
extern func cycle_counter() -> u64;
```

The template and constraints use LLVM's inline-assembly syntax: `$0`, `$1`...
name operands (outputs first), `$$` is a literal `$`, `=r` is an output, `r`
an input, `{reg}` pins a register, `~{reg}` declares a clobber, and a digit
ties an input to an output (`"=r,0"`). `dialect: "intel"` switches x86
templates to Intel syntax. `pure: true` lets the optimizer merge or drop calls
whose results are unused; without it every call is kept, in order. A variant
without `arch:` applies to every architecture. Operands are integers and
floats. The compiler rejects mismatched operand counts, unsupported operand
types and a missing variant for the target before LLVM sees the assembly.

### Machine-level memory intrinsics

Declared in an `extern "codira-intrinsic"` block and emitted in place, with
addresses as `usize`:

- `stack_alloc(size, align) -> usize`: memory in the calling function's
  frame, freed when it returns. Constant sizes get one slot in the entry block.
- `load_nt_<t>`/`store_nt_<t>`: non-temporal accesses that bypass the cache.
- `prefetch_read`, `prefetch_write`, `prefetch_read_once`.
- `fence_acquire`, `fence_release`, `fence_acq_rel`, `fence_seq_cst`,
  `compiler_fence`.
- `atomic_load_<t>`, `atomic_store_<t>`, `atomic_cas_<t>(addr, expected, new)`,
  and `atomic_<op>_<t>(addr, value)` returning the old value, for `add sub and
  or xor swap max min` (signedness from the type: `atomic_max_i64` compares
  signed). All sequentially consistent.
- `cache_line_size() -> usize`, `frame_address() -> usize`.

`<t>` is `u8`/`i8` through `u64`/`i64` (`f32`/`f64` for non-temporal and atomic
loads and stores). As with `load_*`/`store_*`, nothing checks the address --
unless the program is built with one of the experimental modes below.

### Experimental hardware safety

> **EXPERIMENTAL.** These modes cover the unchecked escape hatches
> (`load_*`/`store_*` and the other address intrinsics, `stack_alloc`, region
> arenas and OS pages). None of it has run on MTE or CHERI hardware: the
> machine it was written on has neither, and no `qemu-aarch64`. What was
> verified, and how, is stated for each mode. Expect changes.

#### ARM MTE: `--sanitize=memtag`

```bash
codira build --target aarch64-unknown-linux-gnu --target-feature +mte --sanitize=memtag
codira build --target aarch64-unknown-linux-gnu --target-cpu cortex-a710 --sanitize=memtag
```

For aarch64 **Linux** targets whose features include `+mte` (from
`--target-feature +mte`, a comma-separated list of LLVM feature changes, or
from an MTE CPU given to `--target-cpu`). Any other target is refused before
code generation: another architecture ("needs an aarch64 target with the
`+mte` feature"), aarch64 without `+mte` ("needs the `+mte` target feature"),
and non-Linux aarch64 (no `PROT_MTE`/`PR_SET_TAGGED_ADDR_CTRL`).

What is tagged, with a random 4-bit tag in bits 56..59 of the address that the
CPU compares with the memory's tag on every load and store:

- `stack_alloc` slots (generated code): rounded up to 16-byte granules,
  `llvm.aarch64.irg` (`IRG`) picks a tag, `llvm.aarch64.settag` (`STG`/`ST2G`)
  writes it, and `stack_alloc` returns the tagged address. Before every
  return the slot is set back to tag 0, so an address that outlives the frame
  faults. A `stack_alloc` with a size computed at run time is refused with its
  location.
- Region arenas (runtime, `native/memtag_aarch64.c`): chunks are `PROT_MTE`
  mappings; each allocation is rounded to granules and gets a tag that is
  never 0 and never the previous allocation's, so overflowing into the next
  allocation always faults. `codira_region_reset` sets the memory back to tag
  0 and `codira_region_free` unmaps it, so stale addresses fault.
- OS pages (`codira_page_alloc*`): mapped `PROT_MTE`, one tag per mapping;
  huge pages are not used under memtag.

Tag 0 is never handed out, so an untagged address -- an integer forged into
an address, or a pointer from code that does not know the tag -- faults on
tagged memory. The garbage-collected heap and `codira_aligned_alloc` stay
untagged (they come from the C allocator, which is not mapped `PROT_MTE`) and
work exactly as before.

Tagged addresses are still `usize`. Adding an offset keeps the tag (any
user-space address plus an in-bounds offset stays below 2^56). Comparing or
subtracting addresses of *different* allocations compares their tags first, so
it is meaningless there; `addr & 0x00ff_ffff_ffff_ffff` strips a tag. Passing
a tagged address to C works (top-byte-ignore, and the tagged-address ABI for
system calls). A page that will be *executed* must be called through its
untagged address.

At load time the runtime asks for synchronous tag-check faults
(`prctl(PR_SET_TAGGED_ADDR_CTRL)`, tags 1..15 allowed) and adds `PROT_MTE` to
the loading thread's stack. A memtag library loaded where that is impossible
(no MTE, or not aarch64 Linux) is refused rather than run unchecked. Threads
that existed before the library loaded are not checked.

Verified here: an aarch64 Linux object built with `+mte` has `IRG` and tag
stores for a `stack_alloc` (the test decodes the object's instruction stream,
and disassembles it with the LLVM installation's `llvm-objdump`) and an object
built without the mode has no MTE instruction; the runtime's C primitives
compile for aarch64 with `IRG`, `ST2G`, `STG` and `LDG` in their disassembly;
a freestanding aarch64 test program built from them links. The test that
*runs* that program (in-bounds accesses work; overflowing, untagged and stale
pointers end in SIGSEGV) runs only where `/proc/cpuinfo` lists `mte` or
`qemu-aarch64` is on `PATH`, and is skipped here with that reason. Limits:
tag checks are probabilistic only where two allocations happen to share a tag
(1 in 15, never for neighbours in a region); `--sanitize=memtag` cannot go
through the daemon or `--watch`; there is no aarch64 build of the runtime in
this repository's tests, so the region/page integration has only been
compiled for the host, where it is inactive.

#### CHERI and Morello targets: refused

`--target aarch64-morello` (or any triple naming `morello`, `cheri` or
`purecap`) is refused with an explanation, not faked. On Morello every
pointer is a 129-bit capability -- a 64-bit address plus bounds compressed
relative to it and permissions in 64 more bits, with a validity tag the
hardware keeps outside addressable memory -- so pointer size, alignment, the
stack and every load and store change. Building for it needs a
capability-aware LLVM (CHERI LLVM; the LLVM this compiler links does not have
it), the purecap ABI (`-mabi=purecap`), and a Codira runtime, C library and
loader built for it. None of that exists here.

### Proven rewrite rules: `@rewrite`

A library can teach the optimizer an equation over a function's parameters:

```codira
@rewrite(from: 3 * a + 2 * a, to: 5 * a)
func five(a: i64) -> i64 { 5 * a }
```

`from` and `to` (or two positional arguments) are expressions over the
parameters using literals and operators. The function only names the
variables and their types. Each rule is proven by the translation validator
(`codira_tv`) when the package is built, and only a proven rule is used.
A false rule fails the build with a counterexample (`the two sides differ for
a = 1`). So does a rule the validator cannot prove: it decides linear integer
arithmetic, so a rule that multiplies two parameters, uses bitwise operators
or uses floats is refused for now. A rule proven only in the absence of
overflow is accepted when it uses nothing but `+`, `-` and `*` (it then holds
for wrapping `i64` too) and refused otherwise. Proven rules are applied by the
e-graph during Eidos optimization. Today that only feeds whole-function
constant folding, so a rule cannot yet change the code that is generated.

### GPU kernels: `@target(gpu)`

A function marked `@target(gpu)` is also compiled for NVIDIA GPUs. `codira
build` writes its PTX to `target/<module>.ptx`, next to the library, and the
CUDA driver JIT-compiles that for whatever GPU it finds. A GPU function that
returns nothing is a kernel, which the host launches. One that returns a value
is a device function, which kernels can call. Buffers are `usize` device
addresses, read and written with the `load_<t>`/`store_<t>` intrinsics. Each
thread finds its work item with `gpu_thread_id_x/y/z`, `gpu_block_id_*`,
`gpu_block_dim_*` and `gpu_grid_dim_*` (all `-> u32`), and
`gpu_sync_threads()` is the block barrier:

```codira
extern "codira-intrinsic" {
    func gpu_thread_id_x() -> u32;
    func gpu_block_id_x() -> u32;
    func gpu_block_dim_x() -> u32;
    func load_f32(addr: usize) -> f32;
    func store_f32(addr: usize, value: f32);
}

@target(gpu)
public func vadd(a: usize, b: usize, c: usize, n: u32) {
    let i = gpu_block_id_x() * gpu_block_dim_x() + gpu_thread_id_x()
    if i < n {
        let at = (i as usize) * 4
        store_f32(c + at, load_f32(a + at) + load_f32(b + at))
    }
}
```

The GPU has no Codira runtime, so a GPU function may take and return only
integers, floats and booleans, and may call only other `@target(gpu)`
functions and intrinsics: no classes or other garbage-collected values, no
host functions, no `extern` functions. Anything else fails the build with the
reason. In the host library the same function is an ordinary function, and
the geometry intrinsics describe a one-thread launch (ids 0, dimensions 1).

Launching from Rust goes through the `codira_gpu` crate: `Gpu::open()`,
`load_ptx`, `upload`, `launch`, `copy_to`. It opens the CUDA driver library at
run time, so nothing links against CUDA, and a machine without a GPU gets an
error instead of a crash. There is no launch syntax in Codira yet. A vector
add over 100,000 elements is tested on an RTX 4080 against the host result;
without a driver or device that test reports that it was skipped.

### Quantum kernels (`codira quantum`)

A function marked `@target(qpu)` is a quantum kernel: the compiler runs its
body at compile time and turns it into a circuit. Gates come from an
`extern "codira-quantum"` block -- print the full block with
`codira quantum prelude` and paste it into the file.

```codira
extern "codira-quantum" {
    func h(q: i64);
    func cx(control: i64, target: i64);
    func measure(q: i64, c: i64) -> bool;
}

@target(qpu)
func ghz(n: i64) {
    h(0);
    let mut i = 1;
    while i < n {
        cx(i - 1, i);
        i += 1;
    }
    let mut j = 0;
    while j < n {
        measure(j, j);
        j += 1;
    }
}
```

```bash
codira quantum run ghz.code --param n=5 --shots 1000 --seed 7     # built-in simulator
codira quantum run ghz.code --param n=5 --exact                    # exact probabilities
codira quantum build ghz.code --param n=5 --emit qir -o ghz.ll     # QIR base profile (text)
codira quantum build ghz.code --param n=5 --emit qir-bc -o ghz.bc  # QIR bitcode (QDK / Azure)
codira quantum build ghz.code --param n=5 --emit qasm3             # OpenQASM 3 (also qasm2)
codira quantum import circuit.qasm                                 # Qiskit export -> .code kernel
codira quantum run circuit.qasm --shots 100                        # run a .qasm/.ll/.bc directly
```

Rules that matter when writing kernels:

- Parameters must be known at compile time: pass `--param name=value`.
- Loops need compile-time bounds (they are unrolled; a fuel limit stops
  runaway loops with an error).
- You may ignore or return a `measure` result, but never branch on it or
  compute with it: the QIR base profile has no measurement feedback, and the
  compiler rejects it with the source line.
- Kernels may call other kernels and ordinary helper functions; ordinary
  functions may not call gates.
- Kernels use integers, floats, bools, `let`/`let mut`, `if`, `loop`/`while`
  and calls. Arrays, tuples, structs and methods are not supported inside
  kernels yet.
- Don't name a variable after a gate (`x`, `t`, `s`, ...): it shadows the
  gate function.

From Python, `bindings/python/codira_quantum` converts kernels to and from
Qiskit circuits and produces QIR bitcode; see `crates/codira_quantum/README.md`
for the full surface, the profile limits, and what is verified.

### Zero-knowledge proofs: `codira attest`

An `oblivious func` never branches on, indexes by or loops over its inputs,
so it is a fixed circuit. `codira attest` proves, in zero knowledge, that the
function returns a given value on inputs only the prover knows -- the
roadmap's `attest score(x, y) reveal r`, as a command:

```codira
oblivious func score(x: i64, y: i64) -> i64 {
    var total = 0
    for i in 0..3 {
        total += x * (i + 1) - (y >> i)
    }
    total + ct_select(x > y, 10i64, -10i64)
}
```

```bash
codira attest build score.code::score --out-dir keys   # score.r1cs .circom .pk .vk
codira attest prove score.code::score --input x=-7 --input y=1234 --pk keys/score.pk -o score.proof
codira attest verify --vk keys/score.vk --proof score.proof              # valid
codira attest verify --vk keys/score.vk --proof score.proof --reveal 5   # INVALID
```

- The result is the one public value (`reveal`); every input stays private.
  `verify` needs only the verifying key and the proof file.
- Integer semantics are exact: `i64 + i64` wraps at 2^64 in the proof as it
  does natively (values are bit-decomposed and reduced; the field's own wrap
  never shows). Per-operator constraint costs are in
  `crates/codira_circuits/README.md` (64-bit `+` is free until reduced, `*`
  costs 1 plus a 130-constraint reduction, `<` costs 66).
- The circuit subset: integers of 8 to 64 bits and `bool`; `+ - * & | ^ ~ !
  << >>`, comparisons, `as`, `ct_select`, `declassify`; `let`/`var`; calls to
  other oblivious/`@constant_time` functions (inlined); `while`/`loop`/`for`
  with compile-time bounds (unrolled); `/` and `%` on compile-time values
  only. Floats, `i128`, arrays, structs, objects, effects, `print` and any
  branch on an input are refused with the line and the reason.
- Proofs are Groth16 over BN254 (arkworks). `build` runs a one-party setup:
  whoever runs it could forge proofs for that circuit. The `.r1cs` (iden3
  format) and `.circom` files work with circom/snarkjs for anything else.
- Shifting by the width or more is undefined natively; circuits give 0 (or
  all sign bits for signed `>>`).

### FPGA: `@target(fpga)`

A function marked `@target(fpga)` is also compiled to a synthesizable
Verilog-2001 module: `codira build` writes `target/<function>.v`, one
combinational module with an input port per parameter and an output port
`result`. The function stays an ordinary host function too.

```codira
@target(fpga)
public func clamp(x: i32, lo: i32, hi: i32) -> i32 {
    var r = x
    if x < lo { r = lo }        // an `if` on an input becomes a multiplexer
    if x > hi { r = hi }
    r
}
```

- Same subset as `attest`, plus `if`/`&&`/`||` on inputs (both sides are
  built and selected between). A `return`/`break` under an input-dependent
  condition, or a loop whose bound depends on an input, is refused; so is
  anything outside the subset, which fails `codira build` with the reason.
- Exact two's complement: every net is unsigned and exactly as wide as its
  type; signed `<` is `$signed(a) < $signed(b)`, signed `>>` is `>>>`.
- Validated by the netlist evaluator against native execution, and by Yosys
  (the pip-installable YoWASP build) parsing, synthesising (`synth -noabc`)
  and evaluating the output. No simulator such as Icarus or Verilator, and
  no vendor FPGA flow, has run it. No clocks, registers or pipelining yet.

### C, C++ and assembly in a package: `[native]`

```toml
[native]
sources = ["c/kernels.c", "c/table.cpp", "c/crc.S"]
include = ["c/include"]
defines = ["SCALE=3"]
flags = ["-O3"]
libs = []
```

```codira
extern "C" {
    func c_mul_add(a: i64, b: i64) -> i64;
}
public func mul_add(a: i64, b: i64) -> i64 { c_mul_add(a, b) }
```

`codira build` compiles C (`.c`) and C++ (`.cc/.cpp/.cxx`) with Clang to LLVM
bitcode and links it into each module that calls it **before** that module
is optimized, so LLVM inlines C into its Codira callers (tested: at `-O2` a C
helper leaves no call behind). Assembly (`.s`, or `.S`/`.asm` through the
preprocessor) is assembled to objects and linked. An `extern "C"` function the
native code defines is called directly, not through the runtime's dispatch
table. On Windows such an assembly links the DLL C runtime and gets a real
entry point, so C++ static constructors run.

Clang is `CODIRA_CLANG`, else the one in the LLVM installation the compiler was
built against, else `clang` on `PATH`. Outputs are cached in
`target/native/` and rebuilt when the source, a header it includes, or the
flags change. Limits: each module is its own library, so two modules that use
one C file each get their own copy of its globals; watch mode and the daemon
compile native sources only when the package is loaded; C++ exceptions must
not escape into Codira code.

### Importing a C header: `codira import-c`

```bash
codira import-c c/include/kernels.h -I c/include -o src/kernels.code
```

Clang reads the header exactly as a C compiler would (macros, typedefs,
includes) and `import-c` writes an `extern "C"` block for every function the
header itself declares whose types Codira can carry: integers, floats,
`_Bool`, and pointers, which become `usize` addresses. Structs and unions by
value, variadic functions, `static`/`inline` functions and `long double` are
listed in a comment with the reason instead. `--target` reads the header for
another target (`long` is 32 bits on Windows); `--cxx` reads it as C++.

### Bun

`bindings/bun` is a Bun plugin: with `preload = ["codira-bun/register"]` in
`bunfig.toml`, `import { add } from "./native/src/mod.code"` builds the
package, binds that module's `@export("C")` functions through `bun:ffi` and
returns them as named exports. Each build is loaded from a private copy named
after its hash, so `bun --hot` sees rebuilds. Underneath it uses
`codira bindgen --runtime bun --module <file> --named-exports`, and
`--print-library` to find the built library.

---

## 5. End-to-end status today

Verified through the full pipeline (parser → HIR → LLVM → link → run):

- `struct`, `class`, `func`, `let`/`var`, tuples and tuple patterns
- field access, arithmetic, comparisons, `as` casts
- `if`/`else`, `while`, `loop`, `for` over ranges and arrays, `break`,
  `continue`, short-circuit `&&`/`||`, function calls, methods via `extend`,
  `Type.assoc_fn()` static calls
- arrays: literals, bounds-checked indexing and element assignment, `len()`,
  `unchecked_get(i)` (arrays of tuples type-check but do not link yet: the
  runtime has no type info for tuple element types)
- `array_filled(v, n)`, `select`, `fma`, `likely`/`unlikely` (see "Hot-loop
  builtins"; not supported by the Cranelift `--jit` backend, which refuses them)
- `str` literals, and `print`/`println` (`codira run` shows the output)
- optionals (`T?`, `nil`, `if let`, `while let`, postfix `!`, `== nil`) and
  `match` over literals, tuples, strings and optionals, checked exhaustive
- `enum` with payloads: construction, `match`/`if let`/`while let` with
  nested variant patterns (checked exhaustive), `==`, methods via `extend`,
  enums in fields, arrays, parameters and results, across modules and hot
  reload (see "Enums & pattern matching" for the limits)
- generic functions, structs and classes, monomorphised per instantiation
  (see "Generics" above for the limits; generic functions are not exported
  to the host)
- traits with static dispatch: `trait` declarations with required and
  default methods and supertraits, `extend Type: Trait` for structs,
  classes, enums, generic structs and primitive types, bounds
  (`[T: A + B]`, `where`, `extend Box[T: A]`), trait-method calls resolved
  per instantiation (see "Traits and bounds" for the limits: no generic
  traits, associated types, trait objects or operators on a `T`)
- imports and visibility (`public`/`internal`/`private`)
- importing standard-library modules, explicitly, with no prelude:
  `std.crypto.hash`, `std.fs.fs`, `std.memory.region`, `std.bit`,
  `std.math.constants`, `std.math.fast`, `std.hashlib.hash`,
  `std.base64.base64`, `std.random.rng`, `std.sys.terminate`,
  `std.math.math`, `std.time.time`, `std.complex.complex`,
  `std.collections.optional` (see "Importing the standard library" for
  what is loaded, how it is linked, which modules qualify and why the
  others do not yet)
- functions returning `never`, called locally, across modules and as
  `extern "C"` declarations (see "Functions")
- `public` functions that take and return value structs, called from
  other modules and from the host
- algebraic effects and structured concurrency (see below)
- function values: named functions as values, closures with by-value
  captures, calls through locals, fields and arrays (see "Function values
  and closures")

Contracts (`requires`, `ensures`, `invariant`, `decreases`, `ghost let`,
refinement types on parameters and returns) are checked statically by Z3 in
`codira check` and `codira build`; see "Contracts and verification" above for
what the checker models and what it reports as uncheckable.
`codira build --certify` writes a proof certificate per library (obligations
as SMT-LIB2 queries, source and machine-code hashes) and `codira certify`
re-checks it with Z3 alone; see "Proof certificates" for exactly what that
establishes.
`codira api-diff` classifies public API changes between two versions as
major, minor or patch -- contract changes decided by Z3 implication -- and
`--check` fails when the manifest's version is not bumped far enough.

Runtime (hosts embedding `codira_runtime`):

- The GC heap is a segregated size-class allocator with per-thread bump
  allocation; `Runtime::gc_collect()` collects objects not reachable from
  host-rooted handles. There are no stack maps: never collect while another
  thread runs Codira code.
- `Runtime::builder(lib).heap_limit(bytes)` caps the GC heap. An allocation
  that would exceed it first collects (scanning the allocating thread's stack
  conservatively; only while one thread allocates) and retries; if memory is
  still short the process aborts with a message naming the type and limit.
  It caps the GC heap only, not raw allocations.
- The runtime provides raw memory services a program reaches with
  `extern "C"` declarations (wrapped in `std/memory/region.code` and
  `std/memory/page.code`): region arenas with byte quotas and bulk reset
  (`codira_region_*`), OS pages with W^X protection, huge pages and icache
  flushing (`codira_page_*`, `codira_icache_flush`), and aligned allocation.
  Addresses are `usize`; failures return 0/false. Region memory is untraced.
- Files and hashes (Linux tested; macOS and Windows built but never run):
  the runtime provides file and hash services that a program reaches with
  `extern "C"` declarations, wrapped in `std/fs/fs.code` and
  `std/crypto/hash.code`. A program imports the wrappers
  (`import std.fs.fs.*`, `import std.crypto.hash.*`) or declares the
  services itself.
  - **Opening:** `codira_fs_open(path_addr: usize, path_len: usize, flags:
    u32) -> i64`. The flags are READ 1, WRITE 2, APPEND 4, CREATE 8,
    TRUNCATE 16 and CREATE_NEW 32; pass the path with `str_data`/`str_len`.
  - **Reading and writing:** `codira_fs_read` and `codira_fs_write(fd: i64,
    buf: usize, len: usize) -> i64` are single calls that may return a short
    count; a read returns 0 at the end of the file.
  - **Other file calls:** `codira_fs_seek(fd, offset: i64, whence: u32) ->
    i64` and `codira_fs_size(fd) -> i64`. `codira_fs_fsync`,
    `codira_fs_close`, `codira_fs_remove`, `codira_fs_rename` and
    `codira_fs_sync_dir` return `i32`.
  - **Results:** a value >= 0, or a negative `FS_ERR_*` code: -2 not found,
    -5 invalid argument, -6 bad handle, -7 path too long, -8 path not UTF-8,
    -18 wrong mode, and so on. `codira_fs_last_os_error()` gives the raw
    errno.
  - **Handles** are generation-checked runtime handles, not OS descriptors,
    so a closed or forged handle is refused.
  - **Durability:** a new or renamed name is durable only after
    `codira_fs_sync_dir` on its directory. That call is unsupported on
    Windows, where renames are not made durable. On Linux, do not retry
    after an fsync error.
  - **Hashes:** `codira_hash_sha256`, `codira_hash_sha512` and
    `codira_hash_blake3(data_addr, len, out_addr) -> bool` write 32, 64 and
    32 bytes. They use the RustCrypto `sha2` and the `blake3` crates and are
    checked against the NIST and BLAKE3 known answers. They may be declared
    `@noalloc @nopanic @noblock` (a trusted assertion, as
    `std/crypto/hash.code` does).
  - **Effects:** file services are ordinary externs, so they count as
    blocking.
  - **Not cryptographic:** `std.hashlib` (FNV-1a) and `std.random`
    (xoshiro256**).
  - **Clocks and termination:** `codira_time_now() -> f64` (seconds since
    the Unix epoch), `codira_time_monotonic_ns() -> u64` (from the first
    call in the process), `codira_time_sleep_ns(u64)`, and the hooks
    `panic` is written on (`codira_write_stderr(addr, len)`,
    `codira_abort`, `codira_exit(code)`, `codira_is_debug`), for
    `std/time/time.code` and `std/sys/terminate.code`.
  - Tests: `crates/codira/tests/fs_and_hash.rs`. Durability is claimed for Linux
    only; macOS and Windows are built but not run.
- Database and tensors (experimental, x86-64 Linux tested): the
  `codira_db` engine and a first slice of `codira_ai` are reached through the
  runtime services `codira_db_*` and `codira_ai_*`, wrapped in
  `std/db/db.code` and `std/ai/runtime.code`. Until `std` can be imported,
  declare the `extern "C"` services in the program.
  - **Database:** a program can open a directory with an fsync policy, run
    snapshot-isolation transactions (`get` into a caller buffer, `put`,
    `delete`), commit, abort, checkpoint and close. Commit returns the
    commit timestamp, and a write-write conflict returns its own code, -3.
    Snapshot isolation is not serializable: write skew is possible. A
    directory may be opened once per process, and nothing stops a second
    process. Scans and typed tables are not exposed.
  - **Tensors:** a program can create f32 tensors from a buffer and shape,
    and run `matmul` (batched), broadcasting `add`, `relu` and `softmax`. It
    can read values back, and train a Linear layer with
    `codira_ai_linear_mse_sgd_step`.
  - **Handles and errors:** handles are generation-checked `i64` values,
    tagged by kind, so a transaction handle passed as a database handle is
    refused. Tensors and layers are not traced by the GC and must be freed
    explicitly. Every error is a documented negative code.
  - **Effects:** all of these services count as blocking and allocating.
  - Tests: `crates/codira/tests/db_and_ai.rs`.
- Experimental hardware safety: `codira build --sanitize=memtag` tags
  `stack_alloc` slots, region memory and OS pages with ARM MTE on aarch64
  Linux targets with `+mte` (built and inspected, never run: see
  "Experimental hardware safety").
- Fuel: `codira build --fuel` instruments every function entry and loop
  iteration to decrement a counter; `codira start --fuel <n>` /
  `codira run --fuel <n>` (or `RuntimeBuilder::fuel(n)`) sets the budget.
  Running out prints a message and ends the call (a `FuelExhausted` fault a
  HERACLES guard catches, with `--heal`). Measured cost: about 2-12% in loops.
  Uninstrumented code is not metered.
- Hot reload keeps object state across layout changes: fields are matched
  by name, numeric fields that change type convert with saturating
  semantics, and a renamed field keeps its value when marked
  @renamed(from: "old"). Runtime::save_heap / restore_heap write the GC
  heap to a file and rebuild it in a runtime running the same program.

Circuits: `codira attest build|prove|verify` proves `oblivious` functions
in zero knowledge (Groth16 over BN254, exact integer semantics, `.r1cs` and
Circom output), and `codira build` writes `target/<function>.v` (Verilog) for
`@target(fpga)` functions; see their sections above for the subset.

`@noalloc`, `@nopanic`, `@noblock` and `@realtime` are checked statically in
`codira check` and `codira build` (see their section above); `@deadline` is
rejected.
Experimental: `@envelope(..)` declarations are checked the same way, and
`codira envelope` reports every function's inferred envelope (see
"Semantic envelopes").

Algebraic effects (`effect`, `uses`, `perform`, `handle`, `resume`) with
one-shot, tail-resumptive or aborting handlers, nested handlers, and aborts
across call frames; see "Algebraic effects" above for the version-1
restrictions (no captures in arms, `uses` functions callable only within
their module).

Structured concurrency (`scope`, `spawn`, `channel(n)`, `ch <- v`, `<-ch`)
on a worker pool; see "Structured concurrency" above for what may cross
between tasks (scalars and channels only), `select` over channel
operations (fair, with an optional `default`), and session types (two
tasks following dual protocols, checked at compile time; see "Session
types").

Parsed but **not yet lowered** -- the compiler stops with "... is not
supported by the code generator yet" naming the construct: postfix `^`. Not available at
all yet: generic enums, generic traits, trait objects (dynamic dispatch),
`String`, module-level `let` and `var` (rejected with "module-level `let`
is not supported yet"), in-place parameters (`inout`/`mut`/`out` and
`mut T`/`&mut T`, rejected with "... are not supported yet"), the
standard library's
prelude in programs, and the standard-library modules that are not
importable (most of `std/`: see "Importing the standard library").

---

## 6. Reference material (in this repository)

- `spec/LANGUAGE_SPEC.md` — the canonical, current language spec.
- `spec/HERACLES_Codira_Examples.code` — self-healing framework examples
  (`@heal(...)`, refinement types, `supervisor`).
- `book/src/**` — chapter-based tutorial (getting started, hot-reload, arrays,
  structs).
- `examples/**` — runnable Codira (and Rust, C++, C) example projects.
- `std/` — the Codira standard library source (`*.code`).

Always cross-check written code against `spec/LANGUAGE_SPEC.md` and the parser
tests in `crates/codira_syntax/src/tests.rs`.

---

## 7. Agent coding rules

1. **Use `.code` files.** Start with a single `src/mod.code`; nest modules via
   subdirectories when a package grows.
2. **Prefer explicit types** at module and interop boundaries (inference works
   but less clarity).
3. **No `null`** — design around `Type?` optionals and explicit error returns.
4. **No `::`, no `<>`** — use `.` paths and `[T]` generics on args.
5. **Semicolons** are optional; keep lines mostly semicolon-free unless two
   statements share one line.
6. **Verify** with `codira build` and `codira start` rather than guessing.
7. When this guide and the spec disagree, **obey `spec/LANGUAGE_SPEC.md`**.

Happy coding in Codira!