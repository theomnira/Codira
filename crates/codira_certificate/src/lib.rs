//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Proof certificates for compiled Codira libraries: the record `codira build
//!   --certify` writes next to each `.codiralib`, and the checker `codira
//!   certify` runs on it without the compiler's front end.
//!
//! # What a certificate holds
//!
//! For every function of the library: its symbol, a SHA-256 of its source
//! text (with the file and byte range it came from), the byte range of its
//! machine code in the library file with a SHA-256 of those bytes, and every
//! proof obligation the contract verifier generated for it -- each as the
//! self-contained SMT-LIB2 queries Z3 decided, with the answer that made the
//! obligation proven (`unsat`) or the verdict that left it unproven. The
//! library file as a whole is hashed too.
//!
//! Since format version 2 (`codira-certificate/2`) every function also
//! carries its *inferred semantic envelope* (`allocation: none`, `panic:
//! true`, ...: `lab/rfcs/0001-semantic-envelope.md`), the value of each
//! checked key as the effect analysis computed it from the source at build
//! time. Version 1 certificates are still read and checked; they record no
//! envelopes, and a check says so. Any other format is rejected.
//!
//! It is a sidecar file, `<library>.cert.json`, not a section inside the
//! library: the certificate contains hashes of the library's bytes, which a
//! section inside those bytes would change (a fixed point no linker offers);
//! a sidecar also leaves the library byte-for-byte what the linker wrote,
//! so the code that is certified is the code that runs. `codira build --exe`
//! carries the certificates inside the executable's payload, next to the
//! libraries they describe.
//!
//! # What checking establishes, exactly
//!
//! [`check`] uses this crate, `object` (to read the library's symbol table)
//! and Z3 -- nothing of the parser, type checker, verifier or code
//! generator. It confirms:
//!
//! 1. the library file is the one the certificate was made for (whole-file
//!    hash);
//! 2. each function's code bytes hash to the recorded value, and, where the
//!    library has a symbol table, the function's symbol starts where the
//!    certificate says its code starts -- so a changed instruction is pinned on
//!    the function it belongs to;
//! 3. every obligation recorded as proven is proven *again*: each of its
//!    queries is parsed (only declarations and assertions are accepted), and
//!    Z3, under the checker's own deterministic `rlimit`, answers `unsat`.
//!    Where the path condition alone is unsatisfiable the obligation holds only
//!    vacuously, and that is reported;
//! 4. optionally, each function's source range still hashes to the recorded
//!    value.
//!
//! Recorded envelopes are *not* re-derived by [`check`]: that needs the
//! effect analysis, which is part of the front end. [`check`] only confirms
//! that a version 2 certificate records one for every function, and reports
//! them as not re-derived. `codira certify` re-derives them from the source
//! with the front end and compares them with [`check_envelopes`]; any
//! difference fails the certificate. That step establishes that the
//! recorded envelopes are the ones today's analysis computes for today's
//! source -- a source-level analysis result, recorded at build time. It is
//! not a statement about the machine code (the envelope is bound to the code
//! only by sitting in the same certificate, under hashes), and its trusted
//! base adds the front end and the effect analysis to the one below.
//!
//! What it does *not* establish:
//!
//! * that the queries are the right ones. The verification-condition generator
//!   (`codira_hir::verify`) turned source and contract into them; the checker
//!   re-proves the recorded obligations, not that they capture what the source
//!   means, nor that the machine code implements the source (the queries are
//!   about the source program, and the code is bound to them only by sitting in
//!   the same certificate, under hashes);
//! * anything about functions or code the certificate does not list, or about
//!   the Codira runtime that loads the library;
//! * authenticity: the certificate is not signed. Anyone who can rewrite both
//!   the library and its certificate consistently can produce a pair that
//!   checks. Detecting a tampered *library* relies on the certificate having
//!   been kept apart from it (or signed by other means).
//!
//! The trusted computing base of a passing check is therefore: Z3, this
//! crate and `object`, SHA-256, and the verification-condition generator
//! that produced the queries.
//!
//! Translation validation (`codira_tv`) does not run during `codira build`
//! today, so certificates carry no translation-validation results; the
//! certificate says so rather than leaving the field out.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    path::Path,
};

