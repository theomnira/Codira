//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - `codira lint`: lints that use what the compiler knows about the code,
//!   beyond style. Each one reports only what the compiler established, not a
//!   guess, and each can be silenced.
//!
//! The lints:
//!
//! - `alloc-in-loop`: an allocation the effect analysis finds in a loop -- a
//!   `class` constructed, an array literal, `print`, a boxed cross-module call
//!   -- or a call in a loop to a function whose summary allocates. An
//!   allocation the analysis only assumes (in foreign code or behind a
//!   `perform`) is not reported;
//! - `large-copy`: a parameter whose type is a value struct (or tuple) of at
//!   least 128 bytes, copied on every call. Types whose size is not known
//!   without instantiating a generic are not reported;
//! - `unused`: a private function, struct, enum, constant or type alias that
//!   nothing in the package names. Anything with an attribute other than
//!   `@noalloc`, `@nopanic`, `@noblock`, `@realtime`, `@envelope` or `@inline`
//!   is left alone (it may be an entry point), as are trait implementations and
//!   `main`;
//! - `extern-assertion`: an `extern` C function trusted to be `@noalloc` or
//!   `@noblock` that the C library documents as allocating or blocking
//!   (`malloc`, `printf`, `sleep`, ...);
//! - `extern-gc-reference`: an `extern "C"` function that passes or returns a
//!   `class` (garbage-collected) value; C code that keeps the pointer holds a
//!   reference the collector does not see;
//! - `asm`: an `@asm` variant marked `pure: true` that has no output (the
//!   optimiser may delete every call) or clobbers memory (it has an effect
//!   beyond its outputs);
//! - `unproven-division`: an integer `/` or `%` whose divisor the compiler did
//!   not prove safe, so it is checked at run time and can panic -- the same
//!   decision the code generators follow;
//! - `noalloc-candidate` (off by default; `--warn noalloc-candidate`): a public
//!   function the analysis proves never allocates, not yet marked `@noalloc`
//!   (or `@realtime`, or with an `@envelope` that rules allocation out).
//!
//! Silencing: `--allow <lint>` on the command line; a comment
//! `// codira-lint: allow(<lint>, ...)` on the reported line or the line
//! above it; `//! codira-lint: allow(<lint>)` anywhere in a file for the
//! whole file. `allow(all)` silences every lint.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

use anyhow::anyhow;
use codira_compiler::{Config, Driver, Target};
use codira_hir::{
    realtime::{Effect, Source},
    ArithOp, BinaryOp, Expr, FloatBitness, Function, HasSource, HirDatabase, InFile, IntBitness,
    Package, StructMemoryKind, Ty, TyKind,
};
use codira_hir_input::{FileId, SourceRootId};
use codira_syntax::{AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange};

use crate::{ops::build::locate_manifest, ExitStatus};

/// Every lint: name and one-line description.
pub(crate) const LINTS: &[(&str, &str)] = &[
    (
        "alloc-in-loop",
        "an allocation, or a call that allocates, inside a loop",
    ),
    (
        "large-copy",
        "a parameter of a value type of 128 bytes or more, copied on every call",
    ),
    ("unused", "a private item nothing in the package names"),
    (
        "extern-assertion",
        "an extern trusted as @noalloc/@noblock that the C library documents otherwise",
    ),
    (
        "extern-gc-reference",
        "an extern \"C\" function that passes or returns a garbage-collected class",
    ),
    (
        "asm",
        "a pure @asm variant with no output, or one that clobbers memory",
    ),
    (
        "unproven-division",
        "an integer division whose divisor is not proven safe",
    ),
    (
        "noalloc-candidate",
        "a public function proven never to allocate, without @noalloc (off by default)",
    ),
];

/// Lints that only report with `--warn <lint>`: suggestions, not problems.
const OFF_BY_DEFAULT: &[&str] = &["noalloc-candidate"];

const LARGE_COPY_BYTES: u64 = 128;

/// The workspace's source root: the package or directory being linted, as
/// opposed to the standard library.
const WORKSPACE: SourceRootId = SourceRootId(0);

#[derive(clap::Args)]
pub struct Args {
    /// Path to the manifest of the package to lint
    #[clap(long, conflicts_with = "path")]
    manifest_path: Option<PathBuf>,

    /// Directory of Codira sources to lint instead of a package
    #[clap(long)]
    path: Option<PathBuf>,

