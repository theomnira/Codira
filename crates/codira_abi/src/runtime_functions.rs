//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The catalogue of runtime functions: every function the runtime provides
//!   for generated code to call (`new`, `codira.string_*`, `codira.array_*`,
//!   the panics, printing, formatting, tasks), with its signature and what it
//!   may do: allocate, panic, return an error, block, park at a safepoint,
//!   touch state shared with other threads, take data-dependent time, and
//!   whether it is safe in a `@realtime` function.
//!
//! This is the one place that knowledge lives. The LLVM backend's
//! declarations (`codira_codegen::intrinsics`), the Cranelift backend's
//! imports (`codira_cranelift`) and the effect analysis
//! (`codira_hir::realtime`) read it, and tests check that the runtime
//! registers exactly these functions with exactly these signatures
//! (`codira_runtime`'s `catalogue_matches_registration`) and that neither
//! backend references a runtime symbol missing from it
//! (`codira_abi/tests/runtime_catalogue.rs`).
//!
//! Native services that the standard library reaches through `extern`
//! declarations (`codira_fs_*`, `codira_time_*`, ...) are not here: generated
//! code does not call them on its own, and the effect analysis treats every
//! `extern` function as opaque unless its declaration asserts otherwise.
//!
//! # Adding a function
//!
//! Add an entry to [`CATALOGUE`] next to its group, register it in the
//! runtime, and declare it in the backends that call it. The tests name
//! whichever of the three is missing.

/// How a parameter or result is passed, as the C ABI sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Repr {
    /// No value (`void`).
    Unit,
    /// A byte: a `bool` (0 or 1), a flag or a small code.
    U8,
    /// A 32-bit unsigned integer (a line, a column, a code point, flags).
    U32,
    /// A 32-bit signed integer.
    I32,
    /// A 64-bit signed integer.
    I64,
    /// A 64-bit unsigned integer (also each half of a 128-bit one).
    U64,
    /// A pointer-sized unsigned integer: a length, a capacity, an offset or
    /// an opaque runtime handle (a scope, a channel, an endpoint).
    Usize,
    /// An IEEE 754 single.
    F32,
    /// An IEEE 754 double.
    F64,
    /// `*const u8`: bytes the callee reads.
    ConstBytes,
    /// `*mut u8`: a slot the callee writes (a `String`, an offset, a parsed
    /// value).
    MutBytes,
    /// `*const c_void`: a type handle, or a GC handle the callee reads.
    Handle,
    /// `*mut c_void`: the allocator handle.
    MutHandle,
    /// `*const *mut c_void`: a GC object handle (a pointer to the object's
    /// header, whose first word is the payload address).
    Object,
}

impl Repr {
    /// The size of the value in bytes on a target with `pointer_size`-byte
    /// pointers.
    pub const fn size(self, pointer_size: usize) -> usize {
        match self {
            Repr::Unit => 0,
            Repr::U8 => 1,
            Repr::U32 | Repr::I32 | Repr::F32 => 4,
            Repr::I64 | Repr::U64 | Repr::F64 => 8,
            Repr::Usize
            | Repr::ConstBytes
            | Repr::MutBytes
            | Repr::Handle
            | Repr::MutHandle
            | Repr::Object => pointer_size,
        }
    }

    /// Whether the value is a float (passed in a floating-point register).
    pub const fn is_float(self) -> bool {
        matches!(self, Repr::F32 | Repr::F64)
    }

    /// Whether the value is an address.
    pub const fn is_pointer(self) -> bool {
        matches!(
            self,
            Repr::ConstBytes | Repr::MutBytes | Repr::Handle | Repr::MutHandle | Repr::Object
        )
    }
}

/// What a function is for, which is how the backends collect them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    /// Allocation on the garbage-collected heap.
    Gc,
    /// `print`/`println`.
    Print,
    /// Run-time checks that end the program, and the byte comparison a
    /// string-literal `match` arm makes.
    Panic,
    /// `String` operations.
    String,
    /// Format specs (`f"{x:>8}"`), `{x:?}` and `main`'s error exit.
    Format,
    /// Array operations generated code delegates.
    Array,
    /// `scope`, `spawn`, channels and sessions.
    Tasks,
    /// Fuel instrumentation (`codira build --fuel`).
    Fuel,
}

/// Where a function may allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Alloc {
    /// It never allocates.
    No,
    /// On the garbage-collected heap: a GC allocation is a safepoint and,
    /// at the heap limit, collects.
    GcHeap,
    /// On the system heap (formatting scratch, task bookkeeping): it takes
    /// the system allocator's locks and aborts when memory runs out.
    SystemHeap,
}

/// Whether, and how, a function ends the program instead of returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Panics {
    /// It always returns.
    Never,
    /// Only when memory runs out: the process aborts with a message.
    OnOutOfMemory,
    /// When its arguments break the documented rule (an out-of-range slice,
    /// an empty separator), at the source location it is given, with status
    /// 101; and when memory runs out.
    OnBadArgument,
    /// It is the panic: it never returns.
    Always,
}

/// How a function reports that it could not do what was asked, other than
/// by panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorReturn {
    /// It cannot fail.
    None,
    /// It returns `u8` 1 on success and 0 on failure, and writes its value
    /// to its out-slot only on success (`string_parse_i64`, ...).
    StatusFlag,
}

/// State a function reads or writes that another thread can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shared {
    /// The garbage-collected heap (its lock and its accounting).
    GcHeap,
    /// The system allocator.
    SystemHeap,
    /// Standard output (under its lock).
    Stdout,
    /// Standard error.
    Stderr,
    /// The process as a whole: it exits or aborts.
    Process,
    /// The task pool, a scope's bookkeeping, channels and sessions.
    Tasks,
    /// The fuel counter.
    Fuel,
}

