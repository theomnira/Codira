//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! `codira_mcode` -- the machine-instruction layer of Project APEX
//! (experimental: nothing in `codira build` calls it yet).
//!
//! It models a deliberately small, exactly specified subset of two
//! instruction sets: 64-bit general-purpose integer instructions of x86-64
//! and `AArch64` that read and write registers only (no memory, no branches).
//! For that subset it provides
//!
//! * an instruction representation with a parser and printer for the
//!   assembler syntax `llvm-objdump` and `llvm-mc` use (Intel syntax for
//!   x86-64), so real compiler output can be read back;
//! * one semantics per instruction, written against [`Domain`], interpreted
//!   both concretely and as Z3 bit-vector terms ([`domain`]);
//! * straight-line [`Block`]s with explicit live-in and live-out registers,
//!   and [`prove_equivalent`], which decides with Z3 whether two blocks
//!   compute the same live-out values for every input;
//! * versioned per-microarchitecture cost [`profile`]s and a block cost
//!   estimate (critical path, port pressure, issue width, code size). The
//!   crate's `profiles/` directory holds two: x86-64 latencies and
//!   throughputs measured with `examples/calibrate.rs` on an Emerald Rapids
//!   virtual machine (port sets from uops.info), and `AArch64` values
//!   transcribed from the Arm Neoverse N1 Software Optimization Guide, never
//!   measured;
//! * [`measure`], which assembles blocks into a timing harness and runs them
//!   on the host processor, so that a cost model is never the last word.
//!
//! What is *not* modelled, and therefore refused rather than guessed:
//! memory operands, 8/16/32-bit register forms, flags as outputs (a block
//! whose flags are live at its end cannot be rewritten by anything built on
//! this crate), branches, and floating point.

// Instruction tables keep one arm per instruction shape, even where two
// shapes share a value, so each shape's entry can be read and changed alone.
#![allow(clippy::match_same_arms)]

pub mod aarch64;
pub mod domain;
pub mod measure;
pub mod profile;
pub mod x86;

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;

use codira_smt::{BoolExpr, Context, SatResult};
pub use domain::{Concrete, Domain, Symbolic};

/// An instruction set the crate models.
pub trait Isa: Copy + Eq + Hash + fmt::Debug + 'static {
    type Reg: Copy + Eq + Hash + Ord + fmt::Debug + fmt::Display;
    type Inst: Clone + Eq + Hash + fmt::Debug + fmt::Display;

    /// Short target name (`x86_64`, `aarch64`).
    const NAME: &'static str;

    /// Registers the instruction reads, in operand order (duplicates kept).
    fn reads(inst: &Self::Inst) -> Vec<Self::Reg>;
    /// The register the instruction writes. Every modelled instruction
    /// writes exactly one general-purpose register.
    fn writes(inst: &Self::Inst) -> Self::Reg;
    /// Whether the instruction overwrites the condition flags.
    fn clobbers_flags(inst: &Self::Inst) -> bool;
    /// The value written to [`Isa::writes`], given the values of the
    /// registers it reads (looked up through `read`).
    fn eval<D: Domain>(d: &D, inst: &Self::Inst, read: &dyn Fn(Self::Reg) -> D::V) -> D::V;
    /// The cost-table key of the instruction's form, e.g. `add r64, r64`.
    fn form(inst: &Self::Inst) -> &'static str;
    /// Encoded length in bytes.
    fn encoded_len(inst: &Self::Inst) -> u32;
    /// The ISA extension the instruction needs beyond the base ISA, if any.
    fn required_feature(inst: &Self::Inst) -> Option<&'static str>;
    /// Parses one instruction in the syntax [`fmt::Display`] prints.
    fn parse(text: &str) -> Result<Self::Inst, String>;
    /// The instruction with every register renamed through `map`.
    fn rename(inst: &Self::Inst, map: &dyn Fn(Self::Reg) -> Self::Reg) -> Self::Inst;
    /// Registers a block may use as scratch when nothing else says
    /// otherwise: caller-saved registers of the platform's C ABI.
    fn caller_saved() -> Vec<Self::Reg>;
}

/// A straight-line block: instructions plus the registers whose values flow
/// in and whose values are observed afterwards. Every other register the
/// block writes is assumed dead at its end; flags are assumed dead at its end.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Block<I: Isa> {
    pub insts: Vec<I::Inst>,
    pub live_in: Vec<I::Reg>,
    pub live_out: Vec<I::Reg>,
}

impl<I: Isa> Block<I> {
    pub fn new(insts: Vec<I::Inst>, live_in: Vec<I::Reg>, live_out: Vec<I::Reg>) -> Self {
        Block {
            insts,
            live_in,
            live_out,
        }
    }

    /// Parses one instruction per line (blank lines and `;`/`//` comments
    /// ignored).
    pub fn parse(text: &str, live_in: Vec<I::Reg>, live_out: Vec<I::Reg>) -> Result<Self, String> {
        let mut insts = Vec::new();
        for line in text.lines() {
            let line = line.split(';').next().unwrap_or("");
            let line = line.split("//").next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            insts.push(I::parse(line).map_err(|e| format!("`{line}`: {e}"))?);
        }
        let block = Block::new(insts, live_in, live_out);
        block.check()?;
        Ok(block)
    }