use codira_smt::{CheckOutcome, Context, UnknownReason};
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The `format` of the certificates this crate writes. Version 2 added each
/// function's inferred semantic envelope ([`FunctionRecord::envelope`]) and
/// [`Certificate::envelope_basis`].
pub const FORMAT: &str = "codira-certificate/2";

/// The previous format, still read and checked: it records no envelopes,
/// and a check of one says so.
pub const FORMAT_V1: &str = "codira-certificate/1";

/// What a recorded envelope is, written into every certificate that carries
/// envelopes.
pub const ENVELOPE_BASIS: &str =
    "source-level effect analysis (Codira Semantic Envelope, lab/rfcs/0001-semantic-envelope.md) \
     as computed at build time; codira certify re-derives it from the source with the compiler's \
     front end. It is not a statement about the machine code, and nothing enforces it at run \
     time";

/// Appended to a library's file name to name its certificate:
/// `mod.codiralib` -> `mod.codiralib.cert.json`.
pub const SIDECAR_SUFFIX: &str = ".cert.json";

/// What the certificate says about translation validation.
pub const TRANSLATION_VALIDATION_NOT_RUN: &str =
    "not run: codira build does not invoke translation validation, so no optimizer rewrite of \
     this library is certified";

/// The certificate path for a library path.
pub fn sidecar_path(library: &Path) -> std::path::PathBuf {
    let mut name = library.as_os_str().to_owned();
    name.push(SIDECAR_SUFFIX);
    name.into()
}

/// SHA-256 of `bytes`, as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A proof certificate for one library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    /// Always [`FORMAT`].
    pub format: String,
    /// The compiler that wrote it, e.g. `codira 0.6.0-dev`.
    pub compiler: String,
    /// The Z3 that decided the obligations at build time.
    pub solver: String,
    /// The `rlimit` budget each query was decided under.
    pub rlimit: u32,
    pub artifact: ArtifactRecord,
    /// See [`TRANSLATION_VALIDATION_NOT_RUN`].
    pub translation_validation: String,
    /// [`ENVELOPE_BASIS`] in a [`FORMAT`] certificate; absent in a
    /// [`FORMAT_V1`] one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_basis: Option<String>,
    pub functions: Vec<FunctionRecord>,
}

/// The library file the certificate describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    /// The file name, e.g. `mod.codiralib`.
    pub file: String,
    pub size: u64,
    pub sha256: String,
}

/// One function of the library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionRecord {
    /// The function's path in its package, e.g. `geometry::area`.
    pub name: String,
    /// The symbol its machine code carries in the library.
    pub symbol: String,
    pub source: SourceRecord,
    /// Where its machine code is; `None` when it has none of its own in the
    /// library (see `code_note`).
    pub code: Option<CodeRecord>,
    /// Why `code` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_note: Option<String>,
    pub obligations: Vec<ObligationRecord>,
    /// Its inferred semantic envelope, key to value (`"allocation":
    /// "none"`, ...), for every checked key: see [`ENVELOPE_BASIS`]. Every
    /// function of a [`FORMAT`] certificate has one; a [`FORMAT_V1`]
    /// certificate has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<BTreeMap<String, String>>,
}

/// A function's envelope as re-derived from the source, to compare with the
/// recorded one ([`check_envelopes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedEnvelope {
    /// As [`FunctionRecord::name`].
    pub name: String,
    /// As [`SourceRecord::file`] and [`SourceRecord::line`].
    pub file: String,
    pub line: u32,
    pub values: BTreeMap<String, String>,
}

/// Where a function's source text is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRecord {
    /// Relative to the package's source directory.
    pub file: String,
    /// 1-based line of the signature.
    pub line: u32,
    /// Byte range of the whole function in `file`.
    pub start: u64,
    pub end: u64,
    pub sha256: String,
}

/// A byte range of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CodeRange {
    pub offset: u64,
    pub len: u64,
}

/// [`CodeRange`] and the hash of the bytes in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeRecord {
    pub offset: u64,
    pub len: u64,
    pub sha256: String,
}

