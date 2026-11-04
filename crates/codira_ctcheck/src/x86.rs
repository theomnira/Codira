//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The x86-64 rules: decoding with `iced-x86`, rules (a)-(c) on every
//!   instruction of the function's range, and rule (d) by a forward taint
//!   analysis over registers, the flags and stack slots.

use std::collections::{BTreeSet, HashMap};

use iced_x86::{
    Code, Decoder, DecoderOptions, FlowControl, Formatter, Instruction, InstructionInfoFactory,
    IntelFormatter, Mnemonic, OpAccess, OpKind, Register,
};

use crate::{
    CallTargets, CallingConvention, CodeResult, FunctionSpec, Param, ParamClass, Rule, Violation,
};

/// Instructions whose latency depends on their operands on mainstream
/// x86-64 cores. Integer division (Intel's and AMD's optimisation manuals and
/// Agner Fog's instruction tables give latency ranges, and neither is in
/// Intel's "data operand independent timing" list); floating-point division
/// and square root, scalar and packed, legacy and VEX/EVEX (latency ranges in
/// the same tables; also not in Intel's list); and the x87 division, square
/// root, remainder and transcendental instructions (microcoded, with
/// operand-dependent iteration counts).
pub const VARIABLE_TIME: &[Mnemonic] = &[
    Mnemonic::Div,
    Mnemonic::Idiv,
    Mnemonic::Divss,
    Mnemonic::Divsd,
    Mnemonic::Divps,
    Mnemonic::Divpd,
    Mnemonic::Vdivss,
    Mnemonic::Vdivsd,
    Mnemonic::Vdivps,
    Mnemonic::Vdivpd,
    Mnemonic::Vdivsh,
    Mnemonic::Vdivph,
    Mnemonic::Sqrtss,
    Mnemonic::Sqrtsd,
    Mnemonic::Sqrtps,
    Mnemonic::Sqrtpd,
    Mnemonic::Vsqrtss,
    Mnemonic::Vsqrtsd,
    Mnemonic::Vsqrtps,
    Mnemonic::Vsqrtpd,
    Mnemonic::Vsqrtsh,
    Mnemonic::Vsqrtph,
    Mnemonic::Fdiv,
    Mnemonic::Fdivp,
    Mnemonic::Fdivr,
    Mnemonic::Fdivrp,
    Mnemonic::Fidiv,
    Mnemonic::Fidivr,
    Mnemonic::Fsqrt,
    Mnemonic::Fprem,
    Mnemonic::Fprem1,
    Mnemonic::Fsin,
    Mnemonic::Fcos,
    Mnemonic::Fsincos,
    Mnemonic::Fptan,
    Mnemonic::Fpatan,
    Mnemonic::Fyl2x,
    Mnemonic::Fyl2xp1,
    Mnemonic::F2xm1,
];

const SYSV_INTEGER: [Register; 6] = [
    Register::RDI,
    Register::RSI,
    Register::RDX,
    Register::RCX,
    Register::R8,
    Register::R9,
];
const WIN64_INTEGER: [Register; 4] = [Register::RCX, Register::RDX, Register::R8, Register::R9];
const RESULTS: [Register; 4] = [Register::RAX, Register::RDX, Register::ZMM0, Register::ZMM1];

/// Where each parameter is passed: its registers, or its stack slot (an
/// offset from the stack pointer at entry).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Location {
    Registers(Vec<Register>),
    Stack { offset: i64, len: i64 },
}

fn vector(n: usize) -> Register {
    // ZMM0..ZMM31 are consecutive in iced's `Register`.
    Register::try_from(Register::ZMM0 as usize + n).unwrap_or(Register::ZMM0)
}

