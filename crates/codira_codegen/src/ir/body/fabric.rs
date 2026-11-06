//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - String Fabric: the code generated to build strings. The inline append
//!   of `s += t` (a capacity check, a copy and a store of the fill, with no
//!   call), numbers appended straight into the destination's buffer
//!   (`s += n.to_string()` makes no temporary string), and compiled format
//!   plans: an `f"..."` literal or an `a + b + c` chain becomes one buffer
//!   sized for the whole result and one append per piece.
//!
//! # The inline append
//!
//! A `String` is `{ buf, start, len }` over a GC `[u8]` buffer whose first
//! eight bytes are its owner word (see `codira_runtime`'s `string` module
//! for the rule and the soundness argument). Appending `n` bytes compiles
//! to:
//!
//! ```text
//! payload = *buf                    ; the array's length (the fill),
//! fill, capacity, owner = payload[0], payload[8], payload[16]
//! if owner == thread pointer && fill == start + len && capacity - fill >= n:
//!     memcpy(payload + 16 + fill, data, n)
//!     payload[0] = fill + n         ; a plain (monotonic) store
//!     result = { buf, start, len + n }
//! else:
//!     result = codira.string_append(...)    ; copies, or grows
//! ```
//!
//! The thread pointer is `llvm.thread.pointer` (`fs:0` on x86-64 Linux,
//! `TPIDR_EL0` on AArch64 Linux), the register the runtime records as the
//! owner of the buffers a thread allocates. On other targets the inline
//! check is for a buffer marked shared, and the fill is claimed with a
//! `cmpxchg`, as the runtime does there.
//!
//! # Numbers
//!
//! `s += n.to_string()` for an integer of at most 64 bits checks for room
//! for the longest text such an integer has (20 bytes) and writes the
//! digits in place with `codira.fmt.dec`, a function generated into the
//! module (two digits per step from a table, the digit count from the
//! number's bit length); `s += x.to_string()` for a float, a 128-bit
//! integer, or an append without room calls the runtime, which writes the
//! text into the destination as well. No temporary `String` is made.
//!
//! # Format plans
//!
//! Interpolation lowers `f"[{x}] {n:08}"` to `"[".to_string() +
//! x.to_string() + "] " + {n:08}` (see `codira_hir`'s `expr::interpolation`):
//! a chain of `+` whose first piece makes a new string. Such a chain is
//! compiled as a whole here, from the pieces and their types alone: each
//! piece is evaluated in order (holes are variables and fields, so no piece
//! has a side effect another could see), a buffer is allocated once with
//! room for the whole result -- the literal pieces' lengths, the strings'
//! lengths and an upper bound for each number -- and every piece is
//! appended to it with the inline append, a number without a temporary, a
//! spec through the runtime's `string_append_format_*` with the spec as
//! constants. The format string itself is not in the program: nothing is
//! parsed at run time. `s += f"..."` appends the pieces to `s` directly.
//! A chain that starts with an existing `String` (`s + t + u`) appends to
//! it, as before.

use codira_hir::{
    ArithOp, BinaryOp, BuiltinFn, Expr, ExprId, FormatCount, FormatKind, FormatSpec, IntBitness,
    StringOp, StructKind, Ty, TyKind,
};
use inkwell::{
    module::Linkage,
    values::{
        BasicMetadataValueEnum, BasicValue, BasicValueEnum, FloatValue, FunctionValue, IntValue,
        PointerValue,
    },
    AddressSpace, AtomicOrdering, IntPredicate,
};

use super::BodyIrGenerator;
use crate::intrinsics;

/// Where the fields of an array payload are: the fill, the capacity, then
/// the elements (whose first word is a string buffer's owner).
const CAPACITY_OFFSET: u64 = 8;
const ELEMENTS_OFFSET: u64 = 16;

/// The longest decimal text of an integer of at most 64 bits, sign
/// included (`-9223372036854775808` and `18446744073709551615`).
const MAX_INT_TEXT: u64 = 20;

/// The most a plan allocates up front for one piece whose size has no
/// useful bound (a float with a huge precision); it grows past it.
const ESTIMATE_LIMIT: u64 = 256;

