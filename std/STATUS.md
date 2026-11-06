# Standard library status

What each module of `std/` does today, what shows it, and what stands in
its way. One row per module that exists (96 files, the prelude included)
and one per module that is planned. This page says what the standard
library *is*; [ARCHITECTURE.md](ARCHITECTURE.md) says what it should
become and [ROADMAP.md](ROADMAP.md) in what order.

## Headline

| status | modules | meaning |
|---|---:|---|
| importable | 36 | A program's `import std.<module>.*` checks, builds and loads. |
| checks-but-not-importable | 4 | Clean when `std/` is checked as one package with its prelude, but a program importing it fails: it imports a module with errors, or uses a prelude name. |
| fails | 56 | The module's own file has diagnostics. |
| planned | 50 | No file yet; part of the target layout. |
| unsupported | 7 | Not planned, with the reason given in its row. |

Of the 36 importable modules, 34 are run end to end by a program against
independently computed values; `std.memory.page` and
`std.builtin.type_aliases` are imported but not run.

## How the status column is made

The **status** column is not written by hand. It is derived from the two
standard-library ratchets, and `scripts/std_status.py --check` fails when
this page disagrees with them:

- `crates/codira_compiler/tests/snapshots/stdlib_import_ratchet__stdlib_import_status.snap`
  (does a one-line program `import std.<module>.*` check, build and load?)
  decides **importable**;
- `crates/codira_compiler/tests/snapshots/stdlib_check_ratchet__stdlib_check_status.snap`
  (diagnostics per file when `std/` is checked as one package with its
  prelude) separates **checks-but-not-importable** (no diagnostics of its
  own) from **fails**.

`python3 scripts/std_status.py` prints each module's derived status, the
first error the import ratchet records and its `needs:` reason. When a
ratchet snapshot changes, update the rows here in the same commit.
**planned** and **unsupported** rows name modules that have no file; they
are editorial, and the script only checks that they do not name a module
that exists.

"Importable" means *usable*, not *correct*. The **what works** column
names the test that runs a module; a module with no test there has not
been shown to compute anything.

## Columns

- **blocked by**: the first error of the import ratchet, shortened, and,
  in italics, its `needs:` reason where the ratchet records one (what the
  compiler cannot express yet, as opposed to a fault of the module).
- **safety**: what the module does not promise. Integer arithmetic wraps
  everywhere in Codira (no overflow trap), so a function documented
  without an overflow rule wraps.
- **platforms**:
  - **L+CI**: written and run on x86-64 Linux, and in the CI test job
    (`cargo nextest --workspace` on Linux, Windows and macOS). In CI run 98
    (commit `a980406deb`) none of these tests was among the failures on any
    runner; the per-runner test lists are not kept in the repository.
  - **L**: written and run on x86-64 Linux; its tests are not yet part of a
    recorded CI run.
  - **L+CI (imported)**: the import ratchet checks, builds and loads it on
    every CI runner; no program runs it.
  - **L+CI (source)**: the module's source is compiled into a test program
    that runs it (not through `import std`).
  - **—**: nothing runs.

Tests named below live in `crates/codira/tests/std_imports.rs` unless a
path is given.

## Importable