/// The locations of `params` under `convention`; `None` for a parameter
/// this does not know how to place.
fn locations(convention: CallingConvention, params: &[Param]) -> Vec<Option<Location>> {
    let mut out = Vec::with_capacity(params.len());
    match convention {
        CallingConvention::SysV | CallingConvention::Aapcs64 => {
            let (mut gpr, mut xmm) = (0usize, 0usize);
            // The first stack argument is above the return address.
            let mut stack = 8i64;
            for param in params {
                let location = match param.class {
                    ParamClass::Integer if gpr < SYSV_INTEGER.len() => {
                        gpr += 1;
                        Location::Registers(vec![SYSV_INTEGER[gpr - 1]])
                    }
                    ParamClass::Integer128 if gpr + 1 < SYSV_INTEGER.len() => {
                        gpr += 2;
                        Location::Registers(vec![SYSV_INTEGER[gpr - 2], SYSV_INTEGER[gpr - 1]])
                    }
                    ParamClass::Float if xmm < 8 => {
                        xmm += 1;
                        Location::Registers(vec![vector(xmm - 1)])
                    }
                    ParamClass::Integer128 => {
                        // 16-byte aligned on the stack.
                        stack = (stack + 15) & !15;
                        stack += 16;
                        Location::Stack {
                            offset: stack - 16,
                            len: 16,
                        }
                    }
                    ParamClass::Integer | ParamClass::Float => {
                        stack += 8;
                        Location::Stack {
                            offset: stack - 8,
                            len: 8,
                        }
                    }
                };
                out.push(Some(location));
            }
        }
        CallingConvention::Win64 => {
            for (i, param) in params.iter().enumerate() {
                let location = match param.class {
                    // Passed by reference: the pointer is in the slot, the
                    // secret is in memory.
                    ParamClass::Integer128 => None,
                    ParamClass::Integer if i < 4 => {
                        Some(Location::Registers(vec![WIN64_INTEGER[i]]))
                    }
                    ParamClass::Float if i < 4 => Some(Location::Registers(vec![vector(i)])),
                    // Above the return address and the 32-byte shadow space.
                    _ => Some(Location::Stack {
                        offset: 40 + 8 * (i as i64 - 4),
                        len: 8,
                    }),
                };
                out.push(location);
            }
        }
    }
    out
}

/// A set of full registers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Registers([u64; 4]);

impl Registers {
    fn bit(reg: Register) -> (usize, u64) {
        let index = reg.full_register() as usize;
        (index / 64 % 4, 1 << (index % 64))
    }
    fn contains(&self, reg: Register) -> bool {
        if reg == Register::None {
            return false;
        }
        let (word, bit) = Self::bit(reg);
        self.0[word] & bit != 0
    }
    fn set(&mut self, reg: Register, tainted: bool) {
        if reg == Register::None {
            return;
        }
        let (word, bit) = Self::bit(reg);
        if tainted {
            self.0[word] |= bit;
        } else {
            self.0[word] &= !bit;
        }
    }
    fn union(&mut self, other: &Registers) -> bool {
        let before = *self;
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a |= b;
        }
        *self != before
    }
}

/// The taint state before an instruction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    registers: Registers,
    flags: bool,
    /// Offset of the stack pointer from its value at entry; `None` once it
    /// is lost.
    sp: Option<i64>,
    /// `rbp` as an offset from the entry stack pointer, while it is a frame
    /// pointer.
    frame: Option<i64>,
    /// Tainted stack bytes, by offset from the entry stack pointer.
    stack: BTreeSet<i64>,
    /// A tainted value was stored where the stack offset was unknown.
    stack_wild: bool,
    /// A tainted value was stored to memory that is not the stack.
    memory: bool,
}

impl State {
    fn join(&mut self, other: &State) -> bool {
        let before = self.clone();
        self.registers.union(&other.registers);
        self.flags |= other.flags;
        if self.sp != other.sp {
            self.sp = None;
        }
        if self.frame != other.frame {
            self.frame = None;
        }
        self.stack.extend(other.stack.iter().copied());
        self.stack_wild |= other.stack_wild;
        self.memory |= other.memory;
        *self != before
    }

    /// The entry-relative stack offset `base + displacement` addresses, if
    /// `base` is the stack or frame pointer and its offset is known.
    fn stack_offset(&self, base: Register, index: Register, displacement: i64) -> StackRef {
        let base_offset = match base {
            Register::RSP | Register::ESP => self.sp,
            Register::RBP | Register::EBP => match self.frame {
                Some(frame) => Some(frame),
                None => return StackRef::NotStack,
            },
            _ => return StackRef::NotStack,
        };
        match (base_offset, index) {
            (Some(offset), Register::None) => StackRef::Known(offset + displacement),
            _ => StackRef::Unknown,
        }
    }

    fn stack_tainted(&self, offset: i64, len: i64) -> bool {
        self.stack_wild
            || self
                .stack
                .range(offset..offset + len.max(1))
                .next()
                .is_some()
    }
}

enum StackRef {
    Known(i64),
    Unknown,
    NotStack,
}

struct Decoded {
    instructions: Vec<Instruction>,
    index: HashMap<u64, usize>,
}