    /// Do not report this lint (repeatable)
    #[clap(long, value_name = "LINT")]
    allow: Vec<String>,

    /// Report this lint, which is off by default (repeatable)
    #[clap(long, value_name = "LINT")]
    warn: Vec<String>,

    /// Exit with an error when this lint reports anything (repeatable;
    /// `all` for every lint). Implies `--warn`
    #[clap(long, value_name = "LINT")]
    deny: Vec<String>,

    /// List the lints and exit
    #[clap(long)]
    list: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Finding {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub lint: &'static str,
    pub message: String,
    pub help: Option<String>,
}

pub fn lint(args: Args) -> anyhow::Result<ExitStatus> {
    if args.list {
        for (name, what) in LINTS {
            println!("{name:20} {what}");
        }
        return Ok(ExitStatus::Success);
    }
    for name in args.allow.iter().chain(&args.deny).chain(&args.warn) {
        if name != "all" && !LINTS.iter().any(|(n, _)| n == name) {
            return Err(anyhow!(
                "unknown lint '{name}'; `codira lint --list` lists them"
            ));
        }
    }
    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: codira_compiler::OptimizationLevel::None,
        mode: codira_compiler::BuildMode::default(),
        out_dir: None,
        emit_ir: false,
    };
    let source_dir = match &args.path {
        Some(path) => path.clone(),
        None => {
            let manifest = locate_manifest(args.manifest_path.as_deref())?;
            codira_project::Package::from_file(&manifest)?.source_directory()
        }
    };
    let driver = Driver::with_source_directory(&source_dir, config)?;
    let db: &dyn HirDatabase = driver.db();

    let parse_errors: usize = workspace_files(db)
        .iter()
        .map(|&f| db.parse(f).errors().len())
        .sum();
    if parse_errors > 0 {
        eprintln!(
            "note: the sources have {parse_errors} syntax error(s); `codira check` reports them. \
             Lints run on what parses."
        );
    }

    let mut findings = run_lints(db);
    let enabled = |lint: &str| {
        !OFF_BY_DEFAULT.contains(&lint)
            || args
                .warn
                .iter()
                .chain(&args.deny)
                .any(|w| w == lint || w == "all")
    };
    findings.retain(|f| enabled(f.lint));
    let allowed: HashSet<&str> = args.allow.iter().map(String::as_str).collect();
    findings.retain(|f| !allowed.contains(f.lint) && !allowed.contains("all"));
    let directives = Directives::of(db);
    findings.retain(|f| !directives.allows(f));
    findings.sort();
    findings.dedup();

    let mut per_lint: BTreeMap<&str, usize> = BTreeMap::new();
    for f in &findings {
        println!(
            "warning[{}]: {}:{}:{}: {}",
            f.lint, f.path, f.line, f.column, f.message
        );
        if let Some(help) = &f.help {
            println!("    help: {help}");
        }
        *per_lint.entry(f.lint).or_default() += 1;
    }
    let counts: Vec<String> = per_lint.iter().map(|(l, n)| format!("{l}: {n}")).collect();
    println!(
        "{} finding(s){}",
        findings.len(),
        if counts.is_empty() {
            String::new()
        } else {
            format!(" ({})", counts.join(", "))
        }
    );
    let denied = findings
        .iter()
        .any(|f| args.deny.iter().any(|d| d == "all" || d == f.lint));
    Ok(ExitStatus::from(!denied))
}

fn workspace_files(db: &dyn HirDatabase) -> Vec<FileId> {
    let mut files: Vec<FileId> = workspace_functions(db)
        .iter()
        .map(|f| f.file_id(db))
        .collect();
    for package in Package::all(db) {
        for module in package.modules(db) {
            if let Some(file) = module.file_id(db) {
                if db.file_source_root(file) == WORKSPACE {
                    files.push(file);
                }
            }
        }
    }
    files.sort_by_key(|f| f.0);
    files.dedup();
    files
}

fn workspace_functions(db: &dyn HirDatabase) -> Vec<Function> {
    Package::all(db)
        .into_iter()
        .flat_map(|p| p.modules(db))
        .flat_map(|m| m.all_functions(db))
        .filter(|f| db.file_source_root(f.file_id(db)) == WORKSPACE)
        .collect()
}

