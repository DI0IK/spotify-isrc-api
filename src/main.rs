use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use rspotify::{model::FullTrack, prelude::*, ClientCredsSpotify, Credentials};
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use std::env;
use std::time::Duration;

// --- Schema Definition ---
const SYNC_DB_SCHEMA: &str = r#"
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
  `external_id_upc` text,
  `copyright_c` text,
  `copyright_p` text,
  `label` text NOT NULL,
  `popularity` integer NOT NULL,
  `release_date` text NOT NULL,
  `release_date_precision` text NOT NULL,
  `total_tracks` integer NOT NULL,
  `external_id_ean` text,
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

CREATE TABLE IF NOT EXISTS `missing_tracks_queue` (
    `rowid` integer PRIMARY KEY NOT NULL,
    `track_id` text NOT NULL,
    `enqueued_at` integer NOT NULL,
    `status` text NOT NULL,
    `attempts` integer NOT NULL DEFAULT 0,
    `last_attempt_at` integer
);

-- Indices for performance
CREATE INDEX IF NOT EXISTS `tracks_id_unique` ON `tracks` (`id`);
CREATE INDEX IF NOT EXISTS `tracks_isrc` ON `tracks` (`external_id_isrc`);
CREATE INDEX IF NOT EXISTS `track_artists_track_id` ON `track_artists` (`track_rowid`);
CREATE INDEX IF NOT EXISTS `album_images_album_id` ON `album_images` (`album_rowid`);
CREATE UNIQUE INDEX IF NOT EXISTS `missing_tracks_queue_track_id_unique` ON `missing_tracks_queue` (`track_id`);
CREATE INDEX IF NOT EXISTS `missing_tracks_queue_status_rowid` ON `missing_tracks_queue` (`status`, `rowid`);
"#;

// --- JSON Query Template ---
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

#[derive(sqlx::FromRow)]
struct QueuedTrack {
    rowid: i64,
    track_id: String,
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

async fn resolve_track(
    Path(raw_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let clean_id = clean_spotify_id(&raw_id);
    let end_range = format!("{}{}", clean_id, "{");

    // Check Legacy (Read-Only)
    if let Ok(Some(row)) = sqlx::query_as::<_, SpotifyTrackExport>(JSON_RECONSTRUCT_QUERY)
        .bind(&clean_id)
        .bind(&end_range)
        .fetch_optional(&state.legacy_pool)
        .await
    {
        return (StatusCode::OK, Json(row.spotify_data)).into_response();
    }

    // Check Sync (New Data)
    if let Ok(Some(row)) = sqlx::query_as::<_, SpotifyTrackExport>(JSON_RECONSTRUCT_QUERY)
        .bind(&clean_id)
        .bind(&end_range)
        .fetch_optional(&state.sync_pool)
        .await
    {
        return (StatusCode::OK, Json(row.spotify_data)).into_response();
    }

    // Fail -> Queue
    if queue_missing_track(&state.sync_pool, &clean_id)
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": "queue_error"})),
        )
            .into_response();
    }