/// One proof obligation and how it was decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObligationRecord {
    /// `postcondition`, `precondition of f`, `division by zero`, ...
    pub kind: String,
    /// The clause as written.
    pub clause: String,
    /// 1-based line of the site the obligation arises at.
    pub line: u32,
    pub proven: bool,
    /// Why it is not proven: the counterexample or the reason it is
    /// unknown. `None` when proven.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unproven_because: Option<String>,
    /// Enforced by a run-time check instead (`@runtime_checked`).
    pub runtime_checked: bool,
    pub queries: Vec<Query>,
}

/// One Z3 query: premise asserted first, negated goal last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub smtlib2: String,
    /// What Z3 answered at build time: `unsat` (the goal holds there),
    /// `sat` or `unknown`.
    pub expected: String,
}

/// Where each function symbol's code is in an object file: name to the
/// byte ranges of every symbol of that name (more than one when names
/// repeat).
pub type SymbolMap = HashMap<String, Vec<CodeRange>>;

/// Reads `bytes` as an object file (PE/COFF, ELF or Mach-O) and returns the
/// file range of every function symbol in its symbol table.
///
/// A symbol's extent is its recorded size where the format has one (ELF),
/// and otherwise runs to the next function symbol in the same section or to
/// the end of the section's data -- so on PE and Mach-O a range also covers
/// the alignment padding after the function and any code the compiler
/// placed there without a symbol. Returns an empty map for a file without a
/// symbol table, and `Err` for bytes that are not an object file.
pub fn locate_functions(bytes: &[u8]) -> Result<SymbolMap, String> {
    use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

    let file = object::File::parse(bytes).map_err(|e| format!("not an object file: {e}"))?;
    let mut by_section: HashMap<object::SectionIndex, Vec<(u64, u64, String)>> = HashMap::new();
    for symbol in file.symbols() {
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

    let mut map = SymbolMap::new();
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
        let mut addresses: Vec<u64> = symbols.iter().map(|s| s.0).collect();
        addresses.dedup();
        for (address, size, name) in symbols {
            if address < start || address >= end {
                continue;
            }
            let next = addresses
                .iter()
                .copied()
                .find(|&a| a > address)
                .unwrap_or(end)
                .min(end);
            let stop = if size > 0 {
                (address + size).min(end)
            } else {
                next
            };
            map.entry(name).or_default().push(CodeRange {
                offset: file_offset + (address - start),
                len: stop - address,
            });
        }
    }
    Ok(map)
}

/// Looks `symbol` up in `map`, allowing for Mach-O's leading underscore.
pub fn lookup<'a>(map: &'a SymbolMap, symbol: &str) -> Option<&'a [CodeRange]> {
    map.get(symbol)
        .or_else(|| map.get(&format!("_{symbol}")))
        .map(Vec::as_slice)
}

impl CodeRecord {
    /// The range and hash of `range` in `bytes`; `None` if it lies outside.
    pub fn of(bytes: &[u8], range: CodeRange) -> Option<CodeRecord> {
        let slice = slice(bytes, range)?;
        Some(CodeRecord {
            offset: range.offset,
            len: range.len,
            sha256: sha256_hex(slice),
        })
    }

    pub fn range(&self) -> CodeRange {
        CodeRange {
            offset: self.offset,
            len: self.len,
        }
    }
}

fn slice(bytes: &[u8], range: CodeRange) -> Option<&[u8]> {
    let start = usize::try_from(range.offset).ok()?;
    let len = usize::try_from(range.len).ok()?;
    bytes.get(start..start.checked_add(len)?)
}

impl Certificate {
    /// Reads a certificate from its JSON text.
    pub fn from_json(text: &str) -> Result<Certificate, String> {
        let certificate: Certificate =
            serde_json::from_str(text).map_err(|e| format!("not a certificate: {e}"))?;
        if !certificate.is_readable() {
            return Err(format!(
                "certificate format `{}` is neither `{FORMAT}` nor `{FORMAT_V1}`",
                certificate.format
            ));
        }
        Ok(certificate)
    }

    /// Whether this crate reads the certificate's format.
    pub fn is_readable(&self) -> bool {
        self.format == FORMAT || self.format == FORMAT_V1
    }

    /// Whether the format records envelopes (version 2 and later).
    pub fn records_envelopes(&self) -> bool {
        self.format == FORMAT
    }

    /// The certificate as JSON text.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a certificate always serializes")
    }
}

