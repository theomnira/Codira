#!/usr/bin/env python3
"""Runs the AURORA correctness gates and prints a summary.

    python3 scripts/aurora_gates.py                 # the CI-sized run (minutes)
    python3 scripts/aurora_gates.py --profile long  # hours, for a local machine
    python3 scripts/aurora_gates.py --only fuzz-compiler,loom
    python3 scripts/aurora_gates.py --list

Every gate is a set of cargo test runs with environment knobs; TESTING.md
says what each one checks and what it cannot catch. Profiles scale the
time-bounded gates:

    quick   a smoke run of every gate, about 10 minutes plus build time
    ci      the nightly CI run, about 30 minutes plus build time
    long    hours: --hours sets the total for the time-bounded gates

A fresh seed is drawn per run (--seed to repeat one) and printed with every
gate, so a failure can be replayed. The sanitizer gates use
RUSTC_BOOTSTRAP=1 to pass -Zsanitizer to the stable compiler, and separate
target directories (target/asan, target/tsan, target/loom) so they do not
invalidate the normal build. The exit status is non-zero if any gate fails.
"""

import argparse
import os
import random
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HOST = "x86_64-unknown-linux-gnu"

# Seconds per test for the time-bounded gates (gc_stress has four such
# tests, service_soak two, the fuzzers one each), programs for the
# differential gate and the scale of the JSON fuzzing. For `long` the
# first four are shares of the --hours budget.
PROFILES = {
    "quick": {"stress": 20, "soak": 30, "fuzz": 30, "strings": 20, "diff": 150, "json": 2},
    "ci": {"stress": 120, "soak": 300, "fuzz": 300, "strings": 120, "diff": 1000, "json": 10},
    "long": {"stress": 0.30, "soak": 0.30, "fuzz": 0.25, "strings": 0.15, "diff": 20000, "json": 50},
}


def base_env():
    env = dict(os.environ)
    env.setdefault("LLVM_SYS_221_PREFIX", "/usr/lib/llvm-22")
    env.setdefault("CARGO_INCREMENTAL", "0")
    env.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    env.setdefault("CARGO_PROFILE_TEST_DEBUG", "0")
    env.setdefault("RUST_BACKTRACE", "1")
    return env


def cargo_test(args, env=None, test_args=()):
    # Every test binary runs even when an earlier one fails.
    cmd = ["cargo", "test", "--no-fail-fast", *args]
    if test_args:
        cmd += ["--", *test_args]
    return (cmd, env or {})


def sanitizer_env(kind):
    supp = ROOT / "scripts" / "sanitizers" / "tsan.supp"
    if kind == "address":
        return {
            "RUSTC_BOOTSTRAP": "1",
            "RUSTFLAGS": "--cfg codira_asan -Zsanitizer=address",
            "CARGO_TARGET_DIR": str(ROOT / "target" / "asan"),
            # Conservative scanning cannot see frames moved to the fake
            # stacks this option uses.
            "ASAN_OPTIONS": "detect_stack_use_after_return=0",
        }
    return {
        "RUSTC_BOOTSTRAP": "1",
        "RUSTFLAGS": "-Zsanitizer=thread",
        "CARGO_TARGET_DIR": str(ROOT / "target" / "tsan"),
        "TSAN_OPTIONS": f"halt_on_error=1 suppressions={supp}",
    }


