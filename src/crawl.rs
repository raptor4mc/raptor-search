//crawl.rs
use scraper::{Html, Selector};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use once_cell::sync::Lazy;
use std::time::Duration;
use std::io::Write;
use bytes::Bytes;
use tokio::sync::mpsc;
use std::sync::Arc;

use crate::algorithm::extract;
use crate::algorithm::tokenize;

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("raptor-search/0.1 (+https://github.com/raptor4mc)")
        .build()
        .expect("failed to build http client")
});

const BAD_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".svg", ".ico", ".zip", ".tar",
    ".pdf", ".woff", ".woff2", ".ttf", ".css", ".js", ".xml", ".json",
    ".mp4", ".mp3", ".webm", ".wav", ".otf", ".eot", ".exe", ".dmg",
    ".gz", ".rar", ".7z", ".iso",
];

const NO_FOLLOW_DOMAINS: &[&str] = &[
    "twitter.com", "x.com", "facebook.com", "instagram.com",
    "tiktok.com", "linkedin.com", "pinterest.com", "snapchat.com",
    "youtube.com", "youtu.be", "twitch.tv", "discord.com",
    "discord.gg", "t.me", "telegram.org", "whatsapp.com",
    "reddit.com", "news.ycombinator.com", "medium.com",
];

const SKIP_URL_PATTERNS: &[&str] = &[
    "github.com/blob/",
    "github.com/actions/",
    "github.com/commit/",
    "github.com/pull/",
    "github.com/issues/",
    "github.com/compare/",
    "github.com/releases/",
    "github.com/edit/",
    "github.com/tree/",
    "docs.rs",
    "crates.io",
    "codecov.io",
    "github.com/workflows/",
];

const JUNK_SIGNALS: &[&str] = &[
    "window.WIZ_global_data",
    "window.__",
    ":root {",
    "featureFlags",
    "var _gaq",
    "WIZ_global",
];

#[derive(Debug, Clone)]
pub struct StorageTask {
    pub url: String,
    pub content: String,
    pub content_hash: String,
}

pub struct CrawledPage {
    pub title: String,
    pub snippet: String,
    /// Clean, deduped, script/style-free text — this is what actually gets
    /// indexed for full-text search (see migrations/002_algorithm_upgrade.sql).
    /// Capped to a reasonable length so the index doesn't bloat.
    pub body_text: String,
    pub meta_description: Option<String>,
    pub content_hash: String,
    pub content: String,
    pub discovered_urls: Vec<String>,
    pub metadata: crate::algorithm::metadata::PageMetadata,
}

pub fn is_junk_url(url: &str) -> bool {
    SKIP_URL_PATTERNS.iter().any(|p| url.contains(p))
}

