#!/bin/sh
#
# Copyright (c) 2026 Omnira CJSC
# Author: Tunjay Akbarli
#
# Builds the Codira library and runs every caller against it, failing if any
# of them does. Each caller exits non-zero on a mismatch, so this is a test
# and not just a demonstration.
#
#     sh examples/interop/run-all.sh [path/to/codira]
#
# Callers whose toolchain is not installed are skipped with a note rather
# than failing the run -- not everyone has all five.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
codira=${1:-$repo/target/debug/codira}

library="$here/codira/target/mod.codiralib"

echo "building the Codira library"
"$codira" build --manifest-path "$here/codira/codira.toml"
test -f "$library" || { echo "expected $library to exist"; exit 1; }

failures=0
run() {
    name=$1
    shift
    echo
    echo "$name"
    if "$@"; then
        :
    else
        echo "  -> $name FAILED"
        failures=$((failures + 1))
    fi
}

skip() {
    echo
    echo "$1"
    echo "  (skipped: $2 is not installed)"
}

# --- C -----------------------------------------------------------------------
if command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; then
    compiler=$(command -v cc 2>/dev/null || command -v gcc)
    # POSIX needs -ldl for dlopen; Windows toolchains do not have it.
    if [ "${OS-}" = "Windows_NT" ]; then dl=""; else dl="-ldl"; fi
    # shellcheck disable=SC2086
    "$compiler" -O2 -o "$here/c/main" "$here/c/main.c" $dl
    run "C" "$here/c/main" "$library"
else
    skip "C" "cc/gcc"
fi

# --- Rust --------------------------------------------------------------------
if command -v cargo >/dev/null 2>&1; then
    run "Rust" cargo run --quiet --manifest-path "$here/rust/Cargo.toml" -- "$library"
else
    skip "Rust" "cargo"
fi

# --- Python ------------------------------------------------------------------
if command -v python3 >/dev/null 2>&1 || command -v python >/dev/null 2>&1; then
    py=$(command -v python3 2>/dev/null || command -v python)
    run "Python" "$py" "$here/python/main.py" "$library"
else
    skip "Python" "python"
fi

# --- TypeScript --------------------------------------------------------------
if command -v bun >/dev/null 2>&1; then
    run "TypeScript" bun run "$here/typescript/main.ts" "$library"
elif command -v deno >/dev/null 2>&1; then
    echo
    echo "TypeScript"
    echo "  (skipped: the example targets bun:ffi; Deno needs --allow-ffi and Deno.dlopen)"
else
    skip "TypeScript" "bun"
fi

# --- The Codira entry point, which exercises the `extern \"C\"` path ------------
run "Codira (hosted, reaches extern \"C\")" "$codira" start "$library" main

echo
if [ "$failures" -eq 0 ]; then
    echo "all callers passed"
else
    echo "$failures caller(s) failed"
    exit 1
fi
