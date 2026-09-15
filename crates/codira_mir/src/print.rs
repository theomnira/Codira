//! Copyright (c) 2026 Omnira CJSC
//!
//! MLIR-flavored textual form for [`Body`] -- the debugging/testing surface
//! every serious IR needs (KGEN analog: MLIR's generic op printer, driven
//! there by `kgen-opt`). Deliberately one-way: this IR has no parser, the
//! textual form exists for humans and snapshot tests, and keeping it
//! write-only means it can favor readability over round-trip fidelity.
//!
//! Format, by example:
//!
//! ```text
//! %2 = core.add %0, %1
//! %3 = cf.while(%2) {
//! cond(1): {
//!   %0 = cf.block_arg 0
//!   %1 = core.const 10
//!   %2 = core.lt %0, %1
//! }
//! body(1): {
//!   %0 = cf.block_arg 0
//!   %1 = core.const 1
//!   %2 = core.add %0, %1
//!   cf.yield %2
//! }
//! }
//! ```
//!
//! Value numbers (`%N`) are region-local positional indices, matching the
//! region-local `OpId` scoping used everywhere else in this crate.

use std::fmt::Write as _;

use rustc_hash::FxHashMap;

use crate::op::{Attr, Body, OpId, OpKind};

/// Renders `body` in the textual form described in the module doc.
pub fn print_body(body: &Body) -> String {
    let mut out = String::new();
    print_into(body, 0, &mut out);
    out
}

// Several op kinds print identically but are distinct concepts
// (`core.arg` vs `cf.block_arg` vs `core.tuple_get`); merging the arms
// would couple their formatting to each other.
#[allow(clippy::match_same_arms)]
fn print_into(body: &Body, indent: usize, out: &mut String) {
    let mut numbering: FxHashMap<OpId, usize> = FxHashMap::default();
    for (n, (id, op)) in body.iter().enumerate() {
        numbering.insert(id, n);
        let pad = "  ".repeat(indent);

        // `Yield` produces no value; everything else is `%N = ...`.
        if matches!(op.kind, OpKind::Yield) {
            let _ = write!(out, "{pad}cf.yield");
            print_operand_list(&op.operands, &numbering, out);
            out.push('\n');
            continue;
        }

        let _ = write!(out, "{pad}%{n} = {}", op_name(&op.kind));
        match &op.kind {
            OpKind::Const(attr) => {
                out.push(' ');
                print_attr(attr, out);
            }
            OpKind::ParamRef(name) => {
                let _ = write!(out, " @{name}");
            }
            OpKind::Arg(i) => {
                let _ = write!(out, " {i}");
            }
            OpKind::BlockArg(i) => {
                let _ = write!(out, " {i}");
            }
            OpKind::TupleGet(i) => {
                print_operand_list(&op.operands, &numbering, out);
                let _ = write!(out, "[{i}]");
            }
            OpKind::Call(symbol) => {
                let _ = write!(out, " @{symbol}");
                out.push('(');
                print_bare_operands(&op.operands, &numbering, out);
                out.push(')');
            }
            OpKind::While | OpKind::For => {
                out.push('(');
                print_bare_operands(&op.operands, &numbering, out);
                out.push(')');
            }
            _ => print_operand_list(&op.operands, &numbering, out),
        }

        if !op.regions.is_empty() {
            out.push_str(" {\n");
            let labels: &[&str] = match op.kind {
                OpKind::If => &["then", "else"],
                OpKind::While => &["cond", "body"],
                OpKind::For => &["body"],
                _ => &[],
            };
            for (i, region) in op.regions.iter().enumerate() {
                let label = labels.get(i).copied().unwrap_or("region");
                let pad = "  ".repeat(indent);
                let _ = writeln!(out, "{pad}{label}({}): {{", region.num_args);
                print_into(&region.body, indent + 1, out);
                let _ = writeln!(out, "{pad}}}");
            }
            let pad = "  ".repeat(indent);
            let _ = write!(out, "{pad}}}");
        }
        out.push('\n');
    }
}