    (
        StatusCode::NOT_FOUND,
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
        if let Ok(Some(queued)) = take_next_queued_track(&sync_pool).await {
            if let Ok(tid) = rspotify::model::TrackId::from_id(&queued.track_id) {
                if let Ok(track) = spotify.track(tid, None).await {
                    if save_to_sync_db(&sync_pool, track).await.is_ok() {
                        let _ = remove_queued_track(&sync_pool, queued.rowid).await;
                    } else {
                        let _ = reset_queued_track(&sync_pool, queued.rowid).await;
                    }
                } else {
                    let _ = reset_queued_track(&sync_pool, queued.rowid).await;
                }
            } else {
                let _ = remove_queued_track(&sync_pool, queued.rowid).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn queue_missing_track(pool: &SqlitePool, track_id: &str) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT OR IGNORE INTO missing_tracks_queue (track_id, enqueued_at, status, attempts) VALUES (?, ?, 'pending', 0)",
    )
    .bind(track_id)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

async fn take_next_queued_track(pool: &SqlitePool) -> Result<Option<QueuedTrack>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let queued = sqlx::query_as::<_, QueuedTrack>(
        "SELECT rowid, track_id FROM missing_tracks_queue WHERE status = 'pending' ORDER BY rowid LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(item) = queued.as_ref() {
        let now = chrono::Utc::now().timestamp();
        sqlx::query(
            "UPDATE missing_tracks_queue SET status = 'processing', attempts = attempts + 1, last_attempt_at = ? WHERE rowid = ?",
        )
        .bind(now)
        .bind(item.rowid)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(queued)
}

async fn remove_queued_track(pool: &SqlitePool, rowid: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM missing_tracks_queue WHERE rowid = ?")
        .bind(rowid)
        .execute(pool)
        .await?;
    Ok(())
}

async fn reset_queued_track(pool: &SqlitePool, rowid: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE missing_tracks_queue SET status = 'pending' WHERE rowid = ?")
        .bind(rowid)
        .execute(pool)
        .await?;
    Ok(())
}

async fn save_to_sync_db(pool: &SqlitePool, track: FullTrack) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let now = chrono::Utc::now().timestamp();

    // 1. Markets
    sqlx::query(
        "INSERT OR IGNORE INTO available_markets (rowid, available_markets) VALUES (1, 'ALL')",
    )
    .execute(&mut *tx)
    .await?;

    // 2. Album
    let album = track.album;
    let album_id = album
        .id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_default();
    sqlx::query(
        "INSERT OR IGNORE INTO albums (id, fetched_at, name, album_type, available_markets_rowid, label, popularity, release_date, release_date_precision, total_tracks) 
         VALUES (?, ?, ?, ?, 1, 'Unknown', ?, ?, ?, ?)"
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
        .bind(img.width.unwrap_or_default() as i64)
        .bind(img.height.unwrap_or_default() as i64)
        .bind(&img.url)
        .execute(&mut *tx)
        .await?;
    }

    // 3. Track
    let tid = track
        .id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_default();
    let isrc = track.external_ids.get("isrc").cloned();
    sqlx::query(
        "INSERT OR IGNORE INTO tracks (id, fetched_at, name, preview_url, album_rowid, track_number, external_id_isrc, popularity, available_markets_rowid, disc_number, duration_ms, explicit)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?)"
    )
    .bind(&tid)
    .bind(now)
    .bind(&track.name)
    .bind(track.preview_url.as_deref())
    .bind(album_rowid)
    .bind(track.track_number as i64)
    .bind(isrc.as_deref())
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

    // 4. Artists
    for artist in track.artists {
        let aid = artist
            .id
            .as_ref()
            .map(|id| id.to_string())
            .unwrap_or_default();
        sqlx::query("INSERT OR IGNORE INTO artists (id, fetched_at, name, followers_total, popularity) VALUES (?, ?, ?, 0, 0)")
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
async fn main() {
    let legacy_url = env::var("LEGACY_DB_URL").expect("LEGACY_DB_URL must be set (with ?mode=ro)");
    let sync_url = env::var("SYNC_DB_URL").unwrap_or_else(|_| "sqlite://sync.db".to_string());

    let legacy_pool = SqlitePool::connect(&legacy_url).await.unwrap();
    let sync_pool = SqlitePool::connect(&sync_url).await.unwrap();

    // --- CRITICAL: Initialize Schema in the Sync DB ---
    sqlx::query(SYNC_DB_SCHEMA)
        .execute(&sync_pool)
        .await
        .expect("Failed to init Sync DB schema");

    let state = AppState {
        legacy_pool,
        sync_pool: sync_pool.clone(),
    };

    let worker_pool = sync_pool.clone();
    tokio::spawn(async move { spotify_worker(worker_pool).await });

    let app = Router::new()
        .route("/api/isrc/:id", get(resolve_track))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:2345").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
