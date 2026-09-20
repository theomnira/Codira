//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: September 17, 2026
//!
//! Functionality:
//! - Lowers `extern "codira-intrinsic"` declarations to inline LLVM IR.

use inkwell::{
    builder::Builder,
    context::Context,
    values::{BasicMetadataValueEnum, BasicValue, BasicValueEnum},
    AddressSpace,
};

/// The width and interpretation of one memory access.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scalar {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

impl Scalar {
    /// Parses the type suffix shared by every `load_*`/`store_*` name.
    ///
    /// Signedness is deliberately absent: a load moves bits, and `u32` and
    /// `i32` move the same 32 of them. Both spellings are accepted so a
    /// declaration can name the type it actually wants, and both map here.
    fn from_suffix(suffix: &str) -> Option<Self> {
        Some(match suffix {
            "u8" | "i8" => Scalar::I8,
            "u16" | "i16" => Scalar::I16,
            "u32" | "i32" => Scalar::I32,
            "u64" | "i64" => Scalar::I64,
            "f32" => Scalar::F32,
            "f64" => Scalar::F64,
            _ => return None,
        })
    }

    fn alignment(self) -> u32 {
        match self {
            Scalar::I8 => 1,
            Scalar::I16 => 2,
            Scalar::I32 | Scalar::F32 => 4,
            Scalar::I64 | Scalar::F64 => 8,
        }
    }

    fn ir_type<'ink>(self, context: &'ink Context) -> inkwell::types::BasicTypeEnum<'ink> {
        match self {
            Scalar::I8 => context.i8_type().into(),
            Scalar::I16 => context.i16_type().into(),
            Scalar::I32 => context.i32_type().into(),
            Scalar::I64 => context.i64_type().into(),
            Scalar::F32 => context.f32_type().into(),
            Scalar::F64 => context.f64_type().into(),
        }
    }
}

/// Why an intrinsic call could not be lowered.
pub enum IntrinsicError {
    /// The name is not one this compiler knows.
    Unknown,
    /// The name is known but the call does not match its shape.
    Arity { expected: usize, found: usize },
}