/// One runtime function: its name in the dispatch table, its C signature
/// and what it may do. See the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFunction {
    /// The name the runtime registers it under and generated code imports.
    /// Except for `new` and `new_array` these start with `codira.`, which no
    /// Codira function can be named.
    pub symbol: &'static str,
    /// What it is for.
    pub group: Group,
    /// The parameters, by name, in order.
    pub params: &'static [(&'static str, Repr)],
    /// The result.
    pub ret: Repr,
    /// Where it may allocate.
    pub alloc: Alloc,
    /// Whether it may end the program.
    pub panics: Panics,
    /// Whether it reports failure through its result.
    pub error: ErrorReturn,
    /// Whether it may wait for another thread: a lock, a full or empty
    /// channel, a scope's tasks, or another thread's collection.
    pub blocks: bool,
    /// Whether the calling thread may park in it while another thread
    /// collects, so that every GC reference the caller holds must be
    /// visible to the collector (on its stack, today, which is scanned
    /// conservatively) across the call.
    pub safepoint: bool,
    /// What state shared with other threads it touches.
    pub shared: &'static [Shared],
    /// Whether its running time is independent of the contents of the data
    /// it is given (it may depend on lengths and sizes): what an `oblivious`
    /// function may call on secret data.
    pub constant_time: bool,
    /// Whether a `@realtime` function may call it: it neither allocates,
    /// blocks, panics nor parks, and its running time is bounded by the
    /// sizes of its arguments.
    pub realtime_safe: bool,
    /// Whether it starts concurrent work.
    pub spawns: bool,
    /// What it does.
    pub doc: &'static str,
}

impl RuntimeFunction {
    /// A function that reads its arguments, returns, and does nothing else:
    /// no allocation, no panic, no blocking, no shared state. The
    /// `with_*`/`allocating_*` methods below add what it does do.
    const fn pure(
        symbol: &'static str,
        group: Group,
        params: &'static [(&'static str, Repr)],
        ret: Repr,
        doc: &'static str,
    ) -> Self {
        Self {
            symbol,
            group,
            params,
            ret,
            alloc: Alloc::No,
            panics: Panics::Never,
            error: ErrorReturn::None,
            blocks: false,
            safepoint: false,
            shared: &[],
            constant_time: false,
            realtime_safe: true,
            spawns: false,
            doc,
        }
    }

    /// Allocates on the GC heap: a safepoint, which takes the heap's lock,
    /// may wait for or run a collection, and aborts when memory runs out.
    const fn allocating_gc(mut self) -> Self {
        self.alloc = Alloc::GcHeap;
        if matches!(self.panics, Panics::Never) {
            self.panics = Panics::OnOutOfMemory;
        }
        self.blocks = true;
        self.safepoint = true;
        self.shared = &[Shared::GcHeap];
        self.realtime_safe = false;
        self
    }

    /// Allocates on the system heap.
    const fn allocating_system(mut self, shared: &'static [Shared]) -> Self {
        self.alloc = Alloc::SystemHeap;
        if matches!(self.panics, Panics::Never) {
            self.panics = Panics::OnOutOfMemory;
        }
        self.blocks = true;
        self.shared = shared;
        self.realtime_safe = false;
        self
    }

    /// Panics on an argument that breaks its rule.
    const fn checking(mut self) -> Self {
        self.panics = Panics::OnBadArgument;
        self.realtime_safe = false;
        self
    }

    /// Reports failure with a status flag.
    const fn fallible(mut self) -> Self {
        self.error = ErrorReturn::StatusFlag;
        self
    }

    /// Waits for other threads, parked: a collection may run meanwhile.
    const fn blocking(mut self, shared: &'static [Shared]) -> Self {
        self.blocks = true;
        self.safepoint = true;
        self.shared = shared;
        self.realtime_safe = false;
        self
    }

    /// Runs in time independent of its data's contents.
    const fn constant_time(mut self) -> Self {
        self.constant_time = true;
        self
    }

    /// Whether it can end the program instead of returning normally.
    pub const fn can_panic(&self) -> bool {
        !matches!(self.panics, Panics::Never)
    }

    /// Whether it never returns.
    pub const fn never_returns(&self) -> bool {
        matches!(self.panics, Panics::Always)
    }

    /// Whether it allocates, anywhere.
    pub const fn allocates(&self) -> bool {
        !matches!(self.alloc, Alloc::No)
    }
}

use Repr::*;

const TY_ALLOC: [(&str, Repr); 2] = [("ty", Handle), ("alloc", MutHandle)];

macro_rules! with_alloc_out {
    ($($param:expr),* $(,)?) => {
        &[$($param,)* TY_ALLOC[0], TY_ALLOC[1], ("out", MutBytes)]
    };
}

/// `new`.
pub const NEW: RuntimeFunction = RuntimeFunction::pure(
    "new",
    Group::Gc,
    &[("type_handle", Handle), ("alloc_handle", MutHandle)],
    Object,
    "Allocates an object of the type `type_handle` names (its payload uninitialized) and \
     returns its handle.",
)
.allocating_gc();

/// `new_array`.
pub const NEW_ARRAY: RuntimeFunction = RuntimeFunction::pure(
    "new_array",
    Group::Gc,
    &[
        ("type_handle", Handle),
        ("length", Usize),
        ("alloc_handle", MutHandle),
    ],
    Object,
    "Allocates an array of the array type `type_handle` names with room for `length` \
     elements, its length set to `length` and its elements uninitialized.",
)
.allocating_gc();

/// `codira.array_grow`.
pub const ARRAY_GROW: RuntimeFunction = RuntimeFunction::pure(
    "codira.array_grow",
    Group::Gc,
    &[
        ("array", Object),
        ("min_capacity", Usize),
        ("alloc_handle", MutHandle),
    ],
    Unit,
    "Makes room in `array` for at least `min_capacity` elements: moves its elements to a \
     buffer at least twice as large (and at least 4). The array keeps its handle.",
)
.allocating_gc();

/// `codira.panic`.
pub const PANIC: RuntimeFunction = RuntimeFunction {
    panics: Panics::Always,
    shared: &[Shared::Stdout, Shared::Stderr, Shared::Process],
    realtime_safe: false,
    ..RuntimeFunction::pure(
        "codira.panic",
        Group::Panic,
        &[
            ("message", ConstBytes),
            ("message_len", Usize),
            ("file", ConstBytes),
            ("file_len", Usize),
            ("line", U32),
            ("column", U32),
        ],
        Unit,
        "Flushes standard output, writes `panic at file:line:column: message` to standard \
         error and exits with status 101.",
    )
};

