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
//   * `func DType.is_unsigned(self)` is a method of `DType` written the
//     pre-redesign way and must move into an `extend DType { .. }` block, which
//     means finding where the whole declaration ends -- brace matching, not a
//     line count.
//
// So the pass runs over the real token stream from `codira_syntax::tokenize`,
// skipping COMMENT and STRING tokens entirely. That is also why this lives
// in `codira_syntax` rather than in `scripts/`: it needs the lexer.
//
// # What it does not do
//
// It does not invent syntax for constructs the language genuinely lacks
// (function types, closures, variadics -- S9/S10/S11). Those are left alone
// and show up in the `stdlib_gaps` report instead.
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
/// bracket or paren (for a tuple type), a `?`, or a literal -- an integer
/// for a const generic, a string for the Mojo-style parameterisation
/// `std/sys/intrinsics.code` uses. Seeing anything else is the signal that
/// the brackets were an index or a comparison after all.
fn is_type_list_token(kind: SyntaxKind) -> bool {
    kind == IDENT
        || kind == WHITESPACE
        || kind == T_COMMA
        || kind == T_DOT
        || kind == T_L_BRACKET
        || kind == T_R_BRACKET
        || kind == T_L_PAREN
        || kind == T_R_PAREN
        || kind == T_QUESTION
        || kind == T_INT_NUMBER
        || kind == STRING
}

// The lexer's `SyntaxKind`s used above, named locally so the scanning code
// reads as grammar rather than as a wall of imports.
use codira_syntax::SyntaxKind::{
    COLON as T_COLON, COMMA as T_COMMA, DOT as T_DOT, EXCLAMATION as T_EXCL, FUNC_KW as T_FUNC_KW,
    GT as T_GT, INT_NUMBER as T_INT_NUMBER, LT as T_LT, L_BRACKET as T_L_BRACKET,
    L_CURLY as T_L_CURLY, L_PAREN as T_L_PAREN, MINUS as T_MINUS, QUESTION as T_QUESTION,
    R_BRACKET as T_R_BRACKET, R_CURLY as T_R_CURLY, R_PAREN as T_R_PAREN, SEMI as T_SEMI,
};

