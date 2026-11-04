//! Superoptimising the functions of a linked x86-64 ELF library in place
//! (`codira build --superoptimize`, experimental).
//!
//! For every function symbol of the library whose origin the caller vouches
//! for ([`ConstantTime::NotRequired`]):
//!
//! 1. The function is decoded. A function with an indirect jump is skipped
//!    whole: its branch targets are not all known.
//! 2. For every `ret`, the longest run of modelled instructions right before
//!    it is taken, cut after the last branch target inside it (from anywhere
//!    in the library), so the run is only ever entered at its start.
//! 3. Under the System V ABI only `rax` is live at `ret`, the flags are dead,
//!    and callee-saved registers must hold their entry values: a run that
//!    writes one (or `rsp`) is skipped. The run is superoptimised
//!    ([`crate::superoptimize`]), which proves the lowered replacement equal
//!    to the original with Z3.
//! 4. The replacement is encoded at the run's address and must not be longer
//!    than the run; the rest of the run is filled with one multi-byte `nop`
//!    (two when more than nine bytes are left), so `ret` stays where it was.
//! 5. Independent check of what was written: the patched bytes are decoded
//!    again and proven equal to the original run. A mismatch keeps the
//!    original bytes.
//!
//! Untouched: relocations (a run with a dynamic relocation in it is
//! skipped), unwind tables (no run changes `rsp` or `rbp` or any
//! callee-saved register), function sizes and addresses. Line tables still
//! map the run's addresses to the same source lines, now for fewer
//! instructions.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use codira_mcode::profile::{Cost, Profile};
use codira_mcode::x86::{AluOp, Inst, Reg, ShiftOp, X86_64};
use codira_mcode::{prove_equivalent, Block, Isa};
use iced_x86::{
    Code, Decoder, DecoderOptions, Encoder, FlowControl, Instruction, MemoryOperand, OpKind,
    Register,
};
use object::{Object, ObjectSection, ObjectSymbol};

use crate::{superoptimize_cached, Config, ConstantTime, Origin};

/// One rewritten run.
#[derive(Clone, Debug)]
pub struct Rewrite {
    pub function: String,
    pub address: u64,
    pub original: Block<X86_64>,
    pub replacement: Block<X86_64>,
    pub original_bytes: usize,
    pub replacement_bytes: usize,
    pub cost_before: Cost,
    pub cost_after: Cost,
}

/// What a pass over a library did.
#[derive(Clone, Debug, Default)]
pub struct LibraryReport {
    pub functions_seen: usize,
    /// Functions refused because they must run in constant time.
    pub constant_time_refused: usize,
    /// Function symbols nobody vouched for (runtime glue, foreign code).
    pub unknown_skipped: usize,
    /// Functions skipped for an indirect jump.
    pub indirect_skipped: usize,
    /// Runs handed to the search.
    pub runs_searched: usize,
    pub rewrites: Vec<Rewrite>,
    /// Why some runs were not rewritten although a cheaper sequence exists.
    pub notes: Vec<String>,
}

fn reg(r: Register) -> Option<Reg> {
    Reg::from_name(&format!("{r:?}").to_lowercase())
}

fn to_iced_reg(r: Reg) -> Register {
    use Register as R;
    match r {
        Reg::Rax => R::RAX,
        Reg::Rcx => R::RCX,
        Reg::Rdx => R::RDX,
        Reg::Rbx => R::RBX,
        Reg::Rsp => R::RSP,
        Reg::Rbp => R::RBP,
        Reg::Rsi => R::RSI,
        Reg::Rdi => R::RDI,
        Reg::R8 => R::R8,
        Reg::R9 => R::R9,
        Reg::R10 => R::R10,
        Reg::R11 => R::R11,
        Reg::R12 => R::R12,
        Reg::R13 => R::R13,
        Reg::R14 => R::R14,
        Reg::R15 => R::R15,
    }
}

fn is_nop(i: &Instruction) -> bool {
    matches!(
        i.code(),
        Code::Nopw | Code::Nopd | Code::Nopq | Code::Nop_rm16 | Code::Nop_rm32 | Code::Nop_rm64
    )
}

