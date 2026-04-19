use std::path::PathBuf;

use anyhow::Result;

use super::settings_offline::AppSettings;
use super::store_offline::{MusicCache, SECONDS_PER_DAY, epoch_secs};

impl MusicCache {
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

    pub fn auto_offline_candidates(
        &self,
        settings: &AppSettings,
        limit: usize,
    ) -> Result<Vec<String>> {
        if !settings.auto_cache_listen_again && (!settings.auto_manage_offline_cache || limit == 0)
        {
            return Ok(Vec::new());
        }

        let now = epoch_secs();
        let min_total_plays = settings.offline_rewatch_threshold.saturating_add(1).max(1);
        let decay_window_days = settings.score_decay_window_days.max(1) as f64;
        let min_score = settings.minimum_offline_score.max(0.0);
        let mut candidate_ids = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        if settings.auto_cache_listen_again {
            for video_id in self.listen_again_candidate_ids(settings)? {
                if seen_ids.insert(video_id.clone()) {
                    candidate_ids.push(video_id);
                }
            }
        }

        if !settings.auto_manage_offline_cache || limit == 0 {
            return Ok(candidate_ids);
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

        let mut heuristic_count = 0usize;
        for (video_id, _) in ranked {
            if heuristic_count >= limit {
                break;
            }
            if seen_ids.insert(video_id.clone()) {
                candidate_ids.push(video_id);
                heuristic_count += 1;
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

    pub fn store_listen_again_snapshot(
        &self,
        video_ids: &[String],
        settings: &AppSettings,
    ) -> Result<()> {
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
        self.prune_stale_listen_again_candidates(settings)?;
        Ok(())
    }

    pub fn offline_listen_again_track_ids(&self, settings: &AppSettings) -> Result<Vec<String>> {
        let cutoff = self.listen_again_cutoff(settings);
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

    fn listen_again_candidate_ids(&self, settings: &AppSettings) -> Result<Vec<String>> {
        let cutoff = self.listen_again_cutoff(settings);
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

    fn prune_stale_listen_again_candidates(&self, settings: &AppSettings) -> Result<()> {
        let cutoff = self.listen_again_cutoff(settings);
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

    fn listen_again_cutoff(&self, settings: &AppSettings) -> i64 {
        epoch_secs() - (settings.listen_again_cache_days.max(1) as i64 * SECONDS_PER_DAY)
    }
}