/// How [`check`] runs.
#[derive(Debug, Clone)]
pub struct CheckOptions {
    /// The `rlimit` every query is decided under -- the checker's own,
    /// never the certificate's.
    pub rlimit: u32,
    /// The package's source directory, to compare source hashes against.
    pub source_dir: Option<std::path::PathBuf>,
}

impl Default for CheckOptions {
    fn default() -> Self {
        CheckOptions {
            rlimit: codira_smt::DEFAULT_RLIMIT,
            source_dir: None,
        }
    }
}

/// What [`check`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The library's file name, as the certificate records it.
    pub artifact: String,
    /// Checks that failed. Any entry means the library does not match its
    /// certificate, or a recorded proof does not hold.
    pub failures: Vec<String>,
    /// Obligations the certificate itself records as not proven.
    pub unproven: Vec<String>,
    /// Proven obligations whose path condition is unsatisfiable.
    pub vacuous: Vec<String>,
    /// Facts worth knowing that are neither failures nor proofs.
    pub notes: Vec<String>,
    pub functions: usize,
    /// Functions whose code bytes were hashed and matched.
    pub code_checked: usize,
    pub obligations_proven: usize,
    pub queries_checked: usize,
    pub sources_checked: usize,
    /// Functions whose certificate records an envelope.
    pub envelopes_recorded: usize,
    /// Recorded envelopes re-derived from the source and found equal
    /// ([`check_envelopes`]); [`check`] alone re-derives none.
    pub envelopes_rederived: usize,
    /// Why the recorded envelopes were not re-derived, when they were not
    /// ([`check`] sets it; [`check_envelopes`] clears it).
    pub envelopes_not_rederived: Option<String>,
}

impl Report {
    /// Whether every check passed.
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "certificate for {}:", self.artifact)?;
        for failure in &self.failures {
            writeln!(f, "  FAILED: {failure}")?;
        }
        for unproven in &self.unproven {
            writeln!(f, "  unproven: {unproven}")?;
        }
        for vacuous in &self.vacuous {
            writeln!(f, "  vacuous: {vacuous}")?;
        }
        for note in &self.notes {
            writeln!(f, "  note: {note}")?;
        }
        writeln!(
            f,
            "  {} function(s); code of {} matched; {} obligation(s) re-proven with {} Z3 \
             quer{}; {} recorded as unproven; {} source range(s) checked",
            self.functions,
            self.code_checked,
            self.obligations_proven,
            self.queries_checked,
            if self.queries_checked == 1 {
                "y"
            } else {
                "ies"
            },
            self.unproven.len(),
            self.sources_checked,
        )?;
        if self.envelopes_recorded > 0 {
            match &self.envelopes_not_rederived {
                Some(reason) => writeln!(
                    f,
                    "  {} semantic envelope(s) recorded, NOT re-derived: {reason}",
                    self.envelopes_recorded
                )?,
                None => writeln!(
                    f,
                    "  {} semantic envelope(s) recorded, {} re-derived from the source and equal",
                    self.envelopes_recorded, self.envelopes_rederived
                )?,
            }
        }
        write!(
            f,
            "  {}",
            if self.passed() {
                "certificate holds"
            } else {
                "certificate REJECTED"
            }
        )
    }
}

/// The SMT-LIB2 commands a recorded query may contain. Everything else --
/// `push`, `reset`, `set-option` (which can change solver-wide settings),
/// definitions -- is refused, so a query is exactly a list of declarations
/// and assertions.
const ALLOWED_COMMANDS: &[&str] = &[
    "set-info",
    "set-logic",
    "declare-fun",
    "declare-const",
    "assert",
    "check-sat",
];