fn decode(code: &[u8], address: u64) -> Decoded {
    let mut decoder = Decoder::with_ip(64, code, address, DecoderOptions::NONE);
    let mut instructions = Vec::new();
    let mut index = HashMap::new();
    let mut instruction = Instruction::default();
    while decoder.can_decode() {
        decoder.decode_out(&mut instruction);
        index.insert(instruction.ip(), instructions.len());
        instructions.push(instruction);
    }
    Decoded {
        instructions,
        index,
    }
}

fn disassemble(instruction: &Instruction) -> String {
    let mut formatter = IntelFormatter::new();
    formatter
        .options_mut()
        .set_space_after_operand_separator(true);
    let mut text = String::new();
    formatter.format(instruction, &mut text);
    text
}

/// Whether `instruction` sets its destination to zero whatever its inputs
/// (`xor eax, eax` and friends).
fn is_zero_idiom(instruction: &Instruction) -> bool {
    let same = |a: u32, b: u32| {
        instruction.op_kind(a) == OpKind::Register
            && instruction.op_kind(b) == OpKind::Register
            && instruction.op_register(a) == instruction.op_register(b)
    };
    match instruction.mnemonic() {
        Mnemonic::Xor
        | Mnemonic::Sub
        | Mnemonic::Pxor
        | Mnemonic::Xorps
        | Mnemonic::Xorpd
        | Mnemonic::Psubb
        | Mnemonic::Psubw
        | Mnemonic::Psubd
        | Mnemonic::Psubq => instruction.op_count() == 2 && same(0, 1),
        Mnemonic::Vpxor
        | Mnemonic::Vxorps
        | Mnemonic::Vxorpd
        | Mnemonic::Vpxord
        | Mnemonic::Vpxorq => instruction.op_count() == 3 && same(1, 2),
        _ => false,
    }
}

fn is_read(access: OpAccess) -> bool {
    matches!(
        access,
        OpAccess::Read | OpAccess::CondRead | OpAccess::ReadWrite | OpAccess::ReadCondWrite
    )
}

fn is_write(access: OpAccess) -> bool {
    matches!(
        access,
        OpAccess::Write | OpAccess::CondWrite | OpAccess::ReadWrite | OpAccess::ReadCondWrite
    )
}

/// What the transfer function found at one instruction.
#[derive(Default)]
struct Step {
    /// Whether any input of the instruction (registers, flags, memory) is
    /// tainted.
    input_tainted: bool,
    secret_address: Option<String>,
    secret_count: bool,
}

struct Analysis<'c, 'a> {
    convention: CallingConvention,
    calls: &'c CallTargets<'a>,
    factory: InstructionInfoFactory,
    call_taint: HashMap<String, Vec<bool>>,
}

impl Analysis<'_, '_> {
    fn entry_state(&self, params: &[Param]) -> State {
        let mut state = State {
            sp: Some(0),
            ..State::default()
        };
        for (param, location) in params.iter().zip(locations(self.convention, params)) {
            if !param.secret {
                continue;
            }
            match location {
                Some(Location::Registers(registers)) => {
                    for register in registers {
                        state.registers.set(register, true);
                    }
                }
                Some(Location::Stack { offset, len }) => {
                    state.stack.extend(offset..offset + len);
                }
                None => {}
            }
        }
        state
    }

