//! x86-64: 64-bit general-purpose register instructions, Intel syntax.
//!
//! Semantics follow the Intel SDM, Vol. 2 (instruction reference) for the
//! 64-bit operand size: arithmetic wraps modulo 2^64; shift counts given as
//! immediates are masked to 6 bits (all modelled immediates are in `0..64`);
//! `imul` with a 64-bit destination keeps the low 64 bits of the product,
//! which do not depend on signedness; `lea` computes its address expression
//! modulo 2^64 without touching memory or flags; `andn` (BMI1) computes
//! `!src1 & src2`. Flags: everything except `mov`, `lea` and `not` writes
//! them.

use std::fmt;

use crate::{Domain, Isa};

/// The sixteen 64-bit general-purpose registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Reg {
    Rax,
    Rcx,
    Rdx,
    Rbx,
    Rsp,
    Rbp,
    Rsi,
    Rdi,
    R8,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
}

pub const ALL_REGS: [Reg; 16] = [
    Reg::Rax,
    Reg::Rcx,
    Reg::Rdx,
    Reg::Rbx,
    Reg::Rsp,
    Reg::Rbp,
    Reg::Rsi,
    Reg::Rdi,
    Reg::R8,
    Reg::R9,
    Reg::R10,
    Reg::R11,
    Reg::R12,
    Reg::R13,
    Reg::R14,
    Reg::R15,
];

impl Reg {
    pub fn name(self) -> &'static str {
        match self {
            Reg::Rax => "rax",
            Reg::Rcx => "rcx",
            Reg::Rdx => "rdx",
            Reg::Rbx => "rbx",
            Reg::Rsp => "rsp",
            Reg::Rbp => "rbp",
            Reg::Rsi => "rsi",
            Reg::Rdi => "rdi",
            Reg::R8 => "r8",
            Reg::R9 => "r9",
            Reg::R10 => "r10",
            Reg::R11 => "r11",
            Reg::R12 => "r12",
            Reg::R13 => "r13",
            Reg::R14 => "r14",
            Reg::R15 => "r15",
        }
    }

    pub fn from_name(name: &str) -> Option<Reg> {
        ALL_REGS.iter().copied().find(|r| r.name() == name)
    }
}

impl fmt::Display for Reg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AluOp {
    Add,
    Sub,
    And,
    Or,
    Xor,
    Imul,
}

