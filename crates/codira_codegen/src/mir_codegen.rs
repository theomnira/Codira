//! Copyright (c) 2026 Omnira CJSC
//!
//! `codira_mir::Body` -> real LLVM IR, via inkwell.
//!
//! This is `spec/KGEN_SUPERSET_STATUS.md` roadmap item 5 / session-2
//! item #19: the piece that makes an elaborated (parameter-free)
//! `codira_mir::Body` -- the output of `codira_comptime::elaborate`,
//! reached via `codira_hir`'s `elaborate_generator` salsa query --
//! actually compilable, not just interpretable. Structural analog of
//! `modular/KGEN/lib/KGENToLLVM` *plus* KGEN's `LowerLoops` pass: the IR
//! keeps structured control flow (`cf.if`/`cf.while`/`cf.for` region ops
//! with loop-carried values, MLIR `scf`-style -- see `codira_mir::op`'s
//! module doc) all the way to this boundary, and this module performs the
//! structured->CFG flattening (region ops -> basic blocks, branches, and
//! phi nodes) in the same breath as the LLVM translation, exactly where
//! KGEN runs `LowerLoops` immediately before its LLVM lowering
//! (`modular/KGEN/lib/Compiler/Pipeline/Pipeline.cpp`, late-opt pipeline).
//!
//! # Coverage and honesty
//!
//! The full elaborated op set is lowered: `Const` (int/bool/float),
//! `Arg`, integer and float arithmetic (with int->float promotion when
//! either operand of a mixed `Add`/`Sub`/`Mul`/`Div`/`Rem` is a float,
//! matching `codira_mir::fold`'s promotion rule), comparisons (signed for
//! ints, ordered `O*` predicates for floats -- IEEE semantics per
//! `fold.rs`), logic ops, bitwise ops (`Shr` is an **arithmetic** shift:
//! `Attr::Int` is signed `i64`, and `fold.rs` is the semantics
//! reference), `Tuple`/`TupleGet` (an anonymous LLVM struct value),
//! `If`, `While`, `For`, and `Call` (via [`lower_mir_body_with_callees`]).
//!
//! What still honestly refuses (`None`, never a guess):
//! * a surviving `param.ref` / `Const(ParamRef)` -- elaboration should have
//!   removed every one that was going to be resolved, so a survivor means the
//!   caller under-specialized;
//! * `Const(Unit)` as a *value* -- there is no unit ABI at this layer;
//! * `Const(Str)` -- no string ABI at this layer yet (no memory model to put
//!   the bytes in);
//! * a `cf.yield` anywhere other than the tail of a loop body being lowered by
//!   this module (the verifier rejects those bodies too);
//! * loops with **zero** carried values -- their result is `Unit`, which has no
//!   value representation here (same reason as `Const(Unit)`);
//! * a `Call` to a symbol the caller didn't supply an LLVM function for.
//!
//! All integers are treated as signed 64-bit and all floats as `f64`
//! (`codira_mir::Attr` has no width tags yet -- see architecture doc §8
//! non-goals).
//!
//! # Loop flattening scheme
//!
//! Both loop ops lower to the classic rotated-entry-free form:
//!
//! ```text
//! entry:            br cond
//! cond:             phi per carried value (and the induction variable,
//!                   for `cf.for`); evaluate the condition;
//!                   br cond?, body, exit
//! body:             loop body ops (same phi values as `cond` -- one
//!                   region-argument frame per *iteration*, shared by
//!                   both regions); br cond   [the phis' back edge]
//! exit:             carried phis are the loop's result
//! ```
//!
//! The condition is evaluated once per iteration *including* the first --
//! standard while semantics, matching the comptime interpreter.
//!
//! # Region-argument frames
//!
//! `cf.block_arg(i)` reads argument `i` of the innermost enclosing region
//! *that takes arguments*. `cf.if` regions take none and are transparent
//! (see `codira_mir::verify`: an `If` branch is verified with its
//! enclosing region's `num_args`), so the lowering keeps a **stack** of
//! frames: loop-body/cond lowering pushes the current iteration's phi
//! values and pops them afterwards, while `If` lowering passes the stack
//! through untouched. A `BlockArg` inside an `If` inside a loop therefore
//! resolves to the loop's carried values, exactly as the verifier's
//! scoping rule dictates.

use inkwell::{
    builder::Builder,
    context::Context,
    module::Module,
    types::BasicTypeEnum,
    values::{
        AggregateValueEnum, BasicMetadataValueEnum, BasicValueEnum, FloatValue, FunctionValue,
        IntValue, PhiValue,
    },
    FloatPredicate, IntPredicate,
};
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

