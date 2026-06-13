use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::network::{SponsorSegment, normalize_segments};

use super::store_offline::{MusicCache, epoch_secs};

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

impl MusicCache {
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

    /// Like [`offline_tracks`](Self::offline_tracks) but ordered by descending
    /// offline score — `play_count` decayed by how long ago the track was last
    /// played, matching the auto-cache retention scoring in
    /// `auto_offline_candidates` (`score = play_count / (1 + days_since/decay)`).
    /// Tracks with no play history score 0 and sink to the bottom.
    pub fn offline_tracks_by_score(
        &self,
        decay_window_days: u32,
    ) -> Result<Vec<(String, String, String, Option<String>)>> {
        let decay = decay_window_days.max(1) as f64;
        let mut stmt = self.db.prepare(
            "SELECT o.video_id, COALESCE(p.title, o.video_id), COALESCE(p.artist, ''),
                    CASE WHEN o.art_path != '' THEN o.art_path ELSE NULL END
             FROM offline o
             LEFT JOIN plays p ON o.video_id = p.video_id
             ORDER BY
                 COALESCE(p.play_count, 0)
                   / (1.0 + (MAX(strftime('%s', 'now') - COALESCE(p.last_played, 0), 0) / 86400.0) / ?1)
                   DESC,
                 p.last_played DESC",
        )?;
        let rows = stmt
            .query_map([decay], |row| {
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

    /// Mark a song as pinned (manually downloaded, won't be auto-evicted).
    pub fn pin_offline(&self, video_id: &str) -> Result<()> {
        self.db.execute(
            "UPDATE offline SET pinned = 1 WHERE video_id = ?1",
            [video_id],
        )?;
        Ok(())
    }

    /// Let the automatic offline manager evict this song again if it no longer qualifies.
    pub fn unpin_offline(&self, video_id: &str) -> Result<()> {
        self.db.execute(
            "UPDATE offline SET pinned = 0 WHERE video_id = ?1",
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

    /// Return the temp-cached path if this song has been prefetched.
    pub fn get_temp_audio(&self, video_id: &str) -> Option<PathBuf> {
        let path = self.temp_dir.join(format!("{video_id}.audio"));
        path.exists().then_some(path)
    }

    /// Write prefetched audio to the temp directory.
    pub fn store_temp_audio(&self, video_id: &str, data: &[u8]) {
        let path = self.temp_dir.join(format!("{video_id}.audio"));
        let part_path = self
            .temp_dir
            .join(format!("{video_id}.{}.part", Uuid::new_v4()));

        let write_result =
            std::fs::write(&part_path, data).and_then(|_| std::fs::rename(&part_path, &path));
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&part_path);
            eprintln!("[PREFETCH] failed to write temp {video_id}: {e}");
        } else {
            eprintln!("[PREFETCH] cached {video_id} ({} KB)", data.len() / 1024);
        }
    }

    /// Clear all temp prefetch files.
    pub fn clear_temp(&self) {
        if let Ok(entries) = std::fs::read_dir(&self.temp_dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    pub fn remove_temp_audio(&self, video_id: &str) {
        if let Some(path) = self.get_temp_audio(video_id) {
            if let Err(err) = std::fs::remove_file(&path) {
                eprintln!("[PREFETCH] failed to evict temp {video_id}: {err}");
            } else {
                eprintln!("[PREFETCH] evicted temp {video_id}");
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/offline_cache_files_offline.rs"]
mod tests;
