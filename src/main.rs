use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::Serialize;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::env;

// The struct perfectly matches our new SQL output
#[derive(Serialize, sqlx::FromRow)]
struct TrackData {
    title: String,
    artist: Option<String>,
    isrc: Option<String>,
}

// Handler to fetch Title, Artist, and ISRC
async fn resolve_isrc(
    Path(spotify_id): Path<String>,
    State(pool): State<SqlitePool>,
) -> impl IntoResponse {
    // We join the three tables together using their rowids.
    // GROUP_CONCAT ensures that if a track has multiple artists,
    // they are combined into one string like "Daft Punk, Pharrell Williams"
    let query = r#"
        SELECT 
            t.name as title, 
            t.external_id_isrc as isrc,
            GROUP_CONCAT(a.name, ', ') as artist
        FROM tracks t
        LEFT JOIN track_artists ta ON t.rowid = ta.track_rowid
        LEFT JOIN artists a ON ta.artist_rowid = a.rowid
        WHERE t.id = ?
        GROUP BY t.id
    "#;

    // Execute query and map the result directly to the TrackData struct
    let result = sqlx::query_as::<_, TrackData>(query)
        .bind(&spotify_id)
        .fetch_optional(&pool)
        .await;

    match result {
        Ok(Some(track_data)) => (StatusCode::OK, Json(track_data)).into_response(),

        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Track not found in local database"})),
        )
            .into_response(),

        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// A simple health check endpoint
async fn health_check() -> &'static str {
    "OK"
}

#[tokio::main]
async fn main() {
    // Read the DB URL from the environment
    let db_url = env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://spotify.db".to_string());

    println!("Connecting to database at: {}", db_url);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .expect("Failed to connect to SQLite database");

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/api/isrc/:id", get(resolve_isrc))
        .with_state(pool);

    let port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("0.0.0.0:{}", port);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("ISRC API running on http://{}", addr);
    axum::serve(listener, app).await.unwrap();
}