pub(crate) fn run_lints(db: &dyn HirDatabase) -> Vec<Finding> {
    let functions = workspace_functions(db);
    let mut out = Vec::new();
    for &function in &functions {
        alloc_in_loop(db, function, &mut out);
        large_copy(db, function, &mut out);
        extern_lints(db, function, &mut out);
        unproven_division(db, function, &mut out);
        noalloc_candidate(db, function, &mut out);
    }
    unused(db, &mut out);
    out
}

fn finding(
    db: &dyn HirDatabase,
    file: FileId,
    range: TextRange,
    lint: &'static str,
    message: String,
    help: Option<String>,
) -> Finding {
    let pos = db.line_index(file).line_col(range.start());
    Finding {
        path: db.file_relative_path(file).to_string(),
        line: pos.line + 1,
        column: pos.col_utf16 + 1,
        lint,
        message,
        help,
    }
}

/// The syntax node a pointer points at.
fn node_at(db: &dyn HirDatabase, site: &InFile<SyntaxNodePtr>) -> SyntaxNode {
    let root = db.parse(site.file_id).syntax_node();
    site.value.to_node(&root)
}

/// The innermost loop that runs `node` once per iteration, if any, within
/// its function (a closure is a boundary: its body runs when it is called).
fn enclosing_loop(node: &SyntaxNode) -> Option<SyntaxNode> {
    let mut child = node.clone();
    for ancestor in node.ancestors().skip(1) {
        match ancestor.kind() {
            SyntaxKind::FUNCTION_DEF | SyntaxKind::CLOSURE_EXPR => return None,
            SyntaxKind::WHILE_EXPR | SyntaxKind::LOOP_EXPR => return Some(ancestor),
            // The iterable of a `for` is evaluated once; only the body
            // repeats.
            SyntaxKind::FOR_EXPR if child.kind() == SyntaxKind::BLOCK_EXPR => {
                return Some(ancestor)
            }
            _ => {}
        }
        child = ancestor;
    }
    None
}

fn line_of(db: &dyn HirDatabase, file: FileId, node: &SyntaxNode) -> u32 {
    db.line_index(file).line_col(node.text_range().start()).line + 1
}

/// Whether the analysis *found* the effect at this source, as opposed to
/// assuming it for code it cannot see (foreign code, a `perform`).
fn is_definite(source: &Source) -> bool {
    !source.effects.contains(Effect::Foreign) && !source.effects.contains(Effect::Perform)
}

fn alloc_in_loop(db: &dyn HirDatabase, function: Function, out: &mut Vec<Finding>) {
    let sources = db.effect_sources(function);
    for source in &sources.sources {
        if !source.effects.contains(Effect::Alloc) || !is_definite(source) {
            continue;
        }
        let node = node_at(db, &source.site);
        if let Some(lp) = enclosing_loop(&node) {
            let file = source.site.file_id;
            out.push(finding(
                db,
                file,
                node.text_range(),
                "alloc-in-loop",
                format!(
                    "allocation on every iteration of the loop at line {}: {}",
                    line_of(db, file, &lp),
                    source.what
                ),
                Some("hoist the allocation out of the loop and reuse it".to_string()),
            ));
        }
    }
    for call in &sources.calls {
        let summary = db.effect_summary(call.callee);
        let Some(witness) = summary.witness(Effect::Alloc) else {
            continue;
        };
        if !is_definite(&witness.source) {
            continue;
        }
        let node = node_at(db, &call.site);
        let Some(lp) = enclosing_loop(&node) else {
            continue;
        };
        let mut chain = vec![call.callee.name(db).to_string()];
        chain.extend(witness.calls.iter().map(|c| c.callee.name(db).to_string()));
        let file = call.site.file_id;
        out.push(finding(
            db,
            file,
            node.text_range(),
            "alloc-in-loop",
            format!(
                "`{}` allocates, and is called on every iteration of the loop at line {} \
                 (via {}: {})",
                call.callee.name(db),
                line_of(db, file, &lp),
                chain.join(" -> "),
                witness.source.what
            ),
            Some("hoist the call out of the loop, or pass it a buffer to reuse".to_string()),
        ));
    }
}

