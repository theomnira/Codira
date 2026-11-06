#!/usr/bin/env python3
# Copyright (c) 2026 Omnira CJSC
# Author: Tunjay Akbarli
# Date: November 6, 2026
#
# Functionality:
# - Runs the JSON benchmarks end to end: writes the inputs
#   (`gen_inputs.py`), builds the Codira program with the `codira` given
#   (a release build, for numbers worth reporting), the host that runs it
#   and the Rust program, runs the two programs alternately `--rounds`
#   times from a directory holding `data/`, checks that they print the same
#   check values, and prints a table of the medians, in milliseconds.
#
#   python3 examples/json_bench/bench.py --codira <path to codira> \
#       --work <scratch directory> [--rounds 3] [--rust-features ...]
#
# Build products go under `--work`, never into the repository.

import argparse
import os
import statistics
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))


def run(cmd, **kw):
    print("+", " ".join(cmd), file=sys.stderr)
    return subprocess.run(cmd, check=True, **kw)


def lines(output):
    """{(input, phase): (nanoseconds, check)} from a program's output."""
    out = {}
    for line in output.decode().splitlines():
        name, phase, ns, check = line.split(" ")
        out[(name, phase)] = (int(ns), check)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--codira", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--rust-features", default="preserve_order,float_roundtrip")
    args = ap.parse_args()
    work = os.path.abspath(args.work)
    os.makedirs(work, exist_ok=True)

    run([sys.executable, os.path.join(HERE, "gen_inputs.py"), os.path.join(work, "data")])
    env = dict(os.environ, CODIRA_STD_PATH=os.path.join(ROOT, "std"))
    run(
        [args.codira, "build", "-O", "3", "--manifest-path", os.path.join(HERE, "codira.toml")],
        env=env,
    )
    run(
        [
            "cargo", "build", "--release", "--manifest-path",
            os.path.join(HERE, "host", "Cargo.toml"),
            "--target-dir", os.path.join(work, "host-target"),
        ]
    )
    run(
        [
            "cargo", "build", "--release", "--no-default-features",
            "--features", args.rust_features, "--manifest-path",
            os.path.join(HERE, "rust", "Cargo.toml"),
            "--target-dir", os.path.join(work, "rust-target"),
        ]
    )
    host = os.path.join(work, "host-target", "release", "json_bench_host")
    library = os.path.join(HERE, "target", "mod.codiralib")
    rust = os.path.join(work, "rust-target", "release", "json_bench_rust")

    codira_runs, rust_runs = [], []
    for _ in range(args.rounds):
        codira_runs.append(lines(run([host, library], cwd=work, capture_output=True).stdout))
        rust_runs.append(lines(run([rust], cwd=work, capture_output=True).stdout))

    keys = list(codira_runs[0])
    print("| input | phase | Codira (ms) | Rust (ms) | Codira / Rust | check |")
    print("|---|---|---:|---:|---:|---|")
    failed = False
    for key in keys:
        c = statistics.median(r[key][0] for r in codira_runs) / 1e6
        r = statistics.median(r[key][0] for r in rust_runs) / 1e6
        checks = {run[key][1] for run in codira_runs + rust_runs}
        same = "same" if len(checks) == 1 else "DIFFERENT: " + ", ".join(sorted(checks))
        failed |= len(checks) != 1
        print(f"| {key[0]} | {key[1]} | {c:.1f} | {r:.1f} | {c / r:.2f}x | {same} |")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
