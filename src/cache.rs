use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::sponsorblock::{
    SponsorSegment, default_skip_categories, normalize_segments, sanitize_categories,
};

#[derive(Debug, Clone)]
pub struct AppSettings {
    pub auto_sync_chromium_cookies: bool,
    pub chromium_cookie_path: String,
    pub auto_manage_offline_cache: bool,
    pub auto_cache_listen_again: bool,
    pub offline_cache_target: u32,
    pub offline_rewatch_threshold: u32,
    pub score_decay_window_days: u32,
    pub minimum_offline_score: f64,
    pub prefetch_enabled: bool,
    pub prefetch_ahead: u32,
    pub prefer_local_cache_on_playback: bool,
    pub sponsorblock_enabled: bool,
    pub sponsorblock_categories: Vec<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            auto_sync_chromium_cookies: true,
            chromium_cookie_path: String::new(),
            auto_manage_offline_cache: true,
            auto_cache_listen_again: true,
            offline_cache_target: 100,
            offline_rewatch_threshold: 1,
            score_decay_window_days: 30,
            minimum_offline_score: 0.0,
            prefetch_enabled: true,
            prefetch_ahead: 3,
            prefer_local_cache_on_playback: true,
            sponsorblock_enabled: true,
            sponsorblock_categories: default_skip_categories(),
        }
    }
}

/// Manages play history (SQLite) and offline audio/artwork cache on disk.
///
/// Directory layout under `$HOME/Music/Static/`:
///   data.db          – SQLite database (play counts, offline index)
///   offline/         – downloaded audio files  (`{video_id}.audio`)
///   offline/art/     – downloaded artwork      (`{video_id}.jpg`)
pub struct MusicCache {
    db: Connection,
    base_dir: PathBuf,
    offline_dir: PathBuf,
    pub art_dir: PathBuf,
    temp_dir: PathBuf,
}

const LISTEN_AGAIN_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

impl MusicCache {
    fn default_base_dir() -> PathBuf {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join("Music/Static")
    }

