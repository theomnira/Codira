#!/usr/bin/env python3
# Copyright (c) 2026 Omnira CJSC
# Author: Tunjay Akbarli
# Date: November 6, 2026
#
# Functionality:
# - The runner of the AURORA benchmark suite (`benches/aurora/`): builds
#   every variant of every workload (Codira, Rust, C++) with recorded
#   commands and flags, runs each kernel in a process of its own, interleaving
#   kernels and variants in a shuffled order every round, and checks that the
#   variants of a kernel print the same check value.
# - Reports, per kernel and variant, the median time with a bootstrap 95%
#   confidence interval, the Codira / Rust and Codira / C++ ratios with
#   bootstrap intervals, the peak resident set, the GC heap figures the Codira
#   host prints, and `perf stat` counters where `perf` works (it says which
#   events it could not count, and why).
# - Measures compile time and binary size of every workload.
# - Records the machine (CPU, cores, memory, OS, kernel, virtualisation, load)
#   and the toolchains (versions, flags, commit).
# - Writes `results.json` (everything, every sample) and `summary.md`.
# - With `--baseline <results.json>`, compares with a stored run and fails
#   (exit status 3) on a statistically significant regression: a one-sided
#   Mann-Whitney U test per kernel, Holm-corrected over the kernels, and a
#   bootstrap interval for the change that excludes zero.
#
#   python3 scripts/bench.py                       # the full suite
#   python3 scripts/bench.py --suite ci            # the CI subset
#   python3 scripts/bench.py --only strings.       # kernels matching a text
#   python3 scripts/bench.py --suite ci --baseline benches/aurora/baseline/ci.json
#
# See benches/aurora/README.md for the method and how to update a baseline.
# Standard library only: it runs on a bare CI image with Python 3.11+.

import argparse
import datetime
import hashlib
import json
import math
import os
import platform
import random
import re
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import tomllib

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
SUITE_DIR = os.path.join(ROOT, "benches", "aurora")
VARIANTS = ("codira", "rust", "cpp")
PERF_EVENTS = ("cycles", "instructions", "cache-misses", "branch-misses",
               "context-switches", "cpu-migrations", "page-faults", "task-clock")
RUST_FLAGS = ["--edition", "2021", "-C", "opt-level=3", "-C", "codegen-units=1"]
CXX_FLAGS = ["-O3", "-std=c++23", "-pthread", "-DNDEBUG"]
CODIRA_FLAGS = ["build", "-O", "3"]
BOOTSTRAP = 2000


def log(*args):
    print(*args, file=sys.stderr, flush=True)


def sh(cmd, **kw):
    """Runs a command, returns its stdout as text (stripped), or None."""
    try:
        return subprocess.run(cmd, capture_output=True, text=True, check=True,
                              **kw).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None


# ---------------------------------------------------------------- statistics

def median(xs):
    return statistics.median(xs) if xs else None


def bootstrap_median_ci(xs, rng, level=0.95):
    """Percentile bootstrap interval for the median."""
    if len(xs) < 2:
        return (xs[0], xs[0]) if xs else (None, None)
    meds = sorted(statistics.median(rng.choices(xs, k=len(xs))) for _ in range(BOOTSTRAP))
    lo = meds[int((1 - level) / 2 * BOOTSTRAP)]
    hi = meds[min(BOOTSTRAP - 1, int((1 + level) / 2 * BOOTSTRAP))]
    return lo, hi


def bootstrap_ratio_ci(num, den, rng, level=0.95):
    """Interval for median(num) / median(den), resampling both independently."""
    if len(num) < 2 or len(den) < 2:
        return None, None
    rs = []
    for _ in range(BOOTSTRAP):
        d = statistics.median(rng.choices(den, k=len(den)))
        if d:
            rs.append(statistics.median(rng.choices(num, k=len(num))) / d)
    rs.sort()
    return rs[int((1 - level) / 2 * len(rs))], rs[min(len(rs) - 1, int((1 + level) / 2 * len(rs)))]


def mann_whitney_greater(xs, ys):
    """One-sided p-value of Mann-Whitney U for "xs tends to be greater than ys",
    normal approximation with tie and continuity correction."""
    n1, n2 = len(xs), len(ys)
    if n1 == 0 or n2 == 0:
        return 1.0
    allv = sorted([(v, 0) for v in xs] + [(v, 1) for v in ys])
    ranks = [0.0] * len(allv)
    ties = 0.0
    i = 0
    while i < len(allv):
        j = i
        while j + 1 < len(allv) and allv[j + 1][0] == allv[i][0]:
            j += 1
        r = (i + j) / 2 + 1
        for k in range(i, j + 1):
            ranks[k] = r
        t = j - i + 1
        ties += t ** 3 - t
        i = j + 1
    r1 = sum(r for r, (_, g) in zip(ranks, allv) if g == 0)
    u1 = r1 - n1 * (n1 + 1) / 2
    n = n1 + n2
    mu = n1 * n2 / 2
    var = n1 * n2 / 12 * ((n + 1) - ties / (n * (n - 1)))
    if var <= 0:
        return 1.0
    z = (u1 - mu - 0.5) / math.sqrt(var)
    return 0.5 * math.erfc(z / math.sqrt(2))


# --------------------------------------------------------------- environment

def find_perf():
    """A `perf` that runs on this kernel, or (None, reason)."""
    cands = [os.environ.get("AURORA_PERF"), shutil.which("perf")]
    cands += sorted(_glob("/usr/lib/linux-tools/*/perf"), reverse=True)
    for c in cands:
        if c and sh([c, "--version"]):
            return c, None
    return None, "no `perf` binary that runs on this kernel"


def _glob(pattern):
    import glob
    return glob.glob(pattern)


def probe_perf(perf):
    """{event: "ok" | reason} for the events counted."""
    if not perf:
        return {}
    with tempfile.NamedTemporaryFile("r", suffix=".perf") as out:
        r = subprocess.run([perf, "stat", "-x,", "-o", out.name, "-e", ",".join(PERF_EVENTS),
                            "--", "true"], capture_output=True, text=True)
        if r.returncode != 0:
            return {e: "perf stat failed: " + r.stderr.strip()[:200] for e in PERF_EVENTS}
        got = parse_perf(open(out.name).read())
    status = {}
    for e in PERF_EVENTS:
        if e in got and got[e] is not None:
            status[e] = "ok"
        else:
            status[e] = "not supported (no hardware performance counters visible to this VM)" \
                if e in ("cycles", "instructions", "cache-misses", "branch-misses") \
                else "not supported"
    return status