/// The head of every top-level command of an SMT-LIB2 script.
fn command_heads(text: &str) -> Result<Vec<String>, String> {
    let mut heads = Vec::new();
    let mut depth = 0usize;
    let mut chars = text.chars().peekable();
    let mut expecting_head = false;
    while let Some(c) = chars.next() {
        match c {
            ';' => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                    }
                    Some('"') => break,
                    Some(_) => {}
                    None => return Err("unterminated string literal".to_string()),
                }
            },
            '|' => loop {
                match chars.next() {
                    Some('|') => break,
                    Some(_) => {}
                    None => return Err("unterminated quoted symbol".to_string()),
                }
            },
            '(' => {
                depth += 1;
                expecting_head = depth == 1;
            }
            ')' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| "unbalanced `)`".to_string())?;
            }
            c if c.is_whitespace() => {}
            c => {
                if depth == 0 {
                    return Err(format!("unexpected `{c}` outside a command"));
                }
                if expecting_head {
                    let mut head = String::from(c);
                    while let Some(&n) = chars.peek() {
                        if n.is_whitespace() || n == '(' || n == ')' {
                            break;
                        }
                        head.push(n);
                        chars.next();
                    }
                    heads.push(head);
                    expecting_head = false;
                }
            }
        }
    }
    if depth != 0 {
        return Err("unbalanced `(`".to_string());
    }
    Ok(heads)
}

/// How a recorded query decided when checked again.
enum Recheck {
    /// `unsat`; `vacuous` when the premise alone is already unsat.
    Unsat {
        vacuous: bool,
    },
    Sat,
    Unknown(String),
}

fn recheck(text: &str, rlimit: u32) -> Result<Recheck, String> {
    let heads = command_heads(text)?;
    if let Some(bad) = heads
        .iter()
        .find(|h| !ALLOWED_COMMANDS.contains(&h.as_str()))
    {
        return Err(format!("the query contains a `{bad}` command"));
    }
    let asserts = heads.iter().filter(|h| *h == "assert").count();
    if asserts != 2 {
        return Err(format!(
            "the query has {asserts} assertion(s); a recorded obligation has exactly two (premise, \
             negated goal)"
        ));
    }
    let ctx = Context::new();
    ctx.set_rlimit(rlimit);
    let assertions = ctx.parse_smtlib2(text)?;
    if assertions.len() != 2 {
        return Err(format!(
            "the query parsed to {} assertion(s), not two",
            assertions.len()
        ));
    }
    let solver = ctx.solver();
    solver.assert(assertions[0]);
    solver.assert(assertions[1]);
    Ok(match solver.check_detailed() {
        CheckOutcome::Unsat => {
            let premise = ctx.solver();
            premise.assert(assertions[0]);
            Recheck::Unsat {
                vacuous: premise.check_detailed() == CheckOutcome::Unsat,
            }
        }
        CheckOutcome::Sat => Recheck::Sat,
        CheckOutcome::Unknown(UnknownReason::ResourceLimit) => {
            Recheck::Unknown(format!("resource limit {rlimit} exhausted"))
        }
        CheckOutcome::Unknown(UnknownReason::Other(reason)) => Recheck::Unknown(reason),
    })
}

