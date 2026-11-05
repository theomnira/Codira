# Standard library architecture

The target layout of Codira's standard library: its layers, one name for
each thing, the conventions every module follows, and the order in which
modules can be built. [STATUS.md](STATUS.md) says what exists today;
[ROADMAP.md](ROADMAP.md) turns this page into milestones.

The aim is a library that is more coherent and easier to use than the
best of Rust, C++, Swift, Go, Zig, Java, Kotlin, C#, Python, TypeScript,
OCaml, Haskell, D and Julia, and as safe and as fast as the best of them.
"Coherent" is the part most within reach and the part most standard
libraries lose over time: one name per concept, one error model, one way
to say "may allocate", one way to reach the operating system.

---

## 1. Where we start

Today `std/` has 89 files. 17 can be imported by a program; 6 more check
clean only when `std/` is checked as one package with its prelude; 66
have errors of their own (see STATUS.md for each). Reading every file
gives this picture:

- **Two generations of code.** The importable modules (`std.fs.fs`,
  `std.crypto.hash`, `std.time.time`, `std.math.*`, `std.bit.*`,
  `std.memory.region`, `std.random.rng`, ...) are recent, small, honest
  about their limits and tested end to end. Most of the rest is older:
  written against a design of the language that did not happen (a
  borrow checker, destructors, `&T` references, a `Pointer[T]` the
  compiler would understand), or ported in shape from Mojo's standard
  library (`DType`, `SIMD[T, N]`, `StringLiteral`, `OwnedPointer`,
  `ArcPointer`, `InlineArray`, the `gpu` tree).
- **No error model.** Fallible functions return negative `i64` codes
  (`std.fs.fs`, `std.db.db`), `bool`, an empty array (`std.base64`), a
  `struct OsResult[T]`, or a `Result[T, Error]` that does not exist yet
  (`std.sys.arg`).
