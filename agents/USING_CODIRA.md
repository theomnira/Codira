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

`f"..."` (an `f` directly before the quote) is an interpolated string
literal, `f"x = {x}"`: see "Strings".

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

`@inline` before a function asks for every call of it to be inlined
(LLVM's `alwaysinline`). It matters most across modules: a call into
another module's library is otherwise an indirect call through the
dispatch table, which LLVM can never inline, while a public `@inline`
function is compiled into each library that calls it, like an instance of
a generic function, and inlined there. Use it for small functions on hot
paths (`std.text.ascii`'s `parse_i64`, `std.hashlib.hash`'s integer
hashes, `std.collections.map`'s lookups). As for generic instances, a hot
reload of the function's module reaches the callers when they are rebuilt.
(`inline_functions_of_other_modules_are_compiled_in`,
`crates/codira/tests/hash_map.rs`.)

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
for x in xs.iter().filter(f)  // a lazy pipeline (see "Lazy pipelines")
for x in it { }               // an object of a class with `next(self) -> T?`
while cond { if skip { continue } }
loop { if done { break } }

let xs = [3, 1, 4]
xs.len()                      // 3, as usize
xs[i]                         // bounds-checked: an out-of-range index panics
xs.unchecked_get(i)           // no check, for hot loops with a proven index
```

A range (`a..b`, `a..=b`) is only valid as a `for` loop's iterable or as the
start of a lazy pipeline (`(0..n).map(f).sum()`) for now -- there is no range
value type. `continue` is a contextual keyword: it means
"next iteration" only when it stands alone (before `}`, `;` or a line break).
`&&` and `||` short-circuit.

An out-of-range index prints `panic at mod.code:7:13: index out of bounds: the
length is 3 but the index is -1` to stderr and ends the program with exit
status 101. A panic cannot be caught from Codira yet.

### Growable arrays and collections

`[T]` itself grows: it is Codira's `Vec`. There is no separate list type.

```codira
let xs: [i64] = []                 // empty: the object, no element buffer yet
for i in 0..10 { xs.push(i * i) }  // amortized O(1): a full buffer doubles
let last = xs.pop()                // i64?: nil when empty
xs.insert(0, 100)                  // 0 <= i <= len, shifts the rest up
let gone = xs.remove(3)            // i < len, shifts the rest down
xs.append([7, 8])                  // another array's elements (`extend` is a keyword)
xs.reserve(1000)                   // room for 1000 more without growing
xs.clear()                         // length 0, capacity kept
xs.len(); xs.capacity(); xs.is_empty(); xs.last()   // last: T?

xs.contains(49); xs.index_of(49)   // bool; usize? (first match)
xs.sort(); xs.reverse()            // in place
xs.binary_search(49)               // usize? in a sorted array
let part = xs[2..5]                // a copy of elements 2, 3, 4 (`..=` includes the end)
let copy = xs.copy()

let doubled = xs.map(func(x) { x * 2 })           // [i64]
let big = xs.filter(func(x) { x > 10 })           // [i64]
let total = xs.fold(0, func(acc, x) { acc + x })  // i64
xs.any(func(x) { x == 4 }); xs.all(func(x) { x >= 0 })  // stop early
xs.sum(); xs.min(); xs.max()                      // T, T?, T?
for (i, x) in xs.enumerate() { }                  // index and element
```

- **Every index is checked**, with the source location: `insert`, `remove`
  and slicing panic like `xs[i]` does (`panic at mod.code:5:13: index out of
  bounds: the length is 2 but the index is 2`; a slice whose start is after
  its end says `slice starts after its end`), exit status 101.
- **Slicing copies.** `xs[lo..hi]` is a new array, so it can never dangle
  when `xs` later grows or shrinks, and writing to it leaves `xs` alone. It
  costs O(hi - lo). There is no view type.
- **What the element type needs.** `push`, `pop`, `insert`, `remove`,
  `append`, `map`, `filter`, `fold`, `any`, `all`, `copy`, `reverse` and
  slicing take any `T`, in generic code too. `contains`/`index_of` need
  integers, floats (IEEE `==`: `NaN` is never found), `bool` or `str`
  (bytes); `sort`/`binary_search` integers, floats, `bool` or `str`;
  `min`/`max` integers, floats or `bool` (a float `NaN` is skipped, unless
  it is the first element); `sum` integers (wrapping) or floats (added in
  order from 0). `binary_search` returns the index of some equal element
  when there are several. Anything else is a compile error saying what is
  needed. These are builtin, not trait-bounded: operators are not resolved
  through traits, so on a type parameter they are errors too; generic code
  uses `Eq`/`Ord` from `std.cmp.cmp` and writes the loop.
- **`sort` is unstable** (pattern-defeating quicksort, in place, no
  allocation, O(n log n) worst case: the runtime's Rust `sort_unstable`).
  Floats sort by IEEE 754 total order (`-NaN < -inf < .. < -0.0 < 0.0 < ..
  < inf < NaN`). There is no `sort_by` with a comparator yet.
- **Closures are compiled in place.** When the argument of `map`, `filter`,
  `fold`, `any` or `all` is a closure literal without `return`, its body is
  generated inside the loop: no function value is created and nothing is
  called, so `xs.fold(0, func(a, x) { a + x })` is the loop written by hand
  (measured below). A named function (`xs.map(square)`) is called directly.
  Any other function value is evaluated once and called through on every
  element. A closure with `return` is an ordinary function value (`return`
  leaves the closure).
- **These are eager.** `xs.map(f).filter(g).sum()` builds the mapped and
  the filtered arrays (one allocation per stage, none per element), and
  runs `f` on every element before `g` sees any. The lazy form,
  `xs.iter().map(f).filter(g).sum()`, is one loop with no array in between
  (see "Lazy pipelines" below).
- **Changing an array while a loop runs over it** is memory-safe and
  bounded: `for x in xs`, and the closure-taking methods, read the length
  once before the first iteration and stop at the smaller of it and the
  current length. An element pushed during the loop is not visited; when
  the loop shortens the array it ends at the new end. Loop with
  `while i < xs.len()` to see the pushed elements too.
- **Sharing.** An array is a reference: `let ys = xs` shares one array, and
  a function that pushes onto its parameter grows the caller's array. Use
  `copy()` or a slice for an independent array.
- **Effects.** `push`, `insert`, `reserve`, `append` (which can grow the
  buffer), `copy`, `map`, `filter` and slicing (a new array) allocate, so
  `@noalloc` rejects them; `insert`, `remove` and slicing can panic. `pop`,
  `clear`, `contains`, `sort`, `sum`, `fold`, `any`, `all` and the rest do
  neither, and a closure compiled in place adds only what its body does: a
  `@noalloc @nopanic` function may call `xs.fold(0, func(a, x) { a + x })`.
  A function value passed instead counts as unknown (every effect).
- Not supported by `codira run --jit` (Cranelift): a program using arrays
  says so and runs on LLVM.

**Collections in the standard library** (`import std.collections.<module>`),
built on `[T]`, all classes (assigning one shares it):

```codira
import std.collections.map.{Map}
import std.collections.set.{Set}

let ages: Map[str, i64] = Map.new()
ages.insert("ada", 36);                  // returns the replaced value, i64?
if let age = ages.get("ada") { println(age) }
ages.get_or("bob", 0); ages.contains_key("ada"); ages.remove("ada")
ages.len(); ages.keys(); ages.values(); ages.entries(); ages.clear()
for (name, age) in ages.iter() { println(name, " ", age) }
let big: Map[i64, i64] = Map.with_capacity(10000)   // no growth up to 10000
big.reserve(5000); big.capacity(); big.shrink_to_fit()

let seen: Set[i64] = Set.from_array([1, 2, 3])
seen.insert(4); seen.contains(2); seen.remove(1)   // bool each
seen.union(other); seen.intersection(other); seen.difference(other)
```

- `Map[K, V]` (`std.collections.map`) is a **Swiss table**: `8 * G` slots
  in `G` groups of 8 (a power of two), each slot a `(K, V)` pair, and one
  control byte per slot packed into a `u64` per group -- EMPTY (`0xFF`),
  DELETED (`0x80`, a tombstone), or FULL with the top 7 bits of the key's
  hash. A lookup reads the group the hash's low bits pick and tests its 8
  control bytes at once with word arithmetic (SWAR, as hashbrown's portable
  implementation does: the language has no vector types, so no SSE2/NEON
  16-byte groups); the bytes equal to the 7-bit tag give the few slots
  whose key is compared, and a group with an EMPTY byte ends the search.
  Groups are probed in triangular order. The table holds at most 7/8 of
  its slots and doubles when full; a removal leaves a tombstone only when
  its group has no EMPTY byte (a probe could pass through it), and a full
  table whose live entries are at most half its capacity is rebuilt at its
  size, which clears the tombstones.
- **Keys** need `Hashable` (`std.hashlib.hash`) and `Eq`
  (`std.cmp.cmp`), which every integer and float type, `bool`, `str` and
  `String` have; implement both for a type of your own.
- **Hashing, DoS-resistant by default.** `Map.new()` hashes with
  SipHash-1-3 under a secret 128-bit key of its own, drawn from the
  runtime's entropy service with the map's first insertion (see
  `std.hashlib.hash` below for why SipHash-1-3): whoever does not know the
  key cannot choose keys that collide. `Map.with_key(HashKey.new(k0, k1))`
  uses a key you give (the same iteration order every run, as resistant as
  the key is secret). `Map.unkeyed()` uses the keys' fast `hash_value()`
  (the MurmurHash3 finalizer for integers, FNV-1a for strings) and is
  **not DoS-resistant**: hash-flooding keys make it quadratic
  (`flooding_keys_slow_the_unkeyed_hash_and_not_the_keyed_default`
  computes 20,000 such keys by inverting the hash: 393 ms to insert and
  find them unkeyed, 1.5 ms keyed, 1.5 ms for random keys).
- **Custom hashers** are the key type's: wrap the key in a struct whose
  `Hashable` implementation (`hash_keyed` for `new`/`with_key`,
  `hash_value` for `unkeyed`) hashes what you want, for instance strings
  equal up to case (`custom_key_types_bring_their_own_hash`); every call is
  resolved at compile time and inlined. A hasher held as a function value
  was measured first and dropped: the indirect call it put into the probe
  loop of every map, custom or not, stopped LLVM from keeping the table in
  registers across lookups (2 ns more per lookup in a 1K-entry map).
- **Small maps**: a map of at most 7 entries is one group; with integer,
  float or `bool` keys (`compares_cheaply()`) it is scanned without
  hashing, which costs less than SipHash (`map_small5_secure_get` below).
- **Capacity**: `Map.new()` allocates only the map object; the key, the
  control words and the slot array come with the first insertion. The
  slot array is filled with copies of a key and value at hand (an array of
  a type parameter has no default value), so `with_capacity`/`reserve`
  allocate the control words at once and the slots on the first insertion.
  A removed entry's key and value stay in its slot until it is reused.
- **Iteration** (`m.iter()`, an iterator object over `(key, value)`
  pairs, in table order, which differs between maps): replacing the value
  of a present key and removing keys during an iteration are allowed --
  an entry removed before the iterator reaches it is not produced, and
  every entry present throughout is produced exactly once; an entry
  inserted under a new key may or may not be produced; an insertion that
  grows or rebuilds the table, and `reserve`, `shrink_to_fit` and `clear`,
  make the iterator's next step panic ("the map was resized or cleared
  while being iterated"). `keys()`, `values()` and `entries()` are copies.
  Nothing is ever memory-unsafe. (`iteration_follows_the_documented_mutation_rules`)
- `Set[T]` (`std.collections.set`) and `Counter[T]`
  (`std.collections.counter`: `add`, `count`, `most_common`, ...) are on
  `Map` and hash as it does; both have `unkeyed()`, `Set` also
  `with_capacity`, `with_key` and `reserve`.
- `Deque[T]` (`std.collections.deque`): a ring buffer, `push_front`,
  `push_back`, `pop_front`, `pop_back` (`T?`), `front`, `back`, `get`.
- `BinaryHeap[T]` (`std.collections.binary_heap`): a max-heap, elements
  need `Ord`; `push`, `pop` (largest first, `T?`), `peek`,
  `to_sorted_array`, `from_array` (O(n)).
- The static constructors take their type arguments from the expected type:
  `let m: Map[i64, i64] = Map.new()`.
- A `for` body, like any block, is checked against `()`: write
  `m.insert(k, v);` (with the `;`) as the last statement of a loop body,
  because `insert` returns the old value.
- Tests: `std_collections_map_matches_rust_hash_map` (`tests/std_imports.rs`)
  replays random sequences -- removals, re-insertions, growth to 50,000
  keys, `reserve`/`shrink_to_fit`/`clear`, tombstone churn over 48 and
  2,000 keys -- on every hasher, on `String` keys and on `Set`, against
  Rust's `HashMap`/`HashSet`; `tests/hash_map.rs` has the rest.

**Measured** (x86-64 VM, Intel Xeon @ 2.10 GHz, 4 cores; Codira at `-O 2`
with a release build of the compiler and runtime, Rust 1.97 at `-C
opt-level=3`; median of 5 alternating runs, milliseconds). The programs are
`examples/collections_bench/src/mod.code` and
`examples/collections_bench/collections_rust.rs`; both print the same
checksums.

| work | Codira | Rust |
|---|---:|---:|
| push 10M `i64` onto an empty array (no `reserve`) | 42.8 | 43.2 (`Vec::push`) |
| sum 10M: `xs.sum()` | 9.6 | 9.4 (`iter().sum()`) |
| sum 10M: `xs.fold(0, func(a, x) { a + x })` | 9.5 | 9.4 (`iter().fold`) |
| sum 10M: `for x in xs` | 9.0 | 9.5 (`for x in &v`) |
| `xs.map(x * 2).filter(x % 3 == 0).sum()` | 78.9 | 15.9 (lazy chain) |
| the same as one `fold` | 16.1 | 15.9 |
| sort 1M random `i64` | 19.5 | 19.7 (`sort_unstable`; `sort` 29.9) |

The map, at 1K, 100K and 1M keys (`i64` keys random, `String` keys
`"key-<n>"`); every 1K and 100K measurement repeats the operation on fresh
maps so that each is at least a million operations. Same machine, method
and checksums as above, `codira start` with its default heap limit; load
average about 2 on the 4 cores during the runs (other builds), so
differences under 10% are noise.

*Same hash on both sides* (`Map.unkeyed()` against `HashMap` with a `BuildHasher` computing the same function: the MurmurHash3 finalizer for `i64`, FNV-1a over the bytes for strings; neither is DoS-resistant):

| keys | operation | Codira ns/op | Rust ns/op | Codira / Rust |
|---|---|---:|---:|---:|
| 1K `i64` | insert into a new map | 62.4 | 17.9 | 3.49x |
| 1K `i64` | lookup, hit | 4.3 | 3.0 | 1.43x |
| 1K `i64` | lookup, miss | 3.7 | 3.0 | 1.23x |
| 1K `i64` | iterate | 2.9 | 0.9 | 3.11x |
| 1K `i64` | remove | 6.4 | 5.8 | 1.09x |
| 100K `i64` | insert into a new map | 28.1 | 17.3 | 1.62x |
| 100K `i64` | lookup, hit | 10.4 | 6.8 | 1.52x |
| 100K `i64` | lookup, miss | 13.6 | 8.8 | 1.54x |
| 100K `i64` | iterate | 3.9 | 1.6 | 2.45x |
| 100K `i64` | remove | 19.7 | 15.7 | 1.25x |
| 1M `i64` | insert into a new map | 65.7 | 63.9 | 1.03x |
| 1M `i64` | lookup, hit | 27.5 | 25.5 | 1.08x |
| 1M `i64` | lookup, miss | 10.5 | 9.5 | 1.10x |
| 1M `i64` | iterate | 6.1 | 3.9 | 1.54x |
| 1M `i64` | remove | 33.8 | 24.3 | 1.39x |
| 1K `String` | insert into a new map | 88.8 | 47.0 | 1.89x |
| 1K `String` | lookup, hit | 19.6 | 20.7 | 0.95x |
| 1K `String` | lookup, miss | 15.8 | 15.3 | 1.03x |
| 1K `String` | iterate | 3.2 | 0.9 | 3.39x |
| 1K `String` | remove | 24.4 | 25.5 | 0.96x |
| 100K `String` | insert into a new map | 94.9 | 52.9 | 1.79x |
| 100K `String` | lookup, hit | 34.9 | 34.6 | 1.01x |
| 100K `String` | lookup, miss | 37.0 | 31.8 | 1.16x |
| 100K `String` | iterate | 3.9 | 1.8 | 2.19x |
| 100K `String` | remove | 45.3 | 46.5 | 0.97x |
| 1M `String` | insert into a new map | 241.2 | 164.9 | 1.46x |
| 1M `String` | lookup, hit | 105.9 | 90.1 | 1.18x |
| 1M `String` | lookup, miss | 42.8 | 44.5 | 0.96x |
| 1M `String` | iterate | 10.5 | 6.7 | 1.58x |
| 1M `String` | remove | 106.6 | 103.0 | 1.04x |

*The DoS-resistant defaults* (`Map.new()` against `HashMap::new()`: SipHash-1-3 under a random key on both sides):

| keys | operation | Codira ns/op | Rust ns/op | Codira / Rust |
|---|---|---:|---:|---:|
| 1K `i64` | insert into a new map | 91.0 | 39.5 | 2.30x |
| 1K `i64` | lookup, hit | 11.1 | 10.8 | 1.02x |
| 1K `i64` | lookup, miss | 9.9 | 12.2 | 0.81x |
| 1K `i64` | iterate | 3.6 | 0.9 | 3.79x |
| 1K `i64` | remove | 14.6 | 14.5 | 1.01x |
| 100K `i64` | insert into a new map | 42.2 | 35.6 | 1.18x |
| 100K `i64` | lookup, hit | 20.1 | 19.7 | 1.02x |
| 100K `i64` | lookup, miss | 20.9 | 18.2 | 1.15x |
| 100K `i64` | iterate | 3.7 | 1.6 | 2.35x |
| 100K `i64` | remove | 28.6 | 29.4 | 0.97x |
| 1M `i64` | insert into a new map | 96.0 | 97.3 | 0.99x |
| 1M `i64` | lookup, hit | 73.3 | 67.5 | 1.09x |
| 1M `i64` | lookup, miss | 21.6 | 23.5 | 0.92x |
| 1M `i64` | iterate | 7.2 | 4.8 | 1.52x |
| 1M `i64` | remove | 68.2 | 88.3 | 0.77x |
| 1K `String` | insert into a new map | 93.8 | 60.6 | 1.55x |
| 1K `String` | lookup, hit | 18.2 | 23.8 | 0.77x |
| 1K `String` | lookup, miss | 14.8 | 21.0 | 0.71x |
| 1K `String` | iterate | 3.4 | 1.2 | 2.89x |
| 1K `String` | remove | 21.4 | 26.5 | 0.81x |
| 100K `String` | insert into a new map | 91.0 | 64.8 | 1.40x |
| 100K `String` | lookup, hit | 36.3 | 38.6 | 0.94x |
| 100K `String` | lookup, miss | 34.0 | 32.4 | 1.05x |
| 100K `String` | iterate | 4.1 | 1.8 | 2.34x |
| 100K `String` | remove | 39.3 | 46.7 | 0.84x |
| 1M `String` | insert into a new map | 293.6 | 263.4 | 1.11x |
| 1M `String` | lookup, hit | 108.6 | 124.6 | 0.87x |
| 1M `String` | lookup, miss | 52.0 | 54.6 | 0.95x |
| 1M `String` | iterate | 11.1 | 6.9 | 1.61x |
| 1M `String` | remove | 115.4 | 136.4 | 0.85x |

*Small maps* (five `i64` keys):

| work | hasher | Codira ns | Rust ns |
|---|---|---:|---:|
| build a map of 5 (per insert, map creation included) | default (SipHash) | 42.5 | 28.5 |
| look up in it (half hits) | default (SipHash) | 3.7 | 12.2 |
| build a map of 5 (per insert, map creation included) | same unkeyed hash | 60.4 | 22.8 |
| look up in it (half hits) | same unkeyed hash | 3.8 | 3.8 |

- **Lookups and removals** run at Rust's speed or faster with the default
  hasher (0.71x to 1.15x Rust's time across sizes and key types), and
  within 0.95x to 1.54x with the same unkeyed hash. What is left: a probe
  tests 8 control bytes per word where hashbrown tests 16 with SSE2, and
  the table is reached through the map object and two arrays (each a
  handle to its payload) where Rust's sits in registers.
- **Insertion into a new map** takes 1.0x to 3.5x Rust's time, the most
  for small maps grown from nothing: a map of 1,000 entries grows 8 times
  and every table is a new GC allocation, where `malloc` reuses the last
  one. Measured on the same build, outside the benchmark: 1,000 insertions
  into a fresh unkeyed map take 29 ns each, 12 ns with the table reserved
  first (`with_capacity`), and 15 ns per insert-and-remove into an
  existing table.
- **Iteration** is 1.5x to 3.8x slower: `next()` of an iterator object,
  whose state lives in its heap object, per entry; Rust's iterator stays
  in registers. `keys()`, `values()` and `entries()` copy at memory speed.
- **Small maps**: looking up in a map of five integer keys is 3.3 times as
  fast as Rust's with SipHash (the keys are compared, never hashed) and
  the same with the unkeyed hash. Building one costs 1.5x to 2.6x Rust's:
  the map object, its control word and its slot array are three GC
  allocations, and a keyed map draws its key from the runtime.
- Against the map this replaced (open addressing, a state byte, a key and
  a value in three bounds-checked arrays; measured on this machine as
  106.2 ms to insert 1M random `i64` keys and 48.1 ms to look them up,
  1.6x Rust's with the same hash): 65.7 and 27.5 ms here, 1.03x and 1.08x
  Rust's, with every hash inlined (`mix64` used to be a call through the
  dispatch table) and, by default, DoS-resistant.

- Pushing, sorting and the closure-based reductions run at Rust's speed;
  the sums are bound by memory bandwidth (80 MB). The `fold` with a closure
  literal compiles to the same vectorized loop as the `for` loop (checked
  on the IR in `closure_literals_are_compiled_in_place`,
  `crates/codira/tests/collections.rs`).
- The eager chain is about 5 times slower than Rust's because each stage
  builds its array (10M and then 10M elements allocated, zeroed and
  written); fused into one `fold` it is on par, and so is the lazy
  pipeline `xs.iter().map(..).filter(..).sum()` (see "Lazy pipelines").
- What growth cost existing loops: `for x in xs` now also tests the current
  length on every iteration. Measured against the old lowering written out
  by hand, a loop that only reads is unchanged (9.4 vs 9.4 ms over 10M
  elements: LLVM hoists the second length load), a loop storing to a class
  field is within noise (29.6 vs 29.3 ms), and a loop storing into another
  array is 6% slower (22.4 vs 21.2 ms) because the length is reloaded after
  each store; with `CODIRA_ARRAY_TBAA=1` that loop is unchanged (17.0 vs
  17.3 ms). Program: `examples/collections_loop_cost/src/mod.code`.

### Hashing: `std.hashlib.hash`

```codira
import std.hashlib.hash.{Hashable, HashKey, SipHasher13, siphash13_str}

let key = HashKey.random()                 // 128 bits, fresh on every call
let h = "ada".hash_keyed(key)             // SipHash-1-3 of the bytes
let same = siphash13_str(key, "ada") == h
let fast = "ada".hash_value()             // FNV-1a: unkeyed, not DoS-resistant
let mine = SipHasher13.new(key).write_u64(7).write_str("x").finish()
```

- **Two kinds of hash.** `hash_keyed(key)` is SipHash under a secret
  128-bit key: what `Map`, `Set` and `Counter` use by default, each under
  a key of its own. `hash_value()` is fixed: the MurmurHash3 finalizer for
  integers, FNV-1a for `str` and `String`; anyone who chooses the keys can
  make them collide, so it is for trusted data (`Map.unkeyed()`).
- **Why SipHash-1-3.** SipHash (Aumasson and Bernstein, "SipHash: a fast
  short-input PRF", INDOCRYPT 2012) was designed as the keyed hash for
  hash tables: fast on short inputs, and its outputs cannot be predicted by
  whoever does not hold the key, so colliding keys cannot be computed
  offline. SipHash-2-4 is the variant its authors claim to be a PRF
  (`siphash24_*` here); SipHash-1-3, with one compression and three
  finalization rounds, is the default table hash of Rust's `HashMap` and of
  Python since 3.11. It gives up part of the security margin for about
  twice the speed, which suits a hash table, where an attacker sees at most
  timing and iteration order, never a hash. The faster seeded hashes
  (aHash, wyhash and the like) have no published analysis under a secret
  key, and some of their kind (MurmurHash2 and 3, CityHash64) were shown by
  SipHash's authors to have multicollisions that work for every seed.
- **Keys.** `HashKey.random()` calls the runtime's entropy service: the
  process draws a 128-bit secret from the operating system once
  (`getrandom`), and key `n` is `(PRF(secret, 2n), PRF(secret, 2n + 1))`
  with SipHash-2-4 as the PRF, so each map's key is independent of every
  other's, and one map's iteration order tells nothing about another's
  (copying a map into another in iteration order does not cluster it, as it
  can with one shared hash). `HashKey.new(k0, k1)` is a fixed key, for
  tests and reproducible orders.
- **A type of your own** gets `hash_keyed` by default as SipHash of its
  `hash_value`, which is only as collision-resistant as `hash_value` is
  injective (a `combine_hash` of fields is not). Override it by writing the
  fields to a `SipHasher13` (`write_u64`, `write_i64`, `write_str`,
  `write_string`, `write_bytes`): SipHash-1-3 over an injective encoding
  (a string's bytes in words, padded, then its length), so `("ab", "c")`
  and `("a", "bc")` do not collide.
- **Speed.** An integer's keyed hash is one SipHash block and the length
  block, straight-line and inlined (the implementations are `@inline`, so a
  map's probe loop has no call); a string's is a loop over 8-byte words
  (`string_data` gives a `String`'s bytes without a call). Bytes are read
  as little-endian words, on the little-endian targets Codira supports.
- Tests: `std_siphash_matches_the_reference` (every length 0 to 40, random
  keys, through every entry point) against a reference implementation
  that `the_reference_siphash_is_siphash` checks against the paper's
  vector, Rust's `SipHasher` (2-4) and Rust's `DefaultHasher` (1-3);
  `keyed_maps_differ_between_maps_and_between_runs`;
  `codira_runtime`'s `native_random` unit tests (the entropy service and
  the key derivation against the authors' vector).

### Lazy pipelines: `xs.iter().map(f).filter(g).sum()`

```codira
class Countdown { n: i64 }
extend Countdown {
    func next(self) -> i64? {            // an iterator: `nil` when done
        if self.n == 0 { return nil }
        self.n -= 1
        self.n
    }
}

let xs: [i64] = [3, 1, 4, 1, 5, 9, 2, 6]
let ws: [f64] = [0.5, 1.5]
let total = xs.iter().map(func(x) { x * 2 }).filter(func(x) { x % 3 == 0 }).sum()
let hits = xs.iter().enumerate().filter(func(p) { p.0 % 2 == 0 }).count()   // usize
let dot = ws.iter().zip(ws).fold(0.0, func(acc, p) { acc + p.0 * p.1 })
let head = xs.iter().take_while(func(x) { x < 5 }).collect()               // [i64]
let squares = (0..10).map(func(i) { i * i }).collect()                     // a range is a source
let last_big = xs.iter().rev().find(func(x) { x > 4 })                     // i64?
for (i, x) in xs.iter().skip(2).enumerate() { }
let down = Countdown { n: 3 }
for x in down { }                                                          // 2, 1, 0
let c = Countdown { n: 5 }
println(c.iter().chain(0..2).sum())                                        // 4+3+2+1+0+0+1
```

- **Sources.** `xs.iter()` on an array; a range, directly (`(0..n).map(f)`)
  or as `(0..n).iter()`; `it.iter()` on an *iterator object* -- an object of
  a class with a method `next(self) -> T?` taking nothing else, which is
  called until it returns `nil` (`for x in it` loops over one directly);
  the pieces of a string, `s.lines()` and `s.split(sep)`, read as they are
  found with no array (`s.split(",").map(f).count()`, `s.lines().iter()..`,
  `for line in s.lines()`; see "Strings"). `iter()` on a pipeline changes
  nothing.
- **Adapters**, each returning a pipeline: `map(f)`, `filter(f)`,
  `take(n)`, `skip(n)` (`n: usize`), `take_while(f)`, `skip_while(f)`,
  `enumerate()` (`(usize, T)` pairs), `zip(ys)` (`(T, U)` pairs, ending with
  the shorter side; `ys` an array, a range or a bare `ys.iter()`),
  `chain(ys)` (these elements, then those of an array, a range or any other
  pipeline of the same element type) and `rev()`.
- **Terminals**, each running the pipeline: `sum()` (integers, wrapping;
  floats, added in order from `0.0`), `fold(init, f)`, `count()` (`usize`),
  `any(f)`, `all(f)`, `find(f)` (`T?`), `position(f)` (`usize?`), `min()`,
  `max()` (`T?`; integers, floats or `bool`, with the arrays' rules: a
  float `NaN` is skipped unless it comes first), `collect()` (a new `[T]`),
  and `for x in pipeline { .. }` (with `break`, `continue` and `return`).

#### The design, and why

A pipeline has the type `Iter[T]`, and that type has no run-time
representation: the compiler sees the whole chain, from its source to its
terminal or the `for` loop it feeds, and compiles it into **one loop**. Each
element is pushed through the stages in order; every closure literal is
generated in place (no function value, no call -- as for the eager array
methods), the state of `take`, `skip`, `enumerate` and `zip` is a counter
in a register, and nothing is allocated except the array `collect`
returns. This is a guarantee, not an optimization that inlining may or may
not achieve: it holds at every `-O` level, and `pipelines_compile_to_one_loop`
(`crates/codira/tests/pipelines.rs`) checks at `-O 2` that the loops of
`map`/`filter`/`sum`, `enumerate`/`filter`/`count`, a `zip`/`fold` dot
product, `take_while`, a range, `rev`/`position`, `skip`/`take`/`max` and a
`for` loop over `chain`/`enumerate` contain no call (other than LLVM
intrinsics), no allocation and no function value.

The price is that **a pipeline cannot leave the expression that builds
it**. Storing one in a variable (`let p = xs.iter().map(f)`), returning it,
passing it to a function, putting it in an array, a tuple or the branches
of an `if`, or leaving it unused (`xs.iter().map(f);` does nothing) is a
compile error that says to end it with a terminal or `for`, or to
`collect()` it. A real iterator object, the alternative, would need either
a generic `Iterator[T]` trait with trait objects (neither exists: traits are
not generic yet and there are no trait objects) or one class per adapter,
and every stage would be a call through a closure value the optimizer may
or may not inline -- the cost the eager methods were measured to avoid.
Iterator objects with `next` cover the other direction: a type can be
*iterated* (and be a source) without the compiler knowing its insides, at
the cost of a direct call of its `next` per element.

#### Semantics

Exactly Rust's iterator semantics, which the tests use as the oracle
(`random_pipelines_match_rust_iterators`: random pipelines over random
arrays and ranges, whose closures log every element they see, must print
the results *and the logs* that the same pipelines built from Rust's
iterators print):

- The source, then the arguments of each stage left to right, then the
  terminal's, are evaluated once, before the first element. A function value
  passed to a stage is evaluated then; a named function is called directly.
- Each element passes through all the stages before the next is pulled,
  so the closures' side effects interleave as in Rust:
  `xs.iter().map(f).filter(g)` calls `f(x0)`, `g(f(x0))`, `f(x1)`, ...
- Nothing is pulled after a stage has ended: `take(n)` checks its count
  before pulling, so the stages before it never see an `n + 1`-th element
  (and `take(0)` pulls nothing); `take_while` stops at the first rejected
  element, which it has seen; `zip` pulls its own side first and stops when
  the other side is exhausted; `any`, `all`, `find`, `position` and `break`
  stop the whole pipeline. `chain(ys)` starts on `ys` when its first part is
  exhausted (or ended by a `take` before the `chain`).
- An array source is read as `for x in xs` reads it: its length once, and
  the loop also stops at the current length, so a closure that pushes onto
  the array does not extend the pipeline and one that shortens it ends it
  there (memory-safe either way). `rev()` reads from the last element down
  and ends if the array has been shortened below the next index.
- A range counts like `for i in a..b`; an inclusive range ending at its
  type's maximum stops instead of wrapping (`(0u8..=255u8).count()` is
  256), and so does a reversed one starting at the minimum.
- Closures capture by value as everywhere, but a closure compiled in place
  reads the variable when each element reaches it. A `for` body therefore
  may not assign a variable that a stage's closure reads (compile error),
  so the difference never shows.
- `sum()` of floats starts from `0.0`, like the arrays' `sum()`; Rust's
  starts from `-0.0`, so an empty sum (or a sum of `-0.0`s) is `0.0` here
  and `-0.0` in Rust. Everything else agrees bit for bit.
- The eager array methods are unchanged: `xs.map(f).filter(g)` still builds
  both arrays and runs `f` on every element first.

`rev()` needs a source it can walk backwards: an array or a range, with
only `map` and `filter` (and `rev`) before it -- `xs.iter().filter(f).rev()`,
`(0..n).map(f).rev()`. After `take`, `skip`, `enumerate`, `zip`, `chain` or
on an iterator object it is an error (Rust allows some of these through
`ExactSizeIterator`/`DoubleEndedIterator`; here, `collect()` first). `zip`
takes an array, a range or a bare `ys.iter()`, read by position; to zip with
a pipeline that has stages, `collect()` it first.

A value struct with `next` is refused ("`next(self)` would get a copy and
could never advance"), as are a `next` with other parameters or one not
returning `T?`. A class can be generic (`class Cycle[T]` with `next(self) ->
T?`). A class's own `iter` method, if it declares one, is called instead.

#### Effects

A pipeline over an array or a range with closure literals and a terminal
other than `collect` neither allocates nor panics nor blocks, and is
bounded by its source: a `@noalloc @nopanic` (or `@realtime`) function may
use it (`effects_of_pipelines`, `crates/codira_hir/src/builtin/pipeline.rs`).
A closure adds what its body does. `collect` allocates its array (with the
exact length when it is known: no `filter`, `take_while`, `skip_while` or
iterator object in the pipeline; otherwise it grows as `push` does). A
function value passed to a stage counts as unknown (every effect). A
pipeline or `for` loop over an iterator object adds what its `next` does,
and is unbounded for `@realtime`.

#### Measured

x86-64 VM, Intel Xeon @ 2.10 GHz, 4 cores (shared: load average about 2
during the runs); Codira at `-O 2` with a release build of the compiler and
runtime, Rust 1.97 at `-C opt-level=3`; median of 5 alternating runs,
milliseconds, 10M elements. The programs are
`examples/pipelines_bench/src/mod.code` and
`examples/pipelines_bench/pipelines_rust.rs`; both print the same checksums.

| work | Codira | Rust |
|---|---:|---:|
| `xs.iter().map(x * 2).filter(x % 3 == 0).sum()` | 15.0 | 16.2 |
| the same, eager: `xs.map(..).filter(..).sum()` | 76.6 | -- |
| the same as one `fold` | 15.1 | -- |
| `rs.iter().enumerate().filter((i + x) % 5 == 0).count()`, random `rs` | 15.5 | 15.3 |
| dot product: `fs.iter().zip(gs).fold(0.0, acc + a * b)` | 16.5 | 16.4 |
| `xs.iter().take_while(x < n - 1000).sum()` | 13.5 | 14.0 |
| `xs.iter().map(x * 3 + 1).collect()` | 42.6 | 41.2 |
| `rs.iter().filter(x % 3 == 0).collect()` (a third kept) | 58.1 | 55.9 |
| `(0..n).filter(i % 3 == 0).map(i ^ (i >> 3)).sum()` | 5.3 | 6.6 |
| `xs.iter().chain(rs).filter(x % 7 == 1).count()` (20M) | 36.4 | 36.0 |

- The chain that took 78.9 ms eager (76.6 here) is now at Rust's speed,
  and as fast as the hand-fused `fold`. The other reductions are within
  noise of Rust (runs vary by 1-2 ms on this machine); the range pipeline is
  faster, from a different loop LLVM made of it.
- `collect` is 3-4% slower: both sides are dominated by writing 80 MB (the
  mapped array) or growing the filtered one, and Codira's array comes zeroed
  from the GC heap. With the exact length known (`map`), Codira allocates the
  array once and its loop vectorizes; a filtered `collect` grows by doubling
  like `push`.
- A pipeline whose closures only compute (no call, assignment, allocation
  or string operation) reads its arrays like a Rust slice iterator: length
  and element buffer once. When a closure could change an array, the loop
  re-checks the current length per element, as `for x in xs` does (see
  "What growth cost existing loops" above for that cost).

#### Back ends and what is not done

- `codira run --jit` (Cranelift) does not compile pipelines; it says so
  ("does not compile a lazy pipeline") and the program runs on LLVM.
- No `Iter[T]` values: no storing, returning or passing a pipeline, no
  functions generic over iterators, no `Iterator[T]` trait (traits are not
  generic yet). A function that wants to hand out a sequence returns an
  array or an iterator object.
- Not provided: `flat_map`, `step_by`, `peekable`, `last`, `nth`,
  `sum`/`min`/`max` with a key or a comparator, `rev` after the adapters
  above, `zip` with a staged pipeline, `rev()` over the pieces of a
  string, other `str`/`String` sources (bytes or characters).
- A `for` loop over a pipeline with `chain` compiles its body once per
  part of the chain (two copies for one `chain`), like the stages after the
  `chain`: code size, not time.

### Arrays: layout, growth and the host

An array value is a handle to a GC object whose first word points at the
payload -- length, capacity, then the elements (references for class
elements). Growing moves the payload to a buffer at least twice as big
(the runtime's `codira.array_grow`, through the GC heap: a safepoint that
collects at the heap limit like an allocation) and frees an old out-of-line
buffer; the handle, so every reference to the array, stays the same.

- **Why `[T]` and not a new `List[T]`:** the layout already had a capacity
  and the handle indirection hot reload needs, so growth only replaces the
  payload pointer. `ArrayRef` in the host API reads the same layout and is
  unchanged (`len()`, and `capacity()`, which may now exceed it); the
  runtime type info is unchanged; there is one array type to learn.
- **Safety under growth:** generated code never keeps an element address
  across anything that can grow the array -- every access reloads the
  payload pointer after the last user code it depends on -- and the
  collector, heap snapshots and hot-reload migration all stop at the
  length (slots past it are never read). A host holding an `ArrayRef`
  iterator must not call Codira code that grows that array while iterating.
- **Hot reload, heap snapshots:** a grown array migrates and saves like any
  array; a migration or a restore gives it a capacity equal to its length.
- **Alias classes** (`CODIRA_ARRAY_TBAA=1`, see
  `crates/codira_codegen/src/ir/array_tbaa.rs`) stay sound with growth and
  are not made more conservative: lengths are now also written by `push`,
  `pop`, `insert`, `remove`, `clear` and `append`, but with the length tag,
  into the payload header; element accesses are still tagged only below the
  capacity, and the handle is still written only by runtime calls, after
  which LLVM reloads it. The capacity is loaded with the length tag (it is
  the payload header's other word).

### Strings: `str` and `String`

```codira
let lit = "héllo"                     // str: static bytes, allocates nothing
let s = lit.to_string()               // String: owned, on the GC heap
let t: String = "converted"           // a str converts where a String is expected
var log = "".to_string()
for i in 0..3 { log += f"[{i}]" }     // "[0][1][2]", appended in place
let parts = "a,b,,c".split(",")       // [String]: ["a", "b", "", "c"]
if let n = parts[0].parse_i64() { println(n) } else { println("not a number") }
println(s.len(), " bytes, ", s.char_count(), " chars")   // 6 bytes, 5 chars
println(s.slice(0, 3))                // "hé" (byte range; panics off a char boundary)
```

There are two string types:

- **`str`** is a literal: an address and a byte length of static bytes in
  the program image. It allocates nothing, ever. It has `len()`, can be
  printed, compared (`==`, `<`) and passed to the runtime with
  `str_data`/`str_len`; every other method first copies it into a
  `String`.
- **`String`** is the owned string, always valid UTF-8. It is the one string
  type a program builds, stores, returns and passes around; there is no
  separate borrowed type and no lifetime to write. The intrinsic
  `string_data(s: String) -> usize` (declared in `extern
  "codira-intrinsic"`) is the address of a non-empty `String`'s first byte,
  for reading its `s.len()` bytes with the `load_*` intrinsics, as
  `std.hashlib.hash` and `std.cmp.cmp` do; an empty `String` may have no
  buffer, so check the length first.

#### The design, and why

`String` is known to the compiler and backed by the runtime, not written in
`std`: every program has it without an import, literals convert to it, it
has operators and interpolation, and its operations are direct calls into
Rust code in the runtime (validation, search, splitting and number parsing
are Rust's own `str` implementations).

A `String` value is three words: a reference to a buffer on the GC heap
(an ordinary `[u8]` array object), the offset of the string's first byte in
it, and its length. That gives one type the roles Rust splits between
`String` and `&str`:

- **Values, not references.** The bytes a `String` covers are never
  written again. `let b = a` copies three words, and nothing done through
  `a` can change `b`, so a string is safe to keep as a map key, to store in
  a class, or to hand to another module.
- **Cheap views.** `slice`, `trim`, `trim_start`, `trim_end`, and the pieces
  of `split` and `lines` are new values over the same buffer: no bytes are
  copied, and the GC keeps the buffer alive while any view of it exists. Rust
  needs `&str` and a lifetime for this; here the collector does that job.
- **Appending in place.** `s += t` (and `s + t`) writes into the buffer only
  past the end of every existing value: when `s` ends exactly at the
  buffer's fill and there is room, the bytes go there and the fill rises;
  otherwise the bytes are copied into a new buffer, which doubles the
  capacity when `s` was at the fill (it is being built up) and is exact
  otherwise. A loop of `s += piece` is amortized O(1) per byte, as Rust's
  `push_str` is, with no builder type. Appending to a value that does not
  end at the fill (an older copy, a view in the middle) copies.

#### String Fabric: appends and formatting without a call

The AURORA memory architecture asks three things of string building, and
the compiler and runtime do them as follows (`codira_codegen`'s
`ir::body::fabric`, `codira_runtime`'s `string` module).

- **An append with room is a check, a copy and a store.** `s += t` is
  compiled inline:

  ```text
  payload = *s.buf                       ; fill, capacity, owner word
  if owner == thread pointer && fill == s.start + s.len && capacity - fill >= n:
      memcpy(payload + 16 + fill, t.data, n);  payload.fill = fill + n
  else:
      call codira.string_append          ; copies, or grows
  ```

  No allocation, no call through the dispatch table, no atomic
  read-modify-write; `string_fabric.rs`'s
  `the_common_append_path_calls_nothing` checks the `-O 2` IR.
- **Numbers go straight into the destination.** `s += n.to_string()` (and
  a `{n}` hole) writes the digits of an integer of at most 64 bits in
  place, with a digit writer generated into the module (two digits per
  step from a table); no temporary string exists. A float, a 128-bit
  integer, or an append without room calls the runtime, which writes the
  text into the destination's buffer (or the one it grows into).
- **Static format strings are compiled into a plan.** Interpolation
  already parses and checks every spec at compile time; an `f"..."`
  literal (and any `a + b + c` chain whose first piece makes new text) is
  then compiled as one plan: its pieces evaluated left to right, one buffer
  sized for the whole result (the literal pieces' lengths, the strings'
  lengths, an upper bound per number), and one append per piece. A literal
  piece is an inline copy; a decimal integer with a constant width and an
  ASCII fill (`{n:08}`, `{n:>6}`, `{n:*^9}`) is written inline, padding
  and sign included; any other spec is one call of
  `codira.string_append_format_int`, `_float` or `_text` with the spec as
  constant arguments, which writes into the destination; `{x:?}` is the
  type's generated debug function writing into it. The template text is
  not in the program, and nothing is parsed, boxed or collected into an
  argument array at run time (`a_static_format_plan_parses_nothing_at_run_time`).
  `s += f"..."` appends the pieces to `s` itself. `std.format.format`
  (a template known only at run time) stays the general path.
- **A plan usually allocates nothing.** A function keeps the result of its
  last plan in a stack slot, and the next plan starts as the empty string
  at that result's end, in the same buffer, when the buffer is this
  thread's, the result still ends at its fill and the room is there;
  otherwise it allocates a buffer of at least 256 bytes. The earlier
  results lie below the fill and are never written again. A loop of
  `f"{n:08}"` or `n.to_string()` allocates once per few dozen results.

**Who may raise the fill, and why no atomic is needed.** Two values on
one buffer can both end at its fill (`let b = a`); if both were appended
to at the same time, both could write the same bytes. Within one thread
that cannot happen: appends run one after the other, and the second sees
the fill the first raised and copies. So every buffer carries, in its
first eight bytes (the *owner word*; a string's bytes start after it), the
identity of the one thread that may raise its fill: the thread pointer of
the thread that allocated it (`fs:0` on x86-64 Linux, `TPIDR_EL0` on
AArch64 Linux, `llvm.thread.pointer` in compiled code). That thread
appends in place with plain loads and stores; **any other thread
appending to a value on the buffer copies**, so it never writes the
buffer at all. The argument that this is sound:

1. The owner word is written before the buffer is reachable from anything
   and never changes, so every thread reads the same owner.
2. No two live threads have the same thread pointer, so at most one thread
   passes the check: the fill, and the bytes above it, are written by that
   thread alone. Other threads read only the bytes of values they were
   given, which lie below the fill and are never written again.
3. A thread pointer is reused only after its thread has exited, and the C
   library hands the memory to a new thread under a lock, after the old one
   is gone, so the new owner sees everything the old one wrote.
4. A value reaches another thread only through something that
   synchronises: Codira tasks share nothing but channels of scalars, so in
   practice the host (`Runtime::invoke` from two threads with one
   `StringRef`, or a class instance both reach).
5. A buffer whose owner word is stale -- restored from a heap snapshot of
   another process -- is still matched by at most one thread, which is all
   the argument needs.

Where the thread pointer is not known (other targets) buffers are marked
*shared* (owner word zero) and every thread claims the fill with a
compare-and-swap, as before; compiled code there makes the same inline
check with a `cmpxchg`. A thread that cannot prove ownership takes the
safe path: it copies. Reuse never shows: no `String` value changes after it
is made, which the runtime's property tests check under random appends,
aliases and views and with two threads appending at once to values on one
buffer (`no_value_changes_under_random_appends_and_aliases`,
`two_threads_appending_on_one_buffer_change_no_value`).

**No builder type.** A builder would compile to the same check, copy and
store as `+=` does now, so it would not be faster; there is no
`StringBuilder`.

**No small-string optimisation.** Measured before the plans reused their
buffers: a small `String` cost one GC allocation of about 65-70 ns on the
benchmark machine (about 420 instructions, 265 of them in the collector's
allocator), which was most of `f"{n:08}"` (85 ns) and `n.to_string()`
(80 ns). With plans continuing in their function's buffer those are 16 ns
and 12 ns, so an inline representation for short strings -- a second
layout every string operation would have to test for -- would now save
little on these workloads, and is not done.

Considered and not chosen: a *mutable* growable string shared by reference
(as arrays and classes are) needs no offset word, but `let b = a;
a.push_str(..)` would change `b`, which is a classic source of bugs with
string keys; an *immutable* string with a separate builder type is one more
concept, and without the in-place append every `+=` copies (quadratic
loops); a `std` type over a byte array would need an import in every
program and could not have literals convert to it.

What this costs: a `String` is 24 bytes rather than 8; a small view keeps
its whole buffer alive (as a Go slice or an old Java substring does: copy
it, `s.slice(a, b).to_upper()` or `f"{v}"`, to let the buffer go); and a
call to a function of *another module* boxes a `String` argument or result
on the GC heap, as it boxes a value struct.

#### Operations

Construction and conversion:

- `x.to_string()` on a `str`, any integer, float or `bool` (formatted as
  `print` writes them: `1.0`, `-0.0`, `NaN`), or a `String` (itself, no
  copy).
- A `str` used where a `String` is expected -- an argument, a `let` with a
  type, a field, an array element, a return value, an assignment to a
  `String` -- converts implicitly (a copy).
- `string_from_utf8(bytes: [u8]) -> String?`: a copy of the bytes if they
  are valid UTF-8, else `nil`.
- Interpolation: `f"x = {x}, p.y = {p.y}, {self.name}"` is a `String`. A
  hole is a variable optionally followed by fields, as in Rust's
  `format!("{x}")` -- no calls, no operators -- and is converted with
  `to_string()`, so it type-checks like that call (a hole of a type without
  `to_string` is an error at the literal; a type's own `to_string(self) ->
  String` method counts). `{{` and `}}` are literal braces. The literal is
  compiled as a format plan (see "String Fabric" above): one buffer, one
  append per piece. A hole may carry a format spec, `{x:>8.3}`, and `{x:?}`
  writes any value: see "Format specs and the debug form" below.

Operators: `a + b` (either side a `String` or a `str`; the result is a new
`String`), `s += t` (`s` a `String` place, `t` a `String` or `str`), and
`==`, `!=`, `<`, `<=`, `>`, `>=` between any mix of `String` and `str`, by
bytes -- which for UTF-8 is code-point order. `print`/`println` print a
`String` like a `str`.

Methods (text arguments take a `String` or a `str`, so a literal argument
costs nothing):

| method | result | notes |
|---|---|---|
| `len()`, `is_empty()` | `usize`, `bool` | bytes; inline |
| `char_count()` | `usize` | code points (one pass) |
| `chars()` | `[u32]` | the code points, a new array |
| `bytes()` | `[u8]` | a **copy**: changing it changes no string |
| `byte_at(i)` | `u8` | bounds-checked like `a[i]` |
| `find(p)`, `rfind(p)` | `usize?` | byte offset of the first / last match |
| `contains(p)`, `starts_with(p)`, `ends_with(p)` | `bool` | |
| `split(sep)` | `[String]` | Rust's `str::split`; views; an empty `sep` panics; lazy where no array is needed (below) |
| `lines()` | `[String]` | Rust's `str::lines` (`\n`, `\r\n`, no empty last line); views; lazy likewise |
| `trim()`, `trim_start()`, `trim_end()` | `String` | Unicode `White_Space`, as Rust; views |
| `slice(start, end)` | `String` | the byte range; a view |
| `to_upper()`, `to_lower()` | `String` | ASCII letters only (see below) |
| `replace(from, to)` | `String` | Rust's `str::replace` |
| `repeat(n)` | `String` | a negative `n` repeats nothing |
| `parse_i64()`, `parse_f64()` | `i64?`, `f64?` | Rust's `from_str`: no white space, `+`/`-` sign, `inf`, `NaN` |
| `hash()` | `u64` | FNV-1a 64 of the bytes |
| `to_string()` | `String` | itself |

**`lines()` and `split()` without the array.** Where the `[String]` would
be a temporary nothing else sees, the pieces are read as they are found,
in one loop, with no array: `for line in text.lines() { .. }`, `for p in
text.split(",") { .. }`, `text.lines().iter()..` (any pipeline), and a
chain on `text.split(sep)` or `text.lines()` that uses a pipeline
operation arrays do not have -- `text.split(",").map(f).count()`,
`.enumerate()`, `.take(n)`, `.find(f)`; see "Lazy pipelines". The string
and the separator are evaluated once, before the first piece, as the call
would be; a line break or a one-byte separator (always ASCII in UTF-8) is
found with `memchr`, a longer separator with `str::find`; an empty
separator panics before the first piece, with `split`'s message. A chain
of only methods arrays have (`text.split(",").map(f)`,
`.filter(g).sum()`) keeps the eager arrays, so no program changes meaning,
and `let parts = text.split(",")` is an array as always. The eager
`split` on one byte and `lines()` scan with `memchr` too and fill the
array with no list in between.

`std.collections.string` adds what is written in Codira on top: `join`,
`insert`, `remove`, `reverse`, `count`, `compare`, `split_whitespace`,
`string_from_code_point(s)`. `std.format.format` has `format(template,
[args])`, `format_int(value, base)`, `hex`/`bin`/`oct` and padding.

#### Format specs and the debug form

```codira
let x = 3.14159
let n = 42
let w = 9
let name = "héllo"
f"[{x:>10.3}]"          // "[     3.142]"
f"[{n:08}] [{n:+}]"      // "[00000042] [+42]"
f"[{n:*^9}] [{n:#x}]"    // "[***42****] [0x2a]"
f"[{x:>{w}.2}]"         // "[     3.14]": a width (or precision) from a variable
f"{x:e} {n:b}"           // "3.14159e0 101010"
f"[{name:-<8.3}]"        // "[hél-----]": at most 3 chars, left-aligned in 8

struct Point { x: i64, y: f64 }
let p = Point { x: 1, y: 2.0 }
f"{p:?}"                 // "Point { x: 1, y: 2.0 }"
f"{p:#?}"                // the same, one field per line (Rust's `{:#?}`)
p.debug_string()         // what `{p:?}` writes
```

A hole may carry a **format spec** after a colon, in Rust's grammar
(`std::fmt`), with Python's braces for a width or precision held in a
variable:

```text
spec      := [[fill] align] [sign] ['#'] ['0'] [width] ['.' precision] [type]
fill      := any character but '{' and '}'
align     := '<' | '^' | '>'
sign      := '+'
width     := integer | '{' name '}'
precision := integer | '{' name '}'
type      := '' | '?' | 'x' | 'X' | 'o' | 'b' | 'e' | 'E'
```

- What each part means is what it means in Rust: `<`, `^`, `>` align left,
  centred (an odd fill character goes right) or right within `width`
  characters, padding with `fill` (a space unless given); numbers align
  right and everything else left by default. `+` prints the sign of a
  non-negative number. `0` pads a number with zeros between its sign (and
  prefix) and its digits, whatever the fill and alignment say. `#` adds
  `0x`, `0o` or `0b` to `x`/`X`, `o`, `b` (`0x` for `X` too, as in Rust),
  and makes `?` the pretty form `#?`. A float's precision is its digits
  after the point, rounded as Rust's `format!` rounds (to nearest, ties to
  even, on the exact binary value); a string's is the number of characters
  kept. `e`/`E` is the exponent form of a float or an integer, with a
  precision if given. The radix forms of a negative integer print its
  type's bits (an `i8` holding -1 is `ff` under `x`), as Rust does.
