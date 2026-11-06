# Standard library roadmap

Five milestones, ATLAS-1 to ATLAS-5, that take the standard library from
what [STATUS.md](STATUS.md) records to the layout of
[ARCHITECTURE.md](ARCHITECTURE.md). They are in dependency order: a
milestone's modules need only the language features and modules of
earlier milestones (or of their own, built in the order listed). Each
module has an **entry** criterion (what must exist before work starts)
and an **exit** criterion (what must be true for it to count as done),
and each is testable.

The order comes from the audit: what the 89 files need, what blocks them,
what developers reach for first (ARCHITECTURE.md, section 2), and what
the three pieces of language work in progress (generic enums with
`Result` and `?`; `String`; growable arrays with iteration and
`Map`/`Set`/`Deque`/`BinaryHeap`) make possible.

---

## Done means

Every module's exit criterion includes these, written once here:

- **D1 Importable** under its final name: listed as importable in the
  import ratchet snapshot, and `scripts/std_status.py --check` passes.
- **D2 Tested end to end**: a program in `crates/codira/tests/std_imports.rs`
  (or a test file beside it) calls every public function and compares
  with values computed independently: a reference implementation in the
  test (as the xoshiro256** and libm tests do), published test vectors,
  or a differential test against Rust's standard library over random
  inputs. Edge cases are part of it: empty input, zero, the minimum and
  maximum of each integer type, NaN, infinities, subnormals, non-ASCII
  text.
- **D3 Conventions** of ARCHITECTURE.md, section 6: methods rather than
  prefixed free functions; `T?` for absence; `Result[T, E]` with the
  module's error enum for failure; byte indices; no `_platform_*`
  declaration; no `&` between comparisons.
