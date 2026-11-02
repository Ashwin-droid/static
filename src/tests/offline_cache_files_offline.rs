use super::MusicCache;
use std::fs;
use uuid::Uuid;

#[test]
fn clear_temp_removes_prefetched_audio() {
    let base = std::env::temp_dir().join(format!("static-cache-test-{}", Uuid::new_v4()));
    let raw_root = base.to_string_lossy().to_string();

    let cache = MusicCache::new_with_storage_root(Some(&raw_root)).expect("cache init");
    cache.store_temp_audio("video123", b"hello world");
    assert!(cache.get_temp_audio("video123").is_some());

    cache.clear_temp();
    assert!(cache.get_temp_audio("video123").is_none());

    let _ = fs::remove_dir_all(base);
}

#[test]
fn replace_video_id_swaps_rows_and_cache_files_without_alias_table() {
    let base = std::env::temp_dir().join(format!("static-cache-test-{}", Uuid::new_v4()));
    let raw_root = base.to_string_lossy().to_string();

    let cache = MusicCache::new_with_storage_root(Some(&raw_root)).expect("cache init");
    cache
        .record_play(
            "old-video",
            "Old Title",
            "Old Artist",
            Some("https://thumb"),
            42,
        )
        .expect("record play");
    cache
        .store_audio("old-video", b"audio bytes", None)
        .expect("store audio");
    cache.pin_offline("old-video").expect("pin old video");
    cache
        .store_offline_playlist(
            "playlist",
            "Playlist",
            None,
            None,
            &["old-video".to_string()],
        )
        .expect("store playlist");

    let old_audio_path = base.join("offline/old-video.audio");
    assert!(old_audio_path.exists());

    cache
        .replace_video_id("old-video", "new-video")
        .expect("replace video id");

    assert_eq!(cache.get_track_meta("old-video"), None);
    assert_eq!(
        cache.get_track_meta("new-video"),
        Some(("Old Title".to_string(), "Old Artist".to_string()))
    );
    assert!(!cache.is_pinned_offline("old-video"));
    assert!(cache.is_pinned_offline("new-video"));
    assert!(!old_audio_path.exists());
    assert!(cache.get_cached_audio("old-video").is_none());

    let cached_path = cache
        .get_cached_audio("new-video")
        .expect("new id should own cached audio");
    assert_eq!(
        cached_path.file_name().and_then(|name| name.to_str()),
        Some("new-video.audio")
    );
    assert_eq!(
        fs::read(cached_path).expect("read migrated audio"),
        b"audio bytes"
    );
    assert_eq!(
        cache
            .offline_playlist_track_ids("playlist")
            .expect("playlist ids"),
        vec!["new-video".to_string()]
    );

    cache.store_temp_audio("old-video", b"prefetch bytes");
    assert!(cache.get_temp_audio("old-video").is_some());
    assert!(cache.get_temp_audio("new-video").is_none());

    let _ = fs::remove_dir_all(base);
}

#[test]
fn unpin_offline_returns_download_to_auto_managed_pool() {
    let base = std::env::temp_dir().join(format!("static-cache-test-{}", Uuid::new_v4()));
    let raw_root = base.to_string_lossy().to_string();

    let cache = MusicCache::new_with_storage_root(Some(&raw_root)).expect("cache init");
    cache
        .store_audio("video123", b"audio bytes", None)
        .expect("store audio");
    cache.pin_offline("video123").expect("pin video");
    assert!(cache.is_pinned_offline("video123"));

    cache.unpin_offline("video123").expect("unpin video");
    assert!(!cache.is_pinned_offline("video123"));

    let _ = fs::remove_dir_all(base);
}

#[test]
fn scored_offline_tracks_cover_the_whole_store_and_skip_missing_files() {
    let base = std::env::temp_dir().join(format!("static-cache-test-{}", Uuid::new_v4()));
    let raw_root = base.to_string_lossy().to_string();
    let cache = MusicCache::new_with_storage_root(Some(&raw_root)).expect("cache init");

    // Auto download with play history.
    for _ in 0..3 {
        cache
            .record_play("auto", "Auto Song", "Auto Artist", None, 60)
            .expect("record play");
    }
    cache
        .store_audio("auto", b"audio", None)
        .expect("store auto");
    // Manual download, never played.
    cache
        .store_audio("manual", b"audio", None)
        .expect("store manual");
    cache.pin_offline("manual").expect("pin manual");
    // Playlist download.
    cache
        .store_audio("playlist-song", b"audio", None)
        .expect("store playlist song");
    cache
        .store_offline_playlist(
            "playlist",
            "Playlist",
            None,
            None,
            &["playlist-song".to_string()],
        )
        .expect("store playlist");
    // Indexed, but the audio file is gone.
    cache
        .store_audio("ghost", b"audio", None)
        .expect("store ghost");
    fs::remove_file(base.join("offline/ghost.audio")).expect("remove ghost audio");

    let mut tracks = cache.scored_offline_tracks(30).expect("scored tracks");
    tracks.sort_by(|left, right| left.video_id.cmp(&right.video_id));
    let ids: Vec<&str> = tracks.iter().map(|track| track.video_id.as_str()).collect();
    assert_eq!(ids, ["auto", "manual", "playlist-song"]);

    assert_eq!(tracks[0].title, "Auto Song");
    assert_eq!(tracks[0].artist, "Auto Artist");
    assert!(tracks[0].score > 2.9 && tracks[0].score <= 3.0);
    // No play history: titled by id, scored zero.
    assert_eq!(tracks[1].title, "manual");
    assert_eq!(tracks[1].score, 0.0);

    let _ = fs::remove_dir_all(base);
}

#[test]
fn migrating_out_of_a_read_only_root_copies_everything_and_leaves_the_source() {
    let base = std::env::temp_dir().join(format!("static-cache-test-{}", Uuid::new_v4()));
    let old_root = base.join("old");
    let new_root = base.join("new");
    let old_raw = old_root.to_string_lossy().to_string();

    let cache = MusicCache::new_with_storage_root(Some(&old_raw)).expect("cache init");
    cache
        .record_play("video123", "Title", "Artist", None, 60)
        .expect("record play");
    cache
        .store_audio("video123", b"audio bytes", None)
        .expect("store audio");
    drop(cache);

    set_tree_read_only(&old_root, true);
    let migrated = MusicCache::new_with_storage_root(Some(&old_raw)).and_then(|mut cache| {
        cache.migrate_storage_root(&new_root.to_string_lossy())?;
        Ok(cache)
    });
    set_tree_read_only(&old_root, false);
    let cache = migrated.expect("migrate out of a read-only root");

    assert_eq!(cache.base_dir(), new_root.as_path());
    assert_eq!(
        cache.get_track_meta("video123"),
        Some(("Title".to_string(), "Artist".to_string()))
    );
    assert_eq!(
        cache.get_cached_audio("video123"),
        Some(new_root.join("offline/video123.audio"))
    );
    assert!(old_root.join("data.db").exists());
    assert!(old_root.join("offline/video123.audio").exists());

    let _ = fs::remove_dir_all(base);
}

fn set_tree_read_only(path: &std::path::Path, read_only: bool) {
    use std::os::unix::fs::PermissionsExt;
    let is_dir = path.is_dir();
    if is_dir {
        for entry in fs::read_dir(path).expect("read dir") {
            set_tree_read_only(&entry.expect("dir entry").path(), read_only);
        }
    }
    let mode = match (is_dir, read_only) {
        (true, true) => 0o555,
        (true, false) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set permissions");
}
