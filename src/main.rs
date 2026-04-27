use anyhow::Context;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use rspotify::{model::FullTrack, prelude::*, ClientCredsSpotify, Credentials};
use serde::Serialize;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::env;
use std::time::Duration;

// --- Schema Definition ---
const SYNC_DB_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS `sync_queue` (
  `id` TEXT PRIMARY KEY NOT NULL,
  `status` TEXT NOT NULL DEFAULT 'queued'
);

CREATE TABLE IF NOT EXISTS `available_markets` (
  `rowid` integer PRIMARY KEY NOT NULL,
  `available_markets` text NOT NULL
);

CREATE TABLE IF NOT EXISTS `artists` (
  `rowid` integer PRIMARY KEY NOT NULL,
  `id` text NOT NULL,
  `fetched_at` integer NOT NULL,
  `name` text NOT NULL,
  `followers_total` integer NOT NULL,
  `popularity` integer NOT NULL
);

CREATE TABLE IF NOT EXISTS `albums` (
  `rowid` integer PRIMARY KEY NOT NULL,
  `id` text NOT NULL,
  `fetched_at` integer NOT NULL,
  `name` text NOT NULL,
  `album_type` text NOT NULL,
  `available_markets_rowid` integer NOT NULL,
  `label` text NOT NULL,
  `popularity` integer NOT NULL,
  `release_date` text NOT NULL,
  `release_date_precision` text NOT NULL,
  `total_tracks` integer NOT NULL,
  FOREIGN KEY (`available_markets_rowid`) REFERENCES `available_markets`(`rowid`)
);

CREATE TABLE IF NOT EXISTS `tracks` (
  `rowid` integer PRIMARY KEY NOT NULL,
  `id` text NOT NULL,
  `fetched_at` integer NOT NULL,
  `name` text NOT NULL,
  `preview_url` text,
  `album_rowid` integer NOT NULL,
  `track_number` integer NOT NULL,
  `external_id_isrc` text,
  `popularity` integer NOT NULL,
  `available_markets_rowid` integer NOT NULL,
  `disc_number` integer NOT NULL,
  `duration_ms` integer NOT NULL,
  `explicit` integer NOT NULL,
  FOREIGN KEY (`available_markets_rowid`) REFERENCES `available_markets`(`rowid`)
);

CREATE TABLE IF NOT EXISTS `track_artists` (
  `track_rowid` integer NOT NULL,
  `artist_rowid` integer NOT NULL,
  FOREIGN KEY (`track_rowid`) REFERENCES `tracks`(`rowid`),
  FOREIGN KEY (`artist_rowid`) REFERENCES `artists`(`rowid`)
);

CREATE TABLE IF NOT EXISTS `album_images` (
  `album_rowid` integer NOT NULL,
  `width` integer NOT NULL,
  `height` integer NOT NULL,
  `url` text NOT NULL,
  FOREIGN KEY (`album_rowid`) REFERENCES `albums`(`rowid`)
);

CREATE INDEX IF NOT EXISTS `tracks_id_unique` ON `tracks` (`id`);
CREATE INDEX IF NOT EXISTS `track_artists_track_id` ON `track_artists` (`track_rowid`);
CREATE INDEX IF NOT EXISTS `album_images_album_id` ON `album_images` (`album_rowid`);
"#;

const JSON_RECONSTRUCT_QUERY: &str = r#"
    SELECT json_object(
        'id', t.id,
        'name', t.name,
        'popularity', t.popularity,
        'track_number', t.track_number,
        'disc_number', t.disc_number,
        'duration_ms', t.duration_ms,
        'explicit', CASE WHEN t.explicit = 1 THEN json('true') ELSE json('false') END,
        'preview_url', t.preview_url,
        'external_ids', json_object('isrc', t.external_id_isrc),
        'album', (
            SELECT json_object(
                'id', al.id,
                'name', al.name,
                'album_type', al.album_type,
                'release_date', al.release_date,
                'release_date_precision', al.release_date_precision,
                'total_tracks', al.total_tracks,
                'images', (
                    SELECT json_group_array(json_object(
                        'url', img.url, 'width', img.width, 'height', img.height
                    )) FROM album_images img WHERE img.album_rowid = al.rowid
                )
            ) FROM albums al WHERE al.rowid = t.album_rowid
        ),
        'artists', (
            SELECT json_group_array(json_object(
                'id', art.id,
                'name', art.name
            )) FROM artists art
            JOIN track_artists ta ON art.rowid = ta.artist_rowid
            WHERE ta.track_rowid = t.rowid
        )
    ) as spotify_data
    FROM tracks t
    WHERE t.id >= ? AND t.id < ?
    LIMIT 1
