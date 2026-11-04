# Documented, never measured: no AArch64 hardware is available to the
# project. Values transcribed from the Arm Neoverse N1 Software Optimization
# Guide, Issue 4.0, integer instruction table. Pipelines: 0 = I0, 1 = I1,
# 2 = M (a throughput of 3 per cycle means I0, I1 and M).
# - `mul` has no X-form row; it is an alias of `madd` with xzr, so the MADD
#   X-form row (latency 4, 1/3 per cycle, M pipe) is applied: an
#   inference, flagged here.
# - `lsl/lsr/asr #imm` are UBFM/SBFM aliases; `mov x, x` is an ORR alias.
# - issue_width 4 is the decode width; an assumption for this model.
profile aarch64/neoverse-n1-sog@1
isa aarch64
issue_width 4
features
source documented: Arm Neoverse N1 Software Optimization Guide, Issue 4.0 (integer instructions); mul inferred from the MADD X-form row; not measured
form mov x, x | 1 | 0.333 | 1 | 0 1 2
form movz x, imm | 1 | 0.333 | 1 | 0 1 2
form add/sub x, x, x | 1 | 0.333 | 1 | 0 1 2
form add/sub x, x, x, lsl #1-4 | 1 | 0.333 | 1 | 0 1 2
form add/sub x, x, x, shift | 2 | 1 | 1 | 2
form logic x, x, x | 1 | 0.333 | 1 | 0 1 2
form logic x, x, x, shift | 1 | 0.333 | 1 | 0 1 2
form add/sub x, x, imm | 1 | 0.333 | 1 | 0 1 2
form shift x, x, imm | 1 | 0.333 | 1 | 0 1 2
form mul x, x, x | 4 | 3 | 1 | 2
form madd/msub x, x, x, x | 4 | 3 | 1 | 2
