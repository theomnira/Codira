#!/usr/bin/env bash
. $(dirname $0)/common.inc

# `.quad sym` is assembled into R_RISCV_64 even in an RV32 object. That
# is not the word-size absolute relocation on RV32, so it cannot be
# turned into a dynamic relocation and must be resolved at link-time.

cat <<EOF | $CC -march=rv32gc -mabi=ilp32 -o $t/a.o -c -xassembler - || skip
.globl _start
_start:
  ret
.data
.quad _start
EOF

# funis is built without its RV32 target (see NOTICE), so there is nothing
# to link the object with.
out=$(./funis -o $t/exe1 $t/a.o 2>&1 || true)
if [[ $out == *'unsupported target: riscv32'* ]]; then
  echo "funis is built without riscv32: skipped"
  skip
fi

./funis -o $t/exe1 $t/a.o --section-start=.text=0x12345678
readelf -x .data $t/exe1 | grep -F '78563412 00000000'

not ./funis -pie -o $t/exe2 $t/a.o |&
  grep 'R_RISCV_64 relocation .* can not be used; recompile with -fPIC'
