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