def parse_perf(text):
    out = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        f = line.split(",")
        if len(f) < 3:
            continue
        name = f[2].split(":")[0]
        try:
            out[name] = float(f[0])
        except ValueError:
            out[name] = None
    return out


def environment(args, perf, perf_status):
    def first(path, key):
        try:
            for line in open(path):
                if line.startswith(key):
                    return line.split(":", 1)[1].strip()
        except OSError:
            pass
        return None

    flags = (first("/proc/cpuinfo", "flags") or "").split()
    osr = {}
    try:
        for line in open("/etc/os-release"):
            if "=" in line:
                k, v = line.rstrip().split("=", 1)
                osr[k] = v.strip('"')
    except OSError:
        pass
    mem = first("/proc/meminfo", "MemTotal")
    gov = None
    try:
        gov = open("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor").read().strip()
    except OSError:
        pass
    virt = ", ".join(x for x in [
        sh(["systemd-detect-virt", "--container"]) and "container: " + sh(["systemd-detect-virt", "--container"]),
        "hypervisor (CPUID flag set: a virtual machine)" if "hypervisor" in flags else None,
    ] if x) or "none detected"
    return {
        "date": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        "host": platform.node(),
        "cpu": first("/proc/cpuinfo", "model name") or platform.processor(),
        "cpu_mhz": first("/proc/cpuinfo", "cpu MHz"),
        "logical_cpus": os.cpu_count(),
        "cpu_flags_simd": [f for f in flags if f in ("sse4_2", "avx", "avx2", "avx512f", "bmi2", "fma")],
        "memory": mem,
        "os": osr.get("PRETTY_NAME", platform.system()),
        "kernel": platform.release(),
        "arch": platform.machine(),
        "virtualization": virt,
        "cpu_governor": gov or "not exposed",
        "load_average_start": [round(x, 2) for x in os.getloadavg()],
        "python": platform.python_version(),
        "commit": sh(["git", "-C", ROOT, "rev-parse", "HEAD"]),
        "commit_dirty": bool(sh(["git", "-C", ROOT, "status", "--porcelain", "--untracked-files=no"])),
        "codira": args.codira,
        "codira_version": sh([args.codira, "--version"]),
        "codira_binary_sha256": sha256(args.codira),
        "rustc": sh(["rustc", "--version"]),
        "cargo": sh(["cargo", "--version"]),
        "cxx": args.cxx,
        "cxx_version": (sh([args.cxx, "--version"]) or "").splitlines()[0:1],
        "llvm_config": sh([os.path.join(os.environ.get("LLVM_SYS_221_PREFIX", "/usr/lib/llvm-22"),
                                        "bin", "llvm-config"), "--version"]),
        "flags": {
            "codira": CODIRA_FLAGS + ["(target CPU: the compiler's default, generic x86-64)"],
            "codira_host": "cargo build --release (opt-level 3) of benches/aurora/host",
            "rust": RUST_FLAGS,
            "rust_cargo": "cargo build --release (profile in the baseline's Cargo.toml)",
            "cxx": CXX_FLAGS,
        },
        "perf": perf or ("not used (--no-perf)" if args.no_perf else "unavailable"),
        "perf_events": perf_status,
        "codira_env": {k: v for k, v in sorted(os.environ.items())
                       if k.startswith("CODIRA_") and k != "CODIRA_STD_PATH"},
        "runs": args.runs,
        "suite": args.suite,
        "seed": args.seed,
    }


def sha256(path):
    try:
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        return h.hexdigest()
    except OSError:
        return None


# --------------------------------------------------------------------- suite

class Kernel:
    def __init__(self, workload, spec, defaults):
        self.workload = workload
        self.name = spec["name"]
        self.id = f"{workload.name}.{self.name}"
        self.ci = spec.get("ci", False)
        self.entry = spec.get("entry", self.name)
        self.arg = spec.get("arg", self.name)
        # Integers passed to the Codira entry (kernels sharing one function).
        self.args = [str(a) for a in spec.get("args", [])]
        self.heap_limit_mb = spec.get("heap_limit_mb", workload.spec.get("heap_limit_mb",
                                      defaults.get("heap_limit_mb", 1024)))
        self.note = spec.get("note", "")
        # `check_differs = { cpp = "why" }`: that variant prints a different
        # check value for a known, explained reason; it is reported, not
        # failed.
        self.check_differs = spec.get("check_differs", {})
        # `expected_failure = { codira = "why" }`: that variant is known not
        # to complete today (a heap cap below what the runtime needs, say).
        # A failure is reported as expected; a success is reported as news.
        self.expected_failure = spec.get("expected_failure", {})
        self.unavailable = {}
        # `codira = "why"` (or `rust`, `cpp`) in a kernel: that variant
        # cannot be written yet, for the reason given.
        for v in VARIANTS:
            if isinstance(spec.get(v), str):
                self.unavailable[v] = spec[v]


class Workload:
    def __init__(self, spec, defaults):
        self.spec = spec
        self.name = spec["name"]
        self.dir = os.path.join(SUITE_DIR, spec.get("dir", self.name))
        self.title = spec.get("title", self.name)
        self.kernels = [Kernel(self, k, defaults) for k in spec["kernels"]]


SUITE_HEAP_FLOOR = [None]


def load_suite():
    with open(os.path.join(SUITE_DIR, "suite.toml"), "rb") as f:
        data = tomllib.load(f)
    defaults = data.get("defaults", {})
    SUITE_HEAP_FLOOR[0] = data.get("heap_floor")
    return defaults, [Workload(w, defaults) for w in data["workload"]]


# --------------------------------------------------------------------- build