/// A lower bound on the size of a value of type `ty` in bytes (fields are
/// summed without padding), or `None` when it is not known without
/// instantiating a generic.
fn value_size(db: &dyn HirDatabase, ty: &Ty, depth: u32) -> Option<u64> {
    if depth > 16 {
        return None;
    }
    Some(match ty.interned() {
        TyKind::Bool => 1,
        TyKind::Int(int) => match int.bitness {
            IntBitness::X8 => 1,
            IntBitness::X16 => 2,
            IntBitness::X32 => 4,
            IntBitness::X64 | IntBitness::Xsize => 8,
            IntBitness::X128 => 16,
        },
        TyKind::Float(float) => match float.bitness {
            FloatBitness::X32 => 4,
            FloatBitness::X64 => 8,
        },
        TyKind::Str => 16,
        TyKind::Struct(s, subst) => {
            if s.data(db).memory_kind == StructMemoryKind::Gc {
                return Some(8);
            }
            if !subst.interned().is_empty() || s.is_generic(db) {
                return None;
            }
            let mut total = 0;
            for field in s.fields(db) {
                total += value_size(db, &field.ty(db), depth + 1)?;
            }
            total
        }
        TyKind::Tuple(_, subst) => {
            let mut total = 0;
            for t in subst.interned() {
                total += value_size(db, t, depth + 1)?;
            }
            total
        }
        TyKind::Optional(inner) => value_size(db, inner, depth + 1)? + 1,
        TyKind::Array(_) | TyKind::Channel(_) | TyKind::Function(_) | TyKind::Session(..) => 8,
        _ => return None,
    })
}

fn large_copy(db: &dyn HirDatabase, function: Function, out: &mut Vec<Finding>) {
    if function.is_extern(db) {
        return;
    }
    for param in function.params(db) {
        let ty = param.ty();
        let is_value = match ty.interned() {
            TyKind::Struct(s, _) => s.data(db).memory_kind == StructMemoryKind::Value,
            TyKind::Tuple(..) => true,
            _ => false,
        };
        if !is_value {
            continue;
        }
        let Some(size) = value_size(db, ty, 0) else {
            continue;
        };
        if size < LARGE_COPY_BYTES {
            continue;
        }
        let Some(src) = param.source(db) else {
            continue;
        };
        let name = param
            .name(db)
            .map_or_else(|| "a parameter".to_string(), |n| format!("`{n}`"));
        out.push(finding(
            db,
            src.file_id,
            src.value.syntax().text_range(),
            "large-copy",
            format!(
                "{name} of `{}` is a value of at least {size} bytes, copied on every call",
                function.name(db)
            ),
            Some(
                "pass only the fields the function uses, or make the type a `class` if it should \
                 be shared rather than copied"
                    .to_string(),
            ),
        ));
    }
}

/// C functions documented to allocate (and so to be able to block on the
/// allocator's lock).
const C_ALLOCATING: &[&str] = &[
    "malloc",
    "calloc",
    "realloc",
    "reallocarray",
    "aligned_alloc",
    "posix_memalign",
    "valloc",
    "memalign",
    "strdup",
    "strndup",
    "asprintf",
    "vasprintf",
    "getline",
    "getdelim",
    "fopen",
    "fdopen",
    "tmpfile",
    "opendir",
    "printf",
    "fprintf",
    "vprintf",
    "vfprintf",
    "puts",
    "fputs",
    "scandir",
    "realpath",
    "backtrace_symbols",
    "setlocale",
    "newlocale",
];

/// C functions documented to block (wait for I/O, time or another thread).
const C_BLOCKING: &[&str] = &[
    "sleep",
    "usleep",
    "nanosleep",
    "clock_nanosleep",
    "read",
    "write",
    "pread",
    "pwrite",
    "readv",
    "writev",
    "recv",
    "recvfrom",
    "recvmsg",
    "send",
    "sendto",
    "sendmsg",
    "accept",
    "connect",
    "poll",
    "select",
    "epoll_wait",
    "wait",
    "waitpid",
    "pause",
    "fsync",
    "fdatasync",
    "flock",
    "fread",
    "fwrite",
    "fgets",
    "getchar",
    "fflush",
    "pthread_join",
    "pthread_mutex_lock",
    "pthread_cond_wait",
    "pthread_rwlock_rdlock",
    "pthread_rwlock_wrlock",
    "sem_wait",
    "open",
    "close",
];