- **No OS layer beyond files.** `std.os`, `std.pathlib`, `std.sys.arg` and
  `std.format` call `_platform_*` functions no runtime provides. The
  runtime offers files, hashes, clocks, termination, raw memory, the
  database and tensors, and nothing else (STATUS.md, "Runtime services
  today").
- **Many duplicates** (section 5): two path libraries, two hash traits
  with the same name and different methods, two `Representable` traits,
  three places that define `exit`, two random generators, two sets of
  integer limits, math functions in three modules.
- **A systematic bug class**: `&` written for `&&` between comparisons in
  `sort`, `range`, `tuple`, `error` and `alloc` (STATUS.md, Defects).

Three pieces of language work are in progress: generic enums with
`Result[T, E]` and `?`; a `String` type; and growable arrays with
iteration (`map`, `filter`, ...) plus `Map`, `Set`, `Deque` and
`BinaryHeap`. This architecture is designed *for* them: section 10 says
what each one unblocks. Nothing below works around their absence.

---

## 2. What developers actually need

A standard library is judged by what programmers reach for in their
first week. Two download rankings say what that is when a language's
standard library does not provide it.

**crates.io, all-time downloads (API, 2026-11-05).** Rust's standard
library is deliberately small; the most-downloaded crates show what fills
the gap (rank among all crates; build-time crates like `syn`, `quote`,
`cc`, `libc`, `windows-sys` omitted):

| rank | crate | downloads | what it says Codira's `std` needs |
|---:|---|---:|---|
| 1 | hashbrown | 2.98 B | a fast hash map (Rust's own `HashMap` has been hashbrown since Rust 1.36) |
| 3, 5, 6, 21 | getrandom, rand_core, rand, rand_chacha | 2.50 B, 2.12 B, 2.10 B, 1.62 B | random numbers *and* OS entropy in `std` |
| 4 | bitflags | 2.20 B | bit-flag sets |
| 9 | base64 | 1.90 B | encodings |
| 12 | indexmap | 1.83 B | an insertion-ordered map |
| 13 | itertools | 1.78 B | iterator adapters beyond the basics |
| 15 | thiserror | 1.78 B | concise error types (derive) |
| 17, 34, 35 | memchr, regex, aho-corasick | 1.69 B, 1.35 B, 1.34 B | fast byte search and linear-time regex |
| 19, 23, 24 | serde, serde_derive, serde_json | 1.65 B, 1.57 B, 1.56 B | serialization with derive; JSON |
| 22, 38 | itoa, ryu | 1.59 B, 1.30 B | fast number formatting |
| 27 | once_cell | 1.48 B | lazy one-time initialisation |
| 28 | log | 1.48 B | a logging facade |
| 32 | smallvec | 1.41 B | small-buffer vectors |
| 37 | clap | 1.31 B | command-line parsing |
| 43 | parking_lot | 1.24 B | better locks |
| 45 | bytes | 1.24 B | byte buffers |
| 51 | http | 1.21 B | HTTP types |
| 56 | num-traits | 1.20 B | generic numeric traits |
| 58 | sha2 | 1.18 B | cryptographic hashes |
| — | tokio, anyhow, toml, time, url, tracing, uuid, chrono, reqwest, flate2, num-bigint | 1.15 B, 1.13 B, 1.10 B, 1.05 B, 1.03 B, 1.01 B, 0.96 B, 0.95 B, 0.88 B, 0.82 B, 0.72 B | async I/O, any-error with context, TOML, dates and time zones, URLs, tracing, UUIDs, HTTP client, compression, big integers |

**PyPI, downloads in the 30 days to 2026-11-01** (Python's standard
library already has JSON, regex, logging, argparse, datetime, hashlib,
random, secrets, unittest, sqlite3): the most-downloaded packages are
still `requests` (#8, 1.44 B), `cryptography` (#9, 1.32 B), `pyyaml`
(#12), `python-dateutil` (#15), `pydantic` (#18, validation and
serialization), `click` (#20, CLI), `numpy` (#21), `anyio` (#23,
structured concurrency), `pytest` (#25), `attrs` (#29, derive-like
classes), `httpx` (#31) and `protobuf` (#34). Even a "batteries
included" library leaves an HTTP client, real cryptography, better
dates, declarative data classes and a better test runner to packages.

**What the other languages ship.** Go's standard library includes an
HTTP client and server, TLS, JSON, regexp (RE2, linear time), crypto,
`flag`, `testing` with benchmarks and fuzzing, and `log/slog`; it is the
strongest evidence that one well-chosen set removes most ecosystem
splits. Java added `java.time` (JSR-310) after `Date` and `Calendar`
proved unfixable, and an HTTP client in Java 11. C++ standardised
`<regex>` and `unordered_map` with interfaces that fix a slow
implementation in place for ABI reasons. Zig passes allocators
explicitly to everything that allocates. Swift's `String` hides its
indices behind an opaque type that makes simple slicing hard. Python 3's
split between `str` and `bytes` was worth the migration. Rust's lean
standard library produced two incompatible async runtimes.

**Conclusions this architecture adopts:**

1. Ship the things every program needs in `std`, Go-style: collections
   with a DoS-resistant default hash, iterators, strings, formatting,
   errors with context, random numbers with OS entropy, encodings, JSON
   and TOML, a linear-time regex, dates with time zones, CLI arguments,
   logging, testing and benchmarks, an HTTP client, cryptographic
   primitives over audited implementations, compression.
2. Make derive (compile-time generation of equality, hashing, debug
   printing and serialization) a language priority: `serde_derive` and
   `thiserror` are top-25 crates for a reason.
3. One concurrency model. Codira already has structured concurrency
   (`scope`, `spawn`, channels) and algebraic effects; `std` builds on
   them and does not add `async`/`await` (section 3, "advanced").
4. Never freeze an implementation behind an interface that forbids
   improving it (C++'s lesson): no layout or iteration-order promises for
   hash maps; algorithms (sort, hash, formatting) may change.
5. Allocation is visible (Zig's lesson), without Zig's verbosity: the
   effect system already knows which functions allocate (section 7).
6. Byte-indexed UTF-8 strings with explicit character iteration, not
   Swift's opaque indices; text (`String`) and bytes (`[u8]`) are
   different types, as in Python 3 and Rust.

Sources: [crates.io API, crates by downloads](https://crates.io/api/v1/crates?sort=downloads&per_page=60)
(and `/api/v1/crates/<name>` for each crate in the last row), retrieved
2026-11-05; [top PyPI packages](https://hugovk.github.io/top-pypi-packages/),
data of 2026-11-01; [Go standard library](https://pkg.go.dev/std);
[Python standard library](https://docs.python.org/3/library/index.html);
[Rust `HashMap`](https://doc.rust-lang.org/std/collections/struct.HashMap.html);
[java.time](https://docs.oracle.com/en/java/javase/21/docs/api/java.base/java/time/package-summary.html);
[Zig standard library](https://ziglang.org/documentation/master/std/).

---

## 3. Layers

Five API layers. A module belongs to exactly one, and may import only
modules of its own layer or a lower one. The layer says what a module may
assume, not when it is built (that is ROADMAP.md).

| layer | contains | may allocate | may block | may use runtime services |
|---|---|---|---|---|
| **core** | primitives' extensions, `str` operations, slices, `T?` helpers, `Result`, panic and assertions, numbers, bits, math, raw memory and pointers | **no** (checked: `@noalloc`) | no | only `codira_write_stderr`/`codira_abort` (panic) and libm |
| **fundamental** | collections, iterators, `String`, comparison, operators, conversion, hashing, formatting, allocation, random generators, errors | yes, visibly | no | `codira_aligned_*`, region, page |
| **application** | I/O, files, paths, processes, environment, time, threads, locks, atomics, networking, HTTP, data formats, regex, text, encodings, crypto, compression | yes | yes | yes |
| **advanced** | task and channel helpers, streams, serialization framework, binary formats, FFI, logging and tracing, metrics, testing, benchmarks | yes | yes | yes |
| **domains** | database, tensors, SIMD, GPU, units, probability | yes | yes | yes |

### core

Owner's list: primitives, strings, slices, optional, result, panic,
contracts. Contracts (`requires`, `ensures`, refinement types) are
language, not library; core's functions *carry* them where Z3 can use
them (`requires d != 0` on a division helper removes its check).
"Strings" in core means the borrowed `str` and byte-slice operations
that need no allocation; the owned, growable `String` allocates and is
fundamental.

Modules: `std.panic`, `std.option`, `std.result`, `std.str`,
`std.slice`, `std.num`, `std.bit`, `std.math`, `std.mem`, `std.ptr`.

### fundamental

Owner's list: collections, iter, ops, convert, cmp, hash, fmt, math, mem,
alloc, ptr, range. `math`, `mem` and `ptr` move down to core because they
need no allocation and restricted environments need them. `range` is not
a module: ranges are language syntax (`for i in 0..n`), and a range
*type* belongs with `std.ops` if the language exposes one. Added:
`std.string`, `std.random` (generators; OS entropy is a runtime service
used through it) and `std.error`.

Modules: `std.collections`, `std.iter`, `std.string`, `std.cmp`,
`std.ops`, `std.convert`, `std.hash`, `std.fmt`, `std.alloc`,
`std.random`, `std.error`.

### application

Owner's list, all kept: io, fs, path, process, env, time, thread, sync,
atomic, net, http, json, toml, regex, text, encoding, crypto, compress.
Added: `std.time.date` (civil time and time zones; chrono and time are
both near a billion downloads) and `std.cli` (clap is a top-40 crate;
Go and Python ship `flag` and `argparse`).

### advanced

Owner's list: async, concurrent, channel, actor, task, stream,
reflection, serde, binary, ffi, diagnostics, logging, tracing, metrics,
testing, benchmark. Decisions:

- **`async`: not built.** Codira has structured concurrency (`scope`,
  `spawn`, bounded channels, `select`, session types) and one-shot
  algebraic effects. A blocking call inside a spawned task occupies only
  that task's worker thread. Adding `async` functions would split every API in
  two (Rust's and C#'s "function coloring") for a benefit the effect
  system can give without the split: a scheduler can be an effect
  handler. If many-thousands-of-connections servers need it later, the
  path is an I/O effect handled by an event loop, not a new function
  kind.
- **`concurrent`: merged into `std.sync`** (one module for locks,
  `Once`, barriers and concurrent containers).
- **`actor`: not a module.** An actor is a task that owns a channel; the
  pattern is documented in `std.task`.
- **`channel`, `task`, `stream`: thin modules** over the language's
  channels and `scope`/`spawn` (join handles with results, timeouts,
  fan-in/fan-out, iterating a channel).
- **`reflection`: not now.** The language has no reflection; derive
  covers equality, hashing, printing and serialization, which is what
  reflection is mostly used for.
- **`logging` and `tracing`: one module, `std.log`,** with levels,
  structured fields and spans (Rust needed two crates, `log` and
  `tracing`, because the first could not grow spans).
- **`diagnostics`: part of `std.log` and `std.panic`** (backtraces and
  panic hooks are what it would hold).
- Kept: `std.serde`, `std.binary`, `std.ffi`, `std.metrics`,
  `std.testing`, `std.bench`.

### domains

Owner's list: db, ai, tensor, gpu, simd, quantum, probability, image,
units, science. Decisions:

- **`db`**: `std.db` (today `std.db.db`, an unused import away from
  importable). Scans and typed tables when the runtime exposes them.
- **`ai` and `tensor`: one module, `std.tensor`.** Today `std.ai.runtime`
  (runtime-owned tensors behind handles) and `std.ai.tensor`/`linalg`/`nn`
  (Codira kernels over caller memory) are two unrelated tensor types.
  One `Tensor` API, with views over caller memory as a second type in the
  same module. Neural-network layers (`std.tensor.nn`) sit on top.
- **`simd`: `std.simd`**, portable fixed-width vectors, once the language
  has const generics and vector types (today's `std.builtin.simd` needs
  `__simd_storage[T, N]`).
- **`gpu`: frozen.** There is no kernel launch syntax, and the six files
  have 15 to 736 errors each. Nothing is fixed until launches exist; then
  the module is redesigned against what the compiler emits.
- **`units`: `std.units`,** conversions and named units on top of the
  language's `unit(..)` types (the language has dimensions but no
  conversions).
- **`probability`: `std.probability`,** distributions and statistics.
- **`quantum`, `image`, `science`: not in `std`.** `codira quantum`
  already compiles quantum kernels; image codecs are large and
  security-sensitive and belong in packages; "science" is too broad to
  specify, and `std.math`, `std.tensor` and `std.probability` are its
  base.

---

## 4. Today's files in the target layout

Every file of `std/` and where it goes. "Keep" means the module moves
under its new name with its tests; "rewrite" means its API is kept as a
guide but the code is replaced; "retire" means it is deleted.

| today | target | layer | action |
|---|---|---|---|
| `sys/terminate.code` | `std.panic` (panic, assert, unreachable, todo); exit/abort to `std.process` | core | keep |
| `sys/assert_compare.code` | `std.panic` (`assert_eq`, `assert_ne`, ...) | core | keep, after operators through traits |
| `builtin/type_aliases.code` | `std.panic` (`Never`) | core | keep |
| `collections/optional.code` | `std.option` (helpers on `T?`) | core | retire the struct; keep the function names |
| `builtin/error.code` | `std.error` | fundamental | rewrite |
| `builtin/slice.code`, `collections/span.code` | `std.slice` | core | rewrite on the language's array views |
| `builtin/sort.code` | `std.slice` (sort, stable_sort, select_nth, binary_search) | core (stable sort: fundamental) | rewrite; fix the defects in STATUS.md |
| `builtin/swap.code` | `std.mem` (`swap`, `replace`, `take`) | core | rewrite without `Pointer` |
| `builtin/range.code` | `std.ops` (`Range` type), if the language exposes one | fundamental | retire unless needed |
| `builtin/int.code`, `builtin/bool.code`, `builtin/tuple.code` | — (methods on the primitives via `extend`) | core | retire |
| `builtin/types.code` | `std.num` (limits); `std.mem` (`size_of`) | core | retire the aliases |
| `builtin/dtype.code` | `std.tensor` (element type tag) | domains | rewrite, small |
| `builtin/simd.code` | `std.simd` | domains | rewrite after const generics |
| `builtin/string_literal.code` | — (`str` is the literal type) | — | retire |
| `builtin/traits.code` | `std.cmp`, `std.hash`, `std.fmt`, `std.iter` | fundamental | split |
| `math/math.code`, `math/constants.code`, `math/fast.code` | `std.math`, `std.math.fast`; integer limits to `std.num` | core | keep; fix defects |
| `math/sequence.code`, `math/polynomial.code` | `std.math` | core (allocating parts: fundamental) | keep, on `Array` |
| `bit/bit.code`, `bit/mask.code` | `std.bit` | core | keep |
| `complex/complex.code`, `complex/complex_string.code` | `std.math.complex` (+ `Display`) | core | keep; fix defects |
| `memory/memory.code` | `std.mem` | core | keep |
| `memory/pointer.code`, `memory/unsafe_pointer.code` | `std.ptr` (one raw `Ptr[T]`) | core | merge |
| `memory/owned_pointer.code`, `memory/arc_pointer.code` | — | — | retire: the GC and classes give shared ownership; unique ownership needs destructors and move-only types the language does not plan |
| `memory/region.code`, `memory/page.code`, `memory/alloc.code` | `std.alloc` | fundamental | keep region and page; rewrite alloc |
| `collections/list.code`, `collections/array.code` | `std.collections` (`Array`, the growable array in progress); `InlineArray` after const generics | fundamental | replace |
| `collections/string.code` | `std.string` (the `String` in progress) | fundamental | replace |
| `collections/dict.code`, `collections/set.code`, `collections/deque.code`, `collections/binary_heap.code` | `std.collections` (`Map`, `Set`, `Deque`, `BinaryHeap` in progress) | fundamental | replace |
| `collections/bitset.code` | `std.collections` (`BitSet`) | fundamental | rewrite on `Array[u64]` |
| `collections/counter.code` | `std.collections` (`Map` helpers: `count_by`) | fundamental | retire |
| `collections/linked_list.code` | — | — | retire (rarely the right structure; packages may provide it) |
| `hashlib/hash.code`, `hashlib/hasher.code`, `hashlib/string_hash.code` | `std.hash` | fundamental | merge |
| `format/format.code`, `format/repr.code` | `std.fmt` | fundamental | rewrite |
| `random/rng.code`, `random/random.code` | `std.random` | fundamental | merge on `rng.code` |
| `base64/base64.code`, `base64/base64_string.code` | `std.encoding.base64`, `std.encoding.hex` | application | keep; add strict decoding |
| `crypto/hash.code` | `std.crypto.hash` | application | keep |
| `fs/fs.code` | `std.fs` | application | keep the services; add `File` and `Result` |
| `os/os.code` | `std.fs` (directories) | application | rewrite on runtime services |
| `os/path.code`, `pathlib/path.code` | `std.path` | application | merge |
| `os/env.code`, `sys/arg.code` | `std.env` | application | rewrite on runtime services |
| `os/process.code` | `std.process` | application | rewrite |
| `sys/info.code` | `std.env.target` (OS, architecture), `std.thread` (core counts) | application | rewrite |
| `sys/intrinsics.code` | `std.intrinsics` (prefetch, gather/scatter, assume, trap only) | core (unsafe) | cut down |
| `time/time.code` | `std.time` | application | keep |
| `atomic/atomic.code` | `std.atomic` | application | keep, after module-level constants |
| `ffi/cstring.code` | `std.ffi` | advanced | keep |
| `ffi/unsafe_union.code` | — | — | retire (not a C union) |
| `testing/testing.code`, `testing/suite.code` | `std.testing` | advanced | rewrite for `codira test` |
| `db/db.code` | `std.db` | domains | keep |
| `ai/runtime.code`, `ai/tensor/*`, `ai/linalg/*`, `ai/nn/*`, `ai/algorithm/*`, `ai/quantize.code` | `std.tensor`, `std.tensor.nn` | domains | merge |
| `gpu/*.code` | `std.gpu` | domains | frozen |
| `prelude.code` | `std.prelude` | — | rewrite last, with the final names |

---

## 5. Duplicates and conflicts

Each line is one concept with more than one home today. The right-hand
name is the only one that survives.

| concept | today | one name |
|---|---|---|
| process exit and abort | `std.sys.terminate` (`exit`, `abort`), `std.os.process` (`exit`, `abort`) | `std.process.exit`, `std.process.abort` |
| panic and assertions | `std.sys.terminate`, `std.sys.assert_compare`, `std.testing.testing` (`assert_true`, `assert_equal`, ...) | `std.panic` (`panic`, `assert`, `assert_eq`, ...); `std.testing` adds only test-specific helpers |
| command-line arguments | `std.sys.arg` (`args`, `argv`), `std.os.process` (`args`) | `std.env.args` |
| environment variables | `std.os.env`, `std.sys.arg` (`env_get`) | `std.env.var`, `std.env.vars` |
| paths | `std.os.path` (string functions), `std.pathlib.path` (`Path` class) | `std.path` (`Path` and the same operations as functions) |
| filesystem | `std.fs.fs`, `std.os.os` (`listdir`, `mkdir`), `std.pathlib.path` (`read_text`, `mkdir`) | `std.fs` |
| non-cryptographic hashing | `std.hashlib.hash` (`Hashable.hash_value`), `std.builtin.traits` (`Hashable.hash(hasher)`, a different trait of the same name), `std.hashlib.hasher` | `std.hash` (`Hash` trait feeding a `Hasher`; `hash_bytes` etc.) |
| cryptographic hashing | `std.crypto.hash` | `std.crypto.hash` (unchanged); `std.hashlib` is renamed so the two are never confused |
| formatting | `std.format.format`, `std.format.repr` (`Representable`), `std.builtin.traits` (`Stringable`, `Representable`, `Formattable`) | `std.fmt` (`Display`, `Debug`) |
| errors | `std.builtin.error` (`Error`), `std.os.os` (`OsError`, `OsResult[T]`), `std.testing.testing` (`AssertionError`), negative codes in `std.fs.fs`/`std.db.db` | `std.error.Error` trait; one error enum per module (`fs.Error`); `Result[T, E]` everywhere |
| random numbers | `std.random.rng` (explicit `Rng`), `std.random.random` (global, different seeding) | `std.random` on the `Rng` of `rng.code`; a default generator seeded from OS entropy |
| integer limits | `std.math.constants` (`I8_MAX()`), `std.builtin.types` (`INT8_MAX()`) | `std.num` (`i8_max()` until associated constants, then `i8.MAX`) |
| power-of-two helpers | `std.math.math` (`is_power_of_two`, `next_power_of_two_u64`), `std.bit.bit` (`is_power_of_two`, `bit_ceil`) | `std.bit` |
| math functions | `std.math.math`, `std.sys.intrinsics` (`sqrt`, `fma`, `floor`, ...), `std.complex.complex` (its own `extern` `sqrt`) | `std.math` |
| atomics | `std.atomic.atomic`, `std.sys.intrinsics` (`atomic_load`, ...) | `std.atomic` |
| bit intrinsics | `std.bit.bit`, `std.sys.intrinsics` (`ctlz`, `ctpop`, ...) | `std.bit` (compiled to the instructions) |
| page size | `std.memory.page` (`page_size`), `std.sys.info` (`page_size`) | `std.alloc.page_size` |
| type sizes | `std.builtin.types` (`type_sizeof`), `std.sys.info` (`sizeof`) | `std.mem.size_of[T]()` |
| pointers | `std.memory.pointer`, `std.memory.unsafe_pointer`, `owned_pointer`, `arc_pointer` | `std.ptr.Ptr[T]` (raw, unsafe) |
| views | `std.builtin.slice` (`Slice[T]`), `std.collections.span` (`Span[T]`) | the language's array views, with `std.slice` functions |
| growable sequence | `std.collections.list` (`List[T]`, 124 uses), `Array[T]` (108 uses, never defined), the growable array in progress | one type; its library name is `Array[T]` and `List` is retired |
| strings | `std.collections.string` (`String` over `Pointer[u8]`), `std.builtin.string_literal` (`StringLiteral`), the `String` in progress | `std.string.String` (owned) and `str` (literal and borrowed) |
| sorting | `std.builtin.sort`, `std.collections.list` (`sort`) | `std.slice.sort`; collections call it |
| clocks | `std.time.time` (`perf_counter`, `perf_counter_ns`, `instant_now`) | `std.time.Instant` |
| tensors | `std.ai.runtime` (handles), `std.ai.tensor` (views) | `std.tensor` |

### Migration plan

Only the 19 importable modules have users; every other module can move
or disappear without a compatibility step.

1. **New name first.** Create the new module (`std/panic/mod.code` is
   `std.panic`; the import ratchet already maps `mod.code` to its
   directory). For a rename, it is the old file's content moved; the old
   file becomes a re-export (`public import root.panic.*;`) with a header
   saying "deprecated: use `std.panic`".
2. **Both import.** The import ratchet must list both names as importable
   for one release, and `crates/codira/tests/std_imports.rs` runs the
   new name. A re-export that is not importable is a regression the
   ratchet catches.
3. **Release notes** list every rename with its old and new path.
4. **One release later** the re-export is deleted; `codira api-diff`
   reports it as a major change of `std`.
5. For the 72 modules that do not import, the move is direct: no shim.

Concretely, for the importable modules:

| today | new | notes |
|---|---|---|
| `std.sys.terminate` | `std.panic` + `std.process` | `exit`, `exit_success`, `exit_failure`, `abort` go to `std.process` |
| `std.builtin.type_aliases` | `std.panic` | `Never` |
| `std.collections.optional` | `std.option` | the struct is deprecated in favour of `T?` |
| `std.hashlib.hash` | `std.hash` | |
| `std.base64.base64` | `std.encoding.base64`, `std.encoding.hex` | base32 stays with base64 |
| `std.random.rng` | `std.random` | |
| `std.memory.region`, `std.memory.page` | `std.alloc` | |
| `std.fs.fs` | `std.fs` | the stutter goes |
| `std.time.time` | `std.time` | the stutter goes |
| `std.bit.bit`, `std.bit.mask` | `std.bit` | one module |
| `std.math.math`, `std.math.constants` | `std.math` | constants become `public let` when supported; limits go to `std.num` |
| `std.math.fast` | `std.math.fast` | unchanged |
| `std.complex.complex` | `std.math.complex` | |
| `std.crypto.hash` | `std.crypto.hash` | unchanged |

---

## 6. Naming and API conventions

### Module names

- `std.<area>`, one segment where one suffices (`std.fs`, not
  `std.fs.fs`): a file `std/<area>/mod.code`, with sub-modules for large
  areas (`std.crypto.hash`, `std.encoding.base64`, `std.time.date`).
- Lower-case singular nouns (`std.collection` would be wrong only because
  `collections` is the universal spelling; it is the one plural).
- A module never repeats its name in its items: `std.fs.File`, not
  `std.fs.FsFile`; `std.random.Rng.next_u64`, not `rng_next_u64`.

### Types, functions, constants

- Types `UpperCamelCase`; functions, methods, fields and modules
  `snake_case`; constants `SCREAMING_SNAKE_CASE` (`public let PI: f64`
  once module-level constants exist; until then `public func PI() -> f64`).
- **Methods, not prefixed free functions.** `Type.assoc_fn()` and
  `value.method()` work today, so `std` no longer writes
  `duration_add(a, b)` or `region_alloc(r, n, 8)`:

  ```codira
  // today
  let d = duration_add(duration_from_secs(1), duration_from_millis(750))
  let p = region_alloc(r, 64, 8)
  // target
  let d = Duration.from_secs(1).add(Duration.from_millis(750))   // `+` once operator traits exist
  let p = r.alloc(64, 8)
  ```

- **Constructors:** `T.new(..)` for the obvious one; `T.with_capacity(n)`;
  `T.from_<source>(..)` for infallible conversions; `T.parse(text) ->
  Result[T, ParseError]` and `T.try_from(..) -> Result[T, E]` for
  fallible ones; `T.default()` through a `Default` trait.
- **Queries** are nouns or `is_`/`has_` predicates: `len()`, `is_empty()`,
  `capacity()`, `contains(x)`. **Conversions** are `to_<x>` (copy,
  maybe allocate), `as_<x>` (view, free), `into_<x>` (consume).
- **Overloads by suffix** (`abs_f32`, `count_leading_zeros_u32`) are a
  stop-gap for the lack of methods on primitives in `std`. The target is
  methods through traits `std` declares (`extend f32: Float { func
  abs(self) -> f32 .. }`; the package that declares a trait may implement
  it for the primitive types), so `x.abs()`, `n.leading_zeros()`.

### Absence, failure and panics

- **Absence is `T?`.** `map.get(k) -> V?`, `s.find(needle) -> usize?`,
  `v.first() -> T?`. No sentinel `-1`, no placeholder payloads
  (`none(0)`), no `isize` index results.
- **Fallible operations return `Result[T, E]`** and are used with `?`:

  ```codira
  func load_config(path: str) -> Result[Config, fs.Error] {
      let text = fs.read_to_string(path)?
      Config.parse(text).map_err(fs.Error.invalid_data)
  }
  ```

  A function returns `Result` when the failure is a property of the
  input or the environment (a missing file, malformed text, a full
  disk); it panics only when the caller broke a documented precondition
  (an index out of bounds, `unwrap` of `nil`). Every panic is listed in
  the function's documentation under "Panics".
- **A non-panicking twin** exists for every panicking accessor:
  `v[i]` / `v.get(i) -> T?`; `x / y` / `x.checked_div(y) -> T?`.
- **Integer overflow:** operators wrap (that is the language). `std.num`
  provides `checked_*` (`T?`), `wrapping_*`, `saturating_*` and
  `overflowing_*` (`(T, bool)`) for every integer type, and every `std`
  function whose result can overflow says which rule it follows.

### Errors

- One **error enum per module** that fails (`fs.Error`, `json.ParseError`,
  `env.VarError`), with variants a caller can match on; never a bare
  string or a number in a public signature.
- Each implements the shared **`std.error.Error` trait**
  (`message(self) -> String`, `source(self) -> E?` once trait objects
  allow it). `std.error.AnyError` is a box for any `Error` with a
  context chain, for applications (`anyhow`'s role); libraries return
  their own enums (`thiserror`'s role, which derive makes cheap).
- OS errors keep the raw code (`fs.Error.os(code: i32)`) as today's
  `fs_last_os_error` does, so nothing the OS said is lost.

### Strings and indices

- `String` and `str` are UTF-8. **Indices are byte offsets**, as in Rust
  and Go. `len()` is the byte length; `chars()` iterates Unicode scalar
  values; `char_count()` is explicit because it is O(n).
- Slicing at a byte offset that is not a character boundary returns
  `nil` from `get(a, b) -> str?` and panics from the indexing form.
- Bytes are `[u8]` / `Array[u8]`, never `String`. Functions that take
  text from outside (`fs.read_to_string`, `env.var`) validate UTF-8 and
  fail with an error, never produce an invalid `String`.

### Allocation visibility

The effect analysis already computes, for every function, whether it can
allocate, block or panic. `std` makes that visible and usable:

- Every `std` function that does not allocate is **annotated
  `@noalloc`** (checked by the compiler), and `@nopanic`/`@noblock`
  where true. A function that allocates says so in its first doc line
  ("Allocates a new `String`").
- Where a result needs memory, a **non-allocating form writes into a
  caller buffer**: `encoding.base64.encode_into(src, dst) -> usize`
  beside `encode(src) -> Array[u8]`; `fmt.write_int(buf, n)` beside
  `n.to_string()`. The raw `(address, length)` APIs of `std.fs.fs` and
  `std.crypto.hash` are the existing model.
- An **envelope ratchet** for `std`: `codira envelope --json` over each
  layer, snapshotted, so that a core function that starts allocating (a
  key widening) fails CI the way the import ratchet does.
- Two current limits, which section 9 lists as compiler work: a call to a
  function of another module that takes or returns a value struct
  allocates (the cross-module convention boxes it), so `Region`,
  `Duration` and `Complex` cannot be passed to `std` from `@noalloc`
  code; and a trait method called through a bound counts as every
  effect, so generic `std` code cannot promise `@noalloc` until the
  analysis runs per instance.

### Runtime services

Everything that touches the OS is a runtime service with one shape,
the one `std/fs/fs.code` set:

1. The runtime exports `codira_<area>_<op>` (C ABI), registered in the
   runtime's dispatch table (`crates/codira_runtime/src/native_*.rs`),
   taking scalars and `(address, length)` pairs and returning `i64`/`i32`
   (a value >= 0, or a documented negative code). Handles are
   generation-checked and kind-tagged, never OS descriptors. Nothing
   returns a GC object across the C ABI.
2. `std` declares them in an `extern "C"` block, private to the module,
   and wraps them: typed handles (`struct File { _handle: i64 }`),
   `Result` with the module's error enum, `str`/`String` for paths.
3. Effect assertions on an extern (`@noalloc @nopanic @noblock`) are
   made only when true of the runtime's implementation, with the reason
   written beside them, as `std/crypto/hash.code` does.
4. Every service has a known-answer test in the runtime crate and an end
   to end test through the `std` wrapper; the platforms it has run on
   are in STATUS.md.
5. `_platform_*` declarations that nothing provides are not allowed:
   a wrapper is added together with its service.

### Documentation

Each module starts with: what it is for; what it is **not** (not
cryptographic, not DoS-resistant, not constant time, not serializable,
not tested on macOS); the effects of its functions. Each function: one
line, then "Panics", "Errors", "Allocates" where they apply. Claims of
accuracy or complexity are tested or not made.

---

## 7. Restricted environments

**Core must work without the GC heap** (no `class`, no array literal, no
closure creation, no `print`), so that kernels, interrupt handlers,
`@realtime` code and the inner loops of `std` itself can use it:

| core module | without the heap | notes |
|---|---|---|
| `std.panic` | yes | writes through `codira_write_stderr`; a panic message is a `str` |
| `std.option`, `std.result` | yes | value types |
| `std.num`, `std.bit` | yes | pure |
| `std.math` | yes | libm externs need a C library; `std.math.fast` needs nothing |
| `std.str` | yes | views over existing bytes |
| `std.slice` | yes, except `stable_sort` | `stable_sort` takes a scratch buffer argument in its `@noalloc` form |
| `std.mem`, `std.ptr` | yes | raw memory |

**Fundamental may allocate, visibly.** `std.alloc` is the escape hatch
for code that must bound its memory: a `Region` with a quota fails at the
boundary instead of growing the process. Collections take their memory
from the GC heap; an allocator parameter (Zig-style) is not planned
until the language has a way to make it zero-cost when unused.

**Application and above** assume the runtime: services, blocking, the
heap.

What breaks the promise today: see "Allocation visibility" above (value
structs across modules, effects through bounds). Until those are fixed,
core functions take and return scalars, `str` and `T?` only.

---

## 8. Dependency order

What each target module needs from the language, and from other
modules. Language features in **bold** do not exist yet; "in progress"
marks the three being built now. A module can be built when everything
in its row exists.

| module | language | modules |
|---|---|---|
| `std.panic` | — (`assert_eq`: **operators through traits**) | — |
| `std.num` | **module-level constants** (or niladic functions meanwhile), Result (in progress) for `parse` | `std.panic` |
| `std.bit` | — | — |
| `std.math` | — (constants: **module-level constants**) | `std.panic` |
| `std.mem` | **sizeof/alignof intrinsics** | — |
| `std.ptr` | — | `std.mem` |
| `std.option` | generic enums (in progress) for combinators | `std.panic` |
| `std.result` | generic enums, `?` (in progress) | `std.panic` |
| `std.str` | **borrowed slices of `str`** | `std.option` |
| `std.slice` | growable arrays and views (in progress); **operators through traits** for `sort[T: Ord]` | `std.cmp` (for the bounded forms) |
| `std.cmp` | generic enums (`Ordering`); **operators through traits** | — |
| `std.ops` | **operators through traits** | — |
| `std.convert` | **generic traits** (`From[T]`) | — |
| `std.iter` | **generic traits with associated types** (`Iterator.Item`); closures in generic code (work today, capture by value) | `std.option` |
| `std.hash` | **generic traits** (`Hasher`), **in-place parameters** or a `class` hasher | `std.random` (per-process key) |
| `std.string` | String (in progress) | `std.str`, `std.collections` |
| `std.collections` | growable arrays, Map, Set, Deque, BinaryHeap (in progress); **generic traits** for `Hash`/`Eq` keys | `std.hash`, `std.cmp`, `std.iter` |
| `std.fmt` | String (in progress); **generic traits**; **derive** for `Debug` | `std.string` |
| `std.error` | generic enums (in progress); **trait objects** for `AnyError` | `std.fmt` |
| `std.alloc` | — | `std.mem` |
| `std.random` | — (OS entropy: a **runtime service**) | `std.bit` |
| `std.io` | Result (in progress); **generic traits** or **trait objects** for `Reader`/`Writer` | `std.error`, `std.collections` |
| `std.fs` | Result, String (in progress); **deterministic cleanup** (`defer` or destructors) to close handles | `std.io`, `std.path` |
| `std.path` | String (in progress) | `std.string` |
| `std.env` | String (in progress); **runtime services** (args, vars, cwd) | `std.string`, `std.error` |
| `std.process` | String, Result (in progress); **runtime services** (spawn, wait, pipes) | `std.io`, `std.env` |
| `std.time` | — | `std.num` |
| `std.time.date` | String, Result (in progress) | `std.time`, `std.fmt` |
| `std.atomic` | **module-level constants**, **atomic intrinsics** | — |
| `std.sync` | **runtime services** (locks that park for the collector); classes | `std.atomic` |
| `std.thread` | **runtime services** | `std.sync` |
| `std.text` | String (in progress); **module-level constants** (Unicode tables) | `std.string` |
| `std.encoding` | Result, growable arrays (in progress) | `std.error` |
| `std.json`, `std.toml` | generic enums, String, Map (in progress); **derive** for typed decoding | `std.collections`, `std.fmt`, `std.serde` |
| `std.regex` | String, growable arrays (in progress) | `std.text`, `std.collections` |
| `std.crypto` | **secret types** (exist); **runtime services** over audited crates | `std.encoding` |
| `std.compress` | **runtime services** | `std.io` |
| `std.net` | **runtime services** (sockets that park for the collector) | `std.io` |
| `std.http` | String, Result (in progress) | `std.net`, `std.io`, `std.crypto` |
| `std.cli` | String (in progress); **derive** (optional) | `std.env`, `std.fmt` |
| `std.task`, `std.channel`, `std.stream` | **richer task payloads** (strings, arrays, classes across `spawn` and channels) | `std.iter`, `std.time` |
| `std.serde` | **derive**, **generic traits** | `std.fmt`, `std.collections` |
| `std.binary` | Result, growable arrays (in progress) | `std.io` |
| `std.ffi` | String (in progress) | `std.mem`, `std.ptr` |
| `std.log` | String (in progress); **trait objects** (sinks) | `std.fmt`, `std.time`, `std.sync` |
| `std.metrics` | — | `std.atomic` |
| `std.testing` | String (in progress); **operators through traits** | `std.panic`, `std.fmt` |
| `std.bench` | closures (exist) | `std.time` |
| `std.db` | — (scans: **runtime services**) | `std.error` (once it returns `Result`) |
| `std.tensor` | **module-level constants** | `std.alloc` |
| `std.simd` | **const generics**, **vector types** | — |
| `std.units` | **unit conversions** in the language | — |
| `std.probability` | — | `std.random`, `std.math` |

---

## 9. What the language and compiler owe the library

In priority order, each with what it unblocks. ROADMAP.md schedules
them.

1. **Generic enums, `Result[T, E]`, `?`** (in progress): the error model
   of every fallible module (`io`, `fs`, `env`, `process`, `json`,
   `encoding`, `num.parse`), `Option`-style combinators, `Ordering`.
2. **`String`** (in progress): `fmt`, `path`, `env`, `process`, `text`,
   `json`, `toml`, `regex`, `testing`, `log`, `cli`, and the `*_string`
   halves of `base64`, `hashlib`, `complex`.
3. **Growable arrays, iteration, Map/Set/Deque/BinaryHeap** (in
   progress): `collections`, `slice`, everything that returns a
   variable amount of data (`fs.read_to_end`, `encoding`, `random.bytes`).
4. **Module-level constants** (`public let`), evaluated at compile time:
   the first error of 19 of the 72 blocked modules, 17 of them in their
   own file (`atomic` orderings, `ai`, `gpu`, `bitset`, `dict`, `set`,
   `os.path`, `sys.info`, `base64_string`), and the reason
   `std.math.constants` is spelled `PI()` instead of `PI`.
5. **Generic traits and associated types** (`Iterator` with `Item`,
   `Hasher`, `From[T]`, `Index[I]`): `iter`, `convert`, `hash`, `fmt`,
   `collections` with arbitrary keys, `serde`.
6. **Operators through traits on type parameters** (`a == b`, `a < b`,
   `a + b` on a `T`): `cmp`, `ops`, `assert_eq`, generic `sort`, numeric
   generic code, `Duration` and `Complex` with `+`.
7. **In-place parameters** (`mut`/`inout`, rejected today) or mutating
   methods on value types: `Hasher.write`, `atomic`, in-place `shuffle`
   and `sort` on value buffers, buffered readers, without moving state
   into a `class`.
8. **Deterministic cleanup** (`defer`, or destructors on a resource
   type): closing `fs` handles, `db` transactions, tensors and regions on
   every path, including `?` returns. Today every handle leaks unless
   closed by hand.
9. **Trait objects** (dynamic dispatch): `io.Reader`/`Writer` passed
   around, `error.AnyError`, `log` sinks, plugins.
10. **Derive** (compile-time generation of `Eq`, `Hash`, `Debug`,
    `Clone`, `Default`, `Serialize`/`Deserialize`): `serde`, `json`,
    `toml`, `fmt`, `cli`, error enums.
11. **Cross-module value structs without boxing** (a compiler change):
    lets `@noalloc` code pass `Region`, `Duration`, `Complex`, `Span` to
    `std`.
12. **Effects per instance** (the analysis run after monomorphisation):
    lets generic `std` code (`sort[T]`, `Map[K, V].get`) be `@noalloc`.
13. **Richer task and channel payloads** (strings, arrays, classes across
    `spawn` and channels): `task`, `channel`, `stream`, `net` servers.
14. **Const generics** (`N: usize` parameters with values): `InlineArray`,
    `simd`, fixed-size crypto blocks, `BitSet` with inline storage.
15. **`f16`/`bf16` and `sizeof`/`alignof` intrinsics**: `builtin.types`,
    `mem.size_of`, tensors.
16. **Borrowed slices of `str` and arrays** (views without copying):
    `str`, `slice`, zero-copy parsing in `json` and `regex`.
17. **Unit conversions** (`km` to `m`): `units`.
18. **Async**: not requested (section 3).

Two things are deliberately *not* asked for: module-level mutable state
(a global RNG or logger is better as explicit state plus a one-time
initialiser, which needs only constants and `std.sync.Once`), and
reflection (derive covers its uses).

---

## 10. What the work in progress unblocks

**Generic enums, `Result[T, E]`, `?`.** Every module that fails can
return `Result`: the wrappers over runtime services (`fs`, `db`,
`tensor`) change from negative codes to typed errors without changing
the services; `std.option` and `std.result` get combinators; `std.cmp`
gets `Ordering`; `std.collections.optional` is retired. Unblocks
`std.error`, `std.io`, `std.encoding` (strict decoding that can fail),
`std.num.parse`, `std.json`.

**`String`.** The largest single unblocking: `std.format.*`, `std.os.*`,
`std.pathlib`, `std.testing.*`, `std.sys.info`, `std.builtin.error` and
the three `*_string` modules have `String` as their first error. With it
come `std.fmt` (`to_string`, `Display`), `std.path`, `std.env`,
`std.testing`, `std.log`. The engineer's `String` replaces
`std.collections.string` and `std.builtin.string_literal`; the old files
are deleted, not ported.

**Growable arrays with iteration; Map, Set, Deque, BinaryHeap.** These
replace `std.collections.list`, `dict`, `set`, `deque`, `binary_heap`
(and the defects listed for them in STATUS.md) and remove the reason for
`std.memory.pointer` to exist: no collection needs a `Pointer[T]` once
the language has growable storage. Unblocks `std.math.sequence`,
`std.math.polynomial`, `std.collections.bitset`, `counter` (as `Map`
helpers), `std.random` (bytes, shuffle, sample), `std.slice`, and every
API that returns a variable amount of data. Requirements from this
architecture for the engineers' types: the default hash for `Map`/`Set`
is keyed per process (DoS-resistant); iteration order of `Map` is
unspecified; every accessor that panics has a `T?` twin; `with_capacity`
and `reserve` exist so allocation can be done up front.
