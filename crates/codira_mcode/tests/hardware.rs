//! Checks the x86-64 instruction models against the processor the tests
//! run on: every modelled form is assembled, executed on corner-case and
//! pseudo-random inputs, and compared with the concrete semantics. Skipped
//! (with a message) on hosts that are not x86-64 Linux or have no C
//! compiler; that is reported, not counted as a pass of the check.

use codira_mcode::measure::Harness;
use codira_mcode::x86::{Reg, X86_64};
use codira_mcode::Block;

fn inputs() -> Vec<Vec<u64>> {
    let corners = [
        0u64,
        1,
        2,
        3,
        63,
        64,
        0x7fff_ffff,
        0x8000_0000,
        i64::MAX as u64,
        1 << 63,
        u64::MAX,
        u64::MAX - 1,
    ];
    let mut out: Vec<Vec<u64>> = Vec::new();
    for (i, a) in corners.iter().enumerate() {
        out.push(vec![
            *a,
            corners[(i * 7 + 3) % corners.len()],
            corners[(i * 5 + 1) % corners.len()],
        ]);
    }
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..20 {
        let mut v = Vec::new();
        for _ in 0..3 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.push(x);
        }
        out.push(v);
    }
    out
}

#[test]
fn x86_models_agree_with_the_processor() {
    let dir = std::env::temp_dir().join(format!("codira_mcode_hw_{}", std::process::id()));
    let harness = match Harness::detect(&dir) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("SKIPPED: {e}");
            return;
        }
    };
    let ins = vec![Reg::Rdi, Reg::Rsi, Reg::Rdx];
    let forms = [
        "mov rax, rdi",
        "mov rax, -2",
        "movabs rax, -81985529216486895",
        "mov rax, rdi\nadd rax, rsi",
        "mov rax, rdi\nsub rax, rsi",
        "mov rax, rdi\nand rax, rsi",
        "mov rax, rdi\nor rax, rsi",
        "mov rax, rdi\nxor rax, rsi",
        "mov rax, rdi\nimul rax, rsi",
        "mov rax, rdi\nadd rax, -100000",
        "mov rax, rdi\nxor rax, 127",
        "imul rax, rdi, -7",
        "imul rax, rdi, 1000003",
        "mov rax, rdi\nshl rax, 63",
        "mov rax, rdi\nshr rax, 1",
        "mov rax, rdi\nsar rax, 17",
        "mov rax, rdi\nneg rax",
        "mov rax, rdi\nnot rax",
        "lea rax, [rdi + 8*rsi - 2147483648]",
        "lea rax, [rdi + rsi]",
        "lea rax, [4*rsi + 12]",
        "lea rax, [rdx - 1]",
        "andn rax, rdi, rsi",
    ];
    let blocks: Vec<Block<X86_64>> = forms
        .iter()
        .map(|t| Block::parse(t, ins.clone(), vec![Reg::Rax]).unwrap())
        .collect();
    let inputs = inputs();
    let hw = harness.execute(&blocks, &inputs).expect("harness runs");
    for (b, results) in blocks.iter().zip(&hw) {
        assert_eq!(results.len(), inputs.len());
        for (v, got) in inputs.iter().zip(results) {
            assert_eq!(
                &b.run(v),
                got,
                "model and processor disagree on\n{b}for inputs {v:?}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
