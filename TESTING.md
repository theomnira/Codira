# Testing: the AURORA correctness gates

The memory and runtime architecture (AURORA) is held to a set of gates
before anything in it is called production-ready: memory-safety stress,
concurrency stress and race detection, randomized allocation, cycles,
forced allocation failure, fuzzing, backend differential testing and
long-running services. This file says how to run each gate, what it checks,
and what it cannot catch. A gate passing is evidence about what it
exercised, not a proof about anything else.

```bash
python3 scripts/aurora_gates.py --list            # the gates
python3 scripts/aurora_gates.py --profile quick   # every gate, briefly
python3 scripts/aurora_gates.py                   # the CI-sized run (profile ci)
python3 scripts/aurora_gates.py --profile long --hours 6
python3 scripts/aurora_gates.py --only gc-stress,faults --seed 12345
```

The runner draws a fresh seed per run, prints it with every gate and
prints a summary table; a failure names the seed and the gates to replay.
It sets the environment knobs below; every gate is also an ordinary
`cargo test` that runs a short, fixed version of itself on every machine
(fixed seeds and case counts, so a plain `cargo test` cannot find a new
crash on an unrelated change).

| profile | time-bounded gates | total, after the builds |
|---|---|---|
| `quick` | 20 to 30 s per test | about 10 minutes |
| `ci` | 2 to 5 minutes per test | about 30 minutes, plus the sanitizer builds |
| `long` | `--hours` split over stress, soak and fuzzing | hours |

The opt-in workflow `.github/workflows/aurora-gates.yml` runs the `ci`
profile nightly and on demand (`workflow_dispatch`, with a profile and a
gate list). It is not part of the per-change CI.

Build environment, as for the rest of the workspace:

```bash
export LLVM_SYS_221_PREFIX=/usr/lib/llvm-22 CARGO_INCREMENTAL=0 \
       CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
```

## The gates

### `gc-stress`: random graphs against a shadow model

`crates/codira_runtime/tests/gc_stress.rs`, plus the older stress tests
`gc_threads.rs`, `strings_host_gc.rs`, `growable_arrays.rs`,
`heap_limit.rs` and `memory.rs`.

- A Codira program keeps a table of slots and applies a seeded stream of
  random operations to the graph behind it: new nodes, edges (cycles and
  self-loops included), edge removal, promotion of a deep node to a slot,
  bursts of garbage, spot checks. Every node carries its id, a check value
  and a label string derived from the id.
- The random stream comes from a generator written in Codira; a Rust shadow
  model replays it over the same graph without a GC. Periodically the
  program walks everything reachable, checks every node, and folds the ids
  into a digest that must equal the shadow's. A freed-but-reachable object
  shows up as a bad node, a different digest or a crash.
- Collections are forced at random points: from inside Codira code (a host
  function the program calls after every operation collects at a seeded
  rate, so the calling thread parks itself), from a host thread at seeded
  random intervals, and by a heap limit. Graphs also run on worker threads
  (`spawn`), and through the host API (rooted handles, field reads, calls).
- After each test, with nothing rooted, a collection must free every byte.
- `relocation_with_regions_and_groups_keeps_graphs_intact`: relocation on
  (as `CODIRA_GC_COMPACT=1` sets it), the graphs run as the tasks of a
  `group`, and `@region` scratch functions (heap and region objects mixed)
  called from the garbage bursts; the graphs must match the shadow model,
  collections must have moved objects (measured: hundreds per run of a few
  seconds) and regions must have closed. In debug builds every region close
  also stops the world and checks that nothing refers into the region.
- `host_threads_allocate_while_codira_code_collects`: three host threads
  allocate through the host API, holding objects unrooted inside a mutator
  guard or rooted, while Codira code on another thread collects by pacing.
- `pacing_alone_keeps_codira_graphs_bounded`: no heap limit, no host
  thread, no collections from the program -- pacing must collect, and the
  peak heap must stay under 32 MiB (measured: a 5.4 MiB peak over 450 MB
  allocated in 3 s).
- Knobs: `CODIRA_STRESS_SECS` per test (default 3), `CODIRA_STRESS_SEED`
  (default 1). Hours: `CODIRA_STRESS_SECS=3600`.
