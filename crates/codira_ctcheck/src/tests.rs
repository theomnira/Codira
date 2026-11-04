//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Unit tests of the rules on hand-assembled instruction sequences
//!   (encodings checked with `llvm-mc -show-encoding`).

use std::collections::HashMap;

use crate::{
    check_code, Arch, CallTargets, CallingConvention, FunctionReport, FunctionSpec, Param,
    ParamClass, Rule,
};

const BASE: u64 = 0x1000;

fn int(secret: bool) -> Param {
    Param {
        class: ParamClass::Integer,
        secret,
    }
}

fn spec(symbol: &str, params: Option<Vec<Param>>) -> FunctionSpec {
    FunctionSpec {
        symbol: symbol.to_string(),
        name: symbol.to_string(),
        params,
        secret_result: true,
    }
}

/// Checks `code` as function `f` at `BASE`; `symbols` names call targets,
/// `checked` lists the other functions the check covers.
fn run(
    arch: Arch,
    code: &[u8],
    params: Option<Vec<Param>>,
    symbols: &[(u64, &str)],
    checked: &[&str],
) -> FunctionReport {
    let symbols: HashMap<u64, String> = symbols.iter().map(|&(a, n)| (a, n.to_string())).collect();
    let checked: HashMap<String, FunctionSpec> = checked
        .iter()
        .map(|&n| (n.to_string(), spec(n, Some(vec![int(true)]))))
        .collect();
    let resolve = |address: u64| symbols.get(&address).cloned();
    let targets = CallTargets {
        resolve: &resolve,
        checked: &checked,
    };
    let convention = match arch {
        Arch::X86_64 => CallingConvention::SysV,
        Arch::Aarch64 => CallingConvention::Aapcs64,
    };
    check_code(arch, convention, code, BASE, &spec("f", params), &targets)
}

fn x86(code: &[u8], params: Vec<Param>) -> FunctionReport {
    run(Arch::X86_64, code, Some(params), &[], &[])
}

fn rules(report: &FunctionReport) -> Vec<Rule> {
    report.violations.iter().map(|v| v.rule).collect()
}

const RET: u8 = 0xc3;

#[test]
fn a_cmov_select_passes() {
    // mov rax, rsi; cmp rdi, rsi; cmova rax, rdi; ret
    let code = [
        0x48, 0x89, 0xf0, 0x48, 0x39, 0xf7, 0x48, 0x0f, 0x47, 0xc7, RET,
    ];
    let report = x86(&code, vec![int(true), int(true)]);
    assert!(report.passed(), "{report:#?}");
    assert!(report.addresses_checked);
    assert_eq!(report.instructions, 4);
}

#[test]
fn a_conditional_branch_is_caught_and_its_condition_traced() {
    // test dil, 1; jne +2; mov eax, esi; ret
    let code = [0x40, 0xf6, 0xc7, 0x01, 0x75, 0x02, 0x89, 0xf0, RET];
    let report = x86(&code, vec![int(true), int(false)]);
    assert_eq!(rules(&report), [Rule::ConditionalBranch], "{report:#?}");
    let violation = &report.violations[0];
    assert_eq!(violation.address, BASE + 4);
    assert!(violation.disassembly.starts_with("jne"), "{violation}");
    assert_eq!(violation.detail, "the condition is derived from a secret");

    // The same branch on a public parameter is still reported: rule (a)
    // forbids every conditional branch.
    let report = x86(&code, vec![int(false), int(true)]);
    assert_eq!(rules(&report), [Rule::ConditionalBranch]);
    assert!(report.violations[0].detail.contains("not found to depend"));
}

#[test]
fn division_and_square_root_are_variable_time() {
    // mov rax, rdi; xor edx, edx; div rsi; ret
    let code = [0x48, 0x89, 0xf8, 0x31, 0xd2, 0x48, 0xf7, 0xf6, RET];
    let report = x86(&code, vec![int(true), int(true)]);
    assert_eq!(rules(&report), [Rule::VariableTime], "{report:#?}");
    assert!(report.violations[0].disassembly.starts_with("div"));
    assert_eq!(
        report.violations[0].detail,
        "an operand is derived from a secret"
    );
    let report = x86(&code, vec![int(false), int(false)]);
    assert_eq!(rules(&report), [Rule::VariableTime], "public operands too");
    assert!(report.violations[0].detail.contains("not found"));

    // sqrtsd xmm0, xmm1; ret
    let report = x86(&[0xf2, 0x0f, 0x51, 0xc1, RET], vec![]);
    assert_eq!(rules(&report), [Rule::VariableTime]);

    // imul rax, rdi; ret: multiplication is assumed constant-time.
    let report = x86(&[0x48, 0x0f, 0xaf, 0xc7, RET], vec![int(true)]);
    assert!(report.passed(), "{report:#?}");
}

#[test]
fn an_indirect_jump_is_caught() {
    // jmp rax
    let report = x86(&[0xff, 0xe0], vec![]);
    assert_eq!(rules(&report), [Rule::IndirectBranch]);
}

#[test]
fn calls_are_allowed_only_to_checked_functions() {
    // call +0 (to BASE + 5); ret
    let code = [0xe8, 0x00, 0x00, 0x00, 0x00, RET];
    let target = BASE + 5;
    let params = Some(vec![int(true)]);

    let report = run(
        Arch::X86_64,
        &code,
        params.clone(),
        &[(target, "helper")],
        &[],
    );
    assert_eq!(rules(&report), [Rule::Call]);
    assert!(
        report.violations[0].detail.contains("`helper`"),
        "{report:#?}"
    );

    let report = run(Arch::X86_64, &code, params.clone(), &[], &[]);
    assert_eq!(rules(&report), [Rule::Call]);
    assert!(report.violations[0].detail.contains("unresolved"));

    let report = run(
        Arch::X86_64,
        &code,
        params,
        &[(target, "helper")],
        &["helper"],
    );
    assert!(report.passed(), "{report:#?}");
}

