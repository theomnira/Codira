//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Reads a built library or object file: where each function symbol's code
//!   is, and which function a call target (a symbol or an ELF PLT stub)
//!   names.

use std::collections::HashMap;

use object::{
    Architecture, BinaryFormat, Object, ObjectSection, ObjectSymbol, ObjectSymbolTable,
    RelocationTarget, SymbolKind,
};

use crate::{Arch, CallingConvention};

/// One function symbol: its address and the extent of its code.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Symbol {
    address: u64,
    len: u64,
    /// File offset of `address`.
    offset: u64,
}

/// A parsed library or object file.
pub struct Image<'a> {
    bytes: &'a [u8],
    arch: Arch,
    format: BinaryFormat,
    by_name: HashMap<String, Vec<Symbol>>,
    /// Function symbols and resolved PLT stubs, by address.
    by_address: HashMap<u64, String>,
}

impl<'a> Image<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Image<'a>, String> {
        let file = object::File::parse(bytes).map_err(|e| format!("not an object file: {e}"))?;
        let arch = match file.architecture() {
            Architecture::X86_64 => Arch::X86_64,
            Architecture::Aarch64 => Arch::Aarch64,
            other => {
                return Err(format!(
                    "the machine-code constant-time check supports x86-64 and AArch64, not \
                     {other:?}"
                ))
            }
        };

        // Function symbols per section, from the symbol table and the
        // dynamic symbol table.
        let mut by_section: HashMap<object::SectionIndex, Vec<(u64, u64, String)>> = HashMap::new();
        for symbol in file.symbols().chain(file.dynamic_symbols()) {
            if symbol.kind() != SymbolKind::Text {
                continue;
            }
            let (Some(section), Ok(name)) = (symbol.section_index(), symbol.name()) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            by_section.entry(section).or_default().push((
                symbol.address(),
                symbol.size(),
                name.to_owned(),
            ));
        }

        let mut by_name: HashMap<String, Vec<Symbol>> = HashMap::new();
        let mut by_address = HashMap::new();
        for (index, mut symbols) in by_section {
            let Ok(section) = file.section_by_index(index) else {
                continue;
            };
            let Some((file_offset, file_size)) = section.file_range() else {
                continue;
            };
            let start = section.address();
            let end = start + section.size().min(file_size);
            symbols.sort();
            symbols.dedup();
            let mut addresses: Vec<u64> = symbols.iter().map(|s| s.0).collect();
            addresses.dedup();
            for (address, size, name) in symbols {
                if address < start || address >= end {
                    continue;
                }
                // Without a recorded size (Mach-O, COFF) a function runs to
                // the next symbol or the end of its section.
                let stop = if size > 0 {
                    (address + size).min(end)
                } else {
                    addresses
                        .iter()
                        .copied()
                        .find(|&a| a > address)
                        .unwrap_or(end)
                        .min(end)
                };
                by_address.entry(address).or_insert_with(|| name.clone());
                let entry = by_name.entry(name).or_default();
                let symbol = Symbol {
                    address,
                    len: stop - address,
                    offset: file_offset + (address - start),
                };
                if !entry.contains(&symbol) {
                    entry.push(symbol);
                }
            }
        }

        if file.format() == BinaryFormat::Elf {
            for (stub, name) in plt_stubs(&file, arch) {
                by_address.entry(stub).or_insert(name);
            }
        }

        Ok(Image {
            bytes,
            arch,
            format: file.format(),
            by_name,
            by_address,
        })
    }

    pub fn arch(&self) -> Arch {
        self.arch
    }

    pub fn convention(&self) -> CallingConvention {
        match (self.arch, self.format) {
            (Arch::Aarch64, _) => CallingConvention::Aapcs64,
            (Arch::X86_64, BinaryFormat::Coff | BinaryFormat::Pe) => CallingConvention::Win64,
            (Arch::X86_64, _) => CallingConvention::SysV,
        }
    }

    /// The address and bytes of the code of the function named `symbol`
    /// (allowing for Mach-O's leading underscore), or why there is none.
    pub fn function_code(&self, symbol: &str) -> Result<(u64, &'a [u8]), String> {
        let symbols = self
            .by_name
            .get(symbol)
            .or_else(|| self.by_name.get(&format!("_{symbol}")))
            .ok_or_else(|| {
                "not checked: the library has no symbol for it (the optimizer inlined it into \
                 its callers and removed it, or the target keeps no symbol table); code inlined \
                 into another checked function was checked there, code inlined anywhere else was \
                 not"
                .to_string()
            })?;
        let [symbol] = symbols.as_slice() else {
            return Err(format!(
                "not checked: the library has {} symbols named `{symbol}`",
                symbols.len()
            ));
        };
        let start = usize::try_from(symbol.offset).map_err(|e| e.to_string())?;
        let len = usize::try_from(symbol.len).map_err(|e| e.to_string())?;
        let code = self
            .bytes
            .get(start..start + len)
            .ok_or_else(|| "not checked: its symbol lies outside the file".to_string())?;
        Ok((symbol.address, code))
    }

    /// The function a call or jump to `address` reaches, if known.
    pub fn resolve_call(&self, address: u64) -> Option<String> {
        self.by_address.get(&address).cloned()
    }
}