- Without a precision a float has the digits `to_string()` gives it
  (`1.0`, `1e21`, shortest round trip: Rust's `{:?}`), so `{x:>8}` pads what
  `{x}` writes.
- A width or precision `{w}` names a variable, then fields (`{self.width}`),
  of any integer type; a negative value counts as zero.
- **Checked at compile time.** A spec that does not parse, or does not
  apply to the hole's type, is an error at the literal that names the spec
  and the type: ``the format spec `.2` does not apply to a `i64`: integers
  have no precision``, ``invalid format spec: `>8z`: unknown format type
  `z` ``. Integers take the radix and exponent forms but no precision;
  floats take a precision and the exponent forms but no radix; strings and
  `bool` take a fill, an alignment and a width (strings a precision too)
  but no `+`, `0` or `#`; any other type takes only `?`. A value of a
  generic type `T` takes only `?` (what else applies depends on `T`).
- **The debug form, `{x:?}`, `{x:#?}` and `x.debug_string()`, exists for
  every type** with nothing to write: numbers as `to_string()` writes them,
  `bool`, strings quoted and escaped (`"tab\there \"q\""`: `\n`, `\r`,
  `\t`, `\0`, `\\`, `\"`, other ASCII control characters as `\xNN`, the
  rest as it is, so the text reads back as a Codira literal), `nil` or the
  value of an optional, arrays `[1, 2]`, tuples `(1, true)` and `(7,)`,
  structs and classes `Point { x: 1, y: 2.0 }` (a tuple struct `Pair(1,
  2)`, a unit struct by its name), enums `Shape.Circle(2.0)` and
  `Shape.Dot`, generic instantiations by their name (`Box { value: 3 }`),
  a function value `<func>`, a channel `<channel>`. The text is the Codira
  expression that builds the value. For structs, arrays and tuples of
  numbers, `bool`s and strings that need no `\xNN` escape it is exactly
  what Rust's `#[derive(Debug)]` writes, compact and pretty (the tests
  compare the two); enums are written qualified, as Codira writes them
  (Rust writes `Circle(2.0)`), an optional as its value or `nil` (Rust
  writes `Some(..)`/`None`), and a string's control characters as `\xNN`
  (Rust writes `\u{1}`). A user method called `debug_string` takes
  precedence over the builtin one.
