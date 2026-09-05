use super::{
    AppSettings, OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS, PIPEWIRE_DUCKING_VOLUME_CLI_MAX,
    apply_config_value, config_description, cross_device_queue_is_singleton_seed,
    format_config_value, settings_eviction_warning, should_use_local_resume_for_cross_device,
};
use crate::ipc::{PersistedPlaybackState, PlaybackSnapshot};
use crate::models::{CrossDeviceQueue, Track};
use crate::offline::MusicCache;
use std::fs;
use uuid::Uuid;

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
fn config_set_supports_coordination_filter_modes_and_lists() {
    let mut settings = AppSettings::default();

    apply_config_value(&mut settings, "other_mpris_filter_mode", "whitelist").unwrap();
    apply_config_value(
        &mut settings,
        "other_mpris_filter_list",
        " Firefox , firefox, mpv ",
    )
    .unwrap();
    apply_config_value(&mut settings, "pipewire_filter_mode", "blacklist").unwrap();
    apply_config_value(&mut settings, "pipewire_filter_list", "Firefox,Game").unwrap();

    assert_eq!(
        format_config_value(&settings, "other_mpris_filter_mode").unwrap(),
        "whitelist"
    );
    assert_eq!(
        format_config_value(&settings, "other_mpris_filter_list").unwrap(),
        "Firefox,mpv"
    );
    assert_eq!(
        format_config_value(&settings, "pipewire_filter_mode").unwrap(),
        "blacklist"
    );
    assert_eq!(
        format_config_value(&settings, "pipewire_filter_list").unwrap(),
        "Firefox,Game"
    );
}

#[test]
fn config_descriptions_explain_cli_only_ranges() {
    let description = config_description("pipewire_ducking_volume");

    assert!(description.contains("GTK settings UI range: 0.0-1.0."));
    assert!(description.contains("CLI range: 0.0-2.0."));
    assert!(description.contains("CLI-only"));
}

#[test]
fn config_eviction_warning_reports_auto_download_impact() {
    let base = std::env::temp_dir().join(format!("static-cli-test-{}", Uuid::new_v4()));
    let raw_root = base.to_string_lossy().to_string();
    let cache = MusicCache::new_with_storage_root(Some(&raw_root)).expect("cache init");
    cache
        .record_play("video123", "Track", "Artist", None, 0)
        .expect("record play");
    cache
        .store_audio("video123", b"audio bytes", None)
        .expect("store audio");

    let current = AppSettings {
        auto_manage_offline_cache: true,
        auto_cache_listen_again: false,
        offline_cache_target: 1,
        offline_rewatch_threshold: 0,
        ..AppSettings::default()
    };
    let next = AppSettings {
        auto_manage_offline_cache: false,
        ..current.clone()
    };

    let warning = settings_eviction_warning(&cache, &current, &next, "auto_manage_offline_cache")
        .expect("warning calculation")
        .expect("warning");
    assert_eq!(warning.impact, 1);
    assert!(warning.message.contains("1 auto-downloaded song"));

    let _ = fs::remove_dir_all(base);
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

#[test]
fn tied_remote_timestamp_keeps_local_resume() {
    let queue = test_cross_device_queue("PLSeq4utvoqwDKLWx1H_2h7gGXUsGR9Q5A", 0);
    let local = test_resume("different-local-track", 1_778_904_892_070);

    assert!(should_use_local_resume_for_cross_device(
        Some(&queue),
        Some(&local)
    ));
}

#[test]
fn undated_remote_queue_cannot_outrank_local_resume() {
    let mut queue = test_cross_device_queue("PLSeq4utvoqwDKLWx1H_2h7gGXUsGR9Q5A", 0);
    queue.last_watch_timestamp_millis = None;
    let local = test_resume("different-local-track", 1);

    assert!(should_use_local_resume_for_cross_device(
        Some(&queue),
        Some(&local)
    ));
}

#[test]
fn playlist_positions_reject_zero_out_of_range_and_non_numbers() {
    assert_eq!(super::playlist_position("1", 3).unwrap(), 0);
    assert_eq!(super::playlist_position("3", 3).unwrap(), 2);
    for raw in ["0", "4", "-1", "song"] {
        assert!(super::playlist_position(raw, 3).is_err());
    }
    assert!(super::playlist_position("1", 0).is_err());
}

#[test]
fn playlist_moves_use_successor_in_queue_after_removal() {
    // Exhaust every source/destination pair, including first and last slots.
    for len in 1..=8 {
        for from in 0..len {
            for to in 0..len {
                let successor = super::playlist_move_successor(len, from, to);
                let mut reordered: Vec<_> = (0..len).filter(|index| *index != from).collect();
                let insertion = successor
                    .map(|next| reordered.iter().position(|index| *index == next).unwrap())
                    .unwrap_or(reordered.len());
                reordered.insert(insertion, from);
                assert_eq!(reordered[to], from);
                assert_eq!(reordered.len(), len);
            }
        }
    }
}

#[test]
fn playlist_help_explains_occurrences_and_radio() {
    let help = super::help_text();
    assert!(help.contains("static playlist from-radio <title>"));
    assert!(help.contains("Playlist positions are 1-based"));
    assert!(help.contains("New playlists are private"));
}

#[test]
fn playlist_deletion_requires_exact_explicit_confirmation_flag() {
    let args = |values: &[&str]| {
        values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
    };
    assert!(!super::playlist_delete_confirmed(&args(&["PLtest"])).unwrap());
    assert!(super::playlist_delete_confirmed(&args(&["PLtest", "--yes"])).unwrap());
    for values in [
        vec![],
        vec![""],
        vec!["--yes"],
        vec!["--yes", "PLtest"],
        vec!["PLtest", "yes"],
        vec!["PLtest", "--yes=true"],
        vec!["PLtest", "--yes", "extra"],
    ] {
        assert!(super::playlist_delete_confirmed(&args(&values)).is_err());
    }
}

#[test]
fn playlist_delete_and_listen_again_help_describe_server_actions() {
    let help = super::help_text();
    assert!(help.contains("static playlist delete <playlist id> [--yes]"));
    assert!(help.contains("Playlist deletion is permanent"));
    assert!(help.contains("static playlist pin <playlist id>"));
    assert!(help.contains("static playlist unpin <playlist id>"));
    assert!(help.contains("static pin [ytm/yt url | video id | search term]"));
    assert!(help.contains("Pin/unpin update YouTube Music Listen again"));
}
