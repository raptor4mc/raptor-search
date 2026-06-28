use scraper::{Html, Selector};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use once_cell::sync::Lazy;
use rust_stemmers::{Algorithm, Stemmer};
use std::time::Duration;
use std::io::Write;
use bytes::Bytes;

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("raptor-search/0.1 (+https://github.com/raptor4mc)")
        .build()
        .expect("failed to build http client")
});

static STEMMER: Lazy<Stemmer> = Lazy::new(|| Stemmer::create(Algorithm::English));

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "in", "on", "at", "to", "for", "of", "with", "by",
    "from", "is", "it", "its", "was", "are", "be", "been", "has", "had", "have", "will", "would",
    "could", "should", "may", "might", "do", "does", "did", "not", "no", "so", "if", "as", "up",
    "out", "about", "into", "than", "then", "that", "this", "these", "those", "they", "them",
    "their", "there", "when", "where", "which", "who", "how", "what", "we", "you", "i", "he",
    "she", "my", "your", "our", "can", "also",
];

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

pub struct CrawledPage {
    pub title: String,
    pub snippet: String,
    pub content_hash: String,
    pub content: String,
    pub discovered_urls: Vec<String>,
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
    let document = Html::parse_document(&body);

    let title_sel = Selector::parse("title").unwrap();
    let title = document
        .select(&title_sel)
        .next()
        .map(|t| t.inner_html())
        .unwrap_or_else(|| "No title".to_string());

    // Extract from <p> tags first for better snippets
    let p_sel = Selector::parse("p").unwrap();
    let p_text: String = document
        .select(&p_sel)
        .map(|el| el.text().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(" ");

    let raw_text = if p_text.trim().len() > 100 {
        p_text
    } else {
        document.root_element().text().collect::<Vec<_>>().join(" ")
    };

    let filtered = filter_stopwords(&raw_text);
    let snippet: String = filtered.chars().take(200).collect();

    let mut hasher = Sha256::new();
    hasher.update(raw_text.as_bytes());
    let content_hash = hex::encode(hasher.finalize());

    let link_sel = Selector::parse("a[href]").unwrap();
    let base_url = reqwest::Url::parse(url)?;
    let mut discovered_urls = Vec::new();

    let should_follow = !NO_FOLLOW_DOMAINS.iter().any(|d| {
        base_url.host_str().unwrap_or("").contains(d)
    });

    if should_follow {
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
    }

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
        content_hash,
        content: raw_text,
        discovered_urls,
    })
}

fn filter_stopwords(text: &str) -> String {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphabetic()))
        .filter(|word| {
            let lower = word.to_lowercase();
            let clean: String = lower.chars().filter(|c| c.is_alphabetic()).collect();
            !clean.is_empty() && !STOPWORDS.contains(&clean.as_str())
        })
        .map(|w| {
            let lower = w.to_lowercase();
            let clean: String = lower.chars().filter(|c| c.is_alphabetic()).collect();
            STEMMER.stem(&clean).to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn store_page(
    pool: &PgPool,
    url: &str,
    page: &CrawledPage,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Skip junk pages
    if JUNK_SIGNALS.iter().any(|s| page.snippet.contains(s)) {
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
        "INSERT INTO pages (url, title, snippet, content_hash)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (url) DO UPDATE SET
             title = EXCLUDED.title,
             snippet = EXCLUDED.snippet,
             content_hash = EXCLUDED.content_hash,
             crawled_at = now()",
    )
    .bind(url)
    .bind(&page.title)
    .bind(&page.snippet)
    .bind(&page.content_hash)
    .execute(pool)
    .await?;

    // Optionally offload compressed content to S3-compatible storage if configured.
    if let Ok(bucket) = std::env::var("S3_BUCKET") {
        if !page.content.is_empty() {
            let key = format!("pages/{}.zst", page.content_hash);
            // ensure page_blobs table exists
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

            // compress to memory
            let mut buf = Vec::new();
            {
                let mut encoder = zstd::stream::Encoder::new(&mut buf, 3)?;
                encoder.write_all(page.content.as_bytes())?;
                encoder.finish()?;
            }

            // upload using aws-sdk-s3
            let config = aws_config::load_from_env().await;
            let client = aws_sdk_s3::Client::new(&config);
            let bucket_name = bucket;
            let _ = client
                .put_object()
                .bucket(bucket_name)
                .key(&key)
                .body(Bytes::from(buf.clone()).into())
                .send()
                .await;

            let _ = sqlx::query(
                "INSERT INTO page_blobs (content_hash, s3_key, size) VALUES ($1, $2, $3)
                 ON CONFLICT (content_hash) DO NOTHING"
            )
            .bind(&page.content_hash)
            .bind(&key)
            .bind(buf.len() as i64)
            .execute(pool)
            .await?;
        }
    } else {
        // Fallback: save compressed full content to disk to reduce DB storage.
        let dir = std::path::Path::new("database/pages");
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
        }
        let path = dir.join(format!("{}.zst", page.content_hash));
        if !path.exists() {
            use std::fs::File;
            use std::io::Write;
            let f = File::create(&path)?;
            let mut encoder = zstd::stream::Encoder::new(f, 3)?;
            encoder.write_all(page.content.as_bytes())?;
            encoder.finish()?;
        }
    }


    // Prune or mark old pages can be handled separately; keep DB minimal here.
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

    // Optionally prune page_blobs and S3 objects
    if let Ok(bucket) = std::env::var("S3_BUCKET") {
        let rows: Vec<(String,String)> = sqlx::query_as("SELECT content_hash, s3_key FROM page_blobs WHERE uploaded_at < now() - ($1::interval)")
            .bind(format!("{} days", days))
            .fetch_all(pool)
            .await?;
        if !rows.is_empty() {
            let config = aws_config::load_from_env().await;
            let client = aws_sdk_s3::Client::new(&config);
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
