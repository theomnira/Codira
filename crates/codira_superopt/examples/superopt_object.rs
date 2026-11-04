//! End-to-end: superoptimise the straight-line integer tails of the leaf
//! functions in an x86-64 object file.
//!
//! Usage: `superopt_object` <object> <profile> [latency|throughput] [--measure]
//!
//! `llvm-objdump -d -M intel` (from `LLVM_OBJDUMP`, `llvm-objdump-22`, the
//! LLVM 22 installation or `llvm-objdump`) disassembles the object. For
//! every `ret`, the longest run of modelled instructions immediately before
//! it is taken, cut after the last branch target inside it (a block entered
//! in the middle cannot be rewritten as a whole). Under the System V ABI
//! only `rax` is live at `ret` (callee-saved registers must hold their entry
//! values, so a run that writes one is refused) and the flags are dead. The
//! run's live-ins are the registers it reads before writing.

use std::collections::HashSet;
use std::process::Command;

use codira_mcode::measure::Mode;
use codira_mcode::profile::{Objective, Profile};
use codira_mcode::x86::{Reg, X86_64};
use codira_mcode::{Block, Isa};
use codira_superopt::{select_by_measurement, superoptimize, Config, ConstantTime, Origin};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: superopt_object <object> <profile> [latency|throughput] [--measure] [--no-constant-time-code]");
        std::process::exit(2);
    }
    let (obj, profile) = (&args[0], &args[1]);
    let objective = if args.iter().any(|a| a == "throughput") {
        Objective::Throughput
    } else {
        Objective::Latency
    };
    let measure = args.iter().any(|a| a == "--measure");
    let constant_time = if args.iter().any(|a| a == "--no-constant-time-code") {
        ConstantTime::NotRequired
    } else {
        ConstantTime::Unknown
    };
    let profile = Profile::parse(&std::fs::read_to_string(profile).expect("read profile"))
        .expect("parse profile");
    let objdump = std::env::var("LLVM_OBJDUMP").ok().unwrap_or_else(|| {
        [
            "llvm-objdump-22",
            "/usr/lib/llvm-22/bin/llvm-objdump",
            "llvm-objdump",
        ]
        .into_iter()
        .find(|c| Command::new(c).arg("--version").output().is_ok())
        .expect("llvm-objdump")
        .to_string()
    });
    let out = Command::new(&objdump)
        .args(["-d", "-M", "intel", "--no-show-raw-insn"])
        .arg(obj)
        .output()
        .expect("run llvm-objdump");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut funcs: Vec<(String, Vec<(u64, String)>)> = Vec::new();
    for line in text.lines() {
        if let Some(name) = line
            .strip_suffix(">:")
            .and_then(|l| l.split_once(" <"))
            .map(|(_, n)| n)
        {
            funcs.push((name.to_string(), Vec::new()));
        } else if let (Some(f), Some((addr, inst))) =
            (funcs.last_mut(), line.trim().split_once(':'))
        {
            if let Ok(a) = u64::from_str_radix(addr.trim(), 16) {
                let inst = inst
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                f.1.push((a, inst));
            }
        }
    }
    let callee_saved = [
        Reg::Rbx,
        Reg::Rbp,
        Reg::R12,
        Reg::R13,
        Reg::R14,
        Reg::R15,
        Reg::Rsp,
    ];
    let (mut runs, mut improved) = (0, 0);
    for (name, insts) in &funcs {
        let targets: HashSet<u64> = insts
            .iter()
            .filter(|(_, t)| t.starts_with('j'))
            .filter_map(|(_, t)| {
                t.split_whitespace()
                    .nth(1)
                    .and_then(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok())
            })
            .collect();
        for (i, (_, t)) in insts.iter().enumerate() {
            if t != "ret" {
                continue;
            }
            let mut start = i;
            while start > 0 && X86_64::parse(&insts[start - 1].1).is_ok() {
                start -= 1;
            }
            for k in (start + 1..i).rev() {
                if targets.contains(&insts[k].0) {
                    start = k;
                    break;
                }
            }
            if start == i {
                continue;
            }
            let body: Vec<_> = insts[start..i]
                .iter()
                .map(|(_, t)| X86_64::parse(t).unwrap())
                .collect();
            let mut live_in: Vec<Reg> = Vec::new();
            let mut written: Vec<Reg> = Vec::new();
            for inst in &body {
                for r in X86_64::reads(inst) {
                    if !written.contains(&r) && !live_in.contains(&r) {
                        live_in.push(r);
                    }
                }
                written.push(X86_64::writes(inst));
            }
            let block = Block::<X86_64>::new(body, live_in, vec![Reg::Rax]);
            println!("== {name}: {} instructions before ret", block.insts.len());
            if block.written().iter().any(|r| callee_saved.contains(r)) {
                println!("   refused: writes a callee-saved register\n");
                continue;
            }
            if !block.written().contains(&Reg::Rax) {
                println!("   skipped: does not compute rax\n");
                continue;
            }
            runs += 1;
            let config = Config {
                objective,
                ..Config::default()
            };
            match superoptimize(
                &Origin::new(name.clone(), constant_time),
                &block,
                &profile,
                &config,
            ) {
                Ok(d) => {
                    println!("{d}");
                    if d.improvement().is_some() {
                        improved += 1;
                        if measure {
                            let mode = match objective {
                                Objective::Latency => Mode::Latency,
                                Objective::Throughput => Mode::Throughput,
                            };
                            let dir = std::env::temp_dir().join("codira_superopt_measure");
                            match select_by_measurement(&d, 1, mode, &dir) {
                                Ok(ms) => {
                                    for (who, m) in ms {
                                        println!(
                                            "measured  {:9} {:.3} cycles, min {:.3}, max {:.3} ({:?} mode, median of {})",
                                            who.map_or("original".to_string(), |i| format!("cand {i}")),
                                            m.cycles,
                                            m.cycles_min,
                                            m.cycles_max,
                                            mode,
                                            m.repetitions
                                        );
                                    }
                                }
                                Err(e) => println!("   not measured: {e}"),
                            }
                        }
                    }
                    println!();
                }
                Err(e) => println!("   refused: {e}\n"),
            }
        }
    }
    println!(
        "summary: {runs} tails searched, {improved} improved under {}",
        profile.id
    );
}
