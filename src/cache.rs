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
    db: Connection,
    offline_dir: PathBuf,
    art_dir: PathBuf,
}

impl MusicCache {
    pub fn new() -> Result<Self> {
        let base = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join("Music/Static");
        std::fs::create_dir_all(&base)?;

        let offline_dir = base.join("offline");
        std::fs::create_dir_all(&offline_dir)?;

        let art_dir = base.join("offline/art");
        std::fs::create_dir_all(&art_dir)?;

        let db = Connection::open(base.join("data.db"))
            .context("Failed to open cache database")?;

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
            );",
        )?;

        // Migrate: add listen_secs column if missing (existing installs)
        let _ = db.execute_batch(
            "ALTER TABLE plays ADD COLUMN listen_secs INTEGER NOT NULL DEFAULT 0;"
        );


        Ok(Self { db, offline_dir, art_dir })
    }

    // ── Play history ─────────────────────────────────────────────

    /// Record (or increment) a play for a track, accumulating listen time.
    pub fn record_play(
        &self,
        video_id: &str,
        title: &str,
        artist: &str,
        thumbnail_url: Option<&str>,
        listen_secs: u64,
    ) -> Result<()> {
        let now = epoch_secs();
        self.db.execute(
            "INSERT INTO plays (video_id, title, artist, thumbnail_url, play_count, listen_secs, last_played)
             VALUES (?1, ?2, ?3, ?4, 1, ?6, ?5)
             ON CONFLICT(video_id) DO UPDATE SET
                 play_count = play_count + 1,
                 listen_secs = listen_secs + ?6,
                 last_played = ?5,
                 title = ?2,
                 artist = ?3,
                 thumbnail_url = COALESCE(?4, thumbnail_url)",
            rusqlite::params![video_id, title, artist, thumbnail_url, now, listen_secs as i64],
        )?;
        Ok(())
    }

    /// Top-N most played video IDs (ordered by play_count desc, last_played desc).
    /// Only includes songs played more than once so a single listen doesn't trigger a download.
    pub fn top_played(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.db.prepare(
            "SELECT video_id FROM plays
             WHERE play_count >= 2
             ORDER BY play_count DESC, last_played DESC
             LIMIT ?1",
        )?;
        let ids = stmt
            .query_map([limit as i64], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(ids)
    }

    // ── Offline cache ────────────────────────────────────────────

    /// Return the local path if this song has been downloaded.
    pub fn get_cached_audio(&self, video_id: &str) -> Option<PathBuf> {
        let path: Option<String> = self
            .db
            .query_row(
                "SELECT audio_path FROM offline WHERE video_id = ?1",
                [video_id],
                |row| row.get(0),
            )
            .ok();
        path.map(PathBuf::from).filter(|p| p.exists())
    }

    /// Return the local artwork path if this song has cached art.
    pub fn get_cached_art(&self, video_id: &str) -> Option<PathBuf> {
        let path: Option<String> = self
            .db
            .query_row(
                "SELECT art_path FROM offline WHERE video_id = ?1",
                [video_id],
                |row| row.get(0),
            )
            .ok();
        path.filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.exists())
    }

    /// All video IDs currently stored offline.
    pub fn cached_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self.db.prepare("SELECT video_id FROM offline")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(ids)
    }

    /// Write downloaded audio (and optionally artwork) to disk and index it.
    pub fn store_audio(
        &self,
        video_id: &str,
        audio_data: &[u8],
        art_data: Option<&[u8]>,
    ) -> Result<()> {
        let audio_path = self.offline_dir.join(format!("{video_id}.audio"));
        std::fs::write(&audio_path, audio_data)?;

        let art_path_str = if let Some(art) = art_data {
            let p = self.art_dir.join(format!("{video_id}.jpg"));
            std::fs::write(&p, art)?;
            p.to_string_lossy().into_owned()
        } else {
            String::new()
        };

        self.db.execute(
            "INSERT OR REPLACE INTO offline (video_id, audio_path, art_path, cached_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                video_id,
                audio_path.to_string_lossy().as_ref(),
                art_path_str,
                epoch_secs(),
            ],
        )?;

        eprintln!("[CACHE] stored offline: {video_id} ({} KB)", audio_data.len() / 1024);
        Ok(())
    }

    /// Remove a single song from offline cache (files + DB row).
    pub fn remove_cached(&self, video_id: &str) -> Result<()> {
        if let Some(p) = self.get_cached_audio(video_id) {
            let _ = std::fs::remove_file(p);
        }
        if let Some(p) = self.get_cached_art(video_id) {
            let _ = std::fs::remove_file(p);
        }
        self.db
            .execute("DELETE FROM offline WHERE video_id = ?1", [video_id])?;
        eprintln!("[CACHE] evicted offline: {video_id}");
        Ok(())
    }

    /// Get the thumbnail URL stored in play history for a given video.
    pub fn get_thumbnail_url(&self, video_id: &str) -> Option<String> {
        self.db
            .query_row(
                "SELECT thumbnail_url FROM plays WHERE video_id = ?1",
                [video_id],
                |row| row.get(0),
            )
            .ok()
            .flatten()
    }

    #[allow(dead_code)]
    pub fn offline_dir(&self) -> &Path {
        &self.offline_dir
    }
}

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
