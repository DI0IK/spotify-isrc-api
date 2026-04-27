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

fn clean_spotify_id(input: &str) -> String {
    let no_params = input.split('?').next().unwrap_or(input);
    let no_slashes = no_params.split('/').last().unwrap_or(no_params);
    no_slashes
        .split(':')
        .last()
        .unwrap_or(no_slashes)
        .to_string()
}

// --- ENDPOINTS ---

// 1. Health Check
async fn health_check() -> &'static str {
    "OK"
}

// 2. Resolve ISRC from Spotify ID
async fn resolve_isrc(
    Path(raw_id): Path<String>,
    State(pool): State<SqlitePool>,
) -> impl IntoResponse {
    let clean_id = clean_spotify_id(&raw_id);

    // To use the index, we check if the ID is between 'abc' and 'abc{ '
    // (the curly brace '{' is the character immediately following 'z' and 'Z' in ASCII/UTF-8)
    let end_range = format!("{}{}", clean_id, "{");

    let query = r#"
        SELECT 
            t.id as spotify_id,
            t.name as title, 
            t.external_id_isrc as isrc,
            GROUP_CONCAT(a.name, ', ') as artist
        FROM tracks t
        LEFT JOIN track_artists ta ON t.rowid = ta.track_rowid
        LEFT JOIN artists a ON ta.artist_rowid = a.rowid
        WHERE t.id >= ? AND t.id < ?
        GROUP BY t.id
        LIMIT 1
    "#;

    match sqlx::query_as::<_, TrackData>(query)
        .bind(&clean_id)
        .bind(&end_range)
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
    let id_pairs: Vec<(String, String)> = params
        .ids
        .split(',')
        .map(|s| {
            let start = clean_spotify_id(s.trim());
            let end = format!("{}{}", start, "{");
            (start, end)
        })
        .filter(|(s, _)| !s.is_empty())
        .collect();

    if id_pairs.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "No IDs provided"})),
        )
            .into_response();
    }

    // Using a Common Table Expression (CTE) to find the IDs first, then join.
    // This is significantly faster for batch operations.
    let mut query_builder = QueryBuilder::new(
        r#"
        WITH selected_tracks AS (
            SELECT t.rowid, t.id as spotify_id, t.name as title, t.external_id_isrc as isrc
            FROM tracks t
            WHERE 
    "#,
    );

    let mut separated = query_builder.separated(" OR ");
    for (start, end) in id_pairs {
        separated.push("(t.id >= ");
        separated.push_bind_unseparated(start);
        separated.push_unseparated(" AND t.id < ");
        separated.push_bind_unseparated(end);
        separated.push_unseparated(")");
    }

    query_builder.push(
        r#"
        )
        SELECT 
            st.spotify_id, 
            st.title, 
            st.isrc,
            GROUP_CONCAT(a.name, ', ') as artist
        FROM selected_tracks st
        LEFT JOIN track_artists ta ON st.rowid = ta.track_rowid
        LEFT JOIN artists a ON ta.artist_rowid = a.rowid
        GROUP BY st.spotify_id
    "#,
    );

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
        //.route("/api/spotify/:isrc", get(reverse_lookup))
        //.route("/api/search", get(search_tracks))
        //.route("/api/isrc/batch", get(batch_resolve))
        .with_state(pool);

    let port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("0.0.0.0:{}", port);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("Spotify Metadata API running on http://{}", addr);
    axum::serve(listener, app).await.unwrap();
}
