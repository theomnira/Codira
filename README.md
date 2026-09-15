<!--
Copyright (c) 2026 Omnira CJSC
Author: Tunjay Akbarli
Date: August 6, 2026

Functionality: Codira Programming Language
-->

# Codira Programming Language 
### Stable Version: 26.9
### Build: September 15, 2026.

_Codira_ is an Ahead of Time (AOT) programming language for high performance systems.

## Features

- **Ahead of time compilation** - Codira is compiled ahead of time (AOT), as
  opposed to being interpreted or compiled just in time (JIT). By detecting
  errors in the code during AOT compilation, an entire class of runtime errors
  is eliminated. This allows developers to stay within the comfort of their IDE
  instead of having to switch between the IDE and target application to debug
  runtime errors.

- **Statically typed** - Codira resolves types at compilation time instead of at
  runtime, resulting in immediate feedback when writing code and opening the
  door for powerful refactoring tools.

- **First class hot-reloading** - Every aspect of Codira is designed with hot
  reloading in mind. Hot reloading is the process of changing code and resources
  of a live application, removing the need to start, stop and recompile an
  application whenever a function or value is changed.

- **Performance** - AOT compilation combined with static typing ensure that Codira
  is compiled to machine code that can be natively executed on any target
  platform. LLVM is used for compilation and optimization, guaranteeing the best
  possible performance. Hot reloading does introduce a slight runtime overhead,
  but it can be disabled for production builds to ensure the best possible
  runtime performance.

- **Cross compilation** - The Codira compiler is able to compile to all supported
  target platforms from any supported compiler platform.

- **Powerful IDE integration** - The Codira language and compiler framework are
  designed to support source code queries, allowing for powerful IDE
  integrations such as code completion and refactoring tools.
  
```mermaid
flowchart TD
    subgraph FRONTEND [" 1. Frontend & AST Analysis "]
        direction TB
        Src["Codira Source Code<br/>.cod"] --> Lexer["Lexer & Parser"]
        Lexer --> AST["Abstract Syntax Tree"]
        
        subgraph TC [" Type System & Formal Checker "]
            AST --> DynamicTC["Morphic Type Analysis"]
            DynamicTC --> LiquidTC["Refinement Type Checker<br/>Liquid Types"]
            LiquidTC <==>|Invariants Proofs| SMT_Front["codira_smt / Z3"]
        end
        
        TC --> HIR["High-Level IR / Desugared AST"]
    end

    FRONTEND ==>|Lowering Pass| LOWERING

    subgraph LOWERING [" 2. IR Translation & Lowering "]
        HIR --> BuildMIR["codira_mir Builder"]
        BuildMIR --> UnTypedMIR["Region-Structured SSA MIR<br/>codira_mir Dialect"]
    end

    LOWERING ==>|Region-SSA Stream| EIDOS_ENGINE

    subgraph EIDOS_ENGINE [" 3. Mid-End Optimization Pipeline "]
        direction TB

        subgraph MIR_STACK [" Region SSA Infrastructure "]
            UnTypedMIR --> MorphicLower["Morphic Layout Transform<br/>AoS &lt;-&gt; SoA / Value &lt;-&gt; Ref"]
            MorphicLower --> StructSSA["Strongly-Typed Region SSA<br/>i8..i64, f32/f64, Vectors, Tokens"]
        end

        subgraph SATURATION [" codira_egraph Engine (~2K LoC) "]
            StructSSA --> EGraphInit["E-Graph Ingestion"]
            
            subgraph EGRAPH_CORE [" Non-Destructive Saturation Loop "]
                EGraphInit <--> EClass["E-Classes & E-Nodes"]
                EClass <--> MemSSA["Virtual Memory Tokens<br/>Store-to-Load / Dead Store"]
                EClass <--> RuleSet["CEGIS Synthesized Rules"]
            end

            EGRAPH_CORE <==>|Translation Validation| TV["codira_tv"]
            TV <==>|Formal Logic Checks| SMT_Mid["codira_smt Solver"]
        end

        subgraph CEGIS [" CEGIS Rule Discovery Engine "]
            CEGIS_Loop["Grammar Search"] <==>|Counterexample Verification| SMT_Mid
            CEGIS_Loop -->|Inject Proven Rewrites| RuleSet
        end

        subgraph EXTRACTION [" Optimal IR Extraction "]
            EGRAPH_CORE --> CostModel["Microarchitectural Cost Engine<br/>Port Latency / Cache / Reg Pressure"]
            CostModel --> Extractor["ILP / SMT Extractor"]
            Extractor --> OptMIR["Optimal Monomorphized codira_mir"]
        end
    end

    EIDOS_ENGINE ==>|Optimized MIR| BACKEND

    subgraph BACKEND [" 4. Code Generation & Execution Tier "]
        direction TB
        
        OptMIR --> LowerSelect{"Execution Target?"}
        
        LowerSelect -->|JIT / Macro Execution| JIT["In-Flight JIT Bytecode Interpreter"]
        LowerSelect -->|AOT Native Compilation| LLVM_Gen["Codira Codegen Pass"]
        
        LLVM_Gen --> TargetIR["LLVM IR / Target Assembly"]
        TargetIR --> Bin["Native Binary Target<br/>AVX-512 / ARM Neon / RISC-V Vector"]
    end

    classDef frontend fill:#1e293b,stroke:#38bdf8,color:#f8fafc,stroke-width:2px;
    classDef low fill:#0f172a,stroke:#818cf8,color:#f8fafc,stroke-width:2px;
    classDef engine fill:#172554,stroke:#60a5fa,color:#f8fafc,stroke-width:2px;
    classDef backend fill:#064e3b,stroke:#34d399,color:#f8fafc,stroke-width:2px;
    classDef smt fill:#581c87,stroke:#c084fc,color:#f8fafc,stroke-width:2px;

    class FRONTEND frontend;
    class LOWERING low;
    class EIDOS_ENGINE engine;
    class BACKEND backend;
    class SMT_Front,SMT_Mid,TV smt;
```
## Example