/// Lowers `body` to LLVM IR, emitting instructions at the builder's
/// current insertion point (the caller is responsible for having
/// positioned it inside a real basic block of `function` first, exactly
/// like any other codegen helper in this crate). `args` supplies the
/// concrete LLVM values for the body's `OpKind::Arg(i)` references (the
/// function's actual runtime parameters). Returns the body's result value,
/// or `None` if the body isn't fully lowerable (see module doc).
///
/// `core.call` ops lower only through [`lower_mir_body_with_callees`];
/// through this entry point they are unlowerable (`None`), preserving the
/// original signature's behavior for callers that predate calls.
pub fn lower_mir_body<'ink>(
    context: &'ink Context,
    builder: &Builder<'ink>,
    module: &Module<'ink>,
    function: FunctionValue<'ink>,
    body: &codira_mir::Body,
    args: &[BasicValueEnum<'ink>],
) -> Option<BasicValueEnum<'ink>> {
    lower_mir_body_with_callees(
        context,
        builder,
        module,
        function,
        body,
        args,
        &FxHashMap::default(),
    )
}

/// [`lower_mir_body`], extended with a callee table: `core.call(symbol)`
/// ops resolve `symbol` against `callees` (the way KGEN resolves
/// `#kgen.genref` symbol references against the module symbol table --
/// see `codira_mir::GeneratorStore::generator_by_name`, the IR-level
/// counterpart of this map) and lower to a direct LLVM call. A symbol
/// missing from the map -- or a callee returning void -- makes the body
/// unlowerable (`None`), per the module-doc honesty rule.
pub fn lower_mir_body_with_callees<'ink>(
    context: &'ink Context,
    builder: &Builder<'ink>,
    module: &Module<'ink>,
    function: FunctionValue<'ink>,
    body: &codira_mir::Body,
    args: &[BasicValueEnum<'ink>],
    callees: &FxHashMap<SmolStr, FunctionValue<'ink>>,
) -> Option<BasicValueEnum<'ink>> {
    let mut lowerer = Lowerer {
        context,
        builder,
        module,
        function,
        args,
        callees,
        block_counter: 0,
        frames: Vec::new(),
    };
    lowerer.lower_body(body)
}

/// All the state threaded through the recursive region lowering. Values
/// are *not* stored here: they are keyed by `OpId`, which is only
/// meaningful within one specific `Body`'s arena, so each region lowering
/// owns a fresh map (matching the same per-arena scoping
/// `codira_hir::mir_lower` and `codira_comptime::elaborate` both use, for
/// exactly the same reason -- see their doc comments).
struct Lowerer<'a, 'ink> {
    context: &'ink Context,
    builder: &'a Builder<'ink>,
    /// The module being built. Needed to *declare* LLVM intrinsics --
    /// saturating float-to-int conversion lowers to `llvm.fptosi.sat`,
    /// which must be declared in the module before it can be called.
    module: &'a Module<'ink>,
    function: FunctionValue<'ink>,
    args: &'a [BasicValueEnum<'ink>],
    callees: &'a FxHashMap<SmolStr, FunctionValue<'ink>>,
    /// Monotonic counter making generated basic-block names unique across
    /// the whole function, however deeply regions nest.
    block_counter: u32,
    /// The region-argument frame stack -- see the module doc. The innermost
    /// frame is `frames.last()`; `If` regions never push, loop regions
    /// push the current iteration's phi values.
    frames: Vec<Vec<BasicValueEnum<'ink>>>,
}

