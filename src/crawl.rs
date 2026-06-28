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

const BAD_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".svg", ".ico", ".zip", ".tar",
    ".pdf", ".woff", ".woff2", ".ttf", ".css", ".js", ".xml", ".json",
    ".mp4", ".mp3", ".webm", ".wav", ".otf", ".eot", ".exe", ".dmg",
    ".gz", ".rar", ".7z", ".iso",
];

// Don't extract links FROM these domains
// (we still crawl them if linked to, just don't follow their links)
const NO_FOLLOW_DOMAINS: &[&str] = &[
    "twitter.com", "x.com", "facebook.com", "instagram.com",
    "tiktok.com", "linkedin.com", "pinterest.com", "snapchat.com",
    "youtube.com", "youtu.be", "twitch.tv", "discord.com",
    "discord.gg", "t.me", "telegram.org", "whatsapp.com",
    "reddit.com", "news.ycombinator.com", "medium.com",
];

pub struct CrawledPage {
    pub title: String,
    pub snippet: String,
    pub content_hash: String,
    pub discovered_urls: Vec<String>,
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
    let filtered = filter_stopwords(&raw_text);
    let snippet: String = filtered.chars().take(200).collect();

    let mut hasher = Sha256::new();
    hasher.update(raw_text.as_bytes());
    let content_hash = hex::encode(hasher.finalize());

    let link_sel = Selector::parse("a[href]").unwrap();
    let base_url = reqwest::Url::parse(url)?;
    let mut discovered_urls = Vec::new();

    // Don't follow links from social media / video sites
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
                {
                    discovered_urls.push(absolute);
                }
            }
        }
    }

    Ok(CrawledPage {
        title,
        snippet,
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
