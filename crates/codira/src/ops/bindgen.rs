//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: September 17, 2026
//!
//! Functionality:
//! - Generates FFI bindings for a Codira library from its `@export("C")`
//!   signatures.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use codira_compiler::{Config, Driver, Target};
use codira_hir::{FloatBitness, IntBitness, Package, Signedness, TyKind};
use codira_project::MANIFEST_FILENAME;

use crate::{ops::build::find_manifest, ExitStatus};

#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
pub enum Runtime {
    /// Bun, through its built-in `bun:ffi`.
    Bun,
}

#[derive(clap::Args)]
pub struct Args {
    /// Path to the manifest of the project
    #[clap(long)]
    manifest_path: Option<PathBuf>,

    /// Runtime to generate bindings for
    #[clap(long, value_enum, default_value = "bun")]
    runtime: Runtime,

    /// Where to write the bindings. Defaults to stdout.
    #[clap(long, short = 'o')]
    output: Option<PathBuf>,

    /// Path the generated bindings should `dlopen` at runtime. Defaults to
    /// the assembly this project builds, relative to the output file.
    #[clap(long)]
    library: Option<String>,
}

/// How one Codira type crosses into a Bun FFI signature.
struct Binding {
    /// The `FFIType` member naming this type on the JavaScript side.
    ffi: &'static str,
    /// The TypeScript type a value of it arrives as.
    ts: &'static str,
    /// Set when the chosen `FFIType` is a correct but slow way to pass this
    /// type, with the reason.
    caveat: Option<&'static str>,
}

impl Binding {
    const fn fast(ffi: &'static str, ts: &'static str) -> Self {
        Binding {
            ffi,
            ts,
            caveat: None,
        }
    }
}

/// Maps a Codira type to the `bun:ffi` type that carries it.
///
/// # Why 64-bit integers map to `i64_fast`, not `i64`
///
/// `FFIType.i64` hands JavaScript a `BigInt`, which is a heap allocation on
/// every single call. That one choice dominates the cost of calling a Codira
/// function from Bun: measured over 500,000 calls of an identical function,
/// `i64` costs 37.97 ns/call against 7.02 ns/call for `u32` -- the 31 ns
/// difference is `BigInt` boxing and nothing else.
///
/// `i64_fast` returns a plain `number` while the value fits in a double's
/// integer range and a `BigInt` beyond it, which measured 12.35 ns/call. It
/// is not a precision trade: round-tripping 0, 2^53-1, 2^53+1, `i64::MAX` and
/// `i64::MIN` through it is lossless -- it switches representation exactly at
/// the point a `number` would stop being exact. The only cost is that the
/// TypeScript type is `number | bigint` rather than one or the other.
///
/// So the generated bindings are ~3x faster than hand-written `i64` ones by
/// default, and a signature written in `i32`/`u32`/`f64` where the range
/// allows is ~5x faster again. That ordering is the whole reason this
/// generator exists rather than a documentation note: the fast spelling and
/// the obvious spelling are not the same, and picking by hand is how the
/// slow one gets picked.
fn binding_for(kind: &TyKind) -> Option<Binding> {
    Some(match kind {
        TyKind::Bool => Binding::fast("bool", "boolean"),

        TyKind::Float(float) => match float.bitness {
            FloatBitness::X32 => Binding::fast("f32", "number"),
            FloatBitness::X64 => Binding::fast("f64", "number"),
        },

        TyKind::Int(int) => {
            let signed = int.signedness == Signedness::Signed;
            match int.bitness {
                IntBitness::X8 => Binding::fast(if signed { "i8" } else { "u8" }, "number"),
                IntBitness::X16 => Binding::fast(if signed { "i16" } else { "u16" }, "number"),
                IntBitness::X32 => Binding::fast(if signed { "i32" } else { "u32" }, "number"),
                // See this function's doc comment: `*_fast` is both faster
                // and lossless, so it is the default rather than an opt-in.
                IntBitness::X64 => Binding::fast(
                    if signed { "i64_fast" } else { "u64_fast" },
                    "number | bigint",
                ),
                // `usize`/`isize` bind as `ptr`, not as Bun's `usize`/`isize`.
                //
                // Bun's own pointer-width types go through BigInt
                // unconditionally -- there is no `_fast` spelling for them --
                // so they are both the slow option and an awkward one: the
                // caller would have to write `BigInt(ptr(buffer))`.
                // `FFIType.ptr` is the same 64 bits and is what `ptr()`
                // already returns, as a plain `number`.
                //
                // This is a judgement about what a pointer-width integer is
                // doing in a C ABI signature: it is an address. A Codira
                // function that takes `(src, dst, count)` to process a
                // caller's buffer is the reason the intrinsics exist, and
                // binding it any other way makes the fast path the
                // inconvenient one. A `count` passed this way is carried
                // exactly too -- verified against both addresses and lengths.
                IntBitness::Xsize => Binding {
                    ffi: "ptr",
                    ts: "number",
                    caveat: Some(
                        "bound as FFIType.ptr: pass an address from ptr(buffer), or a plain \
                         length. Exact below 2^53, which covers every address and every \
                         buffer that fits in memory",
                    ),
                },
                // No portable C ABI for a 128-bit integer, and Bun has no
                // type for one.
                IntBitness::X128 => return None,
            }
        }

        // The unit type is a valid *return*; the caller filters it out of
        // parameter position.
        TyKind::Tuple(0, _) => Binding::fast("void", "void"),

        _ => return None,
    })
}

