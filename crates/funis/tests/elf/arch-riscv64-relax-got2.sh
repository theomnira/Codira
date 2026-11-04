#!/usr/bin/env bash
. $(dirname $0)/common.inc

# The GOT load follows a call that is relaxed to JAL, so it is shifted by
# the bytes removed from the call.
cat <<EOF | $CC -o $t/a.o -c -xassembler -
.globl get_foo, foo
get_foo:
  .option push
  .option pic
  call t0, bar
  la a0, foo
  .option pop
  ret
bar:
  jr t0
.data
foo:
  .word 0
EOF

cat <<EOF | $CC -o $t/b.o -c -xc -
#include <stdio.h>
extern int foo;
int *get_foo();
int main() { printf("%d\n", get_foo() == &foo); }
EOF

# Before binutils 2.40, GNU as does not mark a GOT load relaxable (no
# R_RISCV_RELAX after its R_RISCV_GOT_HI20), and then the load must stay.
relocs=$($OBJDUMP -r $t/a.o | grep -A1 R_RISCV_GOT_HI20)
if [[ $relocs != *R_RISCV_RELAX* ]]; then
  echo "the assembler does not mark the GOT load relaxable: skipped"
  skip
fi

$CC -B. -o $t/exe $t/a.o $t/b.o
$QEMU $t/exe | grep -x 1

$OBJDUMP -d $t/exe | grep -A3 '<get_foo>:' > $t/log
grep -E $'jal\tt0,' $t/log
grep -E $'addi\ta0,a0,' $t/log