#[test]
fn a_secret_index_is_caught() {
    // mov rax, [rdx + 8*rdi]; ret
    let code = [0x48, 0x8b, 0x04, 0xfa, RET];
    let report = x86(&code, vec![int(true), int(false), int(false)]);
    assert_eq!(rules(&report), [Rule::SecretAddress], "{report:#?}");
    assert!(report.violations[0].detail.contains("rdi"));

    // A public index into a table whose pointer is public passes.
    let report = x86(&code, vec![int(false), int(true), int(false)]);
    assert!(report.passed(), "{report:#?}");
}

#[test]
fn taint_flows_through_stack_slots() {
    // mov [rsp - 8], rdi; mov rax, [rsp - 8]; mov rax, [rsi + rax]; ret
    let code = [
        0x48, 0x89, 0x7c, 0x24, 0xf8, 0x48, 0x8b, 0x44, 0x24, 0xf8, 0x48, 0x8b, 0x04, 0x06, RET,
    ];
    let report = x86(&code, vec![int(true), int(false)]);
    assert_eq!(rules(&report), [Rule::SecretAddress], "{report:#?}");
    assert_eq!(report.violations[0].address, BASE + 10);

    // push rdi; pop rax; mov rax, [rsi + rax]; ret
    let code = [0x57, 0x58, 0x48, 0x8b, 0x04, 0x06, RET];
    let report = x86(&code, vec![int(true), int(false)]);
    assert_eq!(rules(&report), [Rule::SecretAddress], "{report:#?}");

    let report = x86(&code, vec![int(false), int(true)]);
    assert_eq!(rules(&report), [Rule::SecretAddress], "rsi is the base");

    let report = x86(&code, vec![int(false), int(false)]);
    assert!(report.passed());
}

#[test]
fn a_zeroing_idiom_clears_taint() {
    // xor edi, edi; mov rax, [rsi + rdi]; ret
    let code = [0x31, 0xff, 0x48, 0x8b, 0x04, 0x3e, RET];
    let report = x86(&code, vec![int(true), int(false)]);
    assert!(report.passed(), "{report:#?}");
}

#[test]
fn a_secret_repeat_count_is_variable_time() {
    // mov rcx, rdx; rep movsb; ret
    let code = [0x48, 0x89, 0xd1, 0xf3, 0xa4, RET];
    let report = x86(&code, vec![int(false), int(false), int(true)]);
    assert_eq!(rules(&report), [Rule::VariableTime], "{report:#?}");
    assert!(report.violations[0].detail.contains("rcx"));
    let report = x86(&code, vec![int(false), int(false), int(false)]);
    assert!(report.passed(), "{report:#?}");
}

#[test]
fn unmapped_signatures_skip_only_the_address_rule() {
    // mov rax, [rdx + 8*rdi]; test dil, 1; jne +0; ret
    let code = [
        0x48, 0x8b, 0x04, 0xfa, 0x40, 0xf6, 0xc7, 0x01, 0x75, 0x00, RET,
    ];
    let report = run(Arch::X86_64, &code, None, &[], &[]);
    assert_eq!(rules(&report), [Rule::ConditionalBranch]);
    assert!(!report.addresses_checked);
    assert!(report.notes[0].contains("not checked"));
}

fn a64(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

const A64_RET: u32 = 0xd65f_03c0;

#[test]
fn aarch64_csel_passes() {
    // csel x0, x1, x2, eq; madd x0, x1, x2, x3; ret
    let code = a64(&[0x9a82_0020, 0x9b02_0c20, A64_RET]);
    let report = run(Arch::Aarch64, &code, Some(vec![]), &[], &[]);
    assert!(report.passed(), "{report:#?}");
    assert!(!report.addresses_checked);
}

#[test]
fn aarch64_branches_division_and_calls_are_caught() {
    for (word, rule, text) in [
        (0xb400_0040, Rule::ConditionalBranch, "cbz"),
        (0x5400_0041, Rule::ConditionalBranch, "b.ne"),
        (0x3618_0040, Rule::ConditionalBranch, "tbz"),
        (0x9ac1_0800, Rule::VariableTime, "udiv"),
        (0x1e61_c020, Rule::VariableTime, "fsqrt"),
        (0xd61f_0200, Rule::IndirectBranch, "br"),
    ] {
        let report = run(Arch::Aarch64, &a64(&[word, A64_RET]), None, &[], &[]);
        assert_eq!(rules(&report), [rule], "{text}: {report:#?}");
        assert!(
            report.violations[0].disassembly.starts_with(text),
            "{}",
            report.violations[0]
        );
    }

    // bl #8 (to BASE + 8): allowed only to a checked function.
    let code = a64(&[0x9400_0002, A64_RET, A64_RET]);
    let report = run(Arch::Aarch64, &code, None, &[(BASE + 8, "g")], &[]);
    assert_eq!(rules(&report), [Rule::Call]);
    let report = run(Arch::Aarch64, &code, None, &[(BASE + 8, "g")], &["g"]);
    assert!(report.passed(), "{report:#?}");
}