/// Emits an `extern "codira-intrinsic"` call as inline IR.
///
/// # Why these are intrinsics and not a library
///
/// Everything here is one or two machine instructions. Reaching a buffer that
/// JavaScript, Python or C owns means turning an integer address into a
/// pointer and loading through it -- an `inttoptr` and a `load`. Routed
/// through an ordinary function call, the call itself would cost far more
/// than the work, and a per-element call is exactly the shape of the problem
/// this exists to solve: crossing a runtime's FFI boundary once per element
/// costs about 6.4 ns per element in Bun, against roughly a third of a
/// nanosecond for the same work done in a loop on the native side.
///
/// So these are emitted where they are called, which lets a Codira function
/// take `(address, length)` and process a caller-owned buffer at native
/// speed, with the FFI boundary crossed once for the whole buffer instead of
/// once per element.
///
/// # Safety
///
/// There is none, by construction. `load_u32(addr)` reads whatever is at
/// `addr`; nothing checks that the address is mapped, aligned, or owned by
/// anyone in particular. That is the same contract C has, and it is the
/// reason these are spelled `extern` -- a Codira program that uses them is
/// taking responsibility for the address the same way a C program does.
pub fn gen_intrinsic<'ink>(
    name: &str,
    args: &[BasicMetadataValueEnum<'ink>],
    context: &'ink Context,
    module: &inkwell::module::Module<'ink>,
    builder: &Builder<'ink>,
) -> Result<Option<BasicValueEnum<'ink>>, IntrinsicError> {
    let ptr_type = context.ptr_type(AddressSpace::default());

    // `__intrinsic_sqrt_f64` and `sqrt_f64` name the same operation. The
    // prefix is optional because the `extern "codira-intrinsic"` block
    // already says what these are, but it is *useful* where the standard
    // library also exports a function of the plain name: `std/math` has a
    // public `sqrt_f32` wrapper, and without a way to spell the two apart
    // the declaration and its wrapper would collide.
    let name = name.strip_prefix("__intrinsic_").unwrap_or(name);

    if let Some(suffix) = name.strip_prefix("load_") {
        let scalar = Scalar::from_suffix(suffix).ok_or(IntrinsicError::Unknown)?;
        if args.len() != 1 {
            return Err(IntrinsicError::Arity {
                expected: 1,
                found: args.len(),
            });
        }

        let address = args[0].into_int_value();
        let pointer = builder
            .build_int_to_ptr(address, ptr_type, "intrinsic.addr")
            .expect("could not build address cast for intrinsic load");
        let loaded = builder
            .build_load(scalar.ir_type(context), pointer, "intrinsic.load")
            .expect("could not build intrinsic load");
        loaded
            .as_instruction_value()
            .expect("a load is an instruction")
            .set_alignment(scalar.alignment())
            .expect("alignment is a power of two");
        return Ok(Some(loaded));
    }

    if let Some(suffix) = name.strip_prefix("store_") {
        let scalar = Scalar::from_suffix(suffix).ok_or(IntrinsicError::Unknown)?;
        if args.len() != 2 {
            return Err(IntrinsicError::Arity {
                expected: 2,
                found: args.len(),
            });
        }

        let address = args[0].into_int_value();
        let pointer = builder
            .build_int_to_ptr(address, ptr_type, "intrinsic.addr")
            .expect("could not build address cast for intrinsic store");
        let value: BasicValueEnum<'_> = args[1]
            .try_into()
            .expect("intrinsic store value must be a basic value");
        let store = builder
            .build_store(pointer, value)
            .expect("could not build intrinsic store");
        store
            .set_alignment(scalar.alignment())
            .expect("alignment is a power of two");
        // A store produces nothing; the caller substitutes unit.
        return Ok(None);
    }

    // Bit-counting maps to LLVM's own intrinsics, which become a single
    // instruction on every target that has one (`LZCNT`/`TZCNT`/`POPCNT` on
    // x86, `CLZ`/`CNT` on AArch64). Writing the shift-and-mask version in
    // Codira instead costs about ten instructions for the same answer.
    let counting = [
        ("ctlz_", "llvm.ctlz"),
        ("cttz_", "llvm.cttz"),
        ("popcount_", "llvm.ctpop"),
    ]
    .into_iter()
    .find_map(|(prefix, llvm)| name.strip_prefix(prefix).map(|suffix| (suffix, llvm)));

    if let Some((suffix, llvm_name)) = counting {
        let scalar = Scalar::from_suffix(suffix).ok_or(IntrinsicError::Unknown)?;
        let int_type = match scalar {
            Scalar::I8 => context.i8_type(),
            Scalar::I16 => context.i16_type(),
            Scalar::I32 => context.i32_type(),
            Scalar::I64 => context.i64_type(),
            // There is no float population count.
            Scalar::F32 | Scalar::F64 => return Err(IntrinsicError::Unknown),
        };
        if args.len() != 1 {
            return Err(IntrinsicError::Arity {
                expected: 1,
                found: args.len(),
            });
        }

        let intrinsic = inkwell::intrinsics::Intrinsic::find(llvm_name)
            .expect("LLVM always provides the bit-counting intrinsics");
        let declaration = intrinsic
            .get_declaration(module, &[int_type.into()])
            .expect("could not resolve bit-counting intrinsic");

        // `llvm.ctlz`/`llvm.cttz` take a second argument saying whether a
        // zero input is poison. It is `false` here: `ctlz_u32(0)` is 32, the
        // same answer `Math.clz32(0)` gives, rather than undefined behaviour.
        // `llvm.ctpop` takes only the value.
        let mut call_args: Vec<BasicMetadataValueEnum<'_>> = vec![args[0]];
        if llvm_name != "llvm.ctpop" {
            call_args.push(context.bool_type().const_zero().into());
        }

        let result = builder
            .build_direct_call(declaration, &call_args, "intrinsic.bitcount")
            .expect("could not build bit-counting intrinsic call")
            .try_as_basic_value()
            .basic()
            .expect("bit-counting intrinsics return a value");
        return Ok(Some(result));
    }

    if let Some(result) = gen_float_math(name, args, context, module, builder)? {
        return Ok(Some(result));
    }

    if let Some(result) = gen_integer_bit_op(name, args, context, module, builder)? {
        return Ok(Some(result));
    }

    if gen_memory_op(name, args, context, module, builder)? {
        // The memory operations produce nothing; the caller substitutes unit.
        return Ok(None);
    }

    if let Some(result) = gen_bitcast(name, args, context, builder)? {
        return Ok(Some(result));
    }

    if let Some(result) = gen_str_accessor(name, args, context, builder)? {
        return Ok(Some(result));
    }

    Err(IntrinsicError::Unknown)
}