- `{x:>20?}` pads the whole debug text (Rust pads each leaf field instead);
  the debug form takes a fill, an alignment and a width, and `#`, but no
  `+`, `0` or precision.
- **Reference cycles**: a class instance met again while its own fields are
  being written prints as `Node { .. }` instead of looping. An instance
  reached twice without a cycle (shared, not nested) is written both times.
  The runtime keeps the instances in progress on a per-thread stack.
- **How it is built**: the compiler generates one debug function per type
  in the module that needs it (`codira.debug:<type>`, private), from the
  type alone, and a type that contains itself (through an array, a class
  or an optional) gets a function that calls itself. Nothing at run time
  inspects a type; the debug function appends straight into the string
  being built (numbers with the inline digit writer). A spec hole is a
  piece of the literal's format plan (see "String Fabric"): a decimal
  integer with a constant width and an ASCII fill is padded inline; any
  other is one call to `codira.string_append_format_int`, `_float` or
  `_text`, chosen by the type, with the spec passed as four constant
  scalars; Rust's formatter makes the digits of floats and radix forms,
  and the runtime pads them on the stack and appends them to the
  destination.
- `x.type_name()` is the name of `x`'s type as a `str` constant (`FileError`,
  `Box[i64]`, `[u8]`), chosen per instantiation; `std.error.error` records
  it.
