//! `AArch64`: 64-bit general-purpose register instructions (X registers).
//!
//! Semantics follow the Arm Architecture Reference Manual (Arm ARM, A64
//! base instructions) for the 64-bit (`sf = 1`) forms: shifted-register
//! operands apply LSL/LSR/ASR by an immediate in `0..64` to the second
//! source before the operation; `madd d, n, m, a` = `a + n*m`, `msub` =
//! `a - n*m`; `bic` = `n & !m`, `orn` = `n | !m`, `eon` = `n ^ !m`; `movz`
//! writes a 16-bit immediate shifted by 0/16/32/48 and zeroes the rest. None
//! of the modelled forms set flags (the `s`-suffixed forms are not modelled).
//! `x31` is not modelled (it is `sp` or `xzr` depending on the instruction).

use std::fmt;

use crate::{Domain, Isa};

/// `x0` .. `x30`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Reg(pub u8);

impl fmt::Display for Reg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "x{}", self.0)
    }
}

impl Reg {
    pub fn parse(text: &str) -> Result<Reg, String> {
        let t = text.trim();
        let n = t
            .strip_prefix('x')
            .and_then(|n| n.parse::<u8>().ok())
            .filter(|n| *n <= 30)
            .ok_or_else(|| format!("unsupported register `{t}` (only x0..x30 are modelled)"))?;
        Ok(Reg(n))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShiftKind {
    Lsl,
    Lsr,
    Asr,
}

impl ShiftKind {
    fn name(self) -> &'static str {
        match self {
            ShiftKind::Lsl => "lsl",
            ShiftKind::Lsr => "lsr",
            ShiftKind::Asr => "asr",
        }
    }
    fn parse(t: &str) -> Option<ShiftKind> {
        match t {
            "lsl" => Some(ShiftKind::Lsl),
            "lsr" => Some(ShiftKind::Lsr),
            "asr" => Some(ShiftKind::Asr),
            _ => None,
        }
    }
    fn apply<D: Domain>(self, d: &D, v: &D::V, amount: u8) -> D::V {
        match self {
            ShiftKind::Lsl => d.shl(v, u32::from(amount)),
            ShiftKind::Lsr => d.lshr(v, u32::from(amount)),
            ShiftKind::Asr => d.ashr(v, u32::from(amount)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    And,
    Orr,
    Eor,
    Bic,
    Orn,
    Eon,
}

impl BinOp {
    fn name(self) -> &'static str {
        match self {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::And => "and",
            BinOp::Orr => "orr",
            BinOp::Eor => "eor",
            BinOp::Bic => "bic",
            BinOp::Orn => "orn",
            BinOp::Eon => "eon",
        }
    }
    fn parse(t: &str) -> Option<BinOp> {
        [
            BinOp::Add,
            BinOp::Sub,
            BinOp::And,
            BinOp::Orr,
            BinOp::Eor,
            BinOp::Bic,
            BinOp::Orn,
            BinOp::Eon,
        ]
        .into_iter()
        .find(|o| o.name() == t)
    }
    pub fn apply<D: Domain>(self, d: &D, n: &D::V, m: &D::V) -> D::V {
        match self {
            BinOp::Add => d.add(n, m),
            BinOp::Sub => d.sub(n, m),
            BinOp::And => d.and(n, m),
            BinOp::Orr => d.or(n, m),
            BinOp::Eor => d.xor(n, m),
            BinOp::Bic => d.and(n, &d.not(m)),
            BinOp::Orn => d.or(n, &d.not(m)),
            BinOp::Eon => d.xor(n, &d.not(m)),
        }
    }
    fn is_arith(self) -> bool {
        matches!(self, BinOp::Add | BinOp::Sub)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Inst {
    Mov {
        dst: Reg,
        src: Reg,
    },
    /// `movz dst, #imm16, lsl #shift`.
    Movz {
        dst: Reg,
        imm: u16,
        shift: u8,
    },
    /// `op dst, n, m{, kind #amount}`. Arithmetic ops (`add`, `sub`) do not
    /// accept `ror`, which is not modelled at all.
    Bin {
        op: BinOp,
        dst: Reg,
        n: Reg,
        m: Reg,
        shift: Option<(ShiftKind, u8)>,
    },
    /// `add|sub dst, n, #imm12{, lsl #12}`.
    AddSubImm {
        sub: bool,
        dst: Reg,
        n: Reg,
        imm: u16,
        lsl12: bool,
    },
    Mul {
        dst: Reg,
        n: Reg,
        m: Reg,
    },
    /// `madd` (`sub == false`) or `msub`.
    Madd {
        sub: bool,
        dst: Reg,
        n: Reg,
        m: Reg,
        a: Reg,
    },
    /// `lsl|lsr|asr dst, n, #amount` (aliases of `ubfm`/`sbfm`).
    ShiftImm {
        kind: ShiftKind,
        dst: Reg,
        n: Reg,
        amount: u8,
    },
    /// `neg dst, m{, shift}` (alias of `sub dst, xzr, m`).
    Neg {
        dst: Reg,
        m: Reg,
        shift: Option<(ShiftKind, u8)>,
    },
    /// `mvn dst, m` (alias of `orn dst, xzr, m`).
    Mvn {
        dst: Reg,
        m: Reg,
    },
}

impl fmt::Display for Inst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sh = |s: &Option<(ShiftKind, u8)>| match s {
            Some((k, a)) if *a != 0 => format!(", {} #{a}", k.name()),
            _ => String::new(),
        };
        match self {
            Inst::Mov { dst, src } => write!(f, "mov {dst}, {src}"),
            Inst::Movz { dst, imm, shift } => {
                if *shift == 0 {
                    write!(f, "movz {dst}, #{imm}")
                } else {
                    write!(f, "movz {dst}, #{imm}, lsl #{shift}")
                }
            }
            Inst::Bin {
                op,
                dst,
                n,
                m,
                shift,
            } => write!(f, "{} {dst}, {n}, {m}{}", op.name(), sh(shift)),
            Inst::AddSubImm {
                sub,
                dst,
                n,
                imm,
                lsl12,
            } => write!(
                f,
                "{} {dst}, {n}, #{imm}{}",
                if *sub { "sub" } else { "add" },
                if *lsl12 { ", lsl #12" } else { "" }
            ),
            Inst::Mul { dst, n, m } => write!(f, "mul {dst}, {n}, {m}"),
            Inst::Madd { sub, dst, n, m, a } => {
                write!(
                    f,
                    "{} {dst}, {n}, {m}, {a}",
                    if *sub { "msub" } else { "madd" }
                )
            }
            Inst::ShiftImm {
                kind,
                dst,
                n,
                amount,
            } => write!(f, "{} {dst}, {n}, #{amount}", kind.name()),
            Inst::Neg { dst, m, shift } => write!(f, "neg {dst}, {m}{}", sh(shift)),
            Inst::Mvn { dst, m } => write!(f, "mvn {dst}, {m}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Aarch64;

fn imm(text: &str) -> Result<i64, String> {
    let t = text.trim().trim_start_matches('#');
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    let v = if let Some(h) = t.strip_prefix("0x") {
        i64::from_str_radix(h, 16).map_err(|e| format!("bad immediate `{text}`: {e}"))?
    } else {
        t.parse::<i64>()
            .map_err(|e| format!("bad immediate `{text}`: {e}"))?
    };
    Ok(if neg { -v } else { v })
}

fn parse_shift(text: &str) -> Result<(ShiftKind, u8), String> {
    let t = text.trim();
    let (k, a) = t
        .split_once(char::is_whitespace)
        .ok_or_else(|| format!("bad shift `{t}`"))?;
    let kind = ShiftKind::parse(k).ok_or_else(|| format!("unsupported shift `{k}`"))?;
    let amount = imm(a)?;
    if !(0..64).contains(&amount) {
        return Err(format!("shift amount {amount} out of range"));
    }
    Ok((kind, amount as u8))
}

impl Isa for Aarch64 {
    type Reg = Reg;
    type Inst = Inst;
    const NAME: &'static str = "aarch64";

    fn reads(inst: &Inst) -> Vec<Reg> {
        match inst {
            Inst::Mov { src, .. } => vec![*src],
            Inst::Movz { .. } => vec![],
            Inst::Bin { n, m, .. } | Inst::Mul { n, m, .. } => vec![*n, *m],
            Inst::AddSubImm { n, .. } | Inst::ShiftImm { n, .. } => vec![*n],
            Inst::Madd { n, m, a, .. } => vec![*n, *m, *a],
            Inst::Neg { m, .. } | Inst::Mvn { m, .. } => vec![*m],
        }
    }

    fn writes(inst: &Inst) -> Reg {
        match inst {
            Inst::Mov { dst, .. }
            | Inst::Movz { dst, .. }
            | Inst::Bin { dst, .. }
            | Inst::AddSubImm { dst, .. }
            | Inst::Mul { dst, .. }
            | Inst::Madd { dst, .. }
            | Inst::ShiftImm { dst, .. }
            | Inst::Neg { dst, .. }
            | Inst::Mvn { dst, .. } => *dst,
        }
    }

    fn clobbers_flags(_: &Inst) -> bool {
        false
    }

    fn eval<D: Domain>(d: &D, inst: &Inst, read: &dyn Fn(Reg) -> D::V) -> D::V {
        let shifted = |m: Reg, s: &Option<(ShiftKind, u8)>| {
            let v = read(m);
            match s {
                Some((k, a)) => k.apply(d, &v, *a),
                None => v,
            }
        };
        match inst {
            Inst::Mov { src, .. } => read(*src),
            Inst::Movz { imm, shift, .. } => d.lit(u64::from(*imm) << shift),
            Inst::Bin {
                op, n, m, shift, ..
            } => op.apply(d, &read(*n), &shifted(*m, shift)),
            Inst::AddSubImm {
                sub, n, imm, lsl12, ..
            } => {
                let k = d.lit(u64::from(*imm) << if *lsl12 { 12 } else { 0 });
                if *sub {
                    d.sub(&read(*n), &k)
                } else {
                    d.add(&read(*n), &k)
                }
            }
            Inst::Mul { n, m, .. } => d.mul(&read(*n), &read(*m)),
            Inst::Madd { sub, n, m, a, .. } => {
                let p = d.mul(&read(*n), &read(*m));
                if *sub {
                    d.sub(&read(*a), &p)
                } else {
                    d.add(&read(*a), &p)
                }
            }
            Inst::ShiftImm {
                kind, n, amount, ..
            } => kind.apply(d, &read(*n), *amount),
            Inst::Neg { m, shift, .. } => d.neg(&shifted(*m, shift)),
            Inst::Mvn { m, .. } => d.not(&read(*m)),
        }
    }

    fn form(inst: &Inst) -> &'static str {
        match inst {
            Inst::Mov { .. } => "mov x, x",
            Inst::Movz { .. } => "movz x, imm",
            Inst::Bin {
                op,
                shift: None | Some((_, 0)),
                ..
            } if op.is_arith() => "add/sub x, x, x",
            Inst::Bin {
                op,
                shift: Some((ShiftKind::Lsl, 1..=4)),
                ..
            } if op.is_arith() => "add/sub x, x, x, lsl #1-4",
            Inst::Bin { op, .. } if op.is_arith() => "add/sub x, x, x, shift",
            Inst::Bin {
                shift: None | Some((_, 0)),
                ..
            } => "logic x, x, x",
            Inst::Bin { .. } => "logic x, x, x, shift",
            Inst::AddSubImm { .. } => "add/sub x, x, imm",
            Inst::Mul { .. } => "mul x, x, x",
            Inst::Madd { .. } => "madd/msub x, x, x, x",
            Inst::ShiftImm { .. } => "shift x, x, imm",
            Inst::Neg {
                shift: None | Some((_, 0)),
                ..
            } => "add/sub x, x, x",
            Inst::Neg { .. } => "add/sub x, x, x, shift",
            Inst::Mvn { .. } => "logic x, x, x",
        }
    }

    fn encoded_len(_: &Inst) -> u32 {
        4
    }

    fn required_feature(_: &Inst) -> Option<&'static str> {
        None
    }

    fn parse(text: &str) -> Result<Inst, String> {
        let t = text.trim();
        let (mnemonic, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
        let ops: Vec<&str> = rest
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let r = Reg::parse;
        match (mnemonic, ops.as_slice()) {
            ("mov", [d, s]) if s.starts_with('x') => Ok(Inst::Mov {
                dst: r(d)?,
                src: r(s)?,
            }),
            ("mov" | "movz", [d, i, rest @ ..]) if i.starts_with('#') => {
                let value = imm(i)?;
                let shift = match rest {
                    [] => 0,
                    [s] => match parse_shift(s)? {
                        (ShiftKind::Lsl, a) if a % 16 == 0 => a,
                        _ => return Err(format!("bad movz shift in `{t}`")),
                    },
                    _ => return Err(format!("bad operands in `{t}`")),
                };
                let value = u16::try_from(value)
                    .map_err(|_e| format!("`{t}`: only 16-bit movz immediates are modelled"))?;
                Ok(Inst::Movz {
                    dst: r(d)?,
                    imm: value,
                    shift,
                })
            }
            ("add" | "sub", [d, n, i, rest @ ..]) if i.starts_with('#') => {
                let v = imm(i)?;
                let lsl12 = match rest {
                    [] => false,
                    [s] => parse_shift(s)? == (ShiftKind::Lsl, 12),
                    _ => return Err(format!("bad operands in `{t}`")),
                };
                let v = u16::try_from(v)
                    .ok()
                    .filter(|v| *v < 4096)
                    .ok_or_else(|| format!("`{t}`: immediate is not a 12-bit unsigned value"))?;
                Ok(Inst::AddSubImm {
                    sub: mnemonic == "sub",
                    dst: r(d)?,
                    n: r(n)?,
                    imm: v,
                    lsl12,
                })
            }
            (m, [d, n, mm, rest @ ..]) if BinOp::parse(m).is_some() => {
                let op = BinOp::parse(m).unwrap();
                let shift = match rest {
                    [] => None,
                    [s] => Some(parse_shift(s)?),
                    _ => return Err(format!("bad operands in `{t}`")),
                };
                Ok(Inst::Bin {
                    op,
                    dst: r(d)?,
                    n: r(n)?,
                    m: r(mm)?,
                    shift,
                })
            }
            ("mul", [d, n, m]) => Ok(Inst::Mul {
                dst: r(d)?,
                n: r(n)?,
                m: r(m)?,
            }),
            ("madd" | "msub", [d, n, m, a]) => Ok(Inst::Madd {
                sub: mnemonic == "msub",
                dst: r(d)?,
                n: r(n)?,
                m: r(m)?,
                a: r(a)?,
            }),
            (k, [d, n, a]) if ShiftKind::parse(k).is_some() => {
                let amount = imm(a)?;
                if !(0..64).contains(&amount) {
                    return Err(format!("shift amount {amount} out of range"));
                }
                Ok(Inst::ShiftImm {
                    kind: ShiftKind::parse(k).unwrap(),
                    dst: r(d)?,
                    n: r(n)?,
                    amount: amount as u8,
                })
            }
            ("neg", [d, m, rest @ ..]) => Ok(Inst::Neg {
                dst: r(d)?,
                m: r(m)?,
                shift: match rest {
                    [] => None,
                    [s] => Some(parse_shift(s)?),
                    _ => return Err(format!("bad operands in `{t}`")),
                },
            }),
            ("mvn", [d, m]) => Ok(Inst::Mvn {
                dst: r(d)?,
                m: r(m)?,
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
            Inst::Movz { dst, imm, shift } => Inst::Movz {
                dst: map(dst),
                imm,
                shift,
            },
            Inst::Bin {
                op,
                dst,
                n,
                m,
                shift,
            } => Inst::Bin {
                op,
                dst: map(dst),
                n: map(n),
                m: map(m),
                shift,
            },
            Inst::AddSubImm {
                sub,
                dst,
                n,
                imm,
                lsl12,
            } => Inst::AddSubImm {
                sub,
                dst: map(dst),
                n: map(n),
                imm,
                lsl12,
            },
            Inst::Mul { dst, n, m } => Inst::Mul {
                dst: map(dst),
                n: map(n),
                m: map(m),
            },
            Inst::Madd { sub, dst, n, m, a } => Inst::Madd {
                sub,
                dst: map(dst),
                n: map(n),
                m: map(m),
                a: map(a),
            },
            Inst::ShiftImm {
                kind,
                dst,
                n,
                amount,
            } => Inst::ShiftImm {
                kind,
                dst: map(dst),
                n: map(n),
                amount,
            },
            Inst::Neg { dst, m, shift } => Inst::Neg {
                dst: map(dst),
                m: map(m),
                shift,
            },
            Inst::Mvn { dst, m } => Inst::Mvn {
                dst: map(dst),
                m: map(m),
            },
        }
    }

    fn caller_saved() -> Vec<Reg> {
        // AAPCS64: x0-x17 are caller-saved; x16/x17 (IP0/IP1) may be
        // clobbered by veneers, x18 is the platform register.
        (0..=15).map(Reg).collect()
    }
}