impl AluOp {
    fn mnemonic(self) -> &'static str {
        match self {
            AluOp::Add => "add",
            AluOp::Sub => "sub",
            AluOp::And => "and",
            AluOp::Or => "or",
            AluOp::Xor => "xor",
            AluOp::Imul => "imul",
        }
    }

    pub fn apply<D: Domain>(self, d: &D, a: &D::V, b: &D::V) -> D::V {
        match self {
            AluOp::Add => d.add(a, b),
            AluOp::Sub => d.sub(a, b),
            AluOp::And => d.and(a, b),
            AluOp::Or => d.or(a, b),
            AluOp::Xor => d.xor(a, b),
            AluOp::Imul => d.mul(a, b),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShiftOp {
    Shl,
    Shr,
    Sar,
}

impl ShiftOp {
    fn mnemonic(self) -> &'static str {
        match self {
            ShiftOp::Shl => "shl",
            ShiftOp::Shr => "shr",
            ShiftOp::Sar => "sar",
        }
    }
}

/// A modelled instruction. Two-address forms (`Alu`, `AluImm`, `Shift`,
/// `Neg`, `Not`) read and write `dst`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Inst {
    Mov {
        dst: Reg,
        src: Reg,
    },
    MovImm {
        dst: Reg,
        imm: i64,
    },
    Alu {
        op: AluOp,
        dst: Reg,
        src: Reg,
    },
    /// `op dst, imm32` (sign-extended); `Imul` is not allowed here, it has
    /// its own three-operand form.
    AluImm {
        op: AluOp,
        dst: Reg,
        imm: i32,
    },
    /// `imul dst, src, imm32`.
    Imul3 {
        dst: Reg,
        src: Reg,
        imm: i32,
    },
    Shift {
        op: ShiftOp,
        dst: Reg,
        amount: u8,
    },
    Neg {
        dst: Reg,
    },
    Not {
        dst: Reg,
    },
    /// `lea dst, [base + scale*index + disp]`; at least one of base and
    /// index; scale is 1, 2, 4 or 8.
    Lea {
        dst: Reg,
        base: Option<Reg>,
        index: Option<(Reg, u8)>,
        disp: i32,
    },
    /// BMI1 `andn dst, a, b` = `!a & b`.
    Andn {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
}

impl Inst {
    /// Number of address components of a `lea` (base, index, displacement).
    fn lea_components(base: Option<Reg>, index: Option<(Reg, u8)>, disp: i32) -> u32 {
        u32::from(base.is_some()) + u32::from(index.is_some()) + u32::from(disp != 0)
    }
}

impl fmt::Display for Inst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Inst::Mov { dst, src } => write!(f, "mov {dst}, {src}"),
            Inst::MovImm { dst, imm } => {
                if i32::try_from(*imm).is_ok() {
                    write!(f, "mov {dst}, {imm}")
                } else {
                    write!(f, "movabs {dst}, {imm}")
                }
            }
            Inst::Alu { op, dst, src } => write!(f, "{} {dst}, {src}", op.mnemonic()),
            Inst::AluImm { op, dst, imm } => write!(f, "{} {dst}, {imm}", op.mnemonic()),
            Inst::Imul3 { dst, src, imm } => write!(f, "imul {dst}, {src}, {imm}"),
            Inst::Shift { op, dst, amount } => write!(f, "{} {dst}, {amount}", op.mnemonic()),
            Inst::Neg { dst } => write!(f, "neg {dst}"),
            Inst::Not { dst } => write!(f, "not {dst}"),
            Inst::Lea {
                dst,
                base,
                index,
                disp,
            } => {
                let mut parts = Vec::new();
                if let Some(b) = base {
                    parts.push(b.to_string());
                }
                if let Some((i, s)) = index {
                    parts.push(if *s == 1 {
                        i.to_string()
                    } else {
                        format!("{s}*{i}")
                    });
                }
                let mut text = parts.join(" + ");
                if *disp != 0 || text.is_empty() {
                    if text.is_empty() {
                        text = disp.to_string();
                    } else if *disp < 0 {
                        text = format!("{text} - {}", -i64::from(*disp));
                    } else {
                        text = format!("{text} + {disp}");
                    }
                }
                write!(f, "lea {dst}, [{text}]")
            }
            Inst::Andn { dst, a, b } => write!(f, "andn {dst}, {a}, {b}"),
        }
    }
}

/// The x86-64 instruction set (64-bit GPR subset).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct X86_64;

fn parse_int(text: &str) -> Result<i64, String> {
    let t = text.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest.trim()),
        None => (false, t),
    };
    let value = if let Some(hex) = t.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|e| format!("bad number `{text}`: {e}"))? as i64
    } else {
        t.parse::<i64>()
            .map_err(|e| format!("bad number `{text}`: {e}"))?
    };
    Ok(if neg { value.wrapping_neg() } else { value })
}

fn reg(text: &str) -> Result<Reg, String> {
    let t = text.trim();
    Reg::from_name(t)
        .ok_or_else(|| format!("unsupported register `{t}` (only 64-bit GPRs are modelled)"))
}

fn imm32(text: &str) -> Result<i32, String> {
    let v = parse_int(text)?;
    // objdump prints negative imm32 operands as their 64-bit sign extension
    // in hex (0xffffffffffffffff); both spellings mean the same immediate.
    i32::try_from(v).map_err(|_e| format!("immediate `{text}` does not fit a sign-extended imm32"))
}

/// `(base, (index, scale), displacement)` of an address operand.
type Address = (Option<Reg>, Option<(Reg, u8)>, i32);