- Tests: `crates/codira/tests/formatting.rs` prints every spec for seeded
  random `i64`, `i8`, `u64`, `f64`, `f32`, `String` and `bool` values with
  random widths and precisions and compares each line with Rust's
  `format!` of the same spec; the runtime's own unit tests do the same on
  thousands of values. Golden tests cover the debug form of nested types,
  cycles and a type from another module; `codira_hir`'s
  `builtin::format` and `ty::infer::format` tests cover the grammar and the
  misuse errors.

#### Safety

- **Always valid UTF-8.** Literals are checked by the compiler;
  `string_from_utf8` validates; slicing checks character boundaries; every
  other operation (concatenation, case mapping, replacing, splitting,
  trimming, number formatting) maps valid UTF-8 to valid UTF-8. There is no
  unchecked constructor.
- **Every index is checked.** `slice(start, end)` panics unless `0 <= start
  <= end <= len()` and both are character boundaries; `byte_at(i)` panics
  out of range. The report names the call: `panic at mod.code:4:13: byte
  index 2 is not a char boundary; it is inside 'é' (bytes 1..3)`, `string
  slice -1..2 is out of range for a string of length 3`, `index out of
  bounds: the length is 3 but the index is 3`; exit status 101.
- **Bytes are a copy.** `bytes()` returns a new `[u8]`; `string_from_utf8`
  copies its input. Nothing gives a program a mutable view of a string's
  buffer.
- The UTF-8 validation and the slice check are tested against Rust's
  `std::str` with seeded random byte arrays and ranges (property tests in
  `crates/codira_runtime/src/string.rs`), and so are search, split, lines,
  trim, case mapping, replace, repeat, parse and format; a random tree of
  appends checks that no existing value ever changes.

#### Unicode: what is and is not handled

A "char" is a Unicode scalar value (a code point, `u32`), as in Rust: there
is no `char` type and no grapheme clusters (`"é"` written as `e` plus a
combining accent is two chars). `to_upper`/`to_lower` change ASCII letters
only (`"Wörld".to_upper()` is `"WöRLD"`); full Unicode case mapping, which
can change the length (`ß` to `SS`), is not done. Ordering is by code point,
not by any locale's collation. `trim` uses Unicode's `White_Space`
property, as Rust's does.

`hash()` (and `std.hashlib.string_hash`) is FNV-1a: fast and deterministic,
and **not resistant to hash flooding** -- an attacker who chooses the keys
of a table hashed with it can make them all collide.

#### Effects, the GC, hot reload and the host

- **Allocates** (so rejected by `@noalloc`): `to_string()` on anything but a
  `String`, the implicit `str`-to-`String` conversion, `+` and `+=`
  (counted as allocating even when the append fits: whether it fits is a
  property of the buffer at run time, which the compiler does not track,
  so the inline fast path that allocates nothing is not proved and the
  effect stays), `chars`, `bytes`,
  `split`, `lines`, `to_upper`/`to_lower`, `replace`, `repeat`,
  `string_from_utf8`, interpolation (a hole with a format spec and the
  debug form included), `debug_string()`, and every method called on a
  `str`.
  `len`, `is_empty`, comparisons, `find`/`rfind`/`contains`/
  `starts_with`/`ends_with`, `char_count`, `hash`, `parse_*` and the views
  (`trim*`, `slice`) allocate nothing. **Panics** (`@nopanic`): `slice` and
  `byte_at`, besides every allocation.