/// Emits `str_data(s)` and `str_len(s)`: the two halves of a string
/// literal's `{ ptr, usize }`.
///
/// `str` is a compiler primitive, so it has no fields for ordinary member
/// access to reach. Without these there is no way to hand a literal's bytes
/// to anything -- not to a C function, not to the runtime's stderr hook --
/// which would make `str` a type you can hold and never use.
///
/// `str_data` returns the address as an integer rather than a pointer,
/// matching the `usize`-addressed load and store intrinsics above, so the
/// two families compose without a cast in between.
fn gen_str_accessor<'ink>(
    name: &str,
    args: &[BasicMetadataValueEnum<'ink>],
    context: &'ink Context,
    builder: &Builder<'ink>,
) -> Result<Option<BasicValueEnum<'ink>>, IntrinsicError> {
    let field = match name {
        "str_data" => 0,
        "str_len" => 1,
        _ => return Ok(None),
    };

    if args.len() != 1 {
        return Err(IntrinsicError::Arity {
            expected: 1,
            found: args.len(),
        });
    }

    let value = args[0].into_struct_value();
    let extracted = builder
        .build_extract_value(value, field, "intrinsic.str")
        .expect("a `str` has both of its fields");

    if field == 1 {
        return Ok(Some(extracted));
    }

    let address = builder
        .build_ptr_to_int(
            extracted.into_pointer_value(),
            context.i64_type(),
            "intrinsic.str.addr",
        )
        .expect("could not build address cast for a string literal");
    Ok(Some(address.into()))
}

/// Emits `bitcast_u64_f64(bits)` and its three siblings: the same bits read
/// as the other type, which is no instruction at all at run time.
///
/// This is how a float constant that cannot be written as a literal gets
/// built. Infinity and NaN have exact bit patterns and no source spelling,
/// and the usual workarounds -- `1.0 / 0.0` for infinity, `0.0 / 0.0` for
/// NaN -- are constant-folded by the optimiser into whatever it likes, which
/// is not reliably the value asked for.
fn gen_bitcast<'ink>(
    name: &str,
    args: &[BasicMetadataValueEnum<'ink>],
    context: &'ink Context,
    builder: &Builder<'ink>,
) -> Result<Option<BasicValueEnum<'ink>>, IntrinsicError> {
    let target: inkwell::types::BasicTypeEnum<'_> = match name {
        "bitcast_u32_f32" => context.f32_type().into(),
        "bitcast_u64_f64" => context.f64_type().into(),
        "bitcast_f32_u32" => context.i32_type().into(),
        "bitcast_f64_u64" => context.i64_type().into(),
        _ => return Ok(None),
    };

    if args.len() != 1 {
        return Err(IntrinsicError::Arity {
            expected: 1,
            found: args.len(),
        });
    }

    let value: BasicValueEnum<'_> = args[0]
        .try_into()
        .expect("intrinsic bitcast operand must be a basic value");
    let result = builder
        .build_bit_cast(value, target, "intrinsic.bitcast")
        .expect("could not build intrinsic bitcast");
    Ok(Some(result))
}

