use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::env;

// Handler to fetch ISRC from the local SQLite database
async fn resolve_isrc(
    Path(spotify_id): Path<String>,
    State(pool): State<SqlitePool>,
) -> impl IntoResponse {
    let result: Result<Option<String>, sqlx::Error> =
        sqlx::query_scalar("SELECT external_id_isrc FROM tracks WHERE id = ?")
            .bind(&spotify_id)
            .fetch_optional(&pool)
            .await;

    match result {
        Ok(Some(isrc)) => (
            StatusCode::OK,
            Json(serde_json::json!({"spotify_id": spotify_id, "isrc": isrc})),
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