- **D4 Effects declared and checked**: `@noalloc`, `@nopanic`, `@noblock`
  on every function where they hold, verified by the compiler, and the
  module in the `std` envelope snapshot (ARCHITECTURE.md, "Allocation
  visibility"). Core modules: every public function `@noalloc`.
- **D5 Documented honestly**: what it is not (not cryptographic, not
  constant time, not tested on a platform), every panic, every
  allocation; any accuracy or complexity claim is tested by D2.
- **D6 CI**: the tests pass on the Linux, Windows and macOS CI runners, or
  STATUS.md says on which they do not and why.
- **D7 Measured** (for modules marked *perf*): a benchmark in `benches/`
  against the named reference, its result recorded in STATUS.md. The
  target is within 1.25x of the reference on that benchmark; a module that
  misses it is still done, with the number recorded, and a follow-up.

The old file a module replaces is deleted, or becomes a re-export, in the
same change (ARCHITECTURE.md, "Migration plan").

---

## ATLAS-1: foundation

**Goal:** what exists is coherent and correct, and the three pieces of
language work in progress are adopted by the library as they land.

**Milestone entry:** none; the first modules start now. Later modules of
this milestone wait for the in-progress features named in their entry.

**Milestone exit:** every module below meets D1 to D6; the 22 final
names below (`std.panic` to `std.fs`, counting `std.encoding.base64` and
`std.encoding.hex` separately and `std.ai.runtime` not at all)
importable; no `std` file declares `_platform_*`; the code defects of STATUS.md in these modules are fixed,
each with the test that would have caught it; `std.collections.list`,
`dict`, `set`, `deque`, `binary_heap`, `string` and
`std.builtin.string_literal` are deleted.

| # | module | entry | exit (beyond D1-D6) |
|---:|---|---|---|
| 1 | `std.panic` (from `std.sys.terminate`, `std.builtin.type_aliases`) | now | `panic`, `assert`, `assert_with`, `unreachable`, `todo`, `Never`; exit and abort moved to `std.process`; old paths re-export for one release. |
| 2 | `std.bit` (merges `bit.bit`, `bit.mask`) | now | Methods on `u8`..`u64` through a `std` trait or suffixed functions until then; every function tested at widths 0, 1, 63, 64 and for positions >= width. |
| 3 | `std.math`, `std.math.fast`, `std.math.complex` | now | The STATUS.md defects fixed: `tanh` is 1.0 for large x, `sinh`/`tanh` accurate near 0 (via `expm1`), `abs` clears the sign of -0.0 and NaN, `complex_norm` via `hypot`; every function within 2 ulp of Rust's libm at 10^4 random points per function plus edge cases; `fast_*` bounds tested over their whole stated domain. *perf* against Rust's `f64` methods. |
| 4 | `std.num` | now (niladic constants); Result for `parse` | Limits for every integer and float width (replacing both copies); `checked_`, `wrapping_`, `saturating_`, `overflowing_` add/sub/mul/div/neg/shl/pow for every integer width, tested against Rust's methods exhaustively for 8-bit types and at 10^5 random points for wider ones; `parse` for integers and floats returning `Result`, round-trip with `print`'s shortest float output. *perf*: float parsing against Rust's `str::parse::<f64>`. |
| 5 | `std.alloc` (from `std.memory.region`, `std.memory.page`) | now | `Region` with methods (`r.alloc(size, align)`), quota and reset tested; the cross-module boxing of `Region` (ARCHITECTURE.md, section 9, item 11) either fixed or documented on every function. |
| 6 | `std.time` | now | `Duration` and `Instant` with methods; `checked_add`/`saturating_add`; `sleep`; tested as today plus overflow edges. |
| 7 | `std.random` (from `std.random.rng`) | now; a runtime entropy service `codira_random_fill(addr, len) -> i32` (the workspace already depends on the `getrandom` crate) | `Rng.seeded(seed)`, `Rng.from_entropy()`; `range` by rejection (no modulo bias), `f64`, `bool`, `shuffle` and `choose` without bias; sequence for a seed identical on every platform (tested against the Rust reference); the bias defects of `random.random` do not reappear (chi-squared test on a small range). |
| 8 | `std.crypto.hash` | now | Unchanged API; incremental (streaming) hashing added when the runtime exposes it. |
| 9 | `std.db`, `std.ai.runtime` | now | Remove the unused `import root.builtin.types.*` that alone blocks both; then importable, run through `import std` (not pasted source) by `db_and_ai.rs`. |
| 10 | `std.option`, `std.result` | generic enums, `Result`, `?` landed | `map`, `and_then`, `or_else`, `unwrap_or_else`, `ok_or`, `map_err`, `?` interplay tested; `std.collections.optional` deprecated. |
| 11 | `std.error` | as 10 | The per-module error enum convention, `message()`; `fs`, `encoding`, `num` errors use it. (`AnyError` waits for trait objects: ATLAS-2.) |
| 12 | `std.encoding` (`base64`, `base32`, `hex`; from `std.base64.*`) | as 10, growable arrays | Strict decoding by default (rejects bytes outside the alphabet, misplaced or missing padding, non-zero unused bits) returning `Result`; lenient decoding as a named option; `encode_into` without allocation; the standard's test vectors and a round-trip property test over random bytes; the `QQ=A` defect fixed. *perf* against the `base64` crate. |
| 13 | `std.str`, `std.string` | `String` landed | UTF-8 validation (tested against Rust's `str::from_utf8` on random and adversarial bytes), byte-indexed `find`, `rfind`, `split`, `trim`, `replace`, `starts_with`, `to_lowercase` (ASCII first), `chars()`; `get(a, b) -> str?` at non-boundaries. *perf*: `find` against Rust's `str::find` (memchr-class speed). |
| 14 | `std.collections` (`Array`, `Map`, `Set`, `Deque`, `BinaryHeap`) | growable arrays and the four collections landed | Differential tests: random sequences of 10^5 operations compared with Rust's `Vec`, `HashMap`, `HashSet`, `VecDeque`, `BinaryHeap`; removal-heavy sequences (the tombstone defects of `dict.code`); `Map`'s default hash keyed per process; every panicking accessor has a `T?` twin. *perf* against Rust's `HashMap` (hashbrown) and `Vec`. |
| 15 | `std.hash` (from `std.hashlib.*`) | 14 (for its consumer); in-place parameters, or a `class` hasher | FNV-1a and a keyed default (SipHash-1-3 or an equivalent with a published analysis), checked against reference vectors; the duplicate `Hashable` traits gone. |
| 16 | `std.slice` (sort, `binary_search`, reverse, rotate, `select_nth`) | 14 | Sorting tested differentially against Rust's `sort_unstable` and `sort` (stability) on random, sorted, reversed and all-equal inputs of sizes 0 to 10^5; the four `&`-for-`&&` defects and the small-range `nth_element` defect do not reappear. *perf* against Rust's `sort_unstable`. |
| 17 | `std.fs` | 10, 13, 14 | `File` with methods, `fs.Error`, `read_to_end`, `read_to_string` (UTF-8 checked), `write_all`, `atomic_write` (temp, fsync, rename, sync dir); the raw `(address, length)` forms stay. |

---

## ATLAS-2: fundamental

**Goal:** the generic vocabulary (traits) every later module is written
in.

**Milestone entry:** generic traits with associated types; operators
through traits on type parameters; a decision on in-place parameters
(supported, or mutation through `class` receivers everywhere).

**Milestone exit:** every module below meets D1 to D6; `Map[K, V]` takes
any `K: Hash + Eq` defined by a program; `assert_eq` works on any
`T: Eq + Debug`; the prelude is rewritten with the final names (still
for checking `std` as a whole, not imported by programs).

| # | module | entry | exit (beyond D1-D6) |
|---:|---|---|---|
| 1 | `std.cmp` | milestone entry | `Eq`, `Ord`, `Ordering`, `min`, `max`, `clamp`, `max_by_key`; float total order (`total_cmp`) tested against IEEE 754 totalOrder. |
| 2 | `std.ops` | 1 | Operator traits the compiler resolves `+`, `-`, `*`, `/`, `==`, `<`, `[]` through; `Duration` and `Complex` use them. |
| 3 | `std.convert` | 1 | `From`, `Into`, `TryFrom` for every numeric pair, tested at the boundaries. |
| 4 | `std.iter` | 1; closures in generic code | `Iterator` with `Item`; `map`, `filter`, `filter_map`, `zip`, `enumerate`, `chain`, `take`, `skip`, `take_while`, `flat_map`, `fold`, `sum`, `count`, `any`, `all`, `find`, `position`, `min_by`, `collect` into `Array`/`Map`/`String`; adapters `@noalloc` (D4), checked by the per-instance effect analysis (ARCHITECTURE.md, section 9, item 12). *perf*: an adapter chain against the hand-written loop (target: equal). |
| 5 | `std.fmt` | 4, `String` | `Display`, `Debug`; integer and float formatting (shortest round-trip, fixed, exponent, width, fill, alignment, sign); `format` with positional arguments; `write_*` into caller buffers without allocation. Float output tested for round-trip on 10^6 random doubles. *perf* against Rust's `ryu`/`itoa`-backed formatting. |
| 6 | `std.hash` (traits) | 1 | `Hash` and `Hasher` traits; `Map`/`Set` over program-defined keys. |
| 7 | `std.error` (`AnyError`) | trait objects | `AnyError` with context chains (`anyhow`'s role); `?` converts module errors into it. |
| 8 | `std.panic` (`assert_eq`, `assert_ne`, `assert_lt`, `assert_le`) | 1, 5 | Failure messages print both values (from `std.sys.assert_compare`). |
| 9 | `std.mem`, `std.ptr` | `sizeof`/`alignof` intrinsics | `copy`, `fill`, `compare`, `swap`, `replace`, `take`, `size_of[T]`, `align_of[T]`; one raw `Ptr[T]`, documented unsafe; `std.memory.*` retired. |
| 10 | `std.collections` (rest) | 6 | `BitSet`, an insertion-ordered map (`indexmap`'s role), `BTreeMap`/`BTreeSet` (ordered), `SmallArray` when const generics land. Differential tests as in ATLAS-1. |
| 11 | `std.testing` | 8 | Assertions for `codira test` (`@test` functions), approximate float comparison, expected-panic tests through the per-test process isolation `codira test` already has. |

---

## ATLAS-3: application

**Goal:** a program can be a real command-line tool: read its arguments
and environment, read and write files and directories, spawn processes,
parse JSON and TOML, use regular expressions and dates, and use
cryptography and compression.

**Milestone entry:** deterministic cleanup (`defer` or destructors) so
handles are closed on every path; runtime services for stdin/stdout,
directories and metadata, environment, arguments, current directory,
process spawning, and locks that park for the collector.

**Milestone exit:** every module below meets D1 to D6; a sample
command-line program (a `grep`-like tool over a directory tree with JSON
output) is written using only `std` and runs on all three CI platforms.

| # | module | entry | exit (beyond D1-D6) |
|---:|---|---|---|
| 1 | `std.path` | ATLAS-1 `std.string` | One `Path`; join, parent, file name, extension, normalisation, per-platform separators and prefixes (Windows drive and UNC), tested against Rust's `std::path` on generated paths. |
| 2 | `std.io` | milestone entry; trait objects or generic traits | `Reader`, `Writer`, `BufReader`, `BufWriter`, `read_line`, stdin/stdout/stderr; short reads and writes handled. *perf*: buffered line reading against Rust's `BufRead::lines`. |
| 3 | `std.fs` (directories) | 1, 2 | `read_dir`, `create_dir_all`, `remove_dir_all`, `metadata`, `copy`, symlinks; replaces `std.os.os` and the file half of `std.pathlib`. |
| 4 | `std.env` | runtime services | `args`, `var`, `vars`, `set_var`, `current_dir`, OS and architecture constants (from `std.sys.info`); replaces `std.os.env`, `std.sys.arg`. |
| 5 | `std.process` | 2, 4 | `exit`, `abort`, `id`, `Command` (spawn, args, env, cwd, pipes, wait, exit status); replaces `std.os.process`. |
| 6 | `std.atomic` | module-level constants, atomic intrinsics | Load, store, swap, compare-exchange, fetch ops for every integer width with orderings; tested under contention across `spawn`ed tasks. |
| 7 | `std.sync` | 6; runtime lock services | `Mutex`, `RwLock`, `Once`, `Condvar`, `Barrier`; a lock waiting while the collector runs does not deadlock (tested as `gc_threads.rs` does). *perf* against `parking_lot`. |
| 8 | `std.thread` | 7 | Available parallelism, named threads beyond `spawn` if the runtime offers them, `sleep`. |
| 9 | `std.time.date` | ATLAS-2 `std.fmt` | Civil dates, UTC offsets, ISO 8601 (and its internet date-time profile) parse and format, time zones from the system database; tested against Rust's `chrono` or `time` output on random instants. |
| 10 | `std.text` | `String`, module-level constants | Unicode scalar properties, simple and full case mapping, normalisation (NFC/NFD), grapheme clusters, tested against the Unicode Consortium's test files. |
| 11 | `std.json` | ATLAS-2 `std.fmt`, `Map` | Parse to a value type and print; conformance with the JSON standard (the JSONTestSuite cases); depth and size limits. *perf* against `serde_json` on a value tree. Typed decoding waits for `std.serde` (ATLAS-4). |
| 12 | `std.toml` | as 11 | Parse and print TOML 1.0; the `toml-test` suite. |
| 13 | `std.regex` | 10 | Linear-time matching (automata, no backtracking), Unicode classes, captures, `find_iter`, `replace`; the RE2/`regex` crate test suites; a pathological pattern stays linear. *perf* against the `regex` crate. |
| 14 | `std.cli` | 4, ATLAS-2 `std.fmt` | Flags, options, positional arguments, subcommands, generated help; derive-based declaration when derive exists. |
| 15 | `std.crypto` | runtime services over RustCrypto (or equivalently audited) crates; secret types | HMAC, HKDF, SHA-3, an AEAD (AES-GCM and ChaCha20-Poly1305), a CSPRNG, constant-time comparison as an `@constant_time` function passing `--check-constant-time`; known-answer tests from the standards. No algorithm implemented in Codira source. |
| 16 | `std.compress` | runtime services; 2 | deflate, gzip, zlib, zstd streaming; output-size limits; round-trip tests and reference files. |

---

## ATLAS-4: advanced

**Goal:** networked services, structured observability, serialization of
program types, and testing beyond assertions.

**Milestone entry:** derive; richer task and channel payloads (strings,
arrays and classes across `spawn` and channels); runtime socket services
that park for the collector.

**Milestone exit:** every module below meets D1 to D6; a sample HTTP
service (JSON over HTTP, structured logs, metrics) is written using only
`std`, and its load test is recorded.

| # | module | entry | exit (beyond D1-D6) |
|---:|---|---|---|
| 1 | `std.serde` | derive | `Serialize`/`Deserialize` derived for structs and enums; `std.json` and `std.toml` decode into program types; round-trip property tests. *perf* against `serde_json` typed decoding. |
| 2 | `std.task`, `std.channel`, `std.stream` | milestone entry | Join handles with results, cancellation through `scope`, timeouts, fan-in/fan-out, iterating a channel; the actor pattern documented. |
| 3 | `std.net` | runtime sockets | TCP, UDP, DNS resolution; timeouts; a blocking call parks for the collector. |
| 4 | `std.http` | 3, ATLAS-3 `std.io`, `std.crypto` (TLS through the runtime) | HTTP/1.1 client and server, TLS verified by default, chunked encoding, limits on header and body size; tested against a reference server and client. *perf*: requests per second against a Rust `hyper` baseline on the same machine. |
| 5 | `std.log` | trait objects | Levels, structured fields, spans, pluggable sinks; no cost when a level is disabled (measured). |
| 6 | `std.metrics` | ATLAS-3 `std.atomic` | Counters, gauges, histograms; lock-free updates. |
| 7 | `std.binary` | ATLAS-3 `std.io` | Byte-order readers and writers, varints, bounds-checked. |
| 8 | `std.ffi` | `String` | C strings (from `std.ffi.cstring`), C type aliases, callbacks; `std.ffi.unsafe_union` deleted. |
| 9 | `std.testing` (properties) | 1 | Property tests with shrinking, integrated with `codira test` and the counterexample files `codira check` already writes; golden-file tests. |
| 10 | `std.bench` | ATLAS-1 `std.time` | Warm-up, iterations to a time budget, median and spread; used by D7. |

---

## ATLAS-5: domains

**Goal:** the domain libraries that make Codira distinctive, on the
foundation above.

**Milestone entry:** module-level constants (for `std.tensor`); const
generics and vector types (for `std.simd`); a kernel launch syntax (for
`std.gpu`); unit conversions (for `std.units`).

**Milestone exit:** every module below meets D1 to D6.

| # | module | entry | exit (beyond D1-D6) |
|---:|---|---|---|
| 1 | `std.db` | runtime exposes scans and tables | `Result`-based API, range scans, typed tables; the crash tests of `crates/codira_db` cover the `std` path. |
| 2 | `std.tensor`, `std.tensor.nn` (from `std.ai.*`) | module-level constants | One `Tensor` API over runtime-owned tensors and caller-memory views; the kernels of `std.ai.linalg`/`nn` checked against the runtime engine; explicit freeing replaced by deterministic cleanup. *perf* against the runtime engine. |
| 3 | `std.probability` | ATLAS-1 `std.random`, `std.math` | Distributions (uniform, normal, exponential, Poisson, binomial, ...), sampling and statistics, tested by goodness-of-fit. |
| 4 | `std.simd` (from `std.builtin.simd`) | const generics, vector types | Portable `Simd[T, N]` lowering to vector instructions, checked in the IR. |
| 5 | `std.units` | unit conversions in the language | Named SI units and conversions. |
| 6 | `std.gpu` | kernel launch syntax | Redesigned against what the compiler emits; until then frozen. |

---

## Not worth building

| proposed | why not |
|---|---|
| `std.async` | The language has structured concurrency and algebraic effects; `async` functions would split every API in two. A scheduler can be an effect handler if one is ever needed (ARCHITECTURE.md, section 3). |
| `std.actor` | A task owning a channel; documented as a pattern in `std.task`. |
| `std.concurrent` | Duplicates `std.sync`. |
| `std.reflection` | The language has no reflection, and derive covers equality, hashing, printing and serialization. Revisit if a concrete use remains. |
| `std.quantum` | Duplicates `codira quantum`. |
| `std.image`, `std.science` | Not standard-library material: large, security-sensitive (codecs) or unbounded in scope. Packages. |
| `std.range` as a module | Ranges are syntax; a `Range` type, if exposed, belongs in `std.ops`. |
| `std.memory.owned_pointer`, `std.memory.arc_pointer` | Unique ownership needs destructors and move-only types the language does not plan; shared ownership is what GC classes already give. |
| `std.collections.linked_list` | Rarely the right structure; the `Deque` covers queue uses. |
| `std.collections.counter` | A `Map` helper (`count_by`), not a type. |
| `std.builtin.int`, `bool`, `tuple`, `types` (aliases) | Wrappers and aliases of the primitives; methods belong on the primitives themselves. |
| `std.builtin.string_literal` | `str` is the literal type. |
| `std.builtin.dtype` | A runtime enum of element types duplicates the type system; only `std.tensor` needs a tag, a few lines. |
| `std.ffi.unsafe_union` | A byte array is not a C union. |
| `std.sys.intrinsics` (most of it) | Its math, atomic and bit functions duplicate `std.math`, `std.atomic` and `std.bit`; only prefetch, gather/scatter, `assume` and `trap` remain, as `std.intrinsics`. |
| `std.gpu` now | No launch syntax; 6 files with 15 to 736 errors each. Frozen until launches exist. |
| `InlineArray`, `SmallArray` before const generics | Cannot be expressed. |

---

## Language features the library needs

In priority order. "Needed by" is the earliest milestone that cannot
finish without it.

| # | feature | unblocks | needed by |
|---:|---|---|---|
| 1 | Generic enums, `Result[T, E]`, `?` (in progress) | `option`, `result`, `error`, `encoding`, `num.parse`, `fs`, and every fallible module after | ATLAS-1 |
| 2 | `String` (in progress) | `str`/`string`, `fmt`, `path`, `env`, `process`, `text`, `json`, `toml`, `regex`, `testing`, `log`, `cli`; the `*_string` modules | ATLAS-1 |
| 3 | Growable arrays with iteration; `Map`, `Set`, `Deque`, `BinaryHeap` (in progress) | `collections`, `slice`, `random` (bytes, shuffle), `math.sequence`/`polynomial`, every API returning variable-size data | ATLAS-1 |
| 4 | Module-level constants (`public let`, compile-time evaluated; rejected today) | the first error of 19 blocked modules: `atomic`, `ai`/`tensor`, `gpu`, `bitset`, `os.path`, `sys.info`, `base64_string`, `text` tables; `PI` instead of `PI()` | ATLAS-2 (`std.num` limits as `i8.MAX`); ATLAS-3 (`atomic`, `text`) |
| 5 | Generic traits and associated types (rejected today) | `iter`, `convert`, `hash`, `fmt`, `collections` over program-defined keys, `serde`, `io` | ATLAS-2 |
| 6 | Operators through traits on type parameters (rejected today) | `cmp`, `ops`, `assert_eq`, generic `sort`, numeric generic code, `+` on `Duration` and `Complex` | ATLAS-2 |
| 7 | In-place parameters (`mut`/`inout`, rejected today), or mutating methods on value types | `hash` writers, `atomic`, in-place `shuffle`/`sort`, buffered readers without a `class` | ATLAS-2 (decision), ATLAS-3 |
| 8 | Deterministic cleanup (`defer` or destructors) | closing `fs` handles, `db` transactions, tensors, regions on every path including `?` | ATLAS-3 |
| 9 | Trait objects (dynamic dispatch) | `io.Reader`/`Writer` as values, `error.AnyError`, `log` sinks | ATLAS-2 (`AnyError`), ATLAS-3 |
| 10 | Derive (compile-time generation of `Eq`, `Hash`, `Debug`, `Clone`, `Default`, `Serialize`) | `serde`, typed `json`/`toml`, `fmt` for program types, `cli`, error enums | ATLAS-4 (ATLAS-2 if available, for `Debug` in `assert_eq`) |
| 11 | Cross-module value structs without boxing (compiler) | `@noalloc` callers of `Region`, `Duration`, `Complex`, `Span` | ATLAS-1 (`std.alloc` documents it until then) |
| 12 | Effect analysis per generic instance (compiler) | `@noalloc` generic `std` code: `sort[T]`, `Map.get`, iterator adapters | ATLAS-2 |
| 13 | Richer task and channel payloads (strings, arrays, classes) | `task`, `channel`, `stream`, `net` servers | ATLAS-4 |
| 14 | Const generics (`N: usize` with values) | `InlineArray`, `SmallArray`, `simd`, fixed crypto blocks | ATLAS-5 |
| 15 | `f16`, `bf16`; `sizeof`/`alignof` intrinsics | `mem.size_of`, `builtin.types` successors, tensors | ATLAS-2 (intrinsics), ATLAS-5 (types) |
| 16 | Borrowed views of `str` and arrays without copying | zero-copy `str`, `slice`, `json`, `regex` | ATLAS-1 (as part of `String` and arrays, if possible) |
| 17 | Closures that mutate captured state | iterator adapters with state, callbacks; today captures are by value and assignment to them is rejected (a `class` capture is the workaround) | ATLAS-2 (workaround acceptable) |
| 18 | Unit conversions | `std.units` | ATLAS-5 |
| 19 | GPU kernel launch | `std.gpu` | ATLAS-5 |

Not requested: `async` functions; module-level mutable state (explicit
state plus `std.sync.Once` instead); reflection.

---

## Runtime services the library needs

New `codira_*` services, in the shape of ARCHITECTURE.md, section 6, in
the order the milestones need them:

| service | for | milestone |
|---|---|---|
| OS entropy (`codira_random_fill`) | `std.random.from_entropy`, keyed `Map` hashing, `std.crypto` | ATLAS-1 |
| stdin read, stdout write and flush | `std.io` | ATLAS-3 |
| directory listing, metadata, create/remove directory, symlinks, copy | `std.fs` | ATLAS-3 |
| arguments, environment variables, current directory | `std.env` | ATLAS-3 |
| process spawn, wait, pipes, kill | `std.process` | ATLAS-3 |
| locks and condition variables that park for the collector | `std.sync` | ATLAS-3 |
| HMAC, HKDF, AEAD, SHA-3 over audited crates | `std.crypto` | ATLAS-3 |
| deflate/gzip/zstd streams | `std.compress` | ATLAS-3 |
| time zone database lookup | `std.time.date` | ATLAS-3 |
| sockets, DNS, TLS | `std.net`, `std.http` | ATLAS-4 |
| database scans and tables | `std.db` | ATLAS-5 |
