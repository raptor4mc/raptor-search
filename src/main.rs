use axum::debug_handler;
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
};

mod crawl;
mod db;

use crawl::{crawl, store_page};
use db::init_db;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<rusqlite::Connection>>,
}

#[tokio::main]
async fn main() {
    let conn = init_db().unwrap();

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
    };

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
    let conn = state.db.lock().unwrap();

    let pattern = format!("%{}%", params.q.to_lowercase());

    let mut stmt = conn
        .prepare(
            "SELECT title, url, content
             FROM pages
             WHERE lower(title) LIKE ?1
                OR lower(content) LIKE ?1
             LIMIT 10",
        )
        .unwrap();

    let rows = stmt
        .query_map([pattern], |row| {
            let content: String = row.get(2)?;
            Ok(ResultItem {
                title: row.get(0)?,
                url: row.get(1)?,
                snippet: content.chars().take(160).collect(),
            })
        })
        .unwrap();

    let mut results = vec![];

    for r in rows {
        if let Ok(item) = r {
            results.push(item);
        }
    }

    Json(results)
}

#[derive(Deserialize)]
struct CrawlRequest {
    url: String,
}

async fn crawl_handler(
    State(state): State<AppState>,
    Json(payload): Json<CrawlRequest>,
) -> &'static str {
    let url = payload.url;

    // Network first (no DB lock)
    let page = match crawl(&url).await {
        Ok(page) => page,
        Err(err) => {
            eprintln!("crawl failed: {err}");
            return "Failed";
        }
    };

    // Lock only while writing to SQLite
    let conn = state.db.lock().unwrap();

    if let Err(err) = store_page(&conn, &url, &page) {
        eprintln!("db error: {err}");
        return "Failed";
    }

    "OK"
}