/// Rewrites `func T.m(..)` -- a method of `T` written the pre-redesign way
/// -- into an `extend T { .. }` block, which
/// `spec/LANGUAGE_SPEC.md` section 12 states is the only way methods are
/// declared:
///
/// ```text
/// public func DType.is_unsigned(self) -> bool { .. }
/// ```
///
/// becomes, in place:
///
/// ```text
/// extend DType {
///     public func is_unsigned(self) -> bool { .. }
/// }
/// ```
///
/// This spelling is safe to convert because it is *already* a method
/// everywhere that matters: its body and its callers use `receiver.m(..)`,
/// so promoting the declaration changes no call site.
///
/// The other pre-redesign spelling, `func m(self: T, ..)`, is deliberately
/// **not** converted -- see [`rewrite_ascribed_self_params`].
///
/// Each declaration is wrapped in its *own* `extend`, rather than the
/// several for one type being gathered into a single block. That keeps the
/// rewrite local -- no declaration moves, so nothing can be reordered past a
/// comment or a `const` it depended on -- and costs nothing semantically:
/// `InherentImpls` collects a type's methods across every impl that names
/// it, exactly as Rust and Swift do.
///
/// Returns the number of declarations rewritten.
fn rewrite_qualified_methods(text: &str, toks: &[Spanned], edits: &mut Vec<Edit>) -> usize {
    let mut rewritten = 0;

    for i in 0..toks.len() {
        if toks[i].kind != T_FUNC_KW {
            continue;
        }

        // Where the declaration starts: before any `public`/`internal`, so
        // the visibility keyword ends up inside the `extend` block with its
        // function rather than stranded outside it.
        let decl_start = preceding_visibility(text, toks, i).unwrap_or(toks[i].start);

        let after_func = skip_ws(toks, i + 1);
        let Some((owner, name_start)) = method_owner(text, toks, after_func) else {
            continue;
        };

        let Some(decl_end) = declaration_end(toks, after_func) else {
            continue;
        };

        let _ = name_start;

        // Preserve the declaration's own indentation on the `extend` line so
        // the rewritten source still reads as the surrounding file does, and
        // indent the declaration one level inside the new block. The
        // declaration is rewritten as a whole rather than bracketed in place
        // so the body lines move with it -- a method that is textually
        // outdented from the `extend` that now owns it reads as a bug.
        let indent: String = text[..decl_start]
            .chars()
            .rev()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();

        // Drop the `T.` qualifier, or the `: T` ascription on `self`, then
        // re-indent every line of what is left.
        let mut decl = String::with_capacity(decl_end - decl_start);
        decl.push_str(&text[decl_start..owner.span.0]);
        decl.push_str(&text[owner.span.1..decl_end]);

        let body: String = decl
            .lines()
            .map(|line| {
                if line.trim().is_empty() {
                    line.to_string()
                } else {
                    format!("    {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        edits.push(Edit {
            start: decl_start,
            end: decl_end,
            text: format!("extend {owner} {{\n{indent}{body}\n{indent}}}"),
            rule: "qualified-method",
        });

        rewritten += 1;
    }

    rewritten
}

/// The type a pre-redesign method declaration belongs to, plus the byte span
/// that has to be deleted to turn the declaration into a plain function.
struct MethodOwner {
    name: String,
    /// The `T.` qualifier, or the `: T` ascription -- whichever spelling was
    /// used, this is the text that must go.
    span: (usize, usize),
}

impl std::fmt::Display for MethodOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// Recognises a `func T.m(..)` / `func T[A, B].m(..)` qualifier, if
/// `toks[at]` (the token just past `func`) begins one.
///
/// Returns the owning type and the span of the `T.` qualifier, which has to
/// be deleted to leave a plain function declaration behind.
fn method_owner(text: &str, toks: &[Spanned], at: usize) -> Option<(MethodOwner, usize)> {
    if at >= toks.len() || toks[at].kind != IDENT {
        return None;
    }

    // Skip a `[A, B]` generic argument list on the qualifier, so
    // `func SIMD[T, N].splat(..)` is recognised as a method of `SIMD[T, N]`.
    let mut j = at + 1;
    if j < toks.len() && toks[j].kind == T_L_BRACKET {
        let mut depth = 1usize;
        j += 1;
        while j < toks.len() && depth > 0 {
            match toks[j].kind {
                T_L_BRACKET => depth += 1,
                T_R_BRACKET => depth -= 1,
                _ => {}
            }
            j += 1;
        }
    }

    if j >= toks.len() || toks[j].kind != T_DOT {
        return None;
    }

    let owner = text[toks[at].start..toks[j].start].trim().to_string();
    let name_start = skip_ws(toks, j + 1);
    Some((
        MethodOwner {
            name: owner,
            span: (toks[at].start, toks[j].end),
        },
        name_start,
    ))
}

/// Drops explicit generic arguments at ordinary call sites:
/// `dict_with_capacity[K, V](16)` becomes `dict_with_capacity(16)`.
///
/// `spec/LANGUAGE_SPEC.md` section 3 is explicit that these are not part of
/// the language: "explicit generic arguments are only accepted in **type
/// position** and in static member access (`Box[i32].new(5)`); at ordinary
/// call sites, type arguments are always inferred from the arguments -- this
/// keeps expression grammar unambiguous without needing a turbofish-style
/// escape hatch."
///
/// That rule is what makes `foo[Bar](x)` unambiguous, and it is worth
/// keeping: the alternative is inventing a turbofish the language has
/// deliberately avoided. So the call sites are rewritten rather than the
/// grammar relaxed.
///
/// Record literals (`Deque[T] { .. }`) are *not* touched -- those are
/// unambiguous and the parser accepts them; see
/// `expressions::at_record_lit_generic_args`. Neither is static member
/// access (`Box[i32].new(5)`), which the spec names as allowed. The rule
/// therefore only fires on `ident [ .. ] (`.
///
/// Where a type argument cannot be inferred from the arguments -- most of
/// these calls take none, as `zero_value[T]()` does -- it has to come from
/// the expected type instead. That is bidirectional inference doing its
/// job, and it is the same mechanism a tuple literal already relies on.
fn rewrite_call_site_generic_args(text: &str, toks: &[Spanned], edits: &mut Vec<Edit>) {
    for i in 0..toks.len() {
        // The callee must be a bare identifier immediately followed by `[`.
        if toks[i].kind != IDENT {
            continue;
        }

        // ...and must not be a *declaration's* name. `func dict_new[K, V](..)`
        // has exactly the shape this rule looks for, and stripping its `[K, V]`
        // would delete the parameter list rather than an argument list --
        // turning a generic function into one referring to undefined types.
        if preceding_keyword(text, toks, i).is_some_and(|kw| {
            matches!(
                kw,
                "func" | "def" | "struct" | "class" | "enum" | "trait" | "type" | "extend"
            )
        }) {
            continue;
        }
        let open = i + 1;
        if open >= toks.len() || toks[open].kind != T_L_BRACKET {
            continue;
        }
        if toks[i].end != toks[open].start {
            // `arr [i]` with a space is still indexing; require the tight
            // spelling a call site uses.
            continue;
        }

        // Find the matching `]`, bailing on anything that is not a type
        // list -- the same contents check the angle-bracket rule uses.
        //
        // Consecutive groups are all consumed:
        // `llvm_intrinsic["name"][SIMD[u32, 4]](ptr)` -- Mojo-style
        // parameterisation -- carries two, and stripping only the first
        // would leave the second as an index on the call's result.
        let mut close = None;
        let mut group_open = open;
        loop {
            let mut depth = 1usize;
            let mut j = group_open + 1;
            let mut group_close = None;
            while j < toks.len() {
                match toks[j].kind {
                    T_L_BRACKET => depth += 1,
                    T_R_BRACKET => {
                        depth -= 1;
                        if depth == 0 {
                            group_close = Some(j);
                            break;
                        }
                    }
                    k if is_type_list_token(k) => {}
                    _ => break,
                }
                j += 1;
            }
            let Some(group_close) = group_close else {
                break;
            };
            close = Some(group_close);

            let next = skip_ws(toks, group_close + 1);
            if next < toks.len() && toks[next].kind == T_L_BRACKET {
                group_open = next;
                continue;
            }
            break;
        }
        let Some(close) = close else { continue };

        // A call, or static member access.
        //
        // `{` is excluded: `Deque[T] { .. }` is a record literal, which the
        // parser accepts with its arguments because nothing else in the
        // grammar is `path [ .. ] {`.
        //
        // `.` is *included*, despite section 3 listing `Box[i32].new(5)` as
        // allowed, because that spelling is not parseable: it is the same
        // shape as `buf[i].abs()`, so the parser cannot accept one without
        // silently reinterpreting the other. See
        // `expressions::generic_args_terminator`.
        let next = skip_ws(toks, close + 1);
        if next >= toks.len() || !matches!(toks[next].kind, T_L_PAREN | T_DOT) {
            continue;
        }

        edits.push(Edit {
            start: toks[open].start,
            end: toks[close].end,
            text: String::new(),
            rule: "call-site-generic-args",
        });
    }
}

/// Renames the receiver of a top-level `func m(self: T, ..)` from `self` to
/// `this`, throughout that declaration.
///
/// This spelling has no production in the current grammar: `self` with an
/// explicit type ascription is only legal in method-receiver position, and
/// these functions are not methods -- they are free functions that happen to
/// name their first parameter `self`, and they are *called* that way:
/// `std/collections/bitset.code` has `get(self, i)` and `count(self)`, not
/// `self.get(i)`.
///
/// So the meaning-preserving fix is to rename the parameter, not to promote
/// the function into an `extend` block. Promoting it would make every one of
/// those call sites wrong while leaving the file parsing cleanly -- a green
/// ratchet hiding a broken stdlib, which is worse than the parse error it
/// replaced. `std/collections/optional.code` already took exactly this route
/// by hand during its own migration (its functions take `opt`, not `self`).
///
/// Promoting these to real methods is a genuine improvement now that methods
/// work end to end, but it is a call-site rewrite and belongs in its own
/// pass, not smuggled into a syntax migration.
fn rewrite_ascribed_self_params(text: &str, toks: &[Spanned], edits: &mut Vec<Edit>) -> usize {
    let mut renamed = 0;

    for i in 0..toks.len() {
        if toks[i].kind != T_FUNC_KW {
            continue;
        }

        let after_func = skip_ws(toks, i + 1);
        // A qualified method (`func T.m`) is handled by the other rule.
        if method_owner(text, toks, after_func).is_some() {
            continue;
        }

        // Skip a generic parameter list: `func heap_grow[T](self: ..)` is
        // the same shape as `func capacity(self: ..)` once `[T]` is out of
        // the way, and the generic form is most of what `std/collections`
        // is written in.
        let mut cursor = after_func + 1;
        if cursor < toks.len() && toks[cursor].kind == T_L_BRACKET {
            let mut depth = 1usize;
            cursor += 1;
            while cursor < toks.len() && depth > 0 {
                match toks[cursor].kind {
                    T_L_BRACKET => depth += 1,
                    T_R_BRACKET => depth -= 1,
                    _ => {}
                }
                cursor += 1;
            }
        }

        let l_paren = skip_ws(toks, cursor);
        if l_paren >= toks.len() || toks[l_paren].kind != T_L_PAREN {
            continue;
        }
        let self_tok = skip_ws(toks, l_paren + 1);
        if self_tok >= toks.len() || &text[toks[self_tok].start..toks[self_tok].end] != "self" {
            continue;
        }
        let colon = skip_ws(toks, self_tok + 1);
        if colon >= toks.len() || toks[colon].kind != T_COLON {
            continue;
        }

        let Some(decl_end) = declaration_end(toks, after_func) else {
            continue;
        };

        // Rename every `self` in the declaration, parameter and body alike,
        // so `get(self, i)` inside one of these bodies keeps referring to the
        // same binding.
        for tok in toks {
            if tok.start < toks[self_tok].start || tok.end > decl_end {
                continue;
            }
            // Matched on text, not on kind: `self` lexes as its own keyword
            // token, not as an IDENT, so a kind check finds nothing at all.
            if &text[tok.start..tok.end] == "self" && !is_opaque(tok.kind) {
                edits.push(Edit {
                    start: tok.start,
                    end: tok.end,
                    text: "this".to_string(),
                    rule: "ascribed-self-param",
                });
                renamed += 1;
            }
        }
    }

    renamed
}

/// The byte offset just past the end of the declaration starting at `at`.
///
/// A declaration ends either at the `;` of a body-less `extern` signature or
/// at the `}` closing its body, found by brace matching -- which is why this
/// works on tokens rather than on lines.
fn declaration_end(toks: &[Spanned], at: usize) -> Option<usize> {
    let mut i = at;
    while i < toks.len() {
        match toks[i].kind {
            T_SEMI => return Some(toks[i].end),
            T_L_CURLY => {
                let mut depth = 1usize;
                i += 1;
                while i < toks.len() {
                    match toks[i].kind {
                        T_L_CURLY => depth += 1,
                        T_R_CURLY => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(toks[i].end);
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                return None;
            }
            _ => i += 1,
        }
    }
    None
}

/// The text of the nearest non-whitespace token before `idx`, if any.
fn preceding_keyword<'a>(text: &'a str, toks: &[Spanned], idx: usize) -> Option<&'a str> {
    let mut j = idx;
    while j > 0 {
        j -= 1;
        if toks[j].kind == WHITESPACE {
            continue;
        }
        return Some(&text[toks[j].start..toks[j].end]);
    }
    None
}

/// The start offset of a `public`/`internal` keyword immediately preceding
/// the `func` at `func_idx`, if there is one.
fn preceding_visibility(text: &str, toks: &[Spanned], func_idx: usize) -> Option<usize> {
    let mut j = func_idx;
    while j > 0 {
        j -= 1;
        if toks[j].kind == WHITESPACE {
            continue;
        }
        let word = &text[toks[j].start..toks[j].end];
        return (word == "public" || word == "internal").then_some(toks[j].start);
    }
    None
}

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
    } else {
        // A directory argument means "every `.code` file under here", which
        // is what anyone passing one expects and what makes it possible to
        // rehearse the whole pass against a copy of `std/` before touching
        // the real tree.
        let mut expanded = Vec::new();
        for path in paths {
            if path.is_dir() {
                collect_code_files(&path, &mut expanded);
            } else {
                expanded.push(path);
            }
        }
        paths = expanded;
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
        rewrite_qualified_methods(&text, &code, &mut edits);
        rewrite_call_site_generic_args(&text, &code, &mut edits);
        rewrite_ascribed_self_params(&text, &code, &mut edits);

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
