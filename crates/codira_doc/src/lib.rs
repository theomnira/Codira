//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - `codira doc`: static, searchable HTML API documentation for a set of
//!   Codira source files (a package, or the standard library).
//!
//! The input is the source files and, for each function, the semantic
//! envelope the compiler inferred for it (computed by the caller with
//! `codira_hir::realtime::envelope_report`). The output is a directory of
//! static files: an index, one page per module with every documented item
//! (signature, doc comment rendered from Markdown, contracts, attributes,
//! envelope badges, a link to its source line), one highlighted page per
//! source file, and a search index loaded by a small script. It works from
//! `file://`: nothing needs a server.
//!
//! Cross-references: a type name in a signature links to that type's
//! documentation, and `[name]` or `[module.name]` in a doc comment links to
//! the item it names. A name is linked only when it resolves to exactly one
//! documented item, preferring the current module; otherwise it stays text.

mod highlight;
pub mod model;

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write as _,
    path::Path,
};

pub use highlight::{escape, highlight, highlight_lines};
use model::{FileDocs, Item, Kind};
use pulldown_cmark::{BrokenLink, CodeBlockKind, CowStr, Event, Options, Parser, Tag};

/// One source file to document.
#[derive(Debug, Clone)]
pub struct SourceInput {
    /// The path relative to the source root, with `/` separators.
    pub path: String,
    pub text: String,
}

/// One dimension of a function's inferred envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dimension {
    /// `allocation`, `blocking`, `panic`, `termination`, `foreign`,
    /// `concurrency`, `effects` or `secrecy`.
    pub key: String,
    /// Whether the analysis found the effect (the function may allocate,
    /// block, ...).
    pub present: bool,
    /// Why, when present: the chain of calls down to the source.
    pub because: Option<String>,
}

/// A function's envelope as the compiler infers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub dimensions: Vec<Dimension>,
    /// The function's `@envelope` clauses, if it declares one.
    pub declared: Option<Vec<(String, String)>>,
    /// Declared clauses the inferred envelope does not meet.
    pub violated: Vec<String>,
    /// An `extern` function's declaration is trusted, not inferred.
    pub is_extern: bool,
}

/// What to document.
#[derive(Debug, Clone, Default)]
pub struct DocSet {
    /// The page title: the package name, or `std`.
    pub title: String,
    /// The prefix of module names: `std` for the standard library, the
    /// package name otherwise.
    pub namespace: String,
    pub files: Vec<SourceInput>,
    /// Envelopes by source path and byte offset of the function's syntax
    /// node.
    pub envelopes: HashMap<(String, u32), Envelope>,
    /// Document non-`public` items too.
    pub include_private: bool,
}

/// What [`render`] produced.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub modules: usize,
    pub items: usize,
    /// Documented functions with an inferred envelope shown.
    pub with_envelope: usize,
    /// Public items with no doc comment.
    pub undocumented: Vec<String>,
}

struct Module {
    name: String,
    path: String,
    docs: FileDocs,
}

impl Module {
    fn page(&self) -> String {
        format!("{}.html", self.name)
    }
}

/// The module name of a source path: `collections/array.code` in
/// namespace `std` is `std.collections.array`; `mod.code` names its
/// directory.
pub fn module_name(namespace: &str, path: &str) -> String {
    let stem = path.strip_suffix(".code").unwrap_or(path);
    let mut parts: Vec<&str> = std::iter::once(namespace)
        .chain(stem.split('/'))
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() > 1 && parts.last() == Some(&"mod") {
        parts.pop();
    }
    parts.join(".")
}

/// Every name that can be linked: name -> (module, anchor, kind).
struct Index {
    by_name: HashMap<String, Vec<(usize, String, Kind)>>,
    modules: Vec<(String, String)>,
}

impl Index {
    fn new(modules: &[Module], visible: &dyn Fn(&Item) -> bool) -> Index {
        let mut by_name: HashMap<String, Vec<(usize, String, Kind)>> = HashMap::new();
        for (m, module) in modules.iter().enumerate() {
            for item in module.docs.items.iter().filter(|i| visible(i)) {
                if item.kind != Kind::Method {
                    by_name.entry(item.name.clone()).or_default().push((
                        m,
                        item.anchor(),
                        item.kind,
                    ));
                }
            }
        }
        Index {
            by_name,
            modules: modules.iter().map(|m| (m.name.clone(), m.page())).collect(),
        }
    }