<!-- inline HTML is intentionally used to add the id. This allows retrieval of the HTML -->
<pre language="codira">
<code id="code-sample">func fibonacci(n: i32) -> i32 {
    if n <= 1 {
        n
    } else {
        fibonacci(n - 1) + fibonacci(n - 2)
    }
}

// Comments: functions marked as `public` can be called outside the module
public func main() {
    // Native support for bool, f32, f64, i8, u8, u128, i128, usize, isize, etc
    let is_true = true;
    let var = 0.5;

    // Type annotations are not required when a variable's type can be deduced
    let n = 3;

    let result = fibonacci(n);

    // Adding a suffix to a literal restricts its type
    let lit = 15u128;

    let foo = record();
    let bar = tuple();
    let baz = on_heap();
}

// Both record structs and tuple structs are supported
struct Record {
    n: i32,
}

// Struct definitions include whether they are allocated by a garbage collector
// (`gc`) and passed by reference, or passed by `value`. By default, a struct
// is garbage collected.
struct(value) Tuple(f32, f32);

struct(gc) GC(i32);

// The order of function definitions doesn't matter
func record() -> Record {
    // Codira allows implicit returns
    Record { n: 7 }
}

func tuple() -> Tuple {
    // Codira allows explicit returns
    return Tuple(3.14, -6.28);
}

func on_heap() -> GC {
    GC(0)
}</code>
</pre>


## Building from Source

Make sure you have the following dependencies installed on your machine:

* Rust
* LLVM 22.1 (a full dev distribution with `llvm-config` and the LLD static
  libraries; point `LLVM_SYS_221_PREFIX` at its install root -- see
  `book/src/dev/02-building-llvm.md`)
* Z3 (`libz3` -- used by `codira_smt` for refinement-type checking and
  translation validation)

Clone the source code, including all submodules:

```bash
git clone https://github.com/theomnira/codira.git
git submodule update --init --recursive
```

Use `cargo` to build a release version

```bash
cargo build --release
```

## Language server

Codira contains support for the lsp protocol, start the executable using:

```bash
codira language-server
```

Alternatively, you can install editor-specific extensions.