- The buffer is a GC array, so the collector traces it, conservatively from
  the stack (a view keeps its buffer alive). `Runtime::save_heap` /
  `restore_heap` carry strings with the objects that hold them, and a hot
  reload keeps a `String` field's value across layout changes like any
  value-struct field. Tests: `crates/codira_runtime/tests/strings.rs`
  (collections under a 2 MB heap cap while strings are built, sliced and
  compared; a snapshot; a reload that reorders and adds fields) and
  `strings_host_gc.rs` (a host thread calls `collect_blocking` back to back,
  with no pause of its own, while strings are built; the collector's
  fairness rule keeps the rounds about as fast as without it, see "Memory:
  heap limit, pacing, `--runtime-stats`").
- **Host API**: `codira_runtime::StringRef` is a `String` held by the host,
  like `StructRef` and `ArrayRef`: returned by `Runtime::invoke`, read from
  a field with `StructRef::get::<StringRef>("name")` or from a
  `[String]` with `ArrayRef<StringRef>`, made with `StringRef::new(&runtime,
  "text")` to pass as an argument, read with `as_str()`, and kept alive
  across calls with `root()` (`RootedString`). Unrooted, it is not
  protected from a collection.
- Strings cannot be sent between tasks (only scalars and channels cross).

#### Back ends

The LLVM back end compiles all of the above. The Cranelift back end
(`codira run --jit`) has no GC heap: a program that uses `String` runs on
LLVM instead, with a note saying so (`... a `String` (strings live on the GC
heap, which this backend does not reach)`).

#### Performance

Measured against Rust's `String` and `format!` on the same algorithms
(`benches/strings/`: `src/mod.code` and `strings.rs`;
`examples/format_bench/`: `src/mod.code` and `format_rust.rs`; each pair
prints the same check values). Medians of 11 alternating runs of each
program (format specs: 5 runs, each the median of 7 rounds of 1M
formats), on an Intel Xeon @ 2.10 GHz VM (4 vCPUs, Linux 6.18) shared with
other builds (load average 4 to 5 during the runs, so single runs vary by
up to 30%); Codira built with `codira build -O 3` and run by
`benches/strings/host` (the runtime in release mode), Rust 1.97.1 with
`rustc -C opt-level=3`.

| benchmark | before | Codira | Rust | Codira / Rust |
|---|---:|---:|---:|---:|
| 1M short appends (`s += "word "`) | 17.3 ms | 7.5 ms | 5.3 ms | 1.42x |
| 1M `s += i.to_string()` and `"\n"` | 112.6 ms | 12.0 ms | 26.1 ms | 0.46x |
| `lines()` of 10.5 MB into an array | 10.3 ms | 6.5 ms | 5.7 ms | 1.15x |
| `for line in text.lines()`, lazily | -- | 2.3 ms | 4.2 ms | 0.56x |
| `text.split(",").filter(..).count()`, lazily, 10.5 MB | -- | 16.3 ms | 22.1 ms | 0.74x |
| `find` in 10 MB | 1.8 ms | 1.4 ms | 1.7 ms | 0.80x |
| `parse_i64` of 1M fields | 11.5 ms | 6.8 ms | 6.5 ms | 1.05x |
| `f"{x:>10.3}"`, a float, 1M | 261.2 ms | 197.4 ms | 187.9 ms | 1.05x |
| `f"{n:08}"`, an integer, 1M | 102.7 ms | 18.6 ms | 43.4 ms | 0.43x |
| `f"{p:?}"`, `Point { x: i64, y: f64 }`, 1M | 553.1 ms | 164.7 ms | 273.6 ms | 0.60x |

What still loses, and why:

- **1M short appends, 1.4x.** Most of the gap is memory, not the append:
  100 strings of 10,000 appends each took 6.5 ms against Rust's 2.7 ms,
  and Rust's own time rose to 5.1 ms when its strings were leaked instead
  of freed -- which is what happens here, since nothing is collected while
  the benchmark runs and every new buffer is fresh memory. Growth adds to
  it: each doubling allocates a new buffer on the GC heap and copies into
  it, where Rust's `realloc` of a large block remaps pages without
  copying.
- **`lines()` into an array, 1.15x**: the `[String]` (24-byte views,
  4.9 MB) is allocated and filled on the GC heap after a counting pass.
- **A float with a precision, 1.05x**: the digits are Rust's own
  `{:.3}`; the rest is the call into the runtime and the padding.

Each of these used to be a call through the runtime's dispatch table per
append and, for numbers and specs, a new GC string per piece.

Not done yet: inline digits for a float (it is one runtime call per
hole or `to_string()`); a plan buffer shared across functions (each
function has its own); growing a buffer in place on the heap (`realloc`);
a `push_str` method (write `+=`); `match` on a `String`
scrutinee; iterating a string directly (`for c in s.chars()` iterates the
array of code points); a `char` type; Unicode case mapping and
normalization; expressions in holes (a hole is a name and fields); a
user-defined debug form (the generated one cannot be overridden except by
a method named `debug_string`, which `{x:?}` does not call); a precision
for the debug form (`{x:.2?}`); `x?`/`X?` (hex debug); a `Display` trait
(`to_string` is found by name); formatting in `codira run --jit` (such a
program runs on LLVM, as every `String` program does).

### JSON: `std.json.json`

```codira
import std.json.json.{Json, JsonObject, JsonError, parse, to_string, to_pretty_string}
import std.result.result.{Result}

func born(text: String) -> Result[i64, JsonError] {
    let doc = parse(text)?                         // a JsonError leaves with `?`
    if let year = doc.get("born") {
        if let n = year.as_i64() { return Result.Ok(n) }
    }
    Result.Ok(0)
}

let doc = parse("{\"name\": \"Ada\", \"tags\": [\"math\"], \"born\": 1815}").unwrap()
doc.get("name")!.as_str()!                         // "Ada"
doc.get("tags")!.at(0)!.as_str()!                  // "math"
doc.get("born")!.as_i64()!                         // 1815
let o = JsonObject.new().with("n", Json.Int(3)).with("x", Json.Float(0.5))
to_string(Json.Obj(o))                             // {"n":3,"x":0.5}
to_pretty_string(doc)                              // two-space indentation
```

**The value.** `Json` is an enum: `Null`, `Bool(bool)`, `Int(i64)`,
`Float(f64)`, `Str(String)`, `Arr([Json])`, `Obj(JsonObject)`.

- Numbers keep the distinction the text makes: a number with neither a
  fraction nor an exponent that fits an `i64` is an `Int`, exact; every
  other number is a `Float`, the correctly rounded nearest `f64` (also an
  integer beyond the `i64` range). `-0` is `Float(-0.0)`, so the sign
  survives. `as_i64` is `nil` on a `Float` (even `1.0`); `as_f64` converts
  an `Int`.
- **Objects keep insertion order**: a `JsonObject` holds its members in
  one array, in the order the text (or `set`) gave them, so printing a
  parsed document gives its members back in the same order and a round
  trip changes nothing a diff would see. Lookups hash the key (FNV-1a) and
  scan the hashes of objects of up to 8 members; larger objects keep an
  open-addressing index, so `get` is O(1) expected either way. Chosen over
  `Map` (which does not keep order) and over a sorted map (which reorders
  what a person wrote).
- Accessors return optionals: `get(key)`, `at(i)`, `as_bool`, `as_i64`,
  `as_f64`, `as_str`, `as_array` and `as_object` (the shared array or
  object, not a copy), `len` (elements or members, 0 otherwise), the
  `is_*` tests and `kind_name()`. Construction: the variants themselves,
  `Json.string(..)`, `Json.int(..)`, `Json.float(..)`, `Json.boolean(..)`,
  `Json.array(..)`, `Json.object()`, and `JsonObject.new()` with `set(key,
  value) -> Json?` (replaces in place, keeping the position), `with` (set
  and return the object), `remove` (keeps the order; O(n)), `get`,
  `contains_key`, `keys`, `values`, `key_at(i)`, `value_at(i)`.
- **Equality** (`equals(a, b)`, `a.equals(b)`) is structural: arrays
  element by element, objects as sets of members (order ignored), numbers
  by kind and value (`Int(1)` and `Float(1.0)` differ, as their texts
  do; IEEE for floats). `==` on `Json` is not available (an enum with
  non-scalar payloads).

**Parsing.** `parse(text: String) -> Result[Json, JsonError]`,
`parse_with(text, ParseOptions { max_depth, duplicate_keys })`, and
`parse_bytes(bytes: [u8])` / `parse_bytes_with` for bytes (`read_file`'s),
which reports bytes that are not UTF-8 as an `InvalidUtf8` error at the
first bad byte. The grammar is ECMA-404 / IETF RFC 8259 and nothing
more:

- No comments, trailing commas, single quotes, unquoted keys, leading
  zeros, `+` signs, `.5`, `NaN` or `Infinity`, no byte order mark. White
  space is space, tab, line feed and carriage return. Any value may be the
  top level; only white space may follow it.
- Strings: the escapes `\" \\ \/ \b \f \n \r \t \uXXXX` (either case of
  hex digits), with `\uD83D\uDE00` surrogate pairs decoded; a lone or
  mismatched surrogate, any other escape, and an unescaped control
  character (U+0000..U+001F) are errors. DEL and every other character
  are accepted as written.
- A number too large for an `f64` (`1e400`) is an error
  (`NumberOutOfRange`), not an infinity; one too small rounds to zero.
- **Duplicate keys** are an error (`DuplicateKey`, at the second key) by
  default: parsers that disagree about which value wins have been used to
  get data past a check. `DuplicateKeys.KeepLast` keeps the last value at
  the position of the first key, as JavaScript's `JSON.parse` and
  `serde_json` do.
- **Errors** are a `JsonError { kind, offset, line, column }`: the
  `JsonErrorKind` (19 reasons: `UnexpectedEnd`, `ExpectedValue`,
  `TrailingComma`, `InvalidNumber`, `LoneSurrogate`, `DepthLimit`, ...),
  the byte offset, the 1-based line (lines end at line feeds) and the
  1-based column in characters. `reason()` is the phrase, `message()`
  says `trailing comma at line 1, column 7 (byte 6)`.
- **Limits.** Arrays and objects nest at most `max_depth` deep (128 by
  default, as `serde_json`'s, which admits 127; `[[1]]` is 2 deep), and a
  larger `max_depth` is clamped to `MAX_DEPTH_LIMIT()`, 1000: a level costs
  up to about 900 bytes of stack in the parser, and parsing, printing and
  comparing 1000 levels was measured to run in a 1 MiB stack and not in
  768 KiB (the main thread and Codira's task threads have 8 MiB). The size
  of the input is not limited.
- **The parser never panics.** Every input gives a value or an error; the
  tests feed it random bytes and every prefix of valid documents. It is
  written in Codira, recursive descent over the `String`; the runtime
  parses floats (Rust's `f64::from_str`, correctly rounded).

**Printing.** `to_string(v)` is compact; `to_pretty_string(v)` puts each
element and member on a line of its own with two spaces per level and
`"key": value` (the layout of `serde_json`'s `to_string_pretty`);
`to_pretty_string_with(v, indent)` takes the width. Strings escape `"`,
`\` and the control characters (`\b \f \n \r \t`, else `\u00XX`) and
nothing else: every other character is written as itself, in UTF-8.
Floats are written in the shortest form that reads back as the same `f64`
(the runtime's `codira_json_format_f64`, on the `ryu` crate), always with
a `.` or an exponent (`1.0`, `1e300`, `-0.0`), so they read back as a
`Float`; a NaN or an infinity, which JSON cannot express, is written as
`null`. For every value without one, `parse(to_string(v))` equals `v`,
and so does the pretty text: the tests check this on random values, and
that both texts are byte for byte the ones `serde_json` prints.

**Safety, effects and memory.**

- Hostile input: nesting is bounded, the parser does not panic, and every
  index it uses is checked. Not bounded: the input size, the size of the
  tree (64 bytes per array element, 96 per object member, plus an object
  header per array and object) and the time for a large
  object whose keys an attacker chose to collide in FNV-1a, which is
  unseeded (**not DoS-resistant**, unlike `Map.new()`: see "Not done").
- A string without escapes is a view of the input text (no copy): the
  value keeps the whole input buffer alive. Copy what you keep
  (`f"{s}"`) to let the input go.
- Parsing and printing allocate (GC arrays and objects; the parser keeps
  each array's and object's elements on a stack of its own and allocates
  the result once, at its exact size); both may panic only on running out
  of memory. The printer and `equals` recurse: a value nested more deeply
  than any parse allows, built by a program, can overflow the stack.
- `codira run` collects as the program allocates (pacing) under a finite
  heap limit, half of the available memory by default (see "Memory: heap
  limit, pacing, `--runtime-stats`"). The measurements below predate that:
  they ran under a host with a 1 GiB limit and no pacing.
- Values are not sendable between tasks (they hold strings).

**Measured** against Rust's `serde_json::Value` (1.0.133) on the same
machine: an x86-64 VM, Intel Xeon @ 2.10 GHz, 4 vCPUs shared with other
builds (load average 3 to 4 during the runs), 16 GB, Linux 6.18, Rust
1.97.1. Codira: `codira build -O 3` with a release build of the
compiler, run by `examples/json_bench/host` (the runtime in release mode
and a 1 GiB heap limit, so that the collector runs). Rust: `cargo build
--release` with `serde_json`'s `preserve_order` (insertion order, as
here) and `float_roundtrip` (correctly rounded floats, as here). Each
program parses, prints and pretty-prints each input 11 times and reports
the median; the table is the median of 5 alternating runs of both, in
milliseconds. Both print the same check values (node count, string bytes,
sums of the integers and of the floats' bits, output lengths). The inputs
are written by `examples/json_bench/gen_inputs.py` (seeded): `twitter`, an
array of 20,000 records of a social network's API (16.9 MB); `canada`,
GeoJSON polygons of 300,000 17-digit coordinate pairs (11.8 MB); `deep`,
4,000 trees of objects and arrays 100 deep (7.0 MB; its pretty text is
149 MB of mostly indentation). `python3 examples/json_bench/bench.py
--codira <codira> --work <dir>` does all of it.

| input | phase | Codira | Rust | Codira / Rust |
|---|---|---:|---:|---:|
| twitter | parse | 81.8 | 118.0 | 0.69x |
| twitter | print | 56.2 | 51.4 | 1.09x |
| twitter | pretty | 78.3 | 63.4 | 1.24x |
| canada | parse | 51.5 | 47.2 | 1.09x |
| canada | print | 48.8 | 29.3 | 1.67x |
| canada | pretty | 71.7 | 46.0 | 1.56x |
| deep | parse | 63.9 | 88.0 | 0.73x |
| deep | print | 43.5 | 31.2 | 1.40x |
| deep | pretty | 343.0 | 293.6 | 1.17x |

- **Parsing** is within 10% of Rust or faster: what costs in Codira is
  allocation, and the parser allocates each array and object once (at its
  exact size, from its stack) and no string without escapes (views); the
  `IndexMap` behind `preserve_order` hashes every key with SipHash. With
  `serde_json`'s defaults (a `BTreeMap`, floats not correctly rounded)
  Rust parses `twitter` in 71.7 ms and `canada` in 36.5 ms against 86.8
  and 48.1 for Codira in that run -- and reads 126 of `canada`'s 600,000
  floats an ulp off, so the check values differ.
- **Printing is 1.1x to 1.7x slower.** Floats cost most (`canada`): each
  goes through the dispatch table to the runtime's `ryu` and is copied
  back byte by byte from a stack slot, where Rust calls `ryu` directly
  and copies its bytes at once. Every byte is a bounds-checked store into
  the 64 KiB chunk, and each chunk is copied three times more on its way
  into the `String` (a slice, `string_from_utf8`, the append). An earlier
  printer that appended to a `String` per token was 4x to 7x slower; one
  that grew a single byte array, 1.5x to 2.5x (the doublings and the copy
  at the end).
- The times include collections: the heap is capped at 1 GiB and each
  input is parsed 11 times, so where a collection falls depends on what
  was allocated before, and Codira's times vary more between sets of runs
  than Rust's (`twitter` parse: 81.8 and 86.8 ms in two sets). Not
  measured: memory use (a Codira value is 64 bytes, an object member 96),
  and pause times.

**Not done:** typed decoding into program types (waits for derive and
generic traits: a serialization framework comes next); a streaming or
event parser; reading a number's exact text (an integer beyond `i64` is a
float, a decimal is rounded to an `f64`); JSON Pointer and JSON Patch; a
limit on input or tree size; a seeded or flooding-resistant key hash (an
object keeps its own index rather than a `std.collections.map`: its
members stay in insertion order, which `Map` does not keep, the parser
builds every object from one shared stack of members and allocates it
once at its size, and an object of up to 8 members has no index at all;
keying that index with SipHash costs a SipHash per key of every object
above 8 members, which has not been measured against the `serde_json`
numbers above);
printing keys sorted; comments or trailing commas as an option.

### Integer arithmetic, shifts and casts

What each operator does, on every integer width (`i8` to `i128`, `u8` to
`u128`) and on both back ends (LLVM, and Cranelift with `--jit`). None of
them is undefined behaviour.

- **`+`, `-`, `*` and unary `-`** wrap at the type's width (two's
  complement): `127i8 + 1i8` is `-128i8`.
- **`/` and `%`** (and `/=`, `%=`) truncate toward zero, and the remainder
  has the sign of the dividend: `-7 / 2` is `-3`, `-7 % 2` is `-1`. A zero
  divisor panics (`attempt to divide by zero`, `attempt to calculate the
  remainder with a divisor of zero`), and so do the signed `MIN / -1` and
  `MIN % -1`, whose quotient does not fit (`attempt to divide with
  overflow`, `attempt to calculate the remainder with overflow`). The panic
  is reported as an out-of-range index is: `panic at mod.code:5:5: attempt
  to divide by zero` on stderr, exit status 101.

  The check is a compare and a branch the optimizer knows is cold (two for a
  signed type, and LLVM drops the `MIN` test when it knows the dividend is
  not `MIN`). It is left out when the divisor is a positive integer literal,
  and when the function has a contract and the verifier proves the divisor
  non-zero (and, signed, the division free of `MIN / -1`): `requires d > 0`
  does both. That is the same decision the effect analysis makes for
  `@nopanic` (see below), so a division in a `@nopanic` function never has a
  check. Measured on x86-64 at `-O 2`, loops of 200 million `i64` and `u32`
  divisions and remainders by a run-time divisor (constant across the loop,
  or changing every iteration) run within 2% of the same loops with the
  divisor proven safe, which is below the run-to-run noise: the division's
  own latency hides the predicted branch.
- **`<<` and `>>`** take the shift amount modulo the width: only its low
  `log2(width)` bits count, read as unsigned, so `x << 65` on an `i64` is
  `x << 1`, and on a signed type a negative amount `-k` shifts by
  `width - k` (`1i32 << -1i32` is `i32::MIN`). This is what Rust's
  `wrapping_shl`, Java and C# do, and what x86-64 and `AArch64` shift
  instructions do at 32 and 64 bits, so it costs nothing there; an 8- or
  16-bit shift keeps one `and`. `>>` is arithmetic on a signed type and
  logical on an unsigned one. The verifier models shifts the same way.
- **`as` between integers** truncates to a narrower type and sign- or
  zero-extends (by the *source's* signedness) to a wider one; `bool as` an
  integer is 0 or 1.
- **`as` from a float to an integer** rounds toward zero and saturates: a
  value below the target's range gives its `MIN`, above it its `MAX`, and
  NaN gives 0 (`300.5 as u8` is `255`, `-1.0e10 as i16` is `-32768`), for
  every target width, as Rust's `as` does. LLVM implements it with
  `llvm.fpto{s,u}i.sat`; Cranelift converts to 32 or 64 bits with its
  saturating conversion and, for an 8- or 16-bit target, clamps and narrows.
- **`as` from an integer to a float** rounds to the nearest representable
  value (ties to even), and `f64 as f32` rounds the same way. Float
  arithmetic is IEEE 754 in the type's own precision, with no fused or
  reassociated operations; float `/` by zero gives an infinity or NaN, never
  a panic.

### Hot-loop builtins: `array_filled`, `select`, `fma`, `likely`, `unlikely`

```
let xs = array_filled(0.0, n)        // [f64] of n copies; n is a usize
let m  = select(a > b, a, b)         // both operands evaluated, then one LLVM `select`
let y  = fma(x, w, acc)              // x * w + acc, rounded once (f32 or f64)
if unlikely(i >= limit) { .. }       // branch weights for the optimizer
```

- `array_filled(v, n)` makes an array of `n` copies of `v` in one
  allocation. `v` is any value: a number (the fill is a counted loop that
  LLVM turns into a `memset` or vector stores), a tuple, a value struct, a
  string, or a class reference (`n` copies of the one reference). Generic
  code uses it to allocate `n` elements of a type parameter at once
  (`array_filled((key, value), n)` in `std.collections.map`). It allocates,
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
  `-O3` without help.
- An integer division by a run-time divisor checks it (see "Integer
  arithmetic, shifts and casts"); `requires d > 0` on the function removes
  the check, although measured it costs under the noise next to the
  division itself. There are no vector (`<4 x f64>`) types in the language
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
- Not supported (each a clear error): braced variants
  (`Circle { radius: f64 }`), an enum containing itself by value (hold the
  recursion through a `class` or an array), variants as function values
  (`let f = Shape.Circle`), printing an enum with `print` (format it:
  `f"{s:?}"` is `Shape.Circle(2.0)`), and enums in
  `codira run --jit`, the MIR/e-graph path and quantum kernels. The verifier
  treats a payload a pattern binds as an unknown value.
- Hot reload: a stored enum keeps its tag and the slots whose names (`f64.0`,
  `i64.0`, ..) survive; new slots start at zero. Append new variants at the
  end: the tag is the declaration index, so reordering variants changes what
  stored values mean, and a value whose variant was removed makes a later
  `match` stop with "no `match` arm matches this value".

#### Generic enums

```codira
enum Maybe[T] { Just(T), Nothing }
enum Tree[T] { Leaf(T), Node([Tree[T]]) }    // recursion through an array

extend Maybe[T] {
    func or(self, fallback: T) -> T {
        match self { Maybe.Just(v) -> v, Maybe.Nothing -> fallback }
    }
}
extend Maybe[T: Shape] {                     // only where `T: Shape`
    func area(self) -> i64 { match self { Maybe.Just(s) -> s.area(), _ -> 0 } }
}

let a = Maybe.Just(2.5)                      // Maybe[f64], from the payload
let b: Maybe[i64] = Maybe.Nothing            // from the expected type
let n = count([Maybe.Just(1), Maybe.Nothing])
```

- An enum takes type parameters like a struct, and is monomorphised the
  same way: each instantiation (`Maybe[i64]`, `Maybe[str]`) is a type of
  its own with its own layout -- a `Result[i64, i64]` needs one `i64`
  slot, a `Result[i64, f64]` an `i64` and an `f64` slot. No boxing, no
  allocation.
- Arguments are written in type position only (`Maybe[i64]`); a
  construction learns them from its payload and the expected type, a
  pattern from the value it matches. Everything above -- `match` with
  exhaustiveness, `if let`, `while let`, `==` when the instantiation's
  payloads are scalars, fields, arrays, parameters and results, modules,
  hot reload -- works on an instantiation as on a plain enum.
- Methods go in `extend Maybe[T] { .. }`; `extend Maybe[T: Trait]` adds
  methods that exist only where the argument implements the trait (a call
  on another instantiation is a bound error at the call). Bounds on the
  enum's own parameters (`enum E[T: Shape]`) are an error: bound the
  `extend`.
- A value enum cannot contain itself (`enum Tree[T] { Node(Tree[T], T,
  Tree[T]) }` has no finite size, and is an error): recurse through an
  array (`Node([Tree[T]])`) or a `class` (`class Node[T] { value: T, next:
  Link[T] }` with `enum Link[T] { End, More(Node[T]) }`). This is the same
  rule as for a plain enum, and what Rust's `Box` is for.
- To the runtime and to hot reload an instantiation is named with its
  arguments (`results::Result[i64, str]`) and migrates by the slot rules
  above.

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
  share one namespace).
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

`T?` is the standard library's optional; there is no `Option[T]` enum,
since it would add nothing `T?` lacks (`std.collections.optional`'s older
`Optional[T]` struct converts with `to_native`/`from_native`). For "a value
or why not", use `Result[T, E]` (next section).

### Error handling: `Result` and `?`

```codira
import std.result.result.{Result}
import std.fs.file.{read_file, FileError}
import std.text.ascii.{next_word, parse_i64, ParseError}

enum Problem { Io(FileError), Parse(ParseError) }

func sum_file(path: str) -> Result[i64, Problem] {
    let bytes = read_file(path)?              // FileError -> Problem.Io
    var total: i64 = 0
    var at: usize = 0
    while let (start, end) = next_word(bytes, at) {
        total += parse_i64(bytes, start, end)?   // ParseError -> Problem.Parse
        at = end
    }
    Result.Ok(total)
}

func first_word(bytes: [u8]) -> usize? {
    let (start, _) = next_word(bytes, 0)?     // `nil` returns `nil`
    start
}
```

- `Result[T, E]` (`std.result.result`) is `Ok(T)` or `Err(E)`, a generic
  enum. Its methods: `is_ok`, `is_err`, `ok` and `err` (to `T?`/`E?`),
  `unwrap_or`, `unwrap_or_else(func(E) -> T)`, `expect(message)` and
  `unwrap` (panic on an `Err`), `expect_err`, `map`, `map_err`,
  `and_then` (taking closures or named functions), `or`; and
  `from_optional(value, error)`.
- Postfix `?` on a `Result[T, E]` is the `T` inside an `Ok`; on an `Err`
  it returns from the enclosing function (or closure). That function must
  return a `Result[_, F]` of the same `Result` declaration, where either
  `F` is `E`, or `F` is an enum with exactly one variant whose one payload
  is an `E` -- then the error leaves wrapped in that variant
  (`Problem.Io(e)` above). This is what Rust's `From` implementations are
  written for, read off the declaration instead: nothing to write, decided
  at compile time, and two variants holding the same type are an error
  naming both rather than a choice. Or, third, `F` has an
  `@error_conversion` that takes an `E`: the error leaves converted,
  `AnyError.from_error(e)` (see "Errors with context" below).
- On a `T?`, `x?` is the `T`, or returns `nil` from a function that
  returns an optional (of any payload).
- Anything else is a compile error saying which rule is broken: `?` on a
  value that is neither, a function whose result is not a `Result`
  (resp. an optional), an error type that neither matches nor is wrapped
  by exactly one variant nor converted.
- Which enums are a `Result`: any enum declared `Result` with exactly the
  variants `Ok(T)` and `Err(E)` -- the standard library's, or a program's
  own of that shape. A program's own `Result` and the standard one are
  different types; `?` never converts between them.
- What it costs: a tag test and a branch, the error moved into the
  caller's `Err` (and its wrapping variant). No allocation, no call, no
  unwinding. A converting `?` adds the conversion's call on the `Err` path
  only (measured below). The branch is weighted towards `Ok`
  (`llvm.expect`): the error path is laid out of line, after the hot code,
  and calls made only to build the error are not inlined into the success
  path. When the fallible function is inlined -- one of the same library,
  an instance of a generic one, or an `@inline` function of another
  library, which the caller compiles a copy of -- the `Result` is never
  built: its tag test folds into the callee's own branches. A `Result`
  that is returned (a call that stays) comes back in two registers when
  its layout is two words (`Result[i64, i64]`), through memory when it is
  larger. `crates/codira/tests/error_propagation.rs` checks both on the
  optimized IR.
- An early return through `?` is a return, not a panic, for `@nopanic`
  and the other effect promises. The contract verifier checks
  `ensures` at every `?` as a return point; a postcondition about a
  `Result` or optional result is not modelled and is reported as not
  checkable, never passed. The information-flow checker treats `?` on a
  secret as a branch and a return.
- Grammar: `?` is a postfix operator like `!`, binding as tightly as a
  call or a field access (`f(x)?.len()`). The `??` hole is an atom and can
  never follow an expression, so after an expression `?` is always this
  operator (`x??` is `(x?)?`); a `?` that begins a line is not postfix, so
  a `??` on a line of its own is still a hole. `T?` is a type, parsed by
  the type grammar.
- Not yet: a conversion between two different `Result` declarations,
  and `codira run --jit` (Cranelift refuses `?` and generic enums; the
  program runs on LLVM).

#### Errors with context: `std.error.error`

```codira
import std.error.error.{AnyError, Error}
import std.fs.file.{read_file}
import std.result.result.{Result}

struct Limit { max: i64, got: i64 }
extend Limit: Error {                        // any type can be an `Error`
    func message(self) -> String { f"{self.got} is over the limit of {self.max}" }
}

func load(path: str) -> Result[[u8], AnyError] {
    read_file(path).context(f"reading {path}")      // FileError -> AnyError
}

func setup(path: str) -> Result[i64, AnyError] {
    let bytes = load(path).context("loading the configuration")?
    let n = bytes.len() as i64
    if n > 100 {
        let over: Result[i64, Limit] = Result.Err(Limit { max: 100, got: n })
        return Result.Ok(over?)                     // Limit -> AnyError, by `?`
    }
    Result.Ok(n)
}

public func main() -> Result[(), AnyError] {
    let n = setup("app.toml").context("starting")?
    println(n)
    Result.Ok(())
}
// With no app.toml, on standard error, and exit status 1:
// Error: starting: loading the configuration: reading app.toml: file not found
```

For a function that can fail for many reasons its caller will not tell
apart -- an application's, a test's -- `Result[T, AnyError]` and `?` say
what went wrong and what was being done, without an error enum to write.
Where the caller does decide by the cause, return the concrete error type
(`FileError`) or an enum of them, as above; `AnyError` describes an error,
it does not keep it.

- **`Error`** is a trait: `message(self) -> String` says what went wrong
  (a phrase, no trailing period: `"file not found"`); `source(self) ->
  String?` (default `nil`) is the message of the error that caused this one,
  for an error that wraps another; `into_any(self) -> AnyError` (default:
  the message, the source and the type's name) is what the rest is built
  on. Implement it with `extend MyError: Error { func message(self) ->
  String { .. } }`. `std.fs.file`'s `FileError` (`file not found`,
  `permission denied`, `is a directory`, `I/O error <code>`) and
  `std.text.ascii`'s `ParseError` (`no digits`, `invalid digit at byte
  <i>`, `number too large`) implement it.
- **`AnyError`** is a plain value struct: the root cause's message, the
  contexts added on the way out, and the name of the type of the error it
  was made from (`x.type_name()`, a constant). `e.to_string()` (and so
  `f"{e}"`) writes the chain outermost first, `starting: reading app.toml:
  file not found`; `e.chain()` is the same as an array, `e.root_cause()` the
  last part, `e.error_type()` the type's name as a `str?` (`FileError`, or
  `nil` for `AnyError.new(message)`), and `e.context(what)` adds a context
  to an `AnyError` itself. `AnyError` implements `Error` too, returning itself
  from `into_any`, so contexts accumulate rather than nest.
- **`r.context(what)`** on a `Result[T, E]` where `E: Error` is the
  `Result[T, AnyError]` whose `Err` is the error with `what` as its
  outermost context; an `Ok` is passed through. `what` is a `str` or a
  `String` (anything implementing `ContextText`); a literal is copied into
  the error only for an `Err`, an `f".."` is built before the call either
  way. **`r.with_context(f)`** takes a `func() -> String` and calls it only
  for an `Err`. On another `E` both are an error at the
  call: ``the type `Oops` does not implement the trait `Error` ..``.
- **`?` converts.** The third rule of `?`, after "the same type" and "the
  one wrapping variant": when the function's error type `F` has an
  associated function marked `@error_conversion` -- one parameter, returning
  an `F` -- and the error can be passed to it, `?` passes `Err(e)` on as
  `F.conversion(e)`. `AnyError` declares `@error_conversion func
  from_error[E: Error](error: E) -> AnyError`, so in a function returning
  `Result[_, AnyError]`, `?` takes an error of any `Error` type, and an
  `AnyError` as it is (the first rule). The conversion is an ordinary
  generic call, instantiated per error type and resolved at compile time;
  a type its bounds rule out is an error at the `?`: ``?` cannot pass this
  `Oops` error on as a `AnyError`: `AnyError.from_error` takes errors whose
  type implements `Error`, and `Oops` does not; implement it with `extend
  Oops: Error { .. }` ``. A program can declare a conversion for its own
  type the same way; two marked conversions on one type, or one with the
  wrong shape, are errors at the `?` that would use them.
- **Why an attribute**: the rule is keyed on `@error_conversion`, not on a
  type's name or module, so the compiler names no standard-library path,
  and an unrelated function that happens to be called `from_error` is never
  called behind the program's back. A rule keyed on a trait would need a
  generic trait (`From[E]`), which the language does not have yet; the
  attribute plus an ordinary generic bound (`[E: Error]`) expresses the
  same thing, with the bound checked like any other (and reported at the
  `?`).
- **`main` may return `Result[T, E]`** (a `main` in the package's root
  module). On an `Err` the program writes `Error: ` and the error to
  standard error -- its type's `to_string()` when it has one (`AnyError`'s
  writes the chain), a number's or string's text, the debug form otherwise
  (`Error: Why.Code(7)`) -- flushes standard output first, and exits with
  status 1. On an `Ok` it returns the `Ok` value to the host, which `codira
  run` prints like any primitive result (nothing for `()`).
- Effects: making an `AnyError` allocates (its message, its context
  array), and `?` through a conversion is a call of it, so a function with
  a converting `?` is not `@noalloc`; a wrapping or same-type `?` still
  costs nothing.
- **What it costs**, measured (`examples/error_bench/`: a `parse_i64` that
  succeeds or fails, passed on by `?` from a function, 1M calls; and
  `examples/results`' loop -- `next_word`, `parse_i64` and `?` per word --
  over 1M words in memory; the median of 7 runs in each process and the
  median of 5 processes, nanoseconds per call or word). `error_rust.rs` is
  the same work in Rust (the same algorithms, `From` for the wrapping, an
  `AnyError` of the same shape built the same way), printing the same check
  values. x86-64 VM, Intel Xeon @ 2.10 GHz, release builds of the compiler
  from just before and just after the change, Rust 1.97 at `-C
  opt-level=3`; load average about 4.8 on the 4 cores during the runs (a
  later run of the final build, at load 1, gave the same `Ok` times within
  0.3 ns):

  | `?` passing on a `ParseError` | before | now | Rust |
  |---|---:|---:|---:|
  | wrapped in an enum variant (`Problem.Parse(e)`), `Ok` | 5.78 | 3.98 | 2.96 |
  | the same, `Err` | 5.58 | 3.56 | 2.75 |
  | converted to an `AnyError`, `Ok` | 5.69 | 4.01 | 2.77 |
  | `.context("parsing the answer")?`, `Ok` | 5.49 | 3.76 | 2.77 |
  | `examples/results`' loop, per word | 21.5 | 17.4 | 19.0 |

  A conversion's `Err` path, which this work leaves as it was, costs 252
  ns (`converted`) and 318 ns (`.context`) against Rust's 65 and 89: it
  builds the message (an `f".."`) and an `AnyError` in the GC heap.

  The tight `?` loop was 1.95x Rust's time (`wrapped`, `Ok`) and is 1.34x.
  The cause was not the `?`, which was already a tag test and a branch, but
  what surrounded it: `parse_i64` lives in the standard library's own
  library, so every call went through the dispatch table -- an indirect
  call LLVM cannot inline -- and its 32-byte `Result` came back through
  memory to be tested. Rust inlines the parser, and its `Result` never
  exists. Now `std.text.ascii`'s functions are `@inline`, the program
  compiles them in, and the `Result` folds away; the loop over words is
  faster than Rust's. What remains on the `Ok` path is the array's
  indirection (a handle to its payload, reloaded per call) and a bounds
  check per byte. An error is expected to be rare.
- Not done: keeping the original error inside an `AnyError` (that needs
  trait objects; only its message and type name are kept, so there is no
  downcasting), a backtrace, `source` returning an error rather than its
  message, `?` converting between two different `Result` declarations, and
  `main` reporting an `Err` when it is not the root module's `main` (an
  entry run with `codira run other_entry` that returns a `Result` is refused
  by the host as a non-primitive result, as before).

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

### Structured concurrency: `scope`, `group`, `spawn`, channels

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
  runtime's scheduler. It must be inside a `scope` or a `group` (of the same
  function; a handler arm and a closure count as functions of their own),
  and belongs to the innermost one; `f` takes only integers, floats, `bool`
  and `Channel[T]` and declares no effects. Arguments are evaluated by the
  spawner. In a `scope` the result is discarded -- results come back
  through channels; in a `group` it is the group's value (below).
- **No task outlives its block.** That is the only task model: there is no
  unstructured `spawn`. The compiler guarantees it statically (a `spawn`
  outside a `scope`/`group` of the same function is an error, and the
  block's every exit joins), and the runtime checks it again: starting a
  task of a block that has been joined, or freeing a group before joining
  it, stops the process with a message, and debug builds also check, when
  each task ends, that its block was not joined while it ran. Unstructured
  spawning was not added because nothing could then own a task's channels
  or bound its lifetime, and every guarantee below depends on that.
- `let ch = channel(capacity)` creates a bounded blocking `Channel[T]` of
  integers, floats or `bool`, owned by the enclosing `scope` and freed when
  it ends. A channel is only *used*: `ch <- v`, `<-ch`, or passed to a
  function or `spawn`. It cannot be stored, returned, copied into another
  binding or made the value of a block -- that is what keeps it from
  outliving its scope.
- Tasks share nothing but channels, so they cannot race on memory. A task
  may allocate; a collection on any thread stops it at its next allocation
  (or finds it parked in a channel operation or join) and scans its stack,
  so its objects stay alive (see "End-to-end status today").
- Scheduling (`crates/codira_runtime/src/tasks/scheduler.rs`): tasks run to
  completion on worker threads -- a task is compiled code with native
  frames, so it is never suspended or moved. Each worker has its own deques
  (Chase-Lev, via `crossbeam-deque`): a task spawned on a worker goes to its
  deque, the worker takes the newest, idle workers steal the oldest; a task
  spawned from the host goes to a global queue. A task waiting for its
  `scope` or `group` first runs its own children that are still on its
  deque, and nothing else (an unrelated task could wait for something the
  joiner does after the join). The scheduler keeps
  `available_parallelism()` workers active (`CODIRA_TASK_WORKERS=n`
  overrides it), started lazily.
- **Waiting never occupies a worker.** A task that waits -- a channel
  operation, `select`, a join it cannot help with, a sleep
  (`std.time.time`), file I/O (`std.fs`) -- releases its worker for the
  duration, and a sleeping worker is woken or a new one started if tasks
  are queued (Go hands a processor off on a blocking system call the same
  way). So blocked or blocking tasks cannot starve the ones that would
  unblock them, and a thousand tasks that each sleep 100 ms finish in a
  fraction of a second, not 25 s (`fabric_stress.rs`). Workers beyond the
  target exit when they find no work. There is no upper bound on threads
  while tasks wait: bounding it could deadlock a correct program. A task
  that *busy-waits* (`while !flag() {}` without a blocking call) keeps its
  worker, as in any run-to-completion pool. Mismatched sends and receives
  still block forever, as with any blocking channel.
- **Priority classes**: `High`, `Normal` (the default) and `Background`.
  A task inherits its spawner's class; the host picks the class of the
  tasks a call spawns with `codira_runtime::tasks::with_priority(Priority::
  Background, || runtime.invoke(..))`. Each worker starts tasks by a fixed
  weighted round robin, high, normal and background in the ratio 4 : 2 : 1
  while all three have work (measured: 0.571 / 0.286 / 0.143 of the first
  1,400 starts of 9,000 queued tasks, five runs out of five; test
  `priority_classes_share_starts_four_two_one`), so background work is
  slowed, never starved. The ratio is about *starts*: tasks are not
  preempted, so a long background task keeps its worker until it ends.
- `codira_runtime::tasks::stats()` returns the scheduler's counters (tasks
  spawned, stolen, run by a joining worker, waits, threads started and
  peak, starts per class).
- Output printed by a task is not captured by `capture_output` (it is
  per-thread); a task's `print` goes to standard output.
- A secret (`secret T`) cannot be sent on a channel: what a channel delivers
  is public.

#### `group`: tasks that return values

```codira
import std.error.error.{AnyError}
import std.result.result.{Result}

func square(x: i64) -> i64 { x * x }

func check(i: i64) -> Result[i64, AnyError] {
    if i == 13 { return Result.Err(AnyError.new(f"{i} is unlucky")) }
    Result.Ok(i * 2)
}

public func squares(n: i64) -> [i64] {
    group {                         // [i64]: every task's result, in spawn order
        var i = 0
        while i < n { spawn square(i); i += 1 }
    }
}

func checked(n: i64) -> Result[i64, AnyError] {
    let xs = group {                // Result[[i64], AnyError]
        var i = 0
        while i < n { spawn check(i); i += 1 }
    }?                              // the lowest-index error, passed on by `?`
    Result.Ok(xs.len() as i64)
}
```

- `group { .. }` is a `scope` whose tasks return values; `group` is a
  contextual keyword, promoted under the same condition as `scope`. Its
  value is the results of the tasks spawned *directly* in it (not in a
  nested `scope`, `group` or closure), in spawn order: `[R]` when they
  return `R`, or, when they return a `Result[T, E]` (the `Result` of
  `?`, with `Ok` holding its type parameter), `Result[[T], E]` -- `Ok` of
  every `Ok` payload, or one task's `Err`. A group composes with `?`
  (above) and with `std.error.error`'s `AnyError` like any `Result`.
- **Errors, deterministically.** When a task fails (returns `Err`), every
  task spawned *after* it is cancelled; the ones before it are left to
  finish. The group's value is the `Err` of the **lowest-index** task that
  failed. That is the value running the tasks one after another and
  stopping at the first `Err` would give, whatever order they really ran
  in: the lowest failing task is never cancelled (only a failure before it
  could cancel it), and every task before it runs to completion. A task
  cancelled before it started is never started (its result is never
  read). `tests/groups.rs` checks it with two failures 50 times over, and
  `crates/codira/tests/task_groups.rs` with `AnyError` and `?`.
- All tasks must return one type, and not `()` (that is a `scope`), not a
  `secret`, not a channel or endpoint. They take only integers, floats and
  `bool` -- no channels: the group does not start the tasks after a failed
  one, and a task that never runs must not leave a sibling waiting on a
  channel (a task *may* open a `scope` of its own). A result may hold
  anything else, `String`s, arrays, objects: a task's arguments are
  scalars and there is no module-level state, so everything its result
  refers to was made by the task, which has finished -- the result is moved
  to the joining task, not shared. Until then the runtime keeps it in
  memory the collector scans (`codira_memory::gc::root_region`).
- A group that spawns nothing is an error (its value's type comes from its
  tasks). Every exit from the block -- its end, `return`, `break`, `?`, a
  handler aborting -- joins its tasks; only the end reads their results.
- Cost (measured below): a `group` task is a 16-byte-aligned copy of its
  arguments, a deque push and a result slot; the value is built with one
  array allocation.

#### Cancellation

- Every `scope` and `group` records the task that opened it, so they form a
  tree rooted at the host call. A task is *cancelled* when its group
  cancelled it (a failure before it, above) or any task above it in the
  tree is cancelled. `cancelled()` (a builtin, no effects: allowed in
  `@realtime` code) says whether the running task is; it is `false` outside
  any task.
- Cancellation is cooperative: nothing is interrupted, a cancelled task
  stops where its code checks `cancelled()`, and leaves through its
  `scope`s and `group`s as usual, which joins their tasks and frees their
  channels and buffers -- that is how resources are released. A `scope`
  never cancels or skips its own tasks (they talk through channels, and
  skipping one could leave another waiting forever); only a group's
  not-yet-started tasks after a failure are skipped.
- The host cancels a call with `codira_runtime::tasks::CancelToken`:
  `token.run(|| runtime.invoke(..))` on one thread, `token.cancel()` on
  any other; every task below sees `cancelled()`.
- A blocked channel operation is not woken by cancellation (it would have
  to return a value it does not have); a task that may wait for a cancelled
  sibling should check `cancelled()` between waits, or use `select` with
  `default`.

#### Measured

x86-64 VM, Intel Xeon @ 2.10 GHz, 4 cores, 1 NUMA node (shared: load
average about 1 to 1.8 during the runs); Codira at `-O 3` with a release
build of the compiler and runtime, Rust 1.97 at `opt-level = 3` with rayon
1.10 and crossbeam-channel 0.5, Go 1.24; median of 7 alternating runs,
milliseconds, each a warm call. Programs:
`examples/fabric_bench/` (`bench.py` builds and runs them); all three print
the same check values.

| workload | workers | Codira | Rust | Go |
|---|---:|---:|---:|---:|
| spawn/join: 100k tasks returning their index, joined, summed (`group`; `rayon::scope`; goroutines + `WaitGroup`) | 1 | 94.9 | 9.9 | 148.6 |
| | 4 | 122.4 | 70.3 | 64.4 |
| fan-out/fan-in map over 1M items in 64 tasks, gathered into one array (`group` + `append`; `par_iter().map().collect()`; goroutines writing slices) | 1 | 13.2 | 6.6 | 13.1 |
| | 4 | 9.4 | 2.9 | 3.3 |
| producer/consumer: 1M values over a channel of capacity 64 (`scope` + `channel`; crossbeam `bounded` + a thread; buffered `chan`) | 1 | 182.0 | 70.5 | 48.2 |
| | 4 | 186.5 | 35.8 | 50.6 |
| CPU-bound: 64 tasks of 4M xorshift steps | 1 | 491.3 | 485.8 | 493.9 |
| | 4 | 126.4 | 127.4 | 125.9 |

Speedup from 1 to 4 workers on the CPU-bound workload: Codira 3.89x, Rust
3.81x, Go 3.92x. What the losses are:

- **Spawn/join** costs Codira about 1 us per task on one worker, ten times
  rayon's (not profiled further; each task makes a heap copy of its
  arguments, allocates its record in the task tree and stores its result
  under the group's lock), and 1.7-1.9x Rust and Go on four, where all
  three spawn from one thread.
- **Fan-out/fan-in**: the parallel map itself takes 1.8 ms of the 9.4 (5.2
  ms on one worker, measured inside the program); gathering the 64 chunks
  into one fresh 1M-element array -- one copy per chunk, on one thread,
  first-touching 8 MB -- takes about 5.7 ms. rayon writes its result in
  place, in parallel.
- **Channels** are 2.6-5.2x slower than crossbeam's and 3.7x Go's: a
  channel is a mutex-guarded queue with condition variables (the `select`
  and GC protocols are built on it); a lock-free channel is not done. A
  send or receive tries for a few microseconds before it sleeps (before
  that change the pipeline took 534 ms and 400,000 futex calls; now 68).

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

- **Allocates** (and so may also block on the heap's lock, collect, and end
  the program with exit status 102 when the heap limit or memory runs out):
  constructing a `class`, an array literal,
  `array_filled`, the array methods that can grow the buffer (`push`,
  `insert`, `reserve`, `append`) or make a new array (`copy`, `map`,
  `filter`, slicing `xs[lo..hi]`), a pipeline's `collect`, a closure (except
  one an array method or a pipeline stage compiles in place, see "Growable
  arrays and collections" and "Lazy pipelines"),
  `print`/`println`, `channel(n)`, `scope`, `group`, `spawn`, a channel send, `select`
  (even with `default`), opening a session (`S.open()`), making or
  growing a `String` (see "Strings" for exactly which operations), and a
  call to a function of *another module* that takes or returns a value
  struct or a `String` (the cross-module calling convention boxes it).
  String literals, views of a `String` (`slice`, `trim`), value structs,
  tuples and effect handler records allocate nothing.
- **Blocks**: channel send and receive (also on a session endpoint, and
  `ep.choose(..)` and `offer`), `select`, the end of a `scope` or a
  `group` (`@noblock` and `@realtime` code cannot wait on a group,
  directly or through a call: "`group` waits for its tasks at the end"),
  and the allocations above. `cancelled()` does not block.
- **Panics**: `a[i]` and `a[i] = v` (bounds check; `a.unchecked_get(i)` is
  the unchecked escape hatch), `xs.insert(i, x)`, `xs.remove(i)` and slicing
  (their index checks), `s.slice(a, b)` and `s.byte_at(i)` on a
  `String`, integer `/` and `%` (and `/=`, `%=`), calling a
  `@runtime_checked` function that has `requires`, and every allocation.
  An integer division panics on a zero divisor and on a signed `MIN / -1` or
  `MIN % -1` (see "Integer arithmetic, shifts and casts"), so it only counts
  as safe when its divisor is a positive literal or the function has a
  contract and Z3 proves the divisor non-zero (and, signed, not `MIN / -1`).
  Exactly those divisions are compiled without the check. Shifts never
  panic: the amount is taken modulo the width.
- **May not terminate** (`@realtime` only): a `while` or `loop` without a
  `decreases` measure, or whose measure the verifier does not prove, and
  recursion, direct or mutual, and a loop over an iterator object (`for x in
  it`, `it.iter()`: nothing bounds how often its `next` returns a value).
  Other `for` loops, and pipelines over arrays and ranges, are bounded by
  construction.
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
- **Importable today** (36 of 96 modules), each run from a program in
  `crates/codira/tests/std_imports.rs` against independently computed
  values, except `std.memory.page` and `std.builtin.type_aliases` (check,
  build and load; not run):
  - `std.crypto.hash` (SHA-256, SHA-512, BLAKE3), `std.fs.fs`,
    `std.memory.region`, `std.memory.page`.
  - `std.db.db` (the storage engine's transactions) and `std.ai.runtime`
    (f32 tensors and a Linear layer): wrappers over the runtime's database
    and tensor services, run by `crates/codira/tests/db_and_ai.rs`.
    Experimental; see the runtime section for their limits.
  - `std.result.result` (`Result[T, E]` and its methods, see "Error
    handling"), `std.fs.file` (`read_file(path) -> Result[[u8],
    FileError]`) and `std.text.ascii` (`next_word`, and `parse_i64` with
    a `ParseError` carrying the offending byte's index).
  - `std.error.error`: the `Error` trait, `AnyError`, `context` and
    `with_context` (see "Errors with context"), run by
    `crates/codira/tests/error_context.rs`. `FileError` and `ParseError`
    implement `Error`.
  - `std.bit.bit`, `std.bit.mask`: bit counts, reversal, byte swaps,
    rotations, masks and fields on `u64`/`u32`. Exact. `bit_ceil` needs a
    result that fits (`value` at most 2^63).
  - `std.math.constants` (`PI()`, `E()`, `INF()`, `NAN()`, `F64_MAX()`,
    integer limits...: niladic functions, since module-level
    `let`s are not supported) and `std.math.fast` (f32 approximations: `fast_sin`/`fast_cos`
    within 7e-5 absolute, `fast_exp` 0.01 % relative, `fast_log` 5.4e-4
    absolute, `fast_sqrt`/`fast_rsqrt` 0.175 % relative; `fast_pow` and
    `fast_tan` have no stated bound).
  - `std.hashlib.hash`: SipHash-1-3 and SipHash-2-4 under a `HashKey`
    (`HashKey.random()` from the runtime's entropy service), `SipHasher13`,
    and `Hashable` with `hash_keyed` (SipHash) beside `hash_value`; FNV-1a
    64 over `[u8]`, integers, floats and bools, hash combiners, and
    `hash[T: Hashable]`. SipHash under a secret key is DoS-resistant; FNV-1a
    and `hash_value` are not, and nothing here is a cryptographic hash for
    integrity (see "Hashing" below).
  - `std.base64.base64`: base64, base32 and base16 encode and decode over
    `[u8]` (encoded text is its ASCII bytes), standard alphabets with `=`
    padding; decoding is lenient: it skips bytes outside the alphabet,
    returns an empty array only for a few malformed inputs (a base64 group
    starting with `=`, odd-length or non-hex base16), accepts other
    malformed input without an error, and is not canonical (`QQ==` and
    `QR==` both decode to `A`). Not for input that must be validated.
  - `std.random.rng`: xoshiro256** with the state in a `Rng` the caller
    holds, seeded by SplitMix64 (`rng_seeded(seed)`, `rng_next_u64`,
    `rng_next_f64`, `rng_range_i64`, `rng_next_bool`), or from the system
    (`rng_from_entropy()`). The generator is **not cryptographic**
    (predictable from its output); `os_random_u64()` and
    `os_random_bytes(n)` are the operating system's secure generator,
    through the runtime's entropy service `codira_random_fill`.
  - `std.builtin.type_aliases` (`Never`).
  - `std.sys.terminate`: `panic` (writes `panic: <message>` to standard
    error and aborts), `exit`, `abort`, `assert`, `unreachable`, `todo`.
  - `std.cmp.cmp`: the traits `Eq` (`a.eq(b)`) and `Ord` (`a.lt(b)`) for
    every integer and float type, `bool` and `str`, `str_compare`,
    `min_of`/`max_of`.
  - `std.collections.map` (`Map[K, V]`), `std.collections.set` (`Set[T]`),
    `std.collections.counter` (`Counter[T]`), `std.collections.deque`
    (`Deque[T]`) and `std.collections.binary_heap` (`BinaryHeap[T]`), on
    growable arrays (see "Growable arrays and collections"). The hash
    tables are keyed with SipHash-1-3 by default (DoS-resistant), unkeyed
    when asked. `std.hashlib.hash` gives every integer and float type,
    `bool`, `str` and `String` a `Hashable` implementation.
  - What calls `panic`: `std.math.math` (libm-backed `sin`...`atan2`,
    rounding, `gcd`, `factorial`, classification; within 2 ulp of Rust's
    libm in the tests), `std.time.time` (durations, monotonic instants,
    wall clock, sleep), `std.complex.complex`, `std.collections.optional`
    (a generic `Optional[T]` struct; `unwrap` of an empty one panics;
    `to_native`/`from_native` convert to and from `T?`).
  - Built on `String`: `std.collections.string` (`join`, `insert`,
    `remove`, `reverse`, `count`, `split_whitespace`, code points),
    `std.format.format` (`format(template, [args])`, `format_int`,
    `hex`/`bin`/`oct`, padding), `std.hashlib.string_hash`,
    `std.complex.complex_string`, `std.base64.base64_string` (the encodings
    of `std.base64.base64` between `[u8]` and `String`) and `std.sys.arg`
    (the process's arguments and environment variables, as `String`s and
    `String?`s; under `codira run` they are the `codira` command line's).
  - `std.json.json`: a JSON value type, a strict parser and a printer
    (see "JSON"); its conformance and property tests are in
    `crates/codira/tests/json.rs`.
- **Waiting for the language**: what is written against `String` and
  something else the language lacks (`std.os` and `std.pathlib`: module-level
  `let`s; `std.testing`: trait objects; `std.sys.info`: module-level `let`s;
  `std.format.repr`: trait objects); what is still written on `Pointer[T]`
  (`std.collections.list` and `std.collections.dict`, superseded by `[T]`
  and `Map`, `span`, `linked_list`, `bitset`, and through `List`,
  `std.math.sequence` and `std.math.polynomial`); `std.builtin.traits` (generic traits);
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
options as `build`, and `--heap-limit`, `--gc-growth` and `--runtime-stats`
(see "Memory: heap limit, pacing, `--runtime-stats`"; `start`, `test` and
`bench` take the first two, `start` the third too). If the build reports an
error, nothing is run. After the
entry point returns, `run`/`start` print its value if it is a primitive
(`bool`, any integer or float type); a `()` entry point prints nothing. A
`main` returning a `Result[T, E]` returns its `Ok` value (printed the same
way) or ends the program on an `Err` with `Error: ...` on standard error
and exit status 1 (see "Errors with context").

### Memory: heap limit, pacing, `--runtime-stats`

```bash
codira run --heap-limit 512MiB            # cap the GC heap (also 2GiB, 1500MB, 25%, unlimited)
CODIRA_HEAP_LIMIT=1GiB codira run         # the same from the environment
codira run --gc-growth 50                 # collect when the heap grows 50% over its live data
codira run --runtime-stats                # print the collector's statistics at exit
```

```toml
# codira.toml
[runtime]
heap-limit = "512MiB"   # or "25%", or "unlimited"
gc-growth = 100         # or "off"
```

- **Every runtime the tool starts has a finite heap limit**: `run`,
  `start`, `test` and `bench` (and each test's and benchmark's process), and
  the launcher of `build --exe`. The limit comes from the first of
  `--heap-limit`, `CODIRA_HEAP_LIMIT`, `heap-limit` in `[runtime]` (not for
  `start`, which has no manifest; the launcher reads only the environment),
  and the default: **half of the memory available to the process**, the
  smaller of physical memory and the cgroup's memory limit (containers), at
  least 64 MiB, 4 GiB if neither can be read. Sizes take `B`, `K`/`KiB`,
  `M`/`MiB`, `G`/`GiB`, `T`/`TiB` (binary), `KB`/`MB`/`GB`/`TB` (decimal),
  fractions (`1.5GiB`), a percentage of the available memory (`25%`) or
  `unlimited`; less than `64KiB` is refused. A bad value is an error that
  names its source, before anything is built. `unlimited` prints `warning:
  the garbage-collected heap has no limit (...)`. The limit caps the GC
  heap only (blocks, large objects, grown arrays), not region arenas, pages
  or `codira_alloc`.
- **Pacing**: the collector runs when the heap has grown by `gc-growth`
  percent (default 100) over what the previous collection left live, and
  not below 4 MiB of objects, like Go's `GOGC`; so a program that allocates
  far more than the limit in total, with a small live set, runs in a small
  heap. The limit stays the hard cap: an allocation that would exceed it
  collects first. `gc-growth` takes the same three sources as the limit;
  `off` collects only at the limit.
- **Running out**: when an allocation still does not fit after a full
  collection, the program ends with exit status **102** (a panic is 101)
  and, after what it printed:

  ```text
  fatal: out of memory: cannot allocate `Node` (24 bytes)
    heap limit: 1048576 bytes (1.00 MiB)
    committed:  1048576 bytes; the allocation needed 65536 more
    live:       393216 bytes after a full collection
    the limit is 1.00 MiB, from --heap-limit
    raise it with --heap-limit <SIZE>, the CODIRA_HEAP_LIMIT environment variable, or `heap-limit` in the [runtime] table of codira.toml
  ```

  `codira test` reports such a test as `crashed (exit status: 102): fatal:
  out of memory: ...`.
- **`--runtime-stats`** (`run`, `start`) prints, to standard error when the
  entry point returns: the limit and growth and where each came from, heap
  bytes now and at the peak, live bytes, bytes and objects allocated and
  reclaimed, collections by cause (paced, at the heap limit, requested,
  explicit), time with the world stopped, and pause p50/p99/max. Not printed
  when the program ends early (a panic, `exit`, running out of memory).
- The design, the measurements on `examples/gc_churn` and what the
  collector does not do (it stops the world, does not move objects, scans
  stacks conservatively) are in `crates/codira_memory/README.md`. Tests:
  `crates/codira/tests/heap.rs` (the real binary),
  `crates/codira_runtime/tests/heap_limit.rs`,
  `crates/codira_memory/tests/gc_policy.rs`.

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

### Where objects live: `--explain-alloc` and `--frame-alloc` (experimental)

```bash
codira build --explain-alloc                 # every allocation site and why it is on the heap
codira build --frame-alloc --explain-alloc   # ... and move the ones that need not be
codira run --frame-alloc                     # run with frame allocation
```

```
mod.code:6:30: in `make`: class `Point`: escapes: returned from `make`
mod.code:11:17: in `main`: class `Point`: moved to the stack
mod.code:13:18: in `main`: array literal of 3 elements: moved to the stack
mod.code:18:16: in `link`: class `Node`: escapes: stored into field `next` of parameter `head`, which may be a heap object
mod.code:23:13: in `start`: class `Point`: escapes: sent to a task by `spawn`
mod.code:27:28: in `builder`: empty array literal: escapes: grown by `push`, which the runtime does on the heap
mod.code:28:36: in `builder`: class `Point`: escapes: stored into an element of a heap object
```

The compiler's escape analysis decides, for every class instance, array
literal, `array_filled`, array built by `copy`/`map`/`filter`/slicing/
`collect`, and closure, whether the object can outlive the call that made it.
`--explain-alloc` prints its answer for each site of the package (not the
standard library), then the totals. Without `--frame-alloc` a site that
could move says `does not escape (moved to the stack with `--frame-alloc`)`.

An object **escapes** when it is returned (also inside a struct, tuple, enum
or optional, or by `?`), stored into a field or element of an object that is
or may be on the heap (a parameter counts: it may be), captured by a
closure, sent on a channel or to a task (`spawn`), passed to `perform`, or
passed to code the analysis does not see: an `extern` function, a call
through a function value, a trait method on a generic type, an array method
that grows the array (`push`, `insert`, `reserve`, `append`), a builtin it
does not model (`print`, string operations) and **any function of another
module** (a hot reload can replace that module's code without recompiling
the caller), unless it and every function it calls are `@inline` or
generic: those are compiled into the calling library. A call to a function of the same module uses that function's
summary: whether it keeps, returns, or only reads each argument, computed
through everything it calls, recursion included.

With `--frame-alloc`, a class instance or an array literal of at most 16
elements that does not escape lives in its function's stack frame: no
allocation call, no work for the collector, freed when the function returns.
Some objects stay on the heap without escaping: one allocated in a loop (or
in a closure that `map`/`filter`/`fold`/a pipeline compiles in place) that is
kept past its iteration in something outside the loop (the frame slot is
reused every iteration), arrays whose length is known only at run time,
closures, and anything allocated inside a closure body, `handle` or `spawn`.

The program's behaviour does not change: the same results, the same
`{x:?}` output, hot reload and heap snapshots work as before (a reload only
runs when no Codira frame is live; no heap object ever refers to a frame
object, so a snapshot is complete without them). The collector scans stacks
conservatively, so the GC references a frame object holds keep their objects
alive. Not combined with `--sanitize=memtag`, and not used by `--jit`.

It is experimental and off by default: it is tested on x86-64 Linux only (a
differential test runs random programs both ways under a small heap limit,
and GC stress tests check heap objects held only by frame objects), and a
deep recursion uses more stack per call than before (the frame objects of
every active call). The analysis and its correctness contract are documented
in `crates/codira_hir/src/escape.rs`; its results are the salsa queries
`escape_summary` and `escape_analysis`.

Measured (`cargo bench -p codira_runtime --bench frame_alloc`; Intel Xeon
@ 2.10 GHz, 4 cores, Linux x86-64; medians of 21 samples of 2,000,000
iterations, the same checksum from every build; allocation counts exact,
from `Runtime::stats().objects_allocated`):

| workload | heap allocs | frame allocs | heap ns/it | heap + GC (16 MiB) | frame ns/it | Rust stack | Rust `Box` |
|---|---|---|---|---|---|---|---|
| two temporaries passed to a reader | 4,000,000 | 0 | 63.5 | 63.9 | 0.00 | 1.32 | 25.3 |
| builder threaded through setters | 2,000,000 | 0 | 33.9 | 34.2 | 0.00 | 0.00 | 12.3 |
| temporary 4-element array | 2,000,000 | 0 | 45.4 | 46.0 | 0.97 | 0.00 | 17.4 |

With the objects in the frame LLVM keeps their fields in registers and
reduces the first two loops to closed forms, as it does for the Rust stack
versions: "0.00" means the whole call took microseconds, not that the
allocation became free in a loop that still runs. The heap columns are where
these programs are without the flag: an allocation through the runtime costs
about two and a half times a Rust `Box`. ("heap ns/it" runs with no
heap limit, where the collector's pacing still collects; "heap + GC" with a
16 MiB limit.)

`@noalloc` and `@realtime` still count a class construction or an array
literal as an allocation even where `--frame-alloc` would move it: the
attributes do not depend on a build flag. Strings are not analysed (their
buffers stay on the heap). There are no `@region` functions yet: an arena's
memory is not scanned by the collector the way the stack is, so a GC
reference stored in a region object would not keep its object alive; the
design and what it waits for are in `crates/codira_hir/src/escape.rs`.

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

`print`/`println` accept any number of integers (any width), floats, `bool`s,
`str`s and `String`s; anything else is a compile error. A program that defines its own
function named `print` calls its own. See `examples/hello`.

All arguments are evaluated first, left to right, and then printed, so an
argument that calls a function which prints itself does so before any of
this call's output: with `func g() -> i32 { println("in g"); 7 }`,
`println("g is ", g())` prints `in g` and then `g is 7`. Integers print in
decimal; a float prints in the shortest form that reads back as the same
value in its own type, always with a decimal point or an exponent: `1.0`,
`0.1` (for `0.1f32` too), `1e20`, `-0.0`, `NaN`, `inf`.

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
with a note naming what Cranelift does not compile yet. What it compiles
means the same as on LLVM: the same division checks and panic messages,
shift and cast semantics, argument order and float printing (see "Integer
arithmetic, shifts and casts"); `crates/codira/tests/differential.rs` runs
random programs on both and compares. Arrays (and so the growable-array
methods and the collections) are not compiled by Cranelift: such a program
runs on LLVM with the note, and the random programs of the differential
test have no arrays. Measured: a
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

### Superoptimisation: `codira build --superoptimize` (experimental)

```bash
codira build -O 3 --superoptimize
# superoptimiser: `mix` at 0x1910: mov rax, rsi; xor rax, rdi; and rsi, rdi;
#   lea rax, [rax + 2*rsi] => lea rax, [rdi + rsi] (latency 3.2 -> 1.0 cycles,
#   13 -> 4 bytes, proven by Z3)
```

After a library is linked, the straight-line integer code that ends each of
its functions (the instructions just before `ret`) is searched for a cheaper
sequence of up to three instructions. What it does and guarantees:

- A replacement is written only when Z3 proves it returns the same 64-bit
  value for every input, it writes no register the original left alone, and
  it fits in the bytes it replaces (the rest becomes one `nop`, so no address
  moves). The written bytes are decoded again and proven again; on any
  mismatch the original stays.
- Functions that must run in constant time (`oblivious`, `@constant_time`)
  are never touched, nor are generic functions, functions with an indirect
  jump, or code a dynamic relocation writes. It composes with
  `--check-constant-time`, which runs after it, and with `--certify`, whose
  certificate hashes the rewritten code.
- "Cheaper" is decided by a cost model whose x86-64 latencies and
  throughputs were measured on one Intel Emerald Rapids machine; on other
  processors the choice may not be faster. Instructions an extension
  provides (BMI1 `andn`) are used only when `--target-cpu` implies it.
- Results are cached in `target/.superopt-cache`, keyed by the code, the
  cost profile and the search bounds; a cached answer is re-proven before
  use, so rebuilds repeat no search.

Limits: x86-64 ELF libraries only (other targets build unchanged, with a
note); only the code before a `ret`, not loop bodies; 64-bit
general-purpose register arithmetic only (no memory, flags, 32-bit forms or
vectors); a function LLVM inlined into its caller is rewritten only where it
is still called. The search is bounded, so it is not exhaustive.

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

### Formatting: `codira fmt`

`codira fmt` formats every `.code` file of the enclosing package (or the
files and directories given) with the one official standard; it has no
options to configure. `codira fmt --check` writes nothing, lists each file
that is not formatted with the number of lines that would change, and exits
with an error if there is any, which is what CI runs; add `--diff` to see the
changes. `codira fmt --stdin` formats standard input to standard output, for
editors.

The standard:

- four spaces of indentation per open bracket group (`{`, `(`, `[`), plus one
  level for a continuation line: one that starts with a binary operator, a
  `.` of a method chain or a `requires`/`ensures`/`where`/`uses` clause, or
  that follows a line ending in a binary operator, `=` or `->`; a line that
  starts with a closing bracket lines up with the line that opened it;
- one space around binary operators, `=`, compound assignments and `->`,
  after `,`, `;` and `:`, before `{` and inside `{ }`; no space inside `( )`
  and `[ ]`, before `,`, `;` and `:`, around `.`, after a prefix operator or
  `@`, or between a function and its argument list;
- at most one blank line in a row, none after an opening bracket or before a
  closing one, none at the start of the file; no trailing whitespace; one
  newline at the end; `\n` line endings;
- trailing comments keep the spaces before them, so aligned comments stay
  aligned.

Line breaks are never added, removed or moved: the grammar is
newline-sensitive (a line break can end a statement), so where a line breaks
stays the author's choice. The formatter works on the lossless syntax tree,
so comments and blank lines are kept. Every result is re-parsed and compared
with the original before anything is written: the same nodes and tokens in
the same order (only whitespace may differ) and a line break before exactly
the same tokens. A file that fails that check, or that has syntax errors, is
reported and left as it is.

Limits: lines are not wrapped or joined, so there is no maximum width;
column alignment (of `=` signs or struct fields) is not kept, except for
trailing comments; the contents of block comments and strings are never
touched, including their indentation; a pair of tokens no rule covers keeps
"no space" or becomes one space, whichever it had.

### API documentation: `codira doc`

`codira doc` writes HTML documentation for the enclosing package to
`target/doc/` (`--out` elsewhere); `codira doc --std` documents the standard
library this compiler uses (`CODIRA_STD_PATH`, if set). The output is static
files: open `index.html` directly, no server is needed. It has:

- one page per module (`std.collections.array.html`), with each `public`
  item (`--private` adds the rest): its signature, attributes, `requires` and
  `ensures` clauses and doc comment, and the methods of its `extend` blocks;
- doc comments in Markdown. `///` comments directly above an item are its
  documentation; an item with none is documented by a plain `//` block
  directly above it (no blank line between), without decorative rule lines.
  `//!` comments document the module; without them, the comment block that
  opens the file does;
- cross-references: a type name in a signature links to that type, and
  `[name]` or `[module.name]` in a doc comment links to the item. A name is
  linked only when it resolves to one documented item (the current module
  first); an ambiguous one stays text;
- examples: code blocks in doc comments (unlabelled, or marked `codira`) are
  highlighted with the compiler's own lexer;
- for each function, the envelope the effect analysis infers, as badges:
  "allocates", "may block", "may panic", "may not terminate", "calls foreign
  code", "spawns tasks", "performs effects", "touches secrets", and a list of
  what it is free of. Hovering a badge shows the call chain down to the cause.
  A declared `@envelope` shows as met or NOT MET; on an `extern` function it
  shows as trusted and unchecked;
- a highlighted page per source file with line anchors, linked from every
  item;
- a search box (press `/`) over every module and item name and summary.

`--undocumented` lists the public items without documentation;
`--no-envelopes` skips the effect analysis.

Limits: the envelope is the source-level analysis `codira envelope` reports,
which is conservative: a call it cannot see into, such as a compiler
intrinsic, counts as every effect. Struct and enum declarations are shown as
written, private fields included. Method names, and items outside the
documented sources (a package's documentation does not link into `std`), are
not cross-referenced. Doc examples are highlighted but not compiled or run.

### Diagnosing the toolchain: `codira doctor`

`codira doctor` checks what this compiler runs with and prints `[ok  ]`,
`[warn]` or `[FAIL]` per check, with the exact fix under each problem. It
exits with an error if any check fails.

- LLVM: the version of the LLVM linked into the compiler, asked of the
  library itself; 22 is required.
- target: the host's (or `--target <triple>`) must be a supported triple, and
  the linked LLVM must have a backend for it.
- Z3: the version of the linked library; 4.8 or later.
- LLD: linked into the compiler from the same LLVM, so present whenever the
  compiler runs.
- C compiler: the clang `[native]` sources are compiled with (`CODIRA_CLANG`,
  else the one next to the LLVM the compiler was built with, else `clang`);
  it must run and must not be newer than LLVM 22, whose bitcode readers only
  read older bitcode. A failure only inside a package with `[native]`
  sources; elsewhere a warning.
- std library: the directory the compiler uses (`CODIRA_STD_PATH` first), or
  why it finds none.
- inside a checkout of the compiler's own sources, for contributors:
  `llvm-config` for LLVM 22 (from `LLVM_SYS_221_PREFIX`, then `PATH`), and
  nightly rustfmt, which CI formats with.
- disk space where builds go: a warning below 2 GB (20 GB in a compiler
  checkout, where a debug build takes 10-20 GB), a failure below 0.5 GB
  (5 GB).

Limits: it checks the toolchain this binary was built with and can run; it
does not try a build. Free space is measured with `df` on Unix and
PowerShell on Windows. Fixes name the package manager of the platform
(`apt-get` on Linux, Homebrew on macOS, winget or Chocolatey on Windows).

### Lints: `codira lint`

`codira lint` reports what the compiler knows about the package beyond
style. Each lint reports only what the compiler established, never a guess
from the text; `codira lint --list` lists them:

- `alloc-in-loop`: an allocation the effect analysis finds inside a loop (a
  `class` constructed, an array literal, `print`, a cross-module call that
  boxes a value struct), or a call in a loop to a function whose summary
  allocates, with the chain down to the allocation. Allocations the
  analysis only assumes, in foreign code or behind a `perform`, are not
  reported. A `for` loop's iterable is evaluated once and does not count; a
  closure body is not part of the loop.
- `large-copy`: a parameter whose type is a value struct or tuple of at
  least 128 bytes (a lower bound: fields summed without padding), copied on
  every call. Generic types are not sized and not reported.
- `unused`: a private function, struct, enum, constant or type alias that
  nothing in the package names. Items with an attribute other than
  `@noalloc`, `@nopanic`, `@noblock`, `@realtime`, `@envelope` or `@inline`
  (they may be entry points), trait implementations, names starting with
  `_` and `main` are left alone. A method is private unless marked `public`,
  and method calls check visibility, so a non-public method that nothing in
  its package calls can be called from nowhere.
- `extern-assertion`: an `extern "C"` function trusted to be `@noalloc` or
  `@noblock` (or `@realtime`, or by its `@envelope`) whose C library
  namesake is documented to allocate or block (`malloc`, `printf`, `sleep`,
  `read`, `pthread_mutex_lock`, ...).
- `extern-gc-reference`: an `extern "C"` function that takes or returns a
  `class`; C code that keeps the pointer holds a reference the collector
  does not see.
- `asm`: an `@asm` variant marked `pure: true` that has no output (the
  optimiser may delete every call) or clobbers memory.
- `unproven-division`: an integer `/` or `%` whose divisor the compiler did
  not prove safe, so it is checked at run time and can panic. It is the same
  decision the code generators follow; a `requires` that proves the divisor
  non-zero (and, signed, rules out `MIN / -1`) removes the check and the
  finding.
- `noalloc-candidate` (off by default, `--warn noalloc-candidate`): a public
  function the analysis proves never allocates that is not declared
  `@noalloc`.

Findings are warnings: the command fails only for a lint named with
`--deny <lint>` (`--deny all` for every one). To silence a lint: `--allow
<lint>`; a comment `// codira-lint: allow(<lint>, ...)` on the reported line
or the line above it; `//! codira-lint: allow(<lint>)` for the whole file;
`allow(all)` for every lint. `--path <dir>` lints a directory of sources,
such as `std/`, instead of a package.

Limits: `unused` works by name, so an item shadowed by another of the same
name anywhere in the package counts as used, and it does not see uses from
other packages (hence only private items). `large-copy` reports parameters,
not other copies. `extern-assertion` knows a fixed list of C library
functions, not every one. Lints run on what parses; `codira check` reports
syntax errors.

### Benchmarks: `codira bench`

```codira
// benches/work.code
import root.work

@bench
public func work_once() -> i64 { work(10) }        // called once per iteration

@bench
public func work_looped(n: i64) -> i64 {           // runs `n` iterations itself
    var acc = 0
    var i = 0
    while i < n { acc += work(10 + (i & 1)); i += 1 }
    acc
}
```

`codira bench` builds the package with every `.code` file under `benches/`
(mounted under a `benches` module, as `codira test` mounts `tests/`), at
`-O3` (`-O` to change), and times every `public` `@bench` function in
`src/` or `benches/`. `codira bench <text>` runs only the benchmarks whose
name contains the text. A benchmark returns `()`, `bool`, an integer or a
float, which the runner receives so the work is not optimised away. With no
parameters it is called once per iteration, and every iteration includes a
call through the runtime; the run starts by measuring that call overhead on
an empty benchmark. With one `i64` parameter it runs that many iterations
itself, so the overhead is paid once per sample. That is the form for
anything that takes less than a few microseconds. Keep its loop body from
being folded to a constant (vary an input with the iteration number, as
above).

Each benchmark runs in its own processes: `--rounds` of them (3), each
warming up for `--warm-up-ms` (300), sizing its samples from the warm-up,
then taking its share of `--samples` (50) over its share of
`--measurement-ms` (1000). The report per benchmark: the median time per
iteration, the median absolute deviation, a 95% confidence interval for the
median (from order statistics, no distribution assumed), the mean without
severe outliers, the outliers outside Tukey's fences, and the spread of the
round medians.

`--save-baseline <name>` writes the samples to
`target/bench/baselines/<name>.json`. `--baseline <name>` compares with
them. A benchmark is "REGRESSED" or "improved" only when the Mann-Whitney U
test rejects "no change" (p < 0.05) *and* its median moved by more than both
`--threshold` percent (2) and the spread between rounds of either run.
Samples within one process are not independent: frequency scaling, memory
layout and the machine's load move a whole process. A regression makes the
command fail, and so does a `@bench` that is not `public` or has another
signature.

Limits: times are wall-clock, on a machine that is not quiesced by the
runner (no CPU pinning, no frequency control), so compare baselines taken on
the same machine. The call overhead depends on how the `codira` binary
itself was built (a debug build of the compiler is slower). Allocation and
memory use are not measured.

### Supply chain: `codira audit`

`codira audit` reports what a package brings in besides its own Codira code
and what in it is trusted rather than checked:

- licence: `license` (an SPDX expression) and `license-file` from
  `[package]` in `codira.toml`, with the licence file's SHA-256;
- each `[native]` source with its language, size, SHA-256 and the
  `SPDX-License-Identifier` it declares in its first 40 lines; the include
  directories, defines and flags it is compiled with; and the system
  libraries linked by name;
- every `extern` declaration by ABI (`"C"`, `codira-intrinsic`, or none),
  and its unchecked trusted assertions: `@noalloc`, `@nopanic`, `@noblock`,
  `@realtime` and `@envelope(..)` on an extern, and each `@asm` variant
  (whose `pure: true` is an assertion too). The effect analysis takes each
  one at its word; this is the list a reviewer has to check by hand;
- the standard-library modules the package imports, directly or through
  other standard-library modules.

It fails when something the manifest names does not exist (a native source,
an include directory, the licence file). `--strict` also fails when the
package declares no licence or a native source declares no SPDX identifier;
without it those are notes. `--json` prints the report as JSON (schema
`codira-audit/1`). Nothing is built.

Limits: the SPDX identifier is what a file says, not a licence analysis of
its contents; system libraries named in `libs` and the symbols an `extern`
resolves to at link or load time are listed, not inspected; Codira packages
have no package dependencies today, so the standard library is the only
Codira code from outside the package.

---

## 5. End-to-end status today

Verified through the full pipeline (parser → HIR → LLVM → link → run):

- `struct`, `class`, `func`, `let`/`var`, tuples and tuple patterns
- field access, arithmetic, comparisons, `as` casts (integer division checked,
  shifts masked, float-to-integer casts saturating: see "Integer arithmetic,
  shifts and casts")
- `if`/`else`, `while`, `loop`, `for` over ranges and arrays, `break`,
  `continue`, short-circuit `&&`/`||`, function calls, methods via `extend`,
  `Type.assoc_fn()` static calls
- arrays: literals, bounds-checked indexing and element assignment, `len()`,
  `unchecked_get(i)` (arrays of tuples type-check but do not link yet: the
  runtime has no type info for tuple element types)
- growable arrays: `push`, `pop`, `insert`, `remove`, `clear`, `reserve`,
  `append`, `contains`, `index_of`, `sort`, `binary_search`, `reverse`,
  `copy`, slicing `xs[lo..hi]`, `map`/`filter`/`fold`/`any`/`all` with
  closures compiled in place, `sum`/`min`/`max`, `for (i, x) in
  xs.enumerate()`, through the GC heap, hot reload and heap snapshots (see
  "Growable arrays and collections"; tests:
  `crates/codira/tests/collections.rs`,
  `crates/codira_runtime/tests/growable_arrays.rs`)
- `array_filled(v, n)`, `select`, `fma`, `likely`/`unlikely` (see "Hot-loop
  builtins"; not supported by the Cranelift `--jit` backend, which refuses them)
- `str` literals, and `print`/`println` (`codira run` shows the output)
- `String`: construction (`to_string()`, the implicit conversion of a
  `str`, `string_from_utf8`), `+`, `+=`, comparisons, every method in
  "Strings", interpolated literals `f"..{x}.."`, strings in fields, arrays,
  optionals, parameters and results, across modules, to and from the host
  (`StringRef`), through collections, heap snapshots and hot reload
  (Cranelift `--jit` runs such programs on LLVM; see "Strings")
- optionals (`T?`, `nil`, `if let`, `while let`, postfix `!`, `== nil`) and
  `match` over literals, tuples, strings and optionals, checked exhaustive
- `enum` with payloads: construction, `match`/`if let`/`while let` with
  nested variant patterns (checked exhaustive), `==`, methods via `extend`,
  enums in fields, arrays, parameters and results, across modules and hot
  reload (see "Enums & pattern matching" for the limits); generic enums,
  monomorphised per instantiation, with all of the above and bounded
  `extend` blocks (see "Generic enums")
- `Result[T, E]` (`std.result.result`) and postfix `?` on `Result`s and
  optionals, wrapping an error in the caller's error enum or converting it
  through an `@error_conversion`; `std.error.error`'s `Error`, `AnyError`
  and `context`; a `main` returning a `Result` (see "Error handling")
- format specs in interpolation holes (`f"{x:>8.3}"`, checked at compile
  time) and the debug form of every type (`f"{x:?}"`, `f"{x:#?}"`,
  `x.debug_string()`), see "Strings"
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
  `std.collections.optional`, `std.result.result`, `std.fs.file`,
  `std.text.ascii`, `std.error.error`, `std.cmp.cmp`, and `Map`, `Set`, `Deque`,
  `BinaryHeap` and `Counter` in `std.collections` (see "Importing the standard library" for
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
  allocation; `Runtime::collect_blocking()` collects objects not reachable
  from host-rooted handles or from a thread running Codira code and returns
  a `CollectionReport` (objects and bytes reclaimed, the pause, whether it
  was coalesced); `Runtime::request_collection()` asks for one and returns
  at once (a thread of the collector's own serves it; `true` if this call
  queued it, `false` if one was pending).
- **Fairness:** a collection requested from outside (either call, or
  `codira_gc_collect`/`codira_gc_request_collection`) first waits, parked,
  until Codira code has run for the mutator window since the previous
  collection: 1 ms, or four times the previous pause if longer
  (`GcPolicy::min_mutator_window`, `pause_multiple`), skipped when no other
  thread is inside Codira code. A host collecting in a loop therefore stops
  the world for at most a fifth of the time. A request is satisfied by any
  collection that starts after it (paced, another thread's), so concurrent
  requests coalesce into one collection.
- **Pacing** is on in every runtime, `Runtime::builder` included:
  `GcPolicy::default()` collects once the heap's objects exceed twice what
  the previous collection kept, from 4 MiB; `RuntimeBuilder::gc_growth(None)`
  turns it off, `gc_policy` sets all of it. The check is made where an
  allocation takes the heap lock; the next allocation by Codira code (or by
  the host) collects.
- Collections stop the world (stack capture for x86-64 and AArch64 on
  Linux, macOS and Windows; tested on x86-64 Linux only). There are no stack maps, so a collection
  asks every thread running Codira code to park, scans each parked thread's
  stack and spilled callee-saved registers conservatively, collects, and
  resumes them. What is guaranteed:
  - **No live object is freed**, whichever thread collects and whatever
    other threads are doing: `collect_blocking` or `request_collection` from
    any thread, a paced collection or an allocation at the heap limit on any
    thread or task, `codira_gc_collect`. The same stop
    protects `save_heap`/`restore_heap` and the heap migration of a hot
    reload.
  - **Which threads count:** a host thread inside `Runtime::invoke`, every
    `spawn`ed task on the worker pool, and any thread holding
    `codira_runtime::enter_codira()` (C: between `codira_thread_enter` and
    `codira_thread_leave`).
  - **Where threads park:** every GC allocation is a safepoint. A thread
    waiting in a channel send or receive, a `select`, a scope join or
    `codira_time_sleep_ns` is already parked and never holds up a
    collection.
  - **Hot reload:** `Runtime::update` relinks with the world stopped, and
    only while no thread is inside Codira code. Otherwise it returns
    `false` and the next `update` retries, without needing a new file
    event.
  - **Cost:** an allocation in Codira code checks one flag; entering
    Codira code from the host costs about 14 ns, a channel operation about
    26 ns more; an allocation made by the host outside Codira code costs
    about 20 ns more than before (x86-64, measured).
- What is not guaranteed:
  - **Latency:** a collection waits for every running thread to reach a
    safepoint. A loop that neither allocates nor blocks delays it until the
    loop ends (there are no safepoints at loop back-edges). Calls to the
    file, database and tensor services are not safepoints; a collection
    waits for them to return.
  - **Raw function pointers:** a Codira function called through a
    `FunctionDefinition`'s `fn_ptr` (or `codira_function_fn_ptr`) without
    the guard is invisible to the collector, and a reload does not wait for
    it. Take the guard.
  - **Host functions called from Codira code** run as part of a running
    thread. One that waits for something only a collecting thread can
    release (a lock the collector holds, a join) deadlocks with it; wrap
    such waits in `codira_runtime::blocking`. The same applies to a host
    thread that holds the guard.
  - **Unrooted host handles:** a `StructRef` or `ArrayRef` the host keeps
    after `invoke` returns is not protected; root it, or hold the guard.
  - **Scope of a stop:** the registry is per process. A collection in one
    runtime pauses threads running another runtime's code, and a reload
    waits until no runtime's code runs.
  - **Unsupported targets:** where a thread's stack cannot be captured, a
    collection that would need it is refused (`collect_blocking` returns
    `CollectRefused`, and an allocation at the heap limit fails).
- `Runtime::builder(lib).heap_limit(bytes)` caps the GC heap (without it
  there is no cap; `codira_runtime::heap_policy::default_heap_limit` gives
  the `codira` tool's default, half of the available memory). An allocation
  that would exceed it first collects, with the world stopped as above, and
  retries before the other threads resume. If memory is still short, the
  `try_alloc`/`try_alloc_array`/`try_grow_array` API and
  `StringRef::try_new` return `AllocError::LimitExceeded { limit,
  committed, requested, live }`; an allocation by Codira code ends the
  process with the diagnostic shown in "Memory: heap limit, pacing,
  `--runtime-stats`" and exit status 102
  (`codira_runtime::HEAP_EXHAUSTED_EXIT_STATUS`), its last lines from
  `RuntimeBuilder::heap_limit_note`. A 256 KiB reserve is released first so
  reporting has memory. It caps the GC heap only, not raw allocations.
  Tests: `crates/codira_runtime/tests/gc_threads.rs` (tasks allocating and
  checking patterned objects under a small cap and under collections from a
  host thread), `heap_limit.rs` (pacing without a cap, forced failures).
- `Runtime::stats()` returns `RuntimeStats` (heap bytes now and peak, live
  bytes, bytes and objects allocated and reclaimed, collections by cause,
  time with the world stopped, time to safepoint, pause p50/p99/max); its
  `Display` is what `--runtime-stats` prints. Counting is always on: two
  thread-local counter updates per allocation.
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
all yet: generic traits, trait objects (dynamic dispatch),
`match` on a `String`, module-level `let` and `var` (rejected with "module-level `let`
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