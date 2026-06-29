use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
};

mod crawl;
mod db;

use crawl::{crawl, is_junk_url, store_page, storage_worker, StorageTask};
use db::init_db;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

const SEED_URLS: &[&str] = &[
    "https://www.rust-lang.org",
    "https://doc.rust-lang.org/book/",
    "https://doc.rust-lang.org/std/",
    "https://doc.rust-lang.org/reference/",
    "https://doc.rust-lang.org/nomicon/",
    "https://doc.rust-lang.org/rust-by-example/",
    "https://docs.rs",
    "https://crates.io",
    "https://this-week-in-rust.org",
    "https://blog.rust-lang.org",
    "https://users.rust-lang.org",
    "https://internals.rust-lang.org",
    "https://github.com/rust-lang/rust",
    "https://github.com/rust-lang/cargo",
    "https://reddit.com/r/rust.json",
];

#[derive(Clone)]
struct AppState {
    db: Arc<PgPool>,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    if let Ok(secs) = std::env::var("CRAWL_TIMEOUT_SECS") {
        if let Ok(secs) = secs.parse::<u64>() {
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(secs)).await;
                println!("Crawl timeout reached, shutting down.");
                std::process::exit(0);
            });
        }
    }

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL not set");
    let pool = init_db(&database_url).await.expect("Failed to connect to database");
    let pool = Arc::new(pool);

    seed_crawl_queue(&pool).await;

    let db: (String,) = sqlx::query_as("SELECT current_database()")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    println!("Connected to database: {}", db.0);

    let schema: (String,) = sqlx::query_as("SELECT current_schema()")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    println!("Current schema: {}", schema.0);

    let count: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM public.crawl_queue WHERE status = 'pending'"
    )
    .fetch_one(pool.as_ref())
    .await
    .unwrap_or((0,));
    println!("Pending rows in queue: {}", count.0);

    let search_only = std::env::var("SEARCH_ONLY").unwrap_or_default() == "true";

    if !search_only {
        let pool_crawler = pool.clone();
        
        // Create channel for background storage tasks
        let (storage_tx, storage_rx) = mpsc::unbounded_channel();
        
        // Spawn background storage worker
        let pool_storage = pool.clone();
        tokio::spawn(async move {
            storage_worker(pool_storage, storage_rx).await;
        });
        
        // Per-host last access timestamps for politeness
        let host_access: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let host_semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>> = Arc::new(Mutex::new(HashMap::new()));
        let workers: usize = std::env::var("CRAWL_WORKERS").ok().and_then(|s| s.parse().ok()).unwrap_or(16);

        for _ in 0..workers {
            let pool_worker = pool_crawler.clone();
            let host_access = host_access.clone();
            let host_semaphores = host_semaphores.clone();
            let storage_tx = storage_tx.clone();
            tokio::spawn(async move {
                run_crawler_with_politeness(pool_worker, host_access, host_semaphores, storage_tx).await;
            });
        }
    }

    let state = AppState { db: pool };

    let app = Router::new()
        .route("/search", get(search))
        .route("/crawl", post(crawl_handler))
        .route("/admin/prune", post(prune_handler))
        .fallback_service(ServeDir::new("static").append_index_html_on_directories(true))
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Server running on http://localhost:3000");
    axum::serve(listener, app).await.unwrap();
}

async fn seed_crawl_queue(pool: &PgPool) {
    for url in SEED_URLS {
        if let Some(n) = crate::crawl::normalize_url(url) {
            let _ = sqlx::query(
                "INSERT INTO crawl_queue (url) VALUES ($1) ON CONFLICT (url) DO NOTHING"
            )
            .bind(n)
            .execute(pool)
            .await;
        }
    }
}