/// `codira.panic_index_out_of_bounds`.
pub const PANIC_INDEX_OUT_OF_BOUNDS: RuntimeFunction = RuntimeFunction {
    alloc: Alloc::SystemHeap,
    ..RuntimeFunction {
        symbol: "codira.panic_index_out_of_bounds",
        params: &[
            ("index", U64),
            ("index_is_signed", U8),
            ("len", U64),
            ("file", ConstBytes),
            ("file_len", Usize),
            ("line", U32),
            ("column", U32),
        ],
        doc: "Panics (as `codira.panic`) with `index out of bounds: the length is {len} but \
              the index is {index}`, the index read as signed when `index_is_signed` is set.",
        ..PANIC
    }
};

/// `codira.str_eq`.
pub const STR_EQ: RuntimeFunction = RuntimeFunction::pure(
    "codira.str_eq",
    Group::Panic,
    &[
        ("a", ConstBytes),
        ("a_len", Usize),
        ("b", ConstBytes),
        ("b_len", Usize),
    ],
    U8,
    "1 when the two strings' bytes are equal, 0 when not (a string-literal `match` arm).",
);

const PRINT_SHARED: &[Shared] = &[Shared::Stdout, Shared::SystemHeap];

const fn print(
    symbol: &'static str,
    params: &'static [(&'static str, Repr)],
    doc: &'static str,
) -> RuntimeFunction {
    RuntimeFunction::pure(symbol, Group::Print, params, Unit, doc).allocating_system(PRINT_SHARED)
}

/// `codira.print_str`.
pub const PRINT_STR: RuntimeFunction = print(
    "codira.print_str",
    &[("data", ConstBytes), ("len", Usize)],
    "Writes `len` bytes of UTF-8 to standard output (or the thread's capture buffer).",
);
/// `codira.print_i64`.
pub const PRINT_I64: RuntimeFunction = print(
    "codira.print_i64",
    &[("value", I64)],
    "Writes a signed integer in decimal.",
);
/// `codira.print_u64`.
pub const PRINT_U64: RuntimeFunction = print(
    "codira.print_u64",
    &[("value", U64)],
    "Writes an unsigned integer in decimal.",
);
/// `codira.print_i128`.
pub const PRINT_I128: RuntimeFunction = print(
    "codira.print_i128",
    &[("low", U64), ("high", U64)],
    "Writes a signed 128-bit integer, passed as its halves, in decimal.",
);
/// `codira.print_u128`.
pub const PRINT_U128: RuntimeFunction = print(
    "codira.print_u128",
    &[("low", U64), ("high", U64)],
    "Writes an unsigned 128-bit integer, passed as its halves, in decimal.",
);
/// `codira.print_f32`.
pub const PRINT_F32: RuntimeFunction = print(
    "codira.print_f32",
    &[("value", F32)],
    "Writes an `f32` in its shortest round-trip form, with a decimal point or exponent.",
);
/// `codira.print_f64`.
pub const PRINT_F64: RuntimeFunction = print(
    "codira.print_f64",
    &[("value", F64)],
    "Writes an `f64` in its shortest round-trip form, with a decimal point or exponent.",
);
/// `codira.print_bool`.
pub const PRINT_BOOL: RuntimeFunction = print(
    "codira.print_bool",
    &[("value", U8)],
    "Writes `true` (non-zero) or `false`.",
);

/// `codira.region_open`.
pub const REGION_OPEN: RuntimeFunction = RuntimeFunction::pure(
    "codira.region_open",
    Group::Gc,
    &[("alloc_handle", MutHandle)],
    MutHandle,
    "Opens a region arena for a `@region` function and returns its handle: a chunk from \
     the thread's cache or the system, registered as a span of conservative roots.",
)
.allocating_system(&[Shared::SystemHeap, Shared::GcHeap]);

/// `codira.region_alloc`.
pub const REGION_ALLOC: RuntimeFunction = RuntimeFunction::pure(
    "codira.region_alloc",
    Group::Gc,
    &[
        ("region", MutHandle),
        ("size", Usize),
        ("align", Usize),
        ("alloc_handle", MutHandle),
    ],
    Object,
    "Allocates an object with a zeroed `size`-byte payload in the open `region`, taking a \
     new chunk when the current one is full.",
)
.allocating_system(&[Shared::SystemHeap, Shared::GcHeap]);

/// `codira.region_close`.
pub const REGION_CLOSE: RuntimeFunction = RuntimeFunction {
    panics: Panics::Always,
    ..RuntimeFunction::pure(
        "codira.region_close",
        Group::Gc,
        &[("region", MutHandle), ("alloc_handle", MutHandle)],
        Unit,
        "Releases every object of `region` at once and returns its chunks to the thread's \
         cache. With region checks on (debug builds, `CODIRA_CHECK_REGIONS=1`) it first stops \
         the world and aborts if anything still refers into the region.",
    )
    .blocking(&[Shared::SystemHeap, Shared::GcHeap, Shared::Process])
};

/// `codira.gc.foreign_enter`.
pub const GC_FOREIGN_ENTER: RuntimeFunction = RuntimeFunction::pure(
    "codira.gc.foreign_enter",
    Group::Gc,
    &[("frame", ConstBytes)],
    Unit,
    "Marks the thread as running foreign code before a call to a `@noalloc` extern \
     function, so a collection does not wait for it.",
);

/// `codira.gc.foreign_leave`.
pub const GC_FOREIGN_LEAVE: RuntimeFunction = RuntimeFunction::pure(
    "codira.gc.foreign_leave",
    Group::Gc,
    &[],
    Unit,
    "Returns the thread to Codira code after a foreign call, waiting while a collection \
     that started meanwhile runs.",
)
.blocking(&[Shared::GcHeap]);

/// `codira.fuel_exhausted`.
pub const FUEL_EXHAUSTED: RuntimeFunction = RuntimeFunction {
    panics: Panics::Always,
    shared: &[Shared::Fuel, Shared::Stderr, Shared::Process],
    realtime_safe: false,
    ..RuntimeFunction::pure(
        crate::FUEL_EXHAUSTED_FN_NAME,
        Group::Fuel,
        &[],
        Unit,
        "Reports that fuel-instrumented code used up its budget and ends the call (a \
         structured exception on Windows that a HERACLES guard catches; an abort elsewhere).",
    )
};