pub fn normalize_url(u: &str) -> Option<String> {
    let mut parsed = reqwest::Url::parse(u).ok()?;
    // lowercase scheme and host
    let scheme = parsed.scheme().to_lowercase();
    let host = parsed.host_str()?.to_lowercase();
    parsed.set_scheme(&scheme).ok()?;
    parsed.set_host(Some(&host)).ok()?;
    // remove fragment
    parsed.set_fragment(None);

    // strip common tracking query params
    let mut pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .into_owned()
        .filter(|(k, _)| {
            let kl = k.to_lowercase();
            !kl.starts_with("utm_") && kl != "fbclid" && kl != "gclid" && kl != "mc_cid" && kl != "mc_eid"
        })
        .collect();
    pairs.sort();
    if pairs.is_empty() {
        parsed.set_query(None);
    } else {
        parsed.query_pairs_mut().clear().extend_pairs(pairs);
    }
    Some(parsed.into_string().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn test_normalize() {
        let u = "https://Example.COM:443/path/?utm_source=google#frag";
        let n = normalize_url(u).unwrap();
        assert_eq!(n, "https://example.com/path");
    }
}

pub async fn crawl(url: &str) -> Result<CrawledPage, Box<dyn std::error::Error + Send + Sync>> {
    // Prefer checking headers first to avoid downloading non-HTML or huge responses.
    if let Ok(head) = CLIENT.head(url).send().await {
        if head.status().is_success() {
            if let Some(ct) = head.headers().get(reqwest::header::CONTENT_TYPE) {
                let ct = ct.to_str().unwrap_or("");
                if !ct.contains("text/html") {
                    return Err("Non-HTML content, skipping".into());
                }
            }

            if let Some(len) = head.headers().get(reqwest::header::CONTENT_LENGTH) {
                if let Ok(len_str) = len.to_str() {
                    if let Ok(n) = len_str.parse::<u64>() {
                        if n > 2_000_000 {
                            return Err("Content too large, skipping".into());
                        }
                    }
                }
            }
        }
    }

    // Retry GET with exponential backoff for transient errors.
    let max_retries: u32 = std::env::var("CRAWL_MAX_RETRIES").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let mut attempt: u32 = 0;
    let fetch_start = std::time::Instant::now();
    let body = loop {
        let res = CLIENT.get(url).send().await;
        match res {
            Ok(resp) => match resp.text().await {
                Ok(t) => break t,
                Err(e) => {
                    attempt += 1;
                    if attempt > max_retries {
                        return Err(Box::new(e));
                    }
                }
            },
            Err(e) => {
                attempt += 1;
                if attempt > max_retries {
                    return Err(Box::new(e));
                }
            }
        }
        let backoff = 2u64.pow(attempt.min(6));
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
    };
    let page_load_time_ms = fetch_start.elapsed().as_millis() as i32;
    let document = Html::parse_document(&body);

    let title_sel = Selector::parse("title").unwrap();
    let title = document
        .select(&title_sel)
        .next()
        .map(|t| t.inner_html())
        .unwrap_or_else(|| "No title".to_string());

    // Visible, script/style-free text (see algorithm::extract for why this
    // matters — the old `.text()` call over the whole document pulled in
    // raw CSS/JS as if it were page content).
    let main_text = extract::main_content_text(&document);
    let meta_description = extract::meta_description(&document);

    // raw_text is what gets hashed for change-detection and archived to
    // disk/S3 — keep it as the full clean visible text, not the fallback
    // whole-doc-with-script-tags text the old code used.
    let raw_text = if !main_text.is_empty() {
        main_text.clone()
    } else {
        extract::visible_text(&document)
    };

    // Display snippet: meta description first, then deduped body text.
    // Never derived from stemmed/stopword-filtered text.
    let snippet = extract::build_snippet(&document, &main_text, 200);

    // body_text is the deduped, capped text that actually gets indexed for
    // full-text search (see store_page + migration for the new column).
    let deduped = extract::dedupe_boilerplate(&raw_text);
    let body_text: String = deduped.chars().take(5000).collect();

    let mut hasher = Sha256::new();
    hasher.update(raw_text.as_bytes());
    let content_hash = hex::encode(hasher.finalize());

    let link_sel = Selector::parse("a[href]").unwrap();
    let base_url = reqwest::Url::parse(url)?;
    let mut discovered_urls = Vec::new();
    let mut internal_links_count: i32 = 0;
    let mut external_links_count: i32 = 0;

    let should_follow = !NO_FOLLOW_DOMAINS.iter().any(|d| {
        base_url.host_str().unwrap_or("").contains(d)
    });

    for element in document.select(&link_sel) {
        if let Some(href) = element.value().attr("href") {
            let absolute = if href.starts_with("http") {
                href.to_string()
            } else if href.starts_with('/') {
                format!(
                    "{}://{}{}",
                    base_url.scheme(),
                    base_url.host_str().unwrap_or(""),
                    href
                )
            } else {
                continue;
            };

            // Count every real link toward the structural signal, even on
            // no-follow domains — should_follow only controls whether we
            // queue the URL for further crawling, it shouldn't also zero
            // out the link-count signal for those pages.
            if let Ok(link_url) = reqwest::Url::parse(&absolute) {
                if link_url.host_str() == base_url.host_str() {
                    internal_links_count += 1;
                } else {
                    external_links_count += 1;
                }
            }

            if !should_follow {
                continue;
            }

            let lower = absolute.to_lowercase();
            if !lower.contains('#')
                && absolute.starts_with("http")
                && !BAD_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
                && !SKIP_URL_PATTERNS.iter().any(|p| absolute.contains(p))
            {
                if let Some(n) = normalize_url(&absolute) {
                    discovered_urls.push(n);
                }
            }
        }
    }

    let metadata = crate::algorithm::metadata::extract(
        &document,
        url,
        body_text.len(),
        internal_links_count,
        external_links_count,
        page_load_time_ms,
    );

    // Limit discovered URLs per page to avoid queue explosion
    let max_discovered: usize = std::env::var("MAX_DISCOVERED_PER_PAGE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    if discovered_urls.len() > max_discovered {
        discovered_urls.truncate(max_discovered);
    }

    Ok(CrawledPage {
        title,
        snippet,
        body_text,
        meta_description,
        content_hash,
        content: raw_text,
        discovered_urls,
        metadata,
    })
}

// Kept for callers that need stemmed/stopword-filtered tokens (e.g. building
// a custom inverted index instead of relying on Postgres tsvector). Never
// use this output as display text — see algorithm::tokenize doc comment.
#[allow(dead_code)]
pub fn index_tokens_for(text: &str) -> String {
    tokenize::index_text(text)
}

pub async fn store_page(
    pool: &PgPool,
    url: &str,
    page: &CrawledPage,
    storage_tx: Option<mpsc::UnboundedSender<StorageTask>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Skip junk pages. Checked against page.content (script/style already
    // stripped by algorithm::extract), not the display snippet — previously
    // this ran against the snippet built from *unfiltered* text, so a page
    // with an inline <style> containing ":root {" (extremely common on
    // modern sites) would trip this and get silently dropped from the index
    // entirely, no matter how good its actual content was.
    if JUNK_SIGNALS.iter().any(|s| page.content.contains(s)) {
        println!("Skipping junk: {}", url);
        return Ok(());
    }

    // Normalize URL
    let url = url.trim_end_matches('/');

    let existing: Option<(String,)> =
        sqlx::query_as("SELECT content_hash FROM pages WHERE url = $1")
            .bind(url)
            .fetch_optional(pool)
            .await?;

    if let Some((hash,)) = existing {
        if hash == page.content_hash {
            println!("Skipping unchanged: {}", url);
            return Ok(());
        }
    }

    sqlx::query(
        "INSERT INTO pages (
            url, title, snippet, content_hash, body_text, meta_description,
            content_length, heading_count, external_links_count, internal_links_count,
            url_depth, is_https, canonical_url, is_canonical, has_structured_data,
            schema_type, image_count, video_count, hreflang_languages, mobile_friendly,
            page_load_time_ms
         )
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)
         ON CONFLICT (url) DO UPDATE SET
             title = EXCLUDED.title,
             snippet = EXCLUDED.snippet,
             content_hash = EXCLUDED.content_hash,
             body_text = EXCLUDED.body_text,
             meta_description = EXCLUDED.meta_description,
             content_length = EXCLUDED.content_length,
             heading_count = EXCLUDED.heading_count,
             external_links_count = EXCLUDED.external_links_count,
             internal_links_count = EXCLUDED.internal_links_count,
             url_depth = EXCLUDED.url_depth,
             is_https = EXCLUDED.is_https,
             canonical_url = EXCLUDED.canonical_url,
             is_canonical = EXCLUDED.is_canonical,
             has_structured_data = EXCLUDED.has_structured_data,
             schema_type = EXCLUDED.schema_type,
             image_count = EXCLUDED.image_count,
             video_count = EXCLUDED.video_count,
             hreflang_languages = EXCLUDED.hreflang_languages,
             mobile_friendly = EXCLUDED.mobile_friendly,
             page_load_time_ms = EXCLUDED.page_load_time_ms,
             crawled_at = now()",
    )
    .bind(url)
    .bind(&page.title)
    .bind(&page.snippet)
    .bind(&page.content_hash)
    .bind(&page.body_text)
    .bind(&page.meta_description)
    .bind(page.metadata.content_length)
    .bind(page.metadata.heading_count)
    .bind(page.metadata.external_links_count)
    .bind(page.metadata.internal_links_count)
    .bind(page.metadata.url_depth)
    .bind(page.metadata.is_https)
    .bind(&page.metadata.canonical_url)
    .bind(page.metadata.is_canonical)
    .bind(page.metadata.has_structured_data)
    .bind(&page.metadata.schema_type)
    .bind(page.metadata.image_count)
    .bind(page.metadata.video_count)
    .bind(&page.metadata.hreflang_languages)
    .bind(page.metadata.mobile_friendly)
    .bind(page.metadata.page_load_time_ms)
    .execute(pool)
    .await?;

    // Offload storage (S3 or disk) to background task
    if let Some(tx) = storage_tx {
        let task = StorageTask {
            url: url.to_string(),
            content: page.content.clone(),
            content_hash: page.content_hash.clone(),
        };
        let _ = tx.send(task);
    }

    Ok(())
}

