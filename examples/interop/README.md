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

On POSIX add `-ldl` to the C build. Deno's `Deno.dlopen` works the same way as
`bun:ffi` if you prefer it; the signature declarations are identical in shape.

Each caller runs the same set of checks and exits non-zero on any mismatch, so
they double as tests rather than as demonstrations that merely print
something.

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
