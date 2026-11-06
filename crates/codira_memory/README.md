# codira_memory: the garbage-collected heap

This crate holds Codira's memory management: type layouts, hot-reload
memory mapping, region arenas and pages, and the garbage collector in
`src/gc/`. This note describes the first stable parts of the AURORA memory
architecture, which live here and in `codira_runtime`: heap limits and
allocation accounting, collection pacing, the failure policy, the collection
API and the runtime metrics. It ends with what the collector does not do.

## The collector in one paragraph

`MarkSweep` is a mark-sweep collector over a segregated size-class heap
(`gc/heap.rs`), non-moving unless relocation is turned on. Every object is one cell: a 32-byte header followed by
its payload. Cells up to 2 KiB come from 64 KiB blocks of one size class;
each thread bump-allocates from a run of free cells it leased, without
locking. Larger objects are allocated individually, and an array that grows
moves its elements to an out-of-line buffer. A collection stops the world
(`gc/safepoint.rs`): every thread running Codira code parks at its next
safepoint (an allocation, a poll, a blocking runtime call or a foreign
call), the collector reads the frame records of compiled code precisely and
scans the rest of each parked thread's stack and its spilled callee-saved
registers conservatively, marks from those words and from the host's roots,
sweeps, and resumes the threads. Roots, safepoints and relocation are
described in "Garbage collector: roots, safepoints and relocation" below.

## Heap policy

A heap limit caps the bytes the heap holds from the system: blocks (cached
empty ones included), large objects and out-of-line payloads. It does not
count raw allocations (region arenas, `codira_alloc`, pages) or the
runtime's own bookkeeping.

`Runtime::builder` sets no limit unless the host calls
`RuntimeBuilder::heap_limit`; pacing (below) is on either way, so the
collector runs without a limit too. Every runtime the `codira` tool starts
has a finite limit unless `unlimited` is asked for: `codira run`, `start`,
`test` and `bench` (and their per-test and per-benchmark processes), and the
launcher of `codira build --exe`. The setting comes from the first of:

1. the `--heap-limit <SIZE>` flag (`run`, `start`, `test`, `bench`);
2. the `CODIRA_HEAP_LIMIT` environment variable (empty counts as unset);
3. `heap-limit` in the `[runtime]` table of the package's `codira.toml`
   (`run`, `test`, `bench`; `start` has no manifest);
4. the default.