/// The attributes of a function as written: `noalloc`, `envelope`, ...
fn attribute_names(node: &SyntaxNode) -> Vec<(String, String)> {
    node.children()
        .filter(|c| c.kind() == SyntaxKind::ATTRIBUTE_LIST)
        .flat_map(|l| l.children().collect::<Vec<_>>())
        .filter_map(|a| {
            let name = a
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .find(|t| t.kind() == SyntaxKind::IDENT)?
                .text()
                .to_string();
            Some((name, a.text().to_string()))
        })
        .collect()
}

/// Whether the function is declared with the C ABI: inside `extern "C"
/// { .. }`, or `extern "C" func`.
fn is_c_abi(node: &SyntaxNode) -> bool {
    let has_c = |n: &SyntaxNode| {
        n.children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == SyntaxKind::STRING && t.text().trim_matches('"') == "C")
    };
    has_c(node)
        || node
            .children()
            .any(|c| c.kind() == SyntaxKind::EXTERN && has_c(&c))
        || node
            .ancestors()
            .find(|a| a.kind() == SyntaxKind::EXTERN_BLOCK)
            .is_some_and(|b| has_c(&b))
}

fn extern_lints(db: &dyn HirDatabase, function: Function, out: &mut Vec<Finding>) {
    if !function.is_extern(db) {
        return;
    }
    let src = function.source(db);
    let node = src.value.syntax().clone();
    let file = src.file_id;
    let range = src.value.signature_range();
    let name = function.name(db).to_string();
    let attributes = attribute_names(&node);
    let has = |attr: &str| attributes.iter().any(|(n, _)| n == attr);
    let envelope_forbids = |key: &str, value: &str| {
        attributes.iter().any(|(n, text)| {
            n == "envelope" && text.replace(' ', "").contains(&format!("{key}:{value}"))
        })
    };

    let c_abi = is_c_abi(&node);
    if c_abi {
        let claims_noalloc =
            has("noalloc") || has("realtime") || envelope_forbids("allocation", "none");
        let claims_noblock =
            has("noblock") || has("realtime") || envelope_forbids("blocking", "false");
        if claims_noalloc && C_ALLOCATING.contains(&name.as_str()) {
            out.push(finding(
                db,
                file,
                range,
                "extern-assertion",
                format!(
                    "`{name}` is trusted not to allocate, but the C library's `{name}` allocates"
                ),
                Some("remove the assertion; nothing checks it, and callers rely on it".to_string()),
            ));
        }
        if claims_noblock && C_BLOCKING.contains(&name.as_str()) {
            out.push(finding(
                db,
                file,
                range,
                "extern-assertion",
                format!("`{name}` is trusted not to block, but the C library's `{name}` blocks"),
                Some("remove the assertion; nothing checks it, and callers rely on it".to_string()),
            ));
        }

        let is_gc = |ty: &Ty| match ty.interned() {
            TyKind::Struct(s, _) => {
                (s.data(db).memory_kind == StructMemoryKind::Gc).then(|| s.name(db).to_string())
            }
            _ => None,
        };
        for param in function.params(db) {
            if let Some(class) = is_gc(param.ty()) {
                let site = param
                    .source(db)
                    .map_or(range, |p| p.value.syntax().text_range());
                out.push(finding(
                    db,
                    file,
                    site,
                    "extern-gc-reference",
                    format!(
                        "`{name}` passes a `{class}` (a garbage-collected class) to C; C code that \
                         keeps the pointer holds a reference the collector does not see"
                    ),
                    Some(
                        "pass the fields C needs, or a value struct, instead of the class"
                            .to_string(),
                    ),
                ));
            }
        }
        if let Some(class) = is_gc(&function.ret_type(db)) {
            out.push(finding(
                db,
                file,
                range,
                "extern-gc-reference",
                format!(
                    "`{name}` returns a `{class}` (a garbage-collected class) from C; C cannot \
                     allocate one the collector knows about"
                ),
                Some("return a value struct or a handle instead".to_string()),
            ));
        }
    }

    for variant in function.data(db).asm_variants() {
        let arch = variant.arch.as_deref().unwrap_or("every architecture");
        if !variant.pure {
            continue;
        }
        let has_output = variant
            .constraints
            .split(',')
            .any(|c| c.trim_start().starts_with('='));
        if !has_output {
            out.push(finding(
                db,
                file,
                range,
                "asm",
                format!(
                    "the {arch} `@asm` variant of `{name}` is `pure: true` but has no output: the \
                     optimiser may delete every call"
                ),
                Some("drop `pure: true`, or give the assembly an output".to_string()),
            ));
        }
        if variant.constraints.contains("~{memory}") {
            out.push(finding(
                db,
                file,
                range,
                "asm",
                format!(
                    "the {arch} `@asm` variant of `{name}` is `pure: true` but clobbers memory: \
                     merging or dropping calls would lose its writes"
                ),
                Some("drop `pure: true`".to_string()),
            ));
        }
    }
}