// Background worker to handle storage tasks (S3 uploads or disk storage)
pub async fn storage_worker(
    pool: Arc<PgPool>,
    mut rx: mpsc::UnboundedReceiver<StorageTask>,
) {
    while let Some(task) = rx.recv().await {
        if let Err(e) = process_storage_task(&pool, &task).await {
            eprintln!("Storage task failed for {}: {}", task.url, e);
        }
    }
}

// Backblaze B2's S3-compatible API needs a client pointed at B2's endpoint
// with B2's own credentials — `aws_config::load_from_env()` only knows about
// standard AWS_* env vars, so it silently connects to nothing useful (or
// errors) when given B2_KEY_ID/B2_APPLICATION_KEY. This builds an explicit
// client instead.
async fn build_b2_client(endpoint: &str, key_id: &str, app_key: &str) -> aws_sdk_s3::Client {
    let region = extract_region_from_endpoint(endpoint).unwrap_or_else(|| "us-west-004".to_string());
    let creds = aws_credential_types::Credentials::new(key_id, app_key, None, None, "b2-static");
    let config = aws_sdk_s3::config::Builder::new()
        .behavior_version(aws_config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new(region))
        .endpoint_url(endpoint)
        .credentials_provider(creds)
        // B2's S3-compatible API expects path-style requests
        // (https://endpoint/bucket/key) rather than virtual-hosted-style
        // (https://bucket.endpoint/key).
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

// B2 endpoints look like https://s3.us-west-004.backblazeb2.com — the
// region code is the middle segment.
fn extract_region_from_endpoint(endpoint: &str) -> Option<String> {
    let host = endpoint
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let mut parts = host.split('.');
    parts.next()?; // "s3"
    parts.next().map(|s| s.to_string())
}

async fn process_storage_task(
    pool: &PgPool,
    task: &StorageTask,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if task.content.is_empty() {
        return Ok(());
    }

    // Compress content
    let mut buf = Vec::new();
    {
        let mut encoder = zstd::stream::Encoder::new(&mut buf, 3)?;
        encoder.write_all(task.content.as_bytes())?;
        encoder.finish()?;
    }

    // Try B2 if configured (matches the env vars your GitHub Actions
    // workflow already sets: B2_BUCKET_NAME, B2_ENDPOINT, B2_KEY_ID,
    // B2_APPLICATION_KEY), otherwise fall back to disk.
    let b2_config = (
        std::env::var("B2_BUCKET_NAME"),
        std::env::var("B2_ENDPOINT"),
        std::env::var("B2_KEY_ID"),
        std::env::var("B2_APPLICATION_KEY"),
    );

    if let (Ok(bucket), Ok(endpoint), Ok(key_id), Ok(app_key)) = b2_config {
        let key = format!("pages/{}.zst", task.content_hash);

        // Ensure table exists
        let _ = sqlx::query(
            "CREATE TABLE IF NOT EXISTS page_blobs (
                content_hash TEXT PRIMARY KEY,
                s3_key TEXT,
                size BIGINT,
                uploaded_at TIMESTAMP WITH TIME ZONE DEFAULT now()
            )"
        )
        .execute(pool)
        .await;

        // Upload to B2
        let client = build_b2_client(&endpoint, &key_id, &app_key).await;
        client
            .put_object()
            .bucket(&bucket)
            .key(&key)
            .body(Bytes::from(buf.clone()).into())
            .send()
            .await?;

        // Record in DB
        let _ = sqlx::query(
            "INSERT INTO page_blobs (content_hash, s3_key, size) VALUES ($1, $2, $3)
             ON CONFLICT (content_hash) DO NOTHING"
        )
        .bind(&task.content_hash)
        .bind(&key)
        .bind(buf.len() as i64)
        .execute(pool)
        .await?;
    } else {
        // Fallback to disk storage
        let dir = std::path::Path::new("database/pages");
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
        }
        let path = dir.join(format!("{}.zst", task.content_hash));
        if !path.exists() {
            use std::fs::File;
            let f = File::create(&path)?;
            let mut encoder = zstd::stream::Encoder::new(f, 3)?;
            encoder.write_all(task.content.as_bytes())?;
            encoder.finish()?;
        }
    }

    Ok(())
}

