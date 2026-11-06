<!--
Copyright (c) 2026 Omnira CJSC
Author: Tunjay Akbarli
Date: November 6, 2026

Functionality: The reference run of the AURORA benchmark suite: every
kernel's median with its interval, against Rust and C++, with memory, GC
pauses, latency percentiles, counters, compile time and binary size.
-->

# AURORA: the reference run

The first full run of the suite (`python3 scripts/bench.py`, see
`README.md` for the method), on commit `48bf5bed4e` (main at
`f8b145c6a1` and this suite). Later work measures against these numbers.
`baseline/ci.json` holds the samples of its CI kernels for the regression
gate.

**Since then.** The CI subset was rerun against that baseline after main
moved to `16934da443` (task groups, and a scheduler whose channel
operations try before they wait). No kernel regressed, and the heap floor
is unchanged (64 MiB). One kernel moved: `channels.spsc` went from 13.2x to
5.2x Rust's time (a 60% drop, 95% interval -70% to -53%). The tables below
are still the `f8b145c6a1` run.

**The machine.** Intel(R) Xeon(R) Processor @ 2.10GHz, 4 logical CPUs, 16 GB,
Ubuntu 24.04.5 LTS, Linux 6.18.44-fc-v242: a **shared virtual machine** (a
container in a VM; the CPUID hypervisor flag is set). No CPU pinning, no
frequency control, no hardware performance counters (`perf` counts only
software events here). The load average was 0.73
at the start and 1.17 at the end, with nothing else
of ours running. What that implies: absolute times can move by 10-30%
between runs, a single run more than that; the interleaved order spreads
that noise over all variants, which is why the ratios are steadier than
the times, and why the intervals below are part of every number. Compare
against this run only on the same machine class.

**Toolchains.** Codira `codira build -O 3` (a release build of the compiler,
runtime in release mode, the generic x86-64 target); Rust 1.97.1, `rustc
--edition 2021 -C opt-level=3 -C codegen-units=1` (`serde_json` 1.0.133 with
`preserve_order` and `float_roundtrip` for JSON); C++ clang 22.1.8 `-O3
-std=c++23 -pthread -DNDEBUG` with libstdc++ 13 (JSON: g++ 13 and RapidJSON
1.1, because the packaged RapidJSON does not compile with clang 22). Eleven
processes per kernel and variant, after a warm-up round.

## Where Codira stands

Ratios are Codira's time over Rust's (below 1: Codira is faster), from the
tables below.

**At parity or ahead:**
- **Numeric loops and calls**: summing, recursion, branchy integers and float
  escape-time loops, 0.91-1.08x.
- **Arrays and pipelines**: lazy pipelines, folds, `zip`, `take_while`,
  `collect`, push and sort, 0.76-1.08x.
- **Formatting**: integer `to_string`, 0.52x; `{n:08}`, 0.36x; the debug
  form of a struct, 0.61x; float `to_string`, 0.98x.
- **Strings**: appending numbers, 0.40x; lazy `lines()` and `split()`,
  0.72-0.76x; splitting into fields, 0.87x; `find`, 0.97x.
- **`?`**: eight frames deep, 1.13x; a parse loop with `?` per word, 0.99x.
- **Hash maps on 1M keys**: `i64` insert, hit, miss and remove, 0.91-1.22x;
  `String` keys, 0.66-0.95x. Both hold under the unkeyed hash and under the
  keyed SipHash-1-3, which Codira and Rust both use by default.
- **JSON**: parsing `twitter`, 0.82x.
- **Compile time**: 0.08-1.2 s a workload, against 0.35-1.3 s for `rustc` and
  1.6-3.0 s for clang.

**Where Codira loses** (each is a number to bring down; none is averaged
away):
- **Bounded channels.**
  - `spsc` is 12.9x and `mpsc_4` 26.4x Rust's crossbeam-style
    `sync_channel`.
  - Against a textbook mutex-and-condvar queue in C++, Codira is 1.28x and
    0.43x.
  - Blocking and waking threads per message is the cost.
- **Collected object graphs.**
  - `binary_trees` 2.75x, `cyclic_graph` 3.1x, `linked_list` 3.0x Rust.
  - The world is stopped for 324, 184 and 534 ms of those runs.
  - Pause medians are 3, 11 and 63 ms; the longest are 8, 14 and 99 ms.
- **The server workload.**
  - It is 2.3x Rust (`requests`), and 2.9-3.3x on the long run under caps
    from 1 GiB down to 80 MiB.
  - It is 5.5x at 64 MiB, the floor, with 163 collections and 3.2 s stopped.
  - Tail latency: Rust's worst request takes about 5 ms (at its map's
    rehash). Codira's p99.9 is 34 µs against 5-10 µs, and its worst request
    takes 34-82 ms, one collection pause.
- **Errors converted to `AnyError`.**
  - The `Err` path is 6.4x Rust, and 7.8x with a context.
  - Each error allocates its message and a context array.
  - 108 and 168 collections in 1M errors.
- **`?` wrapping an error from an inlined parse** of an input that changes
  every iteration: 2.8x (`Ok`) and 4.7x (`Err`).
- **Heap field stores in a loop** (`store_field`): 2.5x. The field is
  stored on every iteration; Rust and C++ keep it in a register.
- **A 200x200 matrix product with checked indexing**: 3.0x. Rust checks its
  indexes too.
- **Array loops that store**: copying into another array 1.25x; the sieve
  1.11x; the eager `map(..).filter(..).sum()`, which builds two arrays, 1.36x.
- **Concatenating two strings into a new one** (`a + "/" + b`): 2.8x.
  Appending short literals: 1.37x.
- **Iterating a map with `String` keys**: 5.5x; with `i64` keys, 1.48x.
  Building small five-key maps: 1.85x.
- **JSON printing and pretty-printing**: 1.44-2.25x. Parsing `canada` and `deep`: 1.08-1.14x.
  RapidJSON parses 2.6-6x faster than Codira.
- **JSON parsing pays for pacing.**
  - `parse_twitter` spends 298 of its 948 ms with the world stopped.
  - Before pacing, the same kernel took 645 ms on this machine.
- **`pretty_deep` ran out of its 1 GiB cap in one of seven attempts.** The
  runtime reported 843 MB live after a full collection, for a program that
  holds a 7 MB document's tree and one 149 MB text at a time. It has six
  samples below.