class Builder:
    def __init__(self, args, work):
        self.args = args
        self.work = work
        self.records = []

    def run(self, what, cmd, cwd=None, env=None):
        log("+", " ".join(shlex.quote(c) for c in cmd))
        t = time.perf_counter()
        r = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)
        dt = time.perf_counter() - t
        self.records.append({"what": what, "command": cmd, "cwd": cwd, "seconds": round(dt, 3),
                             "status": r.returncode})
        if r.returncode != 0:
            log(r.stdout[-4000:], r.stderr[-4000:])
            return None, dt
        return r, dt

    def codira_env(self):
        return dict(os.environ, CODIRA_STD_PATH=os.path.join(ROOT, "std"))

    def host(self):
        env = dict(os.environ, CARGO_TARGET_DIR=self.args.cargo_target_dir)
        env.setdefault("LLVM_SYS_221_PREFIX", "/usr/lib/llvm-22")
        r, _ = self.run("host", ["cargo", "build", "--release", "--manifest-path",
                                 os.path.join(SUITE_DIR, "host", "Cargo.toml")], env=env)
        if r is None:
            sys.exit("cannot build the Codira host (benches/aurora/host)")
        return os.path.join(self.args.cargo_target_dir, "release", "aurora_host")

    def codira(self, w):
        manifest = os.path.join(w.dir, "codira.toml")
        if not os.path.exists(manifest):
            return None, "no Codira program"
        r, _ = self.run(f"{w.name}/codira", [self.args.codira] + CODIRA_FLAGS +
                        ["--manifest-path", manifest], env=self.codira_env())
        if r is None:
            return None, "the Codira program does not build"
        # The package's library and one library per standard-library module
        # it imports, loaded from the same directory.
        dst = os.path.join(self.work, "bin", f"{w.name}-codira")
        shutil.rmtree(dst, ignore_errors=True)
        shutil.copytree(os.path.join(w.dir, "target"), dst,
                        ignore=shutil.ignore_patterns(".codira*", "*.codira-tmp"))
        return os.path.join(dst, "mod.codiralib"), None

    def rust(self, w):
        out = os.path.join(self.work, "bin", f"{w.name}_rust")
        cargo = w.spec.get("rust_cargo")
        if cargo:
            env = dict(os.environ, CARGO_TARGET_DIR=os.path.join(self.work, "rust-target"))
            r, _ = self.run(f"{w.name}/rust", ["cargo", "build", "--release", "--manifest-path",
                                               os.path.join(w.dir, cargo)] +
                            w.spec.get("rust_cargo_args", []), env=env)
            if r is None:
                return None, "the Rust baseline does not build"
            shutil.copyfile(os.path.join(self.work, "rust-target", "release", w.spec["rust_bin"]), out)
            os.chmod(out, 0o755)
            return out, None
        src = os.path.join(w.dir, f"{w.name}.rs")
        if not os.path.exists(src):
            return None, "no Rust baseline"
        r, _ = self.run(f"{w.name}/rust", ["rustc"] + RUST_FLAGS + ["-o", out, src])
        return (out, None) if r else (None, "the Rust baseline does not build")

    def cpp(self, w):
        src = os.path.join(w.dir, f"{w.name}.cpp")
        if not os.path.exists(src):
            return None, "no C++ baseline"
        out = os.path.join(self.work, "bin", f"{w.name}_cpp")
        r, _ = self.run(f"{w.name}/cpp", [cxx_for(self.args, w)] + CXX_FLAGS + w.spec.get("cxx_args", []) +
                        ["-o", out, src])
        if r is None:
            need = w.spec.get("cxx_requires")
            return None, "the C++ baseline does not build" + (f" (it needs {need})" if need else "")
        return out, None


def cxx_for(args, w):
    """The C++ compiler of a workload: `cxx` in its suite entry, else --cxx."""
    return shutil.which(w.spec["cxx"]) or w.spec["cxx"] if "cxx" in w.spec else args.cxx


def compile_times(builder, workloads, runs, rng):
    """Time `runs` from-scratch builds of each variant of each workload."""
    out = {}
    jobs = []
    for w in workloads:
        if os.path.exists(os.path.join(w.dir, "codira.toml")):
            jobs.append((w, "codira"))
        if not w.spec.get("rust_cargo") and os.path.exists(os.path.join(w.dir, f"{w.name}.rs")):
            jobs.append((w, "rust"))
        if os.path.exists(os.path.join(w.dir, f"{w.name}.cpp")):
            jobs.append((w, "cpp"))
    tmp = os.path.join(builder.work, "compile")
    os.makedirs(tmp, exist_ok=True)
    for r in range(runs):
        rng.shuffle(jobs)
        for w, v in jobs:
            if v == "codira":
                shutil.rmtree(os.path.join(w.dir, "target"), ignore_errors=True)
                cmd = [builder.args.codira] + CODIRA_FLAGS + ["--manifest-path",
                                                              os.path.join(w.dir, "codira.toml")]
                env = builder.codira_env()
                art = os.path.join(w.dir, "target", "mod.codiralib")
            elif v == "rust":
                art = os.path.join(tmp, f"{w.name}_rust")
                cmd = ["rustc"] + RUST_FLAGS + ["-o", art, os.path.join(w.dir, f"{w.name}.rs")]
                env = None
            else:
                art = os.path.join(tmp, f"{w.name}_cpp")
                cmd = [cxx_for(builder.args, w)] + CXX_FLAGS + w.spec.get("cxx_args", []) + \
                    ["-o", art, os.path.join(w.dir, f"{w.name}.cpp")]
                env = None
            t = time.perf_counter()
            p = subprocess.run(cmd, env=env, capture_output=True)
            dt = time.perf_counter() - t
            rec = out.setdefault(f"{w.name}/{v}", {"seconds": [], "command": cmd})
            if p.returncode == 0:
                rec["seconds"].append(dt)
                rec["bytes"] = os.path.getsize(art)
                if v == "codira":
                    libs = _glob(os.path.join(w.dir, "target", "*.codiralib"))
                    rec["libraries"] = len(libs)
                    rec["bytes"] = sum(os.path.getsize(x) for x in libs)
                if v != "codira":
                    stripped = art + ".stripped"
                    if sh(["strip", "-o", stripped, art]) is not None:
                        rec["bytes_stripped"] = os.path.getsize(stripped)
            else:
                rec["error"] = p.stderr.decode(errors="replace")[-500:]
    for rec in out.values():
        rec["median_s"] = median(rec["seconds"])
        rec["ci_s"] = bootstrap_median_ci(rec["seconds"], rng)
    return out