/// One piece of a string being built, evaluated.
pub(super) enum Piece<'ink> {
    /// Bytes: a `String`'s or a `str`'s data and length.
    Bytes(PointerValue<'ink>, IntValue<'ink>),
    /// `n.to_string()` for an integer: the value and its signedness.
    Int(IntValue<'ink>, bool),
    /// `x.to_string()` for a float.
    Float(FloatValue<'ink>),
    /// A hole with a format spec.
    Format(FormatPiece<'ink>),
}

/// A hole with a format spec, evaluated.
pub(super) struct FormatPiece<'ink> {
    ty: Ty,
    value: BasicValueEnum<'ink>,
    spec: FormatSpec,
    width: IntValue<'ink>,
    precision: IntValue<'ink>,
}

/// The size of an integer type in bits (`usize` and `isize` are 64 bits
/// on every target strings run on).
fn bits_of(bitness: IntBitness) -> u32 {
    match bitness {
        IntBitness::X8 => 8,
        IntBitness::X16 => 16,
        IntBitness::X32 => 32,
        IntBitness::X64 | IntBitness::Xsize => 64,
        IntBitness::X128 => 128,
    }
}

/// The decimal digits of an integer of `bits` bits: at most this many
/// bytes, sign included.
fn int_text_bound(bits: u32, signed: bool) -> u64 {
    let digits = match bits {
        0..=8 => 3,
        9..=16 => 5,
        17..=32 => 10,
        33..=64 => 20,
        _ => 39,
    };
    digits + u64::from(signed)
}

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// Whether this target reads the thread pointer the runtime records as
    /// a buffer's owner. Elsewhere buffers are shared and the inline path
    /// claims the fill with a compare-and-swap.
    fn owner_is_thread_pointer(&self) -> bool {
        let target = self.db.target();
        matches!(&*target.arch, "x86_64" | "aarch64") && target.options.os == "linux"
    }

    /// `base + offset` bytes.
    fn byte_offset(
        &self,
        base: PointerValue<'ink>,
        offset: IntValue<'ink>,
        name: &str,
    ) -> PointerValue<'ink> {
        // Safety: every offset taken here is within the array payload.
        unsafe {
            self.builder
                .build_gep(self.context.i8_type(), base, &[offset], name)
                .expect("failed to offset a pointer")
        }
    }

    /// `llvm.expect.i1(condition, true)`.
    fn expect_true(&self, condition: IntValue<'ink>) -> IntValue<'ink> {
        let bool_ty = self.context.bool_type();
        let expect = inkwell::intrinsics::Intrinsic::find("llvm.expect")
            .and_then(|intrinsic| intrinsic.get_declaration(self.module, &[bool_ty.into()]))
            .expect("llvm.expect is always available");
        self.builder
            .build_call(
                expect,
                &[condition.into(), bool_ty.const_int(1, false).into()],
                "likely",
            )
            .expect("failed to build llvm.expect")
            .try_as_basic_value()
            .basic()
            .expect("llvm.expect returns its argument")
            .into_int_value()
    }

    /// The calling thread's thread pointer, as an integer.
    fn thread_pointer(&self) -> IntValue<'ink> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let intrinsic = inkwell::intrinsics::Intrinsic::find("llvm.thread.pointer")
            .and_then(|intrinsic| intrinsic.get_declaration(self.module, &[ptr_ty.into()]))
            .expect("llvm.thread.pointer is available on this target");
        let pointer = self
            .builder
            .build_call(intrinsic, &[], "thread")
            .expect("failed to read the thread pointer")
            .try_as_basic_value()
            .basic()
            .expect("llvm.thread.pointer returns a pointer")
            .into_pointer_value();
        self.builder
            .build_ptr_to_int(pointer, self.hir_types.get_usize_type(), "thread.token")
            .expect("failed to convert the thread pointer")
    }

    /// Appends to `current` with the inline fast path: when this thread may
    /// raise the buffer's fill (see the module docs), `current` ends at it,
    /// and `room` bytes fit after it, `write` writes at most `room` bytes at
    /// the destination it is given and returns how many it wrote; otherwise
    /// `slow` makes the result from `current`'s three words (a runtime
    /// call). `exact` says that `write` writes exactly `room` bytes, which
    /// claiming the fill with a compare-and-swap needs to know before the
    /// bytes are written.
    pub(super) fn gen_fast_append(
        &mut self,
        current: BasicValueEnum<'ink>,
        room: IntValue<'ink>,
        exact: bool,
        write: impl FnOnce(&mut Self, PointerValue<'ink>) -> IntValue<'ink>,
        slow: impl FnOnce(
            &mut Self,
            PointerValue<'ink>,
            IntValue<'ink>,
            IntValue<'ink>,
        ) -> BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let usize_ty = self.hir_types.get_usize_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let thread_pointer = self.owner_is_thread_pointer();
        let (buf, start, len) = self.string_fields(current.into_struct_value());
        if !thread_pointer && !exact {
            return slow(self, buf, start, len);
        }
        let check = self
            .context
            .append_basic_block(self.fn_value, "append.check");
        let fast = self
            .context
            .append_basic_block(self.fn_value, "append.fast");
        let slow_block = self
            .context
            .append_basic_block(self.fn_value, "append.slow");
        let done = self
            .context
            .append_basic_block(self.fn_value, "append.done");

        let has_buffer = self
            .builder
            .build_is_not_null(buf, "has_buffer")
            .expect("failed to test a buffer");
        let has_buffer = self.expect_true(has_buffer);
        self.builder
            .build_conditional_branch(has_buffer, check, slow_block)
            .expect("failed to branch");

        self.builder.position_at_end(check);
        let payload = self
            .builder
            .build_load(ptr_ty, buf, "string.payload")
            .expect("failed to load a buffer's payload")
            .into_pointer_value();
        let fill = self
            .builder
            .build_load(usize_ty, payload, "string.fill")
            .expect("failed to load a buffer's fill")
            .into_int_value();
        let fill_load = fill
            .as_instruction_value()
            .expect("a load is an instruction");
        fill_load
            .set_atomic_ordering(AtomicOrdering::Monotonic)
            .expect("a load can be atomic");
        fill_load.set_alignment(8).expect("a load has an alignment");
        let capacity_ptr = self.byte_offset(
            payload,
            usize_ty.const_int(CAPACITY_OFFSET, false),
            "string.capacity_ptr",
        );
        let capacity = self
            .builder
            .build_load(usize_ty, capacity_ptr, "string.capacity")
            .expect("failed to load a buffer's capacity")
            .into_int_value();
        let elements = self.byte_offset(
            payload,
            usize_ty.const_int(ELEMENTS_OFFSET, false),
            "string.elements",
        );
        let owner = self
            .builder
            .build_load(usize_ty, elements, "string.owner")
            .expect("failed to load a buffer's owner")
            .into_int_value();
        let end = self
            .builder
            .build_int_add(start, len, "string.end")
            .expect("failed to add");
        let expected_owner = if thread_pointer {
            self.thread_pointer()
        } else {
            usize_ty.const_zero()
        };
        let owned = self
            .builder
            .build_int_compare(IntPredicate::EQ, owner, expected_owner, "owned")
            .expect("failed to compare an owner");
        let at_fill = self
            .builder
            .build_int_compare(IntPredicate::EQ, fill, end, "at_fill")
            .expect("failed to compare a fill");
        let free = self
            .builder
            .build_int_sub(capacity, end, "free")
            .expect("failed to subtract");
        let fits = self
            .builder
            .build_int_compare(IntPredicate::UGE, free, room, "fits")
            .expect("failed to compare room");
        let ok = self
            .builder
            .build_and(owned, at_fill, "ok")
            .and_then(|ok| self.builder.build_and(ok, fits, "ok"))
            .expect("failed to combine conditions");
        let ok = self.expect_true(ok);
        if thread_pointer {
            self.builder
                .build_conditional_branch(ok, fast, slow_block)
                .expect("failed to branch");
        } else {
            let claim = self
                .context
                .append_basic_block(self.fn_value, "append.claim");
            self.builder
                .build_conditional_branch(ok, claim, slow_block)
                .expect("failed to branch");
            self.builder.position_at_end(claim);
            let new_fill = self
                .builder
                .build_int_add(end, room, "new_fill")
                .expect("failed to add");
            let swapped = self
                .builder
                .build_cmpxchg(
                    payload,
                    end,
                    new_fill,
                    AtomicOrdering::AcquireRelease,
                    AtomicOrdering::Acquire,
                )
                .expect("failed to build a compare-and-swap");
            let won = self
                .builder
                .build_extract_value(swapped, 1, "claimed")
                .expect("a compare-and-swap says whether it won")
                .into_int_value();
            self.builder
                .build_conditional_branch(won, fast, slow_block)
                .expect("failed to branch");
        }

        self.builder.position_at_end(fast);
        let at = self
            .builder
            .build_int_add(end, usize_ty.const_int(ELEMENTS_OFFSET, false), "at")
            .expect("failed to add");
        let destination = self.byte_offset(payload, at, "string.dest");
        let written = write(self, destination);
        if thread_pointer {
            let new_fill = self
                .builder
                .build_int_add(end, written, "new_fill")
                .expect("failed to add");
            let store = self
                .builder
                .build_store(payload, new_fill)
                .expect("failed to store a fill");
            store
                .set_atomic_ordering(AtomicOrdering::Monotonic)
                .expect("a store can be atomic");
            store.set_alignment(8).expect("a store has an alignment");
        }
        let new_len = self
            .builder
            .build_int_add(len, written, "string.len")
            .expect("failed to add");
        let fast_value = self.make_string(buf, start, new_len);
        let fast_end = self.builder.get_insert_block().expect("inside a block");
        self.builder
            .build_unconditional_branch(done)
            .expect("failed to branch");

        self.builder.position_at_end(slow_block);
        let slow_value = slow(self, buf, start, len);
        let slow_end = self.builder.get_insert_block().expect("inside a block");
        self.builder
            .build_unconditional_branch(done)
            .expect("failed to branch");

        self.builder.position_at_end(done);
        let phi = self
            .builder
            .build_phi(self.hir_types.get_string_type(), "appended")
            .expect("failed to build a phi");
        phi.add_incoming(&[(&fast_value, fast_end), (&slow_value, slow_end)]);
        phi.as_basic_value()
    }

    /// `current` with the `len` bytes at `data` appended.
    pub(super) fn append_bytes(
        &mut self,
        current: BasicValueEnum<'ink>,
        data: PointerValue<'ink>,
        len: IntValue<'ink>,
    ) -> BasicValueEnum<'ink> {
        self.gen_fast_append(
            current,
            len,
            true,
            |this, destination| {
                this.builder
                    .build_memcpy(destination, 1, data, 1, len)
                    .expect("failed to copy bytes");
                len
            },
            |this, buf, start, old_len| {
                this.string_making_call(
                    &intrinsics::string_append,
                    &[
                        buf.into(),
                        start.into(),
                        old_len.into(),
                        data.into(),
                        len.into(),
                    ],
                )
            },
        )
    }

    /// `current` with the decimal digits of the integer `value` appended.
    fn append_int(
        &mut self,
        current: BasicValueEnum<'ink>,
        value: IntValue<'ink>,
        signed: bool,
    ) -> BasicValueEnum<'ink> {
        let usize_ty = self.hir_types.get_usize_type();
        let slow = |this: &mut Self,
                    buf: PointerValue<'ink>,
                    start: IntValue<'ink>,
                    len: IntValue<'ink>| {
            let (low, high) = this.int_halves(value, signed);
            let signed = this.context.i8_type().const_int(u64::from(signed), false);
            this.string_making_call(
                &intrinsics::string_append_int,
                &[
                    buf.into(),
                    start.into(),
                    len.into(),
                    low.into(),
                    high.into(),
                    signed.into(),
                ],
            )
        };
        if value.get_type().get_bit_width() > 64 {
            let (buf, start, len) = self.string_fields(current.into_struct_value());
            return slow(self, buf, start, len);
        }
        let wide = if signed {
            self.builder
                .build_int_s_extend_or_bit_cast(value, self.context.i64_type(), "wide")
        } else {
            self.builder
                .build_int_z_extend_or_bit_cast(value, self.context.i64_type(), "wide")
        }
        .expect("failed to widen an integer");
        let room = usize_ty.const_int(MAX_INT_TEXT, false);
        self.gen_fast_append(
            current,
            room,
            false,
            |this, destination| {
                let function = this.decimal_function();
                let signed = this.context.bool_type().const_int(u64::from(signed), false);
                this.builder
                    .build_call(
                        function,
                        &[destination.into(), wide.into(), signed.into()],
                        "digits",
                    )
                    .expect("failed to call the digit writer")
                    .try_as_basic_value()
                    .basic()
                    .expect("the digit writer returns a count")
                    .into_int_value()
            },
            slow,
        )
    }

    /// `current` with the float `value`'s `to_string()` appended: always
    /// the runtime, which writes Rust's shortest round-trip digits into the
    /// destination.
    fn append_float(
        &mut self,
        current: BasicValueEnum<'ink>,
        value: FloatValue<'ink>,
    ) -> BasicValueEnum<'ink> {
        let (buf, start, len) = self.string_fields(current.into_struct_value());
        let (wide, is_f32) = self.wide_float(value);
        self.string_making_call(
            &intrinsics::string_append_float,
            &[
                buf.into(),
                start.into(),
                len.into(),
                wide.into(),
                is_f32.into(),
            ],
        )
    }

    /// A float as an `f64` and whether it was an `f32` (as an `i8`).
    fn wide_float(&self, value: FloatValue<'ink>) -> (FloatValue<'ink>, IntValue<'ink>) {
        let is_f32 = value.get_type() == self.context.f32_type();
        let wide = if is_f32 {
            self.builder
                .build_float_ext(value, self.context.f64_type(), "wide")
                .expect("failed to widen a float")
        } else {
            value
        };
        (
            wide,
            self.context.i8_type().const_int(u64::from(is_f32), false),
        )
    }

    /// `codira.fmt.dec(dst, value, signed) -> len`, generated into this
    /// module on first use: writes the decimal digits of `value` (an `i64`
    /// if `signed`, else a `u64`) at `dst` and returns how many bytes it
    /// wrote, at most [`MAX_INT_TEXT`]. The digit count comes from the
    /// number's bit length (`bits * 1233 >> 12` is `floor(bits * log10 2)`,
    /// corrected by one comparison with a power of ten), then two digits are
    /// written per step from a table, from the last one back.
    fn decimal_function(&mut self) -> FunctionValue<'ink> {
        const NAME: &str = "codira.fmt.dec";
        if let Some(function) = self.module.get_function(NAME) {
            return function;
        }
        let i8_ty = self.context.i8_type();
        let i64_ty = self.context.i64_type();
        let bool_ty = self.context.bool_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let fn_ty = i64_ty.fn_type(&[ptr_ty.into(), i64_ty.into(), bool_ty.into()], false);
        let function = self
            .module
            .add_function(NAME, fn_ty, Some(Linkage::Private));

        // The tables: "00" to "99", and 0, 10, 100, .. 10^19.
        let pairs: Vec<u8> = (0..100u8)
            .flat_map(|n| [b'0' + n / 10, b'0' + n % 10])
            .collect();
        let pairs_value = self.context.const_string(&pairs, false);
        let pairs_global = self
            .module
            .add_global(pairs_value.get_type(), None, "codira.fmt.pairs");
        pairs_global.set_initializer(&pairs_value);
        pairs_global.set_constant(true);
        pairs_global.set_unnamed_addr(true);
        pairs_global.set_linkage(Linkage::Private);
        let powers: Vec<IntValue<'ink>> = (0..20u32)
            .map(|k| i64_ty.const_int(if k == 0 { 0 } else { 10u64.pow(k) }, false))
            .collect();
        let powers_value = i64_ty.const_array(&powers);
        let powers_global =
            self.module
                .add_global(powers_value.get_type(), None, "codira.fmt.pow10");
        powers_global.set_initializer(&powers_value);
        powers_global.set_constant(true);
        powers_global.set_unnamed_addr(true);
        powers_global.set_linkage(Linkage::Private);

        let resume_at = self.builder.get_insert_block();
        let builder = &self.builder;
        let block = |name: &str| self.context.append_basic_block(function, name);
        let entry = block("entry");
        let minus = block("minus");
        let digits = block("digits");
        let pair_loop = block("pairs");
        let pair_body = block("pair");
        let tail = block("tail");
        let tail_two = block("tail.two");
        let tail_one = block("tail.one");
        let finish = block("finish");
        let param = |index| function.get_nth_param(index).expect("three parameters");
        let dst = param(0).into_pointer_value();
        let value = param(1).into_int_value();
        let signed = param(2).into_int_value();
        let ok = "failed to build the digit writer";
        let gep = |base: PointerValue<'ink>, offset: IntValue<'ink>, name: &str| {
            // Safety: within the destination's room or a table.
            unsafe { builder.build_gep(i8_ty, base, &[offset], name).expect(ok) }
        };

        builder.position_at_end(entry);
        let is_negative = builder
            .build_int_compare(IntPredicate::SLT, value, i64_ty.const_zero(), "is_negative")
            .expect(ok);
        let negative = builder
            .build_and(signed, is_negative, "negative")
            .expect(ok);
        let negated = builder.build_int_neg(value, "negated").expect(ok);
        let magnitude = builder
            .build_select(negative, negated, value, "magnitude")
            .expect(ok)
            .into_int_value();
        let sign = builder
            .build_int_z_extend(negative, i64_ty, "sign")
            .expect(ok);
        builder
            .build_conditional_branch(negative, minus, digits)
            .expect(ok);

        builder.position_at_end(minus);
        builder
            .build_store(dst, i8_ty.const_int(u64::from(b'-'), false))
            .expect(ok);
        builder.build_unconditional_branch(digits).expect(ok);

        builder.position_at_end(digits);
        let out = gep(dst, sign, "out");
        let ctlz = inkwell::intrinsics::Intrinsic::find("llvm.ctlz")
            .and_then(|intrinsic| intrinsic.get_declaration(self.module, &[i64_ty.into()]))
            .expect("llvm.ctlz is always available");
        let or_one = builder
            .build_or(magnitude, i64_ty.const_int(1, false), "or_one")
            .expect(ok);
        let zeros = builder
            .build_call(ctlz, &[or_one.into(), bool_ty.const_zero().into()], "zeros")
            .expect(ok)
            .try_as_basic_value()
            .basic()
            .expect("llvm.ctlz returns a count")
            .into_int_value();
        let bits = builder
            .build_int_sub(i64_ty.const_int(64, false), zeros, "bits")
            .expect(ok);
        let scaled = builder
            .build_int_mul(bits, i64_ty.const_int(1233, false), "scaled")
            .expect(ok);
        let estimate = builder
            .build_right_shift(scaled, i64_ty.const_int(12, false), false, "estimate")
            .expect(ok);
        // Safety: `estimate` is at most 19, within the 20 powers.
        let power_ptr = unsafe {
            builder
                .build_gep(
                    powers_value.get_type(),
                    powers_global.as_pointer_value(),
                    &[i64_ty.const_zero(), estimate],
                    "power_ptr",
                )
                .expect(ok)
        };
        let power = builder
            .build_load(i64_ty, power_ptr, "power")
            .expect(ok)
            .into_int_value();
        let below = builder
            .build_int_compare(IntPredicate::ULT, magnitude, power, "below")
            .expect(ok);
        let below = builder
            .build_int_z_extend(below, i64_ty, "below")
            .expect(ok);
        let count = builder
            .build_int_add(estimate, i64_ty.const_int(1, false), "count")
            .and_then(|count| builder.build_int_sub(count, below, "count"))
            .expect(ok);
        let last = gep(out, count, "last");
        builder.build_unconditional_branch(pair_loop).expect(ok);

        builder.position_at_end(pair_loop);
        let at = builder.build_phi(ptr_ty, "at").expect(ok);
        let rest = builder.build_phi(i64_ty, "rest").expect(ok);
        let at_value = at.as_basic_value().into_pointer_value();
        let rest_value = rest.as_basic_value().into_int_value();
        let hundred = i64_ty.const_int(100, false);
        let more = builder
            .build_int_compare(IntPredicate::UGE, rest_value, hundred, "more")
            .expect(ok);
        builder
            .build_conditional_branch(more, pair_body, tail)
            .expect(ok);

        let copy_pair = |at: PointerValue<'ink>, pair: IntValue<'ink>, name: &str| {
            let offset = builder
                .build_int_mul(pair, i64_ty.const_int(2, false), "offset")
                .expect(ok);
            let from = gep(pairs_global.as_pointer_value(), offset, "pair_ptr");
            let to = gep(at, i64_ty.const_int((-2i64) as u64, true), name);
            builder
                .build_memcpy(to, 1, from, 1, i64_ty.const_int(2, false))
                .expect(ok);
            to
        };
        builder.position_at_end(pair_body);
        let quotient = builder
            .build_int_unsigned_div(rest_value, hundred, "quotient")
            .expect(ok);
        let remainder = builder
            .build_int_unsigned_rem(rest_value, hundred, "remainder")
            .expect(ok);
        let next_at = copy_pair(at_value, remainder, "next_at");
        builder.build_unconditional_branch(pair_loop).expect(ok);
        at.add_incoming(&[(&last, digits), (&next_at, pair_body)]);
        rest.add_incoming(&[(&magnitude, digits), (&quotient, pair_body)]);

        builder.position_at_end(tail);
        let two = builder
            .build_int_compare(
                IntPredicate::UGE,
                rest_value,
                i64_ty.const_int(10, false),
                "two",
            )
            .expect(ok);
        builder
            .build_conditional_branch(two, tail_two, tail_one)
            .expect(ok);
        builder.position_at_end(tail_two);
        copy_pair(at_value, rest_value, "first");
        builder.build_unconditional_branch(finish).expect(ok);
        builder.position_at_end(tail_one);
        let digit = builder
            .build_int_truncate(rest_value, i8_ty, "digit")
            .expect(ok);
        let digit = builder
            .build_int_add(digit, i8_ty.const_int(u64::from(b'0'), false), "digit")
            .expect(ok);
        let first = gep(at_value, i64_ty.const_int(u64::MAX, true), "first");
        builder.build_store(first, digit).expect(ok);
        builder.build_unconditional_branch(finish).expect(ok);
        builder.position_at_end(finish);
        let total = builder.build_int_add(count, sign, "total").expect(ok);
        builder.build_return(Some(&total)).expect(ok);

        if let Some(block) = resume_at {
            self.builder.position_at_end(block);
        }
        function
    }

    /// A new, empty `String` with room for `capacity` bytes, at its fill.
    fn with_capacity(&mut self, capacity: IntValue<'ink>) -> BasicValueEnum<'ink> {
        self.string_making_call(&intrinsics::string_with_capacity, &[capacity.into()])
    }

    /// The pieces of the string expression `expr`, left to right: the
    /// operands of a chain of `+` (`a + b + c`), or `expr` itself.
    fn chain_parts(&self, expr: ExprId) -> Vec<ExprId> {
        let mut parts = Vec::new();
        let mut current = expr;
        loop {
            match &self.body[current] {
                Expr::BinaryOp {
                    lhs,
                    rhs,
                    op: Some(BinaryOp::ArithOp(ArithOp::Add)),
                } if matches!(self.infer[current].interned(), TyKind::String) => {
                    parts.push(*rhs);
                    current = *lhs;
                }
                _ => {
                    parts.push(current);
                    break;
                }
            }
        }
        parts.reverse();
        parts
    }

    /// Whether the string piece `expr` makes new text rather than being an
    /// existing `String` value: a `str`, a number's or a `str`'s
    /// `to_string()`, or a formatted hole. A chain that starts with one gets
    /// a buffer of its own, sized for the whole.
    fn is_fresh_piece(&self, expr: ExprId) -> bool {
        if matches!(self.infer[expr].interned(), TyKind::Str) {
            return true;
        }
        match &self.body[expr] {
            Expr::Format { .. } => true,
            Expr::MethodCall { receiver, .. } => {
                self.infer.builtin_call(expr) == Some(BuiltinFn::String(StringOp::ToString))
                    && !matches!(self.infer[*receiver].interned(), TyKind::String)
            }
            _ => false,
        }
    }

    /// Evaluates the string piece `expr` (see [`Piece`]): a number's
    /// `to_string()` keeps the number, a hole keeps its value and spec, and
    /// anything else is a `String` or a `str` whose bytes are appended.
    fn eval_piece(&mut self, expr: ExprId) -> Option<Piece<'ink>> {
        let body = self.body.clone();
        match &body[expr] {
            Expr::MethodCall { receiver, .. }
                if self.infer.builtin_call(expr) == Some(BuiltinFn::String(StringOp::ToString))
                    && !self.infer.is_string_conversion(expr)
                    && self.infer.optional_wrap(expr).is_none() =>
            {
                let receiver_ty = self.infer[*receiver].clone();
                let value = self.gen_expr(*receiver)?;
                Some(match receiver_ty.interned() {
                    TyKind::Int(_) => {
                        Piece::Int(value.into_int_value(), self.is_signed_ty(&receiver_ty))
                    }
                    TyKind::Float(_) => Piece::Float(value.into_float_value()),
                    TyKind::Bool => {
                        let text = self.gen_bool_text(value.into_int_value());
                        let (data, len) = self.string_text_of(&TyKind::Str.intern(), text);
                        Piece::Bytes(data, len)
                    }
                    _ => {
                        let (data, len) = self.string_text_of(&receiver_ty, value);
                        Piece::Bytes(data, len)
                    }
                })
            }
            Expr::Format {
                value,
                spec,
                width,
                precision,
            } if self.infer.optional_wrap(expr).is_none() => {
                let ty = self.infer[*value].clone();
                let value = self.gen_expr(*value)?;
                let width = self.gen_count(*width, &spec.width)?;
                let precision = self.gen_count(*precision, &spec.precision)?;
                Some(Piece::Format(FormatPiece {
                    ty,
                    value,
                    spec: spec.clone(),
                    width,
                    precision,
                }))
            }
            _ => {
                let value = self.gen_expr(expr)?;
                let ty = if self.infer.is_string_conversion(expr) {
                    TyKind::String.intern()
                } else {
                    self.infer[expr].clone()
                };
                let (data, len) = self.string_text_of(&ty, value);
                Some(Piece::Bytes(data, len))
            }
        }
    }

    /// The room a plan sets aside for `piece`: a constant part and, for
    /// text, its length.
    fn piece_estimate(&self, piece: &Piece<'ink>) -> (u64, Option<IntValue<'ink>>) {
        match piece {
            Piece::Bytes(_, len) => match len.get_zero_extended_constant() {
                Some(constant) => (constant, None),
                None => (0, Some(*len)),
            },
            Piece::Int(value, signed) => (
                int_text_bound(value.get_type().get_bit_width(), *signed),
                None,
            ),
            Piece::Float(value) => {
                let is_f32 = value.get_type() == self.context.f32_type();
                (if is_f32 { 16 } else { 24 }, None)
            }
            Piece::Format(hole) => self.format_estimate(hole),
        }
    }

    /// The room a plan sets aside for a formatted hole.
    fn format_estimate(&self, hole: &FormatPiece<'ink>) -> (u64, Option<IntValue<'ink>>) {
        let literal = |count: &FormatCount| match count {
            FormatCount::Literal(n) => Some(*n),
            _ => None,
        };
        let width = literal(&hole.spec.width).unwrap_or(0);
        let precision = literal(&hole.spec.precision);
        let body = match hole.ty.interned() {
            _ if hole.spec.is_debug()
                && !matches!(hole.ty.interned(), TyKind::Int(_) | TyKind::Float(_)) =>
            {
                debug_estimate(self.db, &hole.ty, 0)
            }
            TyKind::Int(int_ty) => {
                let bits = bits_of(int_ty.bitness);
                match hole.spec.kind {
                    FormatKind::LowerHex | FormatKind::UpperHex => u64::from(bits.div_ceil(4)) + 2,
                    FormatKind::Octal => u64::from(bits.div_ceil(3)) + 2,
                    FormatKind::Binary => u64::from(bits) + 2,
                    FormatKind::LowerExp | FormatKind::UpperExp => {
                        int_text_bound(bits, true) + 4 + precision.unwrap_or(0)
                    }
                    _ => int_text_bound(bits, true),
                }
            }
            TyKind::Float(_) => match precision {
                Some(p) => 20 + p,
                None => 24,
            },
            TyKind::Bool => 5,
            // The text's own length is added below; a width is what it
            // pads to.
            TyKind::String | TyKind::Str => 0,
            _ => 16,
        };
        let constant = body.max(width).min(ESTIMATE_LIMIT);
        let dynamic = match hole.ty.interned() {
            TyKind::String | TyKind::Str if !hole.spec.is_debug() => {
                Some(self.string_text_of(&hole.ty, hole.value).1)
            }
            _ => None,
        };
        (constant, dynamic)
    }

    /// The bytes a plan sets aside for all of `pieces`.
    fn plan_capacity(&mut self, pieces: &[Piece<'ink>]) -> IntValue<'ink> {
        let usize_ty = self.hir_types.get_usize_type();
        let mut constant = 0u64;
        let mut total = None::<IntValue<'ink>>;
        for piece in pieces {
            let (fixed, dynamic) = self.piece_estimate(piece);
            constant = constant.saturating_add(fixed);
            if let Some(dynamic) = dynamic {
                total = Some(match total {
                    None => dynamic,
                    Some(sum) => self
                        .builder
                        .build_int_add(sum, dynamic, "plan.len")
                        .expect("failed to add lengths"),
                });
            }
        }
        let constant = usize_ty.const_int(constant, false);
        match total {
            None => constant,
            Some(sum) => self
                .builder
                .build_int_add(sum, constant, "plan.capacity")
                .expect("failed to add a capacity"),
        }
    }

    /// `current` with `piece` appended.
    fn append_piece(
        &mut self,
        current: BasicValueEnum<'ink>,
        piece: Piece<'ink>,
    ) -> BasicValueEnum<'ink> {
        match piece {
            Piece::Bytes(data, len) => self.append_bytes(current, data, len),
            Piece::Int(value, signed) => self.append_int(current, value, signed),
            Piece::Float(value) => self.append_float(current, value),
            Piece::Format(hole) => self.append_format(current, hole),
        }
    }

    /// `current` with a formatted hole appended: a number or text with
    /// nothing to pad as a plain piece, the debug form written by its
    /// generated function straight into the destination, and any other
    /// spec through the runtime's `string_append_format_*`, with the spec
    /// as constants (a fill character, the flags word, a width and a
    /// precision, each a constant unless the literal names a variable).
    fn append_format(
        &mut self,
        current: BasicValueEnum<'ink>,
        hole: FormatPiece<'ink>,
    ) -> BasicValueEnum<'ink> {
        let spec = &hole.spec;
        let numeric = matches!(hole.ty.interned(), TyKind::Int(_) | TyKind::Float(_));
        let plain = !spec.width.is_present()
            && !spec.precision.is_present()
            && !spec.plus
            && !spec.alternate
            && !spec.zero
            && matches!(spec.kind, FormatKind::Display | FormatKind::Debug);
        if spec.is_debug() && !numeric {
            if !spec.width.is_present() {
                return self.append_debug(current, &hole.ty, hole.value, spec.is_pretty());
            }
            let text = self.gen_debug_string(&hole.ty, hole.value, spec.is_pretty());
            return self.append_format_text(current, &TyKind::String.intern(), text, &hole);
        }
        let u32_type = self.context.i32_type();
        let fill = u32_type.const_int(u64::from(u32::from(spec.fill)), false);
        let flags = u32_type.const_int(u64::from(spec.abi_flags()), false);
        let (buf, start, len) = self.string_fields(current.into_struct_value());
        let head: [BasicMetadataValueEnum<'ink>; 3] = [buf.into(), start.into(), len.into()];
        let tail: [BasicMetadataValueEnum<'ink>; 4] = [
            fill.into(),
            flags.into(),
            hole.width.into(),
            hole.precision.into(),
        ];
        match hole.ty.interned() {
            TyKind::Int(_) => {
                let value = hole.value.into_int_value();
                let signed = self.is_signed_ty(&hole.ty);
                if plain {
                    return self.append_int(current, value, signed);
                }
                let bits = value.get_type().get_bit_width();
                let (low, high) = self.int_halves(value, signed);
                let mut args = head.to_vec();
                let value_args: [BasicMetadataValueEnum<'ink>; 4] = [
                    low.into(),
                    high.into(),
                    self.context
                        .i8_type()
                        .const_int(u64::from(signed), false)
                        .into(),
                    u32_type.const_int(u64::from(bits), false).into(),
                ];
                args.extend(value_args);
                args.extend(tail);
                self.string_making_call(&intrinsics::string_append_format_int, &args)
            }
            TyKind::Float(_) => {
                let value = hole.value.into_float_value();
                if plain {
                    return self.append_float(current, value);
                }
                let (wide, is_f32) = self.wide_float(value);
                let mut args = head.to_vec();
                let value_args: [BasicMetadataValueEnum<'ink>; 2] = [wide.into(), is_f32.into()];
                args.extend(value_args);
                args.extend(tail);
                self.string_making_call(&intrinsics::string_append_format_float, &args)
            }
            TyKind::Bool => {
                let text = self.gen_bool_text(hole.value.into_int_value());
                self.append_format_text(current, &TyKind::Str.intern(), text, &hole)
            }
            TyKind::String | TyKind::Str => {
                let ty = hole.ty.clone();
                self.append_format_text(current, &ty, hole.value, &hole)
            }
            other => unreachable!("inference admits no spec `{}` on {other:?}", spec.text),
        }
    }

    /// `current` with `text` (a `String` or `str`, as `ty` says) appended,
    /// padded and truncated as `hole`'s spec says.
    fn append_format_text(
        &mut self,
        current: BasicValueEnum<'ink>,
        ty: &Ty,
        text: BasicValueEnum<'ink>,
        hole: &FormatPiece<'ink>,
    ) -> BasicValueEnum<'ink> {
        let (data, data_len) = self.string_text_of(ty, text);
        if !hole.spec.width.is_present() && !hole.spec.precision.is_present() {
            return self.append_bytes(current, data, data_len);
        }
        let u32_type = self.context.i32_type();
        let fill = u32_type.const_int(u64::from(u32::from(hole.spec.fill)), false);
        let flags = u32_type.const_int(u64::from(hole.spec.abi_flags()), false);
        let (buf, start, len) = self.string_fields(current.into_struct_value());
        self.string_making_call(
            &intrinsics::string_append_format_text,
            &[
                buf.into(),
                start.into(),
                len.into(),
                data.into(),
                data_len.into(),
                fill.into(),
                flags.into(),
                hole.width.into(),
                hole.precision.into(),
            ],
        )
    }

    /// `current` with the debug form of `value` (of type `ty`) appended, by
    /// the type's generated debug function writing into the destination.
    fn append_debug(
        &mut self,
        current: BasicValueEnum<'ink>,
        ty: &Ty,
        value: BasicValueEnum<'ink>,
        pretty: bool,
    ) -> BasicValueEnum<'ink> {
        let string_ty = self.hir_types.get_string_type();
        let slot = self.string_slot(string_ty.into(), "debug.out");
        self.builder
            .build_store(slot, current)
            .expect("failed to start a debug string");
        self.gen_debug_into(ty, value, pretty, slot);
        self.builder
            .build_load(string_ty, slot, "debug")
            .expect("failed to load a debug string")
    }

    /// Generates the string expression `expr` -- a chain of `+`, a
    /// formatted hole, an interpolated literal -- as a plan: the pieces
    /// evaluated left to right, then appended, to a buffer sized for all of
    /// them when the first piece makes new text, else to the first piece.
    pub(super) fn gen_string_plan(&mut self, expr: ExprId) -> Option<BasicValueEnum<'ink>> {
        let parts = self.chain_parts(expr);
        let fresh = self.is_fresh_piece(parts[0]);
        let mut start = None;
        let rest = if fresh {
            &parts[..]
        } else {
            start = Some(self.gen_expr(parts[0])?);
            &parts[1..]
        };
        let mut pieces = Vec::with_capacity(rest.len());
        for &part in rest {
            pieces.push(self.eval_piece(part)?);
        }
        let mut value = match start {
            Some(start) => start,
            None => {
                let capacity = self.plan_capacity(&pieces);
                self.with_capacity(capacity)
            }
        };
        for piece in pieces {
            value = self.append_piece(value, piece);
        }
        Some(value)
    }

    /// `place += rhs` for a `String` place: the pieces of `rhs` (one, or
    /// those of a chain or an interpolated literal) evaluated first, as
    /// `rhs` itself would be, then appended to the place's value one after
    /// another -- in place when the buffer has room -- and the result
    /// stored. No string is made for `rhs`.
    pub(super) fn gen_append_assign(
        &mut self,
        lhs: ExprId,
        rhs: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        let parts = self.chain_parts(rhs);
        let mut pieces = Vec::with_capacity(parts.len());
        for &part in &parts {
            pieces.push(self.eval_piece(part)?);
        }
        let place = self.gen_place_expr(lhs)?;
        let string_ty = self.hir_types.get_string_type();
        let mut value = self
            .builder
            .build_load(string_ty, place, "string")
            .expect("failed to load a string place");
        for piece in pieces {
            value = self.append_piece(value, piece);
        }
        self.builder
            .build_store(place, value)
            .expect("failed to store a string");
        Some(self.gen_empty())
    }

    /// Appends a number's `to_string()` (for the debug form): its digits
    /// written into the destination, with no temporary string.
    pub(super) fn append_number(
        &mut self,
        current: BasicValueEnum<'ink>,
        ty: &Ty,
        value: BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        match ty.interned() {
            TyKind::Int(_) => {
                let signed = self.is_signed_ty(ty);
                self.append_int(current, value.into_int_value(), signed)
            }
            TyKind::Float(_) => self.append_float(current, value.into_float_value()),
            other => unreachable!("not a number: {other:?}"),
        }
    }

    /// A new `String` with room for `capacity` bytes (for the debug form).
    pub(super) fn gen_string_with_capacity(&mut self, capacity: u64) -> BasicValueEnum<'ink> {
        let capacity = self.hir_types.get_usize_type().const_int(capacity, false);
        self.with_capacity(capacity)
    }
}

