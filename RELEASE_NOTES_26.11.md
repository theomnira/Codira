# Codira 26.11 release notes

Build of November 5, 2026: 154 commits since 26.10 (`4d27456303`), including merge commits.

26.11 works in two directions. It makes the language Codira describes and the
language a program can use closer to the same thing: traits, standard-library
imports, defined integer semantics. It also turns guarantees the compiler
used to state into ones it checks: envelopes, a machine-code constant-time
check, and garbage collection that is safe while other threads run.

Everything under the headings before "Experimental" is on `main` and covered
by tests. Experimental and opt-in work, known limitations and the platform
picture are listed plainly at the end. Read them before deploying: **Codira is
not yet suitable for production or mission-critical use.** The
[Status for production use](#status-for-production-use) section says why,
item by item.

The workspace version in `Cargo.toml` is still `0.6.0-dev`; "26.11" is the
release label used by the README.

```mermaid
flowchart TD
    %% ---------------------------------------------------------------- legend
    subgraph LEGEND [" Legend "]
        direction LR
        L1["on main at 26.11<br/>(plain outline)"]
        L2["added since 26.11<br/>(amber outline)"]
        L3["on main but unfinished,<br/>experimental or opt-in (dashed)"]
        L4["in-progress / WIP<br/>(dashed, wired into pipeline)"]
    end

    %% ---------------------------------------------------------------- 5
    subgraph TOOLS [" 5. Tooling and interop "]
        direction LR
        CLI["codira CLI<br/>new, build, check, run, start,<br/>test, query, synth, quantum"]
        DevTools["codira fmt, doc, lint,<br/>bench (Mann-Whitney regression check),<br/>audit (licences, hashes, externs)"]
        RunFlags["run / start / test flags<br/>--heap-limit, --gc-growth,<br/>--runtime-stats, --explain-alloc"]
        Daemon["codira_compiler_daemon<br/>warm salsa + LLVM, token-authenticated"]
        LSP["codira_language_server"]
        ImportC["codira import-c<br/>C headers via Clang"]
        EnvCLI["codira envelope<br/>inferred envelope of every function,<br/>--json, --check"]
        Certify["codira certify<br/>re-checks proofs with Z3 alone,<br/>re-derives recorded envelopes"]
        ApiDiff["codira api-diff<br/>breaking-change check, Z3 implication,<br/>a widened envelope is major"]
        Attest["codira attest<br/>zero-knowledge proof of an oblivious function"]
        Ueval["codira_ueval<br/>evaluation harness scaffolding (paper)"]
    end
    TOOLS ==>|drive| FRONTEND

    %% ---------------------------------------------------------------- 1
    subgraph FRONTEND [" 1. Front end: codira_syntax, codira_hir (salsa) "]
        direction TB
        Src["Codira source<br/>*.code, codira.toml"] --> Parse["codira_syntax<br/>lexer, parser, lossless CST<br/>contextual keywords, f-strings"]
        StdLib["std package: 36 of 96 modules importable<br/>std.result, std.error (AnyError, context),<br/>std.json (strict parser, printer, stream, typed),<br/>std.collections.map/set/deque/binary_heap/counter,<br/>std.cmp, std.format, std.fs.file, std.text.ascii,<br/>std.crypto.hash, std.random.rng, std.time, ...;<br/>ratchet tests record which modules check clean"]
        StdLib --> Parse
        Parse --> ItemTree["Item tree and name resolution<br/>modules, visibility, imports,<br/>names across packages"]
        ItemTree --> Infer["Type inference<br/>generics, optionals, generic enums,<br/>closures, effect rows"]
        Infer --> Mono["codira_hir::mono<br/>one instance per type argument"]
        Traits["Traits and bounds<br/>trait, extend T: Trait, T: A + B,<br/>defaults, supertraits;<br/>static dispatch per instance"]
        Traits --> Mono
        Infer --- Traits
        Comptime["codira_comptime<br/>compile-time interpreter<br/>fuel + depth limits"] <-->|folds comptime blocks| Infer

        Enums["enum, match, optionals<br/>exhaustiveness checker"]
        Closures["function values and closures<br/>capture by value, hot-reload safe"]
        Loops["for loops, ranges, continue<br/>bounds-checked indexing"]
        Results["Result and ?<br/>wraps into the one variant that holds<br/>the error; @error_conversion into AnyError;<br/>main may return a Result"]
        Strings["String and [T]<br/>built-in UTF-8 String, growable arrays<br/>with builtin methods; f-string format specs<br/>checked at compile time; {x:?} for every type"]
        Pipelines["Lazy pipelines<br/>xs.iter().map(..).filter(..).sum()<br/>one fused loop, no intermediate arrays;<br/>lines() / split() read lazily"]
        Infer --- Enums
        Infer --- Closures
        Infer --- Loops
        Infer --- Results
        Infer --- Strings
        Infer --- Pipelines

        subgraph CHECKS [" Static checks, every one a diagnostic, never a crash "]
            direction LR
            Effects["Effect obligations<br/>uses / handle"]
            Tasks["Structured concurrency<br/>scope, group (results in spawn order),<br/>spawn, cancelled(), channels,<br/>select, session types"]
            Flow["Information flow<br/>secret, oblivious, ct_select"]
            Units["Units of measure<br/>f64 unit(m / s)"]
            Realtime["@noalloc @nopanic<br/>@noblock @realtime<br/>call-graph fixpoint over 8 effects;<br/>runtime calls read from the catalogue"]
            Envelope["Semantic envelopes<br/>@envelope(allocation: none, ...)<br/>package envelope in codira.toml;<br/>unchecked keys rejected"]
            Codegen_ok["Codegen support<br/>honest refusals"]
            Bindings["Binding rules<br/>assigning to a let, in-place parameters<br/>and module-level let/var are errors"]
            Escape["Escape analysis (ALI)<br/>per-allocation placement, escape summaries;<br/>@region: an escape is a compile error;<br/>string_escape sizes kept format results"]
            Budget["@budget, table declarations,<br/>tensor shape checking, memory<br/>domains, capability types<br/>(in progress)"]
            Realtime --> Envelope
            Effects -.-> Budget
        end
        Infer --> CHECKS

        subgraph VERIFY [" Contract verification: codira_hir::verify "]
            direction LR
            Contracts["requires / ensures<br/>invariant / decreases / ghost<br/>refinement types, bit-exact,<br/>for and continue modelled"]
            Synth["?? holes and choose x where p"]
            TestGen["Counterexample to test<br/>codira test, codira query"]
        end
        Infer --> VERIFY
    end

    SMT["codira_smt<br/>Z3, deterministic rlimit"]
    VERIFY <==>|bit-vector proof obligations| SMT
    CERT["codira_certificate<br/>codira-certificate/2: proofs as SMT-LIB2,<br/>code hashes, inferred envelopes"]
    VERIFY --> CERT
    Envelope --> CERT
    CERT <--> SMT

    %% ---------------------------------------------------------------- 2
    subgraph EIDOS [" 2. Eidos mid-end: codira_mir, codira_egraph, codira_tv "]
        direction TB
        MirLower["mir_lower<br/>HIR to region SSA"] --> MIR["Eidos MIR<br/>typed region SSA, memory states"]
        MIR --> Passes["MIR passes<br/>const_fold, cse, dce, inline,<br/>unroll, simplify, rewrite"]
        Passes --> EGraph["codira_egraph<br/>equality saturation<br/>hard node cap, exact extractor"]
        Rewrite["@rewrite rules<br/>library equations"] -->|admitted only when proven| EGraph
        EGraph --> Fold["eidos_fold<br/>whole-function constant result,<br/>only for effect-free functions"]
        TV["codira_tv<br/>translation validation<br/>counterexamples re-executed"]
        Passes <==>|each transform checked| TV
        Rewrite <==>|proof before use| TV

        LoopSchedules["loop schedules (LLVM hints)<br/>(fails a dependence-safety test)"]
        MultiShot["multi-shot continuations,<br/>sample / observe<br/>(fails posterior checks)"]
        Passes -.-> LoopSchedules
        Passes -.-> MultiShot
    end
    Mono ==>|"--mode=release"| MirLower
    TV <==> SMT
    Realtime -.->|effect summary gates the fold| Fold

    %% ---------------------------------------------------------------- 3
    subgraph BACKENDS [" 3. Back ends: codira_codegen and friends "]
        direction TB
        LLVMGen["codira_codegen<br/>HIR instance to LLVM IR via inkwell<br/>bounds checks, effects, tasks, fuel,<br/>checked integer division, masked shifts,<br/>saturating float casts, value-struct host wrappers,<br/>ct_select stays a cmov at every -O level;<br/>methods link as module::Type::method, @inline"]
        Fabric["String Fabric<br/>inline append into a buffer the thread owns;<br/>compile-time format plans, digits written in place;<br/>kept results sized to what they hold"]
        Roots["Precise stack roots<br/>frame records only where a reference is live<br/>across a call; one-load back-edge polls;<br/>a frame without a record is scanned conservatively"]
        FrameAlloc["--frame-alloc (experimental)<br/>non-escaping objects in the stack frame;<br/>@region functions allocate in an arena"]
        Catalogue["codira_abi runtime catalogue<br/>every runtime function: signature, alloc,<br/>panic, block, safepoint, shared state;<br/>read by LLVM, Cranelift and the effects"]
        ArrayAlias["array alias classes<br/>TBAA tags for array lengths, elements, handles;<br/>opt-in, CODIRA_ARRAY_TBAA=1"]
        ClassFieldAlias["class-field alias information<br/>field updated in loop stays in memory<br/>(store_field 2.5x Rust)"]
        Targets["codira_target<br/>14 targets held to LLVM data layouts<br/>--target-cpu"]
        Native["[native] C, C++, asm in a package<br/>bitcode linked before optimization;<br/>C++ static constructors and destructors run<br/>(.init_array / .fini_array on ELF)"]
        Image["codira_image<br/>direct PE writer (default on Windows),<br/>ELF opt-in, declines what it cannot model"]
        LLD["codira_lld<br/>in-process LLD (fallback);<br/>macOS libraries ad-hoc signed by LLD"]
        Lib[".codiralib<br/>hot-reloadable shared library;<br/>imported std modules get their own"]
        Exe["codira build --exe<br/>standalone executable"]
        Wasm["wasm32-unknown-unknown<br/>module that imports nothing"]
        PTX["@target(gpu)<br/>NVPTX, launched by codira_gpu"]
        CL["codira_cranelift<br/>codira run --jit, no LLVM, no linker<br/>scalar core and arrays (literals, indexing,<br/>push, for); String, regions, groups and<br/>pipelines refused by name, run on LLVM"]
        Quantum["codira_quantum<br/>@target(qpu): circuit IR, simulator,<br/>QIR, OpenQASM 2/3"]
        Circuits["codira_circuits<br/>oblivious function to R1CS, Circom,<br/>Groth16 (attest); Verilog (@target(fpga))"]
        Asm["@asm inline assembly<br/>machine memory intrinsics"]
        MTE["--sanitize=memtag (AArch64)<br/>experimental, never run on MTE hardware;<br/>CHERI / Morello targets refused"]
        Funis["funis_* crates (mold fork, MIT)<br/>ELF + Mach-O linker; library mode unfinished,<br/>not used by the compiler yet"]
        Bindgen["codira bindgen<br/>C, Rust, Go, TypeScript"]
        Bun["Bun plugin<br/>import a .code file"]

        Fabric --> LLVMGen
        Roots --> LLVMGen
        FrameAlloc -.-> LLVMGen
        Catalogue --- LLVMGen
        Catalogue --- CL
        LLVMGen --> Targets
        ArrayAlias -.-> LLVMGen
        ClassFieldAlias -.-> LLVMGen
        Native --> LLVMGen
        Asm --> LLVMGen
        Targets --> Image
        Image --> Lib
        Targets -.->|"declined or other formats"| LLD
        LLD --> Lib
        LLD --> Exe
        Targets --> Wasm
        Targets -.-> MTE
        LLVMGen --> PTX
        Lib --> Bindgen
        Lib --> Bun
    end
    Mono ==> LLVMGen
    Escape ==>|placements| FrameAlloc
    Escape -.->|kept or temporary| Fabric
    Fold -.->|constant shortcut| LLVMGen
    Mono ==>|dev JIT| CL
    Infer ==>|"qpu kernels"| Quantum
    Flow ==>|"oblivious subset"| Circuits
    Realtime --- Catalogue

    %% ---------------------------------------------------------------- 4
    subgraph RUNTIME [" 4. Runtime: codira_runtime, codira_memory, codira_healing "]
        direction TB
        Loader["codira_libloader + codira_abi<br/>type info, GUIDs, dispatch table"]
        Reload["Hot reload<br/>relink dispatch table, migrate heap<br/>@renamed, saturating retypes;<br/>world stopped, only with no thread in Codira code"]
        LiveUpdates["proof-gated live updates<br/>relational prover + runtime protocol<br/>(does not compile yet)"]
        Snap["Heap snapshots<br/>save and restore the whole GC heap"]
        Cells["Function-value code cells<br/>repointed on every reload"]
        GC["codira_memory GC<br/>mark-sweep, size-class blocks, stop-the-world;<br/>precise frame roots, conservative elsewhere;<br/>SIGURG preemption of a thread that never polls<br/>(Linux x86-64 and AArch64)"]
        Policy["Heap policy<br/>finite default limit (50% of available memory),<br/>GOGC-style pacing from Codira allocations,<br/>collect_blocking / request_collection with a<br/>fairness window; controlled OOM exit (102);<br/>Runtime::stats(): heap, pauses p50/p99/max"]
        Relocate["Relocation (opt-in, CODIRA_GC_COMPACT=1)<br/>evacuates sparse blocks; anything a stack,<br/>root, pin or root region refers to stays;<br/>off while Cascade is attached"]
        CheapReloc["relocation that pays for itself<br/>cheap sparse-block selection with Cascade"]
        Cascade["Cascade (opt-in, runtime only)<br/>deferred reference counting, epochs,<br/>bounded cycle collection; the compiler<br/>emits no retains or releases yet"]
        CascadeCompiler["Cascade in the compiler<br/>retain/release insertion from uniqueness analysis<br/>(in progress)"]

        Arenas["@region arenas<br/>per-thread chunk cache, released at once;<br/>registered as root spans; close-time check<br/>in debug builds"]
        RootRegion["root_region<br/>conservatively scanned words: task-group<br/>results, region chunks"]
        StringRT["String runtime<br/>owner word per buffer (thread pointer);<br/>views, Rust str algorithms, ryu floats"]
        Sched["Fabric scheduler<br/>per-worker deques, work stealing,<br/>blocking-I/O pool, task tree with<br/>cooperative cancellation, group results"]
        Regions["Region arenas with quotas<br/>W^X OS pages (std.memory)"]
        EffRT["Effect handlers<br/>resume, abort across frames"]
        Fuel["Fuel metering<br/>--fuel"]
        Heracles["codira_healing (HERACLES)<br/>fault interception (Windows only),<br/>@heal strategies"]

        subgraph SERVICES [" Native services, extern C, generation-checked handles, negative error codes "]
            direction LR
            FsSvc["codira_fs_*<br/>open, read, write, seek,<br/>fsync, rename, sync_dir<br/>(std.fs.fs)"]
            HashSvc["codira_hash_*<br/>SHA-256, SHA-512, BLAKE3<br/>via sha2 / blake3 crates<br/>(std.crypto.hash)"]
            RandSvc["codira_random_*<br/>OS entropy (getrandom),<br/>per-process hash-table key<br/>(std.random.rng, Map's SipHash-1-3)"]
            DbSvc["codira_db_*<br/>open, begin, get, put, delete,<br/>commit (conflict code), checkpoint<br/>(std/db/db.code)"]
            AiSvc["codira_ai_*<br/>f32 tensors, matmul, add, relu,<br/>softmax, Linear + MSE + SGD step<br/>(std/ai/runtime.code)"]
        end

        Loader --> Reload
        Reload --> GC
        Reload --> Snap
        Reload --> Cells
        LiveUpdates -.-> Reload
        Policy --> GC
        Relocate -.-> GC
        CheapReloc -.-> Relocate
        Cascade -.-> GC
        CascadeCompiler -.-> Cascade
        RootRegion --> GC
        Arenas --> RootRegion
        Sched --> RootRegion
        Fuel --> Heracles
    end
    Lib ==> Loader
    Exe ==> Loader
    Fabric ==> StringRT
    FrameAlloc ==> Arenas
    Tasks ==> Sched
    Roots ==>|frame records| GC

    %% ---------------------------------------------------------------- engines
    subgraph ENGINES [" 6. Native engines (pure Rust, no Python, no BLAS) "]
        direction LR
        DbEngine["codira_db<br/>checksummed WAL, MVCC snapshot isolation<br/>(not serializable), atomic checkpoints,<br/>typed tables, add-column migrations;<br/>crash injected at every storage operation"]
        AiEngine["codira_ai<br/>strided tensors, broadcasting, blocked matmul,<br/>tape reverse-mode autodiff (gradient-checked),<br/>attention, transformer block, Adam/AdamW,<br/>INT8 weights, byte-level BPE"]
    end
    DbSvc ==> DbEngine
    AiSvc ==> AiEngine

    %% ---------------------------------------------------------------- quality
    subgraph QUALITY [" 7. Quality "]
        direction LR
        Diff["Differential testing<br/>seeded random programs on LLVM -O2,<br/>LLVM -O0 and Cranelift, arrays included;<br/>frame-alloc and @region differentials"]
        Bench["AURORA benchmark suite (benches/aurora)<br/>Codira vs Rust and C/C++, bootstrap intervals,<br/>heap floor tracked per commit; regression gate<br/>refuses a baseline from another machine"]
        Gates["AURORA gates (TESTING.md)<br/>GC stress with shadow models, fault injection<br/>at every allocation, loom models of the safepoint<br/>protocol, ASan / TSan, fuzzers, gc-modes, soak"]
        CI["CI on Linux, Windows and macOS<br/>plus opt-in aurora-gates and aurora-bench<br/>workflows; last per-OS results at 26.11:<br/>macOS Funis Mach-O failures, nightly fmt"]
        Supply["Supply chain<br/>cargo audit: 1 open advisory<br/>(tracing-subscriber 0.2 via arkworks)"]
    end
    Diff -.->|checks| LLVMGen
    Diff -.->|checks| CL
    Gates -.->|stresses| GC
    Bench -.->|gates| LLVMGen

    %% ---------------------------------------------------------------- 8
    subgraph MCODE [" 8. Machine code: codira_ctcheck, codira_mcode, codira_superopt "]
        direction TB
        CTCheck["codira_ctcheck<br/>codira build --check-constant-time:<br/>no secret branch, no variable-time instruction,<br/>no unchecked call, no secret address (x86-64);<br/>x86-64 and AArch64 machine code"]
        MCodeIR["codira_mcode<br/>x86-64 and AArch64 integer instructions,<br/>one semantics run concretely and as Z3 bit-vectors,<br/>checked against the CPU it runs on"]
        Profiles["cost profiles<br/>x86-64 latencies and throughputs measured<br/>on the host; AArch64 from the Neoverse N1<br/>optimization guide, never measured"]
        Superopt["codira_superopt<br/>codira build --superoptimize (experimental):<br/>the integer tail of each function, up to 3 instructions,<br/>proven equal by Z3, patched in place and re-proven;<br/>refuses constant-time code; cached"]
        SuperoptLoop["loop-body superoptimizer<br/>and other targets (in progress)"]
        MCodeIR --> Superopt
        Profiles --> Superopt
        Superopt -.-> SuperoptLoop
    end
    Lib -.->|"oblivious functions"| CTCheck
    Lib -.->|"--superoptimize"| Superopt
    Superopt <==>|"replacement proven"| SMT
    Flow -.->|"constant-time code refused"| Superopt

    classDef frontend fill:#1e293b,stroke:#38bdf8,color:#f8fafc,stroke-width:2px;
    classDef mid fill:#172554,stroke:#60a5fa,color:#f8fafc,stroke-width:2px;
    classDef backend fill:#064e3b,stroke:#34d399,color:#f8fafc,stroke-width:2px;
    classDef runtime fill:#3b0764,stroke:#c084fc,color:#f8fafc,stroke-width:2px;
    classDef tools fill:#422006,stroke:#fbbf24,color:#f8fafc,stroke-width:2px;
    classDef engines fill:#0c4a6e,stroke:#22d3ee,color:#f8fafc,stroke-width:2px;
    classDef quality fill:#1c1917,stroke:#a8a29e,color:#f8fafc,stroke-width:2px;
    classDef mcode fill:#3f1d1d,stroke:#f87171,color:#f8fafc,stroke-width:2px;
    classDef inner fill:#0f172a,stroke:#64748b,color:#f8fafc,stroke-width:1px;
    classDef smt fill:#581c87,stroke:#e879f9,color:#f8fafc,stroke-width:2px;
    classDef legend fill:#0f172a,stroke:#94a3b8,color:#f8fafc,stroke-width:1px;
    classDef added fill:#1f2937,stroke:#f59e0b,color:#fef3c7,stroke-width:3px;
    classDef wip fill:#111827,stroke:#9ca3af,color:#d1d5db,stroke-width:2px,stroke-dasharray:5 5;
    classDef addedwip fill:#111827,stroke:#f59e0b,color:#fef3c7,stroke-width:3px,stroke-dasharray:5 5;
    classDef old fill:#1f2937,stroke:#64748b,color:#f1f5f9,stroke-width:1px;

    class FRONTEND frontend;
    class EIDOS mid;
    class BACKENDS backend;
    class RUNTIME runtime;
    class TOOLS tools;
    class ENGINES engines;
    class QUALITY quality;
    class MCODE mcode;
    class CHECKS,VERIFY,SERVICES inner;
    class SMT,TV smt;
    class LEGEND legend;
    class L1 old;
    class L2 added;
    class L3,L4 wip;
    class N1,N2,N3,N4,N5,N6,N7,N8 wip;
    class MTE,Funis,Ueval,ArrayAlias,Superopt,Profiles,CI wip;
    class FrameAlloc,Relocate,Cascade addedwip;
    class DevTools,RunFlags,StdLib,Parse,Infer,Results,Strings,Pipelines,Tasks,Realtime,Escape,LLVMGen,Fabric,Roots,Catalogue,CL,GC,Policy,Arenas,RootRegion,StringRT,Sched,RandSvc,Diff,Bench,Gates added;
    class LoopSchedules,MultiShot,ClassFieldAlias,LiveUpdates,CheapReloc,CascadeCompiler,SuperoptLoop,Budget wip;
    linkStyle default stroke:#94a3b8,stroke-width:1.5px;
```

## Highlights

- **Traits and bounds.** `trait`, `extend Type: Trait`, bounds such as
  `[T: A + B]`, default methods and supertraits. Calls are statically
  dispatched per instantiation.
- **Programs can import the standard library.** `import std.crypto.hash`
  and the like, for the 17 of 89 modules that check clean on their own.
- **Defined integer arithmetic, on both back ends.**
  - Integer division and remainder by zero, and signed `MIN / -1`, panic
    with the source location. They used to be undefined behaviour.
  - Shift amounts are taken modulo the width.
  - Float-to-integer casts saturate, and NaN converts to 0.
- **Garbage collection is safe while other threads run Codira code.**
  Collections stop the world: every thread running Codira code parks at a
  safepoint, and its stack is scanned. Hot reload relinks with the world
  stopped, and only when no thread is inside Codira code.
- **Semantic envelopes.**
  - `@envelope(allocation: none, foreign: none, ...)` on a function, and an
    `[envelope]` table in `codira.toml`, are checked over the whole call
    graph.
  - Envelopes are recorded in proof certificates, and `codira api-diff`
    treats a widened envelope as a major change.
- **A machine-code constant-time check.** `codira build --check-constant-time`
  checks the machine code emitted for `oblivious` and `@constant_time`
  functions, on x86-64 and AArch64. It rejects secret-dependent branches,
  variable-time instructions, calls to unchecked code, and (on x86-64)
  secret-dependent addresses.
- **Native storage and tensor engines** (experimental):
  - `codira_db`: a write-ahead log, snapshot-isolation transactions and
    checkpoints.
  - `codira_ai`: tensors, reverse-mode autodiff, layers and optimisers.

  Programs reach both through runtime services.

## Language

- **Traits.** Supported: required and default methods, supertraits,
  associated functions such as `func make() -> Self`, and implementations
  for structs, classes, enums, generic types, primitives and `str`. Each
  implementation is checked against its trait, and a call whose type does
  not meet a bound is an error that names the type, the trait and the
  parameter. Generic traits, trait objects, associated types and operators
  on a type parameter are rejected with an error.
- **Functions returning `never`** compile and run on both back ends. In
  particular, `panic` can be called.
- **Bindings.** These are now errors rather than being accepted without
  effect:
  - assigning to a `let`, including compound assignment and assigning to a
    field of a value struct held in one;
  - `mut`/`inout`/`out` parameters (arguments are passed by copy);
  - module-level `let`/`var`.

  Class fields and array elements stay assignable through a `let`.
- **Integer arithmetic, shifts and casts** are defined; see
  `agents/USING_CODIRA.md`, "Integer arithmetic, shifts and casts". A
  division check is left out only when the divisor is a positive literal or
  the verifier proves it safe.
  - **Measured cost:** within run-to-run noise (2%) on division-heavy loops
    on x86-64 at `-O2`.
  - **Shift masking:** costs no instruction at 32 and 64 bits.
- **`print`/`println`** evaluate all arguments left to right before printing
  anything. Floats print in their own precision (`1.0`, `0.1`, `NaN`, `inf`).
- **Arrays can be assigned** (`a = b`, `rows[i] = row`), which used to crash
  the code generator.
- **`select` and session types**, listed as not in 26.10, are on `main`.

## Standard library

- **Importable modules:**
  - `std.crypto.hash` (SHA-256, SHA-512, BLAKE3, via the RustCrypto `sha2`
    and `blake3` crates);
  - `std.fs.fs`;
  - `std.memory.region` and `std.memory.page`;
  - `std.bit.bit` and `std.bit.mask`;
  - `std.math.constants`, `std.math.fast` and `std.math.math`;
  - `std.hashlib.hash` (FNV-1a, not cryptographic);
  - `std.base64.base64` (RFC 4648 base64, base32 and base16);
  - `std.random.rng` (xoshiro256\*\*, not cryptographic);
  - `std.time.time`, `std.sys.terminate`, `std.complex.complex` and
    `std.collections.optional`;
  - `std.builtin.type_aliases`.
- **Testing.** An importability test records, for every std module, whether
  a program importing it checks, builds and loads, and why not if it does
  not.
- **`std.math.fast` now meets its stated error bounds.** Measured against
  libm, it was off by up to 0.39 where it claimed 1.7e-4.
- **New runtime services:**
  - files (`codira_fs_*`) and hashes (`codira_hash_*`);
  - time (`codira_time_*`) and process termination hooks;
  - a database (`codira_db_*`) and tensors (`codira_ai_*`).

  All of them use generation-checked handles and negative error codes, and
  are wrapped by `std/fs`, `std/crypto`, `std/db` and `std/ai/runtime.code`.

## Verification and tooling

- **Semantic envelopes.**
  - Eight keys are checked: allocation, blocking, panic, termination,
    foreign code, concurrency, effects and secrecy.
  - Keys that cannot be checked yet are rejected.
  - `codira envelope --json` reports every function's inferred envelope.
  - Proof certificates are now `codira-certificate/2`, and `codira certify`
    re-derives the recorded envelopes.
- **`codira build --check-constant-time`** checks machine code. At `-O0` it
  found a real leak, now fixed: an 8-bit or `bool` `ct_select` compiled to
  a branch or a secret-dependent load, although its IR was branch-free.
- **`codira build --superoptimize`** (experimental, x86-64 ELF):
  - It replaces each function's straight-line integer tail with a cheaper
    sequence that Z3 proves equal, and re-proves the bytes it writes.
  - It refuses constant-time code.
- **Differential testing.** Seeded random programs run on LLVM `-O2`,
  LLVM `-O0` and Cranelift, and their outputs are compared. Every
  divergence it found is fixed and kept as a regression test:
  - **A wrong-code bug in the Eidos constant fold:** it dropped output.
  - **A Cranelift ABI bug that only an optimised build of the compiler
    showed:** bool and 8/16-bit values were not extended across calls.
  - **A nested-`if` crash at `-O0`.**
  - **Mismatches between Cranelift and LLVM:**
    - a crash on float-to-narrow-int casts;
    - argument evaluation order;
    - float printing.

## Runtime

- **Stop-the-world collection.**
  - **Guaranteed:** no live object is freed, whichever thread collects:
    `gc_collect`, an allocation at the heap limit, or `codira_gc_collect`.
    `save_heap`/`restore_heap` and hot-reload heap migration are protected
    the same way.
  - **Which threads count:** host threads inside `invoke`, spawned tasks,
    and threads holding `enter_codira()` (C: `codira_thread_enter` and
    `codira_thread_leave`).
  - **Measured cost:** entering Codira code from the host costs about
    14 ns.
  - **Not guaranteed:** see "What is not guaranteed" in
    `agents/USING_CODIRA.md`. A loop that neither allocates nor blocks
    delays a collection until it ends, and functions called through raw
    pointers without the guard are invisible to the collector.
- **`[native]` sources.**
  - Assembly functions are found in the library that declares them, so
    they link on Unix.
  - C++ static constructors and destructors run on ELF: the compiler emits
    `.init_array`/`.fini_array` and `__dso_handle`.
  - On macOS, C++ links against libc++.
- **macOS.**
  - Libraries are ad-hoc signed by LLD (`-adhoc_codesign`) instead of by a
    separate signing step.
  - Hot reload sees the compiler's rename.

## Native engines (experimental)

- **`codira_db`.**
  - **What it is:** a checksummed write-ahead log, MVCC snapshot isolation
    (first committer wins), atomic checkpoints, and typed tables with
    add-column migrations.
  - **Crash testing:** a crash-injection harness crashes a workload at every
    storage operation, about 2,200 simulated crashes. Recovery always gives
    exactly a prefix of the committed transactions.
  - **Limits:** snapshot isolation is not serializable; the data set must
    fit in memory; only one process may use a directory; the durability
    claim is for Unix only; it has never been through a real power loss.
- **`codira_ai`.**
  - **What it is:** strided tensors with broadcasting, a blocked matmul
    (bit-identical to its reference), tape-based reverse-mode autodiff
    checked against numerical gradients, attention and transformer layers,
    Adam/AdamW, INT8 weights and a BPE tokenizer.
  - **Limits:** single-threaded with no explicit SIMD. INT8 is slower than
    f32 (a recorded negative result). It has not been compared with other
    frameworks.

## Build, CI and supply chain

- **CI runs its tests again.** Its first job had been failing, so for many
  runs no test ran on any platform. Fixed in this release:
  - the `nextest` custom-harness protocol in Funis;
  - Windows' static system libraries;
  - LLD on every runner;
  - tests that assumed a Windows host or a newer toolchain;
  - the mdbook job removed, because the book is not in the repository.
- **`cargo audit`:** 38 advisories in 26.10's lockfile, 1 left.
  - **How:** semver-compatible updates, removing `apple-codesign` (which
    brought TLS, HTTP, zip, plist and an RSA crate with an unfixable
    advisory), trimming `object`'s features, and replacing `tempdir`.
  - **Left:** RUSTSEC-2025-0055 in `tracing-subscriber` 0.2, which arkworks
    0.5 uses for the zero-knowledge prover's constraint tracing.

## Experimental

The following are on `main`, opt-in, and not covered by any stability
promise:
- the native engines and their runtime services;
- `--superoptimize`;
- the array alias classes (`CODIRA_ARRAY_TBAA=1`);
- `--sanitize=memtag`;
- `codira_mcode` and its cost profiles (the AArch64 profile is transcribed,
  never measured);
- the GPU target, the quantum and circuit back ends, and Funis.

`--check-constant-time` and envelopes are new in this release and should be
treated as experimental until they have more use.

## Status for production use

Codira 26.11 is **not ready for production or mission-critical use**. What
stands in the way:

- **Platforms.**
  - Linux and Windows (CI run 98, commit `a980406deb`): 3,790 of 3,791
    tests pass on Linux and 2,188 of 2,189 on Windows. The only failure on
    each was a stale generated C header for the runtime's C API, which is
    regenerated in this release. The C++ runtime and doctest jobs pass on
    both.
  - macOS (same run): 3,493 of 3,517 tests pass; 24 fail.
    - **The stale header,** fixed as above.
    - **17 in the Funis linker:**
      - 13 Mach-O and iOS-simulator integration tests from its
        mold-derived suite;
      - its no-leak test;
      - 3 tests of its own test harness.
    - **6 in Codira:**
      - C inlining into Codira callers;
      - an FMA code-generation check;
      - two tuple zero-overhead IR checks;
      - the image-writer comparison;
      - the low-level memory intrinsics test.

    The C++ runtime job passes on macOS. The macOS doctest job failed while
    installing LLVM, before any test ran.
  - The repository's nightly format check fails: the code has not been
    reformatted to its `rustfmt.toml`.
- **Hardware never run:** ARM MTE, CHERI (targets are refused) and GPU
  kernel launches (there is no launch syntax).
- **Standard library:** 72 of 89 modules cannot be imported yet. They need
  `String`, `Pointer[T]`, generic traits, module-level constants or
  in-place parameters.
- **Language gaps:**
  - generic traits, trait objects and associated types;
  - generic enums;
  - `String`;
  - bindings from `if let`, `while let` and `match` are still assignable;
  - `consuming` is accepted without move checking;
  - stack overflow is not ruled out by `@nopanic` or `@realtime`.
- **Runtime:** hot reload has no update admission control. Collections can
  be delayed by loops that neither allocate nor block.
- **Engines:** see their limits above.
- **Review:** no part of the compiler or runtime has had an independent
  security review.

## Upgrading

- **Assigning to a `let`** is now an error; declare the binding with `var`.
- **Rejected forms:** `mut`/`inout`/`out` parameters and module-level
  `let`/`var` are now errors. Return the new value, use a class, or write a
  function that returns the constant.
- **Division:** a division that may divide by zero now panics instead of
  being undefined behaviour. If the divisor is known to be safe, a
  `requires` clause that proves it removes the check.
- **Shift behaviour changed:** a shift by the width or more now uses the
  amount modulo the width.
- **Certificates:** `codira-certificate/2` is written. Version 1 is still
  read and checked, without envelopes.
