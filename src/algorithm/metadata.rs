// algorithm/metadata.rs
//
// Populates the structural columns that already existed on `pages` but were
// never actually written to (content_length, heading_count, image/video
// counts, canonical URL, structured data, hreflang, mobile-friendliness,
// url_depth, is_https, page_load_time_ms). Everything here is "free" — no
// extra network calls, no link-graph computation — it's all derivable from
// the document + URL the crawler already has in hand for this page.
//
// Deliberately NOT included here: domain_authority / page_rank_score (needs
// a real iterative PageRank pass over the whole link graph, not a per-page
// computation) and click_through_rate / bounce_rate / avg_time_on_page
// (needs actual user click telemetry, which nothing in this codebase logs
// yet). Both are legitimate follow-ups but are separate projects.

use scraper::{Html, Selector};

#[derive(Debug, Clone, Default)]
pub struct PageMetadata {
    pub content_length: i32,
    pub heading_count: i32,
    pub external_links_count: i32,
    pub internal_links_count: i32,
    pub url_depth: i32,
    pub is_https: bool,
    pub canonical_url: Option<String>,
    pub is_canonical: bool,
    pub has_structured_data: bool,
    pub schema_type: Option<String>,
    pub image_count: i32,
    pub video_count: i32,
    pub hreflang_languages: Vec<String>,
    pub mobile_friendly: bool,
    pub page_load_time_ms: i32,
}

/// `internal_links_count`/`external_links_count` and `page_load_time_ms` are
/// computed outside this function (link counting happens during the same
/// pass that builds `discovered_urls` in crawl.rs; load time needs a
/// wall-clock timer around the actual HTTP request) — everything else comes
/// straight from the parsed document.
pub fn extract(
    document: &Html,
    url: &str,
    body_text_len: usize,
    internal_links_count: i32,
    external_links_count: i32,
    page_load_time_ms: i32,
) -> PageMetadata {
    let canonical = canonical_url(document);
    let is_canonical = is_canonical(url, &canonical);
    let (has_structured_data, schema_type) = structured_data(document);

    PageMetadata {
        content_length: body_text_len as i32,
        heading_count: heading_count(document),
        external_links_count,
        internal_links_count,
        url_depth: url_depth(url),
        is_https: url.starts_with("https://"),
        canonical_url: canonical,
        is_canonical,
        has_structured_data,
        schema_type,
        image_count: count(document, "img"),
        video_count: count(document, "video, iframe[src*=youtube], iframe[src*=vimeo]"),
        hreflang_languages: hreflang_languages(document),
        mobile_friendly: document
            .select(&Selector::parse(r#"meta[name="viewport"]"#).unwrap())
            .next()
            .is_some(),
        page_load_time_ms,
    }
}

fn count(document: &Html, selector: &str) -> i32 {
    Selector::parse(selector)
        .map(|s| document.select(&s).count() as i32)
        .unwrap_or(0)
}

fn heading_count(document: &Html) -> i32 {
    count(document, "h1, h2, h3, h4, h5, h6")
}

fn canonical_url(document: &Html) -> Option<String> {
    let sel = Selector::parse(r#"link[rel="canonical"]"#).ok()?;
    document
        .select(&sel)
        .next()?
        .value()
        .attr("href")
        .map(|s| s.to_string())
}

/// True when there's no canonical tag at all (page is implicitly its own
/// canonical), or when the declared canonical resolves to this same URL.
fn is_canonical(current_url: &str, canonical: &Option<String>) -> bool {
    let Some(c) = canonical else { return true };
    let Ok(cur) = reqwest::Url::parse(current_url) else { return true };
    let resolved = reqwest::Url::parse(c).or_else(|_| cur.join(c));
    match resolved {
        Ok(can) => {
            cur.host_str() == can.host_str()
                && cur.path().trim_end_matches('/') == can.path().trim_end_matches('/')
        }
        Err(_) => true,
    }
}

/// Checks JSON-LD first (`<script type="application/ld+json">`), falling
/// back to microdata (`itemtype="..."`). Deliberately avoids pulling in a
/// full JSON parser dependency for this — a light substring extraction of
/// `"@type": "X"` is enough to know roughly what kind of content this is
/// (Article, Product, FAQPage, etc.) without needing valid/complete JSON.
fn structured_data(document: &Html) -> (bool, Option<String>) {
    if let Ok(sel) = Selector::parse(r#"script[type="application/ld+json"]"#) {
        if let Some(el) = document.select(&sel).next() {
            let text = el.inner_html();
            return (true, extract_json_ld_type(&text));
        }
    }
    if let Ok(sel) = Selector::parse("[itemtype]") {
        if let Some(el) = document.select(&sel).next() {
            let itemtype = el
                .value()
                .attr("itemtype")
                .map(|s| s.rsplit('/').next().unwrap_or(s).to_string());
            return (true, itemtype);
        }
    }
    (false, None)
}

fn extract_json_ld_type(json_text: &str) -> Option<String> {
    let idx = json_text.find("\"@type\"")?;
    let after = &json_text[idx + 7..];
    let quote_start = after.find('"')? + 1;
    let rest = &after[quote_start..];
    let quote_end = rest.find('"')?;
    Some(rest[..quote_end].to_string())
}

fn hreflang_languages(document: &Html) -> Vec<String> {
    let Ok(sel) = Selector::parse(r#"link[rel="alternate"][hreflang]"#) else {
        return Vec::new();
    };
    document
        .select(&sel)
        .filter_map(|el| el.value().attr("hreflang"))
        .map(|s| s.to_string())
        .collect()
}

fn url_depth(url: &str) -> i32 {
    reqwest::Url::parse(url)
        .ok()
        .map(|u| u.path().split('/').filter(|s| !s.is_empty()).count() as i32)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_json_ld_type() {
        let html = r#"<html><head>
            <script type="application/ld+json">{"@context":"https://schema.org","@type":"Article","headline":"Hi"}</script>
        </head><body></body></html>"#;
        let doc = Html::parse_document(html);
        let (has, schema) = structured_data(&doc);
        assert!(has);
        assert_eq!(schema.as_deref(), Some("Article"));
    }

    #[test]
    fn canonical_matches_same_url() {
        let canonical = Some("https://example.com/page/".to_string());
        assert!(is_canonical("https://example.com/page", &canonical));
    }

    #[test]
    fn canonical_mismatch_detected() {
        let canonical = Some("https://example.com/other-page".to_string());
        assert!(!is_canonical("https://example.com/page", &canonical));
    }

    #[test]
    fn url_depth_counts_segments() {
        assert_eq!(url_depth("https://example.com/a/b/c"), 3);
        assert_eq!(url_depth("https://example.com/"), 0);
    }
}