- **Memory.** The peak RSS of Codira's processes is higher everywhere:
  - the server: 198 MB against 29 MB;
  - JSON parsing: 612 MB against 369 MB (Rust) and 81 MB (C++).

  The libraries are small (8-204 KiB), but they run in the runtime (the
  host: 2.3 MB, 1.7 MB stripped; `codira build --exe`'s launcher: 23.5 MB).
  A Rust binary is 4.3 MB (0.4 MB stripped).

**C++ notes.** libstdc++'s `std::unordered_map` (node-based) is 1.7-10x
slower than both open-addressing tables; `std::vector` growth (no
`realloc`) and `std::sort` make C++ slower on `push_10m` and `sort_1m`.
Those are properties of the standard library, reported as they are.

**Not measured.**
- Hardware counters: the VM exposes none.
- Pause times of a concurrent or incremental collector: the collector stops
  the world.
- Allocation rate per thread, and GC behaviour under several mutator
  threads (the `channels` kernels allocate nothing).
- Start-up time, and the code size of the runtime per program.
- Energy, and Windows and macOS.
- Codira's `--jit` backend.

---

- date: 2026-11-06T10:49:27+00:00, commit `48bf5bed4e13`
- machine: Intel(R) Xeon(R) Processor @ 2.10GHz, 4 logical CPUs, 16472088 kB, Ubuntu 24.04.5 LTS, kernel 6.18.44-fc-v242, virtualization: container: docker, hypervisor (CPUID flag set: a virtual machine), governor: not exposed
- load average (1, 5, 15 min): [0.73, 0.97, 1.08] at the start, [1.17, 1.33, 1.28] at the end
- toolchains: codira 0.6.0-dev (`build -O 3`), rustc 1.97.1 (8bab26f4f 2026-07-14) (`--edition 2021 -C opt-level=3 -C codegen-units=1`), Ubuntu clang version 22.1.8 (++20260714014902+ca7933e47d3a-1~exp1~20260714135019.80) (`-O3 -std=c++23 -pthread -DNDEBUG`)
- runs: 11 processes per kernel and variant, interleaved in a shuffled order each round (seed 20261106), after one warm-up round
- `perf stat` counters recorded: context-switches, cpu-migrations, page-faults, task-clock; not available: cycles (not supported (no hardware performance counters visible to this VM)), instructions (not supported (no hardware performance counters visible to this VM)), cache-misses (not supported (no hardware performance counters visible to this VM)), branch-misses (not supported (no hardware performance counters visible to this VM))

Times are milliseconds of the kernel alone (set-up excluded), the median of the runs with a bootstrap 95% interval for the median in brackets. Ratios are Codira's median over the baseline's, with a bootstrap 95% interval; above 1 means Codira is slower. RSS is the median peak resident set of the whole process, in MiB.

## Strings: append, concatenation, splitting, search, parsing

| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |
|---|---:|---:|---:|---:|---:|---:|---|
| append_short | 7.71 [7.45, 8.43] | 5.64 [5.53, 6.51] | 12.98 [12.71, 14.11] | 1.37x [1.18, 1.50] | 0.59x [0.54, 0.65] | 13 / 7 / 11 | same |
| append_numbers | 11.54 [10.93, 12.42] | 28.71 [28.32, 29.68] | 18.96 [18.30, 20.11] | 0.40x [0.38, 0.44] | 0.61x [0.56, 0.67] | 15 / 9 / 11 | same |
| concat | 119.77 [115.40, 125.03] | 43.31 [42.58, 45.50] | 20.52 [19.75, 22.16] | 2.77x [2.58, 2.92] | 5.84x [5.41, 6.28] | 12 / 2 / 4 | same |
| split_lines | 7.02 [6.77, 7.33] | 5.85 [5.53, 5.99] | 5.97 [5.48, 6.42] | 1.20x [1.15, 1.28] | 1.18x [1.09, 1.30] | 19 / 15 / 17 | same |
| lines_lazy | 2.92 [2.75, 3.01] | 4.05 [3.93, 4.08] | 2.60 [2.51, 2.70] | 0.72x [0.68, 0.75] | 1.12x [1.03, 1.19] | 14 / 12 / 14 | same |
| split_lazy_count | 18.08 [16.63, 18.20] | 23.86 [22.97, 25.86] | 14.30 [13.46, 15.08] | 0.76x [0.68, 0.79] | 1.26x [1.16, 1.34] | 14 / 12 / 14 | same |
| split_fields | 19.25 [18.40, 20.08] | 22.25 [20.59, 24.63] | 23.19 [21.61, 23.79] | 0.87x [0.76, 0.97] | 0.83x [0.79, 0.90] | 33 / 23 / 25 | same |
| find_10mb | 1.51 [1.37, 1.65] | 1.57 [1.48, 1.78] | 0.62 [0.58, 0.70] | 0.97x [0.81, 1.08] | 2.45x [2.03, 2.77] | 23 / 12 / 22 | same |
| parse_i64 | 7.63 [7.16, 7.77] | 5.63 [5.47, 5.93] | 6.22 [6.11, 7.13] | 1.36x [1.25, 1.42] | 1.23x [1.04, 1.26] | 34 / 24 / 30 | same |

Codira GC (`Runtime::stats()` at the end of the kernel's process; medians over the runs, pauses with a bootstrap 95% interval):

| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) | collections (paced / at cap) | stopped (ms) | to safepoint (ms) | pause p50 (µs) | pause p99 (µs) | pause max (µs) |
|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|
| append_short | 1024 | 11.8 | 3.8 | 15 | 1 (1 / 0) | 0.2 | 0.00 | 221 [189, 359] | 221 [189, 359] | 221 [189, 359] |
| append_numbers | 1024 | 12.3 | 4.0 | 16 | 1 (1 / 0) | 0.2 | 0.00 | 221 [189, 225] | 221 [189, 225] | 221 [189, 225] |
| concat | 1024 | 7.6 | 0.0 | 83 | 20 (20 / 0) | 16.3 | 0.00 | 754 [754, 819] | 1513 [1361, 1687] | 1513 [1361, 1687] |
| split_lines | 1024 | 14.8 | 10.0 | 15 | 1 (1 / 0) | 0.0 | 0.00 | 24 [22, 35] | 24 [22, 35] | 24 [22, 35] |
| lines_lazy | 1024 | 10.1 | 0.0 | 10 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| split_lazy_count | 1024 | 10.1 | 0.0 | 10 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| split_fields | 1024 | 28.4 | 5.5 | 28 | 1 (1 / 0) | 0.0 | 0.00 | 24 [23, 28] | 24 [23, 28] | 24 [23, 28] |
| find_10mb | 1024 | 28.7 | 9.5 | 29 | 1 (1 / 0) | 0.0 | 0.00 | 24 [23, 25] | 24 [23, 25] | 24 [23, 25] |
| parse_i64 | 1024 | 31.3 | 7.9 | 39 | 2 (2 / 0) | 0.5 | 0.00 | 229 [205, 247] | 280 [254, 408] | 280 [254, 408] |

`perf stat` (median per process, whole process including start-up):

| kernel | variant | context-switches | cpu-migrations | page-faults | task-clock |
|---|---|---:|---:|---:|---:|
| append_short | codira | 4 | 0 | 3481 | 10 |
| append_short | rust | 0 | 0 | 1328 | 7 |
| append_short | cpp | 0 | 0 | 3276 | 15 |
| append_numbers | codira | 4 | 0 | 4039 | 14 |
| append_numbers | rust | 0 | 0 | 1790 | 30 |
| append_numbers | cpp | 1 | 0 | 3737 | 21 |
| concat | codira | 5 | 0 | 3831 | 122 |
| concat | rust | 0 | 0 | 78 | 44 |
| concat | cpp | 1 | 0 | 132 | 23 |
| split_lines | codira | 4 | 0 | 3989 | 14 |
| split_lines | rust | 1 | 0 | 3472 | 13 |
| split_lines | cpp | 0 | 0 | 4522 | 13 |
| lines_lazy | codira | 4 | 0 | 2783 | 10 |
| lines_lazy | rust | 0 | 0 | 2637 | 11 |
| lines_lazy | cpp | 0 | 0 | 2690 | 10 |
| split_lazy_count | codira | 4 | 0 | 2784 | 25 |
| split_lazy_count | rust | 0 | 0 | 2637 | 31 |
| split_lazy_count | cpp | 1 | 0 | 2691 | 21 |
| split_fields | codira | 4 | 0 | 7488 | 25 |
| split_fields | rust | 1 | 0 | 5419 | 27 |
| split_fields | cpp | 1 | 0 | 9544 | 29 |
| find_10mb | codira | 4 | 0 | 5108 | 27 |
| find_10mb | rust | 0 | 0 | 2518 | 9 |
| find_10mb | cpp | 1 | 0 | 5015 | 30 |
| parse_i64 | codira | 4 | 0 | 9898 | 46 |
| parse_i64 | rust | 1 | 0 | 5697 | 59 |
| parse_i64 | cpp | 1 | 0 | 11709 | 53 |

## Formatting: integers, floats, format specs, the debug form

| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |
|---|---:|---:|---:|---:|---:|---:|---|
| int_display | 11.73 [11.18, 12.17] | 22.42 [20.32, 23.62] | 16.09 [15.84, 16.53] | 0.52x [0.49, 0.58] | 0.73x [0.69, 0.76] | 9 / 2 / 4 | same |
| int_zero_8 | 16.81 [15.49, 17.65] | 46.82 [46.21, 48.55] | 68.07 [65.26, 81.21] | 0.36x [0.33, 0.38] | 0.25x [0.21, 0.26] | 9 / 2 / 4 | same |
| float_display | 137.46 [127.42, 154.75] | 140.05 [131.57, 143.32] | 71.03 [68.02, 77.66] | 0.98x [0.91, 1.11] | 1.94x [1.76, 2.18] | 9 / 2 / 4 | same |
| float_right_10_3 | 215.93 [205.06, 230.98] | 201.76 [197.43, 211.02] | 128.63 [120.98, 139.88] | 1.07x [1.00, 1.16] | 1.68x [1.52, 1.89] | 9 / 2 / 4 | same |
| debug_struct | 160.08 [148.81, 165.48] | 263.58 [247.66, 275.93] | 133.74 [130.35, 143.75] | 0.61x [0.56, 0.66] | 1.20x [1.08, 1.26] | 9 / 2 / 4 | same |

Codira GC (`Runtime::stats()` at the end of the kernel's process; medians over the runs, pauses with a bootstrap 95% interval):

| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) | collections (paced / at cap) | stopped (ms) | to safepoint (ms) | pause p50 (µs) | pause p99 (µs) | pause max (µs) |
|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|
| int_display | 1024 | 4.7 | 0.0 | 7 | 1 (1 / 0) | 0.2 | 0.00 | 231 [213, 266] | 231 [213, 266] | 231 [213, 266] |
| int_zero_8 | 1024 | 4.7 | 0.0 | 9 | 2 (2 / 0) | 0.4 | 0.00 | 147 [131, 172] | 214 [168, 315] | 214 [168, 315] |
| float_display | 1024 | 4.7 | 0.0 | 20 | 4 (4 / 0) | 0.8 | 0.00 | 172 [147, 197] | 273 [221, 330] | 273 [221, 330] |
| float_right_10_3 | 1024 | 4.7 | 0.0 | 11 | 2 (2 / 0) | 0.5 | 0.00 | 254 [238, 262] | 287 [249, 300] | 287 [249, 300] |
| debug_struct | 1024 | 4.8 | 0.0 | 45 | 11 (11 / 0) | 1.8 | 0.00 | 156 [139, 172] | 243 [220, 354] | 243 [220, 354] |

`perf stat` (median per process, whole process including start-up):

| kernel | variant | context-switches | cpu-migrations | page-faults | task-clock |
|---|---|---:|---:|---:|---:|
| int_display | codira | 4 | 0 | 1408 | 14 |
| int_display | rust | 0 | 0 | 77 | 24 |
| int_display | cpp | 1 | 0 | 131 | 18 |
| int_zero_8 | codira | 4 | 0 | 1423 | 19 |
| int_zero_8 | rust | 0 | 0 | 77 | 48 |
| int_zero_8 | cpp | 0 | 0 | 131 | 70 |
| float_display | codira | 4 | 0 | 1439 | 140 |
| float_display | rust | 1 | 0 | 77 | 142 |
| float_display | cpp | 1 | 0 | 134 | 73 |
| float_right_10_3 | codira | 5 | 0 | 1423 | 219 |
| float_right_10_3 | rust | 1 | 0 | 77 | 202 |
| float_right_10_3 | cpp | 0 | 0 | 135 | 130 |
| debug_struct | codira | 4 | 0 | 1551 | 162 |
| debug_struct | rust | 1 | 0 | 108 | 265 |
| debug_struct | cpp | 1 | 0 | 133 | 135 |

## Result propagation with `?`

| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |
|---|---:|---:|---:|---:|---:|---:|---|
| wrapped_ok | 36.10 [34.92, 37.01] | 13.05 [12.78, 14.12] | 24.96 [23.35, 26.17] | 2.77x [2.51, 2.88] | 1.45x [1.34, 1.56] | 4 / 2 / 4 | same |
| wrapped_err | 33.61 [33.00, 35.68] | 7.12 [6.85, 7.27] | 22.00 [21.44, 23.20] | 4.72x [4.59, 5.01] | 1.53x [1.44, 1.63] | 4 / 2 / 4 | same |
| converted_ok | 3.65 [3.50, 3.91] | 2.71 [2.67, 2.87] | 3.40 [3.29, 3.51] | 1.34x [1.23, 1.44] | 1.07x [1.01, 1.15] | 4 / 2 / 4 | same |
| converted_err | 279.68 [269.40, 285.03] | 43.92 [42.61, 45.82] | 29.68 [28.09, 31.94] | 6.37x [6.05, 6.62] | 9.42x [8.72, 9.96] | 11 / 2 / 4 | same |
| context_ok | 4.11 [4.02, 4.21] | 2.80 [2.69, 2.94] | 4.19 [4.08, 4.41] | 1.47x [1.38, 1.52] | 0.98x [0.93, 1.02] | 4 / 2 / 4 | same |
| context_err | 594.53 [566.87, 607.60] | 76.18 [75.19, 81.25] | 66.81 [64.59, 68.76] | 7.80x [7.06, 8.00] | 8.90x [8.33, 9.21] | 11 / 2 / 4 | same |
| sum_words | 18.48 [17.90, 19.15] | 18.76 [18.14, 18.99] | 19.81 [18.57, 22.00] | 0.99x [0.95, 1.04] | 0.93x [0.84, 1.01] | 30 / 9 / 14 | same |
| propagate_depth8 | 11.67 [11.30, 12.23] | 10.32 [9.99, 12.28] | 9.19 [9.00, 9.49] | 1.13x [0.95, 1.19] | 1.27x [1.21, 1.33] | 4 / 2 / 3 | same |

Codira GC (`Runtime::stats()` at the end of the kernel's process; medians over the runs, pauses with a bootstrap 95% interval):

| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) | collections (paced / at cap) | stopped (ms) | to safepoint (ms) | pause p50 (µs) | pause p99 (µs) | pause max (µs) |
|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|
| wrapped_ok | 1024 | 0.1 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| wrapped_err | 1024 | 0.1 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| converted_ok | 1024 | 0.1 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| converted_err | 1024 | 6.2 | 0.0 | 435 | 108 (108 / 0) | 50.9 | 0.02 | 442 [442, 442] | 754 [688, 918] | 933 [719, 1438] |
| context_ok | 1024 | 0.1 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| context_err | 1024 | 6.4 | 0.0 | 689 | 168 (168 / 0) | 144.0 | 0.03 | 819 [786, 819] | 1442 [1311, 1573] | 1502 [1348, 3226] |
| sum_words | 1024 | 26.8 | 8.0 | 46 | 8 (8 / 0) | 8.5 | 0.00 | 590 [508, 754] | 3235 [3073, 3484] | 3235 [3073, 3484] |
| propagate_depth8 | 1024 | 0.0 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |

`perf stat` (median per process, whole process including start-up):

| kernel | variant | context-switches | cpu-migrations | page-faults | task-clock |
|---|---|---:|---:|---:|---:|
| wrapped_ok | codira | 13 | 0 | 247 | 39 |
| wrapped_ok | rust | 0 | 0 | 79 | 14 |
| wrapped_ok | cpp | 1 | 0 | 131 | 26 |
| wrapped_err | codira | 13 | 0 | 247 | 37 |
| wrapped_err | rust | 0 | 0 | 79 | 8 |
| wrapped_err | cpp | 0 | 0 | 131 | 23 |
| converted_ok | codira | 13 | 0 | 248 | 6 |
| converted_ok | rust | 0 | 0 | 79 | 4 |
| converted_ok | cpp | 0 | 0 | 131 | 5 |
| converted_err | codira | 14 | 0 | 10009 | 285 |
| converted_err | rust | 1 | 0 | 79 | 45 |
| converted_err | cpp | 0 | 0 | 131 | 31 |
| context_ok | codira | 13 | 0 | 246 | 7 |
| context_ok | rust | 0 | 0 | 79 | 4 |
| context_ok | cpp | 0 | 0 | 130 | 6 |
| context_err | codira | 15 | 0 | 2102 | 597 |
| context_err | rust | 1 | 0 | 79 | 77 |
| context_err | cpp | 1 | 0 | 131 | 68 |
| sum_words | codira | 13 | 0 | 6936 | 101 |
| sum_words | rust | 1 | 0 | 1886 | 83 |
| sum_words | cpp | 1 | 0 | 4687 | 62 |
| propagate_depth8 | codira | 13 | 0 | 231 | 15 |
| propagate_depth8 | rust | 0 | 0 | 79 | 11 |
| propagate_depth8 | cpp | 0 | 0 | 130 | 11 |

## Arrays and pipelines

| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |
|---|---:|---:|---:|---:|---:|---:|---|
| push_10m | 45.28 [43.46, 45.99] | 41.74 [40.36, 43.87] | 121.64 [110.37, 127.79] | 1.08x [1.01, 1.12] | 0.37x [0.35, 0.41] | 81 / 79 / 131 | same |
| sum_for | 8.97 [8.65, 9.56] | 9.07 [8.77, 9.25] | 8.93 [8.09, 9.32] | 0.99x [0.95, 1.07] | 1.00x [0.95, 1.11] | 80 / 78 / 80 | same |
| sort_1m | 19.97 [19.57, 20.02] | 21.39 [19.44, 24.52] | 81.27 [79.49, 93.03] | 0.93x [0.81, 1.03] | 0.25x [0.21, 0.25] | 12 / 10 / 11 | same |
| map_filter_sum_eager | 80.44 [77.98, 91.47] | 59.28 [58.94, 60.74] | 91.86 [88.81, 96.17] | 1.36x [1.30, 1.55] | 0.88x [0.83, 1.00] | 182 / 155 / 188 | same |
| map_filter_sum_lazy | 15.82 [15.23, 16.16] | 15.25 [14.95, 16.11] | 15.07 [14.28, 15.35] | 1.04x [0.96, 1.07] | 1.05x [1.00, 1.11] | 80 / 78 / 80 | same |
| map_filter_sum_fold | 15.58 [15.37, 16.40] | 15.88 [15.65, 17.20] | 16.12 [15.02, 16.69] | 0.98x [0.91, 1.04] | 0.97x [0.93, 1.05] | 80 / 78 / 80 | same |
| enumerate_filter_count | 15.49 [15.26, 16.76] | 15.45 [14.72, 16.44] | 38.07 [36.73, 39.50] | 1.00x [0.94, 1.10] | 0.41x [0.39, 0.45] | 81 / 78 / 131 | same |
| zip_fold_dot | 16.10 [15.41, 17.39] | 16.04 [15.64, 16.90] | 15.74 [15.36, 16.31] | 1.00x [0.95, 1.08] | 1.02x [0.95, 1.10] | 157 / 155 / 208 | same |
| take_while_sum | 14.13 [13.47, 15.21] | 14.14 [13.56, 14.40] | 13.42 [12.86, 14.37] | 1.00x [0.95, 1.08] | 1.05x [0.98, 1.16] | 80 / 78 / 80 | same |
| map_collect | 43.58 [41.83, 44.83] | 41.56 [41.11, 47.63] | 47.86 [46.22, 51.31] | 1.05x [0.92, 1.09] | 0.91x [0.83, 0.95] | 157 / 155 / 156 | same |
| filter_collect | 59.92 [58.99, 60.95] | 64.51 [61.06, 66.50] | 111.36 [108.26, 114.88] | 0.93x [0.90, 0.99] | 0.54x [0.52, 0.56] | 106 / 104 / 131 | same |
| range_filter_map_sum | 6.00 [5.89, 6.15] | 7.90 [7.20, 8.08] | 10.53 [9.96, 10.88] | 0.76x [0.74, 0.83] | 0.57x [0.55, 0.61] | 4 / 2 / 4 | same |
| chain_filter_count | 37.75 [37.15, 41.54] | 37.05 [36.80, 37.59] | 37.38 [35.65, 37.93] | 1.02x [0.99, 1.12] | 1.01x [0.98, 1.14] | 157 / 155 / 208 | same |

Codira GC (`Runtime::stats()` at the end of the kernel's process; medians over the runs, pauses with a bootstrap 95% interval):

| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) | collections (paced / at cap) | stopped (ms) | to safepoint (ms) | pause p50 (µs) | pause p99 (µs) | pause max (µs) |
|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|
| push_10m | 1024 | 128.1 | 64.0 | 256 | 3 (3 / 0) | 0.1 | 0.00 | 24 [21, 26] | 26 [24, 32] | 26 [24, 32] |
| sum_for | 1024 | 76.4 | 0.0 | 76 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| sort_1m | 1024 | 8.1 | 4.0 | 16 | 1 (1 / 0) | 0.0 | 0.00 | 25 [23, 26] | 25 [23, 26] | 25 [23, 26] |
| map_filter_sum_eager | 1024 | 228.9 | 76.3 | 229 | 1 (1 / 0) | 0.0 | 0.00 | 30 [28, 33] | 30 [28, 33] | 30 [28, 33] |
| map_filter_sum_lazy | 1024 | 76.4 | 0.0 | 76 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| map_filter_sum_fold | 1024 | 76.4 | 0.0 | 76 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| enumerate_filter_count | 1024 | 128.1 | 64.0 | 256 | 3 (3 / 0) | 0.1 | 0.00 | 26 [23, 29] | 28 [26, 34] | 28 [26, 34] |
| zip_fold_dot | 1024 | 256.1 | 128.0 | 512 | 4 (4 / 0) | 0.1 | 0.00 | 24 [20, 27] | 28 [28, 40] | 28 [28, 40] |
| take_while_sum | 1024 | 76.4 | 0.0 | 76 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| map_collect | 1024 | 152.7 | 76.3 | 153 | 1 (1 / 0) | 0.0 | 0.00 | 29 [28, 33] | 29 [28, 33] | 29 [28, 33] |
| filter_collect | 1024 | 160.1 | 128.0 | 320 | 4 (4 / 0) | 0.1 | 0.00 | 23 [20, 26] | 27 [23, 29] | 27 [23, 29] |
| range_filter_map_sum | 1024 | 0.0 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| chain_filter_count | 1024 | 204.4 | 76.3 | 332 | 1 (1 / 0) | 0.0 | 0.00 | 30 [29, 35] | 30 [29, 35] | 30 [29, 35] |

`perf stat` (median per process, whole process including start-up):

| kernel | variant | context-switches | cpu-migrations | page-faults | task-clock |
|---|---|---:|---:|---:|---:|
| push_10m | codira | 4 | 0 | 19786 | 48 |
| push_10m | rust | 0 | 0 | 19641 | 46 |
| push_10m | cpp | 1 | 0 | 52440 | 126 |
| sum_for | codira | 4 | 0 | 19754 | 56 |
| sum_for | rust | 1 | 0 | 19610 | 49 |
| sum_for | cpp | 1 | 0 | 19663 | 67 |
| sort_1m | codira | 4 | 0 | 2208 | 29 |
| sort_1m | rust | 1 | 0 | 2032 | 30 |
| sort_1m | cpp | 1 | 0 | 4138 | 93 |
| map_filter_sum_eager | codira | 4 | 0 | 45796 | 129 |
| map_filter_sum_eager | rust | 1 | 0 | 39142 | 99 |
| map_filter_sum_eager | cpp | 0 | 0 | 53905 | 151 |
| map_filter_sum_lazy | codira | 4 | 0 | 19754 | 66 |
| map_filter_sum_lazy | rust | 1 | 0 | 19609 | 57 |
| map_filter_sum_lazy | cpp | 1 | 0 | 19663 | 72 |
| map_filter_sum_fold | codira | 4 | 0 | 19754 | 65 |
| map_filter_sum_fold | rust | 0 | 0 | 19609 | 56 |
| map_filter_sum_fold | cpp | 1 | 0 | 19663 | 72 |
| enumerate_filter_count | codira | 4 | 0 | 19785 | 82 |
| enumerate_filter_count | rust | 1 | 0 | 19610 | 76 |
| enumerate_filter_count | cpp | 1 | 0 | 52439 | 181 |
| zip_fold_dot | codira | 5 | 0 | 39318 | 142 |
| zip_fold_dot | rust | 0 | 0 | 39141 | 137 |
| zip_fold_dot | cpp | 1 | 0 | 104710 | 294 |
| take_while_sum | codira | 4 | 1 | 19753 | 63 |
| take_while_sum | rust | 1 | 0 | 19610 | 54 |
| take_while_sum | cpp | 1 | 0 | 19663 | 70 |
| map_collect | codira | 4 | 0 | 39286 | 89 |
| map_collect | rust | 1 | 0 | 39142 | 83 |
| map_collect | cpp | 1 | 0 | 39195 | 107 |
| filter_collect | codira | 4 | 0 | 26307 | 126 |
| filter_collect | rust | 0 | 0 | 26161 | 126 |
| filter_collect | cpp | 1 | 0 | 58961 | 255 |
| range_filter_map_sum | codira | 4 | 0 | 205 | 8 |
| range_filter_map_sum | rust | 0 | 0 | 78 | 9 |
| range_filter_map_sum | cpp | 0 | 0 | 131 | 12 |
| chain_filter_count | codira | 4 | 0 | 39318 | 154 |
| chain_filter_count | rust | 1 | 0 | 39141 | 138 |
| chain_filter_count | cpp | 1 | 0 | 71971 | 239 |

- `push_10m`: libstdc++'s `std::vector` grows by allocating new memory and copying; Rust's `Vec` grows with `realloc`, which can remap the pages in place.
- `sort_1m`: C++ is libstdc++'s `std::sort` (introsort); Rust's `sort_unstable` and Codira's `sort` are pattern-defeating quicksort (Codira's is Rust's).
- `chain_filter_count`: C++ has no `views::concat` before C++26; it is two loops.

## Numeric loops and calls

| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |
|---|---:|---:|---:|---:|---:|---:|---|
| sum_for | 8.54 [8.25, 8.94] | 9.37 [8.99, 9.54] | 9.14 [8.09, 9.66] | 0.91x [0.87, 0.98] | 0.93x [0.87, 1.07] | 80 / 78 / 80 | same |
| sum_indexed | 8.98 [7.76, 9.26] | 9.32 [8.89, 9.52] | 9.46 [8.45, 10.49] | 0.96x [0.83, 1.01] | 0.95x [0.80, 1.06] | 80 / 78 / 80 | same |
| store_field | 22.86 [22.46, 24.24] | 9.16 [8.90, 9.63] | 8.79 [8.04, 9.42] | 2.50x [2.37, 2.66] | 2.60x [2.43, 2.88] | 80 / 78 / 80 | same |
| copy_plus_one | 18.67 [18.10, 19.79] | 14.92 [14.64, 15.84] | 14.09 [13.85, 14.36] | 1.25x [1.18, 1.34] | 1.32x [1.28, 1.41] | 157 / 155 / 156 | same |
| fib_32 | 6.98 [6.55, 7.04] | 7.22 [6.98, 7.30] | 7.50 [7.40, 7.61] | 0.97x [0.91, 1.01] | 0.93x [0.87, 0.95] | 4 / 2 / 4 | same |
| sieve_20m | 125.31 [122.05, 127.34] | 112.68 [108.89, 116.34] | 111.50 [110.08, 122.03] | 1.11x [1.06, 1.15] | 1.12x [1.02, 1.14] | 23 / 21 / 23 | same |
| mandelbrot | 45.54 [43.90, 47.27] | 42.20 [41.53, 45.91] | 44.87 [43.05, 46.99] | 1.08x [0.99, 1.12] | 1.02x [0.95, 1.07] | 4 / 2 / 4 | same |
| matmul_200 | 7.05 [6.92, 7.72] | 2.37 [2.25, 2.48] | 1.84 [1.70, 1.90] | 2.97x [2.83, 3.32] | 3.84x [3.68, 4.29] | 5 / 3 / 4 | same |
| collatz_1m | 186.01 [180.53, 186.85] | 187.05 [183.87, 191.23] | 184.85 [182.03, 190.22] | 0.99x [0.96, 1.01] | 1.01x [0.97, 1.03] | 4 / 2 / 4 | same |

Codira GC (`Runtime::stats()` at the end of the kernel's process; medians over the runs, pauses with a bootstrap 95% interval):

| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) | collections (paced / at cap) | stopped (ms) | to safepoint (ms) | pause p50 (µs) | pause p99 (µs) | pause max (µs) |
|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|
| sum_for | 1024 | 76.3 | 0.0 | 76 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| sum_indexed | 1024 | 76.3 | 0.0 | 76 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| store_field | 1024 | 76.4 | 76.3 | 76 | 1 (1 / 0) | 0.0 | 0.00 | 19 [18, 22] | 19 [18, 22] | 19 [18, 22] |
| copy_plus_one | 1024 | 152.6 | 76.3 | 153 | 1 (1 / 0) | 0.0 | 0.00 | 18 [16, 23] | 18 [16, 23] | 18 [16, 23] |
| fib_32 | 1024 | 0.0 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| sieve_20m | 1024 | 19.1 | 0.0 | 19 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| mandelbrot | 1024 | 0.0 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| matmul_200 | 1024 | 0.9 | 0.0 | 1 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| collatz_1m | 1024 | 0.0 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |

`perf stat` (median per process, whole process including start-up):

| kernel | variant | context-switches | cpu-migrations | page-faults | task-clock |
|---|---|---:|---:|---:|---:|
| sum_for | codira | 4 | 0 | 19735 | 63 |
| sum_for | rust | 1 | 0 | 19610 | 49 |
| sum_for | cpp | 0 | 0 | 19662 | 66 |
| sum_indexed | codira | 4 | 0 | 19735 | 64 |
| sum_indexed | rust | 1 | 0 | 19609 | 48 |
| sum_indexed | cpp | 0 | 0 | 19663 | 67 |
| store_field | codira | 4 | 0 | 19751 | 83 |
| store_field | rust | 1 | 0 | 19610 | 49 |
| store_field | cpp | 1 | 0 | 19662 | 66 |
| copy_plus_one | codira | 4 | 0 | 39268 | 113 |
| copy_plus_one | rust | 1 | 0 | 39142 | 103 |
| copy_plus_one | cpp | 1 | 0 | 39194 | 114 |
| fib_32 | codira | 4 | 0 | 203 | 9 |
| fib_32 | rust | 0 | 0 | 78 | 8 |
| fib_32 | cpp | 0 | 0 | 130 | 9 |
| sieve_20m | codira | 4 | 0 | 5085 | 128 |
| sieve_20m | rust | 1 | 0 | 4961 | 115 |
| sieve_20m | cpp | 1 | 0 | 5013 | 114 |
| mandelbrot | codira | 4 | 0 | 202 | 48 |
| mandelbrot | rust | 1 | 0 | 78 | 43 |
| mandelbrot | cpp | 1 | 0 | 131 | 47 |
| matmul_200 | codira | 4 | 0 | 440 | 10 |
| matmul_200 | rust | 0 | 0 | 393 | 4 |
| matmul_200 | cpp | 0 | 0 | 368 | 4 |
| collatz_1m | codira | 4 | 0 | 202 | 188 |
| collatz_1m | rust | 1 | 0 | 78 | 188 |
| collatz_1m | cpp | 1 | 0 | 131 | 186 |

## Hash maps: the same unkeyed hash, and the default keyed (secure) hash, separately

| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |
|---|---:|---:|---:|---:|---:|---:|---|
| insert_i64_fast_1k | 31.65 [30.70, 38.03] | 19.96 [19.44, 20.43] | 61.07 [59.63, 64.82] | 1.59x [1.53, 1.94] | 0.52x [0.49, 0.63] | 9 / 2 / 4 | same |
| hit_i64_fast_1k | 4.13 [4.01, 4.27] | 3.27 [3.18, 3.86] | 5.57 [5.20, 5.72] | 1.26x [1.07, 1.31] | 0.74x [0.71, 0.80] | 5 / 2 / 4 | same |
| miss_i64_fast_1k | 2.88 [2.66, 4.00] | 3.03 [2.96, 3.08] | 6.92 [6.88, 7.82] | 0.95x [0.88, 1.32] | 0.42x [0.37, 0.58] | 5 / 2 / 4 | same |
| iter_i64_fast_1k | 3.58 [3.33, 3.76] | 2.26 [2.21, 2.32] | 2.12 [2.05, 2.48] | 1.59x [1.47, 1.67] | 1.69x [1.40, 1.78] | 5 / 2 / 4 | same |
| remove_i64_fast_1k | 8.23 [7.31, 9.57] | 5.80 [5.57, 5.95] | 19.91 [18.52, 20.19] | 1.42x [1.23, 1.65] | 0.41x [0.36, 0.48] | 5 / 2 / 4 | same |
| insert_i64_fast_100k | 33.83 [33.19, 35.41] | 33.59 [33.23, 36.34] | 146.73 [144.18, 152.32] | 1.01x [0.93, 1.06] | 0.23x [0.22, 0.24] | 14 / 9 / 13 | same |
| hit_i64_fast_100k | 9.22 [8.84, 9.66] | 6.49 [6.11, 6.88] | 19.18 [18.22, 19.88] | 1.42x [1.33, 1.55] | 0.48x [0.45, 0.51] | 10 / 8 / 13 | same |
| miss_i64_fast_100k | 14.88 [14.63, 17.48] | 9.03 [8.79, 9.39] | 27.81 [26.52, 29.23] | 1.65x [1.57, 1.93] | 0.54x [0.51, 0.64] | 10 / 8 / 12 | same |
| iter_i64_fast_100k | 4.41 [4.17, 4.75] | 2.38 [2.28, 2.57] | 23.24 [22.34, 24.51] | 1.85x [1.69, 2.02] | 0.19x [0.17, 0.20] | 10 / 8 / 13 | same |
| remove_i64_fast_100k | 19.87 [19.24, 20.98] | 16.08 [15.68, 18.48] | 41.04 [39.91, 42.66] | 1.24x [1.08, 1.32] | 0.48x [0.45, 0.51] | 10 / 8 / 12 | same |
| insert_i64_fast_1m | 73.10 [72.49, 79.24] | 65.78 [64.53, 71.07] | 244.55 [234.25, 266.73] | 1.11x [1.02, 1.21] | 0.30x [0.27, 0.34] | 64 / 84 / 91 | same |
| hit_i64_fast_1m | 23.12 [22.34, 24.93] | 21.45 [19.99, 24.43] | 40.42 [38.42, 42.15] | 1.08x [0.94, 1.22] | 0.57x [0.54, 0.63] | 80 / 84 / 91 | same |
| miss_i64_fast_1m | 9.40 [9.02, 10.07] | 9.69 [9.35, 9.87] | 59.09 [56.57, 61.77] | 0.97x [0.92, 1.04] | 0.16x [0.15, 0.17] | 80 / 84 / 91 | same |
| iter_i64_fast_1m | 7.81 [7.18, 8.98] | 5.29 [5.03, 5.50] | 81.69 [76.37, 93.52] | 1.48x [1.36, 1.71] | 0.10x [0.08, 0.11] | 80 / 84 / 91 | same |
| remove_i64_fast_1m | 39.60 [35.41, 42.06] | 32.57 [30.98, 35.97] | 119.99 [105.95, 140.11] | 1.22x [1.06, 1.31] | 0.33x [0.27, 0.37] | 80 / 84 / 91 | same |
| insert_string_fast_1k | 88.34 [86.90, 91.05] | 91.10 [88.08, 98.31] | 120.45 [116.72, 127.30] | 0.97x [0.90, 1.02] | 0.73x [0.69, 0.77] | 9 / 2 / 4 | same |
| hit_string_fast_1k | 21.20 [20.63, 25.01] | 21.33 [19.54, 25.21] | 25.94 [23.21, 30.88] | 0.99x [0.84, 1.17] | 0.82x [0.67, 0.96] | 5 / 2 / 4 | same |
| miss_string_fast_1k | 17.16 [16.70, 18.12] | 15.55 [15.13, 19.66] | 24.41 [23.62, 25.74] | 1.10x [0.87, 1.19] | 0.70x [0.66, 0.75] | 5 / 2 / 4 | same |
| iter_string_fast_1k | 3.96 [3.78, 4.05] | 1.24 [1.19, 1.36] | 5.73 [5.64, 6.14] | 3.19x [2.88, 3.32] | 0.69x [0.64, 0.71] | 5 / 2 / 4 | same |
| remove_string_fast_1k | 29.68 [24.92, 34.70] | 36.09 [35.24, 40.84] | 63.00 [56.30, 65.12] | 0.82x [0.65, 0.96] | 0.47x [0.40, 0.56] | 5 / 2 / 4 | same |
| insert_string_fast_100k | 105.16 [102.09, 107.57] | 129.98 [121.46, 137.70] | 306.31 [296.12, 313.11] | 0.81x [0.76, 0.87] | 0.34x [0.33, 0.36] | 28 / 25 / 31 | same |
| hit_string_fast_100k | 38.06 [36.19, 40.83] | 34.70 [32.25, 40.41] | 63.99 [60.74, 68.13] | 1.10x [0.94, 1.23] | 0.59x [0.56, 0.64] | 24 / 22 / 31 | same |
| miss_string_fast_100k | 40.96 [38.61, 43.88] | 31.33 [29.21, 35.15] | 74.08 [72.40, 76.03] | 1.31x [1.17, 1.43] | 0.55x [0.51, 0.59] | 24 / 22 / 31 | same |
| iter_string_fast_100k | 4.49 [4.33, 4.77] | 2.39 [2.34, 2.45] | 33.87 [32.69, 34.84] | 1.88x [1.78, 2.00] | 0.13x [0.12, 0.14] | 24 / 22 / 31 | same |
| remove_string_fast_100k | 48.89 [47.96, 51.71] | 71.95 [68.93, 72.60] | 101.80 [94.23, 107.53] | 0.68x [0.66, 0.72] | 0.48x [0.45, 0.52] | 24 / 22 / 31 | same |
| insert_string_fast_1m | 257.43 [254.04, 268.51] | 280.98 [260.18, 325.21] | 503.67 [476.44, 550.56] | 0.92x [0.79, 1.02] | 0.51x [0.46, 0.55] | 254 / 251 / 274 | same |
| hit_string_fast_1m | 95.65 [89.73, 100.15] | 100.77 [95.13, 109.73] | 151.14 [140.57, 160.71] | 0.95x [0.83, 1.03] | 0.63x [0.58, 0.69] | 254 / 251 / 274 | same |
| miss_string_fast_1m | 40.30 [39.34, 42.59] | 51.01 [49.63, 58.05] | 216.75 [212.72, 232.87] | 0.79x [0.69, 0.84] | 0.19x [0.17, 0.20] | 254 / 251 / 274 | same |
| iter_string_fast_1m | 44.32 [43.40, 45.45] | 8.06 [7.98, 8.46] | 114.43 [104.67, 119.17] | 5.50x [5.23, 5.64] | 0.39x [0.37, 0.43] | 254 / 251 / 274 | same |
| remove_string_fast_1m | 112.65 [99.22, 123.36] | 162.30 [156.96, 174.55] | 262.75 [247.08, 277.74] | 0.69x [0.60, 0.76] | 0.43x [0.37, 0.47] | 254 / 251 / 274 | same |
| small_build_fast | 47.20 [44.28, 49.58] | 25.53 [24.71, 25.78] | 25.30 [24.42, 26.42] | 1.85x [1.73, 1.95] | 1.87x [1.68, 2.02] | 11 / 2 / 4 | same |
| small_get_fast | 4.20 [4.08, 4.45] | 3.53 [3.51, 3.64] | 4.48 [4.37, 4.70] | 1.19x [1.14, 1.26] | 0.94x [0.89, 0.99] | 5 / 2 / 4 | same |
| churn_fast | 99.12 [91.55, 102.47] | 71.42 [66.99, 80.73] | 282.76 [260.33, 301.07] | 1.39x [1.18, 1.49] | 0.35x [0.31, 0.38] | 12 / 9 / 8 | same |
| insert_i64_secure_1k | 59.99 [56.61, 60.23] | 42.58 [41.17, 48.46] | n/a | 1.41x [1.20, 1.46] | – | 9 / 2 / – | same |
| hit_i64_secure_1k | 11.55 [11.00, 12.38] | 9.77 [9.45, 10.28] | n/a | 1.18x [1.11, 1.27] | – | 5 / 2 / – | same |
| miss_i64_secure_1k | 9.11 [8.89, 9.38] | 8.99 [8.83, 9.63] | n/a | 1.01x [0.93, 1.05] | – | 5 / 2 / – | same |
| iter_i64_secure_1k | 3.84 [3.45, 5.95] | 2.34 [2.19, 2.52] | n/a | 1.64x [1.43, 2.59] | – | 5 / 2 / – | same |
| remove_i64_secure_1k | 14.78 [14.48, 15.20] | 12.72 [11.82, 13.20] | n/a | 1.16x [1.12, 1.26] | – | 5 / 2 / – | same |
| insert_i64_secure_100k | 49.61 [46.42, 51.08] | 48.79 [46.95, 52.58] | n/a | 1.02x [0.93, 1.07] | – | 14 / 9 / – | same |
| hit_i64_secure_100k | 20.43 [20.17, 22.98] | 17.56 [16.61, 18.64] | n/a | 1.16x [1.10, 1.31] | – | 10 / 8 / – | same |
| miss_i64_secure_100k | 21.59 [21.30, 22.45] | 15.86 [15.14, 16.43] | n/a | 1.36x [1.31, 1.44] | – | 10 / 8 / – | same |
| iter_i64_secure_100k | 4.44 [4.28, 4.68] | 2.24 [2.21, 2.33] | n/a | 1.98x [1.87, 2.09] | – | 10 / 8 / – | same |
| remove_i64_secure_100k | 30.47 [29.51, 31.69] | 26.62 [25.05, 28.58] | n/a | 1.14x [1.05, 1.23] | – | 10 / 8 / – | same |
| insert_i64_secure_1m | 100.31 [97.00, 111.48] | 88.20 [84.56, 91.10] | n/a | 1.14x [1.09, 1.27] | – | 64 / 84 / – | same |
| hit_i64_secure_1m | 57.94 [53.79, 69.33] | 62.65 [59.47, 67.90] | n/a | 0.92x [0.84, 1.14] | – | 80 / 84 / – | same |
| miss_i64_secure_1m | 20.16 [19.54, 21.68] | 22.27 [21.10, 22.86] | n/a | 0.91x [0.87, 0.98] | – | 80 / 84 / – | same |
| iter_i64_secure_1m | 8.04 [7.59, 9.14] | 5.52 [5.21, 6.01] | n/a | 1.46x [1.31, 1.71] | – | 80 / 84 / – | same |
| remove_i64_secure_1m | 71.86 [67.50, 83.28] | 63.57 [59.78, 69.68] | n/a | 1.13x [1.03, 1.33] | – | 80 / 84 / – | same |
| insert_string_secure_1k | 88.23 [84.56, 93.92] | 115.22 [111.77, 128.71] | n/a | 0.77x [0.67, 0.82] | – | 9 / 2 / – | same |
| hit_string_secure_1k | 18.90 [17.95, 21.21] | 19.14 [17.94, 19.41] | n/a | 0.99x [0.94, 1.14] | – | 5 / 2 / – | same |
| miss_string_secure_1k | 16.40 [16.14, 17.36] | 15.59 [15.10, 16.10] | n/a | 1.05x [1.02, 1.13] | – | 5 / 2 / – | same |
| iter_string_secure_1k | 3.99 [3.67, 4.52] | 1.25 [1.18, 1.31] | n/a | 3.19x [2.91, 3.70] | – | 5 / 2 / – | same |
| remove_string_secure_1k | 24.00 [23.08, 24.47] | 31.30 [30.12, 33.23] | n/a | 0.77x [0.72, 0.80] | – | 5 / 2 / – | same |
| insert_string_secure_100k | 106.67 [100.24, 109.87] | 174.99 [170.27, 186.78] | n/a | 0.61x [0.56, 0.63] | – | 28 / 25 / – | same |
| hit_string_secure_100k | 41.33 [37.50, 49.89] | 40.48 [38.25, 41.63] | n/a | 1.02x [0.91, 1.25] | – | 24 / 22 / – | same |
| miss_string_secure_100k | 35.35 [34.84, 37.13] | 31.80 [31.24, 32.63] | n/a | 1.11x [1.07, 1.17] | – | 24 / 22 / – | same |
| iter_string_secure_100k | 4.53 [4.41, 4.80] | 2.40 [2.23, 2.59] | n/a | 1.89x [1.75, 2.07] | – | 24 / 22 / – | same |
| remove_string_secure_100k | 42.92 [40.67, 44.15] | 64.81 [62.31, 70.40] | n/a | 0.66x [0.59, 0.70] | – | 24 / 22 / – | same |
| insert_string_secure_1m | 327.34 [323.03, 342.71] | 452.83 [434.77, 482.83] | n/a | 0.72x [0.68, 0.77] | – | 254 / 251 / – | same |
| hit_string_secure_1m | 103.45 [97.23, 117.30] | 129.65 [123.19, 140.93] | n/a | 0.80x [0.71, 0.91] | – | 254 / 251 / – | same |
| miss_string_secure_1m | 41.34 [41.08, 44.96] | 62.70 [58.66, 66.53] | n/a | 0.66x [0.62, 0.75] | – | 254 / 251 / – | same |
| iter_string_secure_1m | 43.58 [42.11, 44.38] | 8.20 [8.03, 8.58] | n/a | 5.31x [5.05, 5.45] | – | 254 / 251 / – | same |
| remove_string_secure_1m | 112.75 [108.36, 127.27] | 149.98 [146.08, 161.73] | n/a | 0.75x [0.70, 0.86] | – | 254 / 251 / – | same |
| small_build_secure | 53.23 [52.42, 55.58] | 29.54 [28.13, 30.73] | n/a | 1.80x [1.73, 1.95] | – | 11 / 2 / – | same |
| small_get_secure | 4.32 [4.09, 4.86] | 13.76 [13.05, 14.13] | n/a | 0.31x [0.30, 0.35] | – | 5 / 2 / – | same |
| churn_secure | 165.73 [161.50, 182.51] | 172.43 [166.14, 180.79] | n/a | 0.96x [0.91, 1.07] | – | 12 / 9 / – | same |

Codira GC (`Runtime::stats()` at the end of the kernel's process; medians over the runs, pauses with a bootstrap 95% interval):

| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) | collections (paced / at cap) | stopped (ms) | to safepoint (ms) | pause p50 (µs) | pause p99 (µs) | pause max (µs) |
|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|
| insert_i64_fast_1k | 1024 | 4.5 | 0.1 | 67 | 16 (16 / 0) | 1.3 | 0.00 | 74 [70, 94] | 137 [103, 146] | 137 [103, 146] |
| hit_i64_fast_1k | 1024 | 0.6 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| miss_i64_fast_1k | 1024 | 0.6 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| iter_i64_fast_1k | 1024 | 0.6 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| remove_i64_fast_1k | 1024 | 0.6 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| insert_i64_fast_100k | 1024 | 7.9 | 3.1 | 47 | 10 (10 / 0) | 0.8 | 0.00 | 45 [37, 49] | 164 [154, 186] | 164 [154, 186] |
| hit_i64_fast_100k | 1024 | 5.7 | 3.1 | 8 | 1 (1 / 0) | 0.1 | 0.00 | 120 [119, 150] | 120 [119, 150] | 120 [119, 150] |
| miss_i64_fast_100k | 1024 | 5.7 | 3.1 | 8 | 1 (1 / 0) | 0.1 | 0.00 | 126 [115, 150] | 126 [115, 150] | 126 [115, 150] |
| iter_i64_fast_100k | 1024 | 5.7 | 3.1 | 8 | 1 (1 / 0) | 0.1 | 0.00 | 122 [115, 127] | 122 [115, 127] | 122 [115, 127] |
| remove_i64_fast_100k | 1024 | 5.7 | 3.1 | 8 | 1 (1 / 0) | 0.1 | 0.00 | 119 [108, 123] | 119 [108, 123] | 119 [108, 123] |
| insert_i64_fast_1m | 1024 | 59.5 | 25.0 | 100 | 5 (5 / 0) | 1.2 | 0.00 | 30 [25, 53] | 749 [346, 784] | 749 [346, 784] |
| hit_i64_fast_1m | 1024 | 76.0 | 24.5 | 100 | 4 (4 / 0) | 0.6 | 0.00 | 27 [25, 33] | 451 [406, 667] | 451 [406, 667] |
| miss_i64_fast_1m | 1024 | 76.0 | 24.5 | 100 | 4 (4 / 0) | 0.6 | 0.00 | 27 [24, 35] | 459 [441, 524] | 459 [441, 524] |
| iter_i64_fast_1m | 1024 | 76.0 | 50.0 | 100 | 5 (5 / 0) | 1.8 | 0.00 | 28 [27, 35] | 1161 [1070, 1945] | 1161 [1070, 1945] |
| remove_i64_fast_1m | 1024 | 76.0 | 24.5 | 100 | 4 (4 / 0) | 0.5 | 0.00 | 28 [25, 33] | 459 [446, 487] | 459 [446, 487] |
| insert_string_fast_1k | 1024 | 4.5 | 0.1 | 129 | 32 (32 / 0) | 2.0 | 0.01 | 59 [57, 63] | 101 [91, 143] | 101 [91, 143] |
| hit_string_fast_1k | 1024 | 0.7 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| miss_string_fast_1k | 1024 | 0.7 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| iter_string_fast_1k | 1024 | 0.8 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| remove_string_fast_1k | 1024 | 0.7 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| insert_string_fast_100k | 1024 | 24.8 | 9.5 | 103 | 9 (9 / 0) | 7.3 | 0.00 | 819 [786, 885] | 1324 [1248, 1386] | 1324 [1248, 1386] |
| hit_string_fast_100k | 1024 | 21.3 | 15.9 | 29 | 3 (3 / 0) | 2.5 | 0.00 | 655 [557, 754] | 1473 [1366, 1722] | 1473 [1366, 1722] |
| miss_string_fast_100k | 1024 | 21.3 | 15.9 | 29 | 3 (3 / 0) | 2.6 | 0.00 | 655 [623, 786] | 1602 [1514, 1687] | 1602 [1514, 1687] |
| iter_string_fast_100k | 1024 | 21.3 | 15.9 | 29 | 3 (3 / 0) | 2.5 | 0.00 | 623 [590, 655] | 1479 [1418, 1615] | 1479 [1418, 1615] |
| remove_string_fast_100k | 1024 | 21.3 | 15.9 | 29 | 3 (3 / 0) | 2.8 | 0.00 | 655 [623, 786] | 1602 [1465, 1826] | 1602 [1465, 1826] |
| insert_string_fast_1m | 1024 | 252.2 | 101.0 | 309 | 5 (5 / 0) | 17.7 | 0.00 | 885 [819, 950] | 12689 [11999, 14617] | 12689 [11999, 14617] |
| hit_string_fast_1m | 1024 | 252.2 | 101.0 | 309 | 5 (5 / 0) | 17.5 | 0.00 | 885 [819, 918] | 12376 [11722, 13064] | 12376 [11722, 13064] |
| miss_string_fast_1m | 1024 | 252.2 | 101.0 | 309 | 5 (5 / 0) | 16.9 | 0.00 | 885 [852, 918] | 12019 [11069, 13141] | 12019 [11069, 13141] |
| iter_string_fast_1m | 1024 | 252.2 | 178.6 | 309 | 6 (6 / 0) | 50.4 | 0.00 | 885 [819, 885] | 33024 [31755, 33804] | 33024 [31755, 33804] |
| remove_string_fast_1m | 1024 | 252.2 | 101.0 | 309 | 5 (5 / 0) | 18.4 | 0.00 | 885 [852, 1016] | 13550 [12083, 13951] | 13550 [12083, 13951] |
| small_build_fast | 1024 | 6.6 | 0.0 | 50 | 12 (12 / 0) | 7.0 | 0.00 | 557 [524, 590] | 955 [873, 966] | 955 [873, 966] |
| small_get_fast | 1024 | 0.3 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| churn_fast | 1024 | 7.4 | 2.6 | 9 | 1 (1 / 0) | 0.2 | 0.00 | 206 [184, 241] | 206 [184, 241] | 206 [184, 241] |
| insert_i64_secure_1k | 1024 | 4.5 | 0.1 | 67 | 16 (16 / 0) | 1.2 | 0.00 | 70 [66, 74] | 111 [99, 131] | 111 [99, 131] |
| hit_i64_secure_1k | 1024 | 0.6 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| miss_i64_secure_1k | 1024 | 0.6 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| iter_i64_secure_1k | 1024 | 0.6 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| remove_i64_secure_1k | 1024 | 0.6 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| insert_i64_secure_100k | 1024 | 7.9 | 3.1 | 47 | 10 (10 / 0) | 0.7 | 0.00 | 41 [37, 47] | 158 [151, 178] | 158 [151, 178] |
| hit_i64_secure_100k | 1024 | 5.7 | 3.1 | 8 | 1 (1 / 0) | 0.1 | 0.00 | 117 [114, 138] | 117 [114, 138] | 117 [114, 138] |
| miss_i64_secure_100k | 1024 | 5.7 | 3.1 | 8 | 1 (1 / 0) | 0.1 | 0.00 | 125 [115, 126] | 125 [115, 126] | 125 [115, 126] |
| iter_i64_secure_100k | 1024 | 5.7 | 3.1 | 8 | 1 (1 / 0) | 0.1 | 0.00 | 124 [120, 184] | 124 [120, 184] | 124 [120, 184] |
| remove_i64_secure_100k | 1024 | 5.7 | 3.1 | 8 | 1 (1 / 0) | 0.1 | 0.00 | 130 [111, 137] | 130 [111, 137] | 130 [111, 137] |
| insert_i64_secure_1m | 1024 | 59.5 | 25.0 | 100 | 5 (5 / 0) | 1.3 | 0.00 | 33 [29, 51] | 742 [361, 818] | 742 [361, 818] |
| hit_i64_secure_1m | 1024 | 76.0 | 24.5 | 100 | 4 (4 / 0) | 0.5 | 0.00 | 28 [25, 33] | 471 [424, 766] | 471 [424, 766] |
| miss_i64_secure_1m | 1024 | 76.0 | 24.5 | 100 | 4 (4 / 0) | 0.5 | 0.00 | 28 [26, 29] | 459 [432, 533] | 459 [432, 533] |
| iter_i64_secure_1m | 1024 | 76.0 | 50.0 | 100 | 5 (5 / 0) | 1.6 | 0.00 | 29 [26, 35] | 1090 [1007, 1924] | 1090 [1007, 1924] |
| remove_i64_secure_1m | 1024 | 76.0 | 24.5 | 100 | 4 (4 / 0) | 0.5 | 0.00 | 28 [26, 29] | 447 [415, 522] | 447 [415, 522] |
| insert_string_secure_1k | 1024 | 4.5 | 0.1 | 129 | 32 (32 / 0) | 2.1 | 0.01 | 61 [57, 70] | 124 [97, 163] | 124 [97, 163] |
| hit_string_secure_1k | 1024 | 0.7 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| miss_string_secure_1k | 1024 | 0.7 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| iter_string_secure_1k | 1024 | 0.8 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| remove_string_secure_1k | 1024 | 0.7 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| insert_string_secure_100k | 1024 | 24.8 | 9.5 | 103 | 9 (9 / 0) | 7.5 | 0.00 | 819 [786, 885] | 1310 [1256, 1410] | 1310 [1256, 1410] |
| hit_string_secure_100k | 1024 | 21.3 | 15.9 | 29 | 3 (3 / 0) | 3.1 | 0.00 | 688 [688, 852] | 1749 [1581, 2231] | 1749 [1581, 2231] |
| miss_string_secure_100k | 1024 | 21.3 | 15.9 | 29 | 3 (3 / 0) | 2.6 | 0.00 | 655 [655, 721] | 1546 [1477, 1623] | 1546 [1477, 1623] |
| iter_string_secure_100k | 1024 | 21.3 | 15.9 | 29 | 3 (3 / 0) | 2.7 | 0.00 | 655 [557, 721] | 1606 [1347, 1729] | 1606 [1347, 1729] |
| remove_string_secure_100k | 1024 | 21.3 | 15.9 | 29 | 3 (3 / 0) | 2.5 | 0.00 | 655 [655, 688] | 1509 [1450, 1592] | 1509 [1450, 1592] |
| insert_string_secure_1m | 1024 | 252.2 | 101.0 | 309 | 5 (5 / 0) | 17.7 | 0.00 | 819 [754, 1049] | 12909 [12222, 13653] | 12909 [12222, 13653] |
| hit_string_secure_1m | 1024 | 252.2 | 101.0 | 309 | 5 (5 / 0) | 17.9 | 0.00 | 885 [786, 983] | 13097 [11800, 14274] | 13097 [11800, 14274] |
| miss_string_secure_1m | 1024 | 252.2 | 101.0 | 309 | 5 (5 / 0) | 17.6 | 0.00 | 918 [754, 1311] | 12181 [11052, 14100] | 12181 [11052, 14100] |
| iter_string_secure_1m | 1024 | 252.2 | 178.6 | 309 | 6 (6 / 0) | 50.6 | 0.00 | 819 [754, 918] | 32201 [30927, 33170] | 32201 [30927, 33170] |
| remove_string_secure_1m | 1024 | 252.2 | 101.0 | 309 | 5 (5 / 0) | 17.6 | 0.00 | 885 [819, 1049] | 12503 [11994, 13877] | 12503 [11994, 13877] |
| small_build_secure | 1024 | 6.6 | 0.0 | 50 | 12 (12 / 0) | 7.2 | 0.00 | 557 [508, 590] | 1074 [934, 1196] | 1074 [934, 1196] |
| small_get_secure | 1024 | 0.3 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| churn_secure | 1024 | 7.4 | 2.6 | 9 | 1 (1 / 0) | 0.2 | 0.00 | 201 [192, 212] | 201 [192, 212] | 201 [192, 212] |

`perf stat` (median per process, whole process including start-up):

| kernel | variant | context-switches | cpu-migrations | page-faults | task-clock |
|---|---|---:|---:|---:|---:|
| insert_i64_fast_1k | codira | 13 | 0 | 2121 | 35 |
| insert_i64_fast_1k | rust | 0 | 0 | 101 | 21 |
| insert_i64_fast_1k | cpp | 0 | 0 | 155 | 63 |
| hit_i64_fast_1k | codira | 13 | 0 | 376 | 8 |
| hit_i64_fast_1k | rust | 0 | 0 | 101 | 4 |
| hit_i64_fast_1k | cpp | 1 | 0 | 152 | 7 |
| miss_i64_fast_1k | codira | 13 | 0 | 378 | 6 |
| miss_i64_fast_1k | rust | 0 | 0 | 101 | 4 |
| miss_i64_fast_1k | cpp | 0 | 0 | 153 | 9 |
| iter_i64_fast_1k | codira | 13 | 0 | 393 | 7 |
| iter_i64_fast_1k | rust | 0 | 0 | 101 | 3 |
| iter_i64_fast_1k | cpp | 0 | 0 | 153 | 4 |
| remove_i64_fast_1k | codira | 13 | 0 | 377 | 21 |
| remove_i64_fast_1k | rust | 0 | 0 | 101 | 13 |
| remove_i64_fast_1k | cpp | 0 | 0 | 153 | 41 |
| insert_i64_fast_100k | codira | 13 | 0 | 4404 | 38 |
| insert_i64_fast_100k | rust | 1 | 0 | 11448 | 37 |
| insert_i64_fast_100k | cpp | 1 | 0 | 4864 | 158 |
| hit_i64_fast_100k | codira | 13 | 0 | 1811 | 18 |
| hit_i64_fast_100k | rust | 0 | 0 | 1953 | 14 |
| hit_i64_fast_100k | cpp | 1 | 0 | 3044 | 40 |
| miss_i64_fast_100k | codira | 13 | 0 | 1811 | 26 |
| miss_i64_fast_100k | rust | 0 | 0 | 1953 | 16 |
| miss_i64_fast_100k | cpp | 1 | 0 | 3044 | 48 |
| iter_i64_fast_100k | codira | 13 | 0 | 1811 | 13 |
| iter_i64_fast_100k | rust | 0 | 0 | 1953 | 10 |
| iter_i64_fast_100k | cpp | 1 | 0 | 3044 | 45 |
| remove_i64_fast_100k | codira | 13 | 0 | 1811 | 52 |
| remove_i64_fast_100k | rust | 0 | 0 | 1953 | 37 |
| remove_i64_fast_100k | cpp | 1 | 0 | 3043 | 95 |
| insert_i64_fast_1m | codira | 13 | 0 | 21589 | 89 |
| insert_i64_fast_1m | rust | 1 | 0 | 25309 | 90 |
| insert_i64_fast_1m | cpp | 2 | 0 | 27923 | 347 |
| hit_i64_fast_1m | codira | 13 | 0 | 21684 | 119 |
| hit_i64_fast_1m | rust | 0 | 0 | 25310 | 113 |
| hit_i64_fast_1m | cpp | 1 | 0 | 27923 | 394 |
| miss_i64_fast_1m | codira | 14 | 0 | 21685 | 101 |
| miss_i64_fast_1m | rust | 1 | 0 | 25309 | 100 |
| miss_i64_fast_1m | cpp | 1 | 0 | 27922 | 416 |
| iter_i64_fast_1m | codira | 13 | 0 | 21685 | 98 |
| iter_i64_fast_1m | rust | 1 | 0 | 25309 | 99 |
| iter_i64_fast_1m | cpp | 2 | 0 | 27923 | 427 |
| remove_i64_fast_1m | codira | 13 | 0 | 21622 | 129 |
| remove_i64_fast_1m | rust | 0 | 0 | 25309 | 126 |
| remove_i64_fast_1m | cpp | 1 | 0 | 27922 | 395 |
| insert_string_fast_1k | codira | 13 | 0 | 1405 | 92 |
| insert_string_fast_1k | rust | 0 | 0 | 148 | 93 |
| insert_string_fast_1k | cpp | 1 | 0 | 202 | 122 |
| hit_string_fast_1k | codira | 13 | 0 | 405 | 25 |
| hit_string_fast_1k | rust | 1 | 0 | 141 | 23 |
| hit_string_fast_1k | cpp | 0 | 0 | 200 | 28 |
| miss_string_fast_1k | codira | 13 | 0 | 405 | 21 |
| miss_string_fast_1k | rust | 0 | 0 | 141 | 17 |
| miss_string_fast_1k | cpp | 1 | 0 | 200 | 26 |
| iter_string_fast_1k | codira | 13 | 0 | 422 | 7 |
| iter_string_fast_1k | rust | 0 | 0 | 141 | 3 |
| iter_string_fast_1k | cpp | 0 | 0 | 200 | 8 |
| remove_string_fast_1k | codira | 13 | 0 | 405 | 64 |
| remove_string_fast_1k | rust | 0 | 0 | 141 | 70 |
| remove_string_fast_1k | cpp | 1 | 0 | 199 | 124 |
| insert_string_fast_100k | codira | 13 | 0 | 10184 | 120 |
| insert_string_fast_100k | rust | 1 | 0 | 9846 | 156 |
| insert_string_fast_100k | cpp | 2 | 0 | 8109 | 347 |
| hit_string_fast_100k | codira | 13 | 0 | 5474 | 66 |
| hit_string_fast_100k | rust | 0 | 0 | 6070 | 74 |
| hit_string_fast_100k | cpp | 1 | 0 | 8110 | 132 |
| miss_string_fast_100k | codira | 13 | 0 | 5474 | 70 |
| miss_string_fast_100k | rust | 1 | 0 | 6070 | 74 |
| miss_string_fast_100k | cpp | 1 | 0 | 8110 | 141 |
| iter_string_fast_100k | codira | 13 | 0 | 5475 | 32 |
| iter_string_fast_100k | rust | 0 | 0 | 6070 | 41 |
| iter_string_fast_100k | cpp | 0 | 0 | 8110 | 98 |
| remove_string_fast_100k | codira | 13 | 0 | 5474 | 126 |
| remove_string_fast_100k | rust | 0 | 0 | 6070 | 159 |
| remove_string_fast_100k | cpp | 1 | 0 | 8109 | 245 |
| insert_string_fast_1m | codira | 14 | 0 | 64413 | 384 |
| insert_string_fast_1m | rust | 1 | 0 | 72913 | 550 |
| insert_string_fast_1m | cpp | 4 | 0 | 77535 | 1082 |
| hit_string_fast_1m | codira | 14 | 0 | 64413 | 483 |
| hit_string_fast_1m | rust | 2 | 0 | 72912 | 657 |
| hit_string_fast_1m | cpp | 3 | 0 | 77534 | 1271 |
| miss_string_fast_1m | codira | 14 | 0 | 64413 | 425 |
| miss_string_fast_1m | rust | 2 | 0 | 72912 | 592 |
| miss_string_fast_1m | cpp | 4 | 0 | 77535 | 1326 |
| iter_string_fast_1m | codira | 14 | 0 | 64538 | 428 |
| iter_string_fast_1m | rust | 2 | 0 | 72913 | 543 |
| iter_string_fast_1m | cpp | 3 | 0 | 77535 | 1271 |
| remove_string_fast_1m | codira | 14 | 0 | 64413 | 511 |
| remove_string_fast_1m | rust | 2 | 0 | 72912 | 673 |
| remove_string_fast_1m | cpp | 4 | 0 | 77533 | 1098 |
| small_build_fast | codira | 13 | 0 | 2568 | 50 |
| small_build_fast | rust | 1 | 0 | 77 | 27 |
| small_build_fast | cpp | 0 | 0 | 131 | 27 |
| small_get_fast | codira | 13 | 0 | 310 | 7 |
| small_get_fast | rust | 0 | 0 | 77 | 5 |
| small_get_fast | cpp | 0 | 0 | 131 | 6 |
| churn_fast | codira | 13 | 0 | 2669 | 107 |
| churn_fast | rust | 0 | 0 | 2387 | 78 |
| churn_fast | cpp | 1 | 0 | 1319 | 293 |
| insert_i64_secure_1k | codira | 13 | 0 | 2091 | 63 |
| insert_i64_secure_1k | rust | 1 | 0 | 101 | 44 |
| hit_i64_secure_1k | codira | 13 | 0 | 377 | 15 |
| hit_i64_secure_1k | rust | 0 | 0 | 101 | 11 |
| miss_i64_secure_1k | codira | 13 | 0 | 377 | 12 |
| miss_i64_secure_1k | rust | 0 | 0 | 101 | 10 |
| iter_i64_secure_1k | codira | 13 | 0 | 393 | 7 |
| iter_i64_secure_1k | rust | 0 | 0 | 101 | 4 |
| remove_i64_secure_1k | codira | 13 | 0 | 378 | 35 |
| remove_i64_secure_1k | rust | 0 | 0 | 101 | 27 |
| insert_i64_secure_100k | codira | 13 | 0 | 4404 | 55 |
| insert_i64_secure_100k | rust | 1 | 0 | 11448 | 52 |
| hit_i64_secure_100k | codira | 13 | 0 | 1810 | 31 |
| hit_i64_secure_100k | rust | 1 | 0 | 1953 | 27 |
| miss_i64_secure_100k | codira | 13 | 0 | 1810 | 32 |
| miss_i64_secure_100k | rust | 1 | 0 | 1953 | 26 |
| iter_i64_secure_100k | codira | 13 | 0 | 1811 | 15 |
| iter_i64_secure_100k | rust | 0 | 0 | 1953 | 12 |
| remove_i64_secure_100k | codira | 13 | 0 | 1811 | 72 |
| remove_i64_secure_100k | rust | 1 | 0 | 1953 | 58 |
| insert_i64_secure_1m | codira | 13 | 0 | 21684 | 117 |
| insert_i64_secure_1m | rust | 1 | 0 | 25309 | 113 |
| hit_i64_secure_1m | codira | 13 | 0 | 21683 | 169 |
| hit_i64_secure_1m | rust | 1 | 0 | 25309 | 197 |
| miss_i64_secure_1m | codira | 13 | 0 | 21685 | 133 |
| miss_i64_secure_1m | rust | 1 | 0 | 25309 | 154 |
| iter_i64_secure_1m | codira | 13 | 0 | 21684 | 120 |
| iter_i64_secure_1m | rust | 1 | 0 | 25309 | 141 |
| remove_i64_secure_1m | codira | 13 | 0 | 21685 | 182 |
| remove_i64_secure_1m | rust | 1 | 0 | 25309 | 194 |
| insert_string_secure_1k | codira | 13 | 0 | 1405 | 92 |
| insert_string_secure_1k | rust | 1 | 0 | 280 | 117 |
| hit_string_secure_1k | codira | 13 | 0 | 405 | 23 |
| hit_string_secure_1k | rust | 1 | 0 | 142 | 20 |
| miss_string_secure_1k | codira | 13 | 0 | 405 | 20 |
| miss_string_secure_1k | rust | 0 | 0 | 141 | 17 |
| iter_string_secure_1k | codira | 13 | 0 | 422 | 7 |
| iter_string_secure_1k | rust | 0 | 0 | 142 | 3 |
| remove_string_secure_1k | codira | 13 | 0 | 405 | 52 |
| remove_string_secure_1k | rust | 0 | 0 | 141 | 79 |
| insert_string_secure_100k | codira | 13 | 0 | 10195 | 121 |
| insert_string_secure_100k | rust | 1 | 0 | 9732 | 201 |
| hit_string_secure_100k | codira | 13 | 0 | 5474 | 74 |
| hit_string_secure_100k | rust | 1 | 0 | 6071 | 84 |
| miss_string_secure_100k | codira | 13 | 0 | 5475 | 64 |
| miss_string_secure_100k | rust | 1 | 0 | 6071 | 75 |
| iter_string_secure_100k | codira | 13 | 0 | 5474 | 34 |
| iter_string_secure_100k | rust | 0 | 0 | 6070 | 47 |
| remove_string_secure_100k | codira | 13 | 0 | 5475 | 113 |
| remove_string_secure_100k | rust | 1 | 0 | 6070 | 186 |
| insert_string_secure_1m | codira | 14 | 0 | 64359 | 455 |
| insert_string_secure_1m | rust | 2 | 0 | 72912 | 715 |
| hit_string_secure_1m | codira | 14 | 0 | 64414 | 593 |
| hit_string_secure_1m | rust | 2 | 0 | 72913 | 855 |
| miss_string_secure_1m | codira | 14 | 0 | 64413 | 495 |
| miss_string_secure_1m | rust | 2 | 0 | 72913 | 799 |
| iter_string_secure_1m | codira | 14 | 0 | 64538 | 506 |
| iter_string_secure_1m | rust | 2 | 0 | 72912 | 709 |
| remove_string_secure_1m | codira | 14 | 0 | 64360 | 579 |
| remove_string_secure_1m | rust | 2 | 0 | 72912 | 825 |
| small_build_secure | codira | 13 | 0 | 2568 | 56 |
| small_build_secure | rust | 1 | 0 | 77 | 31 |
| small_get_secure | codira | 13 | 0 | 310 | 7 |
| small_get_secure | rust | 1 | 0 | 77 | 15 |
| churn_secure | codira | 13 | 0 | 2668 | 175 |
| churn_secure | rust | 2 | 0 | 2387 | 181 |

- `insert_i64_secure_1k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `hit_i64_secure_1k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `miss_i64_secure_1k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `iter_i64_secure_1k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `remove_i64_secure_1k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `insert_i64_secure_100k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `hit_i64_secure_100k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `miss_i64_secure_100k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `iter_i64_secure_100k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `remove_i64_secure_100k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `insert_i64_secure_1m` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `hit_i64_secure_1m` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `miss_i64_secure_1m` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `iter_i64_secure_1m` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `remove_i64_secure_1m` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `insert_string_secure_1k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `hit_string_secure_1k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `miss_string_secure_1k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `iter_string_secure_1k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `remove_string_secure_1k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `insert_string_secure_100k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `hit_string_secure_100k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `miss_string_secure_100k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `iter_string_secure_100k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `remove_string_secure_100k` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `insert_string_secure_1m` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `hit_string_secure_1m` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `miss_string_secure_1m` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `iter_string_secure_1m` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `remove_string_secure_1m` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `small_build_secure` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `small_get_secure` cpp: the C++ standard library has no seeded, flooding-resistant hasher
- `churn_secure` cpp: the C++ standard library has no seeded, flooding-resistant hasher

## JSON: parsing, printing, pretty-printing

| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |
|---|---:|---:|---:|---:|---:|---:|---|
| parse_twitter | 947.59 [924.32, 1013.22] | 1158.04 [1143.73, 1173.36] | 240.04 [231.01, 246.47] | 0.82x [0.79, 0.87] | 3.95x [3.82, 4.26] | 612 / 369 / 81 | same |
| parse_canada | 520.23 [507.95, 566.80] | 457.91 [437.93, 477.62] | 199.17 [188.93, 205.32] | 1.14x [1.08, 1.26] | 2.61x [2.53, 2.89] | 397 / 252 / 45 | same |
| parse_deep | 975.70 [935.13, 1005.48] | 901.34 [888.77, 921.62] | 159.87 [156.89, 165.46] | 1.08x [1.03, 1.12] | 6.10x [5.71, 6.29] | 625 / 317 / 59 | same |
| print_twitter | 457.43 [432.91, 481.89] | 303.69 [299.14, 309.23] | 244.82 [235.67, 251.27] | 1.51x [1.43, 1.60] | 1.87x [1.77, 1.97] | 340 / 209 / 83 | same |
| print_canada | 384.72 [379.74, 387.78] | 170.66 [165.96, 173.39] | 239.02 [232.86, 245.23] | 2.25x [2.20, 2.32] | 1.61x [1.57, 1.65] | 228 / 129 / 52 | same (cpp: differs, expected) |
| print_deep | 342.16 [324.74, 361.28] | 166.05 [162.08, 172.18] | 145.06 [135.98, 147.90] | 2.06x [1.96, 2.18] | 2.36x [2.24, 2.56] | 235 / 167 / 48 | same |
| pretty_twitter | 604.44 [588.60, 627.67] | 401.24 [389.98, 411.20] | 420.95 [412.22, 425.00] | 1.51x [1.45, 1.59] | 1.44x [1.40, 1.52] | 362 / 219 / 118 | same |
| pretty_canada | 627.38 [599.60, 640.10] | 330.34 [320.20, 351.16] | 337.01 [331.90, 356.55] | 1.90x [1.74, 1.96] | 1.86x [1.74, 1.91] | 307 / 145 / 104 | same (cpp: differs, expected) |
| pretty_deep | 2234.03 [2192.40, 2360.46] | 1548.40 [1499.22, 1570.83] | 1044.39 [1021.92, 1084.53] | 1.44x [1.42, 1.53] | 2.14x [2.05, 2.26] | 902 / 303 / 328 | same |

Codira GC (`Runtime::stats()` at the end of the kernel's process; medians over the runs, pauses with a bootstrap 95% interval):

| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) | collections (paced / at cap) | stopped (ms) | to safepoint (ms) | pause p50 (µs) | pause p99 (µs) | pause max (µs) |
|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|
| parse_twitter | 1024 | 607.1 | 484.1 | 529 | 6 (6 / 0) | 297.5 | 0.00 | 11010 [10486, 11534] | 170816 [165503, 193881] | 170816 [165503, 193881] |
| parse_canada | 1024 | 392.1 | 180.4 | 325 | 5 (5 / 0) | 94.4 | 0.00 | 8126 [7602, 8389] | 60615 [59703, 63275] | 60615 [59703, 63275] |
| parse_deep | 1024 | 618.9 | 424.8 | 461 | 7 (7 / 0) | 326.8 | 0.00 | 16253 [15204, 16777] | 179242 [172165, 185220] | 179242 [172165, 185220] |
| print_twitter | 1024 | 342.2 | 146.8 | 468 | 6 (6 / 0) | 119.3 | 0.00 | 11534 [10486, 12059] | 38312 [35613, 39935] | 38312 [35613, 39935] |
| print_canada | 1024 | 247.4 | 116.7 | 368 | 6 (6 / 0) | 77.1 | 0.00 | 8389 [7602, 8913] | 23785 [22863, 25595] | 23785 [22863, 25595] |
| print_deep | 1024 | 245.6 | 126.6 | 287 | 6 (6 / 0) | 94.9 | 0.00 | 5243 [5243, 5505] | 37971 [36074, 39490] | 37971 [36074, 39490] |
| pretty_twitter | 1024 | 364.9 | 155.9 | 702 | 7 (7 / 0) | 155.3 | 0.00 | 35652 [35652, 37233] | 37831 [37233, 38595] | 37831 [37233, 38595] |
| pretty_canada | 1024 | 335.2 | 131.1 | 724 | 8 (8 / 0) | 131.1 | 0.00 | 24117 [22020, 24117] | 26821 [25446, 29932] | 26821 [25446, 29932] |
| pretty_deep | 1024 | 1021.7 | 604.1 | 3527 | 15 (10 / 5) | 492.5 | 0.01 | 41943 [39846, 41943] | 52288 [48603, 55859] | 52288 [48603, 55859] |

`perf stat` (median per process, whole process including start-up):

| kernel | variant | context-switches | cpu-migrations | page-faults | task-clock |
|---|---|---:|---:|---:|---:|
| parse_twitter | codira | 18 | 0 | 156682 | 1010 |
| parse_twitter | rust | 4 | 0 | 94734 | 1306 |
| parse_twitter | cpp | 1 | 0 | 32220 | 289 |
| parse_canada | codira | 17 | 0 | 100816 | 556 |
| parse_canada | rust | 1 | 0 | 74586 | 494 |
| parse_canada | cpp | 1 | 0 | 17631 | 229 |
| parse_deep | codira | 19 | 0 | 159368 | 1031 |
| parse_deep | rust | 3 | 0 | 80780 | 1013 |
| parse_deep | cpp | 1 | 0 | 18005 | 190 |
| print_twitter | codira | 18 | 0 | 96027 | 682 |
| print_twitter | rust | 2 | 0 | 73607 | 542 |
| print_twitter | cpp | 1 | 0 | 36739 | 351 |
| print_canada | codira | 17 | 0 | 66022 | 512 |
| print_canada | rust | 1 | 0 | 36320 | 293 |
| print_canada | cpp | 1 | 0 | 19355 | 312 |
| print_deep | codira | 18 | 0 | 66547 | 504 |
| print_deep | rust | 1 | 0 | 44612 | 364 |
| print_deep | cpp | 1 | 0 | 28657 | 197 |
| pretty_twitter | codira | 17 | 0 | 108467 | 834 |
| pretty_twitter | rust | 2 | 0 | 86650 | 648 |
| pretty_twitter | cpp | 2 | 0 | 91580 | 518 |
| pretty_canada | codira | 18 | 0 | 106085 | 754 |
| pretty_canada | rust | 1 | 0 | 67539 | 461 |
| pretty_canada | cpp | 1 | 0 | 41537 | 406 |
| pretty_deep | codira | 22 | 0 | 487443 | 2419 |
| pretty_deep | rust | 5 | 0 | 224513 | 1751 |
| pretty_deep | cpp | 3 | 0 | 378713 | 1099 |

- `print_canada` cpp check value differs: RapidJSON prints 320 of the 600,000 floats one or two digits longer than the shortest round-trip text (Grisu2), 357 bytes in all; the values read back the same
- `pretty_canada` cpp check value differs: RapidJSON prints 320 of the 600,000 floats one or two digits longer than the shortest round-trip text (Grisu2), 357 bytes in all; the values read back the same
- C++ compiler for this workload: `g++` (RapidJSON 1.1, as packaged, assigns to a const member in `GenericStringRef`, which clang 22 rejects)

## Object graphs: allocation-heavy trees, cyclic graphs

| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |
|---|---:|---:|---:|---:|---:|---:|---|
| binary_trees | 807.56 [794.95, 830.53] | 293.86 [281.97, 310.15] | 376.13 [369.10, 399.34] | 2.75x [2.60, 2.86] | 2.15x [2.00, 2.24] | 36 / 10 / 12 | same |
| cyclic_graph | 385.59 [361.21, 388.94] | 124.18 [114.33, 127.05] | 107.06 [101.63, 110.60] | 3.11x [2.88, 3.37] | 3.60x [3.37, 3.81] | 31 / 5 / 6 | same |
| linked_list | 1259.51 [1245.48, 1321.54] | 418.28 [397.37, 464.43] | 243.76 [234.38, 250.97] | 3.01x [2.69, 3.25] | 5.17x [4.99, 5.53] | 261 / 63 / 34 | same |

Codira GC (`Runtime::stats()` at the end of the kernel's process; medians over the runs, pauses with a bootstrap 95% interval):

| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) | collections (paced / at cap) | stopped (ms) | to safepoint (ms) | pause p50 (µs) | pause p99 (µs) | pause max (µs) |
|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|
| binary_trees | 256 | 32.0 | 8.0 | 449 | 100 (100 / 0) | 323.8 | 0.02 | 3015 [2884, 3015] | 7340 [6554, 7602] | 7886 [7667, 8306] |
| cyclic_graph | 256 | 18.1 | 3.8 | 92 | 17 (17 / 0) | 183.9 | 0.00 | 11010 [11010, 11534] | 14258 [12389, 14969] | 14258 [12389, 14969] |
| linked_list | 256 | 256.0 | 65.2 | 381 | 10 (7 / 3) | 533.8 | 0.00 | 62915 [58720, 65012] | 99453 [95233, 105442] | 99453 [95233, 105442] |

`perf stat` (median per process, whole process including start-up):

| kernel | variant | context-switches | cpu-migrations | page-faults | task-clock |
|---|---|---:|---:|---:|---:|
| binary_trees | codira | 6 | 1 | 17137 | 810 |
| binary_trees | rust | 1 | 0 | 2126 | 299 |
| binary_trees | cpp | 1 | 0 | 2177 | 382 |
| cyclic_graph | codira | 5 | 0 | 11357 | 388 |
| cyclic_graph | rust | 1 | 0 | 10562 | 125 |
| cyclic_graph | cpp | 1 | 0 | 767 | 108 |
| linked_list | codira | 7 | 0 | 65962 | 1262 |
| linked_list | rust | 1 | 0 | 15702 | 420 |
| linked_list | cpp | 1 | 0 | 7941 | 253 |

## Concurrent producers and consumers (bounded channels)

| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |
|---|---:|---:|---:|---:|---:|---:|---|
| spsc | 595.06 [560.41, 621.23] | 46.13 [44.60, 50.08] | 465.95 [456.69, 484.26] | 12.90x [11.66, 13.64] | 1.28x [1.19, 1.33] | 4 / 2 / 4 | same |
| mpsc_4 | 9412.30 [9119.61, 9536.21] | 356.62 [314.65, 387.95] | 21663.01 [21378.33, 21880.29] | 26.39x [24.26, 29.99] | 0.43x [0.42, 0.44] | 4 / 2 / 4 | same |
| fan_out_in | 1328.82 [1299.91, 1419.54] | 1161.42 [1102.51, 1179.93] | 1147.87 [1112.52, 1244.04] | 1.14x [1.10, 1.28] | 1.16x [1.05, 1.26] | 4 / 3 / 4 | same |

Codira GC (`Runtime::stats()` at the end of the kernel's process; medians over the runs, pauses with a bootstrap 95% interval):

| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) | collections (paced / at cap) | stopped (ms) | to safepoint (ms) | pause p50 (µs) | pause p99 (µs) | pause max (µs) |
|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|
| spsc | 1024 | 0.0 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| mpsc_4 | 1024 | 0.0 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |
| fan_out_in | 1024 | 0.0 | 0.0 | 0 | 0 (0 / 0) | 0.0 | 0.00 | 0 [0, 0] | 0 [0, 0] | 0 [0, 0] |

`perf stat` (median per process, whole process including start-up):

| kernel | variant | context-switches | cpu-migrations | page-faults | task-clock |
|---|---|---:|---:|---:|---:|
| spsc | codira | 6904 | 1 | 208 | 1093 |
| spsc | rust | 88 | 0 | 87 | 92 |
| spsc | cpp | 7882 | 2 | 138 | 818 |
| mpsc_4 | codira | 1220721 | 101791 | 219 | 13154 |
| mpsc_4 | rust | 13614 | 92 | 98 | 1149 |
| mpsc_4 | cpp | 2184560 | 38010 | 144 | 22851 |
| fan_out_in | codira | 162907 | 8189 | 239 | 1803 |
| fan_out_in | rust | 138154 | 7329 | 114 | 1560 |
| fan_out_in | cpp | 148061 | 10493 | 156 | 1581 |

- `spsc`: Rust's `sync_channel` (from crossbeam) spins before it sleeps; the C++ queue is the textbook mutex and condition variables, which sleep and wake a thread for most messages. Rust is the strong baseline here.
- `mpsc_4`: Not in the CI subset: the C++ queue takes about 20 s here.
- `fan_out_in`: Bound by the latency of waking a worker: each job meets an empty channel.

## A server-style workload with high churn, and long runs under constrained heaps

| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |
|---|---:|---:|---:|---:|---:|---:|---|
| requests | 312.76 [293.98, 337.27] | 134.88 [132.38, 143.12] | 168.97 [162.38, 178.26] | 2.32x [2.15, 2.51] | 1.85x [1.69, 2.02] | 55 / 14 / 11 | same |
| long_run_heap_1024 | 2359.82 [2248.22, 2581.84] | 740.54 [695.74, 863.96] | 943.61 [914.99, 998.54] | 3.19x [2.73, 3.50] | 2.50x [2.36, 2.74] | 198 / 29 / 27 | same |
| long_run_heap_256 | 2339.77 [2193.65, 2522.67] | 775.20 [732.73, 829.12] | 988.72 [916.72, 1033.98] | 3.02x [2.74, 3.29] | 2.37x [2.20, 2.60] | 198 / 29 / 27 | same |
| long_run_heap_128 | 2350.99 [2176.88, 2408.50] | 805.19 [727.00, 892.83] | 958.14 [877.29, 1025.94] | 2.92x [2.54, 3.23] | 2.45x [2.21, 2.70] | 193 / 29 / 27 | same |
| long_run_heap_96 | 2530.32 [2413.29, 2659.64] | 763.62 [749.22, 857.45] | 950.07 [913.89, 1035.47] | 3.31x [2.91, 3.51] | 2.66x [2.44, 2.82] | 150 / 29 / 27 | same |
| long_run_heap_80 | 2629.15 [2379.35, 2921.32] | 831.80 [743.29, 872.52] | 940.99 [908.13, 976.94] | 3.16x [2.78, 3.73] | 2.79x [2.50, 3.13] | 127 / 29 / 27 | same |
| long_run_heap_64 | 4548.98 [4416.55, 4755.89] | 822.54 [771.27, 915.72] | 956.97 [921.44, 1018.02] | 5.53x [4.97, 6.06] | 4.75x [4.45, 4.98] | 102 / 29 / 27 | same |
| long_run_heap_48 | does not complete | 793.66 [741.99, 908.31] | 990.96 [936.61, 1026.84] | – | – | – / 29 / 27 | same |

Latency per operation (median over the runs of each percentile, microseconds):

| kernel | variant | p50 | p95 | p99 | p99.9 | max |
|---|---|---:|---:|---:|---:|---:|
| requests | codira | 0.8 [0.8, 0.8] | 1.2 [1.2, 1.4] | 3.5 [3.4, 4.3] | 34.6 [33.8, 39.4] | 11339.8 [10522.1, 14322.1] |
| requests | rust | 0.4 [0.4, 0.5] | 0.7 [0.7, 0.8] | 1.0 [0.9, 1.2] | 5.4 [5.0, 5.9] | 2539.5 [2402.0, 2656.1] |
| requests | cpp | 0.6 [0.6, 0.6] | 0.9 [0.8, 0.9] | 1.7 [1.5, 1.8] | 9.0 [6.9, 12.5] | 475.2 [397.6, 917.6] |
| long_run_heap_1024 | codira | 1.2 [1.1, 1.3] | 1.9 [1.8, 2.1] | 6.6 [6.2, 7.2] | 34.6 [33.8, 37.6] | 80485.9 [72044.4, 91564.6] |
| long_run_heap_1024 | rust | 0.5 [0.5, 0.6] | 0.8 [0.7, 0.9] | 1.0 [0.9, 1.2] | 5.4 [5.0, 6.4] | 4884.4 [4757.8, 4983.1] |
| long_run_heap_1024 | cpp | 0.7 [0.6, 0.7] | 1.0 [0.9, 1.1] | 1.4 [1.3, 1.6] | 12.3 [7.0, 13.7] | 3211.8 [2921.3, 3309.1] |
| long_run_heap_256 | codira | 1.2 [1.1, 1.3] | 1.9 [1.8, 2.2] | 6.7 [5.9, 7.0] | 33.9 [33.4, 37.4] | 82270.8 [72481.9, 88268.0] |
| long_run_heap_256 | rust | 0.5 [0.5, 0.6] | 0.9 [0.8, 0.9] | 1.1 [1.0, 1.2] | 6.0 [5.6, 10.9] | 4823.2 [4563.6, 5198.5] |
| long_run_heap_256 | cpp | 0.7 [0.7, 0.7] | 1.0 [1.0, 1.2] | 1.4 [1.3, 1.6] | 13.2 [7.0, 13.9] | 3449.9 [2736.6, 4112.5] |
| long_run_heap_128 | codira | 1.2 [1.1, 1.2] | 1.9 [1.8, 2.1] | 6.4 [5.9, 6.7] | 33.7 [33.2, 35.3] | 72014.9 [69628.5, 82274.6] |
| long_run_heap_128 | rust | 0.6 [0.5, 0.6] | 0.9 [0.8, 1.0] | 1.2 [1.1, 1.3] | 9.1 [5.2, 12.3] | 4936.9 [4594.5, 5538.9] |
| long_run_heap_128 | cpp | 0.7 [0.6, 0.7] | 1.0 [0.8, 1.2] | 1.4 [1.2, 1.7] | 12.6 [7.7, 13.7] | 2971.7 [2764.6, 3606.5] |
| long_run_heap_96 | codira | 1.2 [1.1, 1.2] | 1.9 [1.8, 2.1] | 5.9 [5.8, 6.3] | 33.6 [32.6, 34.5] | 54732.5 [52084.3, 59283.9] |
| long_run_heap_96 | rust | 0.5 [0.5, 0.6] | 0.9 [0.8, 1.0] | 1.2 [1.1, 1.3] | 6.2 [5.6, 7.5] | 4827.8 [4494.9, 4873.2] |
| long_run_heap_96 | cpp | 0.7 [0.6, 0.7] | 1.1 [1.0, 1.2] | 1.4 [1.4, 1.6] | 13.3 [11.8, 14.3] | 3127.9 [2840.9, 3449.6] |
| long_run_heap_80 | codira | 1.1 [1.0, 1.2] | 1.8 [1.6, 2.0] | 4.8 [3.8, 5.8] | 33.6 [32.6, 34.2] | 44328.0 [41289.0, 53377.9] |
| long_run_heap_80 | rust | 0.6 [0.5, 0.6] | 1.0 [0.8, 1.0] | 1.3 [1.0, 1.3] | 7.3 [5.8, 12.2] | 4849.6 [4786.3, 5308.8] |
| long_run_heap_80 | cpp | 0.7 [0.7, 0.7] | 1.0 [0.9, 1.0] | 1.4 [1.2, 1.5] | 13.2 [10.3, 13.4] | 3227.3 [2741.4, 3845.1] |
| long_run_heap_64 | codira | 1.1 [1.1, 1.1] | 1.8 [1.7, 1.9] | 3.6 [3.4, 3.7] | 33.6 [33.2, 33.8] | 34090.1 [27957.8, 37004.9] |
| long_run_heap_64 | rust | 0.6 [0.5, 0.6] | 0.9 [0.8, 1.1] | 1.2 [1.1, 1.4] | 10.1 [6.0, 12.4] | 4879.1 [4769.1, 5850.1] |
| long_run_heap_64 | cpp | 0.7 [0.7, 0.7] | 1.0 [0.9, 1.1] | 1.4 [1.3, 1.5] | 13.1 [12.2, 13.9] | 3185.7 [3002.2, 3551.1] |
| long_run_heap_48 | rust | 0.6 [0.5, 0.6] | 0.9 [0.8, 1.0] | 1.2 [1.1, 1.3] | 8.4 [5.7, 13.3] | 4780.4 [4579.8, 4840.2] |
| long_run_heap_48 | cpp | 0.7 [0.7, 0.7] | 1.0 [0.9, 1.2] | 1.4 [1.3, 1.6] | 13.6 [13.2, 13.9] | 3348.8 [2933.1, 3710.1] |

Codira GC (`Runtime::stats()` at the end of the kernel's process; medians over the runs, pauses with a bootstrap 95% interval):

| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) | collections (paced / at cap) | stopped (ms) | to safepoint (ms) | pause p50 (µs) | pause p99 (µs) | pause max (µs) |
|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|
| requests | 1024 | 36.4 | 13.2 | 132 | 13 (13 / 0) | 95.8 | 0.00 | 9437 [8913, 9437] | 11334 [10514, 14317] | 11334 [10514, 14317] |
| long_run_heap_1024 | 1024 | 132.4 | 48.5 | 675 | 18 (18 / 0) | 865.2 | 0.01 | 56623 [54526, 60817] | 80478 [72037, 91556] | 80478 [72037, 91556] |
| long_run_heap_256 | 256 | 132.4 | 48.5 | 675 | 18 (18 / 0) | 853.7 | 0.01 | 56623 [54526, 60817] | 82261 [72474, 88261] | 82261 [72474, 88261] |
| long_run_heap_128 | 128 | 128.0 | 48.5 | 675 | 18 (14 / 4) | 850.9 | 0.01 | 54526 [52429, 62915] | 71999 [69615, 82259] | 71999 [69615, 82259] |
| long_run_heap_96 | 96 | 96.0 | 48.7 | 675 | 27 (7 / 20) | 1069.2 | 0.01 | 46137 [41943, 48234] | 54723 [52076, 59270] | 54723 [52076, 59270] |
| long_run_heap_80 | 80 | 80.0 | 48.7 | 675 | 42 (5 / 37) | 1238.5 | 0.01 | 30409 [26214, 37749] | 44316 [41248, 53364] | 44316 [41248, 53364] |
| long_run_heap_64 | 64 | 64.0 | 48.7 | 675 | 163 (4 / 159) | 3181.7 | 0.04 | 19923 [18874, 19923] | 32506 [27263, 36995] | 34085 [27949, 36995] |

`perf stat` (median per process, whole process including start-up):

| kernel | variant | context-switches | cpu-migrations | page-faults | task-clock |
|---|---|---:|---:|---:|---:|
| requests | codira | 13 | 0 | 16697 | 318 |
| requests | rust | 0 | 0 | 3827 | 139 |
| requests | cpp | 1 | 0 | 2095 | 181 |
| long_run_heap_1024 | codira | 18 | 0 | 65137 | 2377 |
| long_run_heap_1024 | rust | 2 | 0 | 9310 | 756 |
| long_run_heap_1024 | cpp | 3 | 0 | 6191 | 997 |
| long_run_heap_256 | codira | 19 | 0 | 65121 | 2357 |
| long_run_heap_256 | rust | 3 | 0 | 9310 | 793 |
| long_run_heap_256 | cpp | 4 | 0 | 6191 | 1045 |
| long_run_heap_128 | codira | 18 | 0 | 63701 | 2368 |
| long_run_heap_128 | rust | 3 | 0 | 9309 | 823 |
| long_run_heap_128 | cpp | 3 | 0 | 6191 | 1018 |
| long_run_heap_96 | codira | 18 | 0 | 43078 | 2548 |
| long_run_heap_96 | rust | 3 | 0 | 9310 | 780 |
| long_run_heap_96 | cpp | 3 | 0 | 6191 | 1003 |
| long_run_heap_80 | codira | 19 | 0 | 40339 | 2648 |
| long_run_heap_80 | rust | 2 | 0 | 9309 | 848 |
| long_run_heap_80 | cpp | 3 | 0 | 6190 | 1001 |
| long_run_heap_64 | codira | 23 | 0 | 51082 | 4566 |
| long_run_heap_64 | rust | 3 | 0 | 9309 | 839 |
| long_run_heap_64 | cpp | 3 | 0 | 6190 | 1012 |
| long_run_heap_48 | rust | 2 | 0 | 9309 | 812 |
| long_run_heap_48 | cpp | 4 | 0 | 6191 | 1057 |

- `long_run_heap_48` codira: does not complete (the runtime ends the program out of memory: about 51 MB are live after a full collection, and a collection cannot make room under the cap): exit status 102: fatal: out of memory: cannot allocate `[core::i64]`
  heap limit: 50331648 bytes (48.0 MiB)
  committed:  50331648 bytes; the allocation needed 48 more
  live:       40309256 bytes after a full collection
  the host set the limit with `RuntimeBuilder::heap_limit`

## Heap floor

The smallest GC heap cap under which `server.long_run` completes, found by
bisection to 4 MiB (one run per cap tried; measured right after the run
above, on the same build), and what the runtime reports at that cap.

| kernel | completes at (MiB) | fails at (MiB) | live after last GC (MiB) | peak heap (MiB) | collections | time (ms) |
|---|---:|---:|---:|---:|---:|---:|
| server.long_run | 64 | 60 | 48.7 | 64.0 | 163 | 4471 |

History on this machine (the same program): it completed at 80 MiB on
`ea5aea347e` (before the heap policy), failed at 96 and 80 MiB on
`ed9819edfc` (String Fabric keeps a buffer of at least 256 bytes behind a
kept format result), and completes at 64 MiB on `f8b145c6a1` (with the
Swiss-table `Map`).

## Compile time and binary size

From-scratch builds of each workload's program (Codira: `codira build -O 3`; the size is every `.codiralib` it writes, the package's and one per standard-library module it imports, without the runtime that loads them; Rust: one `rustc` invocation; C++: one compiler invocation). Seconds: median [bootstrap 95%].

| workload | variant | build (s) | artifact (KiB) | stripped (KiB) |
|---|---|---:|---:|---:|
| channels | codira | 0.080 [0.069, 0.084] | 8 | – |
| channels | cpp | 2.000 [1.836, 2.050] | 29 | 23 |
| channels | rust | 1.042 [1.038, 1.102] | 4356 | 413 |
| collections | codira | 0.218 [0.207, 0.257] | 25 | – |
| collections | cpp | 2.991 [2.879, 3.052] | 27 | 22 |
| collections | rust | 0.630 [0.599, 0.711] | 4291 | 367 |
| format | codira | 0.156 [0.147, 0.173] | 25 | – |
| format | cpp | 2.606 [2.529, 2.631] | 80 | 67 |
| format | rust | 0.349 [0.335, 0.369] | 4308 | 382 |
| graph | codira | 0.102 [0.093, 0.109] | 13 | – |
| graph | cpp | 1.723 [1.664, 1.784] | 22 | 18 |
| graph | rust | 0.441 [0.424, 0.521] | 4286 | 361 |
| hashmap | codira | 1.188 [1.156, 1.282] | 142 | – |
| hashmap | cpp | 2.232 [2.180, 2.299] | 42 | 35 |
| hashmap | rust | 1.286 [1.228, 1.363] | 4336 | 400 |
| json | codira | 1.075 [1.039, 1.118] | 204 | – |
| json | cpp | 2.689 [2.603, 2.702] | 90 | 82 |
| loops | codira | 0.152 [0.145, 0.174] | 12 | – |
| loops | cpp | 1.598 [1.593, 1.675] | 22 | 18 |
| loops | rust | 0.435 [0.412, 0.459] | 4282 | 359 |
| result | codira | 0.244 [0.232, 0.259] | 105 | – |
| result | cpp | 1.862 [1.745, 1.941] | 26 | 22 |
| result | rust | 0.433 [0.394, 0.439] | 4282 | 358 |
| server | codira | 0.455 [0.418, 0.479] | 116 | – |
| server | cpp | 2.822 [2.704, 3.026] | 87 | 71 |
| server | rust | 0.574 [0.544, 0.614] | 4299 | 368 |
| strings | codira | 0.160 [0.158, 0.177] | 30 | – |
| strings | cpp | 1.819 [1.741, 1.826] | 27 | 22 |
| strings | rust | 0.499 [0.460, 0.556] | 4291 | 367 |