    /// The `href` (relative to a module page) of `name` seen from module
    /// `from`: an item of that name in `from`, or the only one anywhere.
    fn resolve(&self, from: usize, name: &str, types_only: bool) -> Option<String> {
        if let Some((module, rest)) = name.rsplit_once('.') {
            let m = self
                .modules
                .iter()
                .position(|(n, _)| n == module || n.ends_with(&format!(".{module}")))?;
            let (_, anchor, _) = self.by_name.get(rest)?.iter().find(|(i, _, _)| *i == m)?;
            return Some(format!("{}#{anchor}", self.modules[m].1));
        }
        let candidates: Vec<_> = self
            .by_name
            .get(name)?
            .iter()
            .filter(|(_, _, kind)| !types_only || kind.is_type())
            .collect();
        let pick = candidates
            .iter()
            .find(|(m, _, _)| *m == from)
            .or_else(|| (candidates.len() == 1).then(|| &candidates[0]))?;
        Some(format!("{}#{}", self.modules[pick.0].1, pick.1))
    }
}

/// Renders the documentation as `(relative path, contents)` pairs.
pub fn render(set: &DocSet) -> (Vec<(String, String)>, Summary) {
    let mut files: Vec<&SourceInput> = set.files.iter().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let modules: Vec<Module> = files
        .iter()
        .map(|f| Module {
            name: module_name(&set.namespace, &f.path),
            path: f.path.clone(),
            docs: model::extract(&f.text),
        })
        .collect();
    let include_private = set.include_private;
    let visible = move |item: &Item| include_private || item.public;
    let index = Index::new(&modules, &visible);

    let mut out = Vec::new();
    let mut summary = Summary {
        modules: modules.len(),
        ..Summary::default()
    };
    let mut search = Vec::new();

    for (m, module) in modules.iter().enumerate() {
        let mut body = String::new();
        let _ = write!(
            body,
            "<h1>Module <code>{}</code></h1>\n<p class=\"src\"><a href=\"src/{}.html\">source: {}</a></p>\n",
            escape(&module.name),
            escape(&module.path),
            escape(&module.path)
        );
        if !module.docs.docs.is_empty() {
            body.push_str(&markdown(&module.docs.docs, &index, m));
        }
        let items: Vec<&Item> = module.docs.items.iter().filter(|i| visible(i)).collect();
        if items.is_empty() {
            body.push_str("<p class=\"empty\">No documented items.</p>\n");
        }
        let mut by_kind: BTreeMap<Kind, Vec<&Item>> = BTreeMap::new();
        for item in &items {
            by_kind.entry(item.kind).or_default().push(item);
        }
        for (kind, items) in &by_kind {
            let _ = writeln!(body, "<h2>{}</h2>", kind.plural());
            for item in items {
                render_item(
                    &mut body,
                    set,
                    module,
                    m,
                    item,
                    &index,
                    &mut summary,
                    &mut search,
                );
                for method in item.methods.iter().filter(|i| visible(i)) {
                    body.push_str("<div class=\"method\">\n");
                    render_item(
                        &mut body,
                        set,
                        module,
                        m,
                        method,
                        &index,
                        &mut summary,
                        &mut search,
                    );
                    body.push_str("</div>\n");
                }
            }
        }
        search.push(SearchEntry {
            name: module.name.clone(),
            kind: "module",
            href: module.page(),
            summary: first_sentence(&module.docs.docs),
        });
        out.push((module.page(), page(&set.title, &module.name, "", &body)));

        let source = files[m];
        let depth = source.path.matches('/').count() + 1;
        let prefix = "../".repeat(depth);
        let mut code = String::from("<pre class=\"source\">");
        for (i, line) in highlight_lines(&source.text).iter().enumerate() {
            let n = i + 1;
            let _ = writeln!(
                code,
                "<span class=\"line\" id=\"L{n}\"><a class=\"ln\" href=\"#L{n}\">{n}</a>{line}</span>"
            );
        }
        code.push_str("</pre>\n");
        let body = format!(
            "<h1><code>{}</code></h1>\n<p class=\"src\">module <a href=\"{prefix}{}\">{}</a></p>\n{code}",
            escape(&source.path),
            escape(&module.page()),
            escape(&module.name)
        );
        out.push((
            format!("src/{}.html", source.path),
            page(&set.title, &source.path, &prefix, &body),
        ));
    }

    // The index page.
    let mut body = format!(
        "<h1>{}</h1>\n<table class=\"modules\">\n",
        escape(&set.title)
    );
    for module in &modules {
        let documented = module.docs.items.iter().filter(|i| visible(i)).count();
        let _ = writeln!(
            body,
            "<tr><td><a href=\"{}\">{}</a></td><td>{}</td><td class=\"count\">{documented} item(s)</td></tr>",
            escape(&module.page()),
            escape(&module.name),
            escape(&first_sentence(&module.docs.docs))
        );
    }
    body.push_str("</table>\n");
    out.push(("index.html".to_string(), page(&set.title, "", "", &body)));

    let mut js = String::from("window.CODIRA_SEARCH = [\n");
    for entry in &search {
        let _ = writeln!(
            js,
            "[{},{},{},{}],",
            json_string(&entry.name),
            json_string(entry.kind),
            json_string(&entry.href),
            json_string(&entry.summary)
        );
    }
    js.push_str("];\n");
    out.push(("search-index.js".to_string(), js));
    out.push(("static/style.css".to_string(), STYLE.to_string()));
    out.push(("static/search.js".to_string(), SEARCH_JS.to_string()));
    (out, summary)
}

