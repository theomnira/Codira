# Benchmark results

Baseline numbers for the Criterion benchmarks in `benches/benches/benchmarks.rs`,
recorded 2026-11-04. Rerun with `scripts/bench.sh`, which prints the same
machine description first.

## Why `benches/` is a workspace of its own

`benches/Cargo.toml` declares an empty `[workspace]`, so the root workspace
(`members = ["crates/*"]`) does not contain it and nothing in the gates
(`scripts/pre-commit.sh` runs `cargo check --workspace`) builds it. The
likely reason is its dependencies: LuaJIT built from source (`mlua` with
`vendored`) and the Wasmer runtime with Cranelift, which would add minutes
and a C toolchain requirement to every workspace build. The cost of keeping
it out was that it rotted unnoticed. What was broken, and fixed here:

- The three `.code` resources were written in an older syntax (`pub fn`,
  `struct(gc)`, `struct(value)`, untyped `let` reassigned); the compiler
  rejected all of them with syntax errors. They are now `public func`, `class` (garbage
  collected, reference) and `struct` (value).
- `wasmer` 4.x (4.4.0 resolved) does not link with the current toolchain:
  `wasmer_vm::libcalls` references `__rust_probestack`, which Rust no longer
  provides (`rust-lld: error: undefined symbol: __rust_probestack`). Moved to
  `wasmer` 6.
- Unequal work: the LuaJIT Fibonacci started at `a, b = 0, 0`, so it added
  zeros; it now starts at `0, 1` like the others, and uses locals. Its result
  is read as `f64` (Lua numbers are doubles; the sum leaves the `i64` range).
  The Wasm `empty` benchmark passed an `i64` to a function that takes an
  `i32` (wasm32 `isize`) and discarded the resulting error, so it measured a
  failed call; it now passes an `i32` and unwraps.
- `set_struct_field`'s "codira struct(gc)" case used a parent made by
  `make_value_parent`; it now uses `make_gc_parent`.

The benchmarks are still not in the gates: building them takes about 5
minutes from scratch on the machine below (2.5 minutes after a `wasmer`
change) and about 2.2 GB of target directory. Running them is
`scripts/bench.sh`; checking that a recorded number has not regressed is not
automated (see "Not done" below).

## Machine

| | |
|---|---|
| CPUs | 4 (`nproc`), Intel(R) Xeon(R) Processor @ 2.10GHz, virtual machine |
| memory | 15.7 GiB |
| OS | Ubuntu 24.04.5 LTS, kernel 6.18.44-fc-v233 |
| rustc | 1.97.1 (8bab26f4f 2026-07-14) |
| LLVM | 22.1.8 |
| Criterion | 0.5.1, `--warm-up-time 1 --measurement-time 3`, 100 samples |
| others | mlua 0.9.9 (vendored LuaJIT), wasmer 6 (Cranelift compiler) |

The Codira programs are compiled with `OptimizationLevel::Aggressive`
(`benches/benches/util/mod.rs`). The machine was otherwise idle during the
run (`ps` showed no other busy process when it started), but it is a shared
virtual machine: treat differences under about 10% as noise.

## Results

Criterion's estimate with its 95% interval (lower, estimate, upper), one run.

### `fibonacci/<language>/<n>`: call `main(n)`, n iterations of an add

| n | codira | rust | luajit | wasm |
|---:|---:|---:|---:|---:|
| 100 | 74.0 ns (72.8–75.6) | 42.0 ns (41.0–43.1) | 249.5 ns (244.1–256.0) | 207.8 ns (200.5–216.2) |
| 200 | 109.5 ns (104.8–115.0) | 68.8 ns (67.9–69.8) | 310.0 ns (307.2–313.3) | 241.1 ns (234.3–249.3) |
| 500 | 195.6 ns (187.7–206.2) | 152.8 ns (150.5–155.3) | 510.3 ns (500.5–521.4) | 380.8 ns (365.0–398.3) |
| 1000 | 403.3 ns (381.4–427.0) | 313.7 ns (310.2–318.2) | 839.3 ns (827.9–851.6) | 513.3 ns (506.4–520.4) |
| 4000 | 1.307 µs (1.282–1.333) | 1.316 µs (1.282–1.356) | 2.837 µs (2.787–2.893) | 1.703 µs (1.670–1.742) |
| 8000 | 2.731 µs (2.675–2.795) | 2.565 µs (2.524–2.612) | 5.375 µs (5.285–5.478) | 3.333 µs (3.209–3.483) |

