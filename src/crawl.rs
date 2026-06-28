use aws_config::Region;
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, Config as S3Config};
use scraper::{Html, Selector};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "in", "on", "at", "to", "for", "of", "with", "by",
    "from", "is", "it", "its", "was", "are", "be", "been", "has", "had", "have", "will", "would",
    "could", "should", "may", "might", "do", "does", "did", "not", "no", "so", "if", "as", "up",
    "out", "about", "into", "than", "then", "that", "this", "these", "those", "they", "them",
    "their", "there", "when", "where", "which", "who", "how", "what", "we", "you", "i", "he",
    "she", "my", "your", "our", "can", "also",
];

pub struct CrawledPage {
    pub title: String,
    pub snippet: String,
    pub filtered_content: String,
    pub content_hash: String,
    pub discovered_urls: Vec<String>,
}

pub fn build_s3_client() -> S3Client {
    let key_id = std::env::var("B2_KEY_ID").expect("B2_KEY_ID not set");
    let app_key = std::env::var("B2_APPLICATION_KEY").expect("B2_APPLICATION_KEY not set");
    let endpoint = std::env::var("B2_ENDPOINT").expect("B2_ENDPOINT not set");

    let creds = Credentials::new(&key_id, &app_key, None, None, "env");
    let config = S3Config::builder()
        .credentials_provider(creds)
        .region(Region::new("auto"))
        .endpoint_url(endpoint)
        .behavior_version_latest()
        .build();

    S3Client::from_conf(config)
}

pub async fn crawl(url: &str) -> Result<CrawledPage, Box<dyn std::error::Error + Send + Sync>> {
    let body = reqwest::get(url).await?.text().await?;
    let document = Html::parse_document(&body);

    let title_sel = Selector::parse("title").unwrap();
    let title = document
        .select(&title_sel)
        .next()
        .map(|t| t.inner_html())
        .unwrap_or_else(|| "No title".to_string());

    let raw_text: String = document.root_element().text().collect::<Vec<_>>().join(" ");

    let filtered_content = filter_stopwords(&raw_text);
    let snippet = filtered_content.chars().take(200).collect();

    let mut hasher = Sha256::new();
    hasher.update(raw_text.as_bytes());
    let content_hash = hex::encode(hasher.finalize());

    // Extract links
    let link_sel = Selector::parse("a[href]").unwrap();
    let base_url = reqwest::Url::parse(url)?;
    let mut discovered_urls = Vec::new();

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

            // Only keep URLs with rust or crate in them
            let lower = absolute.to_lowercase();
            if (lower.contains("rust") || lower.contains("crate"))
                && !lower.contains('#')
                && absolute.starts_with("http")
            {
                discovered_urls.push(absolute);
            }
        }
    }

    Ok(CrawledPage {
        title,
        snippet,
        filtered_content,
        content_hash,
        discovered_urls,
    })
}

fn filter_stopwords(text: &str) -> String {
    text.split_whitespace()
        .filter(|word| {
            let lower = word.to_lowercase();
            let clean: String = lower.chars().filter(|c| c.is_alphabetic()).collect();
            !clean.is_empty() && !STOPWORDS.contains(&clean.as_str())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn store_page(
    pool: &PgPool,
    _s3: &S3Client,
    url: &str,
    page: &CrawledPage,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

    Ok(())
}