const fn string_pure(
    symbol: &'static str,
    params: &'static [(&'static str, Repr)],
    ret: Repr,
    doc: &'static str,
) -> RuntimeFunction {
    RuntimeFunction::pure(symbol, Group::String, params, ret, doc)
}

const fn string_alloc(
    symbol: &'static str,
    params: &'static [(&'static str, Repr)],
    ret: Repr,
    doc: &'static str,
) -> RuntimeFunction {
    RuntimeFunction::pure(symbol, Group::String, params, ret, doc).allocating_gc()
}

const TWO_TEXTS: &[(&str, Repr)] = &[
    ("a", ConstBytes),
    ("a_len", Usize),
    ("b", ConstBytes),
    ("b_len", Usize),
];
const HAY_NEEDLE: &[(&str, Repr)] = &[
    ("hay", ConstBytes),
    ("hay_len", Usize),
    ("needle", ConstBytes),
    ("len", Usize),
];
const TEXT_PREFIX: &[(&str, Repr)] = &[
    ("s", ConstBytes),
    ("s_len", Usize),
    ("p", ConstBytes),
    ("p_len", Usize),
];

/// `codira.string_from_bytes`.
pub const STRING_FROM_BYTES: RuntimeFunction = string_alloc(
    "codira.string_from_bytes",
    with_alloc_out![("data", ConstBytes), ("len", Usize)],
    Unit,
    "A new `String` of the `len` bytes at `data` (valid UTF-8), written to `out`.",
);
/// `codira.string_append`.
pub const STRING_APPEND: RuntimeFunction = string_alloc(
    "codira.string_append",
    with_alloc_out![
        ("buf", Handle),
        ("start", Usize),
        ("len", Usize),
        ("data", ConstBytes),
        ("data_len", Usize),
    ],
    Unit,
    "`buf/start/len` followed by `data`, in place when the buffer has room past the end of \
     this value and nothing else was appended there; written to `out`.",
);
/// `codira.string_concat`.
pub const STRING_CONCAT: RuntimeFunction = string_alloc(
    "codira.string_concat",
    with_alloc_out![
        ("a", ConstBytes),
        ("a_len", Usize),
        ("b", ConstBytes),
        ("b_len", Usize),
    ],
    Unit,
    "A new `String` of `a` followed by `b`, written to `out`.",
);
/// `codira.string_with_capacity`.
pub const STRING_WITH_CAPACITY: RuntimeFunction = string_alloc(
    "codira.string_with_capacity",
    with_alloc_out![("capacity", Usize)],
    Unit,
    "The empty `String` at the start of a new buffer with room for `capacity` bytes: the \
     start of a compiled format plan.",
);
/// `codira.string_append_int`.
pub const STRING_APPEND_INT: RuntimeFunction = string_alloc(
    "codira.string_append_int",
    with_alloc_out![
        ("buf", Handle),
        ("start", Usize),
        ("len", Usize),
        ("low", U64),
        ("high", U64),
        ("signed", U8),
    ],
    Unit,
    "`buf/start/len` with the decimal digits of an integer (its halves and signedness) \
     appended, in place when it can be: the slow path of `s += n.to_string()`.",
);
/// `codira.string_append_float`.
pub const STRING_APPEND_FLOAT: RuntimeFunction = string_alloc(
    "codira.string_append_float",
    with_alloc_out![
        ("buf", Handle),
        ("start", Usize),
        ("len", Usize),
        ("value", F64),
        ("is_f32", U8),
    ],
    Unit,
    "`buf/start/len` with a float's `to_string()` appended, in place when it can be.",
);
/// `codira.string_take_utf8`.
pub const STRING_TAKE_UTF8: RuntimeFunction = string_alloc(
    "codira.string_take_utf8",
    with_alloc_out![("array", Handle)],
    U8,
    "The bytes of the `[u8]` `array` as a `String` written to `out`, when they are valid \
     UTF-8 (returns 1; else 0 and nothing written). Takes the array's buffer over when it \
     can, leaving the array empty; otherwise copies.",
)
.fallible();
/// `codira.bytes_format_json_f64`.
pub const BYTES_FORMAT_JSON_F64: RuntimeFunction = RuntimeFunction::pure(
    "codira.bytes_format_json_f64",
    Group::Format,
    &[("value", F64), ("out", MutBytes)],
    Usize,
    "Writes the JSON text of `value` (shortest round-trip form, `null` for a NaN or an \
     infinity) to the 24 bytes at `out` and returns its length.",
);
/// `codira.string_find_byte`.
pub const STRING_FIND_BYTE: RuntimeFunction = string_pure(
    "codira.string_find_byte",
    &[("data", ConstBytes), ("len", Usize), ("byte", U8)],
    I64,
    "The offset of the first `byte` in the `len` bytes at `data`, or -1 (`memchr`).",
);
/// `codira.string_eq`.
pub const STRING_EQ: RuntimeFunction = string_pure(
    "codira.string_eq",
    TWO_TEXTS,
    U8,
    "1 when the two strings' bytes are equal.",
);
/// `codira.string_cmp`.
pub const STRING_CMP: RuntimeFunction = string_pure(
    "codira.string_cmp",
    TWO_TEXTS,
    I64,
    "-1, 0 or 1 by byte order.",
);
/// `codira.string_hash`.
pub const STRING_HASH: RuntimeFunction = string_pure(
    "codira.string_hash",
    &[("data", ConstBytes), ("len", Usize)],
    U64,
    "FNV-1a 64 of the bytes.",
)
.constant_time();
/// `codira.string_char_count`.
pub const STRING_CHAR_COUNT: RuntimeFunction = string_pure(
    "codira.string_char_count",
    &[("data", ConstBytes), ("len", Usize)],
    Usize,
    "The number of code points.",
);
/// `codira.string_find`.
pub const STRING_FIND: RuntimeFunction = string_pure(
    "codira.string_find",
    HAY_NEEDLE,
    I64,
    "The byte offset of the first occurrence of `needle`, or -1.",
);
/// `codira.string_rfind`.
pub const STRING_RFIND: RuntimeFunction = string_pure(
    "codira.string_rfind",
    HAY_NEEDLE,
    I64,
    "The byte offset of the last occurrence of `needle`, or -1.",
);
/// `codira.string_contains`.
pub const STRING_CONTAINS: RuntimeFunction = string_pure(
    "codira.string_contains",
    HAY_NEEDLE,
    U8,
    "1 when `needle` occurs in `hay`.",
);
/// `codira.string_starts_with`.
pub const STRING_STARTS_WITH: RuntimeFunction = string_pure(
    "codira.string_starts_with",
    TEXT_PREFIX,
    U8,
    "1 when `s` starts with `p`.",
);
/// `codira.string_ends_with`.
pub const STRING_ENDS_WITH: RuntimeFunction = string_pure(
    "codira.string_ends_with",
    TEXT_PREFIX,
    U8,
    "1 when `s` ends with `p`.",
);
/// `codira.string_split`.
pub const STRING_SPLIT: RuntimeFunction = string_alloc(
    "codira.string_split",
    &[
        ("buf", Handle),
        ("start", Usize),
        ("len", Usize),
        ("sep", ConstBytes),
        ("sep_len", Usize),
        ("ty", Handle),
        ("alloc", MutHandle),
        ("file", ConstBytes),
        ("file_len", Usize),
        ("line", U32),
        ("column", U32),
    ],
    Handle,
    "The pieces between occurrences of `sep`, as a new `[String]` (`ty`) whose pieces share \
     the buffer; panics at the location given on an empty separator.",
)
.checking();
/// `codira.string_lines`.
pub const STRING_LINES: RuntimeFunction = string_alloc(
    "codira.string_lines",
    &[
        ("buf", Handle),
        ("start", Usize),
        ("len", Usize),
        ("ty", Handle),
        ("alloc", MutHandle),
    ],
    Handle,
    "The lines (split at `\\n`, a `\\r` before it dropped), as a new `[String]` (`ty`).",
);
/// `codira.string_trim`.
pub const STRING_TRIM: RuntimeFunction = string_pure(
    "codira.string_trim",
    &[
        ("data", ConstBytes),
        ("len", Usize),
        ("mode", U8),
        ("out_offset", MutBytes),
    ],
    Usize,
    "Trims white space (`mode` 0 both ends, 1 the start, 2 the end): returns the new length \
     and writes the offset of the first byte kept to `out_offset`.",
);
/// `codira.string_change_case`.
pub const STRING_CHANGE_CASE: RuntimeFunction = string_alloc(
    "codira.string_change_case",
    with_alloc_out![("data", ConstBytes), ("len", Usize), ("upper", U8)],
    Unit,
    "ASCII upper- (`upper` 1) or lower-casing, into a new `String`.",
);
/// `codira.string_replace`.
pub const STRING_REPLACE: RuntimeFunction = string_alloc(
    "codira.string_replace",
    with_alloc_out![
        ("data", ConstBytes),
        ("len", Usize),
        ("from", ConstBytes),
        ("from_len", Usize),
        ("to", ConstBytes),
        ("to_len", Usize),
    ],
    Unit,
    "Every `from` replaced by `to`, into a new `String`.",
);
/// `codira.string_repeat`.
pub const STRING_REPEAT: RuntimeFunction = string_alloc(
    "codira.string_repeat",
    with_alloc_out![
        ("data", ConstBytes),
        ("len", Usize),
        ("count", U64),
        ("signed", U8),
    ],
    Unit,
    "`count` copies (a negative signed count is none), into a new `String`; aborts when the \
     result would not fit in memory.",
);
/// `codira.string_check_slice`.
pub const STRING_CHECK_SLICE: RuntimeFunction = string_pure(
    "codira.string_check_slice",
    &[
        ("data", ConstBytes),
        ("len", Usize),
        ("start", U64),
        ("start_signed", U8),
        ("end", U64),
        ("end_signed", U8),
        ("file", ConstBytes),
        ("file_len", Usize),
        ("line", U32),
        ("column", U32),
    ],
    Unit,
    "Panics at the location given unless `start..end` is a valid slice of the string: in \
     range, ordered, on character boundaries.",
)
.checking();
/// `codira.string_parse_i64`.
pub const STRING_PARSE_I64: RuntimeFunction = string_pure(
    "codira.string_parse_i64",
    &[("data", ConstBytes), ("len", Usize), ("out", MutBytes)],
    U8,
    "1 and the value at `out` when the string is a decimal `i64`, 0 otherwise.",
)
.fallible();
/// `codira.string_parse_f64`.
pub const STRING_PARSE_F64: RuntimeFunction = string_pure(
    "codira.string_parse_f64",
    &[("data", ConstBytes), ("len", Usize), ("out", MutBytes)],
    U8,
    "1 and the value at `out` when the string is an `f64`, 0 otherwise.",
)
.fallible();
/// `codira.string_from_i64`.
pub const STRING_FROM_I64: RuntimeFunction = string_alloc(
    "codira.string_from_i64",
    with_alloc_out![("value", I64)],
    Unit,
    "A signed integer in decimal, as a new `String`.",
);
/// `codira.string_from_u64`.
pub const STRING_FROM_U64: RuntimeFunction = string_alloc(
    "codira.string_from_u64",
    with_alloc_out![("value", U64)],
    Unit,
    "An unsigned integer in decimal, as a new `String`.",
);
/// `codira.string_from_i128`.
pub const STRING_FROM_I128: RuntimeFunction = string_alloc(
    "codira.string_from_i128",
    with_alloc_out![("low", U64), ("high", U64), ("signed", U8)],
    Unit,
    "A 128-bit integer, as its halves and signedness, in decimal, as a new `String`.",
);
/// `codira.string_from_f64`.
pub const STRING_FROM_F64: RuntimeFunction = string_alloc(
    "codira.string_from_f64",
    with_alloc_out![("value", F64)],
    Unit,
    "An `f64` as `print` writes it, as a new `String`.",
);
/// `codira.string_from_f32`.
pub const STRING_FROM_F32: RuntimeFunction = string_alloc(
    "codira.string_from_f32",
    with_alloc_out![("value", F32)],
    Unit,
    "An `f32` as `print` writes it, as a new `String`.",
);
/// `codira.string_from_bool`.
pub const STRING_FROM_BOOL: RuntimeFunction = string_alloc(
    "codira.string_from_bool",
    with_alloc_out![("value", U8)],
    Unit,
    "`true` or `false`, as a new `String`.",
);
/// `codira.string_chars`.
pub const STRING_CHARS: RuntimeFunction = string_alloc(
    "codira.string_chars",
    &[
        ("data", ConstBytes),
        ("len", Usize),
        ("ty", Handle),
        ("alloc", MutHandle),
    ],
    Handle,
    "The code points, as a new `[u32]` (`ty`).",
);
/// `codira.string_to_bytes`.
pub const STRING_TO_BYTES: RuntimeFunction = string_alloc(
    "codira.string_to_bytes",
    &[
        ("data", ConstBytes),
        ("len", Usize),
        ("ty", Handle),
        ("alloc", MutHandle),
    ],
    Handle,
    "A copy of the bytes, as a new `[u8]` (`ty`).",
);
/// `codira.string_from_utf8`.
pub const STRING_FROM_UTF8: RuntimeFunction = string_alloc(
    "codira.string_from_utf8",
    with_alloc_out![("array", Handle)],
    U8,
    "1 and a new `String` at `out` when the `[u8]` holds valid UTF-8, 0 otherwise.",
)
.fallible();

