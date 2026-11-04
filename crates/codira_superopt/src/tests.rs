use codira_mcode::aarch64::{Aarch64, Reg as A};
use codira_mcode::profile::{Objective, Profile};
use codira_mcode::x86::{Reg, X86_64};
use codira_mcode::{prove_equivalent, Block};

use crate::{cache_key, superoptimize, superoptimize_cached, Config, ConstantTime, Origin};

fn plain() -> Origin {
    Origin::new("f", ConstantTime::NotRequired)
}

/// Unit-test profile with round numbers; the shipped profiles are in
/// `crates/codira_mcode/profiles/`.
const X86: &str = "\
profile x86_64/test@1
isa x86_64
issue_width 6
features bmi1
source unit test
form mov r64, r64 | 0 | 0.2 | 1 | 0 1 5 6 10
form mov r64, imm32 | 1 | 0.2 | 1 | 0 1 5 6 10
form movabs r64, imm64 | 1 | 0.5 | 1 | 0 1 5 6 10
form alu r64, r64 | 1 | 0.2 | 1 | 0 1 5 6 10
form add/sub r64, imm | 1 | 0.2 | 1 | 0 1 5 6 10
form logic r64, imm | 1 | 0.2 | 1 | 0 1 5 6 10
form imul r64, r64 | 3 | 1 | 1 | 1
form imul r64, r64, imm | 3 | 1 | 1 | 1
form shift r64, imm | 1 | 0.5 | 1 | 0 6
form neg/not r64 | 1 | 0.2 | 1 | 0 1 5 6 10
form lea b+i or b+d | 1 | 0.2 | 1 | 0 1 5 6 10
form lea b+i*s | 2 | 0.5 | 1 | 1 5
form lea b+i*s+d | 2 | 0.5 | 1 | 1 5
form andn r64, r64, r64 | 1 | 0.5 | 1 | 1 5
";

const ARM: &str = "\
profile aarch64/test@1
isa aarch64
issue_width 4
source unit test
form mov x, x | 0 | 0.33 | 1 | 0 1 2
form movz x, imm | 1 | 0.33 | 1 | 0 1 2
form add/sub x, x, x | 1 | 0.33 | 1 | 0 1 2
form add/sub x, x, x, lsl #1-4 | 1 | 0.33 | 1 | 0 1 2
form add/sub x, x, x, shift | 2 | 1 | 1 | 2
form logic x, x, x | 1 | 0.33 | 1 | 0 1 2
form logic x, x, x, shift | 1 | 0.33 | 1 | 0 1 2
form add/sub x, x, imm | 1 | 0.33 | 1 | 0 1 2
form mul x, x, x | 2 | 1 | 1 | 2
form madd/msub x, x, x, x | 2 | 1 | 1 | 2
form shift x, x, imm | 1 | 0.33 | 1 | 0 1 2
";

fn x86(text: &str, i: &[Reg], o: Reg) -> Block<X86_64> {
    Block::parse(text, i.to_vec(), vec![o]).unwrap()
}

#[test]
fn multiply_by_constant_becomes_cheaper_and_is_proven() {
    let p = Profile::parse(X86).unwrap();
    let b = x86(
        "imul rax, rdi, 5\nadd rax, rsi",
        &[Reg::Rdi, Reg::Rsi],
        Reg::Rax,
    );
    let d = superoptimize(&plain(), &b, &p, &Config::default()).unwrap();
    let c = d.improvement().expect("a cheaper sequence exists");
    assert!(c.cost.latency < d.original_cost.latency, "{d}");
    assert!(prove_equivalent(&b, &c.block).is_proven());
    assert!(d.revalidate());
    assert!(c.evidence.contains("check-sat"));
}

#[test]
fn optimal_code_is_left_alone() {
    let p = Profile::parse(X86).unwrap();
    let b = x86("lea rax, [rdi + rsi]", &[Reg::Rdi, Reg::Rsi], Reg::Rax);
    let d = superoptimize(&plain(), &b, &p, &Config::default()).unwrap();
    assert!(d.improvement().is_none(), "{d}");
}

