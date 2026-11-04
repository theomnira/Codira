//! Running blocks on the host processor.
//!
//! A cost model predicts; this module measures. It writes each block into
//! an assembly kernel (a loop that executes the block `UNROLL` times per
//! iteration), compiles the kernels with a C driver using the system C
//! compiler, runs the driver pinned to one CPU, and reports the median time
//! per block execution over repeated runs, with the minimum and maximum.
//!
//! Cycles are not read from a hardware counter (virtual machines often have
//! none; the one these were developed on has none). Instead every run also
//! times a reference kernel, a dependent chain of `add r64, r64`
//! instructions, whose latency is one cycle on every x86-64 processor this
//! crate has a profile for, and converts nanoseconds to core cycles with
//! it. That makes the result independent of the current clock frequency,
//! under the stated assumption.
//!
//! Two modes:
//!
//! * [`Mode::Latency`]: copies are chained, the first live-out of one copy
//!   being the first live-in of the next (the two registers are swapped in
//!   every other copy when they differ). The result is the latency of the
//!   path from the first live-in to the first live-out, in cycles.
//! * [`Mode::Throughput`]: copies run on disjoint register sets and the
//!   inputs are not changed by the block unless a live-out is also a
//!   live-in. The result is cycles per block in steady state.
//!
//! Only x86-64 Linux hosts are supported; anything else is refused with an
//! explanation, never approximated.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::x86::{Reg, ALL_REGS, X86_64};
use crate::{Block, Isa};

pub const UNROLL: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Latency,
    Throughput,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Measurement {
    /// Median core cycles per block execution.
    pub cycles: f64,
    pub cycles_min: f64,
    pub cycles_max: f64,
    /// Median nanoseconds per block execution.
    pub ns: f64,
    /// Measured nanoseconds per core cycle (from the reference chain).
    pub ns_per_cycle: f64,
    pub repetitions: u32,
}

/// The harness: a C compiler and a scratch directory.
pub struct Harness {
    cc: PathBuf,
    dir: PathBuf,
    pub iterations: u64,
    pub repetitions: u32,
}

impl Harness {
    /// Finds a C compiler (`CODIRA_CC`, then `clang-22`, `clang`, `cc`) and
    /// checks the host is x86-64 Linux.
    pub fn detect(dir: &Path) -> Result<Harness, String> {
        if !cfg!(all(target_arch = "x86_64", target_os = "linux")) {
            return Err("measurement needs an x86-64 Linux host".into());
        }
        let candidates: Vec<String> = std::env::var("CODIRA_CC")
            .into_iter()
            .chain(["clang-22", "clang", "cc"].map(String::from))
            .collect();
        let cc = candidates
            .into_iter()
            .find(|c| {
                Command::new(c)
                    .arg("--version")
                    .output()
                    .is_ok_and(|o| o.status.success())
            })
            .ok_or("no C compiler found (set CODIRA_CC)")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        Ok(Harness {
            cc: PathBuf::from(cc),
            dir: dir.to_path_buf(),
            iterations: 2_000_000,
            repetitions: 21,
        })
    }

    /// Measures each block in `mode`. All blocks are compiled into one
    /// program and timed in the same run, interleaved, so slow drift of the
    /// machine affects them alike.
    pub fn measure(
        &self,
        blocks: &[Block<X86_64>],
        mode: Mode,
    ) -> Result<Vec<Measurement>, String> {
        let mut asm = String::from(".intel_syntax noprefix\n.text\n");
        // Reference: dependent add chain, 1 cycle per add.
        let reference =
            Block::<X86_64>::parse("add rax, rcx", vec![Reg::Rax, Reg::Rcx], vec![Reg::Rax])?;
        emit_kernel(&mut asm, "codira_ref", &reference, Mode::Latency)?;
        for (i, b) in blocks.iter().enumerate() {
            b.check()?;
            emit_kernel(&mut asm, &format!("codira_k{i}"), b, mode)?;
        }
        let driver = driver_source(blocks.len(), self.iterations, self.repetitions);
        let asm_path = self.dir.join("kernels.S");
        let c_path = self.dir.join("driver.c");
        let exe = self.dir.join("bench");
        std::fs::write(&asm_path, asm).map_err(|e| e.to_string())?;
        std::fs::write(&c_path, driver).map_err(|e| e.to_string())?;
        let out = Command::new(&self.cc)
            .args(["-O2", "-o"])
            .arg(&exe)
            .arg(&c_path)
            .arg(&asm_path)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "compiling the harness failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let run = Command::new(&exe).output().map_err(|e| e.to_string())?;
        if !run.status.success() {
            return Err(format!(
                "harness failed: {}",
                String::from_utf8_lossy(&run.stderr)
            ));
        }
        parse_output(
            &String::from_utf8_lossy(&run.stdout),
            blocks.len(),
            self.repetitions,
        )
    }
}

