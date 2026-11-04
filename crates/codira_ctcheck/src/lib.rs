//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - A machine-code constant-time checker. It reads the machine code of each
//!   `oblivious` / `@constant_time` function out of a built library, decodes
//!   it, and checks it against a small, explicit rule set.
//!
//! # Why
//!
//! The source checker (`codira_hir::verify::flow`) proves rules about the
//! source, and the code generator emits `ct_select` as an LLVM `select`. That
//! says nothing about what the LLVM backend makes of it: a `select` may become
//! a branch, and a function may end up calling code nobody checked. This
//! crate looks at the instructions that will run.
//!
//! # The rules
//!
//! Within the code range of a checked function (its symbol in the object's
//! symbol table):
//!
//! * **(a) no conditional branch** at all: x86-64 `jcc`, `jrcxz`/`jecxz`,
//!   `loop*`, `xbegin`; `AArch64` `b.cond`, `cbz`/`cbnz`, `tbz`/`tbnz`. This is
//!   deliberately stricter than "no branch on a secret": a branch on a public
//!   loop counter is reported too. Each report says whether the condition was
//!   found to depend on a secret register value (see rule d), which is a
//!   hint, not part of the rule.
//! * **no indirect branch** (a jump table is a branch on a value) and no
//!   instruction the decoder cannot decode.
//! * **(b) no variable-time instruction**: [`x86::VARIABLE_TIME`] and
//!   [`aarch64::VARIABLE_TIME`] (integer and floating-point division and
//!   square root, x87 transcendental and remainder instructions), and on
//!   x86-64 a `rep`-prefixed string instruction whose count register holds a
//!   secret.
//! * **(c) no call** (or tail call) except to another function this check
//!   covers, by symbol, or to [`ALLOWED_CALLS`] (empty today: no runtime
//!   function has been shown to be branch-free). Indirect calls and calls
//!   whose target cannot be resolved are reported.
//! * **(d) no memory access whose address depends on a secret**, x86-64 only,
//!   by a forward taint analysis over registers, the flags and stack slots.
//!   Taint starts in the registers (and stack slots) that the calling
//!   convention passes the secret parameters in, and in the result registers
//!   of calls to functions with a secret result. It does **not** start in
//!   memory: a secret that reaches the function through a pointer (an array,
//!   a struct passed by reference) is invisible here and covered only by the
//!   source rule. A tainted value stored to non-stack memory makes every
//!   later non-stack load tainted. When the parameters cannot be mapped to
//!   registers (anything but integers, `bool`s and floats), rule (d) is not
//!   checked for that function and the report says so.
//!
//! # What this does not check
//!
//! Speculative execution (Spectre-style transient leaks) is out of scope.
//! Micro-architectural effects beyond these rules -- data-dependent latency of
//! instructions not listed (multiplication, shifts and `cmov` are assumed
//! constant-time), cache-bank conflicts, frequency scaling, subnormal floating
//! point -- are not modelled. Passing the check is not a proof of
//! constant-time execution; it is evidence that these rules hold for the
//! instructions in the checked ranges.

pub mod aarch64;
mod image;
#[cfg(test)]
mod tests;
pub mod x86;

use std::collections::HashMap;
use std::fmt;

pub use image::Image;

/// Functions a checked function may call although this check does not cover
/// them. Empty: no runtime function has been shown to be branch-free.
pub const ALLOWED_CALLS: &[&str] = &[];

/// The instruction set of the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Arch::X86_64 => "x86-64",
            Arch::Aarch64 => "AArch64",
        })
    }
}

/// How arguments are passed, which decides where secret parameters start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallingConvention {
    /// The System V AMD64 ABI (Linux, macOS, the BSDs).
    SysV,
    /// The Microsoft x64 convention.
    Win64,
    /// The Arm 64-bit procedure call standard.
    Aapcs64,
}

/// How a parameter is passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamClass {
    /// An integer of at most 64 bits, or a `bool`: one general register.
    Integer,
    /// A 128-bit integer: two general registers.
    Integer128,
    /// An `f32` or `f64`: one vector register.
    Float,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Param {
    pub class: ParamClass,
    pub secret: bool,
}