def gates(budget, seed):
    """Returns (name, description, [commands]) for every gate."""
    stress = {"CODIRA_STRESS_SECS": str(budget["stress"]), "CODIRA_STRESS_SEED": str(seed % 1_000_000 + 1)}
    fuzz = {"CODIRA_FUZZ_SECS": str(budget["fuzz"]), "CODIRA_FUZZ_SEED": str(seed)}
    strings = {"CODIRA_FUZZ_SECS": str(budget["strings"]), "CODIRA_FUZZ_SEED": str(seed)}
    soak = {"CODIRA_SOAK_SECS": str(budget["soak"])}
    asan = sanitizer_env("address")
    tsan = sanitizer_env("thread")
    gc_tests = ["--test", "gc_stress", "--test", "gc_threads", "--test", "strings_host_gc", "--test", "growable_arrays"]
    mode_tests = ["--test", "gc_stress", "--test", "gc_threads", "--test", "fault_injection",
                  "--test", "service_soak", "--test", "string_fabric_threads"]
    return [
        ("gc-stress",
         "seeded random graphs vs a shadow model; collections from Codira code, host threads and the heap limit",
         [cargo_test(["-p", "codira_runtime", *gc_tests, "--test", "heap_limit", "--test", "memory"], stress)]),
        ("gc-modes",
         "the GC gates again with relocation on, with conservative roots only, and in GC stress mode",
         [cargo_test(["-p", "codira_runtime", *mode_tests], {**stress, "CODIRA_GC_COMPACT": "1"}),
          cargo_test(["-p", "codira_runtime", *mode_tests], {**stress, "CODIRA_GC_ROOTS": "conservative"}),
          # Stress mode collects first, so tests that wait for the heap
          # limit or pacing to collect cannot hold; the rest must.
          cargo_test(["-p", "codira_runtime", *mode_tests], {**stress, "CODIRA_GC_STRESS": "200"},
                     ["--skip", "codira_graphs_survive_collections_at_a_heap_limit",
                      "--skip", "pacing_alone_keeps_codira_graphs_bounded",
                      "--skip", "host_threads_allocate_while_codira_code_collects",
                      "--skip", "a_service_with_a_heap_limit",
                      "--skip", "a_paced_service"])]),
        ("faults",
         "the Nth allocation fails, for every N: host API consistency and the out-of-memory exit of generated code",
         [cargo_test(["-p", "codira_memory", "--test", "tests"], {}, ["gc::faults"]),
          cargo_test(["-p", "codira_runtime", "--test", "fault_injection"], {"CODIRA_FAULT_BOUND": "5000"})]),
        ("soak",
         "a request loop with allocation churn: state intact, memory bounded, throughput stable",
         [cargo_test(["-p", "codira_runtime", "--test", "service_soak"], soak, ["--test-threads=1"])]),
        ("loom",
         "a loom model of the safepoint handshake, and its drift check against safepoint.rs",
         [cargo_test(["-p", "codira_memory", "--test", "safepoint_loom", "--release"],
                     {"RUSTFLAGS": "--cfg loom", "CARGO_TARGET_DIR": str(ROOT / "target" / "loom")})]),
        ("asan",
         "AddressSanitizer (and LeakSanitizer on codira_memory) over the GC crate and the runtime's GC tests",
         [cargo_test(["-p", "codira_memory", "--target", HOST, "--tests"], asan),
          cargo_test(["-p", "codira_runtime", "--target", HOST, *gc_tests, "--test", "fault_injection",
                      "--test", "service_soak"],
                     {**asan, "ASAN_OPTIONS": asan["ASAN_OPTIONS"] + ":detect_leaks=0",
                      "CODIRA_STRESS_SECS": "5", "CODIRA_SOAK_SECS": "8"})]),
        ("tsan",
         "ThreadSanitizer (std rebuilt instrumented) over the GC crate and the runtime's threaded GC tests",
         [cargo_test(["-Zbuild-std", "-p", "codira_memory", "--target", HOST, "--test", "tests"], tsan),
          # Under ThreadSanitizer the heap-limit stress runs too slowly to
          # reach its limit; it runs in the gc-stress and asan gates.
          cargo_test(["-Zbuild-std", "-p", "codira_runtime", "--target", HOST, "--test", "gc_threads",
                      "--test", "gc_stress", "--test", "strings_host_gc"],
                     {**tsan, "CODIRA_STRESS_SECS": "5"},
                     ["--skip", "codira_graphs_survive_collections_at_a_heap_limit"])]),
        ("fuzz-compiler",
         "mutated programs through parser, type checker and code generator: diagnostics, never a panic",
         [cargo_test(["-p", "codira_compiler", "--test", "fuzz_compiler"], fuzz),
          cargo_test(["-p", "codira_hir", "--lib"], {}, ["fuzz_regressions"])]),
        ("fuzz-strings",
         "every runtime string operation on random strings, Rust's str as the oracle",
         [cargo_test(["-p", "codira_runtime", "--test", "string_fuzz"], strings)]),
        ("fuzz-json",
         "std.json.json on mutated and random texts, serde_json as the oracle",
         [cargo_test(["-p", "codira", "--test", "json"],
                     {"CODIRA_JSON_FUZZ_SEED": str(seed), "CODIRA_JSON_FUZZ_SCALE": str(budget["json"])},
                     ["mutated_texts", "random_bytes"])]),
        ("differential",
         "LLVM (-O 2, -O 0) and Cranelift agree on seeded random programs",
         [cargo_test(["-p", "codira", "--test", "differential"],
                     {"CODIRA_DIFF_PROGRAMS": str(budget["diff"]), "CODIRA_DIFF_SEED": str(seed)},
                     ["--ignored", "many_programs"])]),
    ]


