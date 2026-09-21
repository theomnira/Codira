//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: September 20, 2026
//!
//! Functionality:
//! - `codira check`: run the frontend over sources and report diagnostics,
//!   without producing any artifact.

use std::{io::stderr, path::PathBuf};

use codira_compiler::{Config, DiagnosticCounts, DiagnosticKind, DisplayColor, Driver, Target};

use crate::{ops::build::find_manifest, ExitStatus};

#[derive(clap::Args)]
pub struct Args {
    /// Directory of Codira sources to check. Defaults to the enclosing
    /// project's source directory.
    #[clap(long)]
    path: Option<PathBuf>,

    /// Path to the manifest of the project
    #[clap(long, conflicts_with = "path")]
    manifest_path: Option<PathBuf>,

    /// Print one `path: n syntax, n semantic` line per file instead of only
    /// the failures.
    #[clap(long)]
    summary: bool,

    /// Rank diagnostics by message shape, most frequent first, with an
    /// example site for each. This is the "what should I fix next" view.
    #[clap(long)]
    gaps: bool,

    /// With `--gaps`, how many buckets to show.
    #[clap(long, default_value_t = 25)]
    gaps_limit: usize,

    /// Use color in output
    #[clap(long, value_enum)]
    color: Option<super::build::UseColor>,
}

/// Type-checks sources and reports what the compiler could not accept.
///
/// `build` answers "can this become a binary", which on a codebase with any
/// gap left in it is a question that stops at the first wall. `check`
/// answers "what does the compiler still not understand", across every file,
/// which is the question you need answered while the compiler is being
/// finished.
pub fn check(args: Args) -> Result<ExitStatus, anyhow::Error> {
    let display_colors = args.color.map_or(DisplayColor::Auto, |clr| match clr {
        super::build::UseColor::Disable => DisplayColor::Disable,
        super::build::UseColor::Enable => DisplayColor::Enable,
        super::build::UseColor::Auto => DisplayColor::Auto,
    });

    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: codira_compiler::OptimizationLevel::None,
        out_dir: None,
        emit_ir: false,
    };

    let (driver, what) = if let Some(path) = args.path {
        let label = path.display().to_string();
        (Driver::with_source_directory(&path, config)?, label)
    } else {
        let manifest_path = if let Some(path) = &args.manifest_path {
            std::fs::canonicalize(path).map_err(|_error| {
                anyhow::anyhow!(
                    "'{}' does not refer to a valid manifest path",
                    path.display()
                )
            })?
        } else {
            let current_dir =
                std::env::current_dir().expect("could not determine current working directory");
            find_manifest(&current_dir).ok_or_else(|| {
                anyhow::anyhow!(
                    "could not find codira.toml in '{}' or a parent directory; \
                     pass --path to check a bare source directory",
                    current_dir.display()
                )
            })?
        };
        let (package, driver) = Driver::with_package_path(&manifest_path, config)?;
        (driver, package.name().to_string())
    };

    // Exactly one walk of every module's diagnostics, however the result is
    // reported. `collect_diagnostics` and `emit_diagnostics` each perform
    // that whole walk independently -- `infer()` itself is salsa-memoized,
    // but the validators that turn its result into diagnostics are not, so
    // calling both here used to mean doing it twice. Measured on this
    // repository's own standard library (77 files, ~3000 diagnostics), that
    // second pass was the single largest phase in this command, ahead of
    // type inference itself.
    //
    // `--gaps` and `--summary` both want the diagnostic *text* -- to bucket
    // by message shape, or to print per-file counts -- so they need the
    // structured pass. Neither wants annotated source snippets, so this is
    // also a behavior change for `--summary`: it no longer additionally
    // renders every diagnostic to the terminal, matching what `--gaps`
    // already did. The plain default path keeps the rendered snippets, and
    // gets its summary counts from the same pass that produces them instead
    // of a second one.
    let (per_file, counts) = if args.gaps || args.summary {
        let per_file = driver.collect_diagnostics();
        let counts = DiagnosticCounts {
            total_files: per_file.len(),
            clean_files: per_file.iter().filter(|f| f.is_clean()).count(),
            syntax: per_file
                .iter()
                .map(|f| f.count_of(DiagnosticKind::Syntax))
                .sum(),
            semantic: per_file
                .iter()
                .map(|f| f.count_of(DiagnosticKind::Semantic))
                .sum(),
        };
        (per_file, counts)
    } else {
        let counts = driver.emit_diagnostics_with_counts(&mut stderr(), display_colors)?;
        (Vec::new(), counts)
    };

    let DiagnosticCounts {
        total_files,
        clean_files,
        syntax,
        semantic,
    } = counts;

    if args.gaps {
        // Two messages that differ only in which identifier they name are
        // the same missing feature reported twice, so the identifier is
        // what has to go before counting. Without this the ranking is just
        // an alphabetical list of every name in the standard library.
        let mut buckets: std::collections::HashMap<String, (usize, String)> =
            std::collections::HashMap::new();

        for file in &per_file {
            for diagnostic in &file.diagnostics {
                let shape = generalize_message(&diagnostic.message);
                let entry = buckets.entry(shape).or_insert_with(|| {
                    (
                        0,
                        format!(
                            "{}:{}: {}",
                            file.relative_path, diagnostic.line, diagnostic.message
                        ),
                    )
                });
                entry.0 += 1;
            }
        }

        let mut ranked: Vec<_> = buckets.into_iter().collect();
        ranked.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(&b.0)));

        let shown = ranked.len().min(args.gaps_limit);
        println!(
            "{} distinct diagnostic shapes; showing {shown}\n",
            ranked.len()
        );
        for (shape, (count, example)) in ranked.iter().take(shown) {
            println!("{count:>6}  {shape}");
            println!("        e.g. {example}");
        }
        println!();
    }

    if args.summary {
        for file in &per_file {
            println!(
                "{}: {} syntax, {} semantic",
                file.relative_path,
                file.count_of(DiagnosticKind::Syntax),
                file.count_of(DiagnosticKind::Semantic),
            );
        }
        println!();
    }

    println!(
        "checked {what}: {clean_files} of {total_files} files clean ({syntax} syntax, {semantic} semantic)"
    );

    Ok(ExitStatus::from(syntax == 0 && semantic == 0))
}