# ----------------------------------------------------------------------- run

def run_one(cmd, env, cwd, perf, timeout):
    """Runs a kernel process; returns ({key: value}, rusage, perf counters, error)."""
    pf = None
    full = cmd
    if perf:
        pf = tempfile.NamedTemporaryFile("r", suffix=".perf", delete=False)
        pf.close()
        full = [perf, "stat", "-x,", "-o", pf.name, "-e", ",".join(PERF_EVENTS), "--"] + cmd
    out = tempfile.TemporaryFile()
    err = tempfile.TemporaryFile()
    p = subprocess.Popen(full, env=env, cwd=cwd, stdout=out, stderr=err)
    deadline = time.monotonic() + timeout
    while True:
        pid, status, ru = os.wait4(p.pid, os.WNOHANG)
        if pid:
            break
        if time.monotonic() > deadline:
            p.kill()
            os.wait4(p.pid, 0)
            return None, None, None, f"timed out after {timeout} s"
        time.sleep(0.002)
    code = os.waitstatus_to_exitcode(status)
    out.seek(0)
    err.seek(0)
    text = out.read().decode(errors="replace")
    counters = None
    if pf:
        try:
            counters = parse_perf(open(pf.name).read())
        finally:
            os.unlink(pf.name)
    if code != 0:
        return None, None, None, f"exit status {code}: " + err.read().decode(errors="replace")[-600:]
    values = {}
    for line in text.splitlines():
        if line.startswith("@@"):
            for pair in line[2:].split():
                k, _, v = pair.partition("=")
                values[k] = v
    if "ns" not in values:
        return None, None, None, "no `@@ ns=` line in the output: " + text[-300:]
    return values, {"maxrss_kb": ru.ru_maxrss, "utime": ru.ru_utime, "stime": ru.ru_stime,
                    "nvcsw": ru.ru_nvcsw, "nivcsw": ru.ru_nivcsw}, counters, None


def run_suite(args, kernels, binaries, host, perf, rng):
    """{kernel id: {variant: {samples: [...], ...}}}"""
    results = {k.id: {} for k in kernels}
    jobs = []
    for k in kernels:
        for v in VARIANTS:
            if v in k.unavailable:
                results[k.id][v] = {"unavailable": k.unavailable[v]}
                continue
            b, why = binaries[k.workload.name][v]
            if b is None:
                results[k.id][v] = {"unavailable": why}
                continue
            if v == "codira":
                cmd = [host, b, k.entry] + k.args
                env = dict(os.environ, AURORA_HEAP_LIMIT_MB=str(k.heap_limit_mb))
            else:
                cmd = [b, k.arg]
                env = dict(os.environ)
            results[k.id][v] = {"command": cmd, "samples": []}
            jobs.append((k, v, cmd, env))
    total = len(jobs) * (args.runs + (0 if args.no_warmup else 1))
    done = 0
    t0 = time.monotonic()
    for rnd in range(0 if args.no_warmup else -1, args.runs):
        order = list(jobs)
        rng.shuffle(order)
        for k, v, cmd, env in order:
            res = results[k.id][v]
            if "error" in res:
                continue
            values, ru, counters, error = run_one(cmd, env, args.work, perf, args.timeout)
            done += 1
            if error and v in k.expected_failure:
                res["expected_failure"] = error
                res["error"] = error  # stop running it
                continue
            if error:
                res["error"] = error
                log(f"!! {k.id} [{v}]: {error}")
                continue
            if rnd < 0:
                continue  # warm-up round: page cache, binaries, data files
            res["samples"].append({"round": rnd, "values": values, "rusage": ru, "perf": counters})
        el = time.monotonic() - t0
        label = "warm-up round" if rnd < 0 else f"round {rnd + 1}/{args.runs}"
        log(f"{label} done ({done}/{total} processes, {el:.0f} s)")
    return results


def summarize(kernels, results, rng):
    """Adds medians, intervals, ratios and checks to `results`; returns rows."""
    rows = []
    for k in kernels:
        r = results[k.id]
        checks = {}
        for v in VARIANTS:
            res = r.get(v, {})
            s = res.get("samples") or []
            if not s:
                continue
            ns = [float(x["values"]["ns"]) for x in s]
            res["ns"] = ns
            res["median_ns"] = median(ns)
            res["ci_ns"] = bootstrap_median_ci(ns, rng)
            rss = [float(x["values"].get("rss_peak_kb") or x["rusage"]["maxrss_kb"]) for x in s]
            res["median_rss_kb"] = median(rss)
            for key in ("p50_ns", "p95_ns", "p99_ns", "p999_ns", "max_ns", "gc_heap_bytes",
                        "gc_peak_heap_bytes", "gc_live_after_last_bytes", "gc_bytes_allocated",
                        "gc_objects_allocated", "gc_collections", "gc_collections_paced",
                        "gc_collections_at_limit", "gc_collection_time_ns", "gc_time_to_safepoint_ns",
                        "gc_pause_count", "gc_pause_p50_ns", "gc_pause_p99_ns", "gc_pause_max_ns",
                        "gc_last_moved", "gc_last_unmovable"):
                vals = [float(x["values"][key]) for x in s if key in x["values"]]
                if vals:
                    res["median_" + key] = median(vals)
                    if key.endswith("_ns"):
                        res["ci_" + key] = bootstrap_median_ci(vals, rng)
            counters = {}
            for e in PERF_EVENTS:
                vals = [x["perf"][e] for x in s if x["perf"] and x["perf"].get(e) is not None]
                if vals:
                    counters[e] = median(vals)
            res["median_perf"] = counters
            cs = {x["values"].get("check") for x in s}
            res["checks"] = sorted(cs)
            checks[v] = cs
        strict = {v: c for v, c in checks.items() if v not in k.check_differs}
        allc = set().union(*strict.values()) if strict else set()
        r["check_agrees"] = len(allc) <= 1
        r["check"] = sorted(allc)
        r["check_differs"] = {v: why for v, why in k.check_differs.items()
                              if v in checks and checks[v] != allc}
        for other in ("rust", "cpp"):
            a, b = r.get("codira", {}).get("ns"), r.get(other, {}).get("ns")
            if a and b:
                r[f"ratio_{other}"] = median(a) / median(b)
                r[f"ratio_{other}_ci"] = bootstrap_ratio_ci(a, b, rng)
        rows.append(k)
    return rows