/// The modelled instruction a decoded one is, if it is one.
pub fn from_iced(i: &Instruction) -> Option<Inst> {
    use Code as C;
    let r = |n: u32| -> Option<Reg> {
        (i.op_kind(n) == OpKind::Register)
            .then(|| reg(i.op_register(n)))
            .flatten()
    };
    let imm = |n: u32| i.immediate(n) as i64;
    let alu = |code: Code| -> Option<AluOp> {
        use Code as C;
        Some(match code {
            C::Add_rm64_r64
            | C::Add_r64_rm64
            | C::Add_rm64_imm8
            | C::Add_rm64_imm32
            | C::Add_RAX_imm32 => AluOp::Add,
            C::Sub_rm64_r64
            | C::Sub_r64_rm64
            | C::Sub_rm64_imm8
            | C::Sub_rm64_imm32
            | C::Sub_RAX_imm32 => AluOp::Sub,
            C::And_rm64_r64
            | C::And_r64_rm64
            | C::And_rm64_imm8
            | C::And_rm64_imm32
            | C::And_RAX_imm32 => AluOp::And,
            C::Or_rm64_r64
            | C::Or_r64_rm64
            | C::Or_rm64_imm8
            | C::Or_rm64_imm32
            | C::Or_RAX_imm32 => AluOp::Or,
            C::Xor_rm64_r64
            | C::Xor_r64_rm64
            | C::Xor_rm64_imm8
            | C::Xor_rm64_imm32
            | C::Xor_RAX_imm32 => AluOp::Xor,
            _ => return None,
        })
    };
    let inst = match i.code() {
        C::Mov_rm64_r64 | C::Mov_r64_rm64 => Inst::Mov {
            dst: r(0)?,
            src: r(1)?,
        },
        C::Mov_r64_imm64 | C::Mov_rm64_imm32 => Inst::MovImm {
            dst: r(0)?,
            imm: imm(1),
        },
        C::Imul_r64_rm64 => Inst::Alu {
            op: AluOp::Imul,
            dst: r(0)?,
            src: r(1)?,
        },
        C::Imul_r64_rm64_imm8 | C::Imul_r64_rm64_imm32 => Inst::Imul3 {
            dst: r(0)?,
            src: r(1)?,
            imm: i32::try_from(imm(2)).ok()?,
        },
        C::Shl_rm64_imm8
        | C::Sal_rm64_imm8
        | C::Shr_rm64_imm8
        | C::Sar_rm64_imm8
        | C::Shl_rm64_1
        | C::Sal_rm64_1
        | C::Shr_rm64_1
        | C::Sar_rm64_1 => {
            let op = match i.code() {
                C::Shl_rm64_imm8 | C::Sal_rm64_imm8 | C::Shl_rm64_1 | C::Sal_rm64_1 => ShiftOp::Shl,
                C::Shr_rm64_imm8 | C::Shr_rm64_1 => ShiftOp::Shr,
                _ => ShiftOp::Sar,
            };
            let amount = if i.op_count() > 1 && i.op_kind(1) == OpKind::Immediate8 {
                i.immediate8()
            } else {
                1
            };
            // The processor masks the count to 6 bits; a count of 0 leaves
            // the flags untouched, which the model does not describe.
            let amount = amount & 63;
            if amount == 0 {
                return None;
            }
            Inst::Shift {
                op,
                dst: r(0)?,
                amount,
            }
        }
        C::Neg_rm64 => Inst::Neg { dst: r(0)? },
        C::Not_rm64 => Inst::Not { dst: r(0)? },
        C::Lea_r64_m => {
            if i.op_kind(1) != OpKind::Memory
                || i.is_ip_rel_memory_operand()
                || i.segment_prefix() != Register::None
            {
                return None;
            }
            let base = match i.memory_base() {
                Register::None => None,
                b => Some(reg(b)?),
            };
            let index = match i.memory_index() {
                Register::None => None,
                x => Some((reg(x)?, u8::try_from(i.memory_index_scale()).ok()?)),
            };
            if base.is_none() && index.is_none() {
                return None;
            }
            let disp = i32::try_from(i.memory_displacement64() as i64).ok()?;
            Inst::Lea {
                dst: r(0)?,
                base,
                index,
                disp,
            }
        }
        C::VEX_Andn_r64_r64_rm64 => Inst::Andn {
            dst: r(0)?,
            a: r(1)?,
            b: r(2)?,
        },
        code => {
            let op = alu(code)?;
            if i.op_kind(1) == OpKind::Register {
                Inst::Alu {
                    op,
                    dst: r(0)?,
                    src: r(1)?,
                }
            } else {
                Inst::AluImm {
                    op,
                    dst: r(0)?,
                    imm: i32::try_from(imm(1)).ok()?,
                }
            }
        }
    };
    // Only what the model's parser accepts (for example, no `rsp` as an
    // index) is accepted here.
    X86_64::parse(&inst.to_string()).ok().filter(|p| *p == inst)
}