/// Replaces the variable parts of a diagnostic message with `_`, so that
/// messages describing the same gap collapse into one bucket.
///
/// Compiler messages quote the offending name, and it is exactly that name
/// which makes every occurrence look unique. Backtick- and quote-delimited
/// runs are the convention this compiler already uses for it, so removing
/// them is enough; anything left that still varies is a message worth
/// rewording rather than a case worth special-handling here.
fn generalize_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut delimiter: Option<char> = None;

    for ch in message.chars() {
        match delimiter {
            Some(open) => {
                if ch == closing_for(open) {
                    delimiter = None;
                    out.push('_');
                }
            }
            None => {
                if matches!(ch, '`' | '\'' | '"') {
                    delimiter = Some(ch);
                } else {
                    out.push(ch);
                }
            }
        }
    }

    // An unterminated quote means the message was not in the expected shape;
    // returning it unchanged is better than returning a truncated prefix.
    if delimiter.is_some() {
        return message.to_string();
    }
    out
}

/// The character that closes a quoting run opened by `open`.
fn closing_for(open: char) -> char {
    open
}

#[cfg(test)]
mod test {
    use super::generalize_message;

    #[test]
    fn generalizing_collapses_quoted_names() {
        assert_eq!(
            generalize_message("cannot resolve type `Foo`"),
            generalize_message("cannot resolve type `Bar`"),
        );
        assert_eq!(
            generalize_message("cannot resolve type `Foo`"),
            "cannot resolve type _"
        );
    }

    #[test]
    fn generalizing_keeps_messages_without_names_intact() {
        assert_eq!(
            generalize_message("expected a function"),
            "expected a function"
        );
    }

    #[test]
    fn generalizing_leaves_an_unterminated_quote_alone() {
        // Truncating here would silently merge unrelated messages into one
        // bucket, which is worse than not generalizing at all.
        assert_eq!(generalize_message("mismatched `type"), "mismatched `type");
    }
}