# --------------------------------------------------------------- heap floor

def heap_floor(args, spec, binaries, host):
    """The smallest GC heap cap, to `step_mb`, under which a Codira kernel
    completes, found by bisection between `lo_mb` and `hi_mb`, with the
    live bytes the runtime reports at that cap. A tracked metric: the next
    memory regression shows up as a larger number."""
    w, entry = spec["workload"], spec["entry"]
    lib = binaries.get(w, {}).get("codira", (None, None))[0]
    if lib is None or host is None:
        return {"error": "the Codira variant of the workload was not built"}
    step = spec.get("step_mb", 4)
    lo, hi = spec.get("lo_mb", 16), spec.get("hi_mb", 256)
    tries = {}

    def attempt(mb):
        env = dict(os.environ, AURORA_HEAP_LIMIT_MB=str(mb))
        values, _, _, error = run_one([host, lib, entry] + [str(a) for a in spec.get("args", [])],
                                      env, args.work, None, args.timeout)
        tries[mb] = values if values else {"error": (error or "")[-300:]}
        log(f"heap floor: {mb} MiB {'completes' if values else 'fails'}")
        return values

    if not attempt(hi):
        return {"error": f"does not complete under the largest cap tried, {hi} MiB", "tries": tries}
    if attempt(lo):
        hi = lo
    else:
        while hi - lo > step:  # invariant: lo fails, hi completes
            mid = (lo + hi) // 2 // step * step or step
            if mid <= lo or mid >= hi:
                break
            if attempt(mid):
                hi = mid
            else:
                lo = mid
    at = tries[hi]
    return {"kernel": f"{w}.{entry}", "floor_mb": hi, "fails_at_mb": lo if lo < hi else None,
            "step_mb": step, "live_after_last_bytes": int(at.get("gc_live_after_last_bytes", 0)),
            "peak_heap_bytes": int(at.get("gc_peak_heap_bytes", 0)),
            "collections": int(at.get("gc_collections", 0)), "ns": int(at["ns"]),
            "tries": tries}


# --------------------------------------------------------------------- gate

def round_ratios(res):
    """Codira / Rust time per round (the two ran in the same round), or the
    Codira times alone where there is no Rust variant."""
    c = {x["round"]: float(x["values"]["ns"]) for x in res.get("codira", {}).get("samples", [])}
    r = {x["round"]: float(x["values"]["ns"]) for x in res.get("rust", {}).get("samples", [])}
    if c and r:
        return "ratio", [c[i] / r[i] for i in sorted(c) if i in r]
    return "time", list(c.values())


def gate(baseline, current, kernel_ids, alpha, rng):
    """One-sided tests of "Codira got slower"; Holm-corrected."""
    tests = []
    for kid in kernel_ids:
        b, c = baseline["kernels"].get(kid), current["kernels"].get(kid)
        if not b or not c:
            continue
        mb, xb = round_ratios(b)
        mc, xc = round_ratios(c)
        if mb != mc or len(xb) < 3 or len(xc) < 3:
            continue
        p = mann_whitney_greater(xc, xb)
        change = median(xc) / median(xb) - 1
        lo, hi = bootstrap_ratio_ci(xc, xb, rng)
        tests.append({"kernel": kid, "metric": mb, "p": p, "change": change,
                      "change_ci": (lo - 1, hi - 1) if lo else (None, None),
                      "baseline_median": median(xb), "current_median": median(xc)})
    tests.sort(key=lambda t: t["p"])
    m = len(tests)
    still = True
    for i, t in enumerate(tests):
        threshold = alpha / (m - i)
        t["holm_threshold"] = threshold
        still = still and t["p"] <= threshold
        t["regressed"] = bool(still and t["change_ci"][0] is not None and t["change_ci"][0] > 0)
    return tests


# ------------------------------------------------------------------ reports

def fmt_ms(ns):
    return "–" if ns is None else f"{ns / 1e6:.2f}"


def fmt_ci(ci):
    if not ci or ci[0] is None:
        return ""
    return f" [{ci[0] / 1e6:.2f}, {ci[1] / 1e6:.2f}]"


def fmt_ratio(r, ci):
    if r is None:
        return "–"
    s = f"{r:.2f}x"
    if ci and ci[0] is not None:
        s += f" [{ci[0]:.2f}, {ci[1]:.2f}]"
    return s


