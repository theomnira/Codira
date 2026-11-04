//! Type-based alias metadata for array lengths and array elements
//! (experimental, opt-in with `CODIRA_ARRAY_TBAA=1`).
//!
//! An indexed access `xs[i]` checks `i` against the array's length, which
//! it loads from the array header. In a loop that also stores to an array
//! element, LLVM must assume the store may have changed the length (both are
//! plain memory), so it reloads the length on every iteration, cannot hoist
//! the check and does not vectorise the loop. A naive `i-k-j` matrix
//! multiply (512 x 512 `f64`) runs about 2.9 times slower than the same C.
//!
//! Measured effect (Emerald Rapids VM, `-O 3`, whole process, 15 interleaved
//! runs): median 180 ms without the tags, 149 ms with them. The length and
//! handle loads of the array that is stored to leave the inner loop; those
//! of the other array do not, because they follow the first bounds check
//! and LLVM cannot prove the handle dereferenceable, so it may not
//! speculate them. The loop is still not vectorised and still well behind C.
//!
//! With this module enabled, three sibling TBAA access tags under a private
//! root mark (1) the loads and stores of an array's length that codegen
//! emits, (2) the element loads and stores of `xs[i]` after its bounds
//! check, of array literals and of `array_filled`, and (3) the load of the
//! pointer from an array's object header to its length and elements. LLVM
//! may then assume that accesses of different classes never overlap. Every
//! other access stays untagged, which LLVM treats as possibly aliasing
//! anything.
//!
//! Why this is sound. An array value points at a garbage-collector object
//! header whose first word points at the payload: the length, the capacity,
//! then the elements inline. A tagged element access addresses
//! `elements + i * size` with `0 <= i < length <= capacity`, inside the
//! payload's element storage, never in the payload header (length,
//! capacity) and never in the object header, which is a different
//! allocation. A tagged length access is the payload's first field. The
//! handle load reads the object header's first word, which generated code
//! never writes; it is written only by runtime functions (allocation, hot
//! reload), which are calls. Lengths are written only when an array is
//! created (Codira arrays do not grow). `unchecked_get` is deliberately not
//! tagged: an out-of-range index there may read another array's header.
//! Raw-memory intrinsics, inline assembly and foreign code emit no tags, so
//! they are never assumed not to alias anything. Assumptions: foreign code
//! does not forge array handles or write a length larger than the capacity
//! (both already break the language's memory safety), and no typed raw
//! load ever produces an array handle from an integer (the generic typed
//! load intrinsic has no lowering today; lowering it for array types would
//! need to drop these tags).
//!
//! The variable is read once per process. A build daemon started without
//! it keeps building without it.

use std::sync::OnceLock;

use inkwell::values::{BasicValue, InstructionValue};

/// Which alias class an access belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArrayAccess {
    /// The length field of an array header.
    Length,
    /// An element reached through a bounds-checked index.
    Element,
    /// The slot of an array's object header that holds the pointer to the
    /// array's length and elements.
    Handle,
}

/// Whether `CODIRA_ARRAY_TBAA=1` was set when the process first asked.
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CODIRA_ARRAY_TBAA").is_some_and(|v| v == "1"))
}

/// Attaches the access tag of `kind` to `instruction` when enabled. The
/// instruction must already be in a basic block.
pub(crate) fn tag(instruction: InstructionValue<'_>, kind: ArrayAccess) {
    if !enabled() {
        return;
    }
    let context = instruction
        .get_parent()
        .expect("an instruction in a basic block")
        .get_context();
    let zero = context.i64_type().const_zero();
    let root =
        context.metadata_node(&[context.metadata_string("codira array alias classes").into()]);
    let name = match kind {
        ArrayAccess::Length => "codira array length",
        ArrayAccess::Element => "codira array element",
        ArrayAccess::Handle => "codira array handle data pointer",
    };
    let ty = context.metadata_node(&[
        context.metadata_string(name).into(),
        root.into(),
        zero.into(),
    ]);
    let access = context.metadata_node(&[ty.into(), ty.into(), zero.into()]);
    instruction
        .set_metadata(access, context.get_kind_id("tbaa"))
        .expect("a scalar TBAA access tag is valid `!tbaa` metadata");
}

/// [`tag`] for the value a load produced.
pub(crate) fn tag_value<'ink>(value: impl BasicValue<'ink>, kind: ArrayAccess) {
    if let Some(instruction) = value.as_instruction_value() {
        tag(instruction, kind);
    }
}