fn unproven_division(db: &dyn HirDatabase, function: Function, out: &mut Vec<Finding>) {
    if function.is_extern(db) || !function.data(db).has_body() {
        return;
    }
    let body = function.body(db);
    let infer = function.infer(db);
    for (expr, e) in body.exprs() {
        let Expr::BinaryOp { op, rhs, .. } = e else {
            continue;
        };
        let is_div = matches!(
            op,
            Some(
                BinaryOp::ArithOp(ArithOp::Divide | ArithOp::Remainder)
                    | BinaryOp::Assignment {
                        op: Some(ArithOp::Divide | ArithOp::Remainder)
                    }
            )
        );
        if !is_div || !codira_hir::realtime::division_needs_check(db, &body, &infer, expr) {
            continue;
        }
        let Some((file, range)) = function.expr_source_range(db, expr) else {
            continue;
        };
        let divisor = function
            .expr_source_range(db, *rhs)
            .map(|(f, r)| db.file_text(f)[r].to_string())
            .unwrap_or_else(|| "the divisor".to_string());
        out.push(finding(
            db,
            file,
            range,
            "unproven-division",
            format!(
                "the divisor `{divisor}` in `{}` is not proven non-zero (or, signed, the division \
                 free of MIN / -1): it is checked at run time and panics",
                function.name(db)
            ),
            Some(format!(
                "add `requires {divisor} != 0` (and, for a signed type, a bound that rules out \
                 MIN / -1) to `{}` so the compiler proves it and drops the check",
                function.name(db)
            )),
        ));
    }
}

fn noalloc_candidate(db: &dyn HirDatabase, function: Function, out: &mut Vec<Finding>) {
    if function.is_extern(db) || !function.data(db).has_body() || function.is_test(db) {
        return;
    }
    let src = function.source(db);
    let node = src.value.syntax().clone();
    let public = node
        .children()
        .any(|c| c.kind() == SyntaxKind::VISIBILITY && c.text().to_string().trim() == "public");
    if !public {
        return;
    }
    let attributes = attribute_names(&node);
    let declared = attributes.iter().any(|(n, text)| {
        n == "noalloc"
            || n == "realtime"
            || (n == "envelope" && text.replace(' ', "").contains("allocation:none"))
    });
    if declared || function.is_generic(db) {
        return;
    }
    if db.effect_summary(function).effects.contains(Effect::Alloc) {
        return;
    }
    out.push(finding(
        db,
        src.file_id,
        src.value.signature_range(),
        "noalloc-candidate",
        format!(
            "`{}` never allocates (the effect analysis proves it); declare it so callers can rely \
             on it and the compiler keeps it true",
            function.name(db)
        ),
        Some("add `@noalloc`".to_string()),
    ));
}

/// Attributes that do not make an item reachable from outside the code.
const PASSIVE_ATTRIBUTES: &[&str] = &[
    "noalloc", "nopanic", "noblock", "realtime", "envelope", "inline",
];

