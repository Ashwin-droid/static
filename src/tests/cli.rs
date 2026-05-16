use super::{
    AppSettings, OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS, PIPEWIRE_DUCKING_VOLUME_CLI_MAX,
    apply_config_value, config_description, cross_device_queue_is_singleton_seed,
    should_use_local_resume_for_cross_device,
};
use crate::ipc::{PersistedPlaybackState, PlaybackSnapshot};
use crate::models::{CrossDeviceQueue, Track};

#[test]
fn config_set_preserves_cli_only_delay_values() {
    let mut settings = AppSettings::default();

    apply_config_value(&mut settings, "other_mpris_halt_auto_resume_secs", "300").unwrap();
    apply_config_value(&mut settings, "pipewire_pause_halt_auto_resume_secs", "300").unwrap();
    apply_config_value(&mut settings, "pipewire_ducking_threshold_secs", "300").unwrap();

    assert!(settings.other_mpris_halt_auto_resume_secs > OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS);
    assert_eq!(settings.pipewire_pause_halt_auto_resume_secs, 300);
    assert_eq!(settings.pipewire_ducking_threshold_secs, 300);
}

#[test]
fn config_set_supports_boosted_pipewire_ducking_volume() {
    let mut settings = AppSettings::default();

    apply_config_value(&mut settings, "pipewire_ducking_volume", "1.75").unwrap();
    assert!((settings.pipewire_ducking_volume - 1.75).abs() < 0.0001);

    apply_config_value(&mut settings, "pipewire_ducking_volume", "9.0").unwrap();
    assert_eq!(
        settings.pipewire_ducking_volume,
        PIPEWIRE_DUCKING_VOLUME_CLI_MAX
    );
}

#[test]
fn config_descriptions_explain_cli_only_ranges() {
    let description = config_description("pipewire_ducking_volume");

    assert!(description.contains("GTK settings UI range: 0.0-1.0."));
    assert!(description.contains("CLI range: 0.0-2.0."));
    assert!(description.contains("CLI-only"));
}

fn test_track(video_id: &str) -> Track {
    Track {
        video_id: video_id.to_string(),
        title: format!("Track {video_id}"),
        artists: Vec::new(),
        album: None,
        duration: None,
        thumbnail_url: None,
        playback_playlist_id: None,
        playback_params: None,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
    }
}

fn test_cross_device_queue(playlist_id: &str, selected_index: usize) -> CrossDeviceQueue {
    CrossDeviceQueue {
        source_label: Some("From your Android device".to_string()),
        last_watch_timestamp_millis: Some(1_778_904_892_070),
        playlist_id: Some(playlist_id.to_string()),
        tracks: vec![test_track("IGO9PySxA1w"), test_track("tail-track")],
        selected_index,
    }
}

fn test_snapshot(video_id: &str) -> PlaybackSnapshot {
    PlaybackSnapshot {
        queue: vec![test_track(video_id)],
        queue_index: 0,
        ..PlaybackSnapshot::default()
    }
}

fn test_resume(video_id: &str, saved_at_unix_millis: u64) -> PersistedPlaybackState {
    PersistedPlaybackState {
        snapshot: test_snapshot(video_id),
        saved_at_unix_millis,
    }
}

#[test]
fn cross_device_rd_amvm_first_item_is_singleton_seed() {
    let queue = test_cross_device_queue("RDAMVMIGO9PySxA1w", 0);

    assert!(cross_device_queue_is_singleton_seed(&queue));
}

#[test]
fn matching_cross_device_singleton_prefers_local_resume() {
    let queue = test_cross_device_queue("RDAMVMIGO9PySxA1w", 0);
    let local = test_resume("IGO9PySxA1w", 1_778_904_000_000);

    assert!(should_use_local_resume_for_cross_device(
        Some(&queue),
        Some(&local)
    ));
}

#[test]
fn playlist_cross_device_queue_does_not_override_with_local_resume() {
    let queue = test_cross_device_queue("PLSeq4utvoqwDKLWx1H_2h7gGXUsGR9Q5A", 0);
    let local = test_resume("IGO9PySxA1w", 1_778_904_000_000);

    assert!(!should_use_local_resume_for_cross_device(
        Some(&queue),
        Some(&local)
    ));
}

#[test]
fn missing_cross_device_queue_falls_back_to_local_resume() {
    let local = test_resume("IGO9PySxA1w", 1_778_904_000_000);

    assert!(should_use_local_resume_for_cross_device(None, Some(&local)));
}

#[test]
fn newer_local_resume_overrides_remote_cross_device_queue() {
    let queue = test_cross_device_queue("PLSeq4utvoqwDKLWx1H_2h7gGXUsGR9Q5A", 0);
    let local = test_resume("different-local-track", 1_778_905_000_000);

    assert!(should_use_local_resume_for_cross_device(
        Some(&queue),
        Some(&local)
    ));
}

#[test]
fn older_local_resume_does_not_override_remote_cross_device_queue() {
    let queue = test_cross_device_queue("PLSeq4utvoqwDKLWx1H_2h7gGXUsGR9Q5A", 0);
    let local = test_resume("different-local-track", 1_778_904_000_000);

    assert!(!should_use_local_resume_for_cross_device(
        Some(&queue),
        Some(&local)
    ));
}