/// How long the debug form of a value of type `ty` usually is: what a
/// buffer for `{x:?}` or `x.debug_string()` is sized to (it grows past it).
pub(super) fn debug_estimate(db: &dyn codira_hir::HirDatabase, ty: &Ty, depth: u32) -> u64 {
    if depth > 3 {
        return 16;
    }
    let estimate = match ty.interned() {
        TyKind::Int(int_ty) => int_text_bound(bits_of(int_ty.bitness), true) / 2 + 2,
        TyKind::Float(_) => 12,
        TyKind::Bool => 5,
        TyKind::String | TyKind::Str => 18,
        TyKind::Optional(payload) => debug_estimate(db, payload, depth + 1),
        TyKind::Tuple(_, elements) => {
            2 + elements
                .iter()
                .map(|element| debug_estimate(db, element, depth + 1) + 2)
                .sum::<u64>()
        }
        TyKind::Struct(strukt, _) => {
            let name = strukt.name(db).to_string().len() as u64;
            let fields = codira_hir::mono::struct_field_types(db, ty);
            let labels: u64 = if strukt.data(db).kind == StructKind::Record {
                strukt
                    .fields(db)
                    .iter()
                    .map(|field| field.name(db).to_string().len() as u64 + 2)
                    .sum()
            } else {
                0
            };
            name + 4
                + labels
                + fields
                    .iter()
                    .map(|field| debug_estimate(db, field, depth + 1) + 2)
                    .sum::<u64>()
        }
        TyKind::Enum(hir_enum, _) => hir_enum.name(db).to_string().len() as u64 + 16,
        _ => 16,
    };
    estimate.min(ESTIMATE_LIMIT)
}