/// Checks `certificate` against the library bytes `artifact`. See the crate
/// docs for exactly what a passing report establishes.
pub fn check(certificate: &Certificate, artifact: &[u8], options: &CheckOptions) -> Report {
    let mut report = Report {
        artifact: certificate.artifact.file.clone(),
        functions: certificate.functions.len(),
        ..Report::default()
    };

    if !certificate.is_readable() {
        report.failures.push(format!(
            "certificate format `{}` is neither `{FORMAT}` nor `{FORMAT_V1}`",
            certificate.format
        ));
        return report;
    }
    if certificate.records_envelopes() {
        for function in &certificate.functions {
            if function.envelope.is_some() {
                report.envelopes_recorded += 1;
            } else {
                report.failures.push(format!(
                    "`{}`: a `{FORMAT}` certificate records an envelope for every function, and \
                     this one has none",
                    function.name
                ));
            }
        }
        if report.envelopes_recorded > 0 {
            report.envelopes_not_rederived = Some(
                "re-deriving an envelope needs the compiler's front end and the source, which \
                 this check does not use (`codira certify` does)"
                    .to_string(),
            );
        }
    } else {
        report.notes.push(format!(
            "the certificate is `{}`, which records no semantic envelopes",
            certificate.format
        ));
    }

    // 1. The whole file.
    let actual = sha256_hex(artifact);
    if actual != certificate.artifact.sha256 || artifact.len() as u64 != certificate.artifact.size {
        report.failures.push(format!(
            "{} is not the file this certificate was made for (SHA-256 {actual}, {} bytes; \
             certificate: {}, {} bytes)",
            certificate.artifact.file,
            artifact.len(),
            certificate.artifact.sha256,
            certificate.artifact.size
        ));
    }

    // 2. Each function's code.
    let symbols = match locate_functions(artifact) {
        Ok(map) => map,
        Err(error) => {
            report.notes.push(format!(
                "could not read the library's symbol table ({error}); code ranges are taken from \
                 the certificate"
            ));
            SymbolMap::new()
        }
    };
    if symbols.is_empty() {
        report.notes.push(
            "the library has no function symbols; code ranges are taken from the certificate"
                .to_string(),
        );
    }
    for function in &certificate.functions {
        let Some(code) = &function.code else {
            if let Some(note) = &function.code_note {
                report
                    .notes
                    .push(format!("`{}` has no code range: {note}", function.name));
            }
            continue;
        };
        match slice(artifact, code.range()) {
            None => report.failures.push(format!(
                "`{}`: the certificate places its code at {}..{}, past the end of the file",
                function.name,
                code.offset,
                code.offset + code.len
            )),
            Some(bytes) if sha256_hex(bytes) != code.sha256 => report.failures.push(format!(
                "`{}`: its machine code (bytes {}..{}) differs from the certified code",
                function.name,
                code.offset,
                code.offset + code.len
            )),
            Some(_) => report.code_checked += 1,
        }
        if !symbols.is_empty() {
            match lookup(&symbols, &function.symbol) {
                Some(ranges) if ranges.iter().any(|r| r.offset == code.offset) => {}
                Some(ranges) => report.failures.push(format!(
                    "`{}`: its symbol `{}` is at byte {} of the library, the certificate says {}",
                    function.name, function.symbol, ranges[0].offset, code.offset
                )),
                None => report.failures.push(format!(
                    "`{}`: the library has no symbol `{}`",
                    function.name, function.symbol
                )),
            }
        }
    }

    // 3. Every recorded proof, again.
    for function in &certificate.functions {
        for obligation in &function.obligations {
            let what = format!(
                "`{}` line {}: {} `{}`",
                function.name, obligation.line, obligation.kind, obligation.clause
            );
            if !obligation.proven {
                let how = if obligation.runtime_checked {
                    " (checked at run time instead)"
                } else {
                    ""
                };
                report.unproven.push(format!(
                    "{what}: {}{how}",
                    obligation
                        .unproven_because
                        .as_deref()
                        .unwrap_or("not proven")
                ));
                continue;
            }
            if obligation.queries.is_empty() {
                report.failures.push(format!(
                    "{what} is recorded as proven but has no query to check"
                ));
                continue;
            }
            let mut holds = true;
            let mut vacuous = true;
            for query in &obligation.queries {
                if query.expected != "unsat" {
                    report.failures.push(format!(
                        "{what} is recorded as proven, but one of its queries was `{}`",
                        query.expected
                    ));
                    holds = false;
                    continue;
                }
                report.queries_checked += 1;
                match recheck(&query.smtlib2, options.rlimit) {
                    Ok(Recheck::Unsat { vacuous: v }) => vacuous &= v,
                    Ok(Recheck::Sat) => {
                        report.failures.push(format!(
                            "{what}: Z3 finds the query satisfiable -- it does not hold"
                        ));
                        holds = false;
                    }
                    Ok(Recheck::Unknown(reason)) => {
                        report
                            .failures
                            .push(format!("{what}: Z3 could not re-prove it ({reason})"));
                        holds = false;
                    }
                    Err(error) => {
                        report
                            .failures
                            .push(format!("{what}: malformed query: {error}"));
                        holds = false;
                    }
                }
            }
            if holds {
                report.obligations_proven += 1;
                if vacuous {
                    report.vacuous.push(format!(
                        "{what} holds only because the path to it is unreachable"
                    ));
                }
            }
        }
    }

    // 4. Sources, when asked for.
    if let Some(dir) = &options.source_dir {
        for function in &certificate.functions {
            let path = dir.join(&function.source.file);
            let range = CodeRange {
                offset: function.source.start,
                len: function.source.end.saturating_sub(function.source.start),
            };
            match std::fs::read(&path) {
                Err(error) => report.failures.push(format!(
                    "`{}`: cannot read its source {}: {error}",
                    function.name,
                    path.display()
                )),
                Ok(bytes) => match slice(&bytes, range) {
                    Some(text) if sha256_hex(text) == function.source.sha256 => {
                        report.sources_checked += 1;
                    }
                    _ => report.failures.push(format!(
                        "`{}`: {} line {} is not the source the certificate was made from",
                        function.name, function.source.file, function.source.line
                    )),
                },
            }
        }
    }

    report
}

