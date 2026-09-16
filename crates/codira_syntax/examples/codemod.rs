// The S2 codemod (RFC-002 section 3.1): mechanically rewrites pre-redesign
// spellings in `.code` sources to the current grammar.
//
//   cargo run -p codira_syntax --example codemod -- [--write] [paths...]
//
// With no paths it walks `std/`. Without `--write` it only reports what it
// would change, so the diff can be inspected before anything is touched.
//
// # Why a tool and not sed
//
// Every rewrite here is context-sensitive in a way a line regex gets wrong:
//
//   * `Array<u8>` is a generic instantiation and must become `Array[u8]`, but
//     `a < b` and `x >> 1` must not be touched. The difference is not lexical
//     -- it needs balanced-delimiter matching plus a check that the contents
//     actually look like a type list.
//   * `Array::new()` must become `Array.new()`, but `std::array` inside a doc
//     comment is prose about C++ and must be left alone.
//
// So the pass runs over the real token stream from `codira_syntax::tokenize`,
// skipping COMMENT and STRING tokens entirely. That is also why this lives
// in `codira_syntax` rather than in `scripts/`: it needs the lexer.
//
// # What it does not do
//
// It does not rewrite `func Type.method(...)` into an `extend` block (that
// is S7, and it needs to *group* declarations, not rewrite them in place),
// and it does not invent syntax for constructs the language genuinely lacks
// (function types, closures -- S10/S11). Those are reported, not rewritten.
use std::{
    fs,
    path::{Path, PathBuf},
};

use codira_syntax::{
    tokenize,
    SyntaxKind::{self, COMMENT, IDENT, STRING, WHITESPACE},
};

/// One token plus where it sits in the source text.
struct Spanned {
    kind: SyntaxKind,
    start: usize,
    end: usize,
}

fn spanned_tokens(text: &str) -> Vec<Spanned> {
    let mut offset = 0usize;
    tokenize(text)
        .into_iter()
        .map(|t| {
            let len: u32 = t.len.into();
            let start = offset;
            offset += len as usize;
            Spanned {
                kind: t.kind,
                start,
                end: offset,
            }
        })
        .collect()
}

/// True for tokens whose text is prose or literal data, never code.
fn is_opaque(kind: SyntaxKind) -> bool {
    matches!(kind, COMMENT | STRING)
}

/// Index of the next token at or after `i` that is not whitespace.
fn skip_ws(toks: &[Spanned], mut i: usize) -> usize {
    while i < toks.len() && toks[i].kind == WHITESPACE {
        i += 1;
    }
    i
}

/// A single text replacement, recorded as a byte range plus its new text.
struct Edit {
    start: usize,
    end: usize,
    text: String,
    /// Which rule produced this, for the report.
    rule: &'static str,
}

/// Rewrites `Name<A, B>` to `Name[A, B]` in code positions.
///
/// The `<` only opens a generic argument list when it *immediately* follows
/// an identifier with no space (`Array<u8>`, never `a < b`), and the span up
/// to the matching `>` must contain only things that can appear in a type
/// list. Anything else -- an operator, a literal, a call -- means this was
/// a comparison and is left untouched.
///
/// Nesting is handled by counting depth, so `Array<Optional<T>>` rewrites
/// both levels: the outer loop visits each `<` independently and emits its
/// own bracket pair.
///
/// Working on *raw* tokens is what makes the depth count simple. The lexer
/// emits punctuation one character at a time (`next_token_inner` falls
/// through to `SyntaxKind::from_char`) and only the parser glues `>>` into a
/// single shift token, so here `Array<Optional<T>>` ends in two separate
/// `GT`s and each closes exactly one level.
fn rewrite_angle_generics(toks: &[Spanned], edits: &mut Vec<Edit>) {
    for i in 0..toks.len() {
        if toks[i].kind != T_LT || i == 0 {
            continue;
        }
        // Must directly follow an identifier: `Array<`, not `Array <` and
        // not `) <`.
        let prev = &toks[i - 1];
        if prev.kind != IDENT || prev.end != toks[i].start {
            continue;
        }

        // Scan for the matching `>`, tracking depth and validating contents.
        let mut depth = 1usize;
        let mut j = i + 1;
        let mut close: Option<usize> = None;
        while j < toks.len() {
            let k = toks[j].kind;
            if is_opaque(k) {
                break;
            }
            if k == T_LT {
                depth += 1;
            } else if k == T_GT {
                depth -= 1;
                if depth == 0 {
                    close = Some(j);
                    break;
                }
            } else if !is_type_list_token(k) {
                break;
            }
            j += 1;
        }

        let Some(close) = close else { continue };

        edits.push(Edit {
            start: toks[i].start,
            end: toks[i].end,
            text: "[".to_string(),
            rule: "angle-generics",
        });
        edits.push(Edit {
            start: toks[close].start,
            end: toks[close].end,
            text: "]".to_string(),
            rule: "angle-generics",
        });
    }
}

