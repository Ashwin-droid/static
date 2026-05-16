use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::store_offline::MusicCache;

#[derive(Debug)]
struct OfflineRow {
    audio_path: String,
    art_path: String,
    cached_at: i64,
    pinned: i64,
}

impl MusicCache {
    pub fn replace_video_id(&self, old_video_id: &str, new_video_id: &str) -> Result<()> {
        if old_video_id == new_video_id {
            return Ok(());
        }

        let offline_update = self.prepare_offline_video_id_update(old_video_id, new_video_id)?;
        let tx = self.db.unchecked_transaction()?;

        tx.execute(
            "INSERT INTO plays (video_id, title, artist, thumbnail_url, play_count, listen_secs, last_played)
             SELECT ?2, title, artist, thumbnail_url, play_count, listen_secs, last_played
             FROM plays WHERE video_id = ?1
             ON CONFLICT(video_id) DO UPDATE SET
                 play_count = plays.play_count + excluded.play_count,
                 listen_secs = plays.listen_secs + excluded.listen_secs,
                 last_played = MAX(plays.last_played, excluded.last_played),
                 title = CASE
                     WHEN excluded.last_played >= plays.last_played THEN excluded.title
                     ELSE plays.title
                 END,
                 artist = CASE
                     WHEN excluded.last_played >= plays.last_played THEN excluded.artist
                     ELSE plays.artist
                 END,
                 thumbnail_url = COALESCE(plays.thumbnail_url, excluded.thumbnail_url)",
            params![old_video_id, new_video_id],
        )?;
        tx.execute("DELETE FROM plays WHERE video_id = ?1", [old_video_id])?;

        tx.execute(
            "INSERT INTO listen_again_candidates (video_id, last_recommended_at)
             SELECT ?2, last_recommended_at
             FROM listen_again_candidates WHERE video_id = ?1
             ON CONFLICT(video_id) DO UPDATE SET
                 last_recommended_at = MAX(
                     listen_again_candidates.last_recommended_at,
                     excluded.last_recommended_at
                 )",
            params![old_video_id, new_video_id],
        )?;
        tx.execute(
            "DELETE FROM listen_again_candidates WHERE video_id = ?1",
            [old_video_id],
        )?;

        tx.execute(
            "INSERT INTO offline_sponsorblock (video_id, segments_json, cached_at)
             SELECT ?2, segments_json, cached_at
             FROM offline_sponsorblock WHERE video_id = ?1
             ON CONFLICT(video_id) DO UPDATE SET
                 segments_json = CASE
                     WHEN excluded.cached_at >= offline_sponsorblock.cached_at THEN excluded.segments_json
                     ELSE offline_sponsorblock.segments_json
                 END,
                 cached_at = MAX(offline_sponsorblock.cached_at, excluded.cached_at)",
            params![old_video_id, new_video_id],
        )?;
        tx.execute(
            "DELETE FROM offline_sponsorblock WHERE video_id = ?1",
            [old_video_id],
        )?;

        match offline_update {
            Some(OfflineVideoIdUpdate::Move {
                audio_path,
                art_path,
                cached_at,
                pinned,
            }) => {
                tx.execute(
                    "INSERT OR REPLACE INTO offline (video_id, audio_path, art_path, cached_at, pinned)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![new_video_id, audio_path, art_path, cached_at, pinned],
                )?;
                tx.execute("DELETE FROM offline WHERE video_id = ?1", [old_video_id])?;
            }
            Some(OfflineVideoIdUpdate::DropOld) => {
                tx.execute("DELETE FROM offline WHERE video_id = ?1", [old_video_id])?;
            }
            None => {}
        }

        let mut playlist_stmt = tx.prepare(
            "SELECT playlist_id, position
             FROM offline_playlist_tracks
             WHERE video_id = ?1",
        )?;
        let playlist_rows = playlist_stmt
            .query_map([old_video_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(playlist_stmt);

        for (playlist_id, old_position) in playlist_rows {
            let existing_position: Option<i64> = tx
                .query_row(
                    "SELECT position FROM offline_playlist_tracks
                     WHERE playlist_id = ?1 AND video_id = ?2",
                    params![playlist_id, new_video_id],
                    |row| row.get(0),
                )
                .optional()?;

            if let Some(existing_position) = existing_position {
                tx.execute(
                    "UPDATE offline_playlist_tracks
                     SET position = ?3
                     WHERE playlist_id = ?1 AND video_id = ?2",
                    params![
                        playlist_id,
                        new_video_id,
                        existing_position.min(old_position)
                    ],
                )?;
                tx.execute(
                    "DELETE FROM offline_playlist_tracks
                     WHERE playlist_id = ?1 AND video_id = ?2",
                    params![playlist_id, old_video_id],
                )?;
            } else {
                tx.execute(
                    "UPDATE offline_playlist_tracks
                     SET video_id = ?3
                     WHERE playlist_id = ?1 AND video_id = ?2",
                    params![playlist_id, old_video_id, new_video_id],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    fn prepare_offline_video_id_update(
        &self,
        old_video_id: &str,
        new_video_id: &str,
    ) -> Result<Option<OfflineVideoIdUpdate>> {
        let old_row = self.offline_row(old_video_id)?;
        let Some(old_row) = old_row else {
            return Ok(None);
        };

        if self.offline_row(new_video_id)?.is_some() {
            remove_file_if_present(&old_row.audio_path);
            remove_file_if_present(&old_row.art_path);
            return Ok(Some(OfflineVideoIdUpdate::DropOld));
        }

        let audio_path = move_cache_file_for_video_id(
            &old_row.audio_path,
            &self.offline_dir,
            &format!("{new_video_id}.audio"),
        )?;
        let art_path = move_cache_file_for_video_id(
            &old_row.art_path,
            &self.art_dir,
            &format!("{new_video_id}.jpg"),
        )?;

        Ok(Some(OfflineVideoIdUpdate::Move {
            audio_path,
            art_path,
            cached_at: old_row.cached_at,
            pinned: old_row.pinned,
        }))
    }

    fn offline_row(&self, video_id: &str) -> Result<Option<OfflineRow>> {
        self.db
            .query_row(
                "SELECT audio_path, art_path, cached_at, pinned
                 FROM offline WHERE video_id = ?1",
                [video_id],
                |row| {
                    Ok(OfflineRow {
                        audio_path: row.get(0)?,
                        art_path: row.get(1)?,
                        cached_at: row.get(2)?,
                        pinned: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }
}

enum OfflineVideoIdUpdate {
    Move {
        audio_path: String,
        art_path: String,
        cached_at: i64,
        pinned: i64,
    },
    DropOld,
}

fn move_cache_file_for_video_id(
    current: &str,
    target_dir: &Path,
    target_name: &str,
) -> Result<String> {
    if current.is_empty() {
        return Ok(String::new());
    }

    let current_path = PathBuf::from(current);
    let target_path = target_dir.join(target_name);
    if current_path == target_path {
        return Ok(current.to_string());
    }

    if current_path.exists() {
        if target_path.exists() {
            remove_file_if_present(current);
        } else {
            if let Some(parent) = target_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&current_path, &target_path)?;
        }
    }

    Ok(target_path.to_string_lossy().into_owned())
}

fn remove_file_if_present(path: &str) {
    if !path.is_empty() {
        let _ = std::fs::remove_file(path);
    }
}
