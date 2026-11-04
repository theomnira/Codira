profile x86_64/emerald-rapids-vm@1
isa x86_64
issue_width 6
features bmi1 bmi2
source latency and reciprocal throughput measured by codira_mcode::measure on `Intel(R) Xeon(R) Processor @ 2.10GHz` (4 cpus), median of 21 runs, ns per cycle 0.3626 from a dependent add chain; issue width 6 documented (Golden Cove rename width); port sets: documented, uops.info instructions.xml (2026-03-29), ADL-P and EMR columns (identical for these forms), not measured
form add/sub r64, imm | 0.2 | 0.25 | 1 | 0 1 5 6 10
form alu r64, r64 | 1 | 0.3 | 1 | 0 1 5 6 10
form andn r64, r64, r64 | 1 | 0.35 | 1 | 1 5 10
form imul r64, r64 | 3 | 1.05 | 1 | 1
form imul r64, r64, imm | 3 | 1.05 | 1 | 1
form lea b+i or b+d | 1 | 0.3 | 1 | 0 1 5 6 10
form lea b+i*s | 2 | 0.45 | 1 | 0 5 6
form lea b+i*s+d | 2 | 0.45 | 1 | 0 5 6
form logic r64, imm | 1 | 0.3 | 1 | 0 1 5 6 10
form mov r64, imm32 | 1 | 0.3 | 1 | 0 1 5 6 10
form mov r64, r64 | 0.2 | 0.3 | 1 | 0 1 5 6 10
form movabs r64, imm64 | 1 | 0.7 | 1 | 0 1 5 6 10
form neg/not r64 | 1 | 0.3 | 1 | 0 1 5 6 10
form shift r64, imm | 1 | 0.6 | 1 | 0 6