| module | status | what works (test) | blocked by | safety | platforms |
|---|---|---|---|---|---|
| `std.ai.runtime` | importable | f32 tensors, matmul, add, relu, softmax, a Linear layer trained by SGD (`crates/codira/tests/db_and_ai.rs`: `the_std_wrappers_compile_and_work`, `a_codira_program_trains_a_linear_layer_and_the_loss_falls`) | — | Experimental. Tensors are not traced by the GC and must be freed. | L+CI |
| `std.base64.base64` | importable | base64, base32, base16 encode/decode over `[u8]` against the standard test vectors (`std_base64_matches_the_standard_test_vectors`) | — | Lenient, non-canonical decoder: accepts malformed input (`QQ=A` decodes to two bytes, see Defects); not for validation. Not constant time. | L+CI |
| `std.base64.base64_string` | importable | base64, base32, base16 between `[u8]` and `String` against the standard test vectors (`std_string_modules_work`) | — | Lenient decoder, as `std.base64.base64`. Not constant time. | L+CI |
| `std.bit.bit` | importable | counts, reversal, byte swap, rotations, widths, floor/ceil powers of two on `u64`/`u32` (`std_bit_operations_are_exact`) | — | `bit_ceil` above 2^63 returns 1. | L+CI |
| `std.bit.mask` | importable | masks, field extract/insert, bit select, single-bit ops (`std_bit_operations_are_exact`) | — | A bit position >= 64 is taken modulo 64 (shifts are masked). | L+CI |
| `std.builtin.type_aliases` | importable | `Never` = `never` | — | — | L+CI (imported) |
| `std.cmp.cmp` | importable | traits `Eq` and `Ord` for every integer and float type, `bool` and `str`, `Eq` for `String` (bytes compared inline, eight at a time); `min_of`, `max_of`, `str_equal`, `str_compare` (`std_collections_map_set_deque_heap_and_counter`; `std_collections_map_matches_rust_hash_map`) | — | IEEE floats: a NaN is not equal to itself, so a NaN key is never found again. | L+CI |
| `std.collections.binary_heap` | importable | `BinaryHeap[T: Ord]`, a max-heap: `push`, `pop`, `peek`, `to_sorted_array` (`std_collections_map_set_deque_heap_and_counter`) | — | A class: copies share the heap. | L+CI |
| `std.collections.counter` | importable | `Counter[T]` on `Map`: `add`, `subtract`, `count`, `total`, `most_common`; `unkeyed` (`std_collections_map_set_deque_heap_and_counter`; `custom_key_types_bring_their_own_hash`) | — | Keyed like `Map` (SipHash-1-3, a key per counter): DoS-resistant; `unkeyed()` is not. | L+CI |
| `std.collections.deque` | importable | `Deque[T]`, a ring buffer: push and pop at both ends, `get`, `set` (`std_collections_map_set_deque_heap_and_counter`) | — | A class: copies share the queue. `get`/`set` out of range panic. | L+CI |
| `std.collections.map` | importable | `Map[K, V]`, a Swiss table: 8-slot groups with a control word each, probed with SWAR, tombstones only where a probe needs them, 7/8 load; `insert`, `get`, `get_or`, `remove`, `contains_key`, `keys`, `values`, `entries`, `iter` (documented rules for changes while iterating), `with_capacity`, `reserve`, `capacity`, `shrink_to_fit`; `new` (SipHash-1-3 under a key per map from the runtime's entropy service), `with_key`, `unkeyed`; small maps of integer keys scanned unhashed (`std_collections_map_matches_rust_hash_map`: random operations with removals, re-insertions, resizes and tombstone churn against Rust's `HashMap` on every hasher and `String` keys; `tests/hash_map.rs`: flooding, iteration rules, keys per map and per run) | — | Keyed default **DoS-resistant**; `unkeyed()` is not. Lookups and removals 0.71x to 1.15x Rust's `HashMap` time with SipHash on both sides, 0.95x to 1.54x with the same unkeyed hash; insertion into new maps 1.0x to 3.5x (each growth is a new GC allocation); iteration 1.5x to 3.8x (agents/USING_CODIRA.md has the tables). No SIMD (the language has no vector types): 8-byte SWAR groups. | L+CI |
| `std.collections.optional` | importable | `some`, `none`, `unwrap` (panics on none), `unwrap_or`, `or_opt` (`std_collections_optional_unwraps_and_panics_on_none`) | — | Superseded by the language's `T?`. | L+CI |
| `std.collections.set` | importable | `Set[T]` on `Map`: `insert`, `contains`, `remove`, `union`, `intersection`, `difference`, `is_subset`; `with_capacity`, `with_key`, `unkeyed`, `reserve` (`std_collections_map_set_deque_heap_and_counter`; `std_collections_map_matches_rust_hash_map` against `HashSet`) | — | Keyed like `Map`: DoS-resistant; `unkeyed()` is not. | L+CI |
| `std.collections.string` | importable | on the built-in `String`: `join`, `insert`, `remove`, `substring`, `reverse`, `count`, `compare`, `split_whitespace`, code points (`std_string_modules_work`) | — | Byte indices; out-of-range or off-boundary indices panic. | L+CI |
| `std.complex.complex` | importable | arithmetic, Smith division, `powi`, `sqrt`, polar (`std_complex_arithmetic`) | — | `complex_norm` overflows above ~1e154 (no `hypot`); see Defects. | L+CI |
| `std.complex.complex_string` | importable | `complex_to_string` (`std_string_modules_work`) | — | — | L+CI |
| `std.crypto.hash` | importable | SHA-256, SHA-512, BLAKE3 over raw memory (RustCrypto `sha2`, `blake3` in the runtime) against NIST and BLAKE3 vectors (`a_program_computes_sha256_through_std_crypto_hash`; `crates/codira/tests/fs_and_hash.rs`: `a_codira_program_computes_the_known_answer_digests`) | — | Cryptographic. Not constant time in the length. `@noalloc @nopanic @noblock` are trusted assertions about the runtime. | L+CI |
| `std.db.db` | importable | snapshot-isolation transactions, commit, conflict, reopen (`db_and_ai.rs`: `the_std_wrappers_compile_and_work`, `a_codira_program_commits_reopens_and_reads_back`, `a_codira_program_observes_a_write_write_conflict`) | — | Experimental. Snapshot isolation, not serializable (write skew). One process per directory, no lock file. Whole data set in memory. | L+CI |
| `std.error.error` | importable | the `Error` trait (`message`, `source`, `into_any`), `AnyError` (a root cause, contexts, the original error's type name; `to_string` writes `outer: inner: root`), `context`/`with_context` on `Result[T, E: Error]`, and `?` converting any `E: Error` to `AnyError` through its `@error_conversion`; `FileError` and `ParseError` implement `Error` (`crates/codira/tests/error_context.rs`: `a_three_level_context_chain_through_question_mark_and_read_file`, `question_mark_converts_every_error_type_and_passes_any_error_on`, `main_returning_an_err_prints_the_chain_and_exits_with_status_1`) | — | An `AnyError` keeps the original error's message and type name, not the error itself (no trait objects): it cannot be downcast. Adding a context copies the context array. | L+CI |
| `std.format.format` | importable | `format(template, [args])` with `{}` and `{0}`, `format_int`/`format_uint` in a base, `hex`/`bin`/`oct`, `pad_left`/`pad_right`/`center` (`std_string_modules_work`) | — | A template is read at run time: an unmatched placeholder is kept as written, not an error (`f".."` is checked at compile time). | L+CI |
| `std.fs.file` | importable | `read_file(path) -> Result[[u8], FileError]` (`NotFound`, `PermissionDenied`, `IsADirectory`, `Other(code)`), errors passed on with `?`; `FileError` implements `std.error.error`'s `Error` (`std_result_fs_file_and_text_ascii_read_and_parse_a_file`; `error_context.rs`) | — | Reads the whole file into memory; no size limit. The file is closed on every path. No writing: use `std.fs.fs`. | L+CI |
| `std.fs.fs` | importable | open, read, write, seek, size, fsync, close, remove, rename, directory sync (`a_program_writes_and_reads_a_file_through_std_fs`; `fs_and_hash.rs`: `a_codira_program_writes_reads_renames_and_removes_a_file`) | — | Raw addresses: the runtime checks length and non-null only. Errors are negative codes, not `Result`. Durability claimed on Linux only; `fs_sync_dir` unsupported on Windows. Handles are never closed automatically. | L+CI |
| `std.hashlib.hash` | importable | SipHash-1-3 and SipHash-2-4 (`siphash13`, `siphash13_str`, `siphash13_u64`, `siphash24_*`), `HashKey` (`random()` from the runtime's entropy service), `SipHasher13`; `Hashable` with `hash_keyed` (SipHash) and `hash_value` (unkeyed: the MurmurHash3 finalizer for integers, FNV-1a for `str` and `String`); FNV-1a 64 over bytes, integers, floats, bools; combiners (`std_siphash_matches_the_reference` against a reference checked with the authors' vector and Rust's SipHash; `std_hashlib_hash_is_fnv1a`) | — | SipHash is DoS-resistant under a secret key, **not** a cryptographic hash for integrity; FNV-1a and `hash_value` are **not DoS-resistant**. `combine_hash` mixes weakly. Bytes read as little-endian words on little-endian targets only. | L+CI |
| `std.hashlib.string_hash` | importable | `hash_string`, `hash_string_bytes`: FNV-1a 64, compared with Rust's (`std_string_modules_work`) | — | **Not cryptographic, not DoS-resistant.** | L+CI |
| `std.json.json` | importable | `Json` (null, bool, `i64` and `f64` numbers, `String`, arrays, insertion-ordered objects), a strict RFC 8259 parser with positions in its errors, compact and pretty printers whose floats round-trip (into a `String` or a reused `[u8]`), a streaming writer (`JsonWriter`, to a buffer or a file) and a pull parser (`JsonReader`) with the parser's rules and limits, and typed decoding over the reader with the JSONPath and byte offset in its errors (`crates/codira/tests/json.rs`: `conformance_with_the_standard`, `random_values_print_like_serde_json_and_round_trip`, `mutated_texts_agree_with_serde_json`, `random_bytes_and_truncations_never_crash`, `nesting_is_limited`, every case also through the reader and the writer; `json_typed.rs`: `random_records_decode_and_encode_like_serde_json`, `decoding_errors_name_the_path_and_the_byte_offset`, `the_writer_refuses_misplaced_calls`, `the_writer_streams_to_a_file`; `std_json_parses_reads_builds_and_prints`) | — | Never panics on input; nesting limited (128 by default, 1000 at most); size not limited. Duplicate keys rejected by default. Object keys hashed with unseeded FNV-1a: **not DoS-resistant** for large objects. Strings without escapes are views that keep the input alive. No derive: typed decoders and encoders are written by hand. | L |
| `std.math.constants` | importable | IEEE values of π, e, limits, infinities, NaN (`std_math_constants_are_the_ieee_values`) | — | Constants are niladic functions (`PI()`) until module-level `let` exists. | L+CI |
| `std.math.fast` | importable | f32 approximations within their stated error (`std_math_fast_is_within_its_stated_error`) | — | Bounds hold for positive normal inputs only; `fast_pow`, `fast_tan` have none; `fast_ipow_*` is not exact. | L+CI |
| `std.math.math` | importable | libm-backed and intrinsic functions, rounding, `gcd`, `factorial`, classification, within 2 ulp of Rust's libm on the tested points (`std_math_math_matches_libm`) | — | `factorial`, `ipow`, `lcm` wrap on overflow. `tanh` is NaN above ~355; `sinh`/`tanh` lose all accuracy near 0 (untested; see Defects). | L+CI |
| `std.memory.page` | importable | (imported only; the runtime services are run by `crates/codira_runtime/tests/native_memory.rs` with their own declarations) | — | Raw OS memory; W^X enforced by the runtime; 0 on failure. | L+CI (imported) |
| `std.memory.region` | importable | arenas with quotas, alloc, used, free (`a_program_uses_std_memory_region_and_its_value_struct`) | — | Untraced memory: GC references stored there are invisible to the collector; stale addresses are not caught. Passing `Region` across modules allocates (value structs are boxed across modules). | L+CI |
| `std.random.rng` | importable | xoshiro256** seeded by SplitMix64, against a Rust reference implementation (`std_random_rng_is_xoshiro256_starstar`); `os_random_u64`, `os_random_bytes`, `rng_from_entropy` from the runtime's entropy service (`the_entropy_service_gives_fresh_bytes`) | — | The generator is **not cryptographic**; the `os_random_*` bytes are (the operating system's generator). | L+CI |
| `std.result.result` | importable | `Result[T, E]` with `is_ok`, `is_err`, `ok`, `err`, `unwrap_or`, `unwrap_or_else`, `map`, `map_err`, `and_then`, `or`, `expect`, `unwrap`, `from_optional`; `?` on it (`std_result_fs_file_and_text_ascii_read_and_parse_a_file`; `crates/codira/tests/generic_enums.rs`) | — | `unwrap`, `expect` and `expect_err` panic. Cranelift `--jit` refuses `?`. A postcondition about a returned `Result` comes back Unknown from the verifier. | L+CI |
| `std.sys.arg` | importable | `args`, `argv`, `program_name`, `has_flag`, `get_option`, `positional_arg`, `env_get`, `env_has` as `String`/`String?` (`std_string_modules_work`) | — | Under `codira run` the arguments are the `codira` command line's, not the program's. | L+CI |
| `std.sys.terminate` | importable | `panic` writes `panic: <message>` and aborts; `assert`, `write_stderr` (`std_sys_terminate_panics_with_its_message`) | — | `debug_assert` follows the runtime's build, not the program's. | L+CI |
| `std.text.ascii` | importable | `is_space`, `is_digit`, `next_word`, `parse_i64` with a `ParseError` naming the offending byte's index, which implements `std.error.error`'s `Error`; all four `@inline`, compiled into their callers (`std_result_fs_file_and_text_ascii_read_and_parse_a_file`; `error_context.rs`; `error_propagation.rs`) | — | ASCII only. `parse_i64` reports overflow as an error, not a wrap. | L+CI |
| `std.time.time` | importable | `Duration` arithmetic, monotonic `Instant`, sleep, wall clock (`std_time_measures_and_sleeps`) | — | Duration arithmetic wraps on overflow (except `duration_sub`, which saturates). Wall clock is an `f64` of seconds. | L+CI |

## Checks but not importable

| module | status | what works (test) | blocked by | safety | platforms |
|---|---|---|---|---|---|
| `std.ai.algorithm.functional` | checks-but-not-importable | — | its import `std.ai.tensor.buffer` fails (module-level `let` `SIZE_F32`) | — | — |
| `std.builtin.string_literal` | checks-but-not-importable | — | its import `std.ai.tensor.raw` fails (module-level `let`) | — | — |
| `std.math.sequence` | checks-but-not-importable | — | `std.collections.list` (`Pointer[T]`); *needs: `List` (std.collections.list)* | — | — |
| `std.prelude` | checks-but-not-importable | — | imports all of `std/` (1051 diagnostics); programs have no prelude by design | — | — |

## Fails

| module | status | what works (test) | blocked by | safety | platforms |
|---|---|---|---|---|---|
| `std.ai.algorithm.reduction` | fails | — | module-level `let` (`PAIRWISE_THRESHOLD`) | — | — |
| `std.ai.linalg.matmul` | fails | — | module-level `let` (`BLOCK`) | Raw addresses, caller-owned memory. | — |
| `std.ai.linalg.vector` | fails | — | module-level `let` (`DOT_THRESHOLD`) | Raw addresses. | — |
| `std.ai.nn.activation` | fails | — | access of a private field (`src.shape`) | — | — |
| `std.ai.nn.layer` | fails | — | access of a private field (`y.shape`) | — | — |
| `std.ai.quantize` | fails | — | module-level `let` (`SYMMETRIC_MAX`) | — | — |
| `std.ai.tensor.buffer` | fails | — | undefined `SIZE_F32` (a module-level `let` of `std.ai.tensor.raw`) | Views over caller memory; no ownership. | — |
| `std.ai.tensor.raw` | fails | — | module-level `let` (`SIZE_F32`) | Raw loads and stores. | — |
| `std.ai.tensor.shape` | fails | — | module-level `let` (`MAX_RANK`) | — | — |
| `std.atomic.atomic` | fails | — | module-level `let` (`RELAXED`); *needs: module-level `let` (its orderings), `String`, and in-place parameters (`mut`), none of which the compiler has: it rejects the first and the last* | No atomic intrinsics are declared to the compiler yet. | — |
| `std.builtin.bool` | fails | — | undefined type `String` | Duplicates the primitive `bool`. | — |
| `std.builtin.dtype` | fails | — | 231 diagnostics (`!` on a method call; a Mojo-style `DType`) | — | — |
| `std.builtin.error` | fails | — | undefined type `String` | Superseded by `std.error.error`. `eq` uses `&` for `&&` (see Defects). | — |
| `std.builtin.int` | fails | — | undefined `__panic` (a prelude name not imported) | Duplicates the primitive integers. | — |
| `std.builtin.range` | fails | — | mismatched types: `&` where `&&` is meant (see Defects) | — | — |
| `std.builtin.simd` | fails | — | undefined `__simd_storage[T, N]` (const generics, SIMD storage) | — | — |
| `std.builtin.slice` | fails | — | `Pointer[T]` and `&T` references | Unchecked views. | — |
| `std.builtin.sort` | fails | — | undefined `__intrinsic_ilog2` | Several algorithm defects (see Defects). | — |
| `std.builtin.swap` | fails | — | undefined type `Pointer[T]` | — | — |
| `std.builtin.traits` | fails | — | undefined type `Hasher`; *needs: generic traits (`Indexable[Index, Element]`, `Iterable[Element]`) and `String`* | Its `Hashable` and `Representable` conflict with `std.hashlib.hash` and `std.format.repr`. | — |
| `std.builtin.tuple` | fails | — | undefined type `String` | Duplicates language tuples. | — |
| `std.builtin.types` | fails | — | undefined type `f16`; *needs: the `f16` and `bf16` types, and sizeof/alignof intrinsics it does not declare* | — | — |
| `std.collections.array` | fails | — | undefined `FixedBuf[T, N]` (const generics) | — | — |
| `std.collections.bitset` | fails | — | module-level `let` (`BITS_PER_WORD`); *needs: `Pointer[u64]` (std.memory.pointer)* | — | — |
| `std.collections.dict` | fails | — | module-level `let` (`BUCKET_EMPTY`); *needs: `Pointer[T]` (std.memory.pointer)* | Duplicate keys after a removal; can loop forever when tombstones fill the table (see Defects). Unkeyed hash: not DoS-resistant. | — |
| `std.collections.linked_list` | fails | — | *needs: `Pointer[T]` (std.memory.pointer)* | — | — |
| `std.collections.list` | fails | — | *needs: `Pointer[T]` (std.memory.pointer)* | — | — |
| `std.collections.span` | fails | — | *needs: `Pointer[T]` (std.memory.pointer)* | Unchecked lifetime. | — |
| `std.ffi.cstring` | fails | — | undefined type `Array[u8]` | — | — |
| `std.ffi.unsafe_union` | fails | — | undefined type `Array[u8]` | Not a C union: a byte array, not layout-compatible. | — |
| `std.format.repr` | fails | — | *needs: `String`* | — | — |
| `std.gpu.globals` | fails | — | module-level `let` (`WARP_SIZE`) | No kernel launch exists. | — |
| `std.gpu.host_info` | fails | — | module-level `let` (`KB`) | — | — |
| `std.gpu.id` | fails | — | 547 diagnostics (`StringLiteral` parameters) | — | — |
| `std.gpu.intrinsics` | fails | — | 736 diagnostics (module-level `let` first) | — | — |
| `std.gpu.profiler` | fails | — | undefined type `String` | — | — |
| `std.gpu.warp` | fails | — | 547 diagnostics (module-level `let` first) | — | — |
| `std.hashlib.hasher` | fails | — | undefined type `Array[u8]`; *needs: in-place parameters: its writers take the hasher as `mut`, and the compiler has only by-copy parameters (it rejects `mut`); and `String`* | `AHasher` uses fixed seeds: not DoS-resistant. | — |
| `std.math.polynomial` | fails | — | *needs: `List` (std.collections.list)* | — | — |
| `std.memory.alloc` | fails | — | 93 diagnostics (`&` for `&&` among them) | — | — |
| `std.memory.arc_pointer` | fails | — | undefined `__intrinsic_sizeof` | Counts change only on explicit `clone`/`destroy`; copies are uncounted. | — |
| `std.memory.memory` | fails | — | undefined `__intrinsic_memcpy` | `constant_time_eq` is not checked to be constant time. | — |
| `std.memory.owned_pointer` | fails | — | undefined `__intrinsic_addr_to_ref` | Nothing is freed automatically; copies can double free. | — |
| `std.memory.pointer` | fails | — | undefined `__panic`; *needs: std.builtin.traits and std.builtin.types, `String`, and intrinsics the compiler does not have (`__intrinsic_to_hex`, `__intrinsic_type_name`)* | Not memory safe: no lifetime or liveness checking. | — |
| `std.memory.unsafe_pointer` | fails | — | undefined type `Pointer[T]` | Unsafe by design. | — |
| `std.os.env` | fails | — | undefined type `String`; its `_platform_*` services do not exist in the runtime | — | — |
| `std.os.os` | fails | — | undefined type `String`; `_platform_*` services do not exist | — | — |
| `std.os.path` | fails | — | module-level `let` (`PATH_SEP`); `_platform_*` services do not exist | `/` only: wrong on Windows. | — |
| `std.os.process` | fails | — | undefined type `Never` (not imported); `_platform_*` services do not exist | — | — |
| `std.pathlib.path` | fails | — | undefined type `String`; `_platform_*` services do not exist | — | — |
| `std.random.random` | fails | — | module-level `var`; *needs: module-level mutable state (its generator is a global, and the compiler rejects a module-level `var`); and the growable `Array`* | Not cryptographic. Its entropy source `_platform_random_seed` does not exist (the runtime now has one, `codira_random_fill`, which `std.random.rng` uses). `shuffle`/`random_choice` have modulo bias (see Defects). | — |
| `std.sys.assert_compare` | fails | — | `==` on a type parameter; *needs: operators on a type parameter (`==`, `<` through traits) and `String` (`Stringable`)* | — | — |
| `std.sys.info` | fails | — | module-level `let` (`os_is_linux`) | — | — |
| `std.sys.intrinsics` | fails | — | undefined trait `StringLiteral` | Unsafe by design. | — |
| `std.testing.suite` | fails | — | undefined type `String` | — | — |
| `std.testing.testing` | fails | — | undefined type `String` | — | — |

## Planned

Modules of the target layout ([ARCHITECTURE.md](ARCHITECTURE.md)) that
have no file yet. "Replaces" names what today's file becomes. The layer
is the API layer of the architecture, not a schedule; the milestone is in
[ROADMAP.md](ROADMAP.md).

| module | status | layer and purpose | blocked by (language, then runtime) | safety target | platforms |
|---|---|---|---|---|---|
| `std.panic` | planned | core: panic, assert, assert_eq, unreachable, todo (replaces `std.sys.terminate`, `std.sys.assert_compare`) | operators through traits (for `assert_eq`) | `@noalloc`; panics by definition | — |
| `std.option` | planned | core: helpers for `T?` (replaces `std.collections.optional`) | generic enums | `@noalloc @nopanic` except `unwrap`/`expect` | — |
| `std.result` | planned | core: `Result[T, E]` combinators (a first version exists as `std.result.result`) | moving it to `std.result` | `@noalloc @nopanic` except `unwrap`/`expect` | — |
| `std.str` | planned | core: operations on `str` (UTF-8 validation, find, split views, trim), byte-indexed | slices of `str` | `@noalloc`; byte indices; `T?` on bad boundaries | — |
| `std.slice` | planned | core: views, search, sort, binary search (replaces `std.builtin.slice`, `std.collections.span`, `std.builtin.sort`) | growable arrays (in progress), operators through traits | `@noalloc` except `stable_sort` | — |
| `std.num` | planned | core: integer and float limits, checked/wrapping/saturating/overflowing arithmetic, parsing (replaces the limits in `std.math.constants` and `std.builtin.types`) | module-level constants, `Result` | `@noalloc @nopanic` | — |
| `std.mem` | planned | core: copy, fill, compare, align, `size_of` (replaces `std.memory.memory`) | sizeof/alignof intrinsics | `@noalloc` | — |
| `std.ptr` | planned | core (unsafe): one raw typed pointer (replaces `std.memory.pointer`, `std.memory.unsafe_pointer`) | — | Explicitly unsafe | — |
| `std.cmp` | planned | fundamental: `Eq`, `Ord`, `Ordering`, min/max | operators through traits (`Eq`/`Ord` exist as `std.cmp.cmp`) | `@noalloc @nopanic` | — |
| `std.ops` | planned | fundamental: operator traits | operators through traits | — | — |
| `std.convert` | planned | fundamental: `From`/`Into`/`TryFrom` | generic traits | — | — |
| `std.iter` | planned | fundamental: `Iterator`, adapters (`map`, `filter`, `zip`, `enumerate`, `chain`, `take`, `fold`, ...) | generic traits with associated types, for an `Iterator[T]` that can be stored and passed; the built-in lazy pipelines (`xs.iter().map(f).sum()`, `for x in it` over a class with `next(self) -> T?`; see agents/USING_CODIRA.md) cover the adapters within one expression | Adapters `@noalloc`; `collect` allocates | — |
| `std.string` | planned | fundamental: owned UTF-8 `String` (replaces `std.collections.string`, `std.builtin.string_literal`) | moving the built-in `String`'s extras into `std.string` | Allocates; byte indices | — |
| `std.collections` | planned | fundamental: `Array` (growable), `Map`, `Set`, `Deque`, `BinaryHeap`, `BitSet`, ordered map (replaces today's `std.collections.*`) | `BitSet`, ordered map, a keyed hasher; `Map`, `Set`, `Deque`, `BinaryHeap` exist under `std.collections.*` on growable arrays | Keyed default hasher (DoS-resistant) | — |
| `std.hash` | planned | fundamental: `Hash`/`Hasher` traits, FNV-1a, a keyed default hasher (replaces `std.hashlib.*`) | generic traits, in-place parameters | Default hasher keyed per process | — |
| `std.fmt` | planned | fundamental: `Display`/`Debug` traits, number formatting and parsing (replaces `std.format.*`) | generic traits; format specs (`f"{x:>8.3}"`) and a debug form of every type (`f"{x:?}"`) are built into the compiler | — | — |
| `std.alloc` | planned | fundamental: `Region`, pages, aligned allocation, allocator interface (replaces `std.memory.region`, `std.memory.page`, `std.memory.alloc`) | — | Raw memory | — |
| `std.random` | planned | fundamental: one `Rng` API (replaces `std.random.rng`, `std.random.random`), seeded or from OS entropy | — (the runtime entropy service exists: `codira_random_fill`) | Default generator not cryptographic; `std.crypto.random` is | — |
| `std.io` | planned | application: `Reader`/`Writer`, buffered I/O, stdin/stdout/stderr | `Result`, growable arrays, trait objects or generic traits; runtime stdin/stdout services | — | — |
| `std.fs` | planned | application: `File`, directories, metadata, `read_to_end`/`write_all` with `Result` (replaces `std.fs.fs`, `std.os.os`, file parts of `std.pathlib.path`) | `Result`, `String`; runtime directory services | Typed handles; `Result` errors | — |
| `std.path` | planned | application: one `Path` type and pure path functions, per-platform separators (replaces `std.os.path`, `std.pathlib.path`) | `String` | — | — |
| `std.env` | planned | application: arguments, environment variables, current directory (replaces `std.os.env`, `std.sys.arg`) | a current-directory service; `std.sys.arg` has arguments and environment | — | — |
| `std.process` | planned | application: exit, abort, pid, spawning commands (replaces `std.os.process`, exit/abort of `std.sys.terminate`) | `String`, `Result`; runtime process services | — | — |
| `std.thread` | planned | application: OS threads beyond `spawn` (names, sleep, available parallelism) | runtime thread services | — | — |
| `std.sync` | planned | application: `Mutex`, `RwLock`, `Once`, `Condvar`, `Barrier`; also covers the proposed `concurrent` | in-place parameters or classes; runtime lock services that park for the GC | — | — |
| `std.atomic` | planned | application: atomics with orderings (replaces `std.atomic.atomic`) | module-level constants; atomic intrinsics in the compiler | — | — |
| `std.text` | planned | application: Unicode (char properties, case, normalization, grapheme segmentation) | `String`, tables (module-level constants) | — | — |
| `std.encoding` | planned | application: base64/base32/hex (strict and lenient), UTF-16, percent-encoding (replaces `std.base64.*`) | `Result`, growable arrays | Strict decoding by default | — |
| `std.toml` | planned | application: parse and print TOML | as `std.json.json` | — | — |
| `std.regex` | planned | application: linear-time regular expressions (no backtracking) | `String`, collections | Linear time: no catastrophic backtracking | — |
| `std.crypto` | planned | application: hashes (today's `std.crypto.hash`), HMAC, HKDF, AEAD, CSPRNG, constant-time compare, over audited runtime crates | runtime services; secret types | Cryptographic; secrets `secret`-typed | — |
| `std.compress` | planned | application: deflate/gzip/zlib, zstd, over runtime crates | runtime services | Output size limits (zip bombs) | — |
| `std.net` | planned | application: TCP, UDP, DNS resolution | runtime socket services that park for the GC | — | — |
| `std.http` | planned | application: HTTP/1.1 client and server, TLS through the runtime | `std.net`, `std.io` | TLS verified by default | — |
| `std.time.date` | planned | application: civil dates and times, UTC offsets, formatting; time zones from the system database | `String`, `Result` | — | — |
| `std.cli` | planned | application: command-line argument parsing | `std.env`, derive (optional) | — | — |
| `std.task` | planned | advanced: helpers on `scope`/`spawn` (join handles, results, cancellation) | richer task payloads (strings, arrays, classes across `spawn`) | — | — |
| `std.channel` | planned | advanced: helpers on the language's channels (fan-in/out, timeouts) | richer channel element types | — | — |
| `std.stream` | planned | advanced: iterators over channels and I/O | `std.iter`, `std.task` | — | — |
| `std.serde` | planned | advanced: serialization framework used by `std.json`/`std.toml` | derive, generic traits | — | — |
| `std.binary` | planned | advanced: byte-order readers/writers, varints, fixed layouts | growable arrays, `Result` | Bounds-checked | — |
| `std.ffi` | planned | advanced: C strings, C types, callbacks (replaces `std.ffi.cstring`; drops `std.ffi.unsafe_union`) | `String` | Unsafe by design | — |
| `std.log` | planned | advanced: logging facade with levels and structured fields; spans for tracing | `String`, trait objects | — | — |
| `std.metrics` | planned | advanced: counters, gauges, histograms | `std.atomic` | — | — |
| `std.testing` | planned | advanced: assertions for `codira test`, property tests, golden files (replaces `std.testing.*`) | `String`, operators through traits | — | — |
| `std.bench` | planned | advanced: benchmarking with warm-up and statistics | `std.time`, closures | — | — |
| `std.tensor` | planned | domains: one tensor API over runtime-owned tensors and caller-memory views (replaces `std.ai.*`) | module-level constants; runtime services | — | — |
| `std.simd` | planned | domains: portable fixed-width vectors (replaces `std.builtin.simd`) | const generics, vector types in the compiler | — | — |
| `std.units` | planned | domains: named units and conversions on top of the language's `unit(..)` types | unit conversions in the language | — | — |
| `std.probability` | planned | domains: distributions, sampling, statistics | `std.random`, `std.math` | — | — |
| `std.async` | unsupported | not planned: `scope`/`spawn`, channels and algebraic effects cover structured concurrency without coloring functions `async`; see ARCHITECTURE.md | — | — | — |
| `std.actor` | unsupported | not planned as a module: an actor is a task owning a channel, a pattern for `std.task` docs | — | — | — |
| `std.concurrent` | unsupported | not planned: merged into `std.sync` | — | — | — |
| `std.reflection` | unsupported | not now: the language has no reflection; derive covers the common uses | — | — | — |
| `std.quantum` | unsupported | not planned: `codira quantum` compiles quantum kernels; a library would duplicate it | — | — | — |
| `std.image` | unsupported | not in `std`: a package (codecs are large and security-sensitive) | — | — | — |
| `std.science` | unsupported | not in `std`: too broad to specify; `std.math`, `std.tensor` and `std.probability` cover the base | — | — | — |

## Defects found by reading the code

These are code defects found while auditing `std/`, listed for whoever
implements the module; the code is not changed by the audit. False
documentation claims found at the same time were corrected in place
(`std.sys.terminate`, `std.math.math`, `std.math.fast`, `std.math.constants`,
`std.bit.bit`, `std.time.time`, `std.collections.optional`,
`std.hashlib.*`, `std.memory.*`, `std.builtin.sort`, `std.base64.base64`).

Line numbers are as of this page's commit.

### In importable modules

| file:line | defect |
|---|---|
| `std/math/math.code:229` | `tanh(x)` is NaN for x above about 354.9: `exp(2x)` overflows and `inf / inf` is NaN. The answer is 1. |
| `std/math/math.code:219,229` | `sinh` and `tanh` cancel catastrophically near 0: `sinh(1e-17)` is 0, not 1e-17 (relative error 100%). Use `expm1`. |
| `std/math/math.code:328` | `abs(-0.0)` returns -0.0 and a negative NaN stays negative; `__intrinsic_fabs_f64` is declared and unused. |
| `std/math/math.code:589` | `gcd` with an argument equal to the minimum i64 returns a negative number (`abs_i64` wraps). |
| `std/math/math.code:611,626` | `factorial` and `ipow` wrap silently on overflow (now documented); there is no checked form. |
| `std/math/math.code:660` | `next_power_of_two_u64(x)` for x above 2^63 returns 0 (undocumented); it also duplicates `std.bit.bit.bit_ceil`, which returns 1 there. |
| `std/math/math.code:716` | `signum(NaN)` is 0.0; IEEE practice (and Rust) is NaN. |
| `std/math/fast.code:40` | `fast_sqrt` of a negative number is 0, not NaN; of infinity, negative infinity (now documented). |
| `std/math/fast.code:64` | `floor_f32` truncates through `i32`, so it is wrong for \|x\| >= 2^31 (saturating cast); callers keep within range today. |
| `std/complex/complex.code:142` | `complex_norm` computes `sqrt(a*a + b*b)`: overflows to infinity for \|z\| above ~1e154 and underflows below ~1e-154; `hypot` exists. |
| `std/complex/complex.code:222` | `complex_powi(z, n)` for n equal to the minimum i64 returns 1 (`-exp` wraps). |
| `std/complex/complex.code:103` | `complex_div` by zero returns NaN components, while `complex_inv` of zero panics: two rules for the same case. |
| `std/complex/complex.code:246` | `complex_sqrt` loses accuracy when the real part is negative and large relative to the imaginary part (`r + re` cancels). |
| `std/base64/base64.code:122` | A group whose third symbol is `=` and fourth is not (`QQ=A`) decodes to two bytes, the second taken from the wrong position, instead of being rejected. Unused bits are not checked, so decoding is not canonical (now documented). |

### In modules that do not import yet

| file:line | defect |
|---|---|
| `std/builtin/sort.code:288` | Insertion sort's loop condition `j >= 0 & cmp(data.get(j as usize), key) > 0` uses non-short-circuit `&`: at `j == -1` it reads index `usize::MAX`. |
| `std/builtin/sort.code:210,241,329` | The same `&`/`|` for `&&`/`||` in `_merge`, `partial_sort` and `_sift_down` (`_sift_down` reads `child + 1` past the end). |
| `std/builtin/sort.code:374` | `nth_element` calls `_partition` on ranges of one or two elements, where `last - 1` and `j - 1` underflow (now documented). |
| `std/builtin/error.code:140`, `std/builtin/tuple.code:86,139,189,246,312` (every `eq` there), `std/builtin/range.code:87,126,170,248,251`, `std/memory/alloc.code:128` | `&` where `&&` is meant, in equality and range tests. A systematic substitution; every `&` between comparisons in `std/` should be reviewed. |
| `std/collections/dict.code:145` | `set` inserts at the first tombstone without looking further along the probe sequence, so a key removed and re-set while another copy sits later in the chain is stored twice. |
| `std/collections/dict.code:164` | Tombstones are not counted toward the load factor, so after enough insert/remove cycles no empty bucket remains and `get` of a missing key loops forever. Same in `get_or_default`, `get_optional`, `remove`, `contains_key`. |
| `std/random/random.code:130` | `randint(I64_MIN, I64_MAX)`: the range count wraps to 0 and the modulo by 0 panics. `std.random.rng.rng_range_i64` handles this. |
| `std/random/random.code:199,216,233` | `shuffle`, `random_choice` and `random_sample` reduce with `%`, which is biased (the module's own `randint` uses rejection sampling). |
| `std/random/random.code:251` | `_platform_random_seed` is provided by nothing; the runtime's entropy service is `codira_random_fill` (used by `std.random.rng.os_random_u64`), which this module does not call. |
| `std/hashlib/hasher.code:74` | `ahasher_mix` is one multiply and one xor-shift per word: weak mixing; `buffer` is unused; `ahasher_reset` leaves `pad`. |
| `std/hashlib/hasher.code:225` | `hash_with(hasher: mut Hasher, value: Hashable)` uses traits as types (trait objects), which the language does not have. |
| `std/os/*.code`, `std/pathlib/path.code`, `std/sys/arg.code`, `std/format/*.code` | Declare `extern func _platform_*` (and `char_from_byte`) returning `String` or `Array`: no runtime provides these symbols, and a C-ABI extern cannot return a GC object. |
| `std/os/path.code:9` | `PATH_SEP` is `/` on every platform. |
| `std/memory/owned_pointer.code`, `std/memory/arc_pointer.code` | Designs that need destructors and move-only types, which the language does not have (now documented). |
| `std/ffi/unsafe_union.code:14` | `UnsafeUnion` stores a GC byte array, so its layout is not a C union's and it cannot be passed to C. |

## Runtime services today

Everything `std` can reach outside pure computation is a runtime service
declared `extern "C"` (`crates/codira_runtime/src/native_*.rs`):

| area | services | wrapped by |
|---|---|---|
| files | `codira_fs_open`, `_read`, `_write`, `_seek`, `_size`, `_fsync`, `_close`, `_remove`, `_rename`, `_sync_dir`, `_last_os_error` | `std.fs.fs` |
| hashes | `codira_hash_sha256`, `_sha512`, `_blake3` | `std.crypto.hash` |
| clocks | `codira_time_now`, `_monotonic_ns`, `_sleep_ns` | `std.time.time` |
| termination | `codira_write_stderr`, `codira_abort`, `codira_exit`, `codira_is_debug` | `std.sys.terminate` |
| memory | `codira_region_*`, `codira_page_*`, `codira_icache_flush`, `codira_aligned_alloc`/`_free` | `std.memory.region`, `std.memory.page` |
| database | `codira_db_*` (open, begin, get, value_len, put, delete, commit, abort, checkpoint, close, last_os_error) | `std.db.db` |
| tensors | `codira_ai_*` (tensor new/read/free/rank/dim/numel, matmul, add, relu, softmax, linear layer) | `std.ai.runtime` |

Not provided, and needed by planned modules: standard input and output
other than `print`/`println`, directory listing and metadata, environment
variables, command-line arguments, current directory, process spawning,
OS entropy, sockets, locks that cooperate with the collector, and threads
beyond `spawn`.
