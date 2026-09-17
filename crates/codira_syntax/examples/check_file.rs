// Parse-only checker for `.code` files: `cargo run -p codira_syntax --example
// check_file -- path/to/file.code [more files...]`. Prints OK/FAIL per file
// and every SyntaxError with its line:col and the offending source line;
// exits 1 if any file failed.
//
// Written to verify std/**/*.code syntax migrations against the current
// grammar (see spec/KGEN_SUPERSET_STATUS.md) without needing the LLVM
// toolchain codira_codegen/codira_compiler require -- codira_syntax has no
// such dependency, so this works in any environment that can build Rust.
// Kept as a standing dev tool for the rest of the std/ migration, not a
// one-off script.
//
// Flags:
//   --limit N   show at most N errors per file (default 10; 0 = all)
//   --first     show only the first error per file -- the one that matters,
//               since a recursive-descent parser's later errors are usually
//               cascade noise from the first desync.
use std::{env, fmt::Write as _, fs};

use codira_syntax::SourceFile;

/// Byte offset -> (1-based line, 1-based column, that line's text).
fn locate(text: &str, offset: usize) -> (usize, usize, &str) {
    let offset = offset.min(text.len());
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |i| line_start + i);
    let line_no = text[..line_start].bytes().filter(|&b| b == b'\n').count() + 1;
    let col = text[line_start..offset].chars().count() + 1;
    (line_no, col, text[line_start..line_end].trim_end())
}

fn main() {
    let mut limit = 10usize;
    let mut paths = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--first" => limit = 1,
            "--limit" => {
                limit = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--limit takes a number");
            }
            _ => paths.push(arg),
        }
    }

    let mut any_errors = false;
    for path in paths {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("FAIL {path}: {err}");
                any_errors = true;
                continue;
            }
        };
        let parse = SourceFile::parse(&text);
        let errors = parse.errors();
        if errors.is_empty() {
            println!("OK   {path}");
            continue;
        }

        any_errors = true;
        println!("FAIL {path}  ({} errors)", errors.len());
        let shown = if limit == 0 { errors.len() } else { limit };
        let mut out = String::new();
        for err in errors.iter().take(shown) {
            let offset: u32 = err.location().offset().into();
            let (line, col, src) = locate(&text, offset as usize);
            let _ = writeln!(out, "     {path}:{line}:{col}: {err}");
            let _ = writeln!(out, "       | {src}");
        }
        if errors.len() > shown {
            let _ = writeln!(out, "     ... {} more", errors.len() - shown);
        }
        print!("{out}");
    }

    if any_errors {
        std::process::exit(1);
    }
}
