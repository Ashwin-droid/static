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