This is the usual order (cargo's, for one): what is typed for this run beats
what the environment says, which beats what the project says. A size is a
number with an optional unit -- `B`, binary `K`/`KiB`, `M`/`MiB`,
`G`/`GiB`, `T`/`TiB`, decimal `KB`, `MB`, `GB`, `TB`, case-insensitive,
fractions allowed (`1.5GiB`) -- or a percentage of the available memory
(`25%`), or `unlimited`. Less than 64 KiB (one block) is refused. A bad
value is an error that names where it came from, reported before anything
is built. `unlimited` prints a warning on standard error. The launcher of an
executable has no flags of its own (its arguments are the program's), so it
reads the environment and the default.

**The default is half of the memory available to the process**: the smaller
of physical memory and the memory limit of the cgroup the process runs in
(cgroup v1 `memory.limit_in_bytes` or v2 `memory.max`, the smallest on the
path to the root of the hierarchy), at least 64 MiB, and 4 GiB when neither
can be read (`codira_runtime::heap_policy`, usable by other hosts). Why
half:

- The limit counts the GC heap only. The rest of the process needs memory
  too: `codira run` compiles in the same process first, and there are loaded
  code, thread stacks, region arenas and pages, and file and database
  buffers.
- Pacing keeps the heap near twice the live data, so the limit matters only
  for a program whose live data keeps growing. At half the memory such a
  program stops with a diagnostic that says what was live and how to raise
  the limit, instead of being killed by the kernel's out-of-memory killer
  (or swapping) with nothing said.
- In a container the cgroup's limit, not the machine's memory, is what the
  process can get. On the machine these measurements ran on, the cgroup
  allows 13.4 GiB of the 15.6 GiB, so the default is 6.68 GiB.
- For comparison, the JVM defaults to a quarter of physical memory and .NET
  to three quarters of a container's limit. A half leaves room for the rest
  of the process and still lets a program use most of a machine without
  configuration.

## Pacing

Before this, a heap without a limit never collected, and `codira run` grew
until the system killed it. Now the heap asks for a collection once it has
grown by `growth_percent` over what the previous collection left live, the
rule of Go's `GOGC`:

```text
trigger = max(min_heap, live_after_the_last_collection * (100 + growth_percent) / 100)
```

The next allocation by a thread running Codira code then collects. An
allocation by the host never does: a host thread's stack is scanned only
while that thread is inside an allocation, so a collection started by one
host thread could free an object another host thread holds unrooted
between two of its allocations (which a host may do as long as it never
collects itself). The collection stays due until Codira code allocates.
The heap limit still collects on a host allocation, as it always has; a
host that sets a limit and allocates from several threads must root what
it holds. The defaults (`GcPolicy::default()`) are `growth_percent = 100` and
`min_heap = 4 MiB`: a program with a small live set collects every 4 MiB of
allocation, and one with a large live set lets the heap grow to about twice
that set. Pacing counts payload bytes (as `Stats::allocated_memory` does,
without the 32-byte headers and the rounding to size classes), so for small
objects the heap holds two to three times the trigger in cells when it
collects.

The per-object fast path pays one relaxed load of a flag. The heap compares
its byte count with the trigger only where an allocation takes the heap lock
anyway (refilling a thread's run of cells, a large object, growing an
array), and sets the flag there. Threads that see the flag at once
coalesce: the first collects, the others find it cleared.

The heap limit stays the hard cap: an allocation that would exceed it
collects first (with the world stopped) and retries before the other
threads resume. Under a tight limit the heap reaches the limit before
pacing's trigger, so all the collections are "at the heap limit" -- the
same collection, counted under another cause.

`--gc-growth <PERCENT|off>`, `CODIRA_GC_GROWTH` and `gc-growth` in
`[runtime]` set the growth, in the same order as the limit
(`RuntimeBuilder::gc_growth` or `gc_policy` for other hosts). `off`
collects only at the limit and on request.

## When memory cannot be reclaimed

The heap never hands out memory past its limit and never reports a failed
allocation as a success.

- **Host API.** `MarkSweep::try_alloc`, `try_alloc_array` and
  `try_grow_array`, and `StringRef::try_new` in the runtime, return
  `AllocError`. `AllocError::LimitExceeded { limit, committed, requested,
  live }` says what the request needed on top of what is committed (a whole
  block for a small object) and what is live: the payload bytes the last
  collection kept plus everything allocated since, which, after the
  collection the failed allocation ran, is the live data. The allocation
  collects first whenever that is safe: the thread is not exiting, not
  itself collecting, and every thread's stack can be scanned.
- **A running program.** Generated code cannot observe a failed allocation,
  and unwinding through Codira frames is not defined, so an allocation by
  Codira code that fails ends the process like a panic: what was printed is
  flushed, the diagnostic goes to standard error, and the exit status is
  **102** (`codira_runtime::HEAP_EXHAUSTED_EXIT_STATUS`; a panic is 101).
  Under `codira run`:

  ```text
  fatal: out of memory: cannot allocate `Node` (24 bytes)
    heap limit: 1048576 bytes (1.00 MiB)
    committed:  1048576 bytes; the allocation needed 65536 more
    live:       393216 bytes after a full collection
    the limit is 1.00 MiB, from --heap-limit
    raise it with --heap-limit <SIZE>, the CODIRA_HEAP_LIMIT environment variable, or `heap-limit` in the [runtime] table of codira.toml
  ```

  The last two lines are the host's (`RuntimeBuilder::heap_limit_note`).
  `codira test` gives the `fatal:` line as the reason a test crashed.
- **Emergency reserve.** The first runtime sets aside 256 KiB of system
  memory (its pages written, so they are resident) and frees it before
  reporting, so formatting the diagnostic, flushing and the at-exit handlers
  have memory even when the failure was the system allocator's
  (`AllocError::SystemOutOfMemory`). A failure at the GC limit leaves the
  system allocator working anyway.

Tests: `heap_limit::exhausting_the_cap_ends_the_process` and
`live_data_beyond_the_cap_ends_the_process` (`codira_runtime`, forced
failures in a child process); `gc_policy::a_failure_at_the_limit_reports_what_is_live`
(this crate); `heap::a_live_set_beyond_the_limit_fails_with_the_documented_message`
and `heap::codira_test_applies_the_limit_to_each_test` (the `codira`
binary).

## The collection API

A collection stops every thread running Codira code. A host thread that
called `collect()` back to back used to leave a Codira thread about one
allocation per collection. There are now two calls, and neither lets a loop
of requests starve Codira code:

- `MarkSweep::collect_blocking()` / `Runtime::collect_blocking()` (C:
  `codira_gc_collect`) returns, once a collection that started after the
  call has finished, a `CollectionReport`: objects and bytes reclaimed, the
  pause, and whether the call was coalesced.
- `MarkSweep::request_collection()` / `Runtime::request_collection()` (C:
  `codira_gc_request_collection`) returns at once: `true` if it queued a
  request, `false` if one was already pending. A thread of the collector's
  own, started by the first request, serves it.

**Fairness.** A request from outside first waits, parked, until Codira code
has run for the *mutator window* since the previous collection of any kind
ended: `GcPolicy::min_mutator_window` (1 ms) or `pause_multiple` (4) times
the previous pause, whichever is longer. Collections requested from outside
therefore stop the world for at most a fifth of the time, however many host
threads ask and however often. The wait is skipped when no other thread is
inside Codira code: there is nobody to starve. Collections that allocations
start (pacing, the limit) do not wait; they are part of the allocating
code's own progress, and how often they run follows its allocation rate.

**Coalescing.** A request is satisfied by any collection that *starts* after
it was made: a paced one, another thread's, another request's. Requests
made while one is pending are folded into it, so N threads asking at once
cause one collection.

Tests (this crate, `tests/gc_policy.rs`):
`back_to_back_blocking_collections_leave_the_mutator_its_window` (a host
loop of `collect_blocking` gets at most `elapsed / window + 1` collections
while the mutator keeps allocating), `concurrent_blocking_calls_coalesce`,
`request_collection_returns_at_once_and_is_served`,
`requests_during_the_window_coalesce`,
`dropping_the_collector_stops_its_request_thread`. In `codira_runtime`,
`strings_host_gc::host_collections_keep_live_strings_intact` runs the string
workload with a host thread calling `collect_blocking` in a loop, with no
sleep, and checks the rounds take about as long as without it (debug build,
three runs: 1.06 s alone and 1.04 s with the collecting thread; 1.08 and
1.05; 1.03 and 1.05; in each, the host made 9 calls, one or two of them
collected and the rest were satisfied by paced collections). The test used
to sleep 100 us between collections because without it the loop took from
seconds to minutes. `test_gc_request_collection` covers the C API.

## Metrics

`MarkSweep::heap_stats()` / `Runtime::stats()` return `GcStats`
(`RuntimeStats` in the runtime):

| field | meaning |
|---|---|
| `heap_bytes`, `peak_heap_bytes` | committed now, and at most |
| `heap_limit` | the limit, if any |
| `live_bytes` | payload bytes the last collection kept plus all allocated since |
| `live_bytes_after_last_collection` | payload bytes the last collection kept |
| `next_paced_collection_at` | pacing's trigger; `None` when pacing is off |
| `bytes_allocated`, `objects_allocated` | ever (a growing array counts each new buffer) |
| `collections` | by cause: paced, at the heap limit, requested, explicit |
| `bytes_reclaimed`, `objects_reclaimed` | what the collections freed |
| `collection_time` | the sum of the pauses |
| `time_to_safepoint` | the part of it spent waiting for threads to stop |
| `pauses` | count, total, p50, p99, max |

A pause runs from asking the threads to stop until they are let go: reaching
safepoints, marking, sweeping and, at the limit, the retried allocation. The
percentiles come from a log-linear histogram (16 buckets per power of two,
976 counters): each is the upper end of its bucket, at most 6.25% above the
true value; `max` is exact. Recording costs the same however long the
program runs.

Counting allocations costs a load and a store of two thread-local counters
per object, with no atomic read-modify-write; a collection reads the clock
three times and takes a mutex once; reading the statistics takes the heap lock
briefly. The overhead is measured below.

`codira run --runtime-stats` and `codira start --runtime-stats` print them
to standard error when the entry point returns, with the settings and where
they came from:

```text
codira runtime stats (wall time 3.54 s):
  heap limit   8.00 MiB, from --heap-limit
  gc growth    100%, from the default
  heap         7.14 MiB now, 7.95 MiB peak, limit 8.00 MiB
  live         258 KiB after the last collection, 3.14 MiB now (next paced collection above 4.00 MiB)
  allocated    114 MiB in 3,820,001 objects
  reclaimed    110 MiB in 3,716,979 objects
  collections  34: 0 paced, 34 at the heap limit, 0 requested, 0 explicit
  gc time      1.75 s with the world stopped (43.6 us of it waiting for threads to stop)
  pauses       p50 46.1 ms, p99 97.2 ms, max 97.2 ms
```

(A debug build of the compiler and runtime; see below for release numbers.)
There is no `codira runtime stats` subcommand: the statistics belong to a
running program, and the `codira` tool has no way to attach to one.

## Measurements

Machine: an x86-64 VM, Intel Xeon @ 2.10 GHz, 4 vCPUs shared with two
other engineers' builds (so times move by 10-20% between runs), 15.6 GiB of
memory with a 13.4 GiB cgroup limit, Linux 6.18, Rust 1.97.1. Compiler and
runtime built with `cargo build --release`; the program with `codira build
-O 3` and run with `codira start target/mod.codiralib main --heap-limit L
--gc-growth G --runtime-stats`. Each configuration ran 5 times, the runs
interleaved (the order reversed every round); the table gives the median
program time with the range, and the statistics and peak RSS (from
`wait4`) of the median run.

**Workload**: `examples/gc_churn`, `main`: a table of 50,000 entries (an
object with two strings each, 6.4 MiB of live payload) and 2,000,000 steps,
each building four short-lived strings and an object and replacing one
entry: 1.17 GiB in 39.4 million objects allocated in total. Every run
printed the same check value.

| limit | growth | time ms, median (range) | collections (paced / at limit) | time stopped | pause p50 | p99 | max | peak heap | peak RSS |
|---|---|---|---|---|---|---|---|---|---|
| 64 MiB | 100% | 4477 (4130-4713) | 188 (188 / 0) | 1.68 s | 8.1 ms | 14.8 ms | 14.8 ms | 27.2 MiB | 71 MiB |
| 64 MiB | off | 4380 (4063-4632) | 52 (0 / 52) | 1.28 s | 24.1 ms | 41.5 ms | 41.5 ms | 63.9 MiB | 117 MiB |
| 256 MiB | 100% | 4389 (4192-4809) | 188 (188 / 0) | 1.62 s | 8.1 ms | 13.1 ms | 13.3 ms | 27.2 MiB | 71 MiB |
| 256 MiB | off | 3411 (3219-3685) | 10 (0 / 10) | 885 ms | 88.1 ms | 99.2 ms | 99.2 ms | 256 MiB | 303 MiB |
| 1 GiB | 100% | 4439 (3905-5014) | 188 (188 / 0) | 1.63 s | 8.1 ms | 15.7 ms | 16.6 ms | 27.2 MiB | 71 MiB |
| 1 GiB | off | 3175 (3010-3292) | 2 (0 / 2) | 605 ms | 302 ms | 315 ms | 315 ms | 1024 MiB | 1072 MiB |

With `--gc-growth 400` under the 256 MiB limit (3 runs): 3744-3877 ms, 48
paced collections, 1.10-1.19 s stopped, pauses p50 24-25 ms, max 30 ms, a
72.5 MiB peak heap.

What this shows:

- **Pacing bounds memory, and costs throughput.** With pacing the heap
  peaks at 27 MiB whatever the limit, against the whole limit without it;
  at 1 GiB the difference in peak RSS is 71 MiB against 1072 MiB. The price
  is time: the default policy spends 1.6 s of a 4.4 s run with the world
  stopped (188 collections), and the program takes 30-40% longer than with
  a 1 GiB heap collected twice. `--gc-growth` is the knob: at 400% the run
  takes 3.8 s in a 72 MiB heap.
- **Pauses follow the heap's size, not the live data.** The live payload
  is 6.4 MiB in every configuration, yet the pause grows from 8 ms (27 MiB
  heap) to 24 ms (64 MiB), 88 ms (256 MiB) and 300 ms (1 GiB): finding the
  rooted objects and sweeping walk every cell of every block. Waiting for
  threads to stop is negligible here (under 0.1 ms in total; one thread,
  which allocates constantly).
- **Under a tight limit pacing makes no difference.** At 64 MiB the
  unpaced run collects at the limit 52 times with 24 ms pauses and takes
  about as long; the paced one never reaches the limit.

**Overhead of the statistics and the pacing check.** The allocation
benchmark of this crate (`cargo bench -p codira_memory --bench gc`,
200,000 8-byte objects allocated by the host, 31 samples per run),
run three times each on the commit before this work and on this one,
alternating:

| workload | before, median ns/object | after |
|---|---|---|
| fresh heap | 24.3, 25.5, 25.7 | 25.1, 27.3, 24.1 |
| after a collection | 24.7, 24.4, 24.8 | 25.8, 27.1, 25.3 |

and a collection of 100,000 live and 100,000 dead objects took 1.62, 1.64
and 1.73 ms before against 1.72, 1.67 and 1.59 ms after. The allocation
fast path is up to about 1-2 ns (4-8%) slower in these runs, at the edge
of their noise; the minimum after a collection moved from 22.7-23.9 to
24.2-24.6 ns. That is the second counter (objects), the pacing flag, and
the check whether the host thread may collect. Collection cost did not
change measurably. It stays on.

**A host collecting in a loop.** `strings_host_gc` (debug build; three
rounds of a string workload) took 1.06, 1.08 and 1.03 s alone and 1.04,
1.05 and 1.05 s with a host thread calling `collect_blocking` back to back.

## What the collector does not do

- **It stops the world.** Every collection pauses every thread running
  Codira code for the whole mark and sweep; there is no incremental,
  concurrent or generational collection. Pauses grow with the heap: marking
  with the live data, sweeping and the scan for rooted objects with every
  object in the heap.
- **A bounded loop is not a safepoint.** Polls sit on the back-edges of
  loops whose trip count the optimizer cannot bound on entry; a counted
  loop, or a loop over an array or a range, has none, so a collection waits
  for it to finish however many iterations it has. Calls into the file,
  database and tensor services, and into `extern` functions not declared
  `@noalloc`, are not safepoints either.
- **Relocation is experimental and mostly-copying.** It is off unless
  turned on; when on, it moves objects only out of blocks at most a quarter
  full, and never an object a stack, a root or a pin refers to.
- **Parts of the stack are still scanned conservatively.** Host frames,
  foreign frames, frames of code without records (the Cranelift JIT), of
  constant-time functions and of functions the record pass could not
  handle, threads parked at a poll, and the callee-saved registers: a word
  there that looks like a pointer keeps garbage alive. Frames are left out
  of the conservative scan on x86-64 Linux only.
- **The safepoint registry is per process.** A collection in one runtime
  pauses the threads running any runtime's code.
- **There is no limit on time spent collecting.** A program whose live data
  sits just below the limit collects at the limit again and again, each
  collection freeing a little, until an allocation finally fails; nothing
  gives up earlier the way the JVM's "GC overhead limit exceeded" does.
- **The host's convenience calls do not return the error.** The `try_*`
  allocation API does, but `StringRef::new` ends the process like Codira
  code does, and `GcRuntime::alloc`/`alloc_array` (behind
  `Runtime::construct_array` and `StructRef` construction) panic.

## Cascade (experimental, off by default)

`MarkSweep::enable_cascade` attaches Cascade, an engine that reclaims
objects before a collection would:

- unique objects at their last use;
- shared objects by deferred, epoch-protected reference counting;
- cycles by bounded concurrent trial deletion.

Only objects allocated through a Cascade `Mutator` are managed, and
compiled programs do not use it yet: the compiler emits no retains or
releases. Without `enable_cascade`, the collector behaves exactly as
described above. The ownership word Cascade needs takes the upper half of
the old 64-bit `inline_capacity` field, so the header stays 32 bytes.
`CASCADE.md` holds the design, the invariants, what is unverified, and the
measurements. Relocation (below) is switched off while Cascade is
attached: the engine keeps object addresses in its own buffers and queues,
which evacuation does not update.

## Garbage collector: roots, safepoints and relocation

The collector (`src/gc/`) is a stop-the-world mark-sweep collector over
size-class blocks (`gc/heap.rs`). This note covers how it finds the roots
on thread stacks, where threads stop for it, and the experimental
relocation of sparse blocks.

### Precise stack roots

Code compiled through LLVM carries *frame records* (`codira_codegen`,
`src/gc_roots.rs` and `cpp/gc_roots.cpp`). A pass over the linked assembly
module, before its final optimization, gives every function that makes a
call that may reach a collection a record on its stack: on entry the
function links it into the thread's chain (`gc/frames.rs`), on every return
it unlinks it. Every value that may hold a reference and is live across
such a call (or is an argument of one: a stack argument lives in the
caller's outgoing-argument area) is stored into a spill word of the record
when it is defined,
and every stack slot that may hold a reference is listed in it. "May hold
a reference" is decided on the IR: a pointer (or an aggregate with one) is
a reference unless it provably points at a stack slot, a global or a
constant; integers count only when `ptrtoint` made them from a reference.
The record's address escapes into the chain, so the optimizer must keep the
spill words current at every call that may read memory -- exactly the
calls that can reach a collection.

At a collection the collector reads the records' slots precisely and
leaves the machine frames they describe (`[lo, hi - 64)`, x86-64 System V)
out of the conservative scan of the thread's stack. The 64 bytes at the
top of each frame stay in the conservative scan: that is where the
prologue saved the caller's callee-saved registers, which may belong to a
host frame. Everything else -- host frames, foreign frames, code compiled
without records (the Cranelift JIT, a library built with
`CODIRA_GC_FRAMES=0`), and functions the pass could not handle -- is still
scanned conservatively.

Why this mechanism and not LLVM's statepoints or its shadow stack:

- **Statepoints** (`gc.statepoint`, `RewriteStatepointsForGC`) need every
  reference in a separate address space all the way through code
  generation, and the code generator uses one untyped `ptr` for references,
  payload pointers and raw addresses alike, passes references to the
  runtime through stack out-slots (which the rewrite does not track), and
  keeps references inside first-class aggregates (enums, strings). The pass
  here works on the IR as it is and treats out-slots and aggregates as
  regions.
- **LLVM's shadow-stack strategy** keeps its chain in one global (not per
  thread) and needs `gcroot` annotations emitted for every reference.
- Both would leave the Cranelift JIT out; the record format is a plain
  interface any back end can produce (see `codira_codegen::gc_roots`).

The cost is per function, not per loop iteration: one indirect call to get
the chain head, a handful of stores on entry and one on return, plus one
store per spilled value at its definition.

Measured on `examples/collections_bench` and `examples/pipelines_bench`
(milliseconds, medians of 11 interleaved runs pinned to one core; "off" is
the same release `codira` building with `CODIRA_GC_FRAMES=0`, which turns
off both records and polls; Rust is `rustc -C opt-level=3` of the
benchmarks' Rust programs; every checksum is identical across the three):

| benchmark | off | on | on / off | Rust |
|---|---:|---:|---:|---:|
| `push_10m` | 43.90 | 45.19 | 1.03 | 44.56 |
| `sum_builtin` | 9.97 | 10.45 | 1.05 | -- |
| `sum_fold_closure` | 9.65 | 10.33 | 1.07 | 10.01 |
| `sum_for_loop` | 9.68 | 10.36 | 1.07 | 10.53 |
| `sum_indexed_while` | 9.82 | 10.57 | 1.08 | -- |
| `map_filter_sum` | 83.82 | 88.01 | 1.05 | 20.28 |
| `map_filter_sum_fused_fold` | 17.24 | 17.20 | 1.00 | -- |
| `sort_1m` | 20.53 | 20.07 | 0.98 | -- |
| `map_int_1m_fast_insert` | 81.16 | 85.75 | 1.06 | 80.69 |
| `map_int_1m_fast_hit` | 55.20 | 55.55 | 1.01 | 40.16 |
| `map_str_1m_fast_insert` | 373.68 | 406.67 | 1.09 | 284.76 |
| `map_str_1m_fast_hit` | 187.85 | 181.80 | 0.97 | 132.25 |
| `map_filter_sum_lazy` | 17.43 | 17.25 | 0.99 | 17.73 |
| `map_filter_sum_eager` | 95.41 | 93.79 | 0.98 | -- |
| `map_filter_sum_fold` | 18.97 | 18.13 | 0.96 | -- |
| `enumerate_filter_count` | 17.93 | 16.82 | 0.94 | 17.14 |
| `zip_fold_dot` | 19.21 | 18.37 | 0.96 | 22.55 |
| `take_while_sum` | 16.81 | 14.78 | 0.88 | 15.08 |
| `map_collect` | 55.96 | 51.53 | 0.92 | 52.81 |
| `filter_collect` | 65.30 | 70.19 | 1.07 | 63.65 |
| `range_filter_map_sum` | 5.16 | 5.58 | 1.08 | 6.62 |
| `chain_filter_count` | 79.16 | 83.60 | 1.06 | 76.51 |

Intel Xeon @ 2.10 GHz (4 vCPUs), Linux 6.18, Rust 1.97.1, `codira build -O
2`. The machine is noisy: the same loop code measured from 15 to 24 ms
across runs, so differences under about 10% are within what repeated runs
of identical code show. The array sums are 5-8% slower in the full
benchmark, although their loops compile to the same IR with and without
records (they are counted loops, so they have no poll); a program that runs
only `for x in xs { s += x }` over 10M elements measured 9.25 ms against
9.11 ms (medians of 7 runs of the best of 15). `map_str_1m_fast_insert` is
the largest loss (9%). Polling every loop back-edge, which an earlier
version did, cost up to 2x on `range_filter_map_sum` and about 35% on
several pipelines; polls are now only on loops nothing bounds.

Selection: `CODIRA_GC_ROOTS=conservative` (or
`frames::set_root_mode(RootMode::Conservative)`) scans every stack word
conservatively again, records included -- a fallback that cannot be wrong
about compiled frames. Precise is the default.

What precise scanning does *not* do: an integer that sits in a
callee-saved register at the safepoint still retains the object it points
into (those registers may hold a host frame's references). The test
`precise_roots::integers_that_look_like_pointers_do_not_retain_objects`
holds 40 such integers across a collection: 5 objects are retained (at
most one per callee-saved register; 6 on an earlier build), against all 40
with the conservative scan.

### Safepoints

- Every allocation (as before), and every call into the runtime that
  allocates.
- **Polls**, inserted after the final optimization so they never block
  vectorization: on the back-edge of every loop whose trip count scalar
  evolution cannot bound when the loop is entered, and at the entry of
  every function that makes a call (which every recursion goes through).
  A poll is a load of the poll word and a branch to a cold call. A loop
  the optimizer can bound (counted loops, loops over arrays and ranges,
  loops that may also exit early) is not polled: a collection may wait for
  it to finish. A thread parked at a poll is scanned conservatively: the
  optimizer ran before the poll existed, so the spill words need not be
  current there.
- **Blocking regions** (`safepoint::blocking`): channel operations, joins,
  sleeps. The thread is parked for their duration.
- **Foreign calls**: a call to an `extern` function declared `@noalloc`
  (a trusted assertion that it neither allocates nor calls back into
  Codira code) is bracketed by `codira.gc.foreign_enter` /
  `foreign_leave`: the thread is parked while the foreign code runs, its
  stack scanned from the stack pointer at the call, plus the callee-saved
  registers at the transition. A call to any other `extern` function keeps
  the thread running, as before.

`CODIRA_GC_STRESS=<n>` collects at every n-th safepoint of a thread (`1`:
every allocation and every poll).

### Relocation (experimental, off by default)

`MarkSweep::set_compaction(true)`, `RuntimeBuilder::compaction(true)` or
`CODIRA_GC_COMPACT=1`. After the sweep, the collector moves the live
objects out of size-class blocks that are at most a quarter full, into the
free cells of the class's other blocks, sparsest blocks first and only
while those cells can take them (so it never needs memory the heap does
not already hold, which is what lets it help at the heap limit), rewrites
every heap reference to a moved object, and releases the emptied blocks.

It is mostly-copying: an object that any stack refers to (precisely or
conservatively), a rooted object (`GcRuntime::root`, `GcRootPtr`,
`RootedStruct`) and a pinned one (`MarkSweep::pin`) never moves, so stack
slots and host-held handles are never rewritten. The words of
`root_region` (task-group results waiting to be joined, the chunks of
`@region` arenas) are scanned conservatively like stack words, so what they
refer to is pinned the same way. Blocks in which a
thread's allocation cache holds a run are left alone. The words of root
regions (task-group results, `@region` arena chunks registered as root
spans) are read with the conservative roots, so what they refer to is
pinned too. While Cascade is attached (`MarkSweep::enable_cascade`),
relocation is off whatever the setting: Cascade's buffers and queues hold
object addresses that evacuation does not rewrite. Hot-reload migration
and heap snapshots run in their own stops and see a heap in which every
reference is current.

Measured with `relocation::fragmentation_*` (400,000 48-byte objects of
which every 16th is kept, then 60,000 160-byte objects, collecting after
each phase, pacing off): the heap holds 19.5 MB after the first phase and
29.7 MB after the second without relocation, 2.6 MB and 11.7 MB with it;
under a 16 MiB heap limit the non-moving collector cannot finish the
second phase (its allocation fails at the limit) and the relocating one
does. Pause cost (release build, the collection itself without the wait
for threads to park, medians of 3 runs): the collection that moves 23,363
objects takes 16.9 ms against 7.9 ms for the same collection without
relocation; the next one takes 3.4 ms against 10.1 ms, the heap being
smaller.

What keeps it experimental: it has run on x86-64 Linux only; the stacks
pin (a frame that refers to a sparse block's object keeps the block); and
host code that keeps a `GcPtr` it neither roots nor pins (in a Rust data
structure) was never safe across a collection and is now also wrong after
one.

### Tests

- `codira_runtime/tests/precise_roots.rs`: integers that look like pointers
  (precise against conservative), references held only by frames under a
  collection at every safepoint in both root modes, a collection while a
  thread is in a `@noalloc` foreign call and while it runs a loop without
  safepoints.
- `codira_runtime/tests/relocation.rs`: heap references follow moved
  objects (without the rewrite the test crashes), rooted and pinned objects
  keep their addresses, and the fragmentation workload above.
- Differential and stress runs of the existing suites (`codira`,
  `codira_runtime`, `codira_memory`, `codira_codegen`) with
  `CODIRA_GC_ROOTS=conservative`, `CODIRA_GC_COMPACT=1` and
  `CODIRA_GC_STRESS=7`. Under stress, tests that count collections by
  cause, measure timing, or wait for a paced or limit collection do not
  hold by construction (stress collections keep the heap small) and were
  left out; see the branch's notes for which.