const fn format_alloc(
    symbol: &'static str,
    params: &'static [(&'static str, Repr)],
    ret: Repr,
    doc: &'static str,
) -> RuntimeFunction {
    RuntimeFunction::pure(symbol, Group::Format, params, ret, doc).allocating_gc()
}

/// `codira.string_append_format_int`.
pub const STRING_APPEND_FORMAT_INT: RuntimeFunction = format_alloc(
    "codira.string_append_format_int",
    with_alloc_out![
        ("buf", Handle),
        ("start", Usize),
        ("len", Usize),
        ("low", U64),
        ("high", U64),
        ("signed", U8),
        ("bits", U32),
        ("fill", U32),
        ("flags", U32),
        ("width", I64),
        ("precision", I64),
    ],
    Unit,
    "`buf/start/len` with an integer (its halves, signedness and size in bits) appended as \
     a format spec says (a fill character, `FormatSpec::abi_flags`, a width and a \
     precision), in place when it can be.",
);
/// `codira.string_append_format_float`.
pub const STRING_APPEND_FORMAT_FLOAT: RuntimeFunction = format_alloc(
    "codira.string_append_format_float",
    with_alloc_out![
        ("buf", Handle),
        ("start", Usize),
        ("len", Usize),
        ("value", F64),
        ("is_f32", U8),
        ("fill", U32),
        ("flags", U32),
        ("width", I64),
        ("precision", I64),
    ],
    Unit,
    "`buf/start/len` with a float (an `f32` widened when `is_f32` is set) appended as a \
     format spec says, in place when it can be.",
);
/// `codira.string_append_format_text`.
pub const STRING_APPEND_FORMAT_TEXT: RuntimeFunction = format_alloc(
    "codira.string_append_format_text",
    with_alloc_out![
        ("buf", Handle),
        ("start", Usize),
        ("len", Usize),
        ("data", ConstBytes),
        ("len", Usize),
        ("fill", U32),
        ("flags", U32),
        ("width", I64),
        ("precision", I64),
    ],
    Unit,
    "`buf/start/len` with text appended padded and truncated as a format spec says.",
);
/// `codira.debug_quote`.
pub const DEBUG_QUOTE: RuntimeFunction = format_alloc(
    "codira.debug_quote",
    with_alloc_out![
        ("buf", Handle),
        ("start", Usize),
        ("len", Usize),
        ("data", ConstBytes),
        ("data_len", Usize),
    ],
    Unit,
    "`buf/start/len` with `data` appended quoted, as a Codira string literal.",
);
/// `codira.debug_newline`.
pub const DEBUG_NEWLINE: RuntimeFunction = format_alloc(
    "codira.debug_newline",
    with_alloc_out![
        ("buf", Handle),
        ("start", Usize),
        ("len", Usize),
        ("depth", I64),
    ],
    Unit,
    "`buf/start/len` with a line break and `depth` levels of indentation appended.",
);
/// `codira.debug_enter`.
pub const DEBUG_ENTER: RuntimeFunction = RuntimeFunction {
    // A push onto a thread-local stack, which can grow.
    alloc: Alloc::SystemHeap,
    panics: Panics::OnOutOfMemory,
    blocks: true,
    shared: &[Shared::SystemHeap],
    realtime_safe: false,
    ..RuntimeFunction::pure(
        "codira.debug_enter",
        Group::Format,
        &[("object", Handle)],
        U8,
        "1 when the class instance `object` is not already being written by `{x:?}` further \
         up this thread's stack (it is then recorded until `debug_leave`), 0 on a reference \
         cycle.",
    )
};
/// `codira.debug_leave`.
pub const DEBUG_LEAVE: RuntimeFunction = RuntimeFunction::pure(
    "codira.debug_leave",
    Group::Format,
    &[],
    Unit,
    "Ends the innermost instance `debug_enter` recorded.",
)
.constant_time();
/// `codira.exit_with_error`.
pub const EXIT_WITH_ERROR: RuntimeFunction = RuntimeFunction {
    panics: Panics::Always,
    shared: &[Shared::Stdout, Shared::Stderr, Shared::Process],
    realtime_safe: false,
    ..RuntimeFunction::pure(
        "codira.exit_with_error",
        Group::Format,
        &[("data", ConstBytes), ("len", Usize)],
        Unit,
        "What `main` does with an `Err` it returns: flushes standard output, writes `Error: \
         <text>` to standard error and exits with status 1.",
    )
};