/// Rewrites `Type::method` to `Type.method` in code positions.
///
/// `.` is the language's single access operator for modules, types and
/// values (`LANGUAGE_SPEC` section 1), so this is an unconditional rewrite --
/// but only outside comments, where `std::array` and `codira_syntax::parse`
/// are prose that must survive verbatim.
///
/// The grammar has no `::` token: `::` lexes as two adjacent `COLON`s, so
/// the rule matches the pair and requires them to be *touching*, which is
/// what keeps a type ascription followed by a labelled thing (`a: b`, `c:
/// d`) from ever looking like a path separator.
fn rewrite_colon_colon(toks: &[Spanned], edits: &mut Vec<Edit>) {
    for i in 0..toks.len().saturating_sub(1) {
        if toks[i].kind == T_COLON
            && toks[i + 1].kind == T_COLON
            && toks[i].end == toks[i + 1].start
        {
            edits.push(Edit {
                start: toks[i].start,
                end: toks[i + 1].end,
                text: ".".to_string(),
                rule: "path-separator",
            });
        }
    }
}

/// Rewrites `-> !` to `-> Never`.
///
/// `!` as a return type is a pre-redesign spelling; the current grammar
/// names the empty type `Never` (`LANGUAGE_SPEC` section 6 uses it directly
/// in `func fail(message: String) -> Never`). Only the `-> !` sequence is
/// rewritten -- a bare `!` is still prefix negation, and a postfix `!` is
/// still force-unwrap.
///
/// `->` is matched as adjacent `MINUS` `GT`, not as `THIN_ARROW`: the
/// arrow is glued together by the parser, and this pass reads raw lexer
/// tokens (see `rewrite_angle_generics`).
fn rewrite_never_return(toks: &[Spanned], edits: &mut Vec<Edit>) {
    for i in 0..toks.len().saturating_sub(1) {
        let is_arrow =
            toks[i].kind == T_MINUS && toks[i + 1].kind == T_GT && toks[i].end == toks[i + 1].start;
        if !is_arrow {
            continue;
        }
        let j = skip_ws(toks, i + 2);
        if j < toks.len() && toks[j].kind == T_EXCL {
            edits.push(Edit {
                start: toks[j].start,
                end: toks[j].end,
                text: "Never".to_string(),
                rule: "never-return",
            });
        }
    }
}

/// Tokens that may legitimately appear inside a generic argument list.
///
/// Deliberately narrow: an identifier, a path dot, a comma, a nested
/// bracket, or an integer (for const generics like `InlineArray<T, N>`
/// written with a literal). Seeing anything else is the signal that the
/// `<` was a comparison after all.
fn is_type_list_token(kind: SyntaxKind) -> bool {
    kind == IDENT
        || kind == WHITESPACE
        || kind == T_COMMA
        || kind == T_DOT
        || kind == T_L_BRACKET
        || kind == T_R_BRACKET
        || kind == T_QUESTION
        || kind == T_INT_NUMBER
}

// The lexer's `SyntaxKind`s used above, named locally so the scanning code
// reads as grammar rather than as a wall of imports.
use codira_syntax::SyntaxKind::{
    COLON as T_COLON, COMMA as T_COMMA, DOT as T_DOT, EXCLAMATION as T_EXCL, GT as T_GT,
    INT_NUMBER as T_INT_NUMBER, LT as T_LT, L_BRACKET as T_L_BRACKET, MINUS as T_MINUS,
    QUESTION as T_QUESTION, R_BRACKET as T_R_BRACKET,
};

/// Applies `edits` to `text`, rightmost first so earlier offsets stay valid.
fn apply(text: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut out = text.to_string();
    for edit in edits {
        out.replace_range(edit.start..edit.end, &edit.text);
    }
    out
}

fn collect_code_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_code_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "code") {
            out.push(path);
        }
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/codira_syntax -> repo root")
        .to_path_buf()
}

fn main() {
    let mut write = false;
    let mut paths: Vec<PathBuf> = Vec::new();
    for arg in std::env::args().skip(1) {
        if arg == "--write" {
            write = true;
        } else {
            paths.push(PathBuf::from(arg));
        }
    }

    let root = repo_root();
    if paths.is_empty() {
        collect_code_files(&root.join("std"), &mut paths);
    }

    let mut total = 0usize;
    let mut touched = 0usize;
    for path in &paths {
        let Ok(text) = fs::read_to_string(path) else {
            eprintln!("skip (unreadable) {}", path.display());
            continue;
        };

        let toks = spanned_tokens(&text);
        // Code-only view: comments and strings are excluded outright, so no
        // rule can reach into prose.
        let code: Vec<Spanned> = toks
            .into_iter()
            .filter(|t| !is_opaque(t.kind))
            .collect::<Vec<_>>();

        let mut edits = Vec::new();
        rewrite_angle_generics(&code, &mut edits);
        rewrite_colon_colon(&code, &mut edits);
        rewrite_never_return(&code, &mut edits);

        if edits.is_empty() {
            continue;
        }

        let mut by_rule = std::collections::BTreeMap::<&str, usize>::new();
        for edit in &edits {
            *by_rule.entry(edit.rule).or_default() += 1;
        }
        let summary: Vec<String> = by_rule
            .iter()
            .map(|(rule, n)| format!("{rule}={n}"))
            .collect();

        let rel = path.strip_prefix(&root).unwrap_or(path);
        println!(
            "{} {}  [{}]",
            if write { "rewrote" } else { "would rewrite" },
            rel.display().to_string().replace('\\', "/"),
            summary.join(" ")
        );

        total += edits.len();
        touched += 1;

        if write {
            let new_text = apply(&text, edits);
            fs::write(path, new_text).expect("writable source file");
        }
    }

    println!(
        "\n{} edits across {} of {} files{}",
        total,
        touched,
        paths.len(),
        if write {
            ""
        } else {
            " (dry run; pass --write)"
        }
    );
}
