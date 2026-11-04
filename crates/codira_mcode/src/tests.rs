use crate::aarch64::{Aarch64, Reg as A};
use crate::profile::{Objective, Profile};
use crate::x86::{Reg, X86_64};
use crate::{prove_equivalent, Block, Equivalence, Isa};

fn x86(text: &str, live_in: &[Reg], live_out: &[Reg]) -> Block<X86_64> {
    Block::parse(text, live_in.to_vec(), live_out.to_vec()).unwrap()
}

#[test]
fn x86_printing_round_trips_through_the_parser() {
    for t in [
        "mov rax, rbx",
        "mov rax, -5",
        "movabs rax, 81985529216486895",
        "add rax, rcx",
        "sub r9, 7",
        "imul rax, rbx",
        "imul rax, rbx, 9",
        "shl rdx, 3",
        "sar rdx, 63",
        "neg r11",
        "not r11",
        "lea rax, [rdi + 2*rsi + 8]",
        "lea rax, [rdi - 16]",
        "lea rax, [8*rdi]",
        "andn rax, rbx, rcx",
    ] {
        let i = X86_64::parse(t).unwrap();
        assert_eq!(i.to_string(), t);
        assert_eq!(X86_64::parse(&i.to_string()).unwrap(), i);
    }
}

#[test]
fn x86_parser_reads_llvm_objdump_spellings() {
    assert_eq!(
        X86_64::parse("lea    rax, [rdi + 2*rsi + 0x8]")
            .unwrap()
            .to_string(),
        "lea rax, [rdi + 2*rsi + 8]"
    );
    assert_eq!(
        X86_64::parse("lea rax, [rdi + rsi*4]").unwrap().to_string(),
        "lea rax, [rdi + 4*rsi]"
    );
    assert_eq!(
        X86_64::parse("add rax, 0xffffffffffffffff")
            .unwrap()
            .to_string(),
        "add rax, -1"
    );
    assert_eq!(
        X86_64::parse("imul rax, rax, 0x5").unwrap().to_string(),
        "imul rax, rax, 5"
    );
    assert!(
        X86_64::parse("add eax, ecx").is_err(),
        "32-bit forms are refused"
    );
    assert!(
        X86_64::parse("mov rax, qword ptr [rdi]").is_err(),
        "memory operands are refused"
    );
    assert!(X86_64::parse("lea rax, [rip + 0x10]").is_err());
}

#[test]
fn x86_concrete_semantics() {
    let b = x86(
        "lea rax, [rdi + 2*rdi]\nadd rax, rsi",
        &[Reg::Rdi, Reg::Rsi],
        &[Reg::Rax],
    );
    assert_eq!(b.run(&[5, 7]), vec![22]);
    assert_eq!(b.run(&[u64::MAX, 0]), vec![u64::MAX.wrapping_mul(3)]);
    let s = x86(
        "sar rdi, 63\nshr rsi, 60",
        &[Reg::Rdi, Reg::Rsi],
        &[Reg::Rdi, Reg::Rsi],
    );
    assert_eq!(s.run(&[1 << 63, u64::MAX]), vec![u64::MAX, 15]);
    let n = x86("andn rax, rdi, rsi", &[Reg::Rdi, Reg::Rsi], &[Reg::Rax]);
    assert_eq!(n.run(&[0b1100, 0b1010]), vec![0b0010]);
}

#[test]
fn block_check_rejects_undefined_reads() {
    assert!(Block::<X86_64>::parse("add rax, rbx", vec![Reg::Rbx], vec![Reg::Rax]).is_err());
    assert!(Block::<X86_64>::parse("mov rax, rbx", vec![Reg::Rbx], vec![Reg::Rcx]).is_err());
}

#[test]
fn mandate_example_imul_versus_lea_is_proven_equivalent() {
    // The mandate's example, with the multiplier made explicit: imul by 3
    // then add, versus lea then add.
    let a = x86(
        "imul rax, rbx, 3\nadd rax, rcx",
        &[Reg::Rbx, Reg::Rcx],
        &[Reg::Rax],
    );
    let b = x86(
        "lea rax, [rbx + 2*rbx]\nadd rax, rcx",
        &[Reg::Rbx, Reg::Rcx],
        &[Reg::Rax],
    );
    let r = prove_equivalent(&a, &b);
    let Equivalence::Proven { smtlib } = r else {
        panic!("expected a proof, got {r:?}")
    };
    assert!(smtlib.contains("check-sat"));
}