/// Encodes modelled instructions at `rip`.
pub fn encode(insts: &[Inst], rip: u64) -> Result<Vec<u8>, String> {
    let mut encoder = Encoder::new(64);
    let mut ip = rip;
    for inst in insts {
        let e =
            |x: Result<Instruction, iced_x86::IcedError>| x.map_err(|e| format!("`{inst}`: {e}"));
        let r = to_iced_reg;
        let fits8 = |v: i64| i8::try_from(v).is_ok();
        let instruction = match inst {
            Inst::Mov { dst, src } => e(Instruction::with2(Code::Mov_rm64_r64, r(*dst), r(*src)))?,
            Inst::MovImm { dst, imm } => match i32::try_from(*imm) {
                Ok(v) => e(Instruction::with2(Code::Mov_rm64_imm32, r(*dst), v))?,
                Err(_) => e(Instruction::with2(
                    Code::Mov_r64_imm64,
                    r(*dst),
                    *imm as u64,
                ))?,
            },
            Inst::Alu { op, dst, src } => {
                let code = match op {
                    AluOp::Add => Code::Add_rm64_r64,
                    AluOp::Sub => Code::Sub_rm64_r64,
                    AluOp::And => Code::And_rm64_r64,
                    AluOp::Or => Code::Or_rm64_r64,
                    AluOp::Xor => Code::Xor_rm64_r64,
                    AluOp::Imul => Code::Imul_r64_rm64,
                };
                e(Instruction::with2(code, r(*dst), r(*src)))?
            }
            Inst::AluImm { op, dst, imm } => {
                let small = fits8(i64::from(*imm));
                let code = match (op, small) {
                    (AluOp::Add, true) => Code::Add_rm64_imm8,
                    (AluOp::Add, false) => Code::Add_rm64_imm32,
                    (AluOp::Sub, true) => Code::Sub_rm64_imm8,
                    (AluOp::Sub, false) => Code::Sub_rm64_imm32,
                    (AluOp::And, true) => Code::And_rm64_imm8,
                    (AluOp::And, false) => Code::And_rm64_imm32,
                    (AluOp::Or, true) => Code::Or_rm64_imm8,
                    (AluOp::Or, false) => Code::Or_rm64_imm32,
                    (AluOp::Xor, true) => Code::Xor_rm64_imm8,
                    (AluOp::Xor, false) => Code::Xor_rm64_imm32,
                    (AluOp::Imul, _) => return Err("imul with an immediate is Imul3".into()),
                };
                e(Instruction::with2(code, r(*dst), *imm))?
            }
            Inst::Imul3 { dst, src, imm } => {
                let code = if fits8(i64::from(*imm)) {
                    Code::Imul_r64_rm64_imm8
                } else {
                    Code::Imul_r64_rm64_imm32
                };
                e(Instruction::with3(code, r(*dst), r(*src), *imm))?
            }
            Inst::Shift { op, dst, amount } => {
                let code = match op {
                    ShiftOp::Shl => Code::Shl_rm64_imm8,
                    ShiftOp::Shr => Code::Shr_rm64_imm8,
                    ShiftOp::Sar => Code::Sar_rm64_imm8,
                };
                e(Instruction::with2(code, r(*dst), i32::from(*amount)))?
            }
            Inst::Neg { dst } => e(Instruction::with1(Code::Neg_rm64, r(*dst)))?,
            Inst::Not { dst } => e(Instruction::with1(Code::Not_rm64, r(*dst)))?,
            Inst::Lea {
                dst,
                base,
                index,
                disp,
            } => {
                let displ_size = u32::from(*disp != 0 || base.is_none());
                let mem = MemoryOperand::with_base_index_scale_displ_size(
                    base.map_or(Register::None, r),
                    index.map_or(Register::None, |(x, _)| r(x)),
                    index.map_or(1, |(_, s)| u32::from(s)),
                    i64::from(*disp),
                    displ_size,
                );
                e(Instruction::with2(Code::Lea_r64_m, r(*dst), mem))?
            }
            Inst::Andn { dst, a, b } => e(Instruction::with3(
                Code::VEX_Andn_r64_r64_rm64,
                r(*dst),
                r(*a),
                r(*b),
            ))?,
        };
        let len = encoder
            .encode(&instruction, ip)
            .map_err(|e| format!("encoding `{inst}`: {e}"))?;
        ip += len as u64;
    }
    Ok(encoder.take_buffer())
}