/// `codira.array_sort`.
pub const ARRAY_SORT: RuntimeFunction = RuntimeFunction::pure(
    "codira.array_sort",
    Group::Array,
    &[("data", MutBytes), ("len", Usize), ("kind", U32)],
    Unit,
    "Sorts the `len` elements at `data`, of the element kind `kind`, in place (unstable, \
     without allocating; floats by IEEE 754 total order).",
);
/// `codira.array_binary_search`.
pub const ARRAY_BINARY_SEARCH: RuntimeFunction = RuntimeFunction::pure(
    "codira.array_binary_search",
    Group::Array,
    &[
        ("data", MutBytes),
        ("len", Usize),
        ("kind", U32),
        ("key", ConstBytes),
    ],
    I64,
    "The index of an element equal to the one at `key` in the sorted `len` elements at \
     `data`, or -1.",
);
/// `codira.array_reverse`.
pub const ARRAY_REVERSE: RuntimeFunction = RuntimeFunction::pure(
    "codira.array_reverse",
    Group::Array,
    &[("data", MutBytes), ("len", Usize), ("stride", Usize)],
    Unit,
    "Reverses the `len` elements of `stride` bytes at `data` in place.",
)
.constant_time();

const TASKS: &[Shared] = &[Shared::Tasks, Shared::SystemHeap];