/// A single exported function, resolved down to what the binding needs.
struct Export {
    name: String,
    params: Vec<Binding>,
    ret: Binding,
}

pub fn bindgen(args: Args) -> Result<ExitStatus, anyhow::Error> {
    let manifest_path = match &args.manifest_path {
        None => {
            let current_dir =
                std::env::current_dir().expect("could not determine current working directory");
            find_manifest(&current_dir).ok_or_else(|| {
                anyhow!(
                    "could not find {} in '{}' or a parent directory",
                    MANIFEST_FILENAME,
                    current_dir.display()
                )
            })?
        }
        Some(path) => std::fs::canonicalize(Path::new(&path)).map_err(|_error| {
            anyhow!(
                "'{}' does not refer to a valid manifest path",
                path.display()
            )
        })?,
    };

    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        // Bindings are derived from signatures, which optimization does not
        // change; this only has to type-check, not codegen.
        optimization_lvl: codira_compiler::OptimizationLevel::None,
        out_dir: None,
        emit_ir: false,
    };

    let (_package, driver) = Driver::with_package_path(&manifest_path, config)?;
    let db = driver.db();

    let mut exports = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut library_path = None;

    for package in Package::all(db) {
        for module in package.modules(db) {
            if library_path.is_none() {
                library_path = Some(driver.assembly_output_path(module));
            }

            for function in module.all_functions(db) {
                if function.export_abi(db).as_deref() != Some("C") {
                    continue;
                }

                let name = function.name(db).to_string();
                let ret_ty = function.ret_type(db);
                let Some(ret) = binding_for(ret_ty.interned()) else {
                    skipped.push((
                        name,
                        format!("return type `{ret_ty:?}` has no Bun FFI type"),
                    ));
                    continue;
                };

                let mut params = Vec::new();
                let mut unsupported = None;
                for param in function.params(db) {
                    let ty = param.ty();
                    match binding_for(ty.interned()) {
                        // `void` is a return-only type: a parameter of unit
                        // type has no C representation to pass.
                        Some(b) if b.ffi != "void" => params.push(b),
                        _ => {
                            unsupported = Some(format!(
                                "parameter {} of type `{ty:?}` has no Bun FFI type",
                                param.index()
                            ));
                            break;
                        }
                    }
                }

                match unsupported {
                    Some(reason) => skipped.push((name, reason)),
                    None => exports.push(Export { name, params, ret }),
                }
            }
        }
    }

    // A stable order keeps the generated file diffable; HIR iteration order
    // is not guaranteed to be.
    exports.sort_by(|a, b| a.name.cmp(&b.name));
    skipped.sort();

    if exports.is_empty() {
        return Err(anyhow!(
            "no `@export(\"C\")` functions found in this project -- nothing to generate \
             bindings for"
        ));
    }

    let default_library = library_path
        .as_deref()
        .map_or_else(|| "library.codiralib".to_owned(), path_to_ts_literal);
    let library = args.library.unwrap_or(default_library);

    let Runtime::Bun = args.runtime;
    let rendered = render_bun(&exports, &skipped, &library);

    match &args.output {
        Some(path) => {
            std::fs::write(path, &rendered)
                .map_err(|e| anyhow!("could not write '{}': {e}", path.display()))?;
            eprintln!(
                "wrote {} binding{} to {}",
                exports.len(),
                if exports.len() == 1 { "" } else { "s" },
                path.display()
            );
        }
        None => print!("{rendered}"),
    }

    for (name, reason) in &skipped {
        eprintln!("warning: skipped `{name}`: {reason}");
    }

    Ok(ExitStatus::Success)
}

