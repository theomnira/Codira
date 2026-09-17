<!--
Copyright (c) 2026 Omnira CJSC
Author: Tunjay Akbarli

Functionality: Cross-language interoperability example for Codira.
-->

# Calling Codira from C, C++, Rust, Python and TypeScript

One Codira library, five callers, no per-language bridge code.

A function marked `@export("C")` is emitted under its own unmangled name and
survives into the built assembly's export table. That makes a `.codiralib` an
ordinary shared object as far as the rest of the world is concerned, so every
language that can call a C function can call Codira — through the mechanism it
already has, with no generated bindings and no build-time dependency on the
Codira toolchain.

```codira
@export("C")
public func codira_add(a: i64, b: i64) -> i64 {
    a + b
}
```

## Running it

Build the library once:

```sh
codira build --manifest-path examples/interop/codira/codira.toml
```

That produces `examples/interop/codira/target/mod.codiralib`. Pass its path to
each caller:

| Language   | Command                                                        | Mechanism |
|------------|----------------------------------------------------------------|-----------|
| C / C++    | `cc -o main main.c && ./main <lib>`                            | `dlopen` / `LoadLibrary` |
| Rust       | `cargo run -- <lib>`                                           | `libloading`, *and* hosting `codira_runtime` |
| Python     | `python main.py <lib>`                                         | `ctypes` (standard library) |
| TypeScript | `bun run main.ts <lib>`                                        | `bun:ffi` (built in) |
| TypeScript | `bun run bench.ts <lib>`                                       | `bun:ffi`, batch kernels |

On POSIX add `-ldl` to the C build. Deno's `Deno.dlopen` works the same way as
`bun:ffi` if you prefer it; the signature declarations are identical in shape.

Each caller runs the same set of checks and exits non-zero on any mismatch, so
they double as tests rather than as demonstrations that merely print
something.

## Let the compiler write the bindings

`main.ts` declares its signatures by hand, which is what every FFI tutorial
does and is also where the easy mistake lives. `codira bindgen --runtime bun`
emits them from the signatures the compiler actually resolved:

```
codira bindgen --runtime bun -o bindings.ts
```

The difference is not only typos. A Codira `i64` bound as `FFIType.i64` arrives
in JavaScript as a `BigInt`, and allocating one per call costs more than most
calls do: 37.97 ns/call against 12.35 ns for `i64_fast` and 7.02 ns for `u32`,
measured over 500,000 calls of the same function. `i64_fast` is not a
precision trade -- it returns a `number` inside the safe-integer range and a
`BigInt` outside it, so the full i64 range round-trips exactly. The generator
picks it; a person writing the obvious thing picks `i64`.

## Scalar calls have a floor, and it is not Codira's

Entering a native function from Bun costs about 6.4 ns whatever the function
does. `Math.clz32` costs 0.23 ns. So a Codira function called once per element
loses to JavaScript by roughly 30x, and no amount of work on the Codira side
changes that -- the boundary is the workload.

Crossing it once per *buffer* is what makes the difference, and it needs
Codira to read memory it did not allocate. `extern "codira-intrinsic"` blocks
declare operations the compiler emits inline -- a load becomes an `inttoptr`
and a `load`, with no call:

```codira
extern "codira-intrinsic" {
    func load_u32(addr: usize) -> u32;
    func store_u32(addr: usize, value: u32);
    func ctlz_u32(value: u32) -> u32;
}

@export("C")
public func codira_clz32_into(src: usize, dst: usize, count: usize) -> usize {
    let mut i: usize = 0;
    while i < count {
        store_u32(dst + i * 4, ctlz_u32(load_u32(src + i * 4)));
        i = i + 1;
    }
    return count;
}
```

From TypeScript that is one call, with `ptr()` supplying the addresses:

```ts
symbols.codira_clz32_into(ptr(source), ptr(destination), source.length);
```

Measured over 500,000 elements by `bench.ts`:

| | total | per element |
|---|---|---|
| TypeScript `Math.clz32`, loop in JS | 0.112 ms | 0.224 ns |
| Codira, one call per element | 3.457 ms | 6.913 ns |
| Codira, one call per buffer | 0.092 ms | 0.184 ns |

`bench.ts` checks its own results -- every clz32 value against `Math.clz32`,
the affine kernel against a JavaScript reference, and the pairwise sum against
Kahan summation -- so a kernel that gets faster by getting wrong fails.

There is no safety in these intrinsics by construction: `load_u32(addr)` reads
whatever is at `addr`. That is the same contract C has, which is why they are
spelled `extern`.

## What crosses the boundary

Everything in the exported API is plain C scalars, and that is deliberate.
Inside, the implementations use ordinary Codira — private functions, structs,
methods, tuple returns and destructuring:

```codira
@export("C")
public func codira_scaled_length_squared(x: f64, y: f64, k: f64) -> f64 {
    let v = Vec2 { x: x, y: y }.scaled(k)
    v.dot(v)
}
```

The `Vec2` never crosses the ABI; it is built and consumed inside. Same for
tuples — `divmod` returns `(i64, i64)`, and the exported entry points project
the parts, because an anonymous LLVM struct is not a portable C type and
pretending otherwise would break the moment a caller's ABI differed.

## The one limitation worth knowing before you design an API

A Codira function that calls an `extern "C"` symbol, or a function in another
module, dispatches through the **Codira runtime's** table. The runtime
populates that table when it loads an assembly, which is what resolves
`extern` symbols in the first place.

So such a function works when the library is loaded by the Codira runtime —
`codira start`, or a Rust host using `codira_runtime` — but **not** when it is
`dlopen`ed directly by C, Python or TypeScript. At that point nothing has
populated the dispatch table, and the call goes through a null pointer.

`codira_hypot` in the example is exactly this case: it calls `extern "C" sqrt`.
The Rust caller reaches it (it hosts the runtime); the others do not, and the
example does not pretend they can.

The practical rule: **keep an exported C API self-contained.** Calls to
private functions in the same module are compiled as direct calls and are
completely fine — `codira_sum_of_squares`, `codira_length_squared` and
`codira_div` all call other Codira code and all work from every caller.

## The other direction

`extern "C"` declares a symbol resolved from the host process at load time, so
Codira can call out to C:

```codira
extern "C" {
    func sqrt(x: f64) -> f64;
}
```

On Unix this resolves against the process itself, which already includes libc.
On Windows the C runtime lives in a separate DLL, so the runtime additionally
searches `ucrtbase.dll`, `msvcrt.dll` and `kernel32.dll`. Either way there is
no declaration file to generate and no linking step to configure.

## C++

`extern "C++"` parses and lowers identically to `extern "C"` today.
`@export("C++")` is deliberately *not* treated as an export: C++ has no stable
name mangling across compilers, so emitting the symbol under its plain Codira
name would produce something no C++ caller could link against. Until linkage-
name metadata is carried through (`spec/LANGUAGE_SPEC.md` §10), call C++ from
Codira the way C++ itself expects — through an `extern "C"` shim.