"#;

#[derive(Serialize, sqlx::FromRow)]
struct SpotifyTrackExport {
    #[sqlx(json)]
    pub spotify_data: serde_json::Value,
}

#[derive(Clone)]
struct AppState {
    legacy_pool: SqlitePool,
    sync_pool: SqlitePool,
}

// --- API Handler ---

fn clean_spotify_id(input: &str) -> String {
    input
        .split('?')
        .next()
        .unwrap_or(input)
        .split('/')
        .last()
        .unwrap_or(input)
        .split(':')
        .last()
        .unwrap_or(input)
        .to_string()
}

fn with_sqlite_mode_if_missing(url: &str, mode: &str) -> String {
    if !url.starts_with("sqlite:") || url.contains("mode=") {
        return url.to_string();
    }

    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}mode={mode}")
}

async fn resolve_track(
    Path(raw_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let clean_id = clean_spotify_id(&raw_id);
    let end_range = format!("{}{}", clean_id, "{");

    // 1. Check Legacy (Read-Only)
    if let Ok(Some(row)) = sqlx::query_as::<_, SpotifyTrackExport>(JSON_RECONSTRUCT_QUERY)
        .bind(&clean_id)
        .bind(&end_range)
        .fetch_optional(&state.legacy_pool)
        .await
    {
        return (StatusCode::OK, Json(row.spotify_data)).into_response();
    }

    // 2. Check Sync (New Data)
    if let Ok(Some(row)) = sqlx::query_as::<_, SpotifyTrackExport>(JSON_RECONSTRUCT_QUERY)
        .bind(&clean_id)
        .bind(&end_range)
        .fetch_optional(&state.sync_pool)
        .await
    {
        return (StatusCode::OK, Json(row.spotify_data)).into_response();
    }

    // 3. Check Current Queue / Failure State
    let queue_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM sync_queue WHERE id = ?")
            .bind(&clean_id)
            .fetch_optional(&state.sync_pool)
            .await
            .unwrap_or(None);

    if let Some(status) = queue_status {
        if status == "failed" {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"status": "failed", "reason": "unresolvable"})),
            )
                .into_response();
        }

        // If it's 'queued' or 'processing', acknowledge it is actively being worked on
        return (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"status": status})),
        )
            .into_response();
    }

    // 4. Add to Database Queue
    let _ = sqlx::query("INSERT INTO sync_queue (id, status) VALUES (?, 'queued')")
        .bind(&clean_id)
        .execute(&state.sync_pool)
        .await;

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"status": "queued"})),
    )
        .into_response()
}

// --- Background Worker ---