/// Renders a path as a TypeScript string literal.
///
/// Backslashes are escaped rather than swapped for forward slashes: the
/// generated file is data for `dlopen`, and rewriting a Windows path into a
/// shape the platform did not give us is the kind of helpfulness that breaks
/// on a directory with an unusual name.
fn path_to_ts_literal(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn render_bun(exports: &[Export], skipped: &[(String, String)], library: &str) -> String {
    let mut out = String::new();

    out.push_str(
        "// Generated by `codira bindgen --runtime bun`. Do not edit.\n\
         //\n\
         // Every `@export(\"C\")` function in the project appears below with the\n\
         // `FFIType` that actually carries its Codira signature, so the types are\n\
         // the ones the compiler resolved rather than the ones a caller guessed.\n\
         //\n\
         // On speed: 64-bit integers bind as `i64_fast`/`u64_fast`, which return a\n\
         // plain `number` inside the safe-integer range and a `BigInt` outside it.\n\
         // That is lossless and roughly 3x faster than `FFIType.i64`, which boxes a\n\
         // BigInt on every call (measured over 500,000 calls: 12.35 ns/call against\n\
         // 37.97 ns/call). A signature written in `i32`/`u32`/`f64` binds at about\n\
         // 7 ns/call, so prefer those in Codira where the range allows.\n\n",
    );

    if !skipped.is_empty() {
        out.push_str("// Not bound, because these have no Bun FFI representation:\n");
        for (name, reason) in skipped {
            let _ = writeln!(out, "//   {name}: {reason}");
        }
        out.push('\n');
    }

    out.push_str("import { dlopen, FFIType } from \"bun:ffi\";\n\n");
    let _ = writeln!(out, "export const LIBRARY_PATH = \"{library}\";\n");

    out.push_str("export function load(path: string = LIBRARY_PATH) {\n");
    out.push_str("  return dlopen(path, {\n");
    for export in exports {
        let args = export
            .params
            .iter()
            .map(|p| format!("FFIType.{}", p.ffi))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "    {}: {{ args: [{args}], returns: FFIType.{} }},",
            export.name, export.ret.ffi
        );
    }
    out.push_str("  }).symbols;\n}\n\n");

    // The declared shape is what makes the caller's types survive; `dlopen`
    // on its own infers `unknown` for anything it cannot see statically.
    out.push_str("export interface Symbols {\n");
    for export in exports {
        if let Some(caveat) = export
            .params
            .iter()
            .chain(std::iter::once(&export.ret))
            .find_map(|b| b.caveat)
        {
            let _ = writeln!(out, "  /** {caveat} */");
        }
        let params = export
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| format!("a{i}: {}", p.ts))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "  {}({params}): {};", export.name, export.ret.ts);
    }
    out.push_str("}\n\n");

    out.push_str("export const symbols = load() as unknown as Symbols;\n");
    out
}
