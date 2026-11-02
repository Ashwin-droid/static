use std::path::Path;

use anyhow::{Context, Result};

use super::store_offline::MusicCache;

impl MusicCache {
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
        let old_db_path = old_base.join("data.db");
        let old_offline_dir = self.offline_dir.clone();
        let old_art_dir = self.art_dir.clone();
        let old_temp_dir = self.temp_dir.clone();

        let (new_offline_dir, new_art_dir, new_temp_dir) = Self::ensure_storage_layout(&new_base)?;

        // The old root is only ever read: it can be a read-only mount (an NTFS
        // partition after an unclean Windows shutdown). Files it won't let go of
        // are copied, and the database is snapshotted and rewritten on the new side.
        move_dir_contents(&old_art_dir, &new_art_dir)?;
        move_dir_contents(&old_offline_dir, &new_offline_dir)?;
        move_dir_contents(&old_temp_dir, &new_temp_dir)?;

        snapshot_database(&self.db, &new_base)?;
        let db =
            Self::open_database(&new_base).context("Failed to open migrated cache database")?;
        db.execute(
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

        db.execute(
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

        // Saved queues carry artwork and audio paths under the old root too; the
        // daemon creates this table lazily.
        let has_playback_state: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'playback_state')",
            [],
            |row| row.get(0),
        )?;
        if has_playback_state {
            db.execute(
                "UPDATE playback_state SET payload = REPLACE(payload, ?1, ?2)",
                rusqlite::params![
                    format!("{}/", old_base.display()),
                    format!("{}/", new_base.display()),
                ],
            )?;
        }

        self.db = db;
        self.base_dir = new_base;
        self.offline_dir = new_offline_dir;
        self.art_dir = new_art_dir;
        self.temp_dir = new_temp_dir;

        let _ = std::fs::remove_file(old_db_path);
        let _ = std::fs::remove_dir(old_art_dir);
        let _ = std::fs::remove_dir(old_offline_dir);
        let _ = std::fs::remove_dir(old_temp_dir);
        let _ = std::fs::remove_dir(old_base);
        Ok(())
    }
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
                // A read-only source keeps its copy.
                let _ = std::fs::remove_file(&source);
            }
        }
    }
    Ok(())
}

/// Writes a consistent copy of `db` to `base/data.db` without writing to `db`'s
/// own file, replacing whatever database the new root already had.
fn snapshot_database(db: &rusqlite::Connection, base: &Path) -> Result<()> {
    let staged = base.join("data.db.migrating");
    let _ = std::fs::remove_file(&staged);
    db.execute("VACUUM INTO ?1", [staged.to_string_lossy().as_ref()])
        .context("Failed to copy the cache database into the new root")?;
    // A leftover journal would be replayed into the new file on open.
    for stale in ["data.db-journal", "data.db-wal", "data.db-shm"] {
        let _ = std::fs::remove_file(base.join(stale));
    }
    std::fs::rename(&staged, base.join("data.db"))?;
    Ok(())
}