/// Renders and writes the documentation under `out_dir`.
pub fn generate(set: &DocSet, out_dir: &Path) -> std::io::Result<Summary> {
    let (files, summary) = render(set);
    for (path, contents) in files {
        let path = out_dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    Ok(summary)
}

struct SearchEntry {
    name: String,
    kind: &'static str,
    href: String,
    summary: String,
}

#[allow(clippy::too_many_arguments)]
fn render_item(
    body: &mut String,
    set: &DocSet,
    module: &Module,
    m: usize,
    item: &Item,
    index: &Index,
    summary: &mut Summary,
    search: &mut Vec<SearchEntry>,
) {
    summary.items += 1;
    let anchor = item.anchor();
    let title = match &item.owner {
        Some(owner) => format!("{owner}.{}", item.name),
        None => item.name.clone(),
    };
    let _ = writeln!(
        body,
        "<section class=\"item\" id=\"{}\">\n<h3><a href=\"#{}\">{}</a> <span class=\"kind\">{}</span><a class=\"srclink\" href=\"src/{}.html#L{}\">source</a></h3>",
        escape(&anchor),
        escape(&anchor),
        escape(&title),
        item.kind.word(),
        escape(&module.path),
        item.line
    );
    let link = |name: &str| index.resolve(m, name, true);
    let _ = writeln!(
        body,
        "<pre class=\"sig\">{}</pre>",
        highlight(&item.signature, &link)
    );

    // Badges: attributes as written, then the inferred envelope.
    let mut badges = String::new();
    if item.is_extern {
        badges.push_str("<span class=\"badge extern\" title=\"declared in an extern block: its body is foreign code the compiler does not see\">extern</span>");
    }
    for attribute in &item.attributes {
        let _ = write!(
            badges,
            "<code class=\"badge attr\">{}</code>",
            escape(attribute)
        );
    }
    if item.kind == Kind::Function || item.kind == Kind::Method {
        if let Some(envelope) = set.envelopes.get(&(module.path.clone(), item.offset)) {
            summary.with_envelope += 1;
            badges.push_str(&envelope_badges(envelope));
        }
    }
    if !badges.is_empty() {
        let _ = writeln!(body, "<div class=\"badges\">{badges}</div>");
    }
    if !item.contracts.is_empty() {
        body.push_str("<div class=\"contracts\"><span class=\"label\">Contract</span>");
        for contract in &item.contracts {
            let _ = write!(body, "<pre>{}</pre>", highlight(contract, &link));
        }
        body.push_str("</div>\n");
    }
    if item.docs.is_empty() {
        if item.public {
            summary
                .undocumented
                .push(format!("{}:{}: {title}", module.path, item.line));
        }
    } else {
        body.push_str("<div class=\"docs\">");
        body.push_str(&markdown(&item.docs, index, m));
        body.push_str("</div>\n");
    }
    body.push_str("</section>\n");
    search.push(SearchEntry {
        name: format!("{}.{title}", module.name),
        kind: item.kind.word(),
        href: format!("{}#{anchor}", module.page()),
        summary: first_sentence(&item.docs),
    });
}

/// The label of a dimension when the effect is present and when absent.
fn labels(key: &str) -> (&'static str, &'static str) {
    match key {
        "allocation" => ("allocates", "no allocation"),
        "blocking" => ("may block", "non-blocking"),
        "panic" => ("may panic", "no panic"),
        "termination" => ("may not terminate", "terminates"),
        "foreign" => ("calls foreign code", "no foreign calls"),
        "concurrency" => ("spawns tasks", "no concurrency"),
        "effects" => ("performs effects", "no effects"),
        "secrecy" => ("touches secrets", "no secrets"),
        _ => ("", ""),
    }
}

fn envelope_badges(envelope: &Envelope) -> String {
    let mut out = String::new();
    let basis = if envelope.is_extern {
        "trusted declaration: an extern function's envelope is not inferred, and nothing checks it"
    } else {
        "inferred by the compiler's effect analysis of the source"
    };
    let present: Vec<&Dimension> = envelope.dimensions.iter().filter(|d| d.present).collect();
    for d in &present {
        let (label, _) = labels(&d.key);
        let because = d.because.as_deref().unwrap_or(basis);
        let _ = write!(
            out,
            "<span class=\"badge effect\" title=\"{}\">{}</span>",
            escape(because),
            escape(if label.is_empty() { &d.key } else { label })
        );
    }
    let absent: Vec<&str> = envelope
        .dimensions
        .iter()
        .filter(|d| !d.present)
        .map(|d| labels(&d.key).1)
        .filter(|l| !l.is_empty())
        .collect();
    if !absent.is_empty() {
        let _ = write!(
            out,
            "<span class=\"badge pure\" title=\"{}\">{}</span>",
            escape(basis),
            escape(&absent.join(" · "))
        );
    }
    if let Some(declared) = &envelope.declared {
        let clauses: Vec<String> = declared.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        let (class, status) = if envelope.is_extern {
            ("trusted", "trusted, unchecked")
        } else if envelope.violated.is_empty() {
            ("met", "met")
        } else {
            ("violated", "NOT MET")
        };
        let _ = write!(
            out,
            "<span class=\"badge {class}\" title=\"@envelope({})\">@envelope {status}</span>",
            escape(&clauses.join(", "))
        );
    }
    out
}

/// Markdown to HTML: code blocks without a language, or marked `codira` or
/// `code`, are highlighted as Codira; `[name]` links resolve to items.
fn markdown(text: &str, index: &Index, module: usize) -> String {
    let mut resolve = |link: BrokenLink<'_>| -> Option<(CowStr<'_>, CowStr<'_>)> {
        let name = link.reference.trim_matches('`').to_string();
        index
            .resolve(module, &name, false)
            .map(|href| (CowStr::from(href), CowStr::from("")))
    };
    let parser =
        Parser::new_with_broken_link_callback(text, Options::ENABLE_TABLES, Some(&mut resolve));
    let mut events = Vec::new();
    let mut code: Option<String> = None;
    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let is_codira = match &kind {
                    CodeBlockKind::Indented => true,
                    CodeBlockKind::Fenced(lang) => {
                        matches!(lang.as_ref(), "" | "codira" | "code")
                    }
                };
                if is_codira {
                    code = Some(String::new());
                } else {
                    events.push(Event::Start(Tag::CodeBlock(kind)));
                }
            }
            Event::Text(t) if code.is_some() => code.as_mut().unwrap().push_str(&t),
            Event::End(Tag::CodeBlock(kind)) => match code.take() {
                Some(source) => events.push(Event::Html(CowStr::from(format!(
                    "<pre class=\"example\">{}</pre>\n",
                    highlight(source.trim_end(), &|_| None)
                )))),
                None => events.push(Event::End(Tag::CodeBlock(kind))),
            },
            // Raw HTML in a doc comment is shown, not interpreted.
            Event::Html(html) => events.push(Event::Text(html)),
            other => events.push(other),
        }
    }
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events.into_iter());
    html
}