/// Intel's recommended multi-byte `nop`s, 1 to 9 bytes.
fn nops(mut n: usize) -> Vec<u8> {
    const TABLE: [&[u8]; 9] = [
        &[0x90],
        &[0x66, 0x90],
        &[0x0f, 0x1f, 0x00],
        &[0x0f, 0x1f, 0x40, 0x00],
        &[0x0f, 0x1f, 0x44, 0x00, 0x00],
        &[0x66, 0x0f, 0x1f, 0x44, 0x00, 0x00],
        &[0x0f, 0x1f, 0x80, 0x00, 0x00, 0x00, 0x00],
        &[0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
        &[0x66, 0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
    ];
    let mut out = Vec::new();
    while n > 0 {
        let k = n.min(9);
        out.extend_from_slice(TABLE[k - 1]);
        n -= k;
    }
    out
}

#[cfg(test)]
pub(crate) fn nops_for_test(n: usize) -> Vec<u8> {
    nops(n)
}

#[cfg(test)]
pub(crate) fn is_nop_for_test(i: &Instruction) -> bool {
    is_nop(i)
}

/// A run before a `ret`: its instructions with their addresses and lengths
/// (nops included, so a pass over already patched code is idempotent).
struct Run {
    start: u64,
    end: u64,
    insts: Vec<Inst>,
}

const CALLEE_SAVED: [Reg; 7] = [
    Reg::Rbx,
    Reg::Rbp,
    Reg::R12,
    Reg::R13,
    Reg::R14,
    Reg::R15,
    Reg::Rsp,
];

fn block_of(insts: &[Inst]) -> Block<X86_64> {
    let mut live_in: Vec<Reg> = Vec::new();
    let mut written: Vec<Reg> = Vec::new();
    for inst in insts {
        for r in X86_64::reads(inst) {
            if !written.contains(&r) && !live_in.contains(&r) {
                live_in.push(r);
            }
        }
        written.push(X86_64::writes(inst));
    }
    Block::new(insts.to_vec(), live_in, vec![Reg::Rax])
}

fn decode_span(bytes: &[u8], address: u64) -> Option<Vec<Inst>> {
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut out = Vec::new();
    while decoder.can_decode() {
        let i = decoder.decode();
        if i.is_invalid() {
            return None;
        }
        if is_nop(&i) {
            continue;
        }
        out.push(from_iced(&i)?);
    }
    Some(out)
}

/// Superoptimises the library in `bytes` (a linked x86-64 ELF shared object)
/// in place. `functions` maps function symbols to whether their code must
/// run in constant time; symbols not in it are left alone.
pub fn optimize_elf(
    bytes: &mut [u8],
    functions: &HashMap<String, ConstantTime>,
    profile: &Profile,
    config: &Config,
    cache: &Path,
) -> Result<LibraryReport, String> {
    let file = object::File::parse(&*bytes).map_err(|e| format!("not an object file: {e}"))?;
    if file.architecture() != object::Architecture::X86_64
        || file.format() != object::BinaryFormat::Elf
    {
        return Err("the superoptimiser rewrites x86-64 ELF libraries only".into());
    }
    let text = file
        .section_by_name(".text")
        .ok_or("the library has no .text section")?;
    let (text_offset, text_size) = text.file_range().ok_or(".text has no file contents")?;
    let text_address = text.address();
    let text_bytes = bytes[text_offset as usize..(text_offset + text_size) as usize].to_vec();

    // Addresses that dynamic relocations write: never inside a rewritten run.
    let relocated: BTreeSet<u64> = file
        .dynamic_relocations()
        .map(|it| it.map(|(offset, _)| offset).collect())
        .unwrap_or_default();

    // Every function symbol in .text, and every direct branch target.
    let mut symbols: Vec<(String, u64, u64)> = file
        .symbols()
        .filter(|s| {
            s.kind() == object::SymbolKind::Text
                && s.size() > 0
                && s.section_index() == Some(text.index())
        })
        .filter_map(|s| Some((s.name().ok()?.to_string(), s.address(), s.size())))
        .collect();
    symbols.sort_by_key(|s| s.1);
    symbols.dedup_by_key(|s| s.1);
    let slice = |address: u64, size: u64| -> Option<&[u8]> {
        let start = address.checked_sub(text_address)? as usize;
        text_bytes.get(start..start + size as usize)
    };
    let mut targets: BTreeSet<u64> = BTreeSet::new();
    let mut decoded: HashMap<u64, (Vec<Instruction>, bool)> = HashMap::new();
    for (_, address, size) in &symbols {
        let Some(code) = slice(*address, *size) else {
            continue;
        };
        let mut decoder = Decoder::with_ip(64, code, *address, DecoderOptions::NONE);
        let mut insts = Vec::new();
        let mut indirect = false;
        while decoder.can_decode() {
            let i = decoder.decode();
            match i.flow_control() {
                FlowControl::ConditionalBranch
                | FlowControl::UnconditionalBranch
                | FlowControl::Call => {
                    targets.insert(i.near_branch_target());
                }
                FlowControl::IndirectBranch => indirect = true,
                _ => {}
            }
            insts.push(i);
        }
        decoded.insert(*address, (insts, indirect));
    }

    let mut report = LibraryReport::default();
    let mut patches: Vec<(u64, Vec<u8>)> = Vec::new();
    for (name, address, _) in &symbols {
        report.functions_seen += 1;
        let constant_time = functions.get(name).copied();
        match constant_time {
            None => {
                report.unknown_skipped += 1;
                continue;
            }
            Some(ConstantTime::Required) => {
                report.constant_time_refused += 1;
                continue;
            }
            _ => {}
        }
        let (insts, indirect) = &decoded[address];
        if *indirect {
            report.indirect_skipped += 1;
            continue;
        }
        for (k, i) in insts.iter().enumerate() {
            if i.flow_control() != FlowControl::Return {
                continue;
            }
            // Walk back over modelled instructions (and nops).
            let mut start = k;
            while start > 0 && (is_nop(&insts[start - 1]) || from_iced(&insts[start - 1]).is_some())
            {
                start -= 1;
            }
            // Enter only at the start: cut after the last branch target.
            for m in (start + 1..k).rev() {
                if targets.contains(&insts[m].ip()) {
                    start = m;
                    break;
                }
            }
            if start == k {
                continue;
            }
            let run = Run {
                start: insts[start].ip(),
                end: insts[k].ip(),
                insts: insts[start..k]
                    .iter()
                    .filter(|i| !is_nop(i))
                    .filter_map(from_iced)
                    .collect(),
            };
            if run.insts.is_empty() || relocated.range(run.start..run.end).next().is_some() {
                continue;
            }
            let block = block_of(&run.insts);
            if block.written().iter().any(|r| CALLEE_SAVED.contains(r))
                || !block.written().contains(&Reg::Rax)
            {
                continue;
            }
            report.runs_searched += 1;
            let origin = Origin::new(name.clone(), ConstantTime::NotRequired);
            let chosen = match superoptimize_cached(&origin, &block, profile, config, cache) {
                Ok((Some(b), _)) => b,
                Ok((None, _)) => continue,
                Err(e) => {
                    report
                        .notes
                        .push(format!("`{name}` at {:#x}: {e}", run.start));
                    continue;
                }
            };
            let span = (run.end - run.start) as usize;
            let mut code = match encode(&chosen.insts, run.start) {
                Ok(c) => c,
                Err(e) => {
                    report
                        .notes
                        .push(format!("`{name}` at {:#x}: {e}", run.start));
                    continue;
                }
            };
            if code.len() > span {
                report.notes.push(format!(
                    "`{name}` at {:#x}: the replacement ({} bytes) is longer than the original ({span} bytes)",
                    run.start,
                    code.len()
                ));
                continue;
            }
            let replacement_bytes = code.len();
            code.extend(nops(span - code.len()));
            // Independent check of the bytes that will be written.
            let ok = decode_span(&code, run.start)
                .map(|insts| Block::new(insts, block.live_in.clone(), block.live_out.clone()))
                .filter(|b| b.insts == chosen.insts)
                .is_some_and(|b| prove_equivalent(&block, &b).is_proven());
            if !ok {
                report.notes.push(format!(
                    "`{name}` at {:#x}: the encoded replacement did not decode back to a proven-equal sequence; kept the original",
                    run.start
                ));
                continue;
            }
            let (Ok(cost_before), Ok(cost_after)) = (profile.cost(&block), profile.cost(&chosen))
            else {
                continue;
            };
            report.rewrites.push(Rewrite {
                function: name.clone(),
                address: run.start,
                original: block,
                replacement: chosen,
                original_bytes: span,
                replacement_bytes,
                cost_before,
                cost_after,
            });
            patches.push((run.start, code));
        }
    }
    for (address, code) in patches {
        let at = (text_offset + (address - text_address)) as usize;
        bytes[at..at + code.len()].copy_from_slice(&code);
    }
    Ok(report)
}