- Checked once by breaking the collector on purpose (not marking stack
  roots): the gate crashed within the first run.

Cannot catch: a leak while a program runs (the conservative scan may keep
garbage alive, and no oracle here tells the two apart), interleavings the
scheduler never produces in the time given, and a peak between the paced
bound and what a real program would need (the bound is fixed, not derived
from the live data).

### `gc-modes`: the same gates under every collector mode

The runner runs `gc_stress`, `gc_threads`, `fault_injection`,
`service_soak` and `string_fabric_threads` again with relocation on
(`CODIRA_GC_COMPACT=1`), with conservative stack roots only
(`CODIRA_GC_ROOTS=conservative`) and in GC stress mode
(`CODIRA_GC_STRESS=200`, a collection at every 200th safepoint). In stress
mode the tests that wait for the heap limit or pacing to collect are
skipped: stress mode collects first, so their trigger assertions cannot
hold; every shadow-model and content check still runs.

Cannot catch: Cascade. It tracks only objects allocated through its own
host-side `Mutator`, which Codira code does not use, so it cannot be
combined with `@region` code or the graph programs; its own oracles are in
`crates/codira_memory/tests/cascade.rs`.

### `faults`: every allocation fails once

Test-only fault injection: `MarkSweep::fail_nth_allocation(Some(n))`,
`RuntimeBuilder::fail_nth_allocation(n)`, or the environment variable
`CODIRA_TEST_FAIL_NTH_ALLOCATION=n` (read by every `Runtime`, so it works
with `codira run`, `codira start` and standalone executables). The `n`th GC
allocation after the library is loaded fails with `AllocError::Injected`,
once, on whichever thread makes it. Every GC allocation point counts:
objects, arrays, array growth, strings, snapshot restores, from generated
code, the runtime and the host. Disarmed it costs one relaxed load per
allocation. Never set the variable in production.

- `crates/codira_memory/tests/gc/faults.rs`: through the host API, for every
  `n` until a graph-building workload completes, the failure is returned,
  what was built and rooted is intact, the partial work is collected, and
  the heap works again; a failed snapshot restore leaves only garbage;
  exactly one allocation fails across threads.
- `crates/codira_runtime/tests/fault_injection.rs`: programs (objects and
  growth, strings, closures, allocation on worker threads) run in child
  processes with allocation `n` failing, for `n = 1, 2, ...` until a run
  completes (38 to 195 allocation points per program). Each run must end
  through the runtime's out-of-memory exit -- the `fatal: out of memory:
  cannot allocate` diagnostic naming the type and the injected failure,
  then exit status 102 -- or complete with the right result. Any other
  status or signal, a panic, a wrong result or a hang fails.
  `CODIRA_FAULT_BOUND` caps `n` (default 3000).

Cannot catch: allocations outside the GC heap (arenas, `codira_alloc`, the
runtime's own Rust allocations), hot-reload migration (it allocates under
the heap lock without these entry points, and panics if that fails), and
recovery inside generated code, which cannot observe a failed allocation.

### `soak`: a long-running service

`crates/codira_runtime/tests/service_soak.rs`. A Codira server with a
fixed-size cache and session table, rooted by the host, serves batches of
requests that build, split and compare strings and replace long-lived
entries, over 8 windows. It checks every object a request touches; that
live bytes after a collection plateau (the last half of the run within 1.5
times the first half, plus 64 KiB); committed bytes within the heap limit,
or, without one, pacing collecting, a peak under 64 MiB and committed
bytes on the same plateau; and, when `CODIRA_SOAK_SECS` is set (the
runner runs it alone),
that the last window's throughput is at least half the early median.
Dropping the server must free everything. Knob: `CODIRA_SOAK_SECS` (total,
default 4; hours with `CODIRA_SOAK_SECS=7200`).

Cannot catch: latency spikes (only window averages), growth slower than the
plateau slack within the run, leaks outside the GC heap.

### `loom`: the safepoint handshake

`crates/codira_memory/tests/safepoint_loom.rs`, run with
`RUSTFLAGS="--cfg loom" cargo test -p codira_memory --test safepoint_loom --release`
(loom is a `cfg(loom)` dev-dependency; normal builds never compile it).

A model of the stop-the-world handshake in `gc/safepoint.rs`
(`make_running`, `make_idle`, `poll`, `stop_the_world`): one or two
mutators touch their objects, park at a safepoint and leave, while a
collector stops the world and touches every mutator's objects. The objects
are loom `UnsafeCell`s, so loom reports any execution where a mutator and
the collector touch them without a happens-before edge. Two negative
controls must be caught (a mutator touching the heap after parking; the
handshake without its store-load order), and are.
`the_model_matches_safepoint_rs` runs in every `cargo test` and fails when
the modelled lines of `safepoint.rs` change, so the model cannot drift.

Model boundaries: it is a copy of the protocol's atomics, not the code;
loom treats `SeqCst` loads and stores as `AcqRel` (loom issue #180), so the
model adds a `SeqCst` fence between each `SeqCst` store and the load after
it, which is what C11's total order gives the real code; the condition
variable and the timed re-checks become bounded retries; two mutators are
explored with at most 3 preemptions (`LOOM_MAX_PREEMPTIONS` changes it);
loom skips load-buffering executions. The stack scan, the registry and
reentrant stops are not modelled.

### `asan` and `tsan`: sanitizers on the stable toolchain

There is no nightly toolchain; the stable 1.97 toolchain ships the
sanitizer runtimes, and `RUSTC_BOOTSTRAP=1` lets it accept `-Zsanitizer`.
This is unsupported by the Rust project but uses the same compiler. Both
use their own target directory.

```bash
# AddressSanitizer + LeakSanitizer: codira_memory, and the runtime's GC tests
ASAN_OPTIONS=detect_stack_use_after_return=0 RUSTC_BOOTSTRAP=1 \
  RUSTFLAGS="--cfg codira_asan -Zsanitizer=address" CARGO_TARGET_DIR=target/asan \
  cargo test -p codira_memory --target x86_64-unknown-linux-gnu --tests