    /// Checks that every register is defined before it is read and that
    /// every live-out register is defined (live-in or written).
    pub fn check(&self) -> Result<(), String> {
        let mut defined: Vec<I::Reg> = self.live_in.clone();
        for inst in &self.insts {
            for r in I::reads(inst) {
                if !defined.contains(&r) {
                    return Err(format!(
                        "`{inst}` reads {r}, which is neither live-in nor written earlier"
                    ));
                }
            }
            let w = I::writes(inst);
            if !defined.contains(&w) {
                defined.push(w);
            }
        }
        for r in &self.live_out {
            if !defined.contains(r) {
                return Err(format!("live-out {r} is never defined"));
            }
        }
        Ok(())
    }

    /// Runs the block in domain `d` on `inputs` (one per live-in register,
    /// in order) and returns the live-out values in order.
    pub fn eval<D: Domain>(&self, d: &D, inputs: &[D::V]) -> Vec<D::V> {
        assert_eq!(inputs.len(), self.live_in.len());
        let mut state: HashMap<I::Reg, D::V> = HashMap::new();
        for (r, v) in self.live_in.iter().zip(inputs) {
            state.insert(*r, v.clone());
        }
        for inst in &self.insts {
            let value = {
                let read = |r: I::Reg| -> D::V {
                    state.get(&r).cloned().unwrap_or_else(|| {
                        panic!("{r} read before definition (Block::check not run)")
                    })
                };
                I::eval(d, inst, &read)
            };
            state.insert(I::writes(inst), value);
        }
        self.live_out.iter().map(|r| state[r].clone()).collect()
    }

    /// Concrete evaluation.
    pub fn run(&self, inputs: &[u64]) -> Vec<u64> {
        self.eval(&Concrete, inputs)
    }

    /// Registers the block writes.
    pub fn written(&self) -> Vec<I::Reg> {
        let mut out = Vec::new();
        for inst in &self.insts {
            let w = I::writes(inst);
            if !out.contains(&w) {
                out.push(w);
            }
        }
        out
    }

    /// Whether any instruction overwrites the flags.
    pub fn clobbers_flags(&self) -> bool {
        self.insts.iter().any(I::clobbers_flags)
    }

    /// Extensions the block needs.
    pub fn required_features(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> =
            self.insts.iter().filter_map(I::required_feature).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub fn encoded_len(&self) -> u32 {
        self.insts.iter().map(I::encoded_len).sum()
    }
}

impl<I: Isa> fmt::Display for Block<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for inst in &self.insts {
            writeln!(f, "{inst}")?;
        }
        Ok(())
    }
}

/// The outcome of an equivalence query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Equivalence {
    /// Z3 found no input on which the live-out values differ. `smtlib` is
    /// the self-contained SMT-LIB2 query (the negated equivalence) whose
    /// `unsat` answer is the evidence; any solver can re-check it.
    Proven { smtlib: String },
    /// The live-out values differ on these inputs (one per live-in).
    Refuted { inputs: Vec<u64> },
    /// The solver gave up (resource limit). Never treated as proven.
    Unknown { reason: String },
    /// The blocks are not comparable (different live-in/live-out lists).
    Incomparable { reason: String },
}

impl Equivalence {
    pub fn is_proven(&self) -> bool {
        matches!(self, Equivalence::Proven { .. })
    }
}

/// Decides whether `a` and `b` compute the same live-out values for every
/// assignment of 64-bit values to the live-in registers.
///
/// Both blocks are encoded with the same symbolic semantics; the query
/// asserts that some live-out differs and asks for `unsat`. A satisfying
/// model is returned as a counterexample and re-checked concretely on both
/// blocks before it is reported (a solver/model disagreement panics: it
/// would mean the two interpretations of the semantics diverged).
pub fn prove_equivalent<I: Isa>(a: &Block<I>, b: &Block<I>) -> Equivalence {
    if a.live_in != b.live_in || a.live_out != b.live_out {
        return Equivalence::Incomparable {
            reason: "live-in or live-out registers differ".into(),
        };
    }
    if let Err(e) = a.check().and(b.check()) {
        return Equivalence::Incomparable { reason: e };
    }
    let ctx = Context::new();
    let sym = Symbolic { ctx: &ctx };
    let vars: Vec<_> = a
        .live_in
        .iter()
        .map(|r| ctx.bv_var(&format!("{r}"), 64))
        .collect();
    let out_a = a.eval(&sym, &vars);
    let out_b = b.eval(&sym, &vars);
    let differs: Vec<BoolExpr<'_>> = out_a
        .iter()
        .zip(&out_b)
        .map(|(x, y)| x.eq(*y).not())
        .collect();
    let mut query = ctx.bool_false();
    for d in differs {
        query = query.or(d);
    }
    let solver = ctx.solver();
    solver.assert(query);
    match solver.check() {
        SatResult::Unsat => Equivalence::Proven {
            smtlib: ctx.to_smtlib2(&[], query),
        },
        SatResult::Sat => {
            let model = solver.model().expect("sat result without a model");
            let inputs: Vec<u64> = vars
                .iter()
                .map(|v| model.eval_bv64(*v).expect("model value for a live-in") as u64)
                .collect();
            assert_ne!(
                a.run(&inputs),
                b.run(&inputs),
                "Z3 counterexample does not separate the blocks concretely: semantics diverged"
            );
            Equivalence::Refuted { inputs }
        }
        SatResult::Unknown => Equivalence::Unknown {
            reason: solver.reason_unknown().to_string(),
        },
    }
}

#[cfg(test)]
mod tests;