#[test]
fn redundant_sequence_collapses() {
    let p = Profile::parse(X86).unwrap();
    // (x ^ y) ^ y = x, via three instructions.
    let b = x86(
        "mov rax, rdi\nxor rax, rsi\nxor rax, rsi",
        &[Reg::Rdi, Reg::Rsi],
        Reg::Rax,
    );
    let d = superoptimize(&plain(), &b, &p, &Config::default()).unwrap();
    let c = d.improvement().unwrap();
    assert_eq!(c.block.insts.len(), 1, "{d}");
    assert_eq!(c.block.to_string(), "mov rax, rdi\n");
}

#[test]
fn preserved_registers_are_never_written() {
    let p = Profile::parse(X86).unwrap();
    // rdi is a live-in the original does not write: no candidate may use it
    // as a temporary, even though that would allow in-place forms.
    let b = x86(
        "mov rax, rdi\nshl rax, 2\nadd rax, rdi",
        &[Reg::Rdi],
        Reg::Rax,
    );
    let d = superoptimize(&plain(), &b, &p, &Config::default()).unwrap();
    for c in &d.candidates {
        assert!(!c.block.written().contains(&Reg::Rdi), "{}", c.block);
        assert!(prove_equivalent(&b, &c.block).is_proven());
    }
}

#[test]
fn signedness_corner_needs_the_solver() {
    // x / 2 rounded toward zero for signed x is not `sar x, 1`; an
    // enumerator that only tried small positive inputs would accept it.
    // Here the original *is* the arithmetic shift, so the logical shift must
    // never be chosen.
    let p = Profile::parse(X86).unwrap();
    let b = x86(
        "mov rax, rdi\nsar rax, 1\nadd rax, 0",
        &[Reg::Rdi],
        Reg::Rax,
    );
    let d = superoptimize(&plain(), &b, &p, &Config::default()).unwrap();
    let c = d.improvement().unwrap();
    assert!(c.block.to_string().contains("sar"), "{d}");
}

#[test]
fn features_the_target_lacks_are_not_used() {
    let mut p = Profile::parse(X86).unwrap();
    let b = x86(
        "mov rax, rsi\nnot rdi\nand rax, rdi",
        &[Reg::Rdi, Reg::Rsi],
        Reg::Rax,
    );
    let with = superoptimize(&plain(), &b, &p, &Config::default()).unwrap();
    assert!(
        with.improvement()
            .unwrap()
            .block
            .to_string()
            .contains("andn"),
        "{with}"
    );
    p.features.clear();
    let without = superoptimize(&plain(), &b, &p, &Config::default()).unwrap();
    for c in &without.candidates {
        assert!(!c.block.to_string().contains("andn"));
    }
}

#[test]
fn multiple_live_outs_are_refused() {
    let p = Profile::parse(X86).unwrap();
    let b = Block::<X86_64>::parse(
        "mov rax, rdi\nmov rcx, rdi",
        vec![Reg::Rdi],
        vec![Reg::Rax, Reg::Rcx],
    )
    .unwrap();
    assert!(superoptimize(&plain(), &b, &p, &Config::default()).is_err());
}

#[test]
fn aarch64_uses_shifted_operands_and_madd() {
    let p = Profile::parse(ARM).unwrap();
    // x0 * 9 + x1 with mul: movz + madd (lat 3) versus add-with-lsl (lat 1) + add.
    let b: Block<Aarch64> = Block::parse(
        "movz x2, #9\nmul x2, x0, x2\nadd x0, x2, x1",
        vec![A(0), A(1)],
        vec![A(0)],
    )
    .unwrap();
    let d = superoptimize(&plain(), &b, &p, &Config::default()).unwrap();
    let c = d.improvement().unwrap();
    assert!(c.cost.latency <= 2.0, "{d}");
    assert!(prove_equivalent(&b, &c.block).is_proven());
}

