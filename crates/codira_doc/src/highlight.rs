//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - Syntax highlighting of Codira code as HTML, from the compiler's own lexer,
//!   so the highlighting agrees with how the compiler reads it.

use codira_syntax::{tokenize, SyntaxKind};

/// HTML-escapes `text`.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The CSS class of a token, if it is highlighted.
fn class_of(kind: SyntaxKind) -> Option<&'static str> {
    if kind.is_keyword() {
        return Some("kw");
    }
    match kind {
        SyntaxKind::COMMENT => Some("cm"),
        SyntaxKind::STRING => Some("st"),
        SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER => Some("nu"),
        SyntaxKind::AT => Some("at"),
        _ => None,
    }
}

/// The tokens of `code` as `(class, text)`, with a name that follows `@`
/// classed as an attribute.
pub fn classify(code: &str) -> Vec<(Option<&'static str>, &str)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut after_at = false;
    for token in tokenize(code) {
        let len = u32::from(token.len) as usize;
        let text = &code[offset..offset + len];
        offset += len;
        let class = if after_at && token.kind == SyntaxKind::IDENT {
            Some("at")
        } else {
            class_of(token.kind)
        };
        after_at = token.kind == SyntaxKind::AT;
        out.push((class, text));
    }
    out
}

/// `code` as highlighted HTML (no surrounding element). `link` may turn an
/// identifier into a link: it returns the `href` for a name.
pub fn highlight(code: &str, link: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    for (class, text) in classify(code) {
        let escaped = escape(text);
        if class.is_none()
            && text
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
        {
            if let Some(href) = link(text) {
                out.push_str(&format!(
                    "<a class=\"ty\" href=\"{}\">{escaped}</a>",
                    escape(&href)
                ));
                continue;
            }
        }
        match class {
            Some(class) => out.push_str(&format!("<span class=\"{class}\">{escaped}</span>")),
            None => out.push_str(&escaped),
        }
    }
    out
}

/// `code` as highlighted HTML split into lines: a token that spans lines
/// (a block comment, a multi-line string) is closed at each line end and
/// reopened on the next line, so each line is well-formed on its own.
pub fn highlight_lines(code: &str) -> Vec<String> {
    let mut lines = vec![String::new()];
    for (class, text) in classify(code) {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                lines.push(String::new());
            }
            if part.is_empty() {
                continue;
            }
            let part = part.strip_suffix('\r').unwrap_or(part);
            let line = lines.last_mut().unwrap();
            match class {
                Some(class) => {
                    line.push_str(&format!("<span class=\"{class}\">{}</span>", escape(part)))
                }
                None => line.push_str(&escape(part)),
            }
        }
    }
    if code.ends_with('\n') {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_keywords_strings_comments_and_attributes() {
        let html = highlight("@noalloc func f() -> i32 { \"<s>\" // c\n }", &|_| None);
        assert!(html.contains("<span class=\"at\">@</span><span class=\"at\">noalloc</span>"));
        assert!(html.contains("<span class=\"kw\">func</span>"));
        assert!(html.contains("<span class=\"st\">&quot;&lt;s&gt;&quot;</span>"));
        assert!(html.contains("<span class=\"cm\">// c</span>"));
    }

    #[test]
    fn links_identifiers_the_resolver_knows() {
        let html = highlight("func f(p: Point) -> i32", &|name| {
            (name == "Point").then(|| "m.html#struct.Point".to_string())
        });
        assert!(html.contains("<a class=\"ty\" href=\"m.html#struct.Point\">Point</a>"));
        assert!(!html.contains(">i32</a>"));
    }

    #[test]
    fn multi_line_tokens_are_split_per_line() {
        let lines = highlight_lines("let a = 1; /* one\ntwo */ let b = 2;\n");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("<span class=\"cm\">/* one</span>"));
        assert!(lines[1].starts_with("<span class=\"cm\">two */</span>"));
    }
}
