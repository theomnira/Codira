// Aggregates `std/**/*.code` parse failures into a ranked gap list:
// `cargo run -p codira_syntax --example stdlib_gaps`.
//
// The per-file ratchet (tests/stdlib_ratchet.rs) answers "did anything
// regress"; this answers the complementary question "what should I build
// next", by bucketing every first-error-per-file (the only non-cascade
// error a recursive-descent parser produces) by message and showing a
// representative source line for each bucket.
//
// Only the FIRST error of each file is counted, deliberately: once the
// parser desyncs, the remaining hundreds of errors describe the recovery
// path, not the language gap, and counting them buries the real signal
// under whichever file happens to be longest.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use codira_syntax::SourceFile;

fn collect_code_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
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

fn line_at(text: &str, offset: usize) -> (usize, &str) {
    let offset = offset.min(text.len());
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let line_no = text[..start].bytes().filter(|&b| b == b'\n').count() + 1;
    (line_no, text[start..end].trim())
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/codira_syntax -> repo root")
        .to_path_buf();

    let mut files = Vec::new();
    collect_code_files(&root.join("std"), &mut files);

    // message -> (file:line, source line) samples
    let mut buckets: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut ok = 0usize;

    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let parse = SourceFile::parse(&text);
        let Some(first) = parse.errors().first() else {
            ok += 1;
            continue;
        };
        let offset: u32 = first.location().offset().into();
        let (line_no, src) = line_at(&text, offset as usize);
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        buckets
            .entry(first.to_string())
            .or_default()
            .push((format!("{rel}:{line_no}"), src.to_string()));
    }

    println!("{ok} of {} std files parse\n", files.len());

    let mut ranked: Vec<_> = buckets.into_iter().collect();
    ranked.sort_by_key(|(_, hits)| std::cmp::Reverse(hits.len()));

    for (message, hits) in ranked {
        println!("{:3} x  {message}", hits.len());
        for (loc, src) in hits.iter().take(4) {
            println!("        {loc}");
            println!("          | {src}");
        }
        if hits.len() > 4 {
            println!("        ... {} more files", hits.len() - 4);
        }
        println!();
    }
}
