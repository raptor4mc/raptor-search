use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
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

use crawl::{crawl, store_page};
use db::init_db;

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

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL not set");
    let pool = init_db(&database_url)
        .await
        .expect("Failed to connect to database");
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

    let count: (i64,) =
        sqlx::query_as("SELECT count(*) FROM public.crawl_queue WHERE status = 'pending'")
            .fetch_one(pool.as_ref())
            .await
            .unwrap_or((0,));
    println!("Pending rows in queue: {}", count.0);

    let pool_crawler = pool.clone();
    tokio::spawn(async move {
        run_crawler(pool_crawler).await;
    });

    let state = AppState { db: pool };

    let app = Router::new()
        .route("/search", get(search))
        .route("/crawl", post(crawl_handler))
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
        let _ =
            sqlx::query("INSERT INTO crawl_queue (url) VALUES ($1) ON CONFLICT (url) DO NOTHING")
                .bind(url)
                .execute(pool)
                .await;
    }
    println!("Seeded {} URLs into crawl queue", SEED_URLS.len());
}

async fn run_crawler(pool: Arc<PgPool>) {
    let count: (i64,) = sqlx::query_as("SELECT count(*) FROM crawl_queue WHERE status = 'pending'")
        .fetch_one(pool.as_ref())
        .await
        .unwrap_or((0,));
    println!("Pending rows in queue: {}", count.0);
    loop {
        let row: Option<(i32, String)> = sqlx::query_as(
            "UPDATE crawl_queue
             SET status = 'processing', attempted_at = now()
             WHERE id = (
                 SELECT id FROM crawl_queue
                 WHERE status = 'pending'
                 ORDER BY id ASC
                 LIMIT 1
                 FOR UPDATE SKIP LOCKED
             )
             RETURNING id, url",
        )
        .fetch_optional(pool.as_ref())
        .await
        .unwrap_or(None);

        match row {
            Some((id, url)) => {
                println!("Crawling: {}", url);
                match crawl(&url).await {
                    Ok(page) => match store_page(pool.as_ref(), &url, &page).await {
                        Ok(_) => {
    let _ = sqlx::query(
        "UPDATE crawl_queue SET status = 'done' WHERE id = $1"
    )
    .bind(id)
    .execute(pool.as_ref())
    .await;
    println!("Done: {}", url);

    // Queue discovered URLs
    for discovered in &page.discovered_urls {
        let _ = sqlx::query(
            "INSERT INTO crawl_queue (url) VALUES ($1) ON CONFLICT (url) DO NOTHING"
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
                            eprintln!("Store error for {}: {:?}", url, e);
                            let _ = sqlx::query(
                                "UPDATE crawl_queue SET status = 'failed' WHERE id = $1",
                            )
                            .bind(id)
                            .execute(pool.as_ref())
                            .await;
                        }
                    },
                    Err(e) => {
                        eprintln!("Crawl error for {}: {}", url, e);
                        let _ =
                            sqlx::query("UPDATE crawl_queue SET status = 'failed' WHERE id = $1")
                                .bind(id)
                                .execute(pool.as_ref())
                                .await;
                    }
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
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT title, url, snippet
         FROM pages
         WHERE search_vector @@ websearch_to_tsquery('english', $1)
         ORDER BY ts_rank(search_vector, websearch_to_tsquery('english', $1)) DESC
         LIMIT 10",
    )
    .bind(&params.q)
    .fetch_all(state.db.as_ref())
    .await
    .unwrap_or_default();

    Json(
        rows.into_iter()
            .map(|(title, url, snippet)| ResultItem {
                title,
                url,
                snippet,
            })
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
    let _ = sqlx::query("INSERT INTO crawl_queue (url) VALUES ($1) ON CONFLICT (url) DO NOTHING")
        .bind(&payload.url)
        .execute(state.db.as_ref())
        .await;
    "Queued"
}
