//! Measures latency and reciprocal throughput of every modelled x86-64
//! form on the host and prints a cost profile (see `profile.rs` for the
//! format). Port sets cannot be measured without hardware counters; they
//! are taken from the table below and the profile says so.
//!
//! Usage: cargo run --release -p `codira_mcode` --example calibrate -- <profile-id> [ports-source]

use codira_mcode::measure::{Harness, Mode};
use codira_mcode::profile::{FormCost, Profile};
use codira_mcode::x86::{Reg, X86_64};
use codira_mcode::{Block, Isa};

/// (form, representative instruction, live-ins, has a data input, uops, ports).
type Row = (
    &'static str,
    &'static str,
    &'static [Reg],
    bool,
    u32,
    &'static [u8],
);

// Port numbers follow Intel's Golden Cove numbering (integer ALUs on
// ports 0, 1, 5, 6 and 10). Source: see PORTS_SOURCE.
const ROWS: &[Row] = &[
    (
        "mov r64, r64",
        "mov rax, rcx",
        &[Reg::Rcx],
        true,
        1,
        &[0, 1, 5, 6, 10],
    ),
    (
        "mov r64, imm32",
        "mov rax, 7",
        &[Reg::Rcx],
        false,
        1,
        &[0, 1, 5, 6, 10],
    ),
    (
        "movabs r64, imm64",
        "movabs rax, 81985529216486895",
        &[Reg::Rcx],
        false,
        1,
        &[0, 1, 5, 6, 10],
    ),
    (
        "alu r64, r64",
        "add rax, rcx",
        &[Reg::Rax, Reg::Rcx],
        true,
        1,
        &[0, 1, 5, 6, 10],
    ),
    (
        "add/sub r64, imm",
        "add rax, 7",
        &[Reg::Rax],
        true,
        1,
        &[0, 1, 5, 6, 10],
    ),
    (
        "logic r64, imm",
        "xor rax, 7",
        &[Reg::Rax],
        true,
        1,
        &[0, 1, 5, 6, 10],
    ),
    (
        "imul r64, r64",
        "imul rax, rcx",
        &[Reg::Rax, Reg::Rcx],
        true,
        1,
        &[1],
    ),
    (
        "imul r64, r64, imm",
        "imul rax, rax, 7",
        &[Reg::Rax],
        true,
        1,
        &[1],
    ),
    (
        "shift r64, imm",
        "shl rax, 3",
        &[Reg::Rax],
        true,
        1,
        &[0, 6],
    ),
    (
        "neg/not r64",
        "neg rax",
        &[Reg::Rax],
        true,
        1,
        &[0, 1, 5, 6, 10],
    ),
    (
        "lea b+i or b+d",
        "lea rax, [rax + rcx]",
        &[Reg::Rax, Reg::Rcx],
        true,
        1,
        &[0, 1, 5, 6, 10],
    ),
    (
        "lea b+i*s",
        "lea rax, [rax + 4*rcx]",
        &[Reg::Rax, Reg::Rcx],
        true,
        1,
        &[0, 5, 6],
    ),
    (
        "lea b+i*s+d",
        "lea rax, [rax + 4*rcx + 8]",
        &[Reg::Rax, Reg::Rcx],
        true,
        1,
        &[0, 5, 6],
    ),
    (
        "andn r64, r64, r64",
        "andn rax, rax, rcx",
        &[Reg::Rax, Reg::Rcx],
        true,
        1,
        &[1, 5, 10],
    ),
];

const PORTS_SOURCE: &str = "port sets: documented, uops.info instructions.xml (2026-03-29), ADL-P and EMR columns (identical for these forms), not measured";

fn main() {
    let mut args = std::env::args().skip(1);
    let id = args.next().unwrap_or_else(|| "x86_64/host@1".into());
    let ports_source = args.next().unwrap_or_else(|| PORTS_SOURCE.into());
    let dir = std::env::temp_dir().join(format!("codira_calibrate_{}", std::process::id()));
    let h = Harness::detect(&dir).expect("measurement harness");
    let blocks: Vec<Block<X86_64>> = ROWS
        .iter()
        .map(|(_, t, ins, _, _, _)| Block::parse(t, ins.to_vec(), vec![Reg::Rax]).unwrap())
        .collect();
    let lat = h.measure(&blocks, Mode::Latency).expect("latency run");
    let tp = h
        .measure(&blocks, Mode::Throughput)
        .expect("throughput run");
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
        })
        .unwrap_or_default();
    // The measurements run x86-64 code, so only an x86-64 host gets this
    // far; the example still has to compile on the others.
    #[allow(unused_mut)]
    let mut features: Vec<String> = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("bmi1") {
            features.push("bmi1".into());
        }
        if std::is_x86_feature_detected!("bmi2") {
            features.push("bmi2".into());
        }
    }
    let mut p = Profile {
        id,
        isa: X86_64::NAME.into(),
        issue_width: 6,
        features,
        source: format!(
            "latency and reciprocal throughput measured by codira_mcode::measure on `{cpu}` ({} cpus), \
             median of {} runs, ns per cycle {:.4} from a dependent add chain; issue width 6 documented \
             (Golden Cove rename width); {ports_source}",
            std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
            h.repetitions,
            lat[0].ns_per_cycle
        ),
        forms: std::collections::BTreeMap::default(),
    };
    for (((form, _, _, has_input, uops, ports), l), t) in ROWS.iter().zip(&lat).zip(&tp) {
        // Round to the nearest 0.05 cycle: finer digits are noise here.
        let r = |x: f64| (x * 20.0).round() / 20.0;
        let latency = if *has_input { r(l.cycles) } else { 1.0 };
        eprintln!(
            "{form:22} latency {:6.3} [{:.3}, {:.3}]  rthroughput {:6.3} [{:.3}, {:.3}]",
            l.cycles, l.cycles_min, l.cycles_max, t.cycles, t.cycles_min, t.cycles_max
        );
        p.forms.insert(
            form.to_string(),
            FormCost {
                latency,
                rthroughput: r(t.cycles),
                uops: *uops,
                ports: ports.to_vec(),
            },
        );
    }
    print!("{p}");
    let _ = std::fs::remove_dir_all(&dir);
}
