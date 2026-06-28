
use reqwest;
use rusqlite::Connection;
use scraper::{Html, Selector};

pub struct CrawledPage {
    pub title: String,
    pub content: String,
}

pub async fn crawl(url: &str) -> Result<CrawledPage, Box<dyn std::error::Error>> {
    let body = reqwest::get(url).await?.text().await?;

    let document = Html::parse_document(&body);
    let selector = Selector::parse("title").unwrap();

    let title = document
        .select(&selector)
        .next()
        .map(|t| t.inner_html())
        .unwrap_or_else(|| "No title".to_string());

    let content = document
        .root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ");

    Ok(CrawledPage { title, content })
}

pub fn store_page(
    conn: &Connection,
    url: &str,
    page: &CrawledPage,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO pages (url, title, content)
         VALUES (?1, ?2, ?3)",
        (&url, &page.title, &page.content),
    )?;

    Ok(())
}
