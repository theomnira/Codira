#!/usr/bin/env python3
# Copyright (c) 2026 Omnira CJSC
# Author: Tunjay Akbarli
# Date: November 6, 2026
#
# Functionality:
# - Runs the task benchmarks end to end: builds the Codira program with the
#   `codira` given (a release build, for numbers worth reporting), the host
#   that times it, the Rust program and, when `go` is on the PATH, the Go
#   program; runs the three alternately `--rounds` times with 1 and with 4
#   workers; checks that they print the same check values; and prints a
#   table of the medians, in milliseconds, and the 1 -> 4 worker speedups.
#
#   python3 examples/fabric_bench/bench.py --codira <path to codira> \
#       --work <scratch directory> [--rounds 5] [--workers 1,4]
#
# Build products go under `--work`, never into the repository.

import argparse
import os
import shutil
import statistics
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))


def run(cmd, **kw):
    print("+", " ".join(cmd), file=sys.stderr)
    return subprocess.run(cmd, check=True, **kw)


def lines(output):
    """{workload: (nanoseconds, check)} from a program's output."""
    out = {}
    for line in output.decode().splitlines():
        name, ns, check = line.split(" ")
        out[name] = (int(ns), check)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--codira", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--workers", default="1,4")
    args = ap.parse_args()
    work = os.path.abspath(args.work)
    os.makedirs(work, exist_ok=True)
    workers = [int(w) for w in args.workers.split(",")]

    env = dict(os.environ, CODIRA_STD_PATH=os.path.join(ROOT, "std"))
    run(
        [args.codira, "build", "-O", "3", "--manifest-path", os.path.join(HERE, "codira.toml")],
        env=env,
    )
    target = os.path.join(work, "target")
    for crate in ("host", "rust"):
        run(
            [
                "cargo", "build", "--release", "--manifest-path",
                os.path.join(HERE, crate, "Cargo.toml"), "--target-dir", target,
            ]
        )
    programs = {
        "Codira": [os.path.join(target, "release", "fabric_bench_host"),
                   os.path.join(HERE, "target", "mod.codiralib")],
        "Rust": [os.path.join(target, "release", "fabric_bench_rust")],
    }
    threads_var = {"Codira": "CODIRA_TASK_WORKERS", "Rust": "RAYON_NUM_THREADS",
                   "Go": "GOMAXPROCS"}
    if shutil.which("go"):
        go = os.path.join(work, "fabric_bench_go")
        run(["go", "build", "-o", go, "."], cwd=os.path.join(HERE, "go"))
        programs["Go"] = [go]

    # results[language][workers] = list of {workload: (ns, check)}
    results = {lang: {w: [] for w in workers} for lang in programs}
    for _ in range(args.rounds):
        for w in workers:
            for lang, cmd in programs.items():
                env = dict(os.environ, **{threads_var[lang]: str(w)})
                results[lang][w].append(lines(run(cmd, env=env, capture_output=True).stdout))

    langs = list(programs)
    first = results[langs[0]][workers[0]][0]
    print("| workload | workers | " + " | ".join(f"{l} (ms)" for l in langs) +
          " | " + " | ".join(f"Codira / {l}" for l in langs[1:]) + " | check |")
    print("|---|---:|" + "---:|" * (2 * len(langs) - 1) + "---|")
    failed = False
    medians = {}
    for name in first:
        for w in workers:
            row = []
            checks = set()
            for lang in langs:
                runs = results[lang][w]
                med = statistics.median(r[name][0] for r in runs) / 1e6
                medians[(lang, name, w)] = med
                row.append(med)
                checks |= {r[name][1] for r in runs}
            same = "same" if len(checks) == 1 else "DIFFERENT: " + ", ".join(sorted(checks))
            failed |= len(checks) != 1
            ratios = [f"{row[0] / r:.2f}x" for r in row[1:]]
            print(f"| {name} | {w} | " + " | ".join(f"{m:.1f}" for m in row) + " | " +
                  " | ".join(ratios) + f" | {same} |")
    if len(workers) >= 2:
        lo, hi = workers[0], workers[-1]
        print()
        print(f"| speedup {lo} -> {hi} workers | " + " | ".join(langs) + " |")
        print("|---|" + "---:|" * len(langs))
        for name in first:
            cells = [f"{medians[(l, name, lo)] / medians[(l, name, hi)]:.2f}x" for l in langs]
            print(f"| {name} | " + " | ".join(cells) + " |")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