impl<'a, 'ink> Lowerer<'a, 'ink> {
    /// Lowers a region body in "expression position": every op is emitted
    /// and the body's result (its last op's value) is returned. Used for
    /// the top-level body, `If` branches, and `While` cond regions -- any
    /// region whose last op is a value, not a `Yield`.
    fn lower_body(&mut self, body: &codira_mir::Body) -> Option<BasicValueEnum<'ink>> {
        let mut values = FxHashMap::default();
        self.lower_ops(body, &mut values, false)?;
        values.get(&body.result()?).copied()
    }

    /// Lowers a *loop body* region: every op except the trailing
    /// `cf.yield` is emitted, and the values named by the yield's operands
    /// (the next iteration's carried values) are returned instead of a
    /// single result.
    fn lower_loop_body(&mut self, body: &codira_mir::Body) -> Option<Vec<BasicValueEnum<'ink>>> {
        let mut values = FxHashMap::default();
        self.lower_ops(body, &mut values, true)?;
        let yield_id = body.result()?;
        let yield_op = body.get(yield_id);
        if !matches!(yield_op.kind, codira_mir::OpKind::Yield) {
            return None;
        }
        yield_op
            .operands
            .iter()
            .map(|&id| values.get(&id).copied())
            .collect()
    }

    /// Maps an Eidos [`codira_mir::TypeId`] to its LLVM type.
    ///
    /// Only scalars are mapped here; aggregates reach codegen as `Tuple`
    /// ops whose LLVM type is built structurally from their elements.
    /// `None` means "no LLVM representation", which makes the caller
    /// decline rather than guess.
    fn llvm_type(&self, ty: codira_mir::TypeId) -> Option<BasicTypeEnum<'ink>> {
        if let Some((width, _signed)) = ty.as_int() {
            // Signedness is not part of an LLVM integer type -- it is a
            // property of the *operations*, which is why the cast kind
            // (sext vs zext) carries it rather than the type.
            let bits = std::num::NonZeroU32::new(u32::from(width))?;
            return Some(self.context.custom_width_int_type(bits).ok()?.into());
        }
        if let Some(width) = ty.as_float() {
            return Some(match width {
                32 => self.context.f32_type().into(),
                64 => self.context.f64_type().into(),
                _ => return None,
            });
        }
        if ty == codira_mir::TypeId::BOOL {
            return Some(self.context.bool_type().into());
        }
        None
    }

    /// Emits a saturating float-to-integer conversion via LLVM's
    /// `llvm.fptosi.sat` / `llvm.fptoui.sat` intrinsics.
    ///
    /// These are overloaded on *both* the result and operand types, so the
    /// intrinsic name carries two type suffixes (e.g.
    /// `llvm.fptosi.sat.i32.f64`). They saturate to the destination
    /// range and map NaN to zero, which is precisely the specified `as`
    /// semantics -- unlike the raw `fptosi` instruction, which is poison
    /// out of range.
    fn build_saturating_fp_to_int(
        &self,
        source: FloatValue<'ink>,
        target: inkwell::types::IntType<'ink>,
        signed: bool,
    ) -> Option<BasicValueEnum<'ink>> {
        let int_bits = target.get_bit_width();
        let float_bits = if source.get_type() == self.context.f32_type() {
            32
        } else {
            64
        };
        let name = format!(
            "llvm.fpto{}i.sat.i{int_bits}.f{float_bits}",
            if signed { 's' } else { 'u' }
        );
        let intrinsic = inkwell::intrinsics::Intrinsic::find(&name)?;
        let declaration =
            intrinsic.get_declaration(self.module, &[target.into(), source.get_type().into()])?;
        self.builder
            .build_call(declaration, &[source.into()], "mir_fptoint_sat")
            .ok()?
            .try_as_basic_value()
            .basic()
    }

    /// Emits one wrapping conversion.
    fn lower_cast(
        &self,
        kind: codira_mir::CastKind,
        source: BasicValueEnum<'ink>,
        target: BasicTypeEnum<'ink>,
    ) -> Option<BasicValueEnum<'ink>> {
        use codira_mir::CastKind as K;
        let b = &self.builder;
        Some(match kind {
            K::Trunc => b
                .build_int_truncate(source.into_int_value(), target.into_int_type(), "mir_trunc")
                .ok()?
                .into(),
            K::Zext => b
                .build_int_z_extend(source.into_int_value(), target.into_int_type(), "mir_zext")
                .ok()?
                .into(),
            K::Sext => b
                .build_int_s_extend(source.into_int_value(), target.into_int_type(), "mir_sext")
                .ok()?
                .into(),
            K::FpTrunc => b
                .build_float_trunc(
                    source.into_float_value(),
                    target.into_float_type(),
                    "mir_fptrunc",
                )
                .ok()?
                .into(),
            K::FpExt => b
                .build_float_ext(
                    source.into_float_value(),
                    target.into_float_type(),
                    "mir_fpext",
                )
                .ok()?
                .into(),
            K::SiToFp => b
                .build_signed_int_to_float(
                    source.into_int_value(),
                    target.into_float_type(),
                    "mir_sitofp",
                )
                .ok()?
                .into(),
            K::UiToFp => b
                .build_unsigned_int_to_float(
                    source.into_int_value(),
                    target.into_float_type(),
                    "mir_uitofp",
                )
                .ok()?
                .into(),
            // Float -> int is **saturating**, per spec/LANGUAGE_SPEC.md:
            // out-of-range clamps to [T::MIN, T::MAX] and NaN maps to 0.
            //
            // LLVM's plain `fptosi`/`fptoui` are *poison* outside the
            // representable range -- using them here would be exactly the
            // undefined behavior the saturating rule exists to forbid. The
            // `llvm.fptosi.sat` / `llvm.fptoui.sat` intrinsics implement
            // the specified semantics directly, including NaN -> 0.
            K::FpToSi | K::FpToUi => {
                let signed = matches!(kind, K::FpToSi);
                self.build_saturating_fp_to_int(
                    source.into_float_value(),
                    target.into_int_type(),
                    signed,
                )?
            }
            K::Bitcast => b.build_bit_cast(source, target, "mir_bitcast").ok()?,
        })
    }

    /// back-edge inputs), not a value-producing op.
    // Several op kinds decline for different documented reasons (no
    // string ABI; an unresolved param means under-elaboration). Merging
    // the arms would erase which refusal is which.
    #[allow(clippy::match_same_arms)]
    fn lower_ops(
        &mut self,
        body: &codira_mir::Body,
        values: &mut FxHashMap<codira_mir::OpId, BasicValueEnum<'ink>>,
        expect_trailing_yield: bool,
    ) -> Option<()> {
        use codira_mir::{Attr, OpKind};

        for (id, op) in body.iter() {
            let operand = |i: usize| -> Option<BasicValueEnum<'ink>> {
                values.get(op.operands.get(i)?).copied()
            };

            let value: BasicValueEnum<'ink> = match &op.kind {
                // Constants are **type-directed**. Materializing every
                // integer literal as LLVM `i64` regardless of `op.ty`
                // silently miscompiles any cast whose source is a literal:
                // `core.const 200 : u8` followed by a bitcast to `i8`
                // would hand LLVM an `i64` and produce a nonsense
                // conversion. See RFC-001 section 1.3 -- the declared type
                // is authoritative.
                OpKind::Const(Attr::Int(v)) => {
                    let (width, signed) = op.ty.as_int().unwrap_or((64, true));
                    let bits = std::num::NonZeroU32::new(u32::from(width))?;
                    self.context
                        .custom_width_int_type(bits)
                        .ok()?
                        .const_int(*v as u64, signed)
                        .into()
                }
                OpKind::Const(Attr::Bool(b)) => self
                    .context
                    .bool_type()
                    .const_int(u64::from(*b), false)
                    .into(),
                // Same type-direction as integer constants: an f32-typed
                // literal must be built in f32, or a following fpext /
                // fptrunc converts from the wrong source type.
                OpKind::Const(Attr::Float(bits)) => {
                    let value = f64::from_bits(*bits);
                    match op.ty.as_float() {
                        Some(32) => self.context.f32_type().const_float(value).into(),
                        _ => self.context.f64_type().const_float(value).into(),
                    }
                }
                // No string ABI at this layer yet (no memory model to put
                // the bytes in) -- honestly unlowerable, see module doc.
                OpKind::Const(Attr::Str(_)) => return None,
                // A `Const(Unit)` or a surviving `ParamRef` reaching
                // codegen means the caller handed us a body that either
                // genuinely computes "nothing" or wasn't fully elaborated
                // -- neither is lowerable here. See module doc.
                OpKind::Const(Attr::Unit | Attr::ParamRef(_)) | OpKind::ParamRef(_) => return None,

                // The caller supplies LLVM values for `core.arg`; when the
                // op carries a type, check it agrees. A silent mismatch makes
                // every downstream cast wrong (e.g. an LLVM `i64` handed in
                // for a `u32`-typed arg turns `zext` into a no-op), and a
                // debug_assert would not catch it in release. Declining is
                // the module's stated convention for "cannot lower this".
                OpKind::Arg(index) => {
                    let value = *self.args.get(*index as usize)?;
                    if !op.ty.is_untyped() {
                        let expected = self.llvm_type(op.ty)?;
                        if value.get_type() != expected {
                            return None;
                        }
                    }
                    value
                }

                // Region argument: the innermost argument-taking region's
                // frame. `If` regions are transparent (they never push a
                // frame), so this correctly reaches through an `If` to the
                // enclosing loop's carried values.
                OpKind::BlockArg(index) => *self.frames.last()?.get(*index as usize)?,

                OpKind::Add | OpKind::Sub | OpKind::Mul | OpKind::Div | OpKind::Rem => {
                    let lhs = operand(0)?;
                    let rhs = operand(1)?;
                    if lhs.is_float_value() || rhs.is_float_value() {
                        // Mixed int/float arithmetic promotes the int side,
                        // matching `codira_mir::fold::float_operand`.
                        let lhs = self.promote_to_float(lhs)?;
                        let rhs = self.promote_to_float(rhs)?;
                        match op.kind {
                            OpKind::Add => self.builder.build_float_add(lhs, rhs, "mir_fadd"),
                            OpKind::Sub => self.builder.build_float_sub(lhs, rhs, "mir_fsub"),
                            OpKind::Mul => self.builder.build_float_mul(lhs, rhs, "mir_fmul"),
                            OpKind::Div => self.builder.build_float_div(lhs, rhs, "mir_fdiv"),
                            OpKind::Rem => self.builder.build_float_rem(lhs, rhs, "mir_frem"),
                            _ => unreachable!(),
                        }
                        .ok()?
                        .into()
                    } else {
                        let lhs = int_value(lhs)?;
                        let rhs = int_value(rhs)?;
                        match op.kind {
                            OpKind::Add => self.builder.build_int_add(lhs, rhs, "mir_add"),
                            OpKind::Sub => self.builder.build_int_sub(lhs, rhs, "mir_sub"),
                            OpKind::Mul => self.builder.build_int_mul(lhs, rhs, "mir_mul"),
                            OpKind::Div => self.builder.build_int_signed_div(lhs, rhs, "mir_div"),
                            OpKind::Rem => self.builder.build_int_signed_rem(lhs, rhs, "mir_rem"),
                            _ => unreachable!(),
                        }
                        .ok()?
                        .into()
                    }
                }

                OpKind::Neg => match operand(0)? {
                    BasicValueEnum::FloatValue(v) => {
                        self.builder.build_float_neg(v, "mir_fneg").ok()?.into()
                    }
                    BasicValueEnum::IntValue(v) => {
                        self.builder.build_int_neg(v, "mir_neg").ok()?.into()
                    }
                    _ => return None,
                },

                OpKind::Eq | OpKind::Ne | OpKind::Lt | OpKind::Le | OpKind::Gt | OpKind::Ge => {
                    let lhs = operand(0)?;
                    let rhs = operand(1)?;
                    if lhs.is_float_value() || rhs.is_float_value() {
                        // Ordered predicates: IEEE comparison semantics per
                        // `fold.rs` (`NaN != NaN`, all orderings false on
                        // NaN), same choice as `ir::body`'s float compares.
                        let predicate = match op.kind {
                            OpKind::Eq => FloatPredicate::OEQ,
                            OpKind::Ne => FloatPredicate::ONE,
                            OpKind::Lt => FloatPredicate::OLT,
                            OpKind::Le => FloatPredicate::OLE,
                            OpKind::Gt => FloatPredicate::OGT,
                            OpKind::Ge => FloatPredicate::OGE,
                            _ => unreachable!(),
                        };
                        let lhs = self.promote_to_float(lhs)?;
                        let rhs = self.promote_to_float(rhs)?;
                        self.builder
                            .build_float_compare(predicate, lhs, rhs, "mir_fcmp")
                            .ok()?
                            .into()
                    } else {
                        let predicate = match op.kind {
                            OpKind::Eq => IntPredicate::EQ,
                            OpKind::Ne => IntPredicate::NE,
                            OpKind::Lt => IntPredicate::SLT,
                            OpKind::Le => IntPredicate::SLE,
                            OpKind::Gt => IntPredicate::SGT,
                            OpKind::Ge => IntPredicate::SGE,
                            _ => unreachable!(),
                        };
                        self.builder
                            .build_int_compare(
                                predicate,
                                int_value(lhs)?,
                                int_value(rhs)?,
                                "mir_cmp",
                            )
                            .ok()?
                            .into()
                    }
                }

                OpKind::And | OpKind::Or => {
                    let lhs = int_value(operand(0)?)?;
                    let rhs = int_value(operand(1)?)?;
                    match op.kind {
                        OpKind::And => self.builder.build_and(lhs, rhs, "mir_and"),
                        OpKind::Or => self.builder.build_or(lhs, rhs, "mir_or"),
                        _ => unreachable!(),
                    }
                    .ok()?
                    .into()
                }

                // Bitwise complement. Distinct from `core.not` (logical, on
                // bool) so the verifier can type-check both; they happen to
                // share one LLVM instruction.
                OpKind::BitNot => {
                    let v = int_value(operand(0)?)?;
                    self.builder.build_not(v, "mir_bitnot").ok()?.into()
                }

                OpKind::Not => {
                    let v = int_value(operand(0)?)?;
                    self.builder.build_not(v, "mir_not").ok()?.into()
                }

                OpKind::BitAnd | OpKind::BitOr | OpKind::BitXor => {
                    let lhs = int_value(operand(0)?)?;
                    let rhs = int_value(operand(1)?)?;
                    match op.kind {
                        OpKind::BitAnd => self.builder.build_and(lhs, rhs, "mir_bitand"),
                        OpKind::BitOr => self.builder.build_or(lhs, rhs, "mir_bitor"),
                        OpKind::BitXor => self.builder.build_xor(lhs, rhs, "mir_bitxor"),
                        _ => unreachable!(),
                    }
                    .ok()?
                    .into()
                }

                OpKind::Shl => {
                    let lhs = int_value(operand(0)?)?;
                    let rhs = int_value(operand(1)?)?;
                    self.builder
                        .build_left_shift(lhs, rhs, "mir_shl")
                        .ok()?
                        .into()
                }
                OpKind::Shr => {
                    let lhs = int_value(operand(0)?)?;
                    let rhs = int_value(operand(1)?)?;
                    // Arithmetic (sign-propagating) shift: `Attr::Int` is
                    // signed `i64` and `fold.rs` uses `>>` on `i64`.
                    self.builder
                        .build_right_shift(lhs, rhs, true, "mir_shr")
                        .ok()?
                        .into()
                }

                // Explicit conversions (RFC-001 section 1.5). The target type
                // comes from `op.ty` -- the first place the typed IR is
                // load-bearing in codegen rather than merely carried.
                //
                // `CastMode::Checked` is deliberately refused: a checked cast
                // carries an undischarged proof obligation, and emitting it as
                // a silent truncation would be exactly the miscompile the mode
                // exists to prevent. It becomes lowerable once refinement
                // checking either discharges the obligation (erase the cast)
                // or materializes a runtime check (RFC-002 section 3.3).
                OpKind::Cast(kind, mode) => {
                    if !matches!(mode, codira_mir::CastMode::Wrapping) {
                        return None;
                    }
                    let source = operand(0)?;
                    let target = self.llvm_type(op.ty)?;
                    self.lower_cast(*kind, source, target)?
                }

                // Explicit conversions (RFC-001 section 1.5). The target type
                // comes from `op.ty` -- the first place the typed IR is
                // load-bearing in codegen rather than merely carried.
                //
                // `CastMode::Checked` is deliberately refused: a checked cast
                // carries an undischarged proof obligation, and emitting it as
                // a silent truncation would be exactly the miscompile the mode
                // exists to prevent. It becomes lowerable once refinement
                // checking either discharges the obligation (erase the cast)
                // or materializes a runtime check (RFC-002 section 3.3).
                OpKind::Tuple => {
                    let elements: Vec<BasicValueEnum<'ink>> =
                        (0..op.operands.len()).map(operand).collect::<Option<_>>()?;
                    self.build_tuple(&elements)?
                }

                OpKind::TupleGet(index) => {
                    let BasicValueEnum::StructValue(tuple) = operand(0)? else {
                        return None;
                    };
                    self.builder
                        .build_extract_value(tuple, *index, "mir_tuple_get")
                        .ok()?
                }

                OpKind::Call(symbol) => {
                    let callee = *self.callees.get(symbol)?;
                    let call_args: Vec<BasicMetadataValueEnum<'ink>> = (0..op.operands.len())
                        .map(|i| operand(i).map(Into::into))
                        .collect::<Option<_>>()?;
                    let call = self
                        .builder
                        .build_call(callee, &call_args, "mir_call")
                        .ok()?;
                    // A void callee produces no value -- unlowerable in a
                    // value-oriented body, per the honesty rule.
                    call.try_as_basic_value().basic()?
                }

                OpKind::If => {
                    let cond = int_value(operand(0)?)?;
                    let [then_region, else_region] = op.regions.as_slice() else {
                        return None;
                    };
                    self.lower_if(cond, then_region, else_region)?
                }

                OpKind::While => self.lower_while(op, values)?,
                OpKind::For => self.lower_for(op, values)?,

                OpKind::Yield => {
                    // The trailing yield of a loop body is a terminator
                    // consumed by `lower_loop_body` (its operands feed the
                    // carried-value phis); it is last by the verifier's
                    // placement rule, so stopping here lowers the whole
                    // region. A yield anywhere else is malformed IR --
                    // honestly unlowerable.
                    if expect_trailing_yield && Some(id) == body.result() {
                        return Some(());
                    }
                    return None;
                }
            };
            values.insert(id, value);
        }
        Some(())
    }

    /// `cf.if` -> conditional branch + phi merge. Region-argument frames
    /// pass through untouched: `If` regions take no block arguments, so a
    /// `BlockArg` inside a branch still resolves against the enclosing
    /// loop's frame (module doc, "Region-argument frames").
    fn lower_if(
        &mut self,
        cond: IntValue<'ink>,
        then_region: &codira_mir::Region,
        else_region: &codira_mir::Region,
    ) -> Option<BasicValueEnum<'ink>> {
        self.block_counter += 1;
        let n = self.block_counter;
        let then_block = self
            .context
            .append_basic_block(self.function, &format!("mir_if_then{n}"));
        let else_block = self
            .context
            .append_basic_block(self.function, &format!("mir_if_else{n}"));
        let merge_block = self
            .context
            .append_basic_block(self.function, &format!("mir_if_merge{n}"));

        self.builder
            .build_conditional_branch(cond, then_block, else_block)
            .ok()?;

        self.builder.position_at_end(then_block);
        let then_value = self.lower_body(&then_region.body)?;
        self.builder.build_unconditional_branch(merge_block).ok()?;
        let then_end_block = self.builder.get_insert_block()?;

        self.builder.position_at_end(else_block);
        let else_value = self.lower_body(&else_region.body)?;
        self.builder.build_unconditional_branch(merge_block).ok()?;
        let else_end_block = self.builder.get_insert_block()?;

        self.builder.position_at_end(merge_block);
        let phi = self
            .builder
            .build_phi(then_value.get_type(), "mir_if_result")
            .ok()?;
        phi.add_incoming(&[(&then_value, then_end_block), (&else_value, else_end_block)]);
        Some(phi.as_basic_value())
    }

    /// `cf.while` -> the rotated CFG in the module doc. `enclosing_values`
    /// is the *enclosing* body's value map, used only to resolve the
    /// loop's initial-value operands.
    fn lower_while(
        &mut self,
        op: &codira_mir::Op,
        enclosing_values: &FxHashMap<codira_mir::OpId, BasicValueEnum<'ink>>,
    ) -> Option<BasicValueEnum<'ink>> {
        let [cond_region, body_region] = op.regions.as_slice() else {
            return None;
        };
        // Zero carried values means a `Unit` result -- no value
        // representation at this layer (module doc), so refuse before
        // emitting anything.
        if op.operands.is_empty() {
            return None;
        }
        let inits: Vec<BasicValueEnum<'ink>> = op
            .operands
            .iter()
            .map(|id| enclosing_values.get(id).copied())
            .collect::<Option<_>>()?;

        self.block_counter += 1;
        let n = self.block_counter;
        let cond_block = self
            .context
            .append_basic_block(self.function, &format!("mir_while_cond{n}"));
        let body_block = self
            .context
            .append_basic_block(self.function, &format!("mir_while_body{n}"));
        let exit_block = self
            .context
            .append_basic_block(self.function, &format!("mir_while_exit{n}"));

        let entry_end_block = self.builder.get_insert_block()?;
        self.builder.build_unconditional_branch(cond_block).ok()?;

        // cond_block: one phi per carried value. These phi values *are*
        // the iteration's region arguments -- the same frame feeds both
        // the cond region and the body region within one iteration (the
        // two regions see identical carried values; only the yield at the
        // body's tail advances them).
        self.builder.position_at_end(cond_block);
        let phis = self.build_carried_phis(&inits, entry_end_block, "mir_while_carry")?;
        let carried: Vec<BasicValueEnum<'ink>> =
            phis.iter().map(|phi| phi.as_basic_value()).collect();

        // The condition is re-evaluated on every arrival at cond_block --
        // once per iteration, including before the first (standard while
        // semantics; the comptime interpreter does the same).
        self.frames.push(carried.clone());
        let cond_result = self.lower_body(&cond_region.body);
        self.frames.pop();
        let cond_value = int_value(cond_result?)?;
        self.builder
            .build_conditional_branch(cond_value, body_block, exit_block)
            .ok()?;

        // body_block: same frame (same phis), ops lowered up to -- not
        // including -- the trailing yield, whose operands become the phi
        // back edge.
        self.builder.position_at_end(body_block);
        self.frames.push(carried.clone());
        let yielded = self.lower_loop_body(&body_region.body);
        self.frames.pop();
        let yielded = yielded?;
        if yielded.len() != phis.len() {
            return None;
        }
        self.builder.build_unconditional_branch(cond_block).ok()?;
        let body_end_block = self.builder.get_insert_block()?;
        for (phi, value) in phis.iter().zip(&yielded) {
            phi.add_incoming(&[(value, body_end_block)]);
        }

        self.builder.position_at_end(exit_block);
        self.loop_result(&carried)
    }

    /// `cf.for` -> the same rotated CFG, with the induction variable as an
    /// extra phi and the direction-aware exit condition. The step is an
    /// SSA value (its sign is unknowable statically), so both directions'
    /// comparisons are emitted and selected between:
    /// `step > 0 ? iv < end : iv > end` -- exactly `OpKind::For`'s
    /// documented iteration rule.
    fn lower_for(
        &mut self,
        op: &codira_mir::Op,
        enclosing_values: &FxHashMap<codira_mir::OpId, BasicValueEnum<'ink>>,
    ) -> Option<BasicValueEnum<'ink>> {
        let [body_region] = op.regions.as_slice() else {
            return None;
        };
        if op.operands.len() < 3 {
            return None;
        }
        // Same `Unit`-result refusal as `lower_while`.
        if op.operands.len() == 3 {
            return None;
        }
        let mut bounds = op
            .operands
            .iter()
            .take(3)
            .map(|id| enclosing_values.get(id).copied().and_then(int_value));
        let start = bounds.next()??;
        let end = bounds.next()??;
        let step = bounds.next()??;
        let inits: Vec<BasicValueEnum<'ink>> = op.operands[3..]
            .iter()
            .map(|id| enclosing_values.get(id).copied())
            .collect::<Option<_>>()?;

        self.block_counter += 1;
        let n = self.block_counter;
        let cond_block = self
            .context
            .append_basic_block(self.function, &format!("mir_for_cond{n}"));
        let body_block = self
            .context
            .append_basic_block(self.function, &format!("mir_for_body{n}"));
        let exit_block = self
            .context
            .append_basic_block(self.function, &format!("mir_for_exit{n}"));

        let entry_end_block = self.builder.get_insert_block()?;
        self.builder.build_unconditional_branch(cond_block).ok()?;

        // cond_block: induction-variable phi + carried-value phis.
        self.builder.position_at_end(cond_block);
        let iv_phi = self
            .builder
            .build_phi(start.get_type(), "mir_for_iv")
            .ok()?;
        iv_phi.add_incoming(&[(&start, entry_end_block)]);
        let iv = iv_phi.as_basic_value().into_int_value();
        let phis = self.build_carried_phis(&inits, entry_end_block, "mir_for_carry")?;
        let carried: Vec<BasicValueEnum<'ink>> =
            phis.iter().map(|phi| phi.as_basic_value()).collect();

        let ascending = self
            .builder
            .build_int_compare(IntPredicate::SLT, iv, end, "mir_for_lt_end")
            .ok()?;
        let descending = self
            .builder
            .build_int_compare(IntPredicate::SGT, iv, end, "mir_for_gt_end")
            .ok()?;
        let zero = self.context.i64_type().const_int(0, false);
        let step_positive = self
            .builder
            .build_int_compare(IntPredicate::SGT, step, zero, "mir_for_step_pos")
            .ok()?;
        let keep_going = self
            .builder
            .build_select(step_positive, ascending, descending, "mir_for_cond")
            .ok()?
            .into_int_value();
        self.builder
            .build_conditional_branch(keep_going, body_block, exit_block)
            .ok()?;

        // body_block: the frame is `[iv, carried...]` -- block arg 0 is
        // the induction variable per `OpKind::For`'s region contract.
        self.builder.position_at_end(body_block);
        let mut frame = Vec::with_capacity(1 + carried.len());
        frame.push(iv.into());
        frame.extend(carried.iter().copied());
        self.frames.push(frame);
        let yielded = self.lower_loop_body(&body_region.body);
        self.frames.pop();
        let yielded = yielded?;
        if yielded.len() != phis.len() {
            return None;
        }
        let next_iv = self
            .builder
            .build_int_add(iv, step, "mir_for_next_iv")
            .ok()?;
        self.builder.build_unconditional_branch(cond_block).ok()?;
        let body_end_block = self.builder.get_insert_block()?;
        iv_phi.add_incoming(&[(&next_iv, body_end_block)]);
        for (phi, value) in phis.iter().zip(&yielded) {
            phi.add_incoming(&[(value, body_end_block)]);
        }

        self.builder.position_at_end(exit_block);
        self.loop_result(&carried)
    }

    /// One phi per carried value, seeded with the entry-edge initial
    /// values. The back edge is added by the caller once the body's yield
    /// values exist.
    fn build_carried_phis(
        &mut self,
        inits: &[BasicValueEnum<'ink>],
        entry_block: inkwell::basic_block::BasicBlock<'ink>,
        name: &str,
    ) -> Option<Vec<PhiValue<'ink>>> {
        inits
            .iter()
            .map(|init| {
                let phi = self.builder.build_phi(init.get_type(), name).ok()?;
                phi.add_incoming(&[(init, entry_block)]);
                Some(phi)
            })
            .collect()
    }

    /// The loop-result convention from `OpKind::While`'s doc: the carried
    /// value itself if N == 1, a `Tuple`-shaped struct if N > 1 (N == 0 is
    /// refused before any emission -- see `lower_while`/`lower_for`).
    fn loop_result(&mut self, carried: &[BasicValueEnum<'ink>]) -> Option<BasicValueEnum<'ink>> {
        match carried {
            [] => None,
            [single] => Some(*single),
            many => self.build_tuple(many),
        }
    }

    /// Packs values into an anonymous LLVM struct via an insertvalue
    /// chain -- the lowering of `core.tuple` and of multi-value loop
    /// results (which `OpKind::While` documents as "`Tuple`-shaped", so
    /// both must produce the same representation for `TupleGet` to project
    /// out of either). Heterogeneous element types are fine: the struct
    /// type is derived from the element value types.
    fn build_tuple(&mut self, elements: &[BasicValueEnum<'ink>]) -> Option<BasicValueEnum<'ink>> {
        let types: Vec<BasicTypeEnum<'ink>> = elements
            .iter()
            .map(inkwell::values::BasicValueEnum::get_type)
            .collect();
        let struct_type = self.context.struct_type(&types, false);
        let mut aggregate: AggregateValueEnum<'ink> = struct_type.get_undef().into();
        for (i, element) in elements.iter().enumerate() {
            aggregate = self
                .builder
                .build_insert_value(aggregate, *element, i as u32, "mir_tuple")
                .ok()?;
        }
        Some(aggregate.into_struct_value().into())
    }

    /// Int->float promotion for mixed arithmetic/comparison, mirroring
    /// `codira_mir::fold::float_operand` (comptime and codegen must agree
    /// on what `2 * 3.14` means).
    fn promote_to_float(&self, value: BasicValueEnum<'ink>) -> Option<FloatValue<'ink>> {
        match value {
            BasicValueEnum::FloatValue(v) => Some(v),
            BasicValueEnum::IntValue(v) => self
                .builder
                .build_signed_int_to_float(v, self.context.f64_type(), "mir_promote")
                .ok(),
            _ => None,
        }
    }
}

/// Honest downcast: `None` (unlowerable) rather than a panic when
/// malformed IR hands a non-integer where an integer is required.
fn int_value(value: BasicValueEnum<'_>) -> Option<IntValue<'_>> {
    match value {
        BasicValueEnum::IntValue(v) => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod tests;