/// A function to check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionSpec {
    /// The symbol its machine code carries.
    pub symbol: String,
    /// The name diagnostics use (for example the function's path).
    pub name: String,
    /// The parameters in order, when every one of them is passed in
    /// registers or stack slots as [`ParamClass`] describes; `None`
    /// otherwise, and then rule (d) is not checked.
    pub params: Option<Vec<Param>>,
    /// Whether its result is secret.
    pub secret_result: bool,
}

/// The rule an instruction breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rule {
    ConditionalBranch,
    IndirectBranch,
    Undecodable,
    VariableTime,
    Call,
    SecretAddress,
}

impl Rule {
    pub fn describe(self) -> &'static str {
        match self {
            Rule::ConditionalBranch => "conditional branch",
            Rule::IndirectBranch => "indirect branch",
            Rule::Undecodable => "instruction that cannot be decoded",
            Rule::VariableTime => "variable-time instruction",
            Rule::Call => "call to code that is not checked",
            Rule::SecretAddress => "memory access at a secret-dependent address",
        }
    }
}

/// One instruction that breaks a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub rule: Rule,
    /// The instruction's address in the image.
    pub address: u64,
    /// The instruction, disassembled (Intel syntax on x86-64).
    pub disassembly: String,
    /// More about it: the call target, whether a condition is secret, ...
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:#x}: {:<32} {}",
            self.address,
            self.disassembly,
            self.rule.describe()
        )?;
        if !self.detail.is_empty() {
            write!(f, " ({})", self.detail)?;
        }
        Ok(())
    }
}

/// The result for one function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionReport {
    pub name: String,
    pub symbol: String,
    /// Where its code is, when it has a symbol of its own.
    pub address: Option<u64>,
    pub size: u64,
    pub instructions: usize,
    pub violations: Vec<Violation>,
    /// Whether rule (d) was checked.
    pub addresses_checked: bool,
    /// Why something was not checked.
    pub notes: Vec<String>,
}

impl FunctionReport {
    /// Whether the function's code was found and decoded.
    pub fn was_checked(&self) -> bool {
        self.address.is_some()
    }

    pub fn passed(&self) -> bool {
        self.was_checked() && self.violations.is_empty()
    }
}

/// The result for an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub arch: Arch,
    pub functions: Vec<FunctionReport>,
}

impl Report {
    pub fn violations(&self) -> impl Iterator<Item = (&FunctionReport, &Violation)> {
        self.functions
            .iter()
            .flat_map(|f| f.violations.iter().map(move |v| (f, v)))
    }

    pub fn passed(&self) -> bool {
        self.functions.iter().all(|f| f.violations.is_empty())
    }
}

/// What a checked function's code may call, and how call targets resolve.
pub struct CallTargets<'a> {
    /// The function whose code starts at an address (a symbol, or a PLT or
    /// import stub), if known.
    pub resolve: &'a dyn Fn(u64) -> Option<String>,
    /// The functions this check covers, by symbol.
    pub checked: &'a HashMap<String, FunctionSpec>,
}

impl CallTargets<'_> {
    fn allowed(&self, name: &str) -> bool {
        self.checked.contains_key(name) || ALLOWED_CALLS.contains(&name)
    }
}

/// What checking one function's code found, including the taint it passes to
/// the functions it calls.
pub(crate) struct CodeResult {
    pub violations: Vec<Violation>,
    pub instructions: usize,
    pub addresses_checked: bool,
    pub notes: Vec<String>,
    /// Per callee symbol, per parameter: whether a call passes it a secret.
    pub call_taint: HashMap<String, Vec<bool>>,
}

