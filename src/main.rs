use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use musicbrainz_rs::entity::recording::Recording;
use musicbrainz_rs::entity::relations::RelationContent;
use musicbrainz_rs::{Fetch, Search};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::env;

#[derive(Serialize, sqlx::FromRow, Debug, Clone)]
struct TrackData {
    pub spotify_id: Option<String>,
    pub title: String,
    pub artist: Option<String>,
    pub isrc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[sqlx(default)]
    pub youtube_url: Option<String>,
}

#[derive(Deserialize)]
struct ResolveParams {
    mbz_yt: Option<bool>,
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

async fn fetch_mbz_youtube(isrc: &str) -> Option<String> {
    let query = format!("isrc:{}", isrc);
    let mut search_query = Recording::search(query);
    let search_result = search_query.execute_async().await.ok()?;

    if let Some(recording) = search_result.entities.first() {
        let mut fetch_query = Recording::fetch();
        let details = fetch_query
            .id(&recording.id)
            .with_url_relations()
            .execute_async()
            .await
            .ok()?;

        if let Some(relations) = details.relations {
            return relations.iter().find_map(|rel| match &rel.content {
                RelationContent::Url(url)
                    if url.resource.contains("youtube.com")
                        || url.resource.contains("youtu.be") =>
                {
                    Some(url.resource.clone())
                }
                _ => None,
            });
        }
    }
    None
}

async fn resolve_isrc(
    Path(raw_id): Path<String>,
    Query(params): Query<ResolveParams>,
    State(pool): State<SqlitePool>,
) -> impl IntoResponse {
    let clean_id = clean_spotify_id(&raw_id);
    let end_range = format!("{}{}", clean_id, "{");

    let query = r#"
        SELECT t.id as spotify_id, t.name as title, t.external_id_isrc as isrc,
        GROUP_CONCAT(a.name, ', ') as artist
        FROM tracks t
        LEFT JOIN track_artists ta ON t.rowid = ta.track_rowid
        LEFT JOIN artists a ON ta.artist_rowid = a.rowid
        WHERE t.id >= ? AND t.id < ?
        GROUP BY t.id LIMIT 1
    "#;

    match sqlx::query_as::<_, TrackData>(query)
        .bind(&clean_id)
        .bind(&end_range)
        .fetch_optional(&pool)
        .await
    {
        Ok(Some(mut data)) => {
            if params.mbz_yt.unwrap_or(false) {
                if let Some(ref isrc_val) = data.isrc {
                    data.youtube_url = fetch_mbz_youtube(isrc_val).await;
                }
            }
            (StatusCode::OK, Json(data)).into_response()
        }
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

#[tokio::main]
async fn main() {
    let db_url = env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://spotify.db".to_string());
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .expect("DB error");

    let app = Router::new()
        .route("/api/isrc/:id", get(resolve_isrc))
        .with_state(pool);
    let port = env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
