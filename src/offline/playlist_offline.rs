use anyhow::Result;
use rusqlite::OptionalExtension;

use super::store_offline::{MusicCache, epoch_secs};

impl MusicCache {
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

    /// Conventional on-disk path for a playlist's cached cover art. The file
    /// may not exist; callers check `.exists()`. Kept in sync with the write
    /// in `download_all_task` and the offline cover repair.
    pub fn offline_playlist_cover_path(&self, playlist_id: &str) -> std::path::PathBuf {
        self.art_dir.join(format!("playlist_{playlist_id}.jpg"))
    }

    /// Update only the stored (remote) cover URL for an offline playlist,
    /// leaving its tracks untouched. Used by the offline cover repair to
    /// restore a recoverable `https://` URL after a stale `file://` was stored.
    pub fn update_offline_playlist_thumbnail(
        &self,
        playlist_id: &str,
        thumbnail_url: &str,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE offline_playlists SET thumbnail_url = ?2 WHERE playlist_id = ?1",
            rusqlite::params![playlist_id, thumbnail_url],
        )?;
        Ok(())
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
}