#[test]
fn wrong_rewrites_are_refuted_with_a_checked_counterexample() {
    let a = x86("imul rax, rbx, 5", &[Reg::Rbx], &[Reg::Rax]);
    let b = x86(
        "lea rax, [rbx + 4*rbx]\nadd rax, 1",
        &[Reg::Rbx],
        &[Reg::Rax],
    );
    match prove_equivalent(&a, &b) {
        Equivalence::Refuted { inputs } => assert_ne!(a.run(&inputs), b.run(&inputs)),
        other => panic!("expected a refutation, got {other:?}"),
    }
    // Arithmetic versus logical shift differ only for negative inputs: a
    // finite set of test vectors with small values would miss it.
    let s = x86("sar rbx, 1\nmov rax, rbx", &[Reg::Rbx], &[Reg::Rax]);
    let l = x86("shr rbx, 1\nmov rax, rbx", &[Reg::Rbx], &[Reg::Rax]);
    for v in 0..1000u64 {
        assert_eq!(s.run(&[v]), l.run(&[v]));
    }
    assert!(matches!(
        prove_equivalent(&s, &l),
        Equivalence::Refuted { .. }
    ));
}

#[test]
fn incomparable_blocks_are_not_compared() {
    let a = x86("mov rax, rbx", &[Reg::Rbx], &[Reg::Rax]);
    let b = x86("mov rcx, rbx", &[Reg::Rbx], &[Reg::Rcx]);
    assert!(matches!(
        prove_equivalent(&a, &b),
        Equivalence::Incomparable { .. }
    ));
}

#[test]
fn aarch64_round_trip_and_semantics() {
    for t in [
        "add x0, x1, x2, lsl #3",
        "sub x0, x1, #12",
        "add x0, x1, #1, lsl #12",
        "bic x3, x4, x5",
        "eon x3, x4, x5, asr #7",
        "madd x0, x1, x2, x3",
        "msub x0, x1, x2, x3",
        "lsr x0, x1, #9",
        "neg x0, x1, lsl #2",
        "mvn x0, x1",
        "movz x0, #65535, lsl #16",
        "mul x0, x1, x2",
    ] {
        let i = Aarch64::parse(t).unwrap();
        assert_eq!(i.to_string(), t);
    }
    let b: Block<Aarch64> = Block::parse(
        "add x0, x0, x0, lsl #1\nadd x0, x0, x1",
        vec![A(0), A(1)],
        vec![A(0)],
    )
    .unwrap();
    assert_eq!(b.run(&[5, 7]), vec![22]);
    let m: Block<Aarch64> = Block::parse(
        "mov x2, #3\nmadd x0, x0, x2, x1",
        vec![A(0), A(1)],
        vec![A(0)],
    )
    .unwrap();
    assert!(prove_equivalent(&b, &m).is_proven());
    assert!(Aarch64::parse("add w0, w1, w2").is_err());
    assert!(Aarch64::parse("add x0, x1, #5000").is_err());
}

const TOY: &str = "\
profile x86_64/toy@1
isa x86_64
issue_width 4
features bmi1
source hand-written for the unit test
form imul r64, r64, imm | 3 | 1 | 1 | 1
form alu r64, r64 | 1 | 0.25 | 1 | 0 1 5 6
form lea b+i*s | 1 | 0.5 | 1 | 1 5
";

#[test]
fn profile_text_round_trips() {
    let p = Profile::parse(TOY).unwrap();
    assert_eq!(Profile::parse(&p.to_string()).unwrap(), p);
    assert!(Profile::parse("isa x86_64").is_err());
}

#[test]
fn cost_model_prefers_lea_over_imul_on_latency() {
    let p = Profile::parse(TOY).unwrap();
    let a = x86(
        "imul rax, rbx, 3\nadd rax, rcx",
        &[Reg::Rbx, Reg::Rcx],
        &[Reg::Rax],
    );
    let b = x86(
        "lea rax, [rbx + 2*rbx]\nadd rax, rcx",
        &[Reg::Rbx, Reg::Rcx],
        &[Reg::Rax],
    );
    let (ca, cb) = (p.cost(&a).unwrap(), p.cost(&b).unwrap());
    assert_eq!(ca.latency, 4.0);
    assert_eq!(cb.latency, 2.0);
    assert!(cb.key(Objective::Latency) < ca.key(Objective::Latency));
    // imul is confined to port 1: throughput bound 1 cycle; lea+add spread.
    assert_eq!(ca.throughput, 1.0);
    assert_eq!(cb.throughput, 0.5);
}

#[test]
fn profile_refuses_unknown_forms_and_missing_features() {
    let mut p = Profile::parse(TOY).unwrap();
    let shift = x86("shl rax, 2", &[Reg::Rax], &[Reg::Rax]);
    assert!(p.cost(&shift).is_err());
    p.forms
        .insert("andn r64, r64, r64".into(), p.forms["alu r64, r64"].clone());
    let andn = x86("andn rax, rbx, rcx", &[Reg::Rbx, Reg::Rcx], &[Reg::Rax]);
    assert!(p.cost(&andn).is_ok());
    p.features.clear();
    assert!(p.cost(&andn).unwrap_err().contains("bmi1"));
}
