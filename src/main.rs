use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::Serialize;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::env; // Make sure Serialize is imported

// 1. Create a struct that matches your desired JSON output
// sqlx::FromRow automatically maps the SQL SELECT columns to these fields
#[derive(Serialize, sqlx::FromRow)]
struct TrackData {
    title: String,
    artist: Option<String>, // Option handles potential NULL values in the DB
    isrc: Option<String>,
}

// Handler to fetch Title, Artist, and ISRC
async fn resolve_isrc(
    Path(spotify_id): Path<String>,
    State(pool): State<SqlitePool>,
) -> impl IntoResponse {
    // 2. The SQL Query
    // Note: Since 'artist' isn't in your 'tracks' table, we use a placeholder.
    // If you have an artists table, change this to something like:
    // SELECT t.name as title, t.external_id_isrc as isrc, a.name as artist
    // FROM tracks t LEFT JOIN artists a ON ... WHERE t.id = ?
    let query = r#"
        SELECT 
            name as title, 
            external_id_isrc as isrc,
            'Unknown Artist' as artist 
        FROM tracks 
        WHERE id = ?
    "#;

    // 3. Execute query and map the result directly to the TrackData struct
    let result = sqlx::query_as::<_, TrackData>(query)
        .bind(&spotify_id)
        .fetch_optional(&pool)
        .await;

    match result {
        Ok(Some(track_data)) => (
            StatusCode::OK,
            Json(track_data), // Axum automatically turns the struct into JSON!
        )
            .into_response(),

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

// A simple health check endpoint (Useful for Docker/Unraid status checks)
async fn health_check() -> &'static str {
    "OK"
}

#[tokio::main]
async fn main() {
    // Read the DB URL from the environment (Passed by Unraid)
    let db_url =
        env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://data/spotify.db".to_string());

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

    // Read the port from the environment (Default to 3000)
    let port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("0.0.0.0:{}", port);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("ISRC API running on http://{}", addr);
    axum::serve(listener, app).await.unwrap();
}
