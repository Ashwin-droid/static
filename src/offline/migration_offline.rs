use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

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
        let new_db_path = new_base.join("data.db");

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

        let placeholder = Connection::open_in_memory().context("Failed to prepare DB migration")?;
        let old_db = std::mem::replace(&mut self.db, placeholder);
        match old_db.close() {
            Ok(()) => {}
            Err((conn, err)) => {
                self.db = conn;
                return Err(err).context("Failed to close cache database before migration");
            }
        }

        copy_file_contents(&old_db_path, &new_db_path)?;
        match Self::open_database(&new_base) {
            Ok(db) => {
                self.db = db;
            }
            Err(err) => {
                self.db = Self::open_database(&old_base)
                    .context("Failed to reopen original cache database after migration error")?;
                return Err(err).context("Failed to open migrated cache database");
            }
        }

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
                std::fs::remove_file(&source)?;
            }
        }
    }
    Ok(())
}

fn copy_file_contents(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() || from == to {
        return Ok(());
    }

    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if to.exists() {
        let _ = std::fs::remove_file(to);
    }

    std::fs::copy(from, to)?;
    Ok(())
}