fn unused(db: &dyn HirDatabase, out: &mut Vec<Finding>) {
    let files = workspace_files(db);
    // Every name referred to anywhere in the workspace, with the offsets of
    // the references.
    let mut refs: HashMap<String, Vec<(FileId, u32)>> = HashMap::new();
    let roots: Vec<(FileId, SyntaxNode)> = files
        .iter()
        .map(|&f| (f, db.parse(f).syntax_node()))
        .collect();
    for (file, root) in &roots {
        for node in root.descendants() {
            if node.kind() == SyntaxKind::NAME_REF {
                refs.entry(node.text().to_string())
                    .or_default()
                    .push((*file, u32::from(node.text_range().start())));
            }
        }
    }
    for (file, root) in &roots {
        for node in root.descendants() {
            let kind = match node.kind() {
                SyntaxKind::FUNCTION_DEF => "function",
                SyntaxKind::STRUCT_DEF => "type",
                SyntaxKind::ENUM_DEF => "enum",
                SyntaxKind::CONST_DEF => "constant",
                SyntaxKind::TYPE_ALIAS_DEF => "type alias",
                _ => continue,
            };
            // Trait declarations and implementations must have their methods.
            if node.ancestors().skip(1).any(|a| {
                a.kind() == SyntaxKind::TRAIT_DEF
                    || (a.kind() == SyntaxKind::EXTEND
                        && a.children_with_tokens().any(|c| {
                            c.kind() == SyntaxKind::COLON
                                || c.kind() == SyntaxKind::INHERITANCE_LIST
                        }))
                    || a.kind() == SyntaxKind::FUNCTION_DEF
            }) {
                continue;
            }
            let Some(name) = node
                .children()
                .find(|c| c.kind() == SyntaxKind::NAME)
                .map(|n| n.text().to_string())
            else {
                continue;
            };
            let public = node.children().any(|c| c.kind() == SyntaxKind::VISIBILITY);
            if public || name == "main" || name.starts_with('_') {
                continue;
            }
            if attribute_names(&node)
                .iter()
                .any(|(n, _)| !PASSIVE_ATTRIBUTES.contains(&n.as_str()))
            {
                continue;
            }
            let range = node.text_range();
            let used = refs.get(&name).is_some_and(|sites| {
                sites.iter().any(|&(f, at)| {
                    f != *file || !range.contains(codira_syntax::TextSize::from(at))
                })
            });
            if used {
                continue;
            }
            let name_range = node
                .children()
                .find(|c| c.kind() == SyntaxKind::NAME)
                .map_or(range, |n| n.text_range());
            out.push(finding(
                db,
                *file,
                name_range,
                "unused",
                format!("{kind} `{name}` is never used"),
                Some("remove it, or make it `public` if it is part of the API".to_string()),
            ));
        }
    }
}

/// `codira-lint: allow(..)` comments.
struct Directives {
    /// (path, line) -> lints allowed on that line.
    lines: HashMap<(String, u32), Vec<String>>,
    files: HashMap<String, Vec<String>>,
}

impl Directives {
    fn of(db: &dyn HirDatabase) -> Directives {
        let mut lines: HashMap<(String, u32), Vec<String>> = HashMap::new();
        let mut files: HashMap<String, Vec<String>> = HashMap::new();
        for file in workspace_files(db) {
            let path = db.file_relative_path(file).to_string();
            let text = db.file_text(file);
            for (i, line) in text.lines().enumerate() {
                let Some(at) = line.find("codira-lint:") else {
                    continue;
                };
                let comment_start = line.find("//").unwrap_or(usize::MAX);
                if comment_start > at {
                    continue;
                }
                let names = parse_allow(&line[at + "codira-lint:".len()..]);
                if line[comment_start..].starts_with("//!") {
                    files.entry(path.clone()).or_default().extend(names);
                } else {
                    let n = i as u32 + 1;
                    for target in [n, n + 1] {
                        lines
                            .entry((path.clone(), target))
                            .or_default()
                            .extend(names.iter().cloned());
                    }
                }
            }
        }
        Directives { lines, files }
    }

    fn allows(&self, finding: &Finding) -> bool {
        let matches = |names: &Vec<String>| names.iter().any(|n| n == "all" || n == finding.lint);
        self.files.get(&finding.path).is_some_and(matches)
            || self
                .lines
                .get(&(finding.path.clone(), finding.line))
                .is_some_and(matches)
    }
}

/// ` allow(a, b) ...` -> ["a", "b"].
pub(crate) fn parse_allow(text: &str) -> Vec<String> {
    let text = text.trim_start();
    let Some(rest) = text.strip_prefix("allow(") else {
        return Vec::new();
    };
    let Some(end) = rest.find(')') else {
        return Vec::new();
    };
    rest[..end]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_directives() {
        assert_eq!(
            parse_allow(" allow(unused, asm) because"),
            ["unused", "asm"]
        );
        assert_eq!(parse_allow("allow(all)"), ["all"]);
        assert!(parse_allow("deny(unused)").is_empty());
        assert!(parse_allow("allow(unused").is_empty());
    }

    #[test]
    fn every_lint_has_a_description() {
        let names: HashSet<&str> = LINTS.iter().map(|(n, _)| *n).collect();
        assert_eq!(names.len(), LINTS.len());
        assert!(LINTS.iter().all(|(_, d)| !d.is_empty()));
    }
}