const fn task(
    symbol: &'static str,
    params: &'static [(&'static str, Repr)],
    ret: Repr,
    doc: &'static str,
) -> RuntimeFunction {
    RuntimeFunction::pure(symbol, Group::Tasks, params, ret, doc)
}

/// `codira.scope_begin`.
pub const SCOPE_BEGIN: RuntimeFunction = task(
    "codira.scope_begin",
    &[],
    Usize,
    "Opens a `scope`: returns a handle that owns the scope's tasks and channels.",
)
.allocating_system(TASKS);
/// `codira.scope_spawn`.
pub const SCOPE_SPAWN: RuntimeFunction = RuntimeFunction {
    spawns: true,
    ..task(
        "codira.scope_spawn",
        &[
            ("scope", Usize),
            ("entry", Usize),
            ("args", Usize),
            ("size", Usize),
        ],
        Unit,
        "Runs `entry(copy of args[..size])` as a task of `scope` on the task pool.",
    )
    .allocating_system(TASKS)
};
/// `codira.scope_end`.
pub const SCOPE_END: RuntimeFunction = task(
    "codira.scope_end",
    &[("scope", Usize)],
    Unit,
    "Waits for every task of `scope` (parked), then frees its channels and the scope.",
)
.blocking(TASKS);
/// `codira.channel_new`.
pub const CHANNEL_NEW: RuntimeFunction = task(
    "codira.channel_new",
    &[
        ("scope", Usize),
        ("capacity", Usize),
        ("element_size", Usize),
    ],
    Usize,
    "Creates a channel of `capacity` elements of `element_size` bytes, owned by `scope`.",
)
.allocating_system(TASKS);
/// `codira.channel_send`.
pub const CHANNEL_SEND: RuntimeFunction = task(
    "codira.channel_send",
    &[("channel", Usize), ("value", Usize)],
    Unit,
    "Sends the element at address `value`, waiting (parked) while the channel is full.",
)
.allocating_system(TASKS)
.blocking(TASKS);
/// `codira.channel_receive`.
pub const CHANNEL_RECEIVE: RuntimeFunction = task(
    "codira.channel_receive",
    &[("channel", Usize), ("out", Usize)],
    Unit,
    "Receives the next element into address `out`, waiting (parked) while there is none.",
)
.blocking(TASKS);
/// `codira.channel_select`.
pub const CHANNEL_SELECT: RuntimeFunction = task(
    "codira.channel_select",
    &[("cases", Usize), ("count", Usize), ("has_default", Usize)],
    Usize,
    "Performs one of `count` channel operations laid out at `cases` (channel, 0 = receive / \
     1 = send, data address) and returns its index; returns `count` at once when \
     `has_default` is set and none can proceed. Builds its wait list on the heap.",
)
.allocating_system(TASKS)
.blocking(TASKS);
/// `codira.session_open`.
pub const SESSION_OPEN: RuntimeFunction = task(
    "codira.session_open",
    &[("scope", Usize), ("out", Usize)],
    Unit,
    "Creates the two endpoints of a session in `scope`, writing their handles to the two \
     `usize`s at `out`.",
)
.allocating_system(TASKS);
/// `codira.session_send`.
pub const SESSION_SEND: RuntimeFunction = task(
    "codira.session_send",
    &[("endpoint", Usize), ("value", Usize), ("size", Usize)],
    Unit,
    "Sends the `size` bytes at `value` to the other endpoint, waiting while it has not taken \
     the previous message.",
)
.blocking(TASKS);
/// `codira.session_receive`.
pub const SESSION_RECEIVE: RuntimeFunction = task(
    "codira.session_receive",
    &[("endpoint", Usize), ("out", Usize), ("size", Usize)],
    Unit,
    "Receives the other endpoint's next message into the `size` bytes at `out`, waiting \
     while there is none.",
)
.blocking(TASKS);
/// `codira.group_begin`.
pub const GROUP_BEGIN: RuntimeFunction = task(
    "codira.group_begin",
    &[("result_size", Usize)],
    Usize,
    "Opens a `group` whose tasks return `result_size` bytes each, and returns its handle.",
)
.allocating_system(TASKS);
/// `codira.group_spawn`.
pub const GROUP_SPAWN: RuntimeFunction = RuntimeFunction {
    spawns: true,
    ..task(
        "codira.group_spawn",
        &[
            ("group", Usize),
            ("entry", Usize),
            ("args", Usize),
            ("size", Usize),
        ],
        Unit,
        "Runs `entry(copy of args[..size], result slot)` as a task of `group` on the task pool.",
    )
    .allocating_system(TASKS)
};
/// `codira.group_end`.
pub const GROUP_END: RuntimeFunction = task(
    "codira.group_end",
    &[("group", Usize)],
    Usize,
    "Waits for every task of `group` (parked) and returns how many it has.",
)
.blocking(TASKS);
/// `codira.group_first_error`.
pub const GROUP_FIRST_ERROR: RuntimeFunction = RuntimeFunction {
    shared: &[Shared::Tasks],
    ..task(
        "codira.group_first_error",
        &[("group", Usize)],
        Usize,
        "The lowest index of a task of the joined `group` that returned an `Err`, or the task \
         count.",
    )
};
/// `codira.group_result`.
pub const GROUP_RESULT: RuntimeFunction = RuntimeFunction {
    shared: &[Shared::Tasks],
    ..task(
        "codira.group_result",
        &[("group", Usize), ("index", Usize), ("out", Usize)],
        Unit,
        "Copies the result of task `index` of the joined `group` to address `out`.",
    )
};
/// `codira.group_free`.
pub const GROUP_FREE: RuntimeFunction = RuntimeFunction {
    panics: Panics::OnBadArgument,
    shared: &[Shared::Tasks, Shared::SystemHeap, Shared::Process],
    realtime_safe: false,
    ..task(
        "codira.group_free",
        &[("group", Usize)],
        Unit,
        "Frees the joined `group` and the results it kept; ends the process if its tasks were \
         not joined.",
    )
};
/// `codira.task_cancelled`.
pub const TASK_CANCELLED: RuntimeFunction = RuntimeFunction {
    shared: &[Shared::Tasks],
    ..task(
        "codira.task_cancelled",
        &[],
        Usize,
        "Whether the running task, or one of its enclosing scopes and groups, has been \
         cancelled: one load, no wait.",
    )
};

