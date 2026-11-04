//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The `AArch64` rules: decoding with `yaxpeax-arm`, and rules (a)-(c) on
//!   every instruction of the function's range. Rule (d), secret-dependent
//!   addresses, is not implemented for `AArch64`.

use yaxpeax_arch::{Decoder, U8Reader};
use yaxpeax_arm::armv8::a64::{InstDecoder, Instruction, Opcode, Operand};

use crate::{CallTargets, CodeResult, FunctionSpec, Rule, Violation};

/// Instructions whose latency depends on their operands on `AArch64` cores:
/// integer division (not in Arm's list of instructions with data-independent
/// timing under `FEAT_DIT`, and documented with operand-dependent latency in
/// the cores' software optimisation guides), and floating-point division and
/// square root (likewise).
pub const VARIABLE_TIME: &[Opcode] = &[Opcode::UDIV, Opcode::SDIV, Opcode::FDIV, Opcode::FSQRT];

fn branch_target(instruction: &Instruction, address: u64) -> Option<u64> {
    instruction
        .operands
        .iter()
        .find_map(|operand| match operand {
            Operand::PCOffset(offset) => Some(address.wrapping_add(*offset as u64)),
            _ => None,
        })
}

pub(crate) fn check(
    code: &[u8],
    address: u64,
    _function: &FunctionSpec,
    calls: &CallTargets<'_>,
) -> CodeResult {
    let decoder = InstDecoder::default();
    let mut violations = Vec::new();
    let end = address + code.len() as u64;
    let mut instructions = 0;
    for (i, word) in code.chunks(4).enumerate() {
        let at_address = address + 4 * i as u64;
        instructions += 1;
        let mut reader = U8Reader::new(word);
        let decoded = decoder.decode(&mut reader);
        let Ok(instruction) = decoded else {
            violations.push(Violation {
                rule: Rule::Undecodable,
                address: at_address,
                disassembly: format!(".word {word:02x?}"),
                detail: String::new(),
            });
            continue;
        };
        let at = |rule, detail: String| Violation {
            rule,
            address: at_address,
            disassembly: instruction.to_string(),
            detail,
        };
        let allowed =
            |name: &str| calls.allowed(name) || calls.allowed(name.trim_start_matches('_'));
        match instruction.opcode {
            Opcode::Bcc(_) | Opcode::CBZ | Opcode::CBNZ | Opcode::TBZ | Opcode::TBNZ => {
                violations.push(at(Rule::ConditionalBranch, String::new()));
            }
            Opcode::BR | Opcode::BRAA | Opcode::BRAAZ | Opcode::BRAB | Opcode::BRABZ => {
                violations.push(at(Rule::IndirectBranch, String::new()));
            }
            Opcode::BLR | Opcode::BLRAA | Opcode::BLRAAZ | Opcode::BLRAB | Opcode::BLRABZ => {
                violations.push(at(Rule::Call, "indirect call".to_string()));
            }
            Opcode::SVC | Opcode::HVC | Opcode::SMC => {
                violations.push(at(Rule::Call, "system call".to_string()));
            }
            Opcode::BL => match branch_target(&instruction, at_address) {
                Some(target) => match (calls.resolve)(target) {
                    Some(name) if allowed(&name) => {}
                    Some(name) => violations.push(at(
                        Rule::Call,
                        format!("`{name}` is not covered by this check"),
                    )),
                    None => {
                        violations.push(at(Rule::Call, format!("unresolved target {target:#x}")));
                    }
                },
                None => violations.push(at(Rule::Call, "unresolved target".to_string())),
            },
            Opcode::B => {
                if let Some(target) = branch_target(&instruction, at_address) {
                    if let Some(name) = (calls.resolve)(target) {
                        if !allowed(&name) {
                            violations.push(at(
                                Rule::Call,
                                format!(
                                    "tail call to `{name}`, which is not covered by this check"
                                ),
                            ));
                        }
                    } else if !(address..end).contains(&target) {
                        violations.push(at(
                            Rule::Call,
                            format!("jump out of the function to unresolved {target:#x}"),
                        ));
                    }
                }
            }
            opcode if VARIABLE_TIME.contains(&opcode) => {
                violations.push(at(Rule::VariableTime, String::new()));
            }
            _ => {}
        }
    }
    CodeResult {
        violations,
        instructions,
        addresses_checked: false,
        notes: vec![
            "secret-dependent addresses not checked: rule (d) is implemented for x86-64 only"
                .to_string(),
        ],
        call_taint: std::collections::HashMap::default(),
    }
}
