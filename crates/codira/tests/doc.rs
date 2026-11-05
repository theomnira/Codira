//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - End-to-end tests of `codira doc`: a package's documentation has a page per
//!   module with signatures, doc comments, cross-references, source links and
//!   the envelope the compiler inferred for each function; the standard library
//!   documents under the `std.` namespace; the output is static files only.

use std::{path::Path, process::Command};

const SOURCE: &str = r#"//! Geometry helpers.

/// A box with a size.
public class Boxed { size: i64 }

/// Makes a [Boxed]; this allocates.
public func make(n: i64) -> Boxed { Boxed { size: n } }

/// Triples and adds one.
///
/// ```
/// let y = step(2) // 7
/// ```
public func step(x: i64) -> i64 { x * 3 + 1 }

func helper() -> i64 { 1 }
"#;

fn codira(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("could not run codira");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[test]
fn doc_documents_a_package_with_envelopes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname=\"geo\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), SOURCE).unwrap();

    let (ok, out) = codira(dir.path(), &["doc", "--undocumented"]);
    assert!(ok, "{out}");
    assert!(out.contains("documented 1 module(s)"), "{out}");
    let doc = dir.path().join("target/doc");
    for file in [
        "index.html",
        "geo.html",
        "src/mod.code.html",
        "search-index.js",
        "static/style.css",
        "static/search.js",
    ] {
        assert!(doc.join(file).is_file(), "missing {file}");
    }
    let page = std::fs::read_to_string(doc.join("geo.html")).unwrap();
    // Doc comments, the module docs and a cross-reference.
    assert!(page.contains("<p>Geometry helpers.</p>"), "{page}");
    assert!(
        page.contains("<a href=\"geo.html#struct.Boxed\">Boxed</a>"),
        "{page}"
    );
    assert!(
        page.contains("<a class=\"ty\" href=\"geo.html#struct.Boxed\">Boxed</a>"),
        "{page}"
    );
    // Envelope badges from the effect analysis: `make` allocates, `step`
    // does not.
    let make =
        &page[page.find("id=\"func.make\"").unwrap()..page.find("id=\"func.step\"").unwrap()];
    assert!(make.contains(">allocates</span>"), "{make}");
    let step = &page[page.find("id=\"func.step\"").unwrap()..];
    assert!(!step.contains(">allocates</span>"), "{step}");
    assert!(step.contains("no allocation"), "{step}");
    // The example is highlighted, the source link points at the line.
    assert!(step.contains("<pre class=\"example\">"), "{step}");
    assert!(page.contains("href=\"src/mod.code.html#L14\""), "{page}");
    // Private items are left out.
    assert!(!page.contains("func.helper"));
    let index = std::fs::read_to_string(doc.join("search-index.js")).unwrap();
    assert!(index.contains("\"geo.step\""), "{index}");
    // Static files only: every link is relative.
    assert!(!page.contains("http://") && !page.contains("https://"));
}

#[test]
fn doc_documents_the_standard_library() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("std-doc");
    let (ok, output) = codira(
        dir.path(),
        &[
            "doc",
            "--std",
            "--no-envelopes",
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert!(ok, "{output}");
    let page = std::fs::read_to_string(out.join("std.collections.array.html")).unwrap();
    assert!(page.contains("Module <code>std.collections.array</code>"));
    assert!(page.contains("id=\"struct.InlineArray\""));
    assert!(
        !page.contains("codira."),
        "library modules use the std. namespace"
    );
}