/// Compares every envelope `certificate` records with `derived`, the
/// envelopes the effect analysis computes from the source now, and records
/// the outcome in `report` (from [`check`] on the same certificate).
///
/// A function is matched by its name, file and signature line. A recorded
/// envelope that differs from the derived one in any key, or whose function
/// is not found in the source, is a failure: either the certificate was
/// edited, or the source (or the analysis) is not the one it was made with.
/// This comparison is plain data; computing `derived` is the caller's work
/// and needs the compiler's front end, so it is outside the trusted base of
/// [`check`].
pub fn check_envelopes(
    certificate: &Certificate,
    derived: &[DerivedEnvelope],
    report: &mut Report,
) {
    if !certificate.records_envelopes() {
        return;
    }
    report.envelopes_not_rederived = None;
    for function in &certificate.functions {
        let Some(recorded) = &function.envelope else {
            continue;
        };
        let found = derived.iter().find(|d| {
            d.name == function.name
                && d.file == function.source.file
                && d.line == function.source.line
        });
        let Some(found) = found else {
            report.failures.push(format!(
                "`{}`: no function of that name at {} line {} in the source, so its recorded \
                 envelope cannot be re-derived",
                function.name, function.source.file, function.source.line
            ));
            continue;
        };
        let keys: std::collections::BTreeSet<&String> =
            recorded.keys().chain(found.values.keys()).collect();
        let differences: Vec<String> = keys
            .into_iter()
            .filter(|k| recorded.get(*k) != found.values.get(*k))
            .map(|k| {
                let show = |v: Option<&String>| v.map_or("(absent)", String::as_str).to_string();
                format!(
                    "`{k}` is recorded as {} but the source gives {}",
                    show(recorded.get(k)),
                    show(found.values.get(k))
                )
            })
            .collect();
        if differences.is_empty() {
            report.envelopes_rederived += 1;
        } else {
            report.failures.push(format!(
                "`{}`: its recorded envelope is not the one the source has: {}",
                function.name,
                differences.join("; ")
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_heads_skip_strings_symbols_and_comments() {
        let heads = command_heads(
            "; a comment (with parens\n(set-info :status unknown)\n(declare-fun |a (b| () \
             Bool)\n(assert (= |a (b| true))(assert \"(push)\")\n(check-sat)\n",
        )
        .unwrap();
        assert_eq!(
            heads,
            ["set-info", "declare-fun", "assert", "assert", "check-sat"]
        );
        assert!(command_heads("(assert true").is_err());
        assert!(command_heads("assert").is_err());
    }

    #[test]
    fn only_declarations_and_assertions_are_accepted() {
        let script = "(declare-fun x () Bool)(push 1)(assert x)(assert (not x))";
        match recheck(script, 1_000_000) {
            Err(error) => assert!(error.contains("`push`"), "{error}"),
            Ok(_) => panic!("a `push` must be refused"),
        }
        // A query that is not premise + negated goal is refused rather than
        // decided: a lone `(assert false)` would otherwise "prove" anything.
        assert!(recheck("(assert false)", 1_000_000).is_err());
    }

    #[test]
    fn a_vacuous_proof_is_told_apart() {
        let vacuous = "(declare-fun x () Bool)(assert (and x (not x)))(assert (not x))";
        assert!(matches!(
            recheck(vacuous, 1_000_000),
            Ok(Recheck::Unsat { vacuous: true })
        ));
        let real = "(declare-fun x () Bool)(assert x)(assert (not x))";
        assert!(matches!(
            recheck(real, 1_000_000),
            Ok(Recheck::Unsat { vacuous: false })
        ));
        let false_claim = "(declare-fun x () Bool)(assert true)(assert (not x))";
        assert!(matches!(recheck(false_claim, 1_000_000), Ok(Recheck::Sat)));
    }

    fn envelope(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// A certificate for `bytes` with one function and no obligations.
    fn certificate_for(
        bytes: &[u8],
        format: &str,
        recorded: Option<&[(&str, &str)]>,
    ) -> Certificate {
        Certificate {
            format: format.to_string(),
            compiler: "codira test".to_string(),
            solver: "z3".to_string(),
            rlimit: 1,
            artifact: ArtifactRecord {
                file: "mod.codiralib".to_string(),
                size: bytes.len() as u64,
                sha256: sha256_hex(bytes),
            },
            translation_validation: TRANSLATION_VALIDATION_NOT_RUN.to_string(),
            envelope_basis: (format == FORMAT).then(|| ENVELOPE_BASIS.to_string()),
            functions: vec![FunctionRecord {
                name: "scale".to_string(),
                symbol: "scale".to_string(),
                source: SourceRecord {
                    file: "mod.code".to_string(),
                    line: 2,
                    start: 0,
                    end: 0,
                    sha256: sha256_hex(b""),
                },
                code: None,
                code_note: Some("test".to_string()),
                obligations: Vec::new(),
                envelope: recorded.map(envelope),
            }],
        }
    }

    #[test]
    fn recorded_envelopes_are_compared_with_the_derived_ones() {
        let bytes = b"not an object file";
        let pure = [("allocation", "none"), ("panic", "false")];
        let certificate = certificate_for(bytes, FORMAT, Some(&pure));
        let text = certificate.to_json();
        assert!(text.contains("\"envelope\""), "{text}");
        assert_eq!(Certificate::from_json(&text).unwrap(), certificate);

        let mut report = check(&certificate, bytes, &CheckOptions::default());
        assert!(report.passed(), "{report}");
        assert_eq!(report.envelopes_recorded, 1);
        assert!(report.envelopes_not_rederived.is_some());
        assert!(report.to_string().contains("NOT re-derived"), "{report}");

        let derived = |pairs: &[(&str, &str)], line| DerivedEnvelope {
            name: "scale".to_string(),
            file: "mod.code".to_string(),
            line,
            values: envelope(pairs),
        };
        check_envelopes(&certificate, &[derived(&pure, 2)], &mut report);
        assert!(report.passed(), "{report}");
        assert_eq!(report.envelopes_rederived, 1);
        assert!(
            report
                .to_string()
                .contains("1 re-derived from the source and equal"),
            "{report}"
        );

        // The source now allocates: the recorded envelope no longer holds.
        let mut report = check(&certificate, bytes, &CheckOptions::default());
        let wider = [("allocation", "any"), ("panic", "false")];
        check_envelopes(&certificate, &[derived(&wider, 2)], &mut report);
        assert!(!report.passed());
        assert!(
            report.failures[0]
                .contains("`allocation` is recorded as none but the source gives any"),
            "{report}"
        );

        // Not found where the certificate says it is.
        let mut report = check(&certificate, bytes, &CheckOptions::default());
        check_envelopes(&certificate, &[derived(&pure, 3)], &mut report);
        assert!(
            report.failures[0].contains("cannot be re-derived"),
            "{report}"
        );
    }

    #[test]
    fn a_version_2_function_without_an_envelope_fails_and_version_1_is_still_read() {
        let bytes = b"not an object file";
        let missing = certificate_for(bytes, FORMAT, None);
        let report = check(&missing, bytes, &CheckOptions::default());
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.contains("`scale`") && f.contains("has none")),
            "{report}"
        );

        let old = certificate_for(bytes, FORMAT_V1, None);
        let text = old.to_json();
        assert!(!text.contains("envelope"), "{text}");
        let old = Certificate::from_json(&text).unwrap();
        let mut report = check(&old, bytes, &CheckOptions::default());
        assert!(report.passed(), "{report}");
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("records no semantic envelopes")),
            "{report}"
        );
        check_envelopes(&old, &[], &mut report);
        assert!(report.passed(), "{report}");

        let mut unknown = old.clone();
        unknown.format = "codira-certificate/9".to_string();
        assert!(Certificate::from_json(&unknown.to_json())
            .unwrap_err()
            .contains("neither"));
    }

    #[test]
    fn sidecar_names_the_library_file() {
        assert_eq!(
            sidecar_path(Path::new("target/mod.codiralib")),
            Path::new("target/mod.codiralib.cert.json")
        );
    }
}