    fn resolve_storage_root(raw: &str) -> PathBuf {
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

    fn ensure_storage_layout(base: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
        std::fs::create_dir_all(base)?;

        let offline_dir = base.join("offline");
        std::fs::create_dir_all(&offline_dir)?;

        let art_dir = base.join("offline/art");
        std::fs::create_dir_all(&art_dir)?;

        let temp_dir = base.join("temp");
        std::fs::create_dir_all(&temp_dir)?;

        Ok((offline_dir, art_dir, temp_dir))
    }

    pub fn new() -> Result<Self> {
        let base = Self::default_base_dir();
        let (offline_dir, art_dir, temp_dir) = Self::ensure_storage_layout(&base)?;
        // Clean stale prefetch files on startup
        if let Ok(entries) = std::fs::read_dir(&temp_dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }

        let db = Connection::open(base.join("data.db")).context("Failed to open cache database")?;

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

        // Migrate: add listen_secs column if missing (existing installs)
        let _ = db
            .execute_batch("ALTER TABLE plays ADD COLUMN listen_secs INTEGER NOT NULL DEFAULT 0;");

        // Migrate: add pinned column (manually downloaded, not auto-evicted)
        let _ =
            db.execute_batch("ALTER TABLE offline ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;");

        // Offline playlists: store playlist metadata + track ordering
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

        Ok(Self {
            db,
            base_dir: base,
            offline_dir,
            art_dir,
            temp_dir,
        })
    }

    pub fn apply_storage_root(&mut self, raw_root: &str) -> Result<()> {
        let base = Self::resolve_storage_root(raw_root);
        let (offline_dir, art_dir, temp_dir) = Self::ensure_storage_layout(&base)?;
        self.base_dir = base;
        self.offline_dir = offline_dir;
        self.art_dir = art_dir;
        self.temp_dir = temp_dir;
        Ok(())
    }

    pub fn migrate_storage_root(&mut self, raw_root: &str) -> Result<()> {
        let new_base = Self::resolve_storage_root(raw_root);
        if self.base_dir == new_base {
            self.apply_storage_root(raw_root)?;
            return Ok(());
        }

        let old_base = self.base_dir.clone();
        let old_offline_dir = self.offline_dir.clone();
        let old_art_dir = self.art_dir.clone();
        let old_temp_dir = self.temp_dir.clone();

        let (new_offline_dir, new_art_dir, new_temp_dir) = Self::ensure_storage_layout(&new_base)?;

        move_dir_contents(&old_art_dir, &new_art_dir)?;
        move_dir_contents(&old_offline_dir, &new_offline_dir)?;
        move_dir_contents(&old_temp_dir, &new_temp_dir)?;

        self.db.execute(
            "UPDATE offline
             SET audio_path = REPLACE(audio_path, ?1, ?2),
                 art_path = CASE
                     WHEN art_path = '' THEN art_path
                     ELSE REPLACE(art_path, ?3, ?4)
                 END",
            rusqlite::params![
                old_offline_dir.to_string_lossy().as_ref(),
                new_offline_dir.to_string_lossy().as_ref(),
                old_art_dir.to_string_lossy().as_ref(),
                new_art_dir.to_string_lossy().as_ref(),
            ],
        )?;

        self.db.execute(
            "UPDATE offline_playlists
             SET thumbnail_url = CASE
                 WHEN thumbnail_url IS NULL THEN NULL
                 ELSE REPLACE(thumbnail_url, ?1, ?2)
             END",
            rusqlite::params![
                old_art_dir.to_string_lossy().as_ref(),
                new_art_dir.to_string_lossy().as_ref(),
            ],
        )?;

        self.base_dir = new_base;
        self.offline_dir = new_offline_dir;
        self.art_dir = new_art_dir;
        self.temp_dir = new_temp_dir;

        let _ = std::fs::remove_dir(old_art_dir);
        let _ = std::fs::remove_dir(old_offline_dir);
        let _ = std::fs::remove_dir(old_temp_dir);
        let _ = std::fs::remove_dir(old_base);
        Ok(())
    }

    pub fn load_settings(&self) -> Result<AppSettings> {
        let mut settings = AppSettings::default();
        let mut stmt = self.db.prepare("SELECT key, value FROM app_settings")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        for row in rows.flatten() {
            match row.0.as_str() {
                "auto_sync_chromium_cookies" => {
                    settings.auto_sync_chromium_cookies =
                        parse_bool_setting(&row.1, settings.auto_sync_chromium_cookies);
                }
                "chromium_cookie_path" => {
                    settings.chromium_cookie_path = row.1;
                }
                "auto_manage_offline_cache" => {
                    settings.auto_manage_offline_cache =
                        parse_bool_setting(&row.1, settings.auto_manage_offline_cache);
                }
                "auto_cache_listen_again" => {
                    settings.auto_cache_listen_again =
                        parse_bool_setting(&row.1, settings.auto_cache_listen_again);
                }
                "offline_cache_target" => {
                    settings.offline_cache_target = row
                        .1
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(settings.offline_cache_target)
                        .clamp(0, 500);
                }
                "offline_rewatch_threshold" => {
                    settings.offline_rewatch_threshold = row
                        .1
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(settings.offline_rewatch_threshold)
                        .clamp(0, 50);
                }
                "score_decay_window_days" => {
                    settings.score_decay_window_days = row
                        .1
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(settings.score_decay_window_days)
                        .clamp(1, 365);
                }
                "minimum_offline_score" => {
                    settings.minimum_offline_score = row
                        .1
                        .parse::<f64>()
                        .ok()
                        .unwrap_or(settings.minimum_offline_score)
                        .clamp(0.0, 999.0);
                }
                "prefetch_enabled" => {
                    settings.prefetch_enabled =
                        parse_bool_setting(&row.1, settings.prefetch_enabled);
                }
                "prefetch_ahead" => {
                    settings.prefetch_ahead = row
                        .1
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(settings.prefetch_ahead)
                        .clamp(0, 12);
                }
                "prefer_local_cache_on_playback" => {
                    settings.prefer_local_cache_on_playback =
                        parse_bool_setting(&row.1, settings.prefer_local_cache_on_playback);
                }
                "sponsorblock_enabled" => {
                    settings.sponsorblock_enabled =
                        parse_bool_setting(&row.1, settings.sponsorblock_enabled);
                }
                "sponsorblock_categories" => {
                    if let Ok(categories) = serde_json::from_str::<Vec<String>>(&row.1) {
                        settings.sponsorblock_categories = sanitize_categories(&categories);
                    }
                }
                _ => {}
            }
        }

        Ok(settings)
    }

    pub fn save_settings(&self, settings: &AppSettings) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        let sponsorblock_categories =
            serde_json::to_string(&sanitize_categories(&settings.sponsorblock_categories))?;
        for (key, value) in [
            (
                "auto_sync_chromium_cookies",
                bool_setting(settings.auto_sync_chromium_cookies),
            ),
            (
                "chromium_cookie_path",
                settings.chromium_cookie_path.clone(),
            ),
            (
                "auto_manage_offline_cache",
                bool_setting(settings.auto_manage_offline_cache),
            ),
            (
                "auto_cache_listen_again",
                bool_setting(settings.auto_cache_listen_again),
            ),
            (
                "offline_cache_target",
                settings.offline_cache_target.to_string(),
            ),
            (
                "offline_rewatch_threshold",
                settings.offline_rewatch_threshold.to_string(),
            ),
            (
                "score_decay_window_days",
                settings.score_decay_window_days.to_string(),
            ),
            (
                "minimum_offline_score",
                settings.minimum_offline_score.to_string(),
            ),
            ("prefetch_enabled", bool_setting(settings.prefetch_enabled)),
            ("prefetch_ahead", settings.prefetch_ahead.to_string()),
            (
                "prefer_local_cache_on_playback",
                bool_setting(settings.prefer_local_cache_on_playback),
            ),
            (
                "sponsorblock_enabled",
                bool_setting(settings.sponsorblock_enabled),
            ),
            ("sponsorblock_categories", sponsorblock_categories.clone()),
        ] {
            tx.execute(
                "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
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

    pub fn upsert_track_metadata(
        &self,
        video_id: &str,
        title: &str,
        artist: &str,
        thumbnail_url: Option<&str>,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO plays (video_id, title, artist, thumbnail_url, play_count, listen_secs, last_played)
             VALUES (?1, ?2, ?3, ?4, 0, 0, 0)
             ON CONFLICT(video_id) DO UPDATE SET
                 title = ?2,
                 artist = ?3,
                 thumbnail_url = COALESCE(?4, thumbnail_url)",
            rusqlite::params![video_id, title, artist, thumbnail_url],
        )?;
        Ok(())
    }

    pub fn auto_offline_candidates(
        &self,
        settings: &AppSettings,
        limit: usize,
    ) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let now = epoch_secs();
        let min_total_plays = settings.offline_rewatch_threshold.saturating_add(1).max(1);
        let decay_window_days = settings.score_decay_window_days.max(1) as f64;
        let min_score = settings.minimum_offline_score.max(0.0);
        let mut candidate_ids = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        if settings.auto_cache_listen_again {
            for video_id in self.listen_again_candidate_ids()? {
                if seen_ids.insert(video_id.clone()) {
                    candidate_ids.push(video_id);
                }
            }
        }

        let mut stmt = self.db.prepare(
            "SELECT video_id, play_count, last_played
             FROM plays
             WHERE play_count >= ?1",
        )?;
        let mut ranked = stmt
            .query_map([min_total_plays as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .filter_map(|row| row.ok())
            .filter_map(|(video_id, play_count, last_played)| {
                let days_since = ((now - last_played).max(0) as f64) / 86_400.0;
                let score = play_count as f64 / (1.0 + (days_since / decay_window_days));
                (score >= min_score).then_some((video_id, score))
            })
            .collect::<Vec<_>>();

        ranked.sort_by(|(_, left), (_, right)| {
            right.partial_cmp(left).unwrap_or(std::cmp::Ordering::Equal)
        });

        for (video_id, _) in ranked {
            if candidate_ids.len() >= limit {
                break;
            }
            if seen_ids.insert(video_id.clone()) {
                candidate_ids.push(video_id);
            }
        }

        Ok(candidate_ids)
    }

    pub fn auto_download_eviction_count(&self, settings: &AppSettings) -> Result<usize> {
        let candidates = self
            .auto_offline_candidates(settings, settings.offline_cache_target as usize)?
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let cached = self.cached_ids()?;
        let pinned = self.pinned_ids()?;
        Ok(cached
            .into_iter()
            .filter(|video_id| !pinned.contains(video_id) && !candidates.contains(video_id))
            .count())
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
    /// Return all offline-cached songs with metadata from play history.
    /// Returns (video_id, title, artist, art_path).
    pub fn offline_tracks(&self) -> Result<Vec<(String, String, String, Option<String>)>> {
        let mut stmt = self.db.prepare(
            "SELECT o.video_id, COALESCE(p.title, o.video_id), COALESCE(p.artist, ''),
                    CASE WHEN o.art_path != '' THEN o.art_path ELSE NULL END
             FROM offline o
             LEFT JOIN plays p ON o.video_id = p.video_id
             ORDER BY p.play_count DESC, p.last_played DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

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

        eprintln!(
            "[CACHE] stored offline: {video_id} ({} KB)",
            audio_data.len() / 1024
        );
        Ok(())
    }

    pub fn get_offline_sponsor_segments(
        &self,
        video_id: &str,
    ) -> Result<Option<Vec<SponsorSegment>>> {
        let payload: Option<String> = self
            .db
            .query_row(
                "SELECT segments_json FROM offline_sponsorblock WHERE video_id = ?1",
                [video_id],
                |row| row.get(0),
            )
            .optional()?;

        let Some(payload) = payload else {
            return Ok(None);
        };

        match serde_json::from_str::<Vec<SponsorSegment>>(&payload) {
            Ok(segments) => Ok(Some(normalize_segments(segments))),
            Err(err) => {
                eprintln!(
                    "[SPONSORBLOCK] invalid offline cache entry for {video_id}, dropping it: {err}"
                );
                let _ = self.db.execute(
                    "DELETE FROM offline_sponsorblock WHERE video_id = ?1",
                    [video_id],
                );
                Ok(None)
            }
        }
    }

    pub fn store_offline_sponsor_segments(
        &self,
        video_id: &str,
        segments: &[SponsorSegment],
    ) -> Result<()> {
        let is_offline = self
            .db
            .query_row(
                "SELECT 1 FROM offline WHERE video_id = ?1",
                [video_id],
                |_| Ok(()),
            )
            .is_ok();
        if !is_offline {
            return Ok(());
        }

        let segments_json = serde_json::to_string(&normalize_segments(segments.to_vec()))
            .context("Failed to serialize SponsorBlock segments")?;
        self.db.execute(
            "INSERT OR REPLACE INTO offline_sponsorblock (video_id, segments_json, cached_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![video_id, segments_json, epoch_secs()],
        )?;
        Ok(())
    }

    pub fn store_listen_again_snapshot(&self, video_ids: &[String]) -> Result<()> {
        let now = epoch_secs();
        let tx = self.db.unchecked_transaction()?;
        for video_id in video_ids {
            tx.execute(
                "INSERT INTO listen_again_candidates (video_id, last_recommended_at)
                 VALUES (?1, ?2)
                 ON CONFLICT(video_id) DO UPDATE SET last_recommended_at = excluded.last_recommended_at",
                rusqlite::params![video_id, now],
            )?;
        }
        tx.commit()?;
        self.prune_stale_listen_again_candidates()?;
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
        let _ = self.db.execute(
            "DELETE FROM offline_sponsorblock WHERE video_id = ?1",
            [video_id],
        );
        self.db
            .execute("DELETE FROM offline WHERE video_id = ?1", [video_id])?;
        eprintln!("[CACHE] evicted offline: {video_id}");
        Ok(())
    }

    /// Get the thumbnail URL stored in play history for a given video.
    pub fn get_thumbnail_url(&self, video_id: &str) -> Option<String> {
        let thumbnail: Option<String> = self
            .db
            .query_row(
                "SELECT thumbnail_url FROM plays WHERE video_id = ?1",
                [video_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();

        thumbnail.and_then(|url| {
            if let Some(path) = url.strip_prefix("file://") {
                if PathBuf::from(path).exists() {
                    Some(url)
                } else {
                    None
                }
            } else {
                Some(url)
            }
        })
    }

    /// Mark a song as pinned (manually downloaded, won't be auto-evicted).
    pub fn pin_offline(&self, video_id: &str) -> Result<()> {
        self.db.execute(
            "UPDATE offline SET pinned = 1 WHERE video_id = ?1",
            [video_id],
        )?;
        Ok(())
    }

    /// Return all pinned (manually downloaded) video IDs.
    pub fn pinned_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .db
            .prepare("SELECT video_id FROM offline WHERE pinned = 1")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(ids)
    }

    /// Check whether a song is a manually pinned offline download.
    pub fn is_pinned_offline(&self, video_id: &str) -> bool {
        self.db
            .query_row(
                "SELECT pinned FROM offline WHERE video_id = ?1",
                [video_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|pinned| pinned != 0)
            .unwrap_or(false)
    }

    /// Check if a song is already cached offline.
    pub fn is_cached(&self, video_id: &str) -> bool {
        self.get_cached_audio(video_id).is_some()
    }

    /// Get (title, artist) metadata for a video from play history.
    pub fn get_track_meta(&self, video_id: &str) -> Option<(String, String)> {
        self.db
            .query_row(
                "SELECT title, artist FROM plays WHERE video_id = ?1",
                [video_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok()
    }

    // ── Offline playlists ──────────────────────────────────────

    /// Store an offline playlist header + track ordering.
    pub fn store_offline_playlist(
        &self,
        playlist_id: &str,
        title: &str,
        thumbnail_url: Option<&str>,
        subtitle: Option<&str>,
        video_ids: &[String],
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO offline_playlists (playlist_id, title, thumbnail_url, subtitle, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![playlist_id, title, thumbnail_url, subtitle, epoch_secs()],
        )?;
        // Replace track list
        self.db.execute(
            "DELETE FROM offline_playlist_tracks WHERE playlist_id = ?1",
            [playlist_id],
        )?;
        for (i, vid) in video_ids.iter().enumerate() {
            self.db.execute(
                "INSERT INTO offline_playlist_tracks (playlist_id, video_id, position)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![playlist_id, vid, i as i64],
            )?;
        }
        Ok(())
    }

    /// Get all offline playlists as (playlist_id, title, thumbnail_url, subtitle).
    pub fn offline_playlists(
        &self,
    ) -> Result<Vec<(String, String, Option<String>, Option<String>)>> {
        let mut stmt = self.db.prepare(
            "SELECT playlist_id, title, thumbnail_url, subtitle
             FROM offline_playlists ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Check if a playlist has been downloaded offline.
    pub fn is_playlist_offline(&self, playlist_id: &str) -> bool {
        self.db
            .query_row(
                "SELECT 1 FROM offline_playlists WHERE playlist_id = ?1",
                [playlist_id],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Get ordered video IDs for an offline playlist.
    pub fn offline_playlist_track_ids(&self, playlist_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.db.prepare(
            "SELECT video_id FROM offline_playlist_tracks
             WHERE playlist_id = ?1 ORDER BY position",
        )?;
        let ids = stmt
            .query_map([playlist_id], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(ids)
    }

    pub fn is_track_in_any_offline_playlist(&self, video_id: &str) -> Result<bool> {
        Ok(self
            .db
            .query_row(
                "SELECT 1 FROM offline_playlist_tracks WHERE video_id = ?1 LIMIT 1",
                [video_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn offline_listen_again_track_ids(&self) -> Result<Vec<String>> {
        let cutoff = epoch_secs() - LISTEN_AGAIN_RETENTION_SECS;
        let mut stmt = self.db.prepare(
            "SELECT lac.video_id
             FROM listen_again_candidates lac
             INNER JOIN offline o ON o.video_id = lac.video_id
             LEFT JOIN plays p ON p.video_id = lac.video_id
             WHERE lac.last_recommended_at >= ?1
                OR COALESCE(p.last_played, 0) >= ?1
             ORDER BY
                CASE
                    WHEN lac.last_recommended_at >= ?1 THEN lac.last_recommended_at
                    ELSE COALESCE(p.last_played, 0)
                END DESC",
        )?;
        let ids = stmt
            .query_map([cutoff], |row| row.get(0))?
            .filter_map(|row| row.ok())
            .collect();
        Ok(ids)
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

    // ── Temp prefetch cache ──────────────────────────────────

    /// Return the temp-cached path if this song has been prefetched.
    pub fn get_temp_audio(&self, video_id: &str) -> Option<PathBuf> {
        let path = self.temp_dir.join(format!("{video_id}.audio"));
        path.exists().then_some(path)
    }

    /// Write prefetched audio to the temp directory.
    pub fn store_temp_audio(&self, video_id: &str, data: &[u8]) {
        let path = self.temp_dir.join(format!("{video_id}.audio"));
        if let Err(e) = std::fs::write(&path, data) {
            eprintln!("[PREFETCH] failed to write temp {video_id}: {e}");
        } else {
            eprintln!("[PREFETCH] cached {video_id} ({} KB)", data.len() / 1024);
        }
    }

    /// Clear all temp prefetch files.
    #[allow(dead_code)]
    pub fn clear_temp(&self) {
        if let Ok(entries) = std::fs::read_dir(&self.temp_dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    pub fn clear_auto_downloads(&self) -> Result<usize> {
        let mut stmt = self
            .db
            .prepare("SELECT video_id FROM offline WHERE pinned = 0")?;
        let ids: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|row| row.ok())
            .collect();

        let removed = ids.len();
        for video_id in ids {
            let _ = self.remove_cached(&video_id);
        }

        Ok(removed)
    }

    fn listen_again_candidate_ids(&self) -> Result<Vec<String>> {
        let cutoff = epoch_secs() - LISTEN_AGAIN_RETENTION_SECS;
        let mut stmt = self.db.prepare(
            "SELECT lac.video_id
             FROM listen_again_candidates lac
             LEFT JOIN plays p ON p.video_id = lac.video_id
             WHERE lac.last_recommended_at >= ?1
                OR COALESCE(p.last_played, 0) >= ?1
             ORDER BY
                CASE
                    WHEN lac.last_recommended_at >= ?1 THEN lac.last_recommended_at
                    ELSE COALESCE(p.last_played, 0)
                END DESC",
        )?;
        let ids = stmt
            .query_map([cutoff], |row| row.get(0))?
            .filter_map(|row| row.ok())
            .collect();
        Ok(ids)
    }

    fn prune_stale_listen_again_candidates(&self) -> Result<()> {
        let cutoff = epoch_secs() - LISTEN_AGAIN_RETENTION_SECS;
        self.db.execute(
            "DELETE FROM listen_again_candidates
             WHERE video_id IN (
                 SELECT lac.video_id
                 FROM listen_again_candidates lac
                 LEFT JOIN plays p ON p.video_id = lac.video_id
                 WHERE lac.last_recommended_at < ?1
                   AND COALESCE(p.last_played, 0) < ?1
             )",
            [cutoff],
        )?;
        Ok(())
    }
}

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn parse_bool_setting(value: &str, default: bool) -> bool {
    match value {
        "1" | "true" | "TRUE" | "yes" | "on" => true,
        "0" | "false" | "FALSE" | "no" | "off" => false,
        _ => default,
    }
}

fn bool_setting(value: bool) -> String {
    if value { "1" } else { "0" }.to_string()
}

fn move_dir_contents(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() || from == to {
        return Ok(());
    }

    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let destination = to.join(entry.file_name());
        if source.is_dir() {
            move_dir_contents(&source, &destination)?;
            let _ = std::fs::remove_dir(&source);
            continue;
        }

        if destination.exists() {
            let _ = std::fs::remove_file(&destination);
        }

        match std::fs::rename(&source, &destination) {
            Ok(_) => {}
            Err(_) => {
                std::fs::copy(&source, &destination)?;
                std::fs::remove_file(&source)?;
            }
        }
    }
    Ok(())
}
