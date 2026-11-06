# codira_memory: the garbage-collected heap

This crate holds Codira's memory management: type layouts, hot-reload
memory mapping, region arenas and pages, and the garbage collector in
`src/gc/`. This note describes the first stable parts of the AURORA memory
architecture, which live here and in `codira_runtime`: heap limits and
allocation accounting, collection pacing, the failure policy, the collection
API and the runtime metrics. It ends with what the collector does not do.

## The collector in one paragraph

`MarkSweep` is a non-moving mark-sweep collector over a segregated size-class
heap (`gc/heap.rs`). Every object is one cell: a 32-byte header followed by
its payload. Cells up to 2 KiB come from 64 KiB blocks of one size class;
each thread bump-allocates from a run of free cells it leased, without
locking. Larger objects are allocated individually, and an array that grows
moves its elements to an out-of-line buffer. A collection stops the world
(`gc/safepoint.rs`): every thread running Codira code parks at its next
allocation or blocking runtime call, the collector scans each parked
thread's stack and spilled callee-saved registers conservatively (generated
code has no stack maps), marks from those words and from the host's roots,
sweeps, and resumes the threads.

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
- **Safepoints are allocations and blocking runtime calls only.** A loop
  that neither allocates nor blocks holds up every collection until it
  ends: there are no polls at loop back-edges, so the time to reach a
  safepoint has no bound. Calls into the file, database and tensor services
  are not safepoints either.
- **It does not move objects.** There is no compaction. A heap fragmented
  into partly used blocks of different size classes keeps them, and an
  allocation can fail at the limit although the free cells together would
  hold it. Empty blocks are given back (up to 16 are kept for reuse).
- **Stack scanning is conservative.** Generated code has no stack maps, so
  every word of a parked thread's stack counts as a possible reference. A
  word that happens to look like a pointer keeps garbage alive. It is also
  why the collector cannot move objects: moving one means rewriting every
  reference to it, and a word a conservative scan found may not be a
  reference at all, so it must not be rewritten. Relocation needs either
  precise stack maps from the code generator, or pinning every object a
  stack word points to and moving only the others (as mostly-copying
  collectors do).
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