async fn spotify_worker(sync_pool: SqlitePool) {
    let creds = Credentials::from_env().expect("Spotify Creds Missing");
    let spotify = ClientCredsSpotify::new(creds);
    spotify.request_token().await.ok();

    loop {
        // Atomic Pop: Update to processing and return the ID in one query
        let next_id: Option<String> = sqlx::query_scalar(
            "UPDATE sync_queue SET status = 'processing' WHERE id = (SELECT id FROM sync_queue WHERE status = 'queued' LIMIT 1) RETURNING id"
        )
        .fetch_optional(&sync_pool)
        .await
        .unwrap_or(None);

        if let Some(id) = next_id {
            let mut success = false;

            if let Ok(tid) = rspotify::model::TrackId::from_id(&id) {
                if let Ok(track) = spotify.track(tid, None).await {
                    if save_to_sync_db(&sync_pool, track).await.is_ok() {
                        success = true;
                        println!("Synced: {}", id);
                    }
                }
            }

            if success {
                // Remove from queue completely on success
                let _ = sqlx::query("DELETE FROM sync_queue WHERE id = ?")
                    .bind(&id)
                    .execute(&sync_pool)
                    .await;
            } else {
                // Mark as failed so the API stops re-queuing it
                let _ = sqlx::query("UPDATE sync_queue SET status = 'failed' WHERE id = ?")
                    .bind(&id)
                    .execute(&sync_pool)
                    .await;
                println!("Marked as failed: {}", id);
            }
        } else {
            // Queue is empty, sleep longer
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn save_to_sync_db(pool: &SqlitePool, track: FullTrack) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    let now = chrono::Utc::now().timestamp();

    sqlx::query(
        "INSERT OR IGNORE INTO available_markets (rowid, available_markets) VALUES (1, 'ALL')",
    )
    .execute(&mut *tx)
    .await?;

    let album = track.album;
    let album_id = album
        .id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_default();
    sqlx::query(
        "INSERT OR IGNORE INTO albums (id, fetched_at, name, album_type, available_markets_rowid, label, popularity, release_date, release_date_precision, total_tracks) 
         VALUES (?, ?, ?, ?, 1, 'Unknown', ?, ?, ?, ?)",
    )
    .bind(&album_id)
    .bind(now)
    .bind(&album.name)
    .bind("album")
    .bind(0_i64)
    .bind(&album.release_date)
    .bind("day")
    .bind(0_i64)
    .execute(&mut *tx)
    .await?;

    let album_rowid: i64 = sqlx::query_scalar("SELECT rowid FROM albums WHERE id = ?")
        .bind(&album_id)
        .fetch_one(&mut *tx)
        .await?;

    for img in album.images {
        sqlx::query(
            "INSERT INTO album_images (album_rowid, width, height, url) VALUES (?, ?, ?, ?)",
        )
        .bind(album_rowid)
        .bind(img.width.unwrap_or(0) as i64)
        .bind(img.height.unwrap_or(0) as i64)
        .bind(img.url)
        .execute(&mut *tx)
        .await?;
    }

    let tid = track
        .id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_default();
    let isrc = track.external_ids.get("isrc").cloned();
    sqlx::query(
        "INSERT OR IGNORE INTO tracks (id, fetched_at, name, preview_url, album_rowid, track_number, external_id_isrc, popularity, available_markets_rowid, disc_number, duration_ms, explicit)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?)",
    )
    .bind(&tid)
    .bind(now)
    .bind(&track.name)
    .bind(&track.preview_url)
    .bind(album_rowid)
    .bind(track.track_number as i64)
    .bind(isrc)
    .bind(0_i64)
    .bind(track.disc_number as i64)
    .bind(track.duration.num_milliseconds())
    .bind(track.explicit)
    .execute(&mut *tx)
    .await?;

    let track_rowid: i64 = sqlx::query_scalar("SELECT rowid FROM tracks WHERE id = ?")
        .bind(&tid)
        .fetch_one(&mut *tx)
        .await?;

    for artist in track.artists {
        let aid = artist
            .id
            .as_ref()
            .map(|id| id.to_string())
            .unwrap_or_default();
        sqlx::query(
            "INSERT OR IGNORE INTO artists (id, fetched_at, name, followers_total, popularity) VALUES (?, ?, ?, 0, 0)",
        )
        .bind(&aid)
        .bind(now)
        .bind(&artist.name)
        .execute(&mut *tx)
        .await?;
        let a_rowid: i64 = sqlx::query_scalar("SELECT rowid FROM artists WHERE id = ?")
            .bind(&aid)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO track_artists (track_rowid, artist_rowid) VALUES (?, ?)",
        )
        .bind(track_rowid)
        .bind(a_rowid)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

// --- Main ---

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let legacy_url = env::var("LEGACY_DB_URL")
        .context("LEGACY_DB_URL required (e.g. sqlite://old.db?mode=ro)")?;
    let legacy_url = with_sqlite_mode_if_missing(&legacy_url, "ro");

    let sync_url_raw = env::var("SYNC_DB_URL").unwrap_or_else(|_| "sqlite://sync.db".to_string());
    let sync_url = with_sqlite_mode_if_missing(&sync_url_raw, "rwc");

    let legacy_pool = SqlitePool::connect(&legacy_url)
        .await
        .with_context(|| format!("Failed to open LEGACY_DB_URL: {legacy_url}"))?;
    let sync_pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&sync_url)
        .await
        .with_context(|| format!("Failed to open SYNC_DB_URL: {sync_url}"))?;

    // Initialize Schema
    sqlx::query(SYNC_DB_SCHEMA)
        .execute(&sync_pool)
        .await
        .context("Failed to initialize sync DB schema")?;

    let state = AppState {
        legacy_pool,
        sync_pool: sync_pool.clone(),
    };

    // Start Worker
    let worker_pool = sync_pool.clone();
    tokio::spawn(async move { spotify_worker(worker_pool).await });

    let app = Router::new()
        .route("/api/isrc/:id", get(resolve_track))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .context("Failed to bind to 0.0.0.0:3000")?;
    println!("API Running on :3000 | Redis-free queue active.");
    axum::serve(listener, app)
        .await
        .context("API server crashed")?;

    Ok(())
}