    /// Applies `instruction` to `state`.
    fn step(&mut self, instruction: &Instruction, state: &mut State) -> Step {
        let mut step = Step::default();
        let info = self.factory.info(instruction);
        let used_registers = info.used_registers().to_vec();
        let used_memory = info.used_memory().to_vec();

        // Inputs: registers read, flags read, memory read.
        let mut input = used_registers
            .iter()
            .filter(|r| is_read(r.access()))
            .any(|r| state.registers.contains(r.register()));
        if instruction.rflags_read() != 0 {
            input |= state.flags;
        }
        for memory in &used_memory {
            let address_tainted =
                state.registers.contains(memory.base()) || state.registers.contains(memory.index());
            if memory.access() == OpAccess::NoMemAccess {
                // `lea`, `nop [..]`: an address computed, nothing accessed.
                input |= address_tainted;
                continue;
            }
            if address_tainted && step.secret_address.is_none() {
                let register = if state.registers.contains(memory.base()) {
                    memory.base()
                } else {
                    memory.index()
                };
                step.secret_address = Some(format!("{register:?}").to_lowercase());
            }
            if is_read(memory.access()) {
                let len = memory.memory_size().size() as i64;
                input |= match state.stack_offset(
                    memory.base(),
                    memory.index(),
                    memory.displacement() as i64,
                ) {
                    StackRef::Known(offset) => state.stack_tainted(offset, len),
                    StackRef::Unknown => state.stack_wild || !state.stack.is_empty(),
                    StackRef::NotStack => state.memory,
                };
            }
        }
        if is_zero_idiom(instruction) {
            input = false;
        }
        step.input_tainted = input;
        if instruction.is_string_instruction()
            && (instruction.has_rep_prefix() || instruction.has_repne_prefix())
        {
            step.secret_count = state.registers.contains(Register::RCX);
        }

        // Calls: arguments, results.
        if matches!(
            instruction.flow_control(),
            FlowControl::Call | FlowControl::IndirectCall
        ) || self.tail_call_target(instruction).is_some()
        {
            self.call(instruction, state);
            return step;
        }

        // Outputs: memory written.
        for memory in &used_memory {
            if !is_write(memory.access()) {
                continue;
            }
            let len = memory.memory_size().size() as i64;
            let displacement = memory.displacement() as i64;
            match state.stack_offset(memory.base(), memory.index(), displacement) {
                StackRef::Known(offset) => {
                    for byte in offset..offset + len.max(1) {
                        if input {
                            state.stack.insert(byte);
                        } else if memory.access() == OpAccess::Write {
                            state.stack.remove(&byte);
                        }
                    }
                }
                StackRef::Unknown => state.stack_wild |= input,
                StackRef::NotStack => state.memory |= input,
            }
        }

        // The stack and frame pointers.
        let sp_written = used_registers
            .iter()
            .any(|r| is_write(r.access()) && r.register().full_register() == Register::RSP);
        let frame_written = used_registers
            .iter()
            .any(|r| is_write(r.access()) && r.register().full_register() == Register::RBP);
        let increment = i64::from(instruction.stack_pointer_increment());
        let new_sp = match instruction.code() {
            Code::Sub_rm64_imm8 | Code::Sub_rm64_imm32
                if instruction.op0_register() == Register::RSP =>
            {
                state.sp.map(|sp| sp - instruction.immediate(1) as i64)
            }
            Code::Add_rm64_imm8 | Code::Add_rm64_imm32
                if instruction.op0_register() == Register::RSP =>
            {
                state.sp.map(|sp| sp + instruction.immediate(1) as i64)
            }
            Code::Mov_rm64_r64 | Code::Mov_r64_rm64
                if instruction.op0_register() == Register::RSP
                    && instruction.op1_register() == Register::RBP =>
            {
                state.frame
            }
            Code::Leaveq => state.frame.map(|frame| frame + 8),
            _ if increment != 0 => state.sp.map(|sp| sp + increment),
            _ if sp_written => None,
            _ => state.sp,
        };
        let new_frame = match instruction.code() {
            Code::Mov_rm64_r64 | Code::Mov_r64_rm64
                if instruction.op0_register() == Register::RBP
                    && instruction.op1_register() == Register::RSP =>
            {
                state.sp
            }
            _ if frame_written => None,
            _ => state.frame,
        };

        // Outputs: registers and flags written.
        for register in &used_registers {
            let reg = register.register();
            if !is_write(register.access()) || matches!(reg.full_register(), Register::RSP) {
                continue;
            }
            // A 32- or 64-bit general register write replaces the whole
            // register; anything else (8/16-bit, vector, conditional)
            // merges with what was there.
            let replaces = register.access() == OpAccess::Write && reg.is_gpr() && reg.size() >= 4;
            if replaces {
                state.registers.set(reg, input);
            } else if input {
                state.registers.set(reg, true);
            }
        }
        if is_zero_idiom(instruction) && instruction.op_kind(0) == OpKind::Register {
            state.registers.set(instruction.op_register(0), false);
        }
        if instruction.rflags_modified() != 0 {
            state.flags = input;
        }
        if frame_written && new_frame.is_some() {
            state.registers.set(Register::RBP, false);
        }
        state.sp = new_sp;
        state.frame = new_frame;
        step
    }

    /// The target of a `jmp` that leaves the function (a tail call).
    fn tail_call_target(&self, instruction: &Instruction) -> Option<u64> {
        if instruction.flow_control() == FlowControl::UnconditionalBranch
            && (self.calls.resolve)(instruction.near_branch_target()).is_some()
        {
            Some(instruction.near_branch_target())
        } else {
            None
        }
    }