#[test]
fn cache_key_binds_profile_and_config_and_hits_revalidate() {
    let p = Profile::parse(X86).unwrap();
    let b = x86("imul rax, rdi, 3", &[Reg::Rdi], Reg::Rax);
    let k1 = cache_key(&b, &p, &Config::default());
    let mut p2 = p.clone();
    p2.forms.get_mut("imul r64, r64, imm").unwrap().latency = 4.0;
    assert_ne!(k1, cache_key(&b, &p2, &Config::default()));
    let c2 = Config {
        objective: Objective::Throughput,
        ..Config::default()
    };
    assert_ne!(k1, cache_key(&b, &p, &c2));

    let dir = std::env::temp_dir().join(format!("codira_superopt_cache_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (first, hit1) = superoptimize_cached(&plain(), &b, &p, &Config::default(), &dir).unwrap();
    let (second, hit2) = superoptimize_cached(&plain(), &b, &p, &Config::default(), &dir).unwrap();
    assert!(!hit1 && hit2);
    assert_eq!(first, second);
    // A tampered entry is detected (revalidation fails) and recomputed.
    std::fs::write(dir.join(format!("{k1}.mcode")), "lea rax, [rdi + 4*rdi]\n").unwrap();
    let (third, hit3) = superoptimize_cached(&plain(), &b, &p, &Config::default(), &dir).unwrap();
    assert!(!hit3);
    assert_eq!(third, first);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn search_is_deterministic() {
    let p = Profile::parse(X86).unwrap();
    let b = x86(
        "imul rax, rdi, 10\nsub rax, rsi",
        &[Reg::Rdi, Reg::Rsi],
        Reg::Rax,
    );
    let a = superoptimize(&plain(), &b, &p, &Config::default()).unwrap();
    let c = superoptimize(&plain(), &b, &p, &Config::default()).unwrap();
    let blocks = |d: &crate::Decision<X86_64>| {
        d.candidates
            .iter()
            .map(|c| c.block.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(blocks(&a), blocks(&c));
    assert_eq!(a.stats.enumerated, c.stats.enumerated);
}

#[test]
fn shipped_profiles_parse_and_cost_the_mandate_example() {
    let x = Profile::parse(include_str!(
        "../../codira_mcode/profiles/x86_64-emerald-rapids-vm.profile"
    ))
    .unwrap();
    let a = Profile::parse(include_str!(
        "../../codira_mcode/profiles/aarch64-neoverse-n1.profile"
    ))
    .unwrap();
    assert_eq!(x.isa, "x86_64");
    assert_eq!(a.isa, "aarch64");
    let imul = x86(
        "imul rax, rbx, 3\nadd rax, rcx",
        &[Reg::Rbx, Reg::Rcx],
        Reg::Rax,
    );
    let lea = x86(
        "lea rax, [rbx + 2*rbx]\nadd rax, rcx",
        &[Reg::Rbx, Reg::Rcx],
        Reg::Rax,
    );
    // Measured on this core: imul 3 cycles, scaled lea 2 cycles.
    assert!(x.cost(&lea).unwrap().latency < x.cost(&imul).unwrap().latency);
}

#[test]
fn constant_time_and_unknown_origins_are_refused_with_a_diagnostic() {
    let p = Profile::parse(X86).unwrap();
    // A block the search would otherwise improve.
    let b = x86(
        "mov rax, rdi\nxor rax, rsi\nxor rax, rsi",
        &[Reg::Rdi, Reg::Rsi],
        Reg::Rax,
    );
    assert!(superoptimize(&plain(), &b, &p, &Config::default())
        .unwrap()
        .improvement()
        .is_some());
    let ct = Origin::new("compare_tags", ConstantTime::Required);
    let e = superoptimize(&ct, &b, &p, &Config::default()).unwrap_err();
    assert!(
        e.contains("compare_tags") && e.contains("constant time"),
        "{e}"
    );
    let unknown = Origin::new("g", ConstantTime::Unknown);
    let e = superoptimize(&unknown, &b, &p, &Config::default()).unwrap_err();
    assert!(e.contains("unknown"), "{e}");
    // The cached entry point refuses before looking at the cache.
    let dir = std::env::temp_dir().join(format!("codira_superopt_ct_{}", std::process::id()));
    assert!(superoptimize_cached(&ct, &b, &p, &Config::default(), &dir).is_err());
    assert!(!dir.exists());
}
