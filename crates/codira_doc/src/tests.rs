//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - Unit tests for item extraction and rendering.

use super::*;

const POINT: &str = r#"//! Points and shapes.
//!
//! See [area] and [Shape].

/// A point in the plane.
@derive(Eq)
public struct Point { x: f64, y: f64 }

/// Something with an area.
public trait Shape {
    /// The area.
    func area(self) -> f64;
}

extend Point {
    /// The distance from the origin.
    public func norm(self) -> f64 { 0.0 }
    func hidden(self) -> f64 { 1.0 }
}

/// The area of the box between two points.
///
/// ```
/// let a = area(Point { x: 0.0, y: 0.0 }, Point { x: 1.0, y: 2.0 })
/// ```
@noalloc
public func area(
    a: Point,
    b: Point,
) -> f64
    requires a.x <= b.x
{
    (b.x - a.x) * (b.y - a.y)
}

func private_helper() {}

public func undocumented() {}

extern "C" {
    public func abs(x: i32) -> i32;
}
"#;

fn set(files: &[(&str, &str)]) -> DocSet {
    DocSet {
        title: "geo".to_string(),
        namespace: "geo".to_string(),
        files: files
            .iter()
            .map(|(p, t)| SourceInput {
                path: p.to_string(),
                text: t.to_string(),
            })
            .collect(),
        ..DocSet::default()
    }
}

fn file<'a>(files: &'a [(String, String)], path: &str) -> &'a str {
    &files
        .iter()
        .find(|(p, _)| p == path)
        .unwrap_or_else(|| panic!("no {path}"))
        .1
}

#[test]
fn extracts_items_docs_signatures_and_contracts() {
    let docs = model::extract(POINT);
    assert_eq!(docs.docs, "Points and shapes.\n\nSee [area] and [Shape].");
    let names: Vec<_> = docs
        .items
        .iter()
        .map(|i| (i.kind, i.name.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            (Kind::Struct, "Point"),
            (Kind::Trait, "Shape"),
            (Kind::Function, "area"),
            (Kind::Function, "private_helper"),
            (Kind::Function, "undocumented"),
            (Kind::Function, "abs"),
        ]
    );
    let point = &docs.items[0];
    assert_eq!(point.docs, "A point in the plane.");
    assert_eq!(point.attributes, ["@derive(Eq)"]);
    assert_eq!(point.signature, "public struct Point { x: f64, y: f64 }");
    let methods: Vec<_> = point
        .methods
        .iter()
        .map(|m| (m.name.as_str(), m.public))
        .collect();
    assert_eq!(methods, [("norm", true), ("hidden", false)]);
    assert_eq!(point.methods[0].docs, "The distance from the origin.");

    let shape = &docs.items[1];
    assert_eq!(shape.signature, "public trait Shape");
    assert_eq!(shape.methods[0].name, "area");
    assert_eq!(shape.methods[0].docs, "The area.");

    let area = &docs.items[2];
    assert_eq!(
        area.signature,
        "public func area(a: Point, b: Point,) -> f64"
    );
    assert_eq!(area.contracts, ["requires a.x <= b.x"]);
    assert_eq!(area.attributes, ["@noalloc"]);
    assert!(area
        .docs
        .starts_with("The area of the box between two points.\n\n```"));
    assert_eq!(area.line, 27);
    assert!(docs.items[5].is_extern);
}

#[test]
fn plain_comment_block_documents_when_there_is_no_doc_comment() {
    let src = "// ── Section ──────\n\n// Returns the length.\n// Never negative.\npublic func len() -> i32 { 0 }\n\n// detached\n\npublic func g() {}\n/// Real docs.\n// not this\npublic func h() {}\n";
    let docs = model::extract(src);
    assert_eq!(docs.items[0].docs, "Returns the length.\nNever negative.");
    assert_eq!(docs.items[1].docs, "");
    assert_eq!(docs.items[2].docs, "Real docs.");
}

#[test]
fn module_names_follow_the_std_namespace() {
    assert_eq!(
        module_name("std", "collections/array.code"),
        "std.collections.array"
    );
    assert_eq!(module_name("std", "prelude.code"), "std.prelude");
    assert_eq!(module_name("std", "fs/mod.code"), "std.fs");
    assert_eq!(module_name("kv", "mod.code"), "kv");
}