fn parse_address(text: &str) -> Result<Address, String> {
    let t = text.trim();
    let inner = t
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("expected `[...]`, found `{t}`"))?;
    // Split into signed terms.
    let mut terms: Vec<(bool, String)> = Vec::new();
    let mut current = String::new();
    let mut negative = false;
    for ch in inner.chars() {
        if ch == '+' || ch == '-' {
            if !current.trim().is_empty() {
                terms.push((negative, current.trim().to_string()));
            }
            current.clear();
            negative = ch == '-';
        } else {
            current.push(ch);
        }
    }
    if !current.trim().is_empty() {
        terms.push((negative, current.trim().to_string()));
    }
    let (mut base, mut index, mut disp) = (None, None, 0i64);
    for (neg, term) in terms {
        if let Some((a, b)) = term.split_once('*') {
            let (a, b) = (a.trim(), b.trim());
            let (r, s) = match (Reg::from_name(a), Reg::from_name(b)) {
                (Some(r), None) => (r, parse_int(b)?),
                (None, Some(r)) => (r, parse_int(a)?),
                _ => return Err(format!("bad scaled index `{term}`")),
            };
            if neg || ![1, 2, 4, 8].contains(&s) || index.is_some() {
                return Err(format!("bad scaled index `{term}`"));
            }
            index = Some((r, s as u8));
        } else if let Some(r) = Reg::from_name(&term) {
            if neg {
                return Err(format!("negated register in `{t}`"));
            }
            if base.is_none() {
                base = Some(r);
            } else if index.is_none() {
                index = Some((r, 1));
            } else {
                return Err(format!("too many registers in `{t}`"));
            }
        } else if term.starts_with(|c: char| c.is_ascii_digit()) {
            let v = parse_int(&term)?;
            disp += if neg { -v } else { v };
        } else {
            return Err(format!(
                "unsupported address term `{term}` (rip-relative and segments are not modelled)"
            ));
        }
    }
    if base.is_none() && index.is_none() {
        return Err("address without a register".into());
    }
    let disp = i32::try_from(disp).map_err(|_e| "displacement does not fit 32 bits".to_string())?;
    Ok((base, index, disp))
}

impl Isa for X86_64 {
    type Reg = Reg;
    type Inst = Inst;
    const NAME: &'static str = "x86_64";

    fn reads(inst: &Inst) -> Vec<Reg> {
        match inst {
            Inst::Mov { src, .. } => vec![*src],
            Inst::MovImm { .. } => vec![],
            Inst::Alu { dst, src, .. } => vec![*dst, *src],
            Inst::AluImm { dst, .. }
            | Inst::Shift { dst, .. }
            | Inst::Neg { dst }
            | Inst::Not { dst } => {
                vec![*dst]
            }
            Inst::Imul3 { src, .. } => vec![*src],
            Inst::Lea { base, index, .. } => base
                .iter()
                .copied()
                .chain(index.iter().map(|(r, _)| *r))
                .collect(),
            Inst::Andn { a, b, .. } => vec![*a, *b],
        }
    }

    fn writes(inst: &Inst) -> Reg {
        match inst {
            Inst::Mov { dst, .. }
            | Inst::MovImm { dst, .. }
            | Inst::Alu { dst, .. }
            | Inst::AluImm { dst, .. }
            | Inst::Imul3 { dst, .. }
            | Inst::Shift { dst, .. }
            | Inst::Neg { dst }
            | Inst::Not { dst }
            | Inst::Lea { dst, .. }
            | Inst::Andn { dst, .. } => *dst,
        }
    }

    fn clobbers_flags(inst: &Inst) -> bool {
        !matches!(
            inst,
            Inst::Mov { .. } | Inst::MovImm { .. } | Inst::Lea { .. } | Inst::Not { .. }
        )
    }

    fn eval<D: Domain>(d: &D, inst: &Inst, read: &dyn Fn(Reg) -> D::V) -> D::V {
        match inst {
            Inst::Mov { src, .. } => read(*src),
            Inst::MovImm { imm, .. } => d.lit(*imm as u64),
            Inst::Alu { op, dst, src } => op.apply(d, &read(*dst), &read(*src)),
            Inst::AluImm { op, dst, imm } => {
                op.apply(d, &read(*dst), &d.lit(i64::from(*imm) as u64))
            }
            Inst::Imul3 { src, imm, .. } => d.mul(&read(*src), &d.lit(i64::from(*imm) as u64)),
            Inst::Shift { op, dst, amount } => {
                let v = read(*dst);
                match op {
                    ShiftOp::Shl => d.shl(&v, u32::from(*amount)),
                    ShiftOp::Shr => d.lshr(&v, u32::from(*amount)),
                    ShiftOp::Sar => d.ashr(&v, u32::from(*amount)),
                }
            }
            Inst::Neg { dst } => d.neg(&read(*dst)),
            Inst::Not { dst } => d.not(&read(*dst)),
            Inst::Lea {
                base, index, disp, ..
            } => {
                let mut acc = d.lit(i64::from(*disp) as u64);
                if let Some(b) = base {
                    acc = d.add(&read(*b), &acc);
                }
                if let Some((i, s)) = index {
                    let scaled = d.shl(&read(*i), u32::from(s.trailing_zeros() as u8));
                    acc = d.add(&acc, &scaled);
                }
                acc
            }
            Inst::Andn { a, b, .. } => d.and(&d.not(&read(*a)), &read(*b)),
        }
    }