    fn call(&mut self, instruction: &Instruction, state: &mut State) {
        let target = match instruction.op0_kind() {
            OpKind::NearBranch64 | OpKind::NearBranch32 | OpKind::NearBranch16 => {
                (self.calls.resolve)(instruction.near_branch_target())
            }
            _ => None,
        };
        let callee = target.as_deref().and_then(|name| {
            self.calls
                .checked
                .get(name)
                .or_else(|| self.calls.checked.get(name.trim_start_matches('_')))
        });
        let mut any_argument = false;
        if let Some(callee) = callee {
            if let Some(params) = &callee.params {
                let taint: Vec<bool> = locations(self.convention, params)
                    .iter()
                    .map(|location| match location {
                        Some(Location::Registers(registers)) => {
                            registers.iter().any(|r| state.registers.contains(*r))
                        }
                        Some(Location::Stack { offset, len }) => {
                            state
                                .sp
                                .map_or(state.stack_wild || !state.stack.is_empty(), |sp| {
                                    // The callee's offsets are from its entry,
                                    // below which the return address was pushed.
                                    state.stack_tainted(sp - 8 + offset, *len)
                                })
                        }
                        None => false,
                    })
                    .collect();
                any_argument = taint.iter().any(|&t| t);
                let entry = self
                    .call_taint
                    .entry(callee.symbol.clone())
                    .or_insert_with(|| vec![false; taint.len()]);
                for (a, b) in entry.iter_mut().zip(taint) {
                    *a |= b;
                }
            }
        }
        // What a call returns: secret if the callee's result is, if a secret
        // went in, or if the callee is not known.
        let result = callee.is_none_or(|c| c.secret_result || any_argument);
        for register in RESULTS {
            state.registers.set(register, result);
        }
        state.flags = result;
        state.memory |= result && callee.is_none();
    }
}

/// Successor instruction addresses of `instruction`, and whether control
/// leaves the function there.
fn successors(instruction: &Instruction, analysis: &Analysis<'_, '_>) -> Vec<u64> {
    match instruction.flow_control() {
        FlowControl::Next | FlowControl::Call | FlowControl::IndirectCall => {
            vec![instruction.next_ip()]
        }
        FlowControl::ConditionalBranch | FlowControl::XbeginXabortXend => {
            let mut next = vec![instruction.next_ip()];
            if matches!(
                instruction.op0_kind(),
                OpKind::NearBranch64 | OpKind::NearBranch32 | OpKind::NearBranch16
            ) {
                next.push(instruction.near_branch_target());
            }
            next
        }
        FlowControl::UnconditionalBranch => {
            if analysis.tail_call_target(instruction).is_some() {
                Vec::new()
            } else {
                vec![instruction.near_branch_target()]
            }
        }
        FlowControl::IndirectBranch
        | FlowControl::Return
        | FlowControl::Interrupt
        | FlowControl::Exception => Vec::new(),
    }
}