/// The floating-point operations that LLVM provides directly.
///
/// Each is one instruction on a target that has it, and a call into the
/// platform math library on one that does not -- which is exactly the
/// tradeoff `libm` makes, decided per-target by the backend rather than
/// per-call by the standard library. Writing `sqrt` in Codira instead would
/// give up the hardware instruction on every target that has one.
///
/// Both widths of each are accepted, distinguished by the `_f32`/`_f64`
/// suffix the standard library already spells them with.
const FLOAT_MATH: &[(&str, &str, usize)] = &[
    ("sqrt", "llvm.sqrt", 1),
    ("sin", "llvm.sin", 1),
    ("cos", "llvm.cos", 1),
    ("exp", "llvm.exp", 1),
    ("exp2", "llvm.exp2", 1),
    ("log", "llvm.log", 1),
    ("log2", "llvm.log2", 1),
    ("log10", "llvm.log10", 1),
    ("fabs", "llvm.fabs", 1),
    ("floor", "llvm.floor", 1),
    ("ceil", "llvm.ceil", 1),
    ("trunc", "llvm.trunc", 1),
    // `llvm.round` rounds halfway cases away from zero, which is what
    // `round` means in every language that ships one; `llvm.rint` would
    // follow the current rounding mode instead.
    ("round", "llvm.round", 1),
    ("pow", "llvm.pow", 2),
    ("copysign", "llvm.copysign", 2),
    ("minnum", "llvm.minnum", 2),
    ("maxnum", "llvm.maxnum", 2),
    ("fma", "llvm.fma", 3),
];

/// Emits `sqrt_f64(x)` and friends.
fn gen_float_math<'ink>(
    name: &str,
    args: &[BasicMetadataValueEnum<'ink>],
    context: &'ink Context,
    module: &inkwell::module::Module<'ink>,
    builder: &Builder<'ink>,
) -> Result<Option<BasicValueEnum<'ink>>, IntrinsicError> {
    let Some((base, width)) = name
        .strip_suffix("_f32")
        .map(|base| (base, Scalar::F32))
        .or_else(|| name.strip_suffix("_f64").map(|base| (base, Scalar::F64)))
    else {
        return Ok(None);
    };

    let Some((_, llvm_name, arity)) = FLOAT_MATH.iter().find(|(codira, _, _)| *codira == base)
    else {
        return Ok(None);
    };

    if args.len() != *arity {
        return Err(IntrinsicError::Arity {
            expected: *arity,
            found: args.len(),
        });
    }

    let float_type = width.ir_type(context);
    let intrinsic = inkwell::intrinsics::Intrinsic::find(llvm_name)
        .expect("LLVM always provides the floating-point math intrinsics");
    let declaration = intrinsic
        .get_declaration(module, &[float_type])
        .expect("could not resolve floating-point math intrinsic");

    let result = builder
        .build_direct_call(declaration, args, "intrinsic.math")
        .expect("could not build floating-point math intrinsic call")
        .try_as_basic_value()
        .basic()
        .expect("floating-point math intrinsics return a value");
    Ok(Some(result))
}