fn print_operand_list(operands: &[OpId], numbering: &FxHashMap<OpId, usize>, out: &mut String) {
    if !operands.is_empty() {
        out.push(' ');
        print_bare_operands(operands, numbering, out);
    }
}

fn print_bare_operands(operands: &[OpId], numbering: &FxHashMap<OpId, usize>, out: &mut String) {
    for (i, operand) in operands.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        match numbering.get(operand) {
            Some(n) => {
                let _ = write!(out, "%{n}");
            }
            // An operand not in this region's numbering is a verifier
            // error; print it recognizably rather than panicking so the
            // printer stays usable on broken IR (its main audience is
            // exactly someone debugging broken IR).
            None => out.push_str("%?"),
        }
    }
}

fn print_attr(attr: &Attr, out: &mut String) {
    match attr {
        Attr::Int(v) => {
            let _ = write!(out, "{v}");
        }
        Attr::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        Attr::Float(bits) => {
            let _ = write!(out, "{:?} : f64", f64::from_bits(*bits));
        }
        Attr::Str(s) => {
            let _ = write!(out, "{s:?}");
        }
        Attr::Unit => out.push_str("unit"),
        Attr::ParamRef(name) => {
            let _ = write!(out, "@{name}");
        }
    }
}

fn op_name(kind: &OpKind) -> &'static str {
    match kind {
        OpKind::Const(_) => "core.const",
        OpKind::Add => "core.add",
        OpKind::Sub => "core.sub",
        OpKind::Mul => "core.mul",
        OpKind::Div => "core.div",
        OpKind::Rem => "core.rem",
        OpKind::Neg => "core.neg",
        OpKind::Eq => "core.eq",
        OpKind::Ne => "core.ne",
        OpKind::Lt => "core.lt",
        OpKind::Le => "core.le",
        OpKind::Gt => "core.gt",
        OpKind::Ge => "core.ge",
        OpKind::And => "core.and",
        OpKind::Or => "core.or",
        OpKind::Not => "core.not",
        OpKind::BitNot => "core.bitnot",
        OpKind::BitAnd => "core.bitand",
        OpKind::BitOr => "core.bitor",
        OpKind::BitXor => "core.bitxor",
        OpKind::Shl => "core.shl",
        OpKind::Shr => "core.shr",
        OpKind::Cast(kind, mode) => match (kind, mode) {
            (crate::CastKind::Trunc, crate::CastMode::Wrapping) => "core.trunc",
            (crate::CastKind::Trunc, crate::CastMode::Checked) => "core.trunc.checked",
            (crate::CastKind::Zext, _) => "core.zext",
            (crate::CastKind::Sext, _) => "core.sext",
            (crate::CastKind::FpTrunc, crate::CastMode::Wrapping) => "core.fptrunc",
            (crate::CastKind::FpTrunc, crate::CastMode::Checked) => "core.fptrunc.checked",
            (crate::CastKind::FpExt, _) => "core.fpext",
            (crate::CastKind::SiToFp, _) => "core.sitofp",
            (crate::CastKind::UiToFp, _) => "core.uitofp",
            (crate::CastKind::FpToSi, crate::CastMode::Wrapping) => "core.fptosi",
            (crate::CastKind::FpToSi, crate::CastMode::Checked) => "core.fptosi.checked",
            (crate::CastKind::FpToUi, crate::CastMode::Wrapping) => "core.fptoui",
            (crate::CastKind::FpToUi, crate::CastMode::Checked) => "core.fptoui.checked",
            (crate::CastKind::Bitcast, _) => "core.bitcast",
        },
        OpKind::Tuple => "core.tuple",
        OpKind::TupleGet(_) => "core.tuple_get",
        OpKind::Call(_) => "core.call",
        OpKind::ParamRef(_) => "param.ref",
        OpKind::Arg(_) => "core.arg",
        OpKind::BlockArg(_) => "cf.block_arg",
        OpKind::If => "cf.if",
        OpKind::While => "cf.while",
        OpKind::For => "cf.for",
        OpKind::Yield => "cf.yield",
    }
}