impl Harness {
    /// Executes every block on the host processor once per input vector and
    /// returns the live-out values: `result[block][input]`. Used to check
    /// the instruction models against the real processor.
    pub fn execute(
        &self,
        blocks: &[Block<X86_64>],
        inputs: &[Vec<u64>],
    ) -> Result<Vec<Vec<Vec<u64>>>, String> {
        let mut asm = String::from(".intel_syntax noprefix\n.text\n");
        for (i, b) in blocks.iter().enumerate() {
            b.check()?;
            if b.live_in.contains(&Reg::Rsp) || b.written().contains(&Reg::Rsp) {
                return Err("blocks that use rsp cannot be executed".into());
            }
            // void k(const uint64_t *in /*rdi*/, uint64_t *out /*rsi*/);
            // inputs and outputs go through static buffers so every
            // register is available to the block.
            let _ = writeln!(asm, ".globl codira_x{i}\ncodira_x{i}:");
            for r in [Reg::Rbx, Reg::Rbp, Reg::R12, Reg::R13, Reg::R14, Reg::R15] {
                let _ = writeln!(asm, "  push {r}");
            }
            for j in 0..b.live_in.len() {
                let _ = writeln!(
                    asm,
                    "  mov rax, [rdi + {}]\n  mov [rip + codira_in + {}], rax",
                    8 * j,
                    8 * j
                );
            }
            let _ = writeln!(asm, "  mov [rip + codira_outp], rsi");
            for (j, r) in b.live_in.iter().enumerate() {
                let _ = writeln!(asm, "  mov {r}, [rip + codira_in + {}]", 8 * j);
            }
            for inst in &b.insts {
                let _ = writeln!(asm, "  {inst}");
            }
            for (j, r) in b.live_out.iter().enumerate() {
                let _ = writeln!(asm, "  mov [rip + codira_out + {}], {r}", 8 * j);
            }
            let _ = writeln!(asm, "  mov rsi, [rip + codira_outp]");
            for j in 0..b.live_out.len() {
                let _ = writeln!(
                    asm,
                    "  mov rax, [rip + codira_out + {}]\n  mov [rsi + {}], rax",
                    8 * j,
                    8 * j
                );
            }
            for r in [Reg::R15, Reg::R14, Reg::R13, Reg::R12, Reg::Rbp, Reg::Rbx] {
                let _ = writeln!(asm, "  pop {r}");
            }
            let _ = writeln!(asm, "  ret");
        }
        asm.push_str(
            ".bss\n.p2align 4\ncodira_in: .zero 128\ncodira_out: .zero 128\ncodira_outp: .zero 8\n",
        );
        let mut c = String::from("#include <stdio.h>\n#include <stdint.h>\n");
        for i in 0..blocks.len() {
            let _ = writeln!(c, "void codira_x{i}(const uint64_t*, uint64_t*);");
        }
        let _ = writeln!(c, "int main(void){{uint64_t in[16],out[16];");
        for (i, b) in blocks.iter().enumerate() {
            for v in inputs {
                for (j, x) in v.iter().take(b.live_in.len()).enumerate() {
                    let _ = write!(c, "in[{j}]={x}ULL;");
                }
                let _ = write!(c, "codira_x{i}(in,out);printf(\"{i}");
                for _ in 0..b.live_out.len() {
                    let _ = write!(c, " %llu");
                }
                let _ = write!(c, "\\n\"");
                for j in 0..b.live_out.len() {
                    let _ = write!(c, ",(unsigned long long)out[{j}]");
                }
                let _ = writeln!(c, ");");
            }
        }
        let _ = writeln!(c, "return 0;}}");
        let asm_path = self.dir.join("exec.S");
        let c_path = self.dir.join("exec.c");
        let exe = self.dir.join("exec");
        std::fs::write(&asm_path, asm).map_err(|e| e.to_string())?;
        std::fs::write(&c_path, c).map_err(|e| e.to_string())?;
        let out = Command::new(&self.cc)
            .args(["-O0", "-o"])
            .arg(&exe)
            .arg(&c_path)
            .arg(&asm_path)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "compiling the executor failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let run = Command::new(&exe).output().map_err(|e| e.to_string())?;
        if !run.status.success() {
            return Err(format!(
                "executor failed: {}",
                String::from_utf8_lossy(&run.stderr)
            ));
        }
        let mut result: Vec<Vec<Vec<u64>>> = vec![Vec::new(); blocks.len()];
        for line in String::from_utf8_lossy(&run.stdout).lines() {
            let mut it = line.split_whitespace();
            let i: usize = it
                .next()
                .and_then(|x| x.parse().ok())
                .ok_or("bad executor output")?;
            result[i].push(
                it.map(|x| x.parse().map_err(|_e| "bad executor output".to_string()))
                    .collect::<Result<_, _>>()?,
            );
        }
        Ok(result)
    }
}