/// Checks the machine code of one function: `code` is its bytes, loaded at
/// `address`. The library entry point for code that is not in an object
/// file (for example a rewritten instruction sequence); [`check_image`]
/// checks every function of a built library.
pub fn check_code(
    arch: Arch,
    convention: CallingConvention,
    code: &[u8],
    address: u64,
    function: &FunctionSpec,
    calls: &CallTargets<'_>,
) -> FunctionReport {
    let result = check_code_inner(arch, convention, code, address, function, calls);
    FunctionReport {
        name: function.name.clone(),
        symbol: function.symbol.clone(),
        address: Some(address),
        size: code.len() as u64,
        instructions: result.instructions,
        violations: result.violations,
        addresses_checked: result.addresses_checked,
        notes: result.notes,
    }
}

fn check_code_inner(
    arch: Arch,
    convention: CallingConvention,
    code: &[u8],
    address: u64,
    function: &FunctionSpec,
    calls: &CallTargets<'_>,
) -> CodeResult {
    match arch {
        Arch::X86_64 => x86::check(convention, code, address, function, calls),
        Arch::Aarch64 => aarch64::check(code, address, function, calls),
    }
}

/// A function to check and where its code is, or why it was not found.
type Located<'b> = (FunctionSpec, Result<(u64, &'b [u8]), String>);

/// Checks every function of `functions` in the built library or object
/// file `bytes` (ELF, Mach-O or PE/COFF; x86-64 or `AArch64`).
///
/// Secret taint passed between checked functions is propagated to a fixed
/// point: a checked function called with a secret in a parameter its
/// signature calls public is checked with that parameter secret. A function
/// whose symbol is missing (the optimizer inlined it everywhere and dropped
/// it) or ambiguous is reported unchecked with a note, and is not a failure:
/// its code, if any, was checked inside whichever checked function it was
/// inlined into.
pub fn check_image(bytes: &[u8], functions: &[FunctionSpec]) -> Result<Report, String> {
    let image = Image::parse(bytes)?;
    let arch = image.arch();
    let convention = image.convention();

    let mut checked: HashMap<String, FunctionSpec> = functions
        .iter()
        .map(|f| (f.symbol.clone(), f.clone()))
        .collect();
    let resolve = |address: u64| image.resolve_call(address);

    // Locate each function once.
    let located: Vec<Located<'_>> = functions
        .iter()
        .map(|function| (function.clone(), image.function_code(&function.symbol)))
        .collect();

    // Propagate secret arguments between checked functions until nothing
    // changes; taint only grows, so this terminates.
    for _ in 0..=functions.len() {
        let mut changed = false;
        let snapshot = checked.clone();
        let targets = CallTargets {
            resolve: &resolve,
            checked: &snapshot,
        };
        for (spec, code) in &located {
            let Ok((address, code)) = code else { continue };
            let current = &snapshot[&spec.symbol];
            let result = check_code_inner(arch, convention, code, *address, current, &targets);
            for (callee, taint) in result.call_taint {
                let Some(callee) = checked.get_mut(&callee) else {
                    continue;
                };
                let Some(params) = callee.params.as_mut() else {
                    continue;
                };
                for (param, &secret) in params.iter_mut().zip(&taint) {
                    if secret && !param.secret {
                        param.secret = true;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    let targets = CallTargets {
        resolve: &resolve,
        checked: &checked,
    };
    let mut reports = Vec::new();
    for (spec, code) in &located {
        let current = &checked[&spec.symbol];
        let mut report = match code {
            Ok((address, code)) => check_code(arch, convention, code, *address, current, &targets),
            Err(reason) => FunctionReport {
                name: spec.name.clone(),
                symbol: spec.symbol.clone(),
                address: None,
                size: 0,
                instructions: 0,
                violations: Vec::new(),
                addresses_checked: false,
                notes: vec![reason.clone()],
            },
        };
        let raised: Vec<usize> = current
            .params
            .iter()
            .flatten()
            .zip(spec.params.iter().flatten())
            .enumerate()
            .filter(|(_, (now, declared))| now.secret && !declared.secret)
            .map(|(i, _)| i + 1)
            .collect();
        if !raised.is_empty() {
            report.notes.push(format!(
                "checked with parameter(s) {raised:?} secret: a checked caller passes a secret \
                 there"
            ));
        }
        reports.push(report);
    }
    Ok(Report {
        arch,
        functions: reports,
    })
}