    fn form(inst: &Inst) -> &'static str {
        match inst {
            Inst::Mov { .. } => "mov r64, r64",
            Inst::MovImm { imm, .. } => {
                if i32::try_from(*imm).is_ok() {
                    "mov r64, imm32"
                } else {
                    "movabs r64, imm64"
                }
            }
            Inst::Alu {
                op: AluOp::Imul, ..
            } => "imul r64, r64",
            Inst::Alu { .. } => "alu r64, r64",
            Inst::AluImm {
                op: AluOp::Add | AluOp::Sub,
                ..
            } => "add/sub r64, imm",
            Inst::AluImm { .. } => "logic r64, imm",
            Inst::Imul3 { .. } => "imul r64, r64, imm",
            Inst::Shift { .. } => "shift r64, imm",
            Inst::Neg { .. } | Inst::Not { .. } => "neg/not r64",
            Inst::Lea {
                base, index, disp, ..
            } => {
                let scaled = matches!(index, Some((_, s)) if *s > 1);
                match (Inst::lea_components(*base, *index, *disp), scaled) {
                    (3, _) => "lea b+i*s+d",
                    (_, true) => "lea b+i*s",
                    _ => "lea b+i or b+d",
                }
            }
            Inst::Andn { .. } => "andn r64, r64, r64",
        }
    }

    fn encoded_len(inst: &Inst) -> u32 {
        let imm_len = |v: i64| if i8::try_from(v).is_ok() { 1 } else { 4 };
        match inst {
            Inst::Mov { .. } => 3,
            Inst::MovImm { imm, .. } => {
                if i32::try_from(*imm).is_ok() {
                    7
                } else {
                    10
                }
            }
            Inst::Alu {
                op: AluOp::Imul, ..
            } => 4,
            Inst::Alu { .. } => 3,
            Inst::AluImm { dst, imm, .. } => {
                if i8::try_from(*imm).is_ok() {
                    4
                } else if *dst == Reg::Rax {
                    6
                } else {
                    7
                }
            }
            Inst::Imul3 { imm, .. } => 3 + imm_len(i64::from(*imm)),
            Inst::Shift { amount, .. } => {
                if *amount == 1 {
                    3
                } else {
                    4
                }
            }
            Inst::Neg { .. } | Inst::Not { .. } => 3,
            Inst::Lea {
                base, index, disp, ..
            } => {
                // REX.W + 8D + ModRM, then SIB and displacement as required.
                let mut len = 3;
                let base_needs_disp = matches!(base, Some(Reg::Rbp | Reg::R13));
                let needs_sib = index.is_some() || matches!(base, Some(Reg::Rsp | Reg::R12));
                if needs_sib {
                    len += 1;
                }
                if base.is_none() {
                    len += 4; // no base: disp32 is mandatory
                } else if *disp != 0 || base_needs_disp {
                    len += imm_len(i64::from(*disp));
                }
                len
            }
            Inst::Andn { .. } => 5,
        }
    }

    fn required_feature(inst: &Inst) -> Option<&'static str> {
        match inst {
            Inst::Andn { .. } => Some("bmi1"),
            _ => None,
        }
    }

    fn parse(text: &str) -> Result<Inst, String> {
        let t = text.trim();
        let (mnemonic, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
        let ops: Vec<&str> = if mnemonic == "lea" {
            match rest.split_once(',') {
                Some((a, b)) => vec![a.trim(), b.trim()],
                None => vec![rest.trim()],
            }
        } else {
            rest.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect()
        };
        let alu = |m: &str| match m {
            "add" => Some(AluOp::Add),
            "sub" => Some(AluOp::Sub),
            "and" => Some(AluOp::And),
            "or" => Some(AluOp::Or),
            "xor" => Some(AluOp::Xor),
            "imul" => Some(AluOp::Imul),
            _ => None,
        };
        let shift = |m: &str| match m {
            "shl" | "sal" => Some(ShiftOp::Shl),
            "shr" => Some(ShiftOp::Shr),
            "sar" => Some(ShiftOp::Sar),
            _ => None,
        };
        match (mnemonic, ops.as_slice()) {
            ("mov" | "movabs", [d, s]) => {
                let dst = reg(d)?;
                match Reg::from_name(s) {
                    Some(src) => Ok(Inst::Mov { dst, src }),
                    None => Ok(Inst::MovImm {
                        dst,
                        imm: parse_int(s)?,
                    }),
                }
            }
            ("imul", [d, s, i]) => Ok(Inst::Imul3 {
                dst: reg(d)?,
                src: reg(s)?,
                imm: imm32(i)?,
            }),
            (m, [d, s]) if alu(m).is_some() => {
                let op = alu(m).unwrap();
                let dst = reg(d)?;
                match Reg::from_name(s) {
                    Some(src) => Ok(Inst::Alu { op, dst, src }),
                    None if op == AluOp::Imul => Ok(Inst::Imul3 {
                        dst,
                        src: dst,
                        imm: imm32(s)?,
                    }),
                    None => Ok(Inst::AluImm {
                        op,
                        dst,
                        imm: imm32(s)?,
                    }),
                }
            }
            (m, [d, a]) if shift(m).is_some() => {
                let amount = parse_int(a)?;
                if !(0..64).contains(&amount) {
                    return Err(format!("shift amount {amount} out of range"));
                }
                Ok(Inst::Shift {
                    op: shift(m).unwrap(),
                    dst: reg(d)?,
                    amount: amount as u8,
                })
            }
            (m, [d]) if shift(m).is_some() => Ok(Inst::Shift {
                op: shift(m).unwrap(),
                dst: reg(d)?,
                amount: 1,
            }),
            ("neg", [d]) => Ok(Inst::Neg { dst: reg(d)? }),
            ("not", [d]) => Ok(Inst::Not { dst: reg(d)? }),
            ("lea", [d, a]) => {
                let (base, index, disp) = parse_address(a)?;
                Ok(Inst::Lea {
                    dst: reg(d)?,
                    base,
                    index,
                    disp,
                })
            }
            ("andn", [d, a, b]) => Ok(Inst::Andn {
                dst: reg(d)?,
                a: reg(a)?,
                b: reg(b)?,
            }),
            _ => Err(format!("unsupported instruction `{t}`")),
        }
    }

    fn rename(inst: &Inst, map: &dyn Fn(Reg) -> Reg) -> Inst {
        match inst.clone() {
            Inst::Mov { dst, src } => Inst::Mov {
                dst: map(dst),
                src: map(src),
            },
            Inst::MovImm { dst, imm } => Inst::MovImm { dst: map(dst), imm },
            Inst::Alu { op, dst, src } => Inst::Alu {
                op,
                dst: map(dst),
                src: map(src),
            },
            Inst::AluImm { op, dst, imm } => Inst::AluImm {
                op,
                dst: map(dst),
                imm,
            },
            Inst::Imul3 { dst, src, imm } => Inst::Imul3 {
                dst: map(dst),
                src: map(src),
                imm,
            },
            Inst::Shift { op, dst, amount } => Inst::Shift {
                op,
                dst: map(dst),
                amount,
            },
            Inst::Neg { dst } => Inst::Neg { dst: map(dst) },
            Inst::Not { dst } => Inst::Not { dst: map(dst) },
            Inst::Lea {
                dst,
                base,
                index,
                disp,
            } => Inst::Lea {
                dst: map(dst),
                base: base.map(map),
                index: index.map(|(r, s)| (map(r), s)),
                disp,
            },
            Inst::Andn { dst, a, b } => Inst::Andn {
                dst: map(dst),
                a: map(a),
                b: map(b),
            },
        }
    }

    fn caller_saved() -> Vec<Reg> {
        // System V AMD64 ABI.
        vec![
            Reg::Rax,
            Reg::Rcx,
            Reg::Rdx,
            Reg::Rsi,
            Reg::Rdi,
            Reg::R8,
            Reg::R9,
            Reg::R10,
            Reg::R11,
        ]
    }
}