/// Emits `bswap_u32(x)` and `bitreverse_u64(x)`.
///
/// Byte swapping is a single instruction (`BSWAP`, `REV`) and bit reversal
/// is one on `AArch64` (`RBIT`); both cost a handful in hand-written Codira.
fn gen_integer_bit_op<'ink>(
    name: &str,
    args: &[BasicMetadataValueEnum<'ink>],
    context: &'ink Context,
    module: &inkwell::module::Module<'ink>,
    builder: &Builder<'ink>,
) -> Result<Option<BasicValueEnum<'ink>>, IntrinsicError> {
    let found = [("bswap_", "llvm.bswap"), ("bitreverse_", "llvm.bitreverse")]
        .into_iter()
        .find_map(|(prefix, llvm)| name.strip_prefix(prefix).map(|suffix| (suffix, llvm)));

    let Some((suffix, llvm_name)) = found else {
        return Ok(None);
    };

    let scalar = Scalar::from_suffix(suffix).ok_or(IntrinsicError::Unknown)?;
    let int_type = match scalar {
        Scalar::I8 => context.i8_type(),
        Scalar::I16 => context.i16_type(),
        Scalar::I32 => context.i32_type(),
        Scalar::I64 => context.i64_type(),
        // `llvm.bswap` rejects i8 in the verifier and there is nothing to
        // swap in one byte anyway; floats have no byte-order intrinsic.
        Scalar::F32 | Scalar::F64 => return Err(IntrinsicError::Unknown),
    };

    if args.len() != 1 {
        return Err(IntrinsicError::Arity {
            expected: 1,
            found: args.len(),
        });
    }

    let intrinsic = inkwell::intrinsics::Intrinsic::find(llvm_name)
        .expect("LLVM always provides the byte- and bit-reversal intrinsics");
    let declaration = intrinsic
        .get_declaration(module, &[int_type.into()])
        .expect("could not resolve bit-reversal intrinsic");

    let result = builder
        .build_direct_call(declaration, args, "intrinsic.bits")
        .expect("could not build bit-reversal intrinsic call")
        .try_as_basic_value()
        .basic()
        .expect("bit-reversal intrinsics return a value");
    Ok(Some(result))
}

/// Emits `memcpy(dst, src, len)`, `memmove(..)` and `memset(dst, byte, len)`.
///
/// Returns whether the name was one of these, since all three produce no
/// value.
///
/// Each takes byte addresses as `usize`, matching the load and store
/// intrinsics above, and is emitted non-volatile: these exist to move bulk
/// data, and marking them volatile would stop the backend turning them into
/// the vectorised form that is the entire reason to call them.
fn gen_memory_op<'ink>(
    name: &str,
    args: &[BasicMetadataValueEnum<'ink>],
    context: &'ink Context,
    _module: &inkwell::module::Module<'ink>,
    builder: &Builder<'ink>,
) -> Result<bool, IntrinsicError> {
    let ptr_type = context.ptr_type(AddressSpace::default());

    let expect_arity = |expected: usize| -> Result<(), IntrinsicError> {
        if args.len() == expected {
            Ok(())
        } else {
            Err(IntrinsicError::Arity {
                expected,
                found: args.len(),
            })
        }
    };

    let as_pointer = |index: usize, label: &str| {
        builder
            .build_int_to_ptr(args[index].into_int_value(), ptr_type, label)
            .expect("could not build address cast for memory intrinsic")
    };

    match name {
        "memcpy" | "memmove" => {
            expect_arity(3)?;
            let destination = as_pointer(0, "intrinsic.dst");
            let source = as_pointer(1, "intrinsic.src");
            let length = args[2].into_int_value();

            // `memcpy` requires the regions not to overlap; `memmove` is the
            // one that allows it. Emitting the wrong one is a miscompile
            // that only shows up on overlapping input, so they stay
            // distinct rather than both lowering to the permissive one.
            if name == "memcpy" {
                builder
                    .build_memcpy(destination, 1, source, 1, length)
                    .expect("could not build memcpy");
            } else {
                builder
                    .build_memmove(destination, 1, source, 1, length)
                    .expect("could not build memmove");
            }
            Ok(true)
        }
        "memset" => {
            expect_arity(3)?;
            let destination = as_pointer(0, "intrinsic.dst");
            let byte = args[1].into_int_value();
            let length = args[2].into_int_value();
            builder
                .build_memset(destination, 1, byte, length)
                .expect("could not build memset");
            Ok(true)
        }
        _ => Ok(false),
    }
}
