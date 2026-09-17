//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
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

    Err(IntrinsicError::Unknown)
}