#[test]
fn renders_pages_links_and_search_index() {
    let (files, summary) = render(&set(&[("mod.code", POINT)]));
    let page = file(&files, "geo.html");
    // Cross-reference from a signature to the type.
    assert!(
        page.contains("<a class=\"ty\" href=\"geo.html#struct.Point\">Point</a>"),
        "{page}"
    );
    // Intra-doc links in the module docs.
    assert!(
        page.contains("<a href=\"geo.html#func.area\">area</a>"),
        "{page}"
    );
    assert!(
        page.contains("<a href=\"geo.html#trait.Shape\">Shape</a>"),
        "{page}"
    );
    // The example is highlighted as Codira.
    assert!(
        page.contains("<pre class=\"example\"><span class=\"kw\">let</span>"),
        "{page}"
    );
    // Source link to the declaration line.
    assert!(page.contains("href=\"src/mod.code.html#L27\""), "{page}");
    // Private items are left out by default; public methods are kept.
    assert!(!page.contains("private_helper"));
    assert!(!page.contains("hidden"));
    assert!(page.contains("id=\"method.Point.norm\""));
    assert!(page.contains("requires"));

    let source = file(&files, "src/mod.code.html");
    assert!(source.contains("id=\"L27\""));
    assert!(source.contains("href=\"../static/style.css\""));

    let search = file(&files, "search-index.js");
    assert!(search.contains("[\"geo.area\",\"func\",\"geo.html#func.area\",\"The area of the box between two points.\"]"), "{search}");
    assert!(file(&files, "index.html").contains("<a href=\"geo.html\">geo</a>"));
    assert_eq!(
        summary.undocumented,
        ["mod.code:38: undocumented", "mod.code:41: abs"]
    );
}

#[test]
fn private_items_on_request() {
    let mut s = set(&[("mod.code", POINT)]);
    s.include_private = true;
    let (files, _) = render(&s);
    let page = file(&files, "geo.html");
    assert!(page.contains("id=\"func.private_helper\""));
    assert!(page.contains("id=\"method.Point.hidden\""));
}

#[test]
fn envelope_badges() {
    let mut s = set(&[("mod.code", POINT)]);
    let offset = model::extract(POINT).items[2].offset;
    let dims = |present: &[&str]| -> Vec<Dimension> {
        [
            "allocation",
            "blocking",
            "panic",
            "termination",
            "foreign",
            "concurrency",
            "effects",
            "secrecy",
        ]
        .iter()
        .map(|k| Dimension {
            key: k.to_string(),
            present: present.contains(k),
            because: present
                .contains(k)
                .then(|| format!("{k} via area -> helper")),
        })
        .collect()
    };
    s.envelopes.insert(
        ("mod.code".to_string(), offset),
        Envelope {
            dimensions: dims(&["allocation", "panic"]),
            declared: Some(vec![("allocation".to_string(), "none".to_string())]),
            violated: vec!["allocation".to_string()],
            is_extern: false,
        },
    );
    let (files, summary) = render(&s);
    let page = file(&files, "geo.html");
    assert!(page.contains("<span class=\"badge effect\" title=\"allocation via area -&gt; helper\">allocates</span>"), "{page}");
    assert!(page.contains(">may panic</span>"));
    assert!(page.contains("non-blocking · terminates · no foreign calls"));
    assert!(page.contains("@envelope NOT MET"));
    assert_eq!(summary.with_envelope, 1);
}

#[test]
fn ambiguous_names_are_not_linked() {
    let a = "public struct Item { x: i32 }\n";
    let b = "public struct Item { y: i32 }\n";
    let c = "public func f(i: Item) {}\n";
    let (files, _) = render(&set(&[("a.code", a), ("b.code", b), ("c.code", c)]));
    let page = file(&files, "geo.c.html");
    assert!(!page.contains("href=\"geo.a.html#struct.Item\""));
    assert!(!page.contains("href=\"geo.b.html#struct.Item\""));
    // In its own module, the local one wins.
    let page = file(&files, "geo.a.html");
    assert!(page.contains("id=\"struct.Item\""));
}

#[test]
fn raw_html_in_docs_is_escaped() {
    let src = "/// <script>alert(1)</script>\npublic func f() {}\n";
    let (files, _) = render(&set(&[("mod.code", src)]));
    let page = file(&files, "geo.html");
    assert!(!page.contains("<script>alert"));
    assert!(page.contains("&lt;script&gt;"));
}
