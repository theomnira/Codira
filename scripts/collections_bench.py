#!/usr/bin/env python3
# Copyright (c) 2026 Omnira CJSC
# Author: Tunjay Akbarli
# Date: November 6, 2026
#
# Functionality:
# - Runs the Codira and Rust sides of examples/collections_bench alternately
#   and prints the median of each measurement, with the checksums both sides
#   printed (which must agree).
#
# Usage, from the repository root, with a release `codira` on PATH (or
# CODIRA=/path/to/codira):
#
#     python3 scripts/collections_bench.py [runs]        # default 5 runs
#
# It builds the Codira side with `codira build -O 2` and the Rust side with
# `rustc -C opt-level=3` into a temporary directory, then runs them
# alternately, `runs` times each. Lines are `name ms [ns/op] (check)`; the
# table printed has the median milliseconds and, for map lines, the median
# nanoseconds per operation, Codira then Rust, and a mark where the checksums
# differ.

import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(ROOT, "examples", "collections_bench")
LINE = re.compile(r"^(\S+) ([0-9.]+) ms(?: ([0-9.]+) ns/op)? \((-?\d+)\)$")

# The Rust side names two measurements differently.
ALIASES = {"sum_iter": "sum_builtin", "sort_unstable_1m": "sort_1m"}


def parse(output):
    out = {}
    for line in output.splitlines():
        m = LINE.match(line.strip())
        if m:
            name = ALIASES.get(m.group(1), m.group(1))
            per = float(m.group(3)) if m.group(3) else None
            out[name] = (float(m.group(2)), per, m.group(4))
    return out


def main():
    runs = int(sys.argv[1]) if len(sys.argv) > 1 else 5
    codira = os.environ.get("CODIRA", "codira")
    work = tempfile.mkdtemp(prefix="collections_bench")
    try:
        shutil.copytree(BENCH, os.path.join(work, "bench"))
        project = os.path.join(work, "bench")
        subprocess.run([codira, "build", "-O", "2"], cwd=project, check=True)
        rust = os.path.join(work, "collections_rust")
        subprocess.run(
            ["rustc", "-C", "opt-level=3", "-o", rust,
             os.path.join(project, "collections_rust.rs")],
            check=True,
        )
        results = {"codira": [], "rust": []}
        for _ in range(runs):
            out = subprocess.run(
                [codira, "start", "target/mod.codiralib", "main"],
                cwd=project, check=True, capture_output=True, text=True,
            ).stdout
            results["codira"].append(parse(out))
            out = subprocess.run([rust], check=True, capture_output=True, text=True).stdout
            results["rust"].append(parse(out))
    finally:
        shutil.rmtree(work, ignore_errors=True)

    names = list(results["codira"][0].keys())
    print(f"{'measurement':36} {'codira ms':>10} {'rust ms':>10} {'codira ns/op':>13} {'rust ns/op':>11}  check")
    for name in names:
        c = [r[name] for r in results["codira"] if name in r]
        r = [x[name] for x in results["rust"] if name in x]
        if not r:
            print(f"{name:36} {statistics.median(v[0] for v in c):10.2f} {'--':>10}")
            continue
        cms = statistics.median(v[0] for v in c)
        rms = statistics.median(v[0] for v in r)
        same = {v[2] for v in c} == {v[2] for v in r} and len({v[2] for v in c}) == 1
        if c[0][1] is not None:
            cper = statistics.median(v[1] for v in c)
            rper = statistics.median(v[1] for v in r)
            print(f"{name:36} {cms:10.2f} {rms:10.2f} {cper:13.2f} {rper:11.2f}  {'same' if same else 'DIFFERENT'}")
        else:
            print(f"{name:36} {cms:10.2f} {rms:10.2f} {'':13} {'':11}  {'same' if same else 'DIFFERENT'}")


if __name__ == "__main__":
    main()