/// The first sentence (or line) of a doc comment, as plain text.
fn first_sentence(docs: &str) -> String {
    let paragraph: String = docs
        .lines()
        .take_while(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let plain: String = paragraph
        .chars()
        .filter(|c| !matches!(c, '`' | '*'))
        .collect();
    let end = plain
        .find(". ")
        .map_or(plain.len(), |i| i + 1)
        .min(plain.len());
    let mut sentence = plain[..end].trim().to_string();
    if sentence.chars().count() > 160 {
        sentence = sentence.chars().take(157).collect::<String>() + "...";
    }
    sentence
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn page(title: &str, heading: &str, prefix: &str, body: &str) -> String {
    let full_title = if heading.is_empty() {
        format!("{title} documentation")
    } else {
        format!("{heading} - {title} documentation")
    };
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<link rel=\"stylesheet\" href=\"{prefix}static/style.css\">\n</head>\n<body>\n\
         <header><a class=\"home\" href=\"{prefix}index.html\">{}</a>\
         <input id=\"search\" type=\"search\" placeholder=\"Search (press /)\" autocomplete=\"off\" data-prefix=\"{prefix}\">\
         <ul id=\"results\"></ul></header>\n<main>\n{body}</main>\n\
         <script src=\"{prefix}search-index.js\"></script>\n<script src=\"{prefix}static/search.js\"></script>\n</body>\n</html>\n",
        escape(&full_title),
        escape(title)
    )
}

const STYLE: &str = include_str!("../assets/style.css");
const SEARCH_JS: &str = include_str!("../assets/search.js");

#[cfg(test)]
mod tests;
