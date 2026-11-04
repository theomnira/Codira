#!/usr/bin/env bash
# Copyright (c) 2026 Omnira CJSC
#
# Reruns the benchmarks in benches/ (a Cargo workspace of its own, see
# benches/RESULTS.md for why) and prints the machine description that
# RESULTS.md records next to the numbers.
#
# Usage: scripts/bench.sh [criterion arguments...]
#   scripts/bench.sh                       # every benchmark
#   scripts/bench.sh fibonacci             # only names matching "fibonacci"
#   scripts/bench.sh -- --save-baseline b1 # keep a named baseline
#   scripts/bench.sh -- --baseline b1      # compare with it
#
# Environment:
#   LLVM_SYS_221_PREFIX  LLVM 22 installation (default /usr/lib/llvm-22)
#   BENCH_TARGET_DIR     Cargo target directory (default target-bench at the
#                        repository root; it is large, delete it afterwards)
#   BENCH_MEASUREMENT    seconds of measurement per benchmark (default 3)
#   BENCH_WARMUP         seconds of warm-up per benchmark (default 1)

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export LLVM_SYS_221_PREFIX="${LLVM_SYS_221_PREFIX:-/usr/lib/llvm-22}"
export CARGO_TARGET_DIR="${BENCH_TARGET_DIR:-$root/target-bench}"
export CARGO_INCREMENTAL=0

echo "== machine"
echo "cpus:   $(nproc)"
echo "cpu:    $(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ //' || uname -p)"
echo "memory: $(grep MemTotal /proc/meminfo 2>/dev/null | awk '{printf "%.1f GiB", $2/1048576}')"
echo "os:     $( (. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME") || uname -s), kernel $(uname -r)"
echo "rustc:  $(rustc --version)"
echo "llvm:   $("$LLVM_SYS_221_PREFIX/bin/llvm-config" --version 2>/dev/null || echo unknown)"
echo "commit: $(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo

filter=()
if [[ $# -gt 0 && "$1" != "--" ]]; then
    filter=("$1")
    shift
fi
[[ "${1:-}" == "--" ]] && shift

cd "$root/benches"
cargo bench --bench benchmarks -- \
    --warm-up-time "${BENCH_WARMUP:-1}" \
    --measurement-time "${BENCH_MEASUREMENT:-3}" \
    "${filter[@]}" "$@"