def markdown(env, workloads, kernels, results, compile_info, gate_tests, floor=None, floor_cmp=None):
    L = []
    L.append("# AURORA benchmark run\n")
    L.append(f"- date: {env['date']}, commit `{(env['commit'] or '?')[:12]}`"
             f"{' (with uncommitted changes)' if env['commit_dirty'] else ''}")
    L.append(f"- machine: {env['cpu']}, {env['logical_cpus']} logical CPUs, {env['memory']}, "
             f"{env['os']}, kernel {env['kernel']}, virtualization: {env['virtualization']}, "
             f"governor: {env['cpu_governor']}")
    la = env.get("load_average_start")
    le = env.get("load_average_end")
    L.append(f"- load average (1, 5, 15 min): {la} at the start, {le} at the end")
    L.append(f"- toolchains: {env['codira_version']} (`{' '.join(env['flags']['codira'][:3])}`), "
             f"{env['rustc']} (`{' '.join(RUST_FLAGS)}`), "
             f"{(env['cxx_version'] or ['?'])[0]} (`{' '.join(CXX_FLAGS)}`)")
    if env.get("codira_env"):
        L.append("- Codira runtime settings from the environment: " +
                 ", ".join(f"`{k}={v}`" for k, v in env["codira_env"].items()))
    L.append(f"- runs: {env['runs']} processes per kernel and variant, interleaved in a shuffled "
             f"order each round (seed {env['seed']}), after one warm-up round")
    supported = [e for e, s in env["perf_events"].items() if s == "ok"]
    missing = [f"{e} ({s})" for e, s in env["perf_events"].items() if s != "ok"]
    if not os.path.isabs(str(env["perf"])):
        L.append(f"- `perf`: {env['perf']}, no counters recorded")
    else:
        L.append(f"- `perf stat` counters recorded: {', '.join(supported) or 'none'}; "
                 f"not available: {', '.join(missing) or 'none'}")
    L.append("\nTimes are milliseconds of the kernel alone (set-up excluded), the median "
             "of the runs with a bootstrap 95% interval for the median in brackets. "
             "Ratios are Codira's median over the baseline's, with a bootstrap 95% "
             "interval; above 1 means Codira is slower. RSS is the median peak resident "
             "set of the whole process, in MiB.\n")
    for w in workloads:
        ks = [k for k in kernels if k.workload is w]
        if not ks:
            continue
        L.append(f"## {w.title}\n")
        L.append("| kernel | Codira | Rust | C++ | Codira/Rust | Codira/C++ | RSS C / R / C++ | check |")
        L.append("|---|---:|---:|---:|---:|---:|---:|---|")
        notes = []
        for k in ks:
            r = results[k.id]
            cells = []
            for v in VARIANTS:
                res = r.get(v, {})
                if "median_ns" in res:
                    cells.append(fmt_ms(res["median_ns"]) + fmt_ci(res["ci_ns"]))
                elif "expected_failure" in res:
                    cells.append("does not complete")
                    notes.append(f"`{k.name}` {v}: does not complete ({k.expected_failure[v]}): "
                                 f"{res['expected_failure'].strip()[:300]}")
                elif "error" in res:
                    cells.append("failed")
                    notes.append(f"`{k.name}` {v}: failed: {res['error'][:200]}")
                else:
                    cells.append("n/a")
                    notes.append(f"`{k.name}` {v}: {res.get('unavailable', 'not run')}")
            rss = " / ".join("–" if "median_rss_kb" not in r.get(v, {}) else
                             f"{r[v]['median_rss_kb'] / 1024:.0f}" for v in VARIANTS)
            check = "same" if r["check_agrees"] else "DIFFERENT: " + ", ".join(r["check"])
            for v, why in r.get("check_differs", {}).items():
                check += f" ({v}: differs, expected)"
                notes.append(f"`{k.name}` {v} check value differs: {why}")
            L.append(f"| {k.name} | {cells[0]} | {cells[1]} | {cells[2]} | "
                     f"{fmt_ratio(r.get('ratio_rust'), r.get('ratio_rust_ci'))} | "
                     f"{fmt_ratio(r.get('ratio_cpp'), r.get('ratio_cpp_ci'))} | {rss} | {check} |")
            if k.note:
                notes.append(f"`{k.name}`: {k.note}")
        if "cxx" in w.spec:
            notes.append(f"C++ compiler for this workload: `{w.spec['cxx']}` "
                         f"({w.spec.get('cxx_why', 'see suite.toml')})")
        lat = [k for k in ks if any("median_p50_ns" in results[k.id].get(v, {}) for v in VARIANTS)]
        if lat:
            L.append("\nLatency per operation (median over the runs of each percentile, microseconds):\n")
            L.append("| kernel | variant | p50 | p95 | p99 | p99.9 | max |")
            L.append("|---|---|---:|---:|---:|---:|---:|")
            for k in lat:
                for v in VARIANTS:
                    res = results[k.id].get(v, {})
                    if "median_p50_ns" not in res:
                        continue
                    cols = []
                    for key in ("p50", "p95", "p99", "p999", "max"):
                        val = res.get(f"median_{key}_ns")
                        ci = res.get(f"ci_{key}_ns")
                        c = f"{val / 1e3:.1f}" if val is not None else "–"
                        if ci and ci[0] is not None:
                            c += f" [{ci[0] / 1e3:.1f}, {ci[1] / 1e3:.1f}]"
                        cols.append(c)
                    L.append(f"| {k.name} | {v} | " + " | ".join(cols) + " |")
        gcs = [k for k in ks if "median_gc_peak_heap_bytes" in results[k.id].get("codira", {})]
        if gcs:
            L.append("\nCodira GC (`Runtime::stats()` at the end of the kernel's process; medians over "
                     "the runs, pauses with a bootstrap 95% interval):\n")
            L.append("| kernel | heap cap (MiB) | peak heap (MiB) | live after last GC (MiB) | allocated (MiB) "
                     "| collections (paced / at cap) | stopped (ms) | to safepoint (ms) "
                     "| pause p50 (µs) | pause p99 (µs) | pause max (µs) |")
            L.append("|---|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|")
            for k in gcs:
                res = results[k.id]["codira"]
                g = lambda key: res.get("median_" + key, 0)

                def pause(q):
                    v, ci = res.get(f"median_gc_pause_{q}_ns"), res.get(f"ci_gc_pause_{q}_ns")
                    if v is None:
                        return "–"
                    return f"{v / 1e3:.0f}" + (f" [{ci[0] / 1e3:.0f}, {ci[1] / 1e3:.0f}]" if ci and ci[0] is not None else "")
                L.append(f"| {k.name} | {k.heap_limit_mb} | {g('gc_peak_heap_bytes') / 2**20:.1f} | "
                         f"{g('gc_live_after_last_bytes') / 2**20:.1f} | {g('gc_bytes_allocated') / 2**20:.0f} | "
                         f"{g('gc_collections'):.0f} ({g('gc_collections_paced'):.0f} / {g('gc_collections_at_limit'):.0f}) | "
                         f"{g('gc_collection_time_ns') / 1e6:.1f} | {g('gc_time_to_safepoint_ns') / 1e6:.2f} | "
                         f"{pause('p50')} | {pause('p99')} | {pause('max')} |")
        perf_rows = [k for k in ks if any(results[k.id].get(v, {}).get("median_perf") for v in VARIANTS)]
        if perf_rows:
            evs = [e for e in PERF_EVENTS if env["perf_events"].get(e) == "ok"]
            L.append("\n`perf stat` (median per process, whole process including start-up):\n")
            L.append("| kernel | variant | " + " | ".join(evs) + " |")
            L.append("|---|---|" + "---:|" * len(evs))
            for k in perf_rows:
                for v in VARIANTS:
                    mp = results[k.id].get(v, {}).get("median_perf")
                    if mp:
                        L.append(f"| {k.name} | {v} | " +
                                 " | ".join(f"{mp[e]:.0f}" if e in mp else "–" for e in evs) + " |")
        if notes:
            L.append("")
            for n in notes:
                L.append(f"- {n}")
        L.append("")
    if floor:
        L.append("## Heap floor\n")
        if "error" in floor:
            L.append(f"Not found: {floor['error']}\n")
        else:
            L.append(f"The smallest GC heap cap under which `{floor['kernel']}` completes, found by "
                     f"bisection to {floor['step_mb']} MiB (one run per cap tried), and what the "
                     f"runtime reports at that cap. A tracked metric: a memory regression raises it.\n")
            L.append("| kernel | completes at (MiB) | fails at (MiB) | live after last GC (MiB) "
                     "| peak heap (MiB) | collections | time (ms) |")
            L.append("|---|---:|---:|---:|---:|---:|---:|")
            L.append(f"| {floor['kernel']} | {floor['floor_mb']} | {floor['fails_at_mb'] or '–'} | "
                     f"{floor['live_after_last_bytes'] / 2**20:.1f} | {floor['peak_heap_bytes'] / 2**20:.1f} | "
                     f"{floor['collections']} | {floor['ns'] / 1e6:.0f} |")
            L.append("")
    if compile_info:
        L.append("## Compile time and binary size\n")
        L.append("From-scratch builds of each workload's program (Codira: `codira build -O 3`; "
                 "the size is every `.codiralib` it writes, the package's and one per "
                 "standard-library module it imports, without the runtime that loads them; Rust: one `rustc` "
                 "invocation; C++: one compiler invocation). Seconds: median [bootstrap 95%].\n")
        L.append("| workload | variant | build (s) | artifact (KiB) | stripped (KiB) |")
        L.append("|---|---|---:|---:|---:|")
        for key in sorted(compile_info):
            rec = compile_info[key]
            w, v = key.split("/")
            if rec.get("median_s") is None:
                L.append(f"| {w} | {v} | failed | | |")
                continue
            ci = rec["ci_s"]
            L.append(f"| {w} | {v} | {rec['median_s']:.3f} [{ci[0]:.3f}, {ci[1]:.3f}] | "
                     f"{rec.get('bytes', 0) / 1024:.0f} | "
                     f"{rec['bytes_stripped'] / 1024:.0f} |" if "bytes_stripped" in rec else
                     f"| {w} | {v} | {rec['median_s']:.3f} [{ci[0]:.3f}, {ci[1]:.3f}] | "
                     f"{rec.get('bytes', 0) / 1024:.0f} | – |")
        L.append("")
    if gate_tests is not None:
        L.append("## Regression gate\n")
        if not gate_tests:
            L.append("No kernel could be compared with the baseline.\n")
        else:
            L.append("One-sided Mann-Whitney U per kernel on the per-round Codira/Rust ratio "
                     "(Codira's time where there is no Rust variant), Holm-corrected; a "
                     "regression also needs the bootstrap 95% interval of the change to exclude 0.\n")
            L.append("| kernel | metric | baseline | current | change [95%] | p | Holm threshold | verdict |")
            L.append("|---|---|---:|---:|---:|---:|---:|---|")
            for t in gate_tests:
                lo, hi = t["change_ci"]
                ci = f"[{lo * 100:+.1f}%, {hi * 100:+.1f}%]" if lo is not None else ""
                fmt = (lambda x: f"{x:.3f}") if t["metric"] == "ratio" else fmt_ms
                L.append(f"| {t['kernel']} | {t['metric']} | {fmt(t['baseline_median'])} | "
                         f"{fmt(t['current_median'])} | {t['change'] * 100:+.1f}% {ci} | {t['p']:.4f} | "
                         f"{t['holm_threshold']:.4f} | {'**REGRESSED**' if t['regressed'] else 'ok'} |")
        if floor_cmp:
            moved = floor_cmp["current_mb"] - floor_cmp["baseline_mb"]
            L.append(f"Heap floor: {floor_cmp['baseline_mb']} MiB in the baseline, "
                     f"{floor_cmp['current_mb']} MiB now"
                     f"{' (higher: memory use grew; one bisection, no statistical test)' if moved > 0 else ''}.")
        L.append("")
    return "\n".join(L)


