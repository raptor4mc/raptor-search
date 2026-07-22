// algorithm/extract.rs
//
// Everything to do with pulling *display-safe* text out of a parsed HTML
// document. The one rule this file exists to enforce: text that ends up in
// `visible_text` must be text a human would actually see rendered on the
// page. Nothing from <script>, <style>, <noscript>, <template>, or <svg>.
//
// This matters more than it sounds like: scraper's `ElementRef::text()`
// walks every descendant text node with no idea what tag it's inside, so a
// naive `document.root_element().text()` call pulls in raw CSS/JS source as
// if it were prose. That was silently nuking pages from the index whenever
// their inline <style> happened to contain a string on the junk-signal list
// (":root {" is extremely common in modern SSR'd sites).

use ego_tree::NodeRef;
use scraper::{Html, Node, Selector};
use std::collections::HashSet;

const SKIP_TAGS: &[&str] = &["script", "style", "noscript", "template", "svg", "iframe"];

/// Walks the full DOM in document order, collecting only text nodes that are
/// not descendants of a skipped tag. This is the thing that should be used
/// anywhere "give me the text on this page" is needed.
pub fn visible_text(document: &Html) -> String {
    let mut buf = String::new();
    collect(document.tree.root(), &mut buf);
    buf
}

fn collect(node: NodeRef<Node>, buf: &mut String) {
    if let Node::Element(el) = node.value() {
        if SKIP_TAGS.contains(&el.name()) {
            return;
        }
    }
    if let Node::Text(t) = node.value() {
        let s = t.text.trim();
        if !s.is_empty() {
            buf.push_str(s);
            buf.push(' ');
        }
    }
    for child in node.children() {
        collect(child, buf);
    }
}

/// Prefer <main>/<article> content when present — it's usually the actual
/// substance of the page, as opposed to nav/header/footer chrome that
/// surrounds it. Falls back to whole-document visible text otherwise.
pub fn main_content_text(document: &Html) -> String {
    for sel in ["main", "article", "[role=main]"] {
        if let Ok(selector) = Selector::parse(sel) {
            if let Some(el) = document.select(&selector).next() {
                let mut buf = String::new();
                collect(*el, &mut buf);
                let trimmed = buf.trim();
                if trimmed.chars().count() > 100 {
                    return trimmed.to_string();
                }
            }
        }
    }
    visible_text(document).trim().to_string()
}

/// `<meta name="description">` (falls back to `og:description`) is almost
/// always a clean, hand-written sentence — far more reliable than anything
/// pulled from the body. This should be the first-choice snippet source.
pub fn meta_description(document: &Html) -> Option<String> {
    let sel = Selector::parse(r#"meta[name="description"], meta[property="og:description"]"#).ok()?;
    document
        .select(&sel)
        .find_map(|el| el.value().attr("content"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Sites frequently repeat the same short line of chrome (mobile-menu
/// duplicates of desktop nav, a "get the app" banner, cookie notices) many
/// times in the raw DOM. Collapse consecutive/repeated short runs so a
/// snippet doesn't end up as "scan get mobile app" x8.
pub fn dedupe_boilerplate(text: &str) -> String {
    // Split on multiple spaces isn't enough since we already joined with
    // single spaces; instead split into "chunks" using simple heuristics:
    // treat runs of >=4 words that repeat verbatim elsewhere in the text as
    // boilerplate after the first occurrence.
    let words: Vec<&str> = text.split_whitespace().collect();
    const WINDOW: usize = 5;
    if words.len() < WINDOW * 2 {
        return text.to_string();
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        if i + WINDOW <= words.len() {
            let window = words[i..i + WINDOW].join(" ").to_lowercase();
            if seen.contains(&window) {
                // Skip this whole repeated window, it's a re-occurrence of
                // something we've already kept once.
                i += WINDOW;
                continue;
            }
            seen.insert(window);
        }
        out.push(words[i]);
        i += 1;
    }
    out.join(" ")
}

/// Builds the display snippet with the priority: meta description > deduped
/// main-content text. Truncates to a sane length for display, on a char
/// boundary and word boundary where possible.
pub fn build_snippet(document: &Html, fallback_text: &str, max_chars: usize) -> String {
    let source = meta_description(document)
        .unwrap_or_else(|| dedupe_boilerplate(fallback_text));

    let truncated: String = source.chars().take(max_chars).collect();
    // Trim to the last full word so we don't cut mid-token.
    match truncated.rfind(' ') {
        Some(idx) if truncated.len() as f64 > max_chars as f64 * 0.8 => truncated[..idx].to_string(),
        _ => truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_script_and_style() {
        let html = r#"
            <html><head>
                <style>:root { --color-primary: #fff; } body { margin:0 }</style>
                <script>window.__DATA__ = { foo: 1 };</script>
            </head>
            <body><p>Discord is great for playing games and chilling with friends.</p></body>
            </html>
        "#;
        let doc = Html::parse_document(html);
        let text = visible_text(&doc);
        assert!(!text.contains("root"));
        assert!(!text.contains("DATA"));
        assert!(text.contains("Discord is great"));
    }

    #[test]
    fn dedupes_repeated_boilerplate() {
        let text = "scan get mobile app scan get mobile app scan get mobile app take control of your data";
        let deduped = dedupe_boilerplate(text);
        assert_eq!(deduped, "scan get mobile app take control of your data");
    }

    #[test]
    fn prefers_meta_description() {
        let html = r#"<html><head><meta name="description" content="A clean hand-written summary."></head>
            <body><p>Lots of noisy body copy that would otherwise be used.</p></body></html>"#;
        let doc = Html::parse_document(html);
        let snippet = build_snippet(&doc, "fallback text here", 200);
        assert_eq!(snippet, "A clean hand-written summary.");
    }
}