/// The ELF PLT stubs of `file`: stub address to the symbol whose GOT slot
/// the stub jumps through.
fn plt_stubs(file: &object::File<'_>, arch: Arch) -> Vec<(u64, String)> {
    // GOT slot address to the name of the symbol a dynamic relocation fills
    // it with.
    let mut slots: HashMap<u64, String> = HashMap::new();
    if let (Some(relocations), Some(dynsym)) =
        (file.dynamic_relocations(), file.dynamic_symbol_table())
    {
        for (offset, relocation) in relocations {
            if let RelocationTarget::Symbol(index) = relocation.target() {
                if let Ok(name) = dynsym.symbol_by_index(index).and_then(|s| s.name()) {
                    if !name.is_empty() {
                        slots.insert(offset, name.to_owned());
                    }
                }
            }
        }
    }
    if slots.is_empty() {
        return Vec::new();
    }

    let mut stubs = Vec::new();
    for section in file.sections() {
        let Ok(name) = section.name() else { continue };
        if !name.starts_with(".plt") {
            continue;
        }
        let Ok(data) = section.data() else { continue };
        let base = section.address();
        match arch {
            Arch::X86_64 => {
                use iced_x86::{Code, Decoder, DecoderOptions, Instruction};
                let mut decoder = Decoder::with_ip(64, data, base, DecoderOptions::NONE);
                let mut previous: Option<Instruction> = None;
                let mut instruction = Instruction::default();
                while decoder.can_decode() {
                    decoder.decode_out(&mut instruction);
                    if instruction.code() == Code::Jmp_rm64
                        && instruction.is_ip_rel_memory_operand()
                    {
                        if let Some(name) = slots.get(&instruction.ip_rel_memory_address()) {
                            stubs.push((instruction.ip(), name.clone()));
                            // An IBT stub starts with `endbr64`.
                            if let Some(p) = previous.filter(|p| p.code() == Code::Endbr64) {
                                stubs.push((p.ip(), name.clone()));
                            }
                        }
                    }
                    previous = Some(instruction);
                }
            }
            Arch::Aarch64 => {
                // `adrp x16, page; ldr x17, [x16, #off]; add x16, x16, #off;
                // br x17`, optionally after `bti c`.
                let word = |i: usize| -> Option<u32> {
                    data.get(i..i + 4)
                        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                };
                let mut i = 0;
                while let (Some(adrp), Some(ldr)) = (word(i), word(i + 4)) {
                    let address = base + i as u64;
                    if let (Some(page), Some(offset)) = (adrp_x16(adrp, address), ldr_x17_x16(ldr))
                    {
                        if let Some(name) = slots.get(&(page + offset)) {
                            stubs.push((address, name.clone()));
                            if i >= 4 && word(i - 4) == Some(0xd503_245f) {
                                stubs.push((address - 4, name.clone()));
                            }
                        }
                    }
                    i += 4;
                }
            }
        }
    }
    stubs
}

/// The page an `adrp x16, page` at `address` computes.
fn adrp_x16(word: u32, address: u64) -> Option<u64> {
    if word & 0x9f00_001f != 0x9000_0010 {
        return None;
    }
    let immlo = u64::from((word >> 29) & 0x3);
    let immhi = u64::from((word >> 5) & 0x7_ffff);
    let imm = ((immhi << 2) | immlo) << 12;
    // Sign-extend the 33-bit offset.
    let imm = ((imm << 31) as i64 >> 31) as u64;
    Some((address & !0xfff).wrapping_add(imm))
}

/// The offset of an `ldr x17, [x16, #offset]`.
fn ldr_x17_x16(word: u32) -> Option<u64> {
    if word & 0xffc0_03ff != 0xf940_0211 {
        return None;
    }
    Some(u64::from((word >> 10) & 0xfff) * 8)
}