# --------------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--suite", choices=["full", "ci"], default="full")
    ap.add_argument("--only", action="append", default=[],
                    help="run only kernels whose id (workload.kernel) contains this text")
    ap.add_argument("--runs", type=int, help="processes per kernel and variant")
    ap.add_argument("--work", default=os.path.join(ROOT, "target", "aurora"))
    ap.add_argument("--out", help="result directory (default: <work>/results/<date>)")
    ap.add_argument("--codira", default=os.path.join(ROOT, "target", "release", "codira"))
    ap.add_argument("--cxx", default=shutil.which("clang++-22") or shutil.which("clang++") or "c++")
    ap.add_argument("--cargo-target-dir", default=os.path.join(ROOT, "target"))
    ap.add_argument("--variants", default=",".join(VARIANTS))
    ap.add_argument("--no-perf", action="store_true")
    ap.add_argument("--no-warmup", action="store_true")
    ap.add_argument("--compile-runs", type=int, help="builds per variant for compile time (0: skip)")
    ap.add_argument("--no-heap-floor", action="store_true", help="skip the heap floor bisection")
    ap.add_argument("--timeout", type=float, default=300)
    ap.add_argument("--seed", type=int, default=20261106)
    ap.add_argument("--baseline", help="results.json to compare with (the regression gate)")
    ap.add_argument("--alpha", type=float, default=0.05, help="family-wise error rate of the gate")
    ap.add_argument("--save-baseline", help="also write the results here, trimmed for a baseline")
    args = ap.parse_args()

    defaults, workloads = load_suite()
    if args.runs is None:
        args.runs = defaults.get("ci_runs" if args.suite == "ci" else "runs", 10)
    if args.compile_runs is None:
        args.compile_runs = 0 if args.suite == "ci" else defaults.get("compile_runs", 5)
    args.codira = os.path.abspath(args.codira)
    if not os.path.exists(args.codira):
        sys.exit(f"no codira at {args.codira}: build it with `cargo build --release -p codira` "
                 "or pass --codira")
    args.work = os.path.abspath(args.work)
    os.makedirs(os.path.join(args.work, "bin"), exist_ok=True)
    variants = [v for v in args.variants.split(",") if v]
    rng = random.Random(args.seed)

    kernels = [k for w in workloads for k in w.kernels
               if (args.suite == "full" or k.ci) and
               (not args.only or any(o in k.id for o in args.only))]
    if not kernels:
        sys.exit("no kernel selected")
    for k in kernels:
        for v in VARIANTS:
            if v not in variants:
                k.unavailable.setdefault(v, "not selected (--variants)")
    used = [w for w in workloads if any(k.workload is w for k in kernels)]

    perf, perf_why = (None, "--no-perf") if args.no_perf else find_perf()
    perf_status = probe_perf(perf) if perf else {e: perf_why for e in PERF_EVENTS}
    if perf and not any(s == "ok" for s in perf_status.values()):
        perf = None
    env = environment(args, perf, perf_status)

    builder = Builder(args, args.work)
    for w in used:
        for script in w.spec.get("setup", []):
            builder.run(f"{w.name}/setup", [sys.executable, "-I", os.path.join(w.dir, script),
                                            os.path.join(args.work, "data")])
    host = builder.host() if "codira" in variants else None
    binaries = {}
    for w in used:
        binaries[w.name] = {
            "codira": builder.codira(w) if "codira" in variants else (None, "not selected"),
            "rust": builder.rust(w) if "rust" in variants else (None, "not selected"),
            "cpp": builder.cpp(w) if "cpp" in variants else (None, "not selected"),
        }

    results = run_suite(args, kernels, binaries, host, perf if not args.no_perf else None, rng)
    env["load_average_end"] = [round(x, 2) for x in os.getloadavg()]
    summarize(kernels, results, rng)

    floor = None
    floor_spec = SUITE_HEAP_FLOOR[0]
    if floor_spec and not args.no_heap_floor and "codira" in variants and \
            any(w.name == floor_spec["workload"] for w in used):
        log("finding the heap floor ...")
        floor = heap_floor(args, floor_spec, binaries, host)

    compile_info = None
    if args.compile_runs > 0:
        log("timing builds ...")
        compile_info = compile_times(builder, used, args.compile_runs, rng)
        for w in used:  # leave a library in place for later runs
            builder.codira(w)

    doc = {
        "format": "aurora-bench/1",
        "environment": env,
        "builds": builder.records,
        "kernels": {k.id: dict(results[k.id], heap_limit_mb=k.heap_limit_mb, ci=k.ci)
                    for k in kernels},
        "compile": compile_info,
        "heap_floor": floor,
    }
    gate_tests = None
    failed_gate = False
    if args.baseline:
        with open(args.baseline) as f:
            base = json.load(f)
        gate_tests = gate(base, doc, [k.id for k in kernels], args.alpha, rng)
        doc["gate"] = {"baseline": args.baseline, "alpha": args.alpha, "tests": gate_tests}
        failed_gate = any(t["regressed"] for t in gate_tests)
        bf, cf = base.get("heap_floor"), floor
        if bf and cf and bf.get("floor_mb") and cf.get("floor_mb"):
            doc["gate"]["heap_floor"] = {"baseline_mb": bf["floor_mb"], "current_mb": cf["floor_mb"],
                                         "baseline_live": bf.get("live_after_last_bytes"),
                                         "current_live": cf.get("live_after_last_bytes")}

    out = args.out or os.path.join(args.work, "results",
                                   datetime.datetime.now().strftime("%Y%m%d-%H%M%S"))
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "results.json"), "w") as f:
        json.dump(doc, f, indent=1, default=str)
    md = markdown(env, used, kernels, results, compile_info, gate_tests, floor,
                  doc.get("gate", {}).get("heap_floor"))
    with open(os.path.join(out, "summary.md"), "w") as f:
        f.write(md + "\n")
    if args.save_baseline:
        trimmed = {"format": doc["format"], "environment": env,
                   "heap_floor": {k: v for k, v in (floor or {}).items() if k != "tries"} or None,
                   "kernels": {kid: {v: {"samples": [{"round": s["round"],
                                                      "values": {"ns": s["values"]["ns"],
                                                                 "check": s["values"].get("check")}}
                                                     for s in r[v].get("samples", [])]}
                                     for v in VARIANTS if isinstance(r.get(v), dict)}
                               for kid, r in doc["kernels"].items()}}
        with open(args.save_baseline, "w") as f:
            json.dump(trimmed, f, indent=1, default=str)
            f.write("\n")
    print(md)
    log(f"results: {out}/results.json, {out}/summary.md")
    bad_checks = [k.id for k in kernels if not results[k.id]["check_agrees"]]
    errors = [f"{k.id} [{v}]" for k in kernels for v in VARIANTS
              if "error" in results[k.id].get(v, {}) and "expected_failure" not in results[k.id][v]]
    news = [f"{k.id} [{v}]" for k in kernels for v in k.expected_failure
            if results[k.id].get(v, {}).get("samples")]
    if news:
        log("expected to fail but completed (update suite.toml): " + ", ".join(news))
    if bad_checks:
        log("check values differ: " + ", ".join(bad_checks))
    if errors:
        log("failed: " + ", ".join(errors))
    if failed_gate:
        log("REGRESSION: " + ", ".join(t["kernel"] for t in gate_tests if t["regressed"]))
        return 3
    return 1 if (bad_checks or errors) else 0


if __name__ == "__main__":
    sys.exit(main())