pub(crate) fn check(
    convention: CallingConvention,
    code: &[u8],
    address: u64,
    function: &FunctionSpec,
    calls: &CallTargets<'_>,
) -> CodeResult {
    let decoded = decode(code, address);
    let mut analysis = Analysis {
        convention,
        calls,
        factory: InstructionInfoFactory::new(),
        call_taint: HashMap::new(),
    };
    let mut violations = Vec::new();
    let mut notes = Vec::new();
    let end = address + code.len() as u64;

    // Rules (a)-(c), and indirect branches, on every decoded instruction.
    for instruction in &decoded.instructions {
        let at = |rule, detail: String| Violation {
            rule,
            address: instruction.ip(),
            disassembly: disassemble(instruction),
            detail,
        };
        if instruction.is_invalid() {
            violations.push(at(Rule::Undecodable, String::new()));
            continue;
        }
        if VARIABLE_TIME.contains(&instruction.mnemonic()) {
            violations.push(at(Rule::VariableTime, String::new()));
        }
        match instruction.flow_control() {
            FlowControl::ConditionalBranch | FlowControl::XbeginXabortXend => {
                violations.push(at(Rule::ConditionalBranch, String::new()));
            }
            FlowControl::IndirectBranch => {
                violations.push(at(Rule::IndirectBranch, String::new()));
            }
            FlowControl::IndirectCall => {
                violations.push(at(Rule::Call, "indirect call".to_string()));
            }
            FlowControl::Call => {
                let is_near = matches!(
                    instruction.op0_kind(),
                    OpKind::NearBranch64 | OpKind::NearBranch32 | OpKind::NearBranch16
                );
                let target = is_near.then(|| instruction.near_branch_target());
                match target.and_then(|t| (calls.resolve)(t)) {
                    Some(name)
                        if calls.allowed(&name) || calls.allowed(name.trim_start_matches('_')) => {}
                    Some(name) => violations.push(at(
                        Rule::Call,
                        format!("`{name}` is not covered by this check"),
                    )),
                    None => violations.push(at(
                        Rule::Call,
                        match target {
                            Some(t) => format!("unresolved target {t:#x}"),
                            None => "unresolved target".to_string(),
                        },
                    )),
                }
            }
            FlowControl::UnconditionalBranch => {
                let target = instruction.near_branch_target();
                if let Some(name) = (calls.resolve)(target) {
                    if !(calls.allowed(&name) || calls.allowed(name.trim_start_matches('_'))) {
                        violations.push(at(
                            Rule::Call,
                            format!("tail call to `{name}`, which is not covered by this check"),
                        ));
                    }
                } else if !(address..end).contains(&target) {
                    violations.push(at(
                        Rule::Call,
                        format!("jump out of the function to unresolved {target:#x}"),
                    ));
                } else if !decoded.index.contains_key(&target) {
                    violations.push(at(
                        Rule::IndirectBranch,
                        "jump into the middle of an instruction".to_string(),
                    ));
                }
            }
            FlowControl::Interrupt if instruction.mnemonic() != Mnemonic::Int3 => {
                violations.push(at(Rule::Call, "software interrupt".to_string()));
            }
            _ => {}
        }
        if matches!(
            instruction.mnemonic(),
            Mnemonic::Syscall | Mnemonic::Sysenter
        ) {
            violations.push(at(Rule::Call, "system call".to_string()));
        }
    }

    // Rule (d): taint from the secret parameters, to a fixed point over the
    // control flow, then one more pass to report.
    let addresses_checked = match &function.params {
        Some(params) if !decoded.instructions.is_empty() => {
            let entry = analysis.entry_state(params);
            let mut states: Vec<Option<State>> = vec![None; decoded.instructions.len()];
            states[0] = Some(entry);
            let mut work = vec![0usize];
            while let Some(i) = work.pop() {
                let Some(mut state) = states[i].clone() else {
                    continue;
                };
                let instruction = decoded.instructions[i];
                analysis.step(&instruction, &mut state);
                for next in successors(&instruction, &analysis) {
                    let Some(&j) = decoded.index.get(&next) else {
                        continue;
                    };
                    let changed = match &mut states[j] {
                        Some(existing) => existing.join(&state),
                        slot @ None => {
                            *slot = Some(state.clone());
                            true
                        }
                    };
                    if changed {
                        work.push(j);
                    }
                }
            }
            for (i, instruction) in decoded.instructions.iter().enumerate() {
                let Some(mut state) = states[i].clone() else {
                    continue;
                };
                let step = analysis.step(instruction, &mut state);
                if let Some(register) = step.secret_address {
                    violations.push(Violation {
                        rule: Rule::SecretAddress,
                        address: instruction.ip(),
                        disassembly: disassemble(instruction),
                        detail: format!("`{register}` holds a value derived from a secret"),
                    });
                }
                if step.secret_count {
                    violations.push(Violation {
                        rule: Rule::VariableTime,
                        address: instruction.ip(),
                        disassembly: disassemble(instruction),
                        detail: "the repeat count in `rcx` is derived from a secret".to_string(),
                    });
                }
                if matches!(
                    instruction.flow_control(),
                    FlowControl::ConditionalBranch | FlowControl::XbeginXabortXend
                ) {
                    let detail = if step.input_tainted {
                        "the condition is derived from a secret"
                    } else {
                        "the condition was not found to depend on a secret register value"
                    };
                    if let Some(v) = violations.iter_mut().find(|v| {
                        v.address == instruction.ip() && v.rule == Rule::ConditionalBranch
                    }) {
                        v.detail = detail.to_string();
                    }
                }
            }
            true
        }
        Some(_) => true,
        None => {
            notes.push(
                "secret-dependent addresses not checked: a parameter or the result is not an \
                 integer, `bool` or float, so where its secrets are passed is not modelled"
                    .to_string(),
            );
            false
        }
    };

    violations.sort_by_key(|v| (v.address, v.rule));
    violations.dedup_by(|a, b| a.address == b.address && a.rule == b.rule);
    CodeResult {
        violations,
        instructions: decoded.instructions.len(),
        addresses_checked,
        notes,
        call_taint: analysis.call_taint,
    }
}
