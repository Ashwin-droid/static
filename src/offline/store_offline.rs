use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Manages play history (SQLite) and offline audio/artwork cache on disk.
///
/// Directory layout under `$HOME/Music/Static/`:
///   data.db          – SQLite database (play counts, offline index)
///   offline/         – downloaded audio files  (`{video_id}.audio`)
///   offline/art/     – downloaded artwork      (`{video_id}.jpg`)
pub struct MusicCache {
    pub(super) db: Connection,
    pub(super) base_dir: PathBuf,
    pub(super) offline_dir: PathBuf,
    pub art_dir: PathBuf,
    pub(super) temp_dir: PathBuf,
}

pub(super) const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

impl MusicCache {
    fn default_base_dir() -> PathBuf {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join("Music/Static")
    }

    pub(super) fn resolve_storage_root(raw: &str) -> PathBuf {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Self::default_base_dir();
        }

        if trimmed == "~" {
            return std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp"));
        }

        if let Some(rest) = trimmed.strip_prefix("~/") {
            return std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp"))
                .join(rest);
        }

        PathBuf::from(trimmed)
    }

    pub(super) fn ensure_storage_layout(base: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
        std::fs::create_dir_all(base)?;

        let offline_dir = base.join("offline");
        std::fs::create_dir_all(&offline_dir)?;

        let art_dir = base.join("offline/art");
        std::fs::create_dir_all(&art_dir)?;

        let temp_dir = base.join("temp");
        std::fs::create_dir_all(&temp_dir)?;

        Ok((offline_dir, art_dir, temp_dir))
    }

    #[allow(dead_code)]
    pub fn new() -> Result<Self> {
        Self::new_with_storage_root(None)
    }

    pub fn new_with_storage_root(raw_root: Option<&str>) -> Result<Self> {
        let default_base = Self::default_base_dir();
        let base = raw_root
            .map(Self::resolve_storage_root)
            .unwrap_or_else(|| default_base.clone());

        if let Some(raw_root) = raw_root {
            let desired_db_path = base.join("data.db");
            let default_db_path = default_base.join("data.db");
            if base != default_base && !desired_db_path.exists() && default_db_path.exists() {
                let mut cache = Self::open_at_base(default_base)?;
                cache.migrate_storage_root(raw_root)?;
                return Ok(cache);
            }
        }

        Self::open_at_base(base)
    }

    pub(super) fn open_at_base(base: PathBuf) -> Result<Self> {
        let (offline_dir, art_dir, temp_dir) = Self::ensure_storage_layout(&base)?;
        let db = Self::open_database(&base)?;

        Ok(Self {
            db,
            base_dir: base,
            offline_dir,
            art_dir,
            temp_dir,
        })
    }

    pub(super) fn open_database(base: &Path) -> Result<Connection> {
        let db = Connection::open(base.join("data.db")).context("Failed to open cache database")?;
        Self::initialize_database(&db)?;
        Ok(db)
    }

    fn initialize_database(db: &Connection) -> Result<()> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS plays (
                video_id     TEXT PRIMARY KEY,
                title        TEXT NOT NULL,
                artist       TEXT NOT NULL,
                thumbnail_url TEXT,
                play_count   INTEGER NOT NULL DEFAULT 1,
                listen_secs  INTEGER NOT NULL DEFAULT 0,
                last_played  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS offline (
                video_id   TEXT PRIMARY KEY,
                audio_path TEXT NOT NULL,
                art_path   TEXT NOT NULL DEFAULT '',
                cached_at  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS offline_sponsorblock (
                video_id      TEXT PRIMARY KEY,
                segments_json TEXT NOT NULL,
                cached_at     INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS listen_again_candidates (
                video_id            TEXT PRIMARY KEY,
                last_recommended_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS app_settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        let _ = db
            .execute_batch("ALTER TABLE plays ADD COLUMN listen_secs INTEGER NOT NULL DEFAULT 0;");

        let _ =
            db.execute_batch("ALTER TABLE offline ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;");

        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS offline_playlists (
                playlist_id    TEXT PRIMARY KEY,
                title          TEXT NOT NULL,
                thumbnail_url  TEXT,
                subtitle       TEXT,
                created_at     INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS offline_playlist_tracks (
                playlist_id  TEXT NOT NULL,
                video_id     TEXT NOT NULL,
                position     INTEGER NOT NULL,
                PRIMARY KEY (playlist_id, video_id),
                FOREIGN KEY (playlist_id) REFERENCES offline_playlists(playlist_id)
            );",
        )?;

        Ok(())
    }

    #[allow(dead_code)]
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    #[allow(dead_code)]
    pub fn offline_dir(&self) -> &Path {
        &self.offline_dir
    }

    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }
}

pub(super) fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
