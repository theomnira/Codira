# Codira 26.11 release notes

Build of November 5, 2026: 154 commits since 26.10 (`4d27456303`), including merge commits.

26.11 works in two directions. It makes the language Codira describes and the
language a program can use closer to the same thing: traits, standard-library
imports, defined integer semantics. It also turns guarantees the compiler
used to state into ones it checks: envelopes, a machine-code constant-time
check, and garbage collection that is safe while other threads run.

Everything under the headings before "Experimental" is on `main` and covered
by tests. Experimental and opt-in work, known limitations and the platform
picture are listed plainly at the end. Read them before deploying: **Codira is
not yet suitable for production or mission-critical use.** The
[Status for production use](#status-for-production-use) section says why,
item by item.

The workspace version in `Cargo.toml` is still `0.6.0-dev`; "26.11" is the
release label used by the README.

## Highlights

- **Traits and bounds.** `trait`, `extend Type: Trait`, bounds such as
  `[T: A + B]`, default methods and supertraits. Calls are statically
  dispatched per instantiation.
- **Programs can import the standard library.** `import std.crypto.hash`
  and the like, for the 17 of 89 modules that check clean on their own.
- **Defined integer arithmetic, on both back ends.**
  - Integer division and remainder by zero, and signed `MIN / -1`, panic
    with the source location. They used to be undefined behaviour.
  - Shift amounts are taken modulo the width.
  - Float-to-integer casts saturate, and NaN converts to 0.
- **Garbage collection is safe while other threads run Codira code.**
  Collections stop the world: every thread running Codira code parks at a
  safepoint, and its stack is scanned. Hot reload relinks with the world
  stopped, and only when no thread is inside Codira code.
- **Semantic envelopes.**
  - `@envelope(allocation: none, foreign: none, ...)` on a function, and an
    `[envelope]` table in `codira.toml`, are checked over the whole call
    graph.
  - Envelopes are recorded in proof certificates, and `codira api-diff`
    treats a widened envelope as a major change.
- **A machine-code constant-time check.** `codira build --check-constant-time`
  checks the machine code emitted for `oblivious` and `@constant_time`
  functions, on x86-64 and AArch64. It rejects secret-dependent branches,
  variable-time instructions, calls to unchecked code, and (on x86-64)
  secret-dependent addresses.
- **Native storage and tensor engines** (experimental):
  - `codira_db`: a write-ahead log, snapshot-isolation transactions and
    checkpoints.
  - `codira_ai`: tensors, reverse-mode autodiff, layers and optimisers.

  Programs reach both through runtime services.

## Language

- **Traits.** Supported: required and default methods, supertraits,
  associated functions such as `func make() -> Self`, and implementations
  for structs, classes, enums, generic types, primitives and `str`. Each
  implementation is checked against its trait, and a call whose type does
  not meet a bound is an error that names the type, the trait and the
  parameter. Generic traits, trait objects, associated types and operators
  on a type parameter are rejected with an error.
- **Functions returning `never`** compile and run on both back ends. In
  particular, `panic` can be called.
- **Bindings.** These are now errors rather than being accepted without
  effect:
  - assigning to a `let`, including compound assignment and assigning to a
    field of a value struct held in one;
  - `mut`/`inout`/`out` parameters (arguments are passed by copy);
  - module-level `let`/`var`.

  Class fields and array elements stay assignable through a `let`.
- **Integer arithmetic, shifts and casts** are defined; see
  `agents/USING_CODIRA.md`, "Integer arithmetic, shifts and casts". A
  division check is left out only when the divisor is a positive literal or
  the verifier proves it safe.
  - **Measured cost:** within run-to-run noise (2%) on division-heavy loops
    on x86-64 at `-O2`.
  - **Shift masking:** costs no instruction at 32 and 64 bits.
- **`print`/`println`** evaluate all arguments left to right before printing
  anything. Floats print in their own precision (`1.0`, `0.1`, `NaN`, `inf`).
- **Arrays can be assigned** (`a = b`, `rows[i] = row`), which used to crash
  the code generator.
- **`select` and session types**, listed as not in 26.10, are on `main`.

## Standard library

- **Importable modules:**
  - `std.crypto.hash` (SHA-256, SHA-512, BLAKE3, via the RustCrypto `sha2`
    and `blake3` crates);
  - `std.fs.fs`;
  - `std.memory.region` and `std.memory.page`;
  - `std.bit.bit` and `std.bit.mask`;
  - `std.math.constants`, `std.math.fast` and `std.math.math`;
  - `std.hashlib.hash` (FNV-1a, not cryptographic);
  - `std.base64.base64` (RFC 4648 base64, base32 and base16);
  - `std.random.rng` (xoshiro256\*\*, not cryptographic);
  - `std.time.time`, `std.sys.terminate`, `std.complex.complex` and
    `std.collections.optional`;
  - `std.builtin.type_aliases`.
- **Testing.** An importability test records, for every std module, whether
  a program importing it checks, builds and loads, and why not if it does
  not.
- **`std.math.fast` now meets its stated error bounds.** Measured against
  libm, it was off by up to 0.39 where it claimed 1.7e-4.
- **New runtime services:**
  - files (`codira_fs_*`) and hashes (`codira_hash_*`);
  - time (`codira_time_*`) and process termination hooks;
  - a database (`codira_db_*`) and tensors (`codira_ai_*`).

  All of them use generation-checked handles and negative error codes, and
  are wrapped by `std/fs`, `std/crypto`, `std/db` and `std/ai/runtime.code`.

## Verification and tooling

- **Semantic envelopes.**
  - Eight keys are checked: allocation, blocking, panic, termination,
    foreign code, concurrency, effects and secrecy.
  - Keys that cannot be checked yet are rejected.
  - `codira envelope --json` reports every function's inferred envelope.
  - Proof certificates are now `codira-certificate/2`, and `codira certify`
    re-derives the recorded envelopes.
- **`codira build --check-constant-time`** checks machine code. At `-O0` it
  found a real leak, now fixed: an 8-bit or `bool` `ct_select` compiled to
  a branch or a secret-dependent load, although its IR was branch-free.
- **`codira build --superoptimize`** (experimental, x86-64 ELF):
  - It replaces each function's straight-line integer tail with a cheaper
    sequence that Z3 proves equal, and re-proves the bytes it writes.
  - It refuses constant-time code.
- **Differential testing.** Seeded random programs run on LLVM `-O2`,
  LLVM `-O0` and Cranelift, and their outputs are compared. Every
  divergence it found is fixed and kept as a regression test:
  - **A wrong-code bug in the Eidos constant fold:** it dropped output.
  - **A Cranelift ABI bug that only an optimised build of the compiler
    showed:** bool and 8/16-bit values were not extended across calls.
  - **A nested-`if` crash at `-O0`.**
  - **Mismatches between Cranelift and LLVM:**
    - a crash on float-to-narrow-int casts;
    - argument evaluation order;
    - float printing.

## Runtime

- **Stop-the-world collection.**
  - **Guaranteed:** no live object is freed, whichever thread collects:
    `gc_collect`, an allocation at the heap limit, or `codira_gc_collect`.
    `save_heap`/`restore_heap` and hot-reload heap migration are protected
    the same way.
  - **Which threads count:** host threads inside `invoke`, spawned tasks,
    and threads holding `enter_codira()` (C: `codira_thread_enter` and
    `codira_thread_leave`).
  - **Measured cost:** entering Codira code from the host costs about
    14 ns.
  - **Not guaranteed:** see "What is not guaranteed" in
    `agents/USING_CODIRA.md`. A loop that neither allocates nor blocks
    delays a collection until it ends, and functions called through raw
    pointers without the guard are invisible to the collector.
- **`[native]` sources.**
  - Assembly functions are found in the library that declares them, so
    they link on Unix.
  - C++ static constructors and destructors run on ELF: the compiler emits
    `.init_array`/`.fini_array` and `__dso_handle`.
  - On macOS, C++ links against libc++.
- **macOS.**
  - Libraries are ad-hoc signed by LLD (`-adhoc_codesign`) instead of by a
    separate signing step.
  - Hot reload sees the compiler's rename.

## Native engines (experimental)

- **`codira_db`.**
  - **What it is:** a checksummed write-ahead log, MVCC snapshot isolation
    (first committer wins), atomic checkpoints, and typed tables with
    add-column migrations.
  - **Crash testing:** a crash-injection harness crashes a workload at every
    storage operation, about 2,200 simulated crashes. Recovery always gives
    exactly a prefix of the committed transactions.
  - **Limits:** snapshot isolation is not serializable; the data set must
    fit in memory; only one process may use a directory; the durability
    claim is for Unix only; it has never been through a real power loss.
- **`codira_ai`.**
  - **What it is:** strided tensors with broadcasting, a blocked matmul
    (bit-identical to its reference), tape-based reverse-mode autodiff
    checked against numerical gradients, attention and transformer layers,
    Adam/AdamW, INT8 weights and a BPE tokenizer.
  - **Limits:** single-threaded with no explicit SIMD. INT8 is slower than
    f32 (a recorded negative result). It has not been compared with other
    frameworks.

## Build, CI and supply chain

- **CI runs its tests again.** Its first job had been failing, so for many
  runs no test ran on any platform. Fixed in this release:
  - the `nextest` custom-harness protocol in Funis;
  - Windows' static system libraries;
  - LLD on every runner;
  - tests that assumed a Windows host or a newer toolchain;
  - the mdbook job removed, because the book is not in the repository.
- **`cargo audit`:** 38 advisories in 26.10's lockfile, 1 left.
  - **How:** semver-compatible updates, removing `apple-codesign` (which
    brought TLS, HTTP, zip, plist and an RSA crate with an unfixable
    advisory), trimming `object`'s features, and replacing `tempdir`.
  - **Left:** RUSTSEC-2025-0055 in `tracing-subscriber` 0.2, which arkworks
    0.5 uses for the zero-knowledge prover's constraint tracing.

## Experimental

The following are on `main`, opt-in, and not covered by any stability
promise:
- the native engines and their runtime services;
- `--superoptimize`;
- the array alias classes (`CODIRA_ARRAY_TBAA=1`);
- `--sanitize=memtag`;
- `codira_mcode` and its cost profiles (the AArch64 profile is transcribed,
  never measured);
- the GPU target, the quantum and circuit back ends, and Funis.

`--check-constant-time` and envelopes are new in this release and should be
treated as experimental until they have more use.

## Status for production use

Codira 26.11 is **not ready for production or mission-critical use**. What
stands in the way:

- **Platforms.**
  - Linux and Windows (CI run 98, commit `a980406deb`): 3,790 of 3,791
    tests pass on Linux and 2,188 of 2,189 on Windows. The only failure on
    each was a stale generated C header for the runtime's C API, which is
    regenerated in this release. The C++ runtime and doctest jobs pass on
    both.
  - macOS (same run): 3,493 of 3,517 tests pass; 24 fail.
    - **The stale header,** fixed as above.
    - **17 in the Funis linker:**
      - 13 Mach-O and iOS-simulator integration tests from its
        mold-derived suite;
      - its no-leak test;
      - 3 tests of its own test harness.
    - **6 in Codira:**
      - C inlining into Codira callers;
      - an FMA code-generation check;
      - two tuple zero-overhead IR checks;
      - the image-writer comparison;
      - the low-level memory intrinsics test.

    The C++ runtime job passes on macOS. The macOS doctest job failed while
    installing LLVM, before any test ran.
  - The repository's nightly format check fails: the code has not been
    reformatted to its `rustfmt.toml`.
- **Hardware never run:** ARM MTE, CHERI (targets are refused) and GPU
  kernel launches (there is no launch syntax).
- **Standard library:** 72 of 89 modules cannot be imported yet. They need
  `String`, `Pointer[T]`, generic traits, module-level constants or
  in-place parameters.
- **Language gaps:**
  - generic traits, trait objects and associated types;
  - generic enums;
  - `String`;
  - bindings from `if let`, `while let` and `match` are still assignable;
  - `consuming` is accepted without move checking;
  - stack overflow is not ruled out by `@nopanic` or `@realtime`.
- **Runtime:** hot reload has no update admission control. Collections can
  be delayed by loops that neither allocate nor block.
- **Engines:** see their limits above.
- **Review:** no part of the compiler or runtime has had an independent
  security review.

## Upgrading

- **Assigning to a `let`** is now an error; declare the binding with `var`.
- **Rejected forms:** `mut`/`inout`/`out` parameters and module-level
  `let`/`var` are now errors. Return the new value, use a class, or write a
  function that returns the constant.
- **Division:** a division that may divide by zero now panics instead of
  being undefined behaviour. If the divisor is known to be safe, a
  `requires` clause that proves it removes the check.
- **Shift behaviour changed:** a shift by the width or more now uses the
  amount modulo the width.
- **Certificates:** `codira-certificate/2` is written. Version 1 is still
  read and checked, without envelopes.