# ThreadSanitizer needs an instrumented std: rustup component add rust-src
TSAN_OPTIONS="suppressions=$PWD/scripts/sanitizers/tsan.supp" RUSTC_BOOTSTRAP=1 \
  RUSTFLAGS="-Zsanitizer=thread" CARGO_TARGET_DIR=target/tsan \
  cargo test -Zbuild-std -p codira_memory --target x86_64-unknown-linux-gnu --test tests
```

- `--cfg codira_asan` exempts only the conservative stack scan's word read
  from instrumentation (`gc::stack_scan::read_stack_word`): the scan reads
  every word of a stack, the redzones ASan puts between locals included,
  which ASan would report. `detect_stack_use_after_return=0` because frames
  moved to ASan's fake stacks are invisible to a conservative scan.
- The ASan gate runs `codira_memory`'s tests with leak detection, and the
  runtime's GC, fault-injection and soak tests without it (compiled
  libraries and the runtime's caches live for the process).
- The TSan gate runs `codira_memory`'s tests and the runtime's threaded GC
  tests (`gc_threads`, `gc_stress` but its heap-limit test, which runs too
  slowly under TSan to reach the limit, and `strings_host_gc`). Its
  suppression file holds one documented issue (below) and nothing else.
  With it, `codira_memory`'s tests report no race on the current code;
  `gc_threads` and `gc_stress` reported none on this branch before its
  last rebase onto main (the heap-policy and scheduler work), and
  `strings_host_gc` has not finished a TSan run yet (it is very slow
  under TSan).

Cannot catch: anything in generated Codira code (it is compiled by the
Codira compiler, not instrumented), the runtime's C sources, and the
LLVM libraries; TSan sees only races whose both sides are in instrumented
Rust (atomics in generated code are invisible to it). Undefined behaviour
that is not a memory error or a race (Miri's domain) is out of scope.

### `fuzz-compiler`: programs never crash the compiler

`crates/codira_compiler/tests/fuzz_compiler.rs`, a seeded mutation fuzzer
on stable Rust. Mutants of the `std/` and `examples/` sources and of a few
programs that type-check (spans deleted, duplicated and moved; tokens,
brackets and boundary literals inserted; identifiers and lines swapped;
truncation) go through the parser and type checker, and through code
generation when they have no diagnostic. Diagnostics are fine; a panic
fails, with the program saved under `target/tmp/fuzz/`; so does a case
that grows the compiler's resident memory by more than 512 MiB, and the run
stops at 4 GiB; a case that hangs for a minute aborts the run. `reduce_crash` (ignored; set
`CODIRA_FUZZ_REDUCE=<file>`) shrinks a crashing program while it still
panics at the same place. Knobs: `CODIRA_FUZZ_SECS`, `CODIRA_FUZZ_SEED`
(default: 250 cases from a fixed seed). Every crash found has a reduced
regression test in `crates/codira_hir/src/fuzz_regressions.rs` (and next
to the fix where it is elsewhere).

Cannot catch: wrong code that compiles (the differential gate's job),
shapes the mutations do not reach from the corpus, stack overflow from deep
nesting (it ends the process rather than panicking).

### `fuzz-strings` and `fuzz-json`

- `crates/codira_runtime/tests/string_fuzz.rs`: seeded random strings
  (multi-byte UTF-8, every kind of white space, separators, numbers)
  through every runtime string operation -- concatenation, comparison,
  searches, `split`, `lines`, trims, `slice`, case mapping, `replace`,
  `repeat`, `parse_i64`, `parse_f64`, `hash`, `char_count`, `chars`,
  integer `to_string` -- with Rust's `str` as the oracle, under a 2 MiB
  heap limit so collections interleave. Knobs: `CODIRA_FUZZ_SECS`,
  `CODIRA_FUZZ_SEED` (default: 30,000 operations from a fixed seed).
  Cannot catch: arguments the generator leaves out (slices off char
  boundaries and empty `split` separators panic by design), float
  formatting (it follows `print`'s rules, not Rust's).
- `crates/codira/tests/json.rs`: `std.json.json` on mutated texts and
  random bytes, `serde_json` as the oracle (accepts and rejects the same
  texts, reads them alike). `CODIRA_JSON_FUZZ_SEED` and
  `CODIRA_JSON_FUZZ_SCALE` (the default is the fixed seed and size of the
  file's own tests).

### `differential`: LLVM against Cranelift

`crates/codira/tests/differential.rs` (see its header): seeded random
well-typed programs run on LLVM at `-O 2` and `-O 0` and on Cranelift must
print the same and exit the same way. The runner uses the ignored
`many_programs` test with `CODIRA_DIFF_PROGRAMS` and `CODIRA_DIFF_SEED`.
Cannot catch: what the generator cannot write -- today Cranelift compiles
the scalar core only, so heap-allocating programs are not compared until
the Cranelift heap-parity work lands.

## What no gate covers yet

- FFI and pinned pointers: no gate passes GC objects through C or pins
  them; the runtime's C API is only exercised by its unit tests.
- Crash and cancellation: no gate kills a thread or process mid-collection
  or cancels a task mid-allocation; panics inside generated code end the
  process (status 101) and are not resumed.
- Hot-reload migration under allocation failure or concurrent collection.
- Escape analysis, stack allocation, precise stack maps, deferred reference
  counting and cycle collection: the graph stress (cycles included) and the
  fault gate are written against the runtime's public behaviour and apply
  unchanged when these land; their own oracles come with them.
- Big-endian: the Swiss-table `Map`'s keyed SipHash reads words
  little-endian, and every Codira target is little-endian (x86-64,
  AArch64, RISC-V 64, wasm32, NVPTX); a big-endian target would need that
  checked first.
- Host code that allocates and then roots an object without a mutator
  guard while another thread runs Codira code: the runtime documents that
  the host must take a guard there; the stress tests follow that rule and
  do not test what happens without it.

## Known issues found by the gates

Each was found by the gate named and is left for the workstream that owns
the code.

- **The stack scan reads TLS (`tsan`).** `stack_scan::stack_base` takes a
  thread's stack top from `pthread_getattr_np`, which on glibc covers the
  whole stack block, including the static TLS block and the thread control
  block at its top. A parked thread keeps writing its own TLS while it
  waits (`parking_lot`'s per-thread data inside `Condvar::wait`, in a
  second collector waiting for the first), so the collector's conservative
  scan races with those writes: ThreadSanitizer reports it as a data race
  (63 reports in one run of `codira_memory`'s tests, all with
  `World::stack_words` on one side). A parked thread holds no GC references
  in that memory, so liveness is not affected, but it is a data race by the
  memory model. `scripts/sanitizers/tsan.supp` suppresses exactly this.
  Fix belongs to the stack-scan / stack-map work: scan from the stack
  pointer range only (exclude the TLS block), or stop scanning
  conservatively.
- **A small heap limit is exhausted by size classes (`fuzz-strings`).**
  With a 512 KiB limit (8 blocks), the string fuzzer ends with exit status
  102: `cannot allocate [core::String]`, `committed: 459312 bytes; the
  allocation needed 65536 more`, `live: 3356 bytes after a full
  collection` (reproduced after the heap-policy work landed). Every size
  class in use needs a 64 KiB block of its own, and a run leased to a
  thread keeps its block out of reach of other classes even after a
  collection, so the practical floor is about one block per size class in
  use per allocating thread, whatever the live data. The string gate runs
  at 2 MiB; the heap-policy work should either state the floor (and refuse
  limits below it) or return leased runs on a collection at the limit.
- **A compile-time memory blow-up, seen once (`fuzz-compiler`).** One
  10-minute run (seed 4242, before this branch was rebased onto the
  heap-policy and string work) was killed by the kernel with the fuzzer's
  process at 13 GB resident. The same seed for 10 minutes after the rebase
  (27,874 cases) peaked at about 100 MiB, so the program was not
  identified. The fuzzer now reports any case that grows the compiler by
  more than 512 MiB, with the program, and stops at 4 GiB instead of being
  killed.

- **The compiled poll word can be cleared during a stop (found reading
  the merge, not by a gate).** `frames::set_stress` calls
  `refresh_poll_word` without the registry lock, while `set_stop` calls it
  with the lock held. A `set_stress(0)` that reads the stop flag just
  before a collector sets it, and stores the poll word just after the
  collector's own refresh, leaves the poll word 0 while a stop is
  requested: compiled code in a loop that neither allocates nor calls then
  never parks, and the collector waits for it. It is a liveness issue and
  only through the test-only stress hook. Taking the registry lock in
  `set_stress` is not enough on its own: the lazy read of
  `CODIRA_GC_STRESS` calls `set_stress` from inside `refresh_poll_word`,
  which `set_stop` runs with the lock held.

## Bugs the gates found and fixed

Each fix is its own commit with a regression test.

| found by | bug | regression test |
|---|---|---|
| fuzz-compiler | `5e` (exponent without digits) panicked lowering the literal | `fuzz_regressions::an_exponent_without_digits_is_an_invalid_literal` |
| fuzz-compiler | `.` at the end of input lexed as an empty tuple index; `e.` panicked | lexer test `a_dot_at_the_end_of_the_input_is_not_an_index` |
| fuzz-compiler | a `{` where a match arm starts looped until the parser's step limit (fixed on main independently; the fuzzer's inputs are regression tests) | parser test `a_brace_where_a_match_arm_should_start_is_an_error` |
| fuzz-compiler | type errors on compiler-made nodes (missing operand, `self` type, desugared `if let`) unwrapped missing syntax | `fuzz_regressions::diagnostics_without_syntax_are_reported_on_the_function` |
| fuzz-compiler | unary `-` on an unsigned integer was `unimplemented!` in the LLVM backend (it wraps, as documented and as in Cranelift) | `functions::negating_an_unsigned_integer_wraps` |
| fuzz-compiler | `func g() -> T;` outside an `extern` block was accepted and code generation panicked on the missing body | `fuzz_regressions::a_function_without_a_body_outside_an_extern_block` |
| fuzz-compiler | a tuple index too large for `usize` panicked | `fuzz_regressions::a_tuple_index_too_large_for_usize` |
| fuzz-compiler | a `{` inside an import list looped until the step limit | parser test `a_brace_inside_an_import_list_is_an_error` |
| fuzz-compiler | `let x;` (no type, no value) and a channel nothing uses were accepted with unknown types, and code generation panicked on them | `fuzz_regressions::bindings_and_channels_whose_type_nothing_determines` |
| asan (leaks) | a C API unit test never destroyed the fields it requested | `test_codira_fields_find_by_name` itself |