// Prune pages older than TTL (days) and optionally delete S3 blobs
pub async fn prune_old_pages(pool: &PgPool) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let days: i64 = std::env::var("PRUNE_DAYS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
    let deleted = sqlx::query_scalar::<_, i64>(
        "DELETE FROM pages WHERE crawled_at < now() - ($1::interval) RETURNING 1"
    )
    .bind(format!("{} days", days))
    .fetch_all(pool)
    .await?
    .len();

    // Optionally prune page_blobs and B2 objects
    if let (Ok(bucket), Ok(endpoint), Ok(key_id), Ok(app_key)) = (
        std::env::var("B2_BUCKET_NAME"),
        std::env::var("B2_ENDPOINT"),
        std::env::var("B2_KEY_ID"),
        std::env::var("B2_APPLICATION_KEY"),
    ) {
        let rows: Vec<(String,String)> = sqlx::query_as("SELECT content_hash, s3_key FROM page_blobs WHERE uploaded_at < now() - ($1::interval)")
            .bind(format!("{} days", days))
            .fetch_all(pool)
            .await?;
        if !rows.is_empty() {
            let client = build_b2_client(&endpoint, &key_id, &app_key).await;
            for (_hash, key) in rows.iter() {
                let _ = client.delete_object().bucket(&bucket).key(key).send().await;
            }
            let _ = sqlx::query("DELETE FROM page_blobs WHERE uploaded_at < now() - ($1::interval)")
                .bind(format!("{} days", days))
                .execute(pool)
                .await?;
        }
    }

    Ok(deleted)
}
