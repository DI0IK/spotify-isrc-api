use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use sqlx::{
    sqlite::{SqlitePool, SqlitePoolOptions},
    QueryBuilder,
};
use std::env;

// --- DATA STRUCTS ---

#[derive(Serialize, sqlx::FromRow)]
struct TrackData {
    spotify_id: Option<String>,
    title: String,
    artist: Option<String>,
    isrc: Option<String>,
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
}

#[derive(Deserialize)]
struct BatchParams {
    ids: String,
}

// --- ENDPOINTS ---

// 1. Health Check
async fn health_check() -> &'static str {
    "OK"
}

// 2. Resolve ISRC from Spotify ID
async fn resolve_isrc(
    Path(spotify_id): Path<String>,
    State(pool): State<SqlitePool>,
) -> impl IntoResponse {
    let query = r#"
        SELECT 
            t.id as spotify_id,
            t.name as title, 
            t.external_id_isrc as isrc,
            GROUP_CONCAT(a.name, ', ') as artist
        FROM tracks t
        LEFT JOIN track_artists ta ON t.rowid = ta.track_rowid
        LEFT JOIN artists a ON ta.artist_rowid = a.rowid
        WHERE t.id = ?
        GROUP BY t.id
    "#;

    match sqlx::query_as::<_, TrackData>(query)
        .bind(&spotify_id)
        .fetch_optional(&pool)
        .await
    {
        Ok(Some(data)) => (StatusCode::OK, Json(data)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Track not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// 3. Reverse Lookup (Get Spotify ID from ISRC)
async fn reverse_lookup(
    Path(isrc): Path<String>,
    State(pool): State<SqlitePool>,
) -> impl IntoResponse {
    let query = r#"
        SELECT 
            t.id as spotify_id,
            t.name as title, 
            t.external_id_isrc as isrc,
            GROUP_CONCAT(a.name, ', ') as artist
        FROM tracks t
        LEFT JOIN track_artists ta ON t.rowid = ta.track_rowid
        LEFT JOIN artists a ON ta.artist_rowid = a.rowid
        WHERE t.external_id_isrc = ?
        GROUP BY t.id
    "#;

    match sqlx::query_as::<_, TrackData>(query)
        .bind(&isrc)
        .fetch_optional(&pool)
        .await
    {
        Ok(Some(data)) => (StatusCode::OK, Json(data)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "ISRC not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// 4. Fuzzy Search (Find tracks by title or artist)
async fn search_tracks(
    Query(params): Query<SearchParams>,
    State(pool): State<SqlitePool>,
) -> impl IntoResponse {
    let fuzzy_query = format!("%{}%", params.q);

    let query = r#"
        SELECT 
            t.id as spotify_id,
            t.name as title, 
            t.external_id_isrc as isrc,
            GROUP_CONCAT(a.name, ', ') as artist
        FROM tracks t
        LEFT JOIN track_artists ta ON t.rowid = ta.track_rowid
        LEFT JOIN artists a ON ta.artist_rowid = a.rowid
        WHERE t.name LIKE ? OR a.name LIKE ?
        GROUP BY t.id
        LIMIT 20
    "#;

    match sqlx::query_as::<_, TrackData>(query)
        .bind(&fuzzy_query)
        .bind(&fuzzy_query)
        .fetch_all(&pool)
        .await
    {
        Ok(results) => (StatusCode::OK, Json(results)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// 5. Batch Resolution (GET version using comma-separated IDs)
async fn batch_resolve(
    Query(params): Query<BatchParams>,
    State(pool): State<SqlitePool>,
) -> impl IntoResponse {
    if params.ids.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "No IDs provided"})),
        )
            .into_response();
    }

    let spotify_ids: Vec<&str> = params
        .ids
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    if spotify_ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "No valid IDs provided"})),
        )
            .into_response();
    }

    let mut query_builder = QueryBuilder::new(
        r#"
        SELECT 
            t.id as spotify_id,
            t.name as title, 
            t.external_id_isrc as isrc,
            GROUP_CONCAT(a.name, ', ') as artist
        FROM tracks t
        LEFT JOIN track_artists ta ON t.rowid = ta.track_rowid
        LEFT JOIN artists a ON ta.artist_rowid = a.rowid
        WHERE t.id IN (
    "#,
    );

    let mut separated = query_builder.separated(", ");
    for id in spotify_ids {
        separated.push_bind(id);
    }
    separated.push_unseparated(") GROUP BY t.id");

    let query = query_builder.build_query_as::<TrackData>();

    match query.fetch_all(&pool).await {
        Ok(results) => (StatusCode::OK, Json(results)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// --- MAIN SERVER LOGIC ---

#[tokio::main]
async fn main() {
    let db_url = env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://spotify.db".to_string());
    println!("Connecting to database at: {}", db_url);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .expect("Failed to connect to SQLite database");

    // Wire up the pure metadata router
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/api/isrc/:id", get(resolve_isrc))
        .route("/api/spotify/:isrc", get(reverse_lookup))
        .route("/api/search", get(search_tracks))
        .route("/api/isrc/batch", get(batch_resolve))
        .with_state(pool);

    let port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("0.0.0.0:{}", port);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("Spotify Metadata API running on http://{}", addr);
    axum::serve(listener, app).await.unwrap();
}