/// Every runtime function generated code may call.
pub const CATALOGUE: &[RuntimeFunction] = &[
    NEW,
    NEW_ARRAY,
    ARRAY_GROW,
    PANIC,
    PANIC_INDEX_OUT_OF_BOUNDS,
    STR_EQ,
    PRINT_STR,
    PRINT_I64,
    PRINT_U64,
    PRINT_I128,
    PRINT_U128,
    PRINT_F32,
    PRINT_F64,
    PRINT_BOOL,
    FUEL_EXHAUSTED,
    REGION_OPEN,
    REGION_ALLOC,
    REGION_CLOSE,
    GC_FOREIGN_ENTER,
    GC_FOREIGN_LEAVE,
    STRING_FROM_BYTES,
    STRING_APPEND,
    STRING_CONCAT,
    STRING_WITH_CAPACITY,
    STRING_APPEND_INT,
    STRING_APPEND_FLOAT,
    STRING_FIND_BYTE,
    STRING_EQ,
    STRING_CMP,
    STRING_HASH,
    STRING_CHAR_COUNT,
    STRING_FIND,
    STRING_RFIND,
    STRING_CONTAINS,
    STRING_STARTS_WITH,
    STRING_ENDS_WITH,
    STRING_SPLIT,
    STRING_LINES,
    STRING_TRIM,
    STRING_CHANGE_CASE,
    STRING_REPLACE,
    STRING_REPEAT,
    STRING_CHECK_SLICE,
    STRING_PARSE_I64,
    STRING_PARSE_F64,
    STRING_FROM_I64,
    STRING_FROM_U64,
    STRING_FROM_I128,
    STRING_FROM_F64,
    STRING_FROM_F32,
    STRING_FROM_BOOL,
    STRING_CHARS,
    STRING_TO_BYTES,
    STRING_FROM_UTF8,
    STRING_APPEND_FORMAT_INT,
    STRING_APPEND_FORMAT_FLOAT,
    STRING_APPEND_FORMAT_TEXT,
    DEBUG_QUOTE,
    DEBUG_NEWLINE,
    DEBUG_ENTER,
    DEBUG_LEAVE,
    EXIT_WITH_ERROR,
    ARRAY_SORT,
    ARRAY_BINARY_SEARCH,
    ARRAY_REVERSE,
    SCOPE_BEGIN,
    SCOPE_SPAWN,
    SCOPE_END,
    CHANNEL_NEW,
    CHANNEL_SEND,
    CHANNEL_RECEIVE,
    CHANNEL_SELECT,
    SESSION_OPEN,
    SESSION_SEND,
    SESSION_RECEIVE,
    TASK_CANCELLED,
    GROUP_BEGIN,
    GROUP_SPAWN,
    GROUP_END,
    GROUP_FIRST_ERROR,
    GROUP_RESULT,
    GROUP_FREE,
    STRING_TAKE_UTF8,
    BYTES_FORMAT_JSON_F64,
];

/// The catalogue entry for `symbol`.
pub fn lookup(symbol: &str) -> Option<&'static RuntimeFunction> {
    CATALOGUE.iter().find(|f| f.symbol == symbol)
}

/// The entries of one group, in catalogue order.
pub fn group(group: Group) -> impl Iterator<Item = &'static RuntimeFunction> {
    CATALOGUE.iter().filter(move |f| f.group == group)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn symbols_are_unique_and_namespaced() {
        let mut seen = HashSet::new();
        for f in CATALOGUE {
            assert!(seen.insert(f.symbol), "`{}` is listed twice", f.symbol);
            assert!(
                f.symbol.starts_with("codira.") || matches!(f.symbol, "new" | "new_array"),
                "`{}` is outside the `codira.` namespace",
                f.symbol
            );
        }
    }

    #[test]
    fn properties_are_consistent() {
        for f in CATALOGUE {
            if f.realtime_safe {
                assert!(
                    !f.allocates() && !f.can_panic() && !f.blocks && !f.safepoint && !f.spawns,
                    "`{}` is marked realtime-safe but is not",
                    f.symbol
                );
            }
            if matches!(f.alloc, Alloc::GcHeap) {
                assert!(
                    f.safepoint,
                    "`{}` allocates on the GC heap: a safepoint",
                    f.symbol
                );
                assert!(f.shared.contains(&Shared::GcHeap), "`{}`", f.symbol);
            }
            if (f.safepoint || f.allocates()) && !f.never_returns() {
                assert!(f.blocks, "`{}` can wait for another thread", f.symbol);
            }
            if f.never_returns() {
                assert_eq!(f.ret, Repr::Unit, "`{}` never returns a value", f.symbol);
                assert!(f.shared.contains(&Shared::Process), "`{}`", f.symbol);
            }
            if f.error == ErrorReturn::StatusFlag {
                assert_eq!(
                    f.ret,
                    Repr::U8,
                    "`{}` returns its status as a byte",
                    f.symbol
                );
            }
            assert!(
                f.params.iter().all(|(_, repr)| *repr != Repr::Unit),
                "`{}` has a unit parameter",
                f.symbol
            );
        }
    }

    #[test]
    fn every_allocating_string_function_takes_the_allocator() {
        for f in group(Group::String).chain(group(Group::Format)) {
            if matches!(f.alloc, Alloc::GcHeap) {
                assert!(
                    f.params
                        .iter()
                        .any(|&(name, repr)| name == "alloc" && repr == Repr::MutHandle),
                    "`{}`",
                    f.symbol
                );
            }
        }
    }
}