async fn run_crawler_with_politeness(
    pool: Arc<PgPool>,
    host_access: Arc<Mutex<HashMap<String, Instant>>>,
    host_semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    storage_tx: mpsc::UnboundedSender<StorageTask>,
) {
    let politeness = Duration::from_secs(1);
    loop {
        // Atomically claim a pending row to avoid races between workers.
        // Claim a pending row, or retry failed rows that are old enough (visibility/backoff)
        let row: Option<(i32, String)> = sqlx::query_as(
            "UPDATE crawl_queue SET status = 'processing', attempted_at = now() WHERE id = (
                SELECT id FROM crawl_queue WHERE (status = 'pending' OR (status = 'failed' AND attempted_at < now() - INTERVAL '1 hour')) ORDER BY id ASC LIMIT 1
            ) RETURNING id, url"
        )
        .fetch_optional(pool.as_ref())
        .await
        .unwrap_or(None);

        match row {
            Some((id, url)) => {
                // Skip junk URLs
                if is_junk_url(&url) {
                    let _ = sqlx::query(
                        "UPDATE crawl_queue SET status = 'done' WHERE id = $1"
                    )
                    .bind(id)
                    .execute(pool.as_ref())
                    .await;
                    continue;
                }

                // politeness: per-host delay + per-host concurrency semaphore
                if let Ok(parsed) = reqwest::Url::parse(&url) {
                    if let Some(host) = parsed.host_str() {
                        // get or create semaphore for host
                        let sem = {
                            let mut sems = host_semaphores.lock().await;
                            sems.entry(host.to_string())
                                .or_insert_with(|| Arc::new(Semaphore::new(2)))
                                .clone()
                        };

                        // acquire permit
                        let permit = sem.acquire_owned().await.unwrap();

                        let mut map = host_access.lock().await;
                        if let Some(last) = map.get(host) {
                            let elapsed = last.elapsed();
                            if elapsed < politeness {
                                let wait = politeness - elapsed;
                                tokio::time::sleep(wait).await;
                            }
                        }
                        map.insert(host.to_string(), Instant::now());

                        println!("Crawling: {}", url);
                        let crawl_result = crawl(&url).await;

                        // permit drops here when out of scope
                        drop(permit);
                        // continue handling result below
                        match crawl_result {
                            Ok(page) => {
                                let store_result = store_page(pool.as_ref(), &url, &page, Some(storage_tx.clone())).await;
                                match store_result {
                                    Ok(_) => {
                                        let _ = sqlx::query(
                                            "UPDATE crawl_queue SET status = 'done' WHERE id = $1"
                                        )
                                        .bind(id)
                                        .execute(pool.as_ref())
                                        .await;

                                        println!("Done: {}", url);

                                        for discovered in &page.discovered_urls {
                                            let _ = sqlx::query(
                                                "INSERT INTO crawl_queue (url) VALUES ($1) ON CONFLICT (url) DO NOTHING"
                                            )
                                            .bind(discovered)
                                            .execute(pool.as_ref())
                                            .await;

                                            // PageRank: increment inbound links
                                            let _ = sqlx::query(
                                                "UPDATE pages SET inbound_links = inbound_links + 1 WHERE url = $1 OR url = rtrim($1, '/')"
                                            )
                                            .bind(discovered)
                                            .execute(pool.as_ref())
                                            .await;
                                        }

                                        if !page.discovered_urls.is_empty() {
                                            println!("Queued {} new URLs from {}", page.discovered_urls.len(), url);
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("Store error for {}: {}", url, e);
                                        let _ = sqlx::query(
                                            "UPDATE crawl_queue SET status = 'failed' WHERE id = $1"
                                        )
                                        .bind(id)
                                        .execute(pool.as_ref())
                                        .await;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("Crawl error for {}: {}", url, e);
                                drop(e);
                                let _ = sqlx::query(
                                    "UPDATE crawl_queue SET status = 'failed' WHERE id = $1"
                                )
                                .bind(id)
                                .execute(pool.as_ref())
                                .await;
                            }
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        continue;
                    }
                }
                // If we couldn't parse the URL host or didn't handle above, do a simple fetch without semaphore.
                println!("Crawling (ungarded): {}", url);
                let crawl_result = crawl(&url).await;
                if let Ok(page) = crawl_result {
                    let store_result = store_page(pool.as_ref(), &url, &page, Some(storage_tx.clone())).await;
                    if store_result.is_ok() {
                        let _ = sqlx::query(
                            "UPDATE crawl_queue SET status = 'done' WHERE id = $1"
                        )
                        .bind(id)
                        .execute(pool.as_ref())
                        .await;
                        println!("Done: {}", url);
                    } else {
                        let _ = sqlx::query(
                            "UPDATE crawl_queue SET status = 'failed' WHERE id = $1"
                        )
                        .bind(id)
                        .execute(pool.as_ref())
                        .await;
                    }
                } else if let Err(e) = crawl_result {
                    eprintln!("Crawl error for {}: {}", url, e);
                    let _ = sqlx::query(
                        "UPDATE crawl_queue SET status = 'failed' WHERE id = $1"
                    )
                    .bind(id)
                    .execute(pool.as_ref())
                    .await;
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
            None => {
                println!("Queue empty, waiting 60s...");
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            }
        }
    }
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    page: Option<i64>,
}

#[derive(Serialize)]
struct ResultItem {
    title: String,
    url: String,
    snippet: String,
}

async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> Json<Vec<ResultItem>> {
    let page = params.page.unwrap_or(0);
    let offset = page * 10;

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT title, url, snippet
         FROM pages
         WHERE search_vector @@ websearch_to_tsquery('english', $1)
         ORDER BY
             ts_rank(search_vector, websearch_to_tsquery('english', $1))
             * log(2 + inbound_links) DESC
         LIMIT 10 OFFSET $2"
    )
    .bind(&params.q)
    .bind(offset)
    .fetch_all(state.db.as_ref())
    .await
    .unwrap_or_default();

    Json(
        rows.into_iter()
            .map(|(title, url, snippet)| ResultItem { title, url, snippet })
            .collect(),
    )
}

#[derive(Deserialize)]
struct CrawlRequest {
    url: String,
}

async fn crawl_handler(
    State(state): State<AppState>,
    Json(payload): Json<CrawlRequest>,
) -> &'static str {
    let _ = sqlx::query(
        "INSERT INTO crawl_queue (url) VALUES ($1) ON CONFLICT (url) DO NOTHING"
    )
    .bind(&payload.url)
    .execute(state.db.as_ref())
    .await;
    "Queued"
}

async fn prune_handler(
    State(state): State<AppState>,
) -> Json<String> {
    match crate::crawl::prune_old_pages(state.db.as_ref()).await {
        Ok(n) => Json(format!("Pruned {} pages", n)),
        Err(e) => Json(format!("Prune failed: {}", e)),
    }
}