def scale(profile, hours):
    budget = dict(PROFILES[profile])
    if profile == "long":
        total = hours * 3600
        tests = {"stress": 4, "soak": 2, "fuzz": 1, "strings": 1}
        for key, count in tests.items():
            budget[key] = int(total * budget[key] / count)
    return budget


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--profile", choices=sorted(PROFILES), default="ci")
    parser.add_argument("--hours", type=float, default=4.0, help="total time for --profile long")
    parser.add_argument("--only", help="comma-separated gate names")
    parser.add_argument("--skip", help="comma-separated gate names")
    parser.add_argument("--seed", type=int, help="the seed (default: random)")
    parser.add_argument("--list", action="store_true", help="list the gates and exit")
    parser.add_argument("--dry-run", action="store_true", help="print the commands only")
    args = parser.parse_args()

    seed = args.seed if args.seed is not None else random.randrange(1, 2**31)
    all_gates = gates(scale(args.profile, args.hours), seed)
    if args.list:
        for name, description, _ in all_gates:
            print(f"{name:14} {description}")
        return 0

    names = [name for name, _, _ in all_gates]
    only = set(args.only.split(",")) if args.only else set(names)
    skip = set(args.skip.split(",")) if args.skip else set()
    unknown = (only | skip) - set(names)
    if unknown:
        parser.error(f"unknown gate(s): {', '.join(sorted(unknown))}")
    if shutil.which("cargo") is None:
        parser.error("cargo is not on PATH")

    print(f"AURORA gates, profile {args.profile}, seed {seed}", flush=True)
    results = []
    for name, description, commands in all_gates:
        if name not in only or name in skip:
            continue
        print(f"\n=== {name}: {description}", flush=True)
        start = time.monotonic()
        status = "pass"
        for cmd, extra in commands:
            env = base_env()
            env.update(extra)
            shown = " ".join(f"{k}={v}" for k, v in extra.items())
            print(f"$ {shown} {' '.join(cmd)}", flush=True)
            if args.dry_run:
                continue
            if subprocess.run(cmd, cwd=ROOT, env=env).returncode != 0:
                status = "FAIL"
                break
        results.append((name, "dry" if args.dry_run else status, time.monotonic() - start))

    print(f"\nSummary (seed {seed}, profile {args.profile}):")
    for name, status, seconds in results:
        print(f"  {name:14} {status:5} {seconds / 60:7.1f} min")
    failed = [name for name, status, _ in results if status == "FAIL"]
    if failed:
        print(f"FAILED: {', '.join(failed)}; replay with --seed {seed} --only {','.join(failed)}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