Not the same arithmetic: Codira and Rust add wrapping `i64`s, Wasm wrapping
`i32`s (the module is wasm32), LuaJIT doubles. The Rust baseline is a
direct call; the others go through their host APIs (`Runtime::invoke`, a
Lua global lookup and call, `Function::call`), whose fixed cost is the
`empty` row below.

### `empty/<language>`: call a function that returns its argument

| codira | rust | luajit | wasm |
|---:|---:|---:|---:|
| 47.4 ns (46.9–48.0) | 0.357 ns (0.344–0.371) | 145.9 ns (143.9–148.3) | 151.1 ns (147.8–155.3) |

The Rust number is a `black_box` round trip, not a call.

### `get_struct_field/<case>/<n>`: n reads of a field from Rust

| case | 100 | 200 | 500 | 1000 |
|---|---:|---:|---:|---:|
| codira fundamental (`f32` field of a class) | 9.09 µs | 17.93 µs | 44.64 µs | 96.97 µs |
| codira struct(gc) (class-typed field) | 9.14 µs | 18.58 µs | 47.56 µs | 93.93 µs |
| codira struct(value) (struct-typed field) | 17.68 µs | 54.42 µs | 70.88 µs | 140.62 µs |
| rust fundamental | 17.9 ns | 36.4 ns | 94.2 ns | 202.9 ns |
| rust struct (clone) | 101.7 ns | 202.0 ns | 494.5 ns | 976.1 ns |
| rust &struct | 18.0 ns | 37.6 ns | 92.9 ns | 184.4 ns |

### `set_struct_field/<case>/<n>`: n writes of a field from Rust

| case | 100 | 200 | 500 | 1000 |
|---|---:|---:|---:|---:|
| codira fundamental | 11.69 µs | 23.07 µs | 58.83 µs | 117.65 µs |
| codira struct(gc) | 12.90 µs | 25.46 µs | 66.36 µs | 126.07 µs |
| codira struct(value) | 13.05 µs | 26.21 µs | 66.64 µs | 131.67 µs |
| rust fundamental | 18.6 ns | 36.0 ns | 87.0 ns | 169.9 ns |
| rust struct | 63.1 ns | 126.3 ns | 321.1 ns | 664.2 ns |
| rust &struct | 64.1 ns | 128.1 ns | 317.1 ns | 650.2 ns |

(Estimates only; the intervals are in the Criterion output and were within
about ±6% except `get_struct_field/codira struct(value)` at 100 and 200,
±16% and ±9%.)

The struct benchmarks measure the reflection-based host API
(`StructRef::get`/`set` look a field up by name and check its type on every
access, about 90 ns for a read and 120 ns for a write here), not field
access inside Codira code, which compiles to a load or store.

## Not done

- No check fails when a number regresses.
  Criterion can compare against a saved baseline
  (`scripts/bench.sh -- --save-baseline main`, then `-- --baseline main`)
  and reports changes, but it does not fail, and on a shared virtual
  machine the noise would make a fixed threshold flaky. A useful gate needs
  either dedicated hardware or an instruction-count measure (for example
  Cachegrind or `perf stat -e instructions`), neither of which is set up.
- The benchmarks cover call overhead, one integer loop and the host
  marshalling API. There is no benchmark of compile time, of the Cranelift
  backend, of the garbage collector or of a realistic program.