fn emit_kernel(
    out: &mut String,
    name: &str,
    block: &Block<X86_64>,
    mode: Mode,
) -> Result<(), String> {
    let mut used: Vec<Reg> = block.live_in.clone();
    used.extend(block.written());
    used.extend(block.live_out.iter().copied());
    used.sort();
    used.dedup();
    if used.contains(&Reg::Rsp) {
        return Err("blocks that use rsp cannot be measured".into());
    }
    let free: Vec<Reg> = ALL_REGS
        .iter()
        .copied()
        .filter(|r| *r != Reg::Rsp && *r != Reg::Rdi && !used.contains(r))
        .collect();
    // rdi carries the iteration count in; it is moved to a free register.
    let counter = *free
        .first()
        .ok_or("block uses every register; no loop counter left")?;
    let copies: Vec<Vec<<X86_64 as Isa>::Inst>> = match mode {
        Mode::Latency => {
            let (a, b) = (block.live_in[0], block.live_out[0]);
            (0..UNROLL)
                .map(|k| {
                    let swap = move |r: Reg| {
                        if k % 2 == 1 && a != b {
                            if r == a {
                                b
                            } else if r == b {
                                a
                            } else {
                                r
                            }
                        } else {
                            r
                        }
                    };
                    block
                        .insts
                        .iter()
                        .map(|i| X86_64::rename(i, &swap))
                        .collect()
                })
                .collect()
        }
        Mode::Throughput => {
            // Disjoint register sets: copy j maps used[i] to pool[j*n + i].
            let pool: Vec<Reg> = ALL_REGS
                .iter()
                .copied()
                .filter(|r| *r != Reg::Rsp && *r != counter && *r != Reg::Rdi)
                .collect();
            let n = used.len();
            let sets = (pool.len() / n).clamp(1, 8);
            (0..UNROLL)
                .map(|k| {
                    let j = k % sets;
                    let map = |r: Reg| {
                        let i = used.iter().position(|u| *u == r).expect("used register");
                        pool[j * n + i]
                    };
                    block
                        .insts
                        .iter()
                        .map(|i| X86_64::rename(i, &map))
                        .collect()
                })
                .collect()
        }
    };
    let saved = [Reg::Rbx, Reg::Rbp, Reg::R12, Reg::R13, Reg::R14, Reg::R15];
    let _ = writeln!(out, ".globl {name}\n.p2align 5\n{name}:");
    for r in saved {
        let _ = writeln!(out, "  push {r}");
    }
    let _ = writeln!(out, "  mov {counter}, rdi");
    for (i, r) in ALL_REGS.iter().enumerate() {
        if *r != Reg::Rsp && *r != counter {
            let _ = writeln!(
                out,
                "  movabs {r}, {}",
                0x9E37_79B9_7F4A_7C15u64.wrapping_mul(i as u64 + 1) as i64
            );
        }
    }
    let _ = writeln!(out, ".p2align 5\n{name}_loop:");
    for copy in copies {
        for inst in copy {
            let _ = writeln!(out, "  {inst}");
        }
    }
    let _ = writeln!(out, "  dec {counter}\n  jnz {name}_loop");
    for r in saved.iter().rev() {
        let _ = writeln!(out, "  pop {r}");
    }
    let _ = writeln!(out, "  ret");
    Ok(())
}

fn driver_source(kernels: usize, iterations: u64, reps: u32) -> String {
    let mut s = String::from(
        "#define _GNU_SOURCE\n#include <sched.h>\n#include <stdio.h>\n#include <stdint.h>\n#include <time.h>\n\
         static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e9+t.tv_nsec;}\n\
         void codira_ref(uint64_t);\n",
    );
    for i in 0..kernels {
        let _ = writeln!(s, "void codira_k{i}(uint64_t);");
    }
    let _ = writeln!(s, "static void (*ks[])(uint64_t) = {{codira_ref");
    for i in 0..kernels {
        let _ = write!(s, ",codira_k{i}");
    }
    let _ = writeln!(
        s,
        "}};\nint main(void){{cpu_set_t c;CPU_ZERO(&c);CPU_SET(0,&c);sched_setaffinity(0,sizeof c,&c);\n\
         const uint64_t n={iterations}ULL;int nk={};\n\
         for(int k=0;k<nk;k++) ks[k](n/10);\n\
         for(int r=0;r<{reps};r++) for(int k=0;k<nk;k++){{double t0=now();ks[k](n);double t1=now();\
         printf(\"%d %.6f\\n\",k,(t1-t0)/(double)(n*{UNROLL}));}}\nreturn 0;}}",
        kernels + 1
    );
    s
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn parse_output(text: &str, kernels: usize, reps: u32) -> Result<Vec<Measurement>, String> {
    let mut samples: Vec<Vec<f64>> = vec![Vec::new(); kernels + 1];
    for line in text.lines() {
        let (k, ns) = line.split_once(' ').ok_or("bad harness output")?;
        let k: usize = k.parse().map_err(|_e| "bad harness output")?;
        samples[k].push(ns.parse().map_err(|_e| "bad harness output")?);
    }
    let ns_per_cycle = median(&mut samples[0].clone());
    Ok(samples[1..]
        .iter()
        .map(|s| {
            let mut s = s.clone();
            let ns = median(&mut s);
            Measurement {
                cycles: ns / ns_per_cycle,
                cycles_min: s[0] / ns_per_cycle,
                cycles_max: s[s.len() - 1] / ns_per_cycle,
                ns,
                ns_per_cycle,
                repetitions: reps,
            }
        })
        .collect())
}
