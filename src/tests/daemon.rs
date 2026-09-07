use super::{
    ActiveTrack, DESYNC_PLAYBACK_START_DELAY, MIN_LISTEN_SECS, MprisHaltAutoPeerTransition,
    PLAYBACK_START_TIMEOUT, PREVIOUS_TRACK_THRESHOLD_MILLIS, PendingMprisTransportDecision,
    PendingMprisTransportIntent, PipeWireSuppressionMode, PlaybackSource, PlaybackStartMode,
    PlaybackTransition, PreviousAction, VolumeRamp, VolumeRampCompletion,
    apply_radio_chips_from_refresh, cache_media_download_retry_delay,
    cached_restricted_pick_offset, current_listened_millis, dedupe_radio_recommendations,
    effective_queue_len, extend_radio_touched_for_insert, has_reached_track_end,
    hide_timing_for_track, mpris_app_key_from_bus, mpris_halt_auto_peer_transition,
    mpris_position_millis_for_track, natural_end_target_index, next_repeat_state_after_natural_end,
    note_sponsorblock_auto_skip, pause_request_already_satisfied, pending_mpris_transport_decision,
    pending_playback_start_is_ready, pipewire_unduck_can_replace_volume_ramp,
    playback_position_confirms_start, playback_start_stalled, prefetch_candidate_indices,
    previous_action, previous_action_position_millis_for_track, promote_untouched_pick,
    radio_chips_with_replacement, radio_pick_weights, rearm_track_repeat_loops_for_snapshot,
    refresh_shuffle_up_next_id, remove_cached_if_unwanted_after_playlist_sync,
    resolve_pending_playback_index, resolve_pipewire_suppression_mode,
    resolve_shuffle_advance_pick, restore_order, should_count_paused_idle_for_shutdown,
    should_desync_playback_start, should_emit_live_mpris_seeked_hint,
    should_evict_cache_after_startup_stall, should_expose_mpris_halt_auto_sidechannel,
    should_resume_after_external_mpris_pause, should_resume_after_pipewire_pause,
    should_retry_after_offline_cache_failure, should_sync_live_mpris_position,
    should_wait_for_radio_extension_at_natural_end, title_similarity, tracking_retry_delay,
    valid_requested_volume, volume_ramp_is_complete, volume_ramp_value, weighted_index,
};
use crate::ipc::{ActiveTrackSnapshot, PlaybackSnapshot, RepeatMode};
use crate::models::{Artist, Track};
use crate::models::{RadioChip, RadioVirtualPlaylist};
use crate::network::{SongRating, SponsorSegment};
use crate::offline::{AppSettings, CoordinationMode, MusicCache};
use gstreamer::ClockTime;
use std::collections::HashSet;
use std::fs;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn test_active_track(started_at: Instant) -> ActiveTrack {
    ActiveTrack {
        video_id: "test-video".to_string(),
        title: "Test Track".to_string(),
        artist: "Test Artist".to_string(),
        thumb_url: None,
        playback_source: PlaybackSource::RemoteStream,
        reporting_enabled: true,
        rating: SongRating::Indifferent,
        tracking: None,
        started_at,
        last_position_millis: 0,
        sponsorblock_auto_skip_offset_millis: 0,
        sponsorblock_skipped_segment_ids: Default::default(),
        startup_seek_millis: None,
        startup_retries: 0,
        awaiting_fresh_position: true,
    }
}

fn test_repeat_settings(after: u32) -> AppSettings {
    AppSettings {
        auto_release_track_loops: true,
        auto_release_track_loops_after: after.max(1),
        ..AppSettings::default()
    }
}

fn test_chip(label: &str, playlist_id: &str, params: Option<&str>) -> RadioChip {
    RadioChip {
        label: label.to_string(),
        playlist_id: playlist_id.to_string(),
        params: params.map(str::to_string),
    }
}

fn test_track(video_id: &str) -> Track {
    Track {
        video_id: video_id.to_string(),
        title: video_id.to_string(),
        artists: vec![Artist {
            name: "Artist".to_string(),
            id: None,
        }],
        album: None,
        duration: None,
        thumbnail_url: None,
        playback_playlist_id: None,
        playback_params: None,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
    }
}

fn test_sponsor_segment(uuid: &str, start_seconds: f64, end_seconds: f64) -> SponsorSegment {
    SponsorSegment {
        uuid: uuid.to_string(),
        category: "intro".to_string(),
        action_type: "skip".to_string(),
        segment: vec![start_seconds, end_seconds],
    }
}

#[test]
fn previous_restarts_current_track_on_first_item_before_threshold() {
    assert_eq!(
        previous_action(
            3,
            0,
            RepeatMode::Off,
            Some(PREVIOUS_TRACK_THRESHOLD_MILLIS),
            true
        ),
        PreviousAction::RestartCurrent
    );
}

#[test]
fn previous_moves_to_prior_track_before_threshold_when_available() {
    assert_eq!(
        previous_action(3, 1, RepeatMode::Off, Some(1_500), true),
        PreviousAction::PlayIndex(0)
    );
}

#[test]
fn previous_wraps_in_repeat_all_mode() {
    assert_eq!(
        previous_action(3, 0, RepeatMode::All, Some(1_500), true),
        PreviousAction::PlayIndex(2)
    );
}

#[test]
fn previous_restarts_current_track_after_threshold() {
    assert_eq!(
        previous_action(
            3,
            1,
            RepeatMode::Off,
            Some(PREVIOUS_TRACK_THRESHOLD_MILLIS + 1),
            true
        ),
        PreviousAction::RestartCurrent
    );
}

#[test]
fn previous_position_uses_resumed_seekbar_position_before_fresh_sample() {
    let started_at = Instant::now() - Duration::from_millis(500);
    let mut track = test_active_track(started_at);
    track.startup_seek_millis = Some(180_000);

    assert_eq!(
        previous_action_position_millis_for_track(&track, Some(0), None),
        180_000
    );
}

#[test]
fn previous_position_treats_hidden_startup_seekbar_as_zero() {
    let started_at = Instant::now() - Duration::from_millis(500);
    let track = test_active_track(started_at);

    assert_eq!(
        previous_action_position_millis_for_track(&track, Some(75_000), None),
        0
    );
}

#[test]
fn previous_position_subtracts_actual_sponsorblock_skip() {
    let started_at = Instant::now() - Duration::from_secs(2);
    let mut track = test_active_track(started_at);
    track.awaiting_fresh_position = false;
    track.last_position_millis = 8_050;
    track.sponsorblock_auto_skip_offset_millis = 8_050;

    assert!(
        previous_action_position_millis_for_track(&track, Some(8_050), None)
            < PREVIOUS_TRACK_THRESHOLD_MILLIS
    );
}

#[test]
fn previous_position_keeps_raw_seekbar_when_sponsorblock_did_not_skip() {
    let started_at = Instant::now() - Duration::from_secs(2);
    let mut track = test_active_track(started_at);
    track.awaiting_fresh_position = false;
    track.last_position_millis = 8_050;

    assert_eq!(
        previous_action_position_millis_for_track(&track, Some(8_050), None),
        8_050
    );
}

#[test]
fn sponsorblock_skip_offset_records_each_segment_once() {
    let started_at = Instant::now() - Duration::from_secs(2);
    let mut track = test_active_track(started_at);
    let segment = test_sponsor_segment("intro-1", 0.0, 8.0);

    note_sponsorblock_auto_skip(&mut track, &segment, 250, 8_050);
    note_sponsorblock_auto_skip(&mut track, &segment, 250, 8_050);

    assert_eq!(track.sponsorblock_auto_skip_offset_millis, 7_800);
}

#[test]
fn prefetch_candidates_include_previous_before_upcoming() {
    assert_eq!(prefetch_candidate_indices(6, 3, 2, None), vec![2, 4, 5]);
}

#[test]
fn prefetch_candidates_do_not_wrap_before_queue_start() {
    assert_eq!(prefetch_candidate_indices(3, 0, 2, None), vec![1, 2]);
}

#[test]
fn prefetch_candidates_lead_with_the_pre_decided_shuffle_pick() {
    // The pick downloads first; previous and the queue-order look-ahead follow,
    // deduped (Android parity: PlaybackLogic.prefetchCandidateIndices).
    assert_eq!(
        prefetch_candidate_indices(10, 3, 3, Some(8)),
        vec![8, 2, 4, 5, 6]
    );
    assert_eq!(
        prefetch_candidate_indices(10, 3, 3, Some(4)),
        vec![4, 2, 5, 6]
    );
    // An out-of-range pick (stale after a queue mutation) is ignored, not
    // crashed on.
    assert_eq!(
        prefetch_candidate_indices(10, 3, 3, Some(12)),
        vec![2, 4, 5, 6]
    );
    assert_eq!(prefetch_candidate_indices(10, 3, 3, None), vec![2, 4, 5, 6]);
}

#[test]
fn repeat_all_radio_uses_touched_len_as_loop_boundary() {
    assert_eq!(effective_queue_len(5, RepeatMode::All, Some(1)), 1);
    assert_eq!(effective_queue_len(5, RepeatMode::All, Some(3)), 3);
}

#[test]
fn radio_recommendations_drop_tracks_already_in_virtual_prefix() {
    let prefix = vec![test_track("seed"), test_track("already-touched")];
    let deduped = dedupe_radio_recommendations(
        &prefix,
        vec![
            test_track("already-touched"),
            test_track("fresh"),
            test_track("fresh"),
        ],
    );

    assert_eq!(deduped, vec![test_track("fresh")]);
}

#[test]
fn queue_insert_extends_radio_touched_boundary_in_daemon_policy() {
    let mut radio = Some(RadioVirtualPlaylist {
        source_video_id: "seed".to_string(),
        touched_len: 2,
        chips: Vec::new(),
        selected_chip_index: None,
    });

    extend_radio_touched_for_insert(&mut radio, 1, 2);

    assert_eq!(radio.expect("radio queue").touched_len, 4);
}

#[test]
fn radio_chip_refresh_preserves_existing_chips_when_refresh_is_empty() {
    let existing = vec![test_chip("All", "RDseed", None)];

    assert_eq!(
        radio_chips_with_replacement(&existing, Vec::new()),
        existing
    );
}

#[test]
fn radio_chip_refresh_replaces_chips_and_defaults_selection() {
    let replacement = vec![
        test_chip("All", "RDseed", None),
        test_chip("Focus", "RDfocus", Some("focus-params")),
    ];
    let mut radio = RadioVirtualPlaylist {
        source_video_id: "seed".to_string(),
        touched_len: 1,
        chips: Vec::new(),
        selected_chip_index: None,
    };

    assert!(apply_radio_chips_from_refresh(
        &mut radio,
        replacement.clone()
    ));
    assert_eq!(radio.chips, replacement);
    assert_eq!(radio.selected_chip_index, Some(0));
}

#[test]
fn radio_chip_refresh_keeps_selected_playlist_when_reordered() {
    let mut radio = RadioVirtualPlaylist {
        source_video_id: "seed".to_string(),
        touched_len: 1,
        chips: vec![
            test_chip("All", "RDseed", None),
            test_chip("Focus", "RDfocus", Some("focus-params")),
        ],
        selected_chip_index: Some(1),
    };
    let replacement = vec![
        test_chip("Focus", "RDfocus", Some("focus-params")),
        test_chip("All", "RDseed", None),
    ];

    assert!(apply_radio_chips_from_refresh(&mut radio, replacement));
    assert_eq!(radio.selected_chip_index, Some(0));
}

#[test]
fn radio_chip_refresh_clamps_stale_selected_chip_index() {
    let mut radio = RadioVirtualPlaylist {
        source_video_id: "seed".to_string(),
        touched_len: 1,
        chips: vec![test_chip("All", "RDseed", None)],
        selected_chip_index: Some(4),
    };

    assert!(apply_radio_chips_from_refresh(&mut radio, Vec::new()));
    assert_eq!(radio.selected_chip_index, Some(0));
}

#[test]
fn natural_end_decrements_track_repeat_budget_before_releasing() {
    assert_eq!(
        next_repeat_state_after_natural_end(RepeatMode::One, Some(3)),
        (RepeatMode::One, Some(2))
    );
}

#[test]
fn natural_end_releases_track_repeat_to_off_when_budget_is_exhausted() {
    assert_eq!(
        next_repeat_state_after_natural_end(RepeatMode::One, Some(0)),
        (RepeatMode::Off, None)
    );
}

#[test]
fn natural_end_repeats_current_track_in_repeat_one_mode() {
    assert_eq!(natural_end_target_index(5, 2, RepeatMode::One), Some(2));
}

#[test]
fn natural_end_wraps_in_repeat_all_mode() {
    assert_eq!(natural_end_target_index(3, 2, RepeatMode::All), Some(0));
}

#[test]
fn natural_end_stops_after_last_track_when_repeat_is_off() {
    assert_eq!(natural_end_target_index(3, 2, RepeatMode::Off), None);
}

#[test]
fn natural_end_waits_for_in_flight_radio_extension_on_last_track() {
    assert!(should_wait_for_radio_extension_at_natural_end(
        3, 2, true, true
    ));
}

#[test]
fn natural_end_does_not_wait_without_radio_extension_in_flight() {
    assert!(!should_wait_for_radio_extension_at_natural_end(
        3, 2, true, false
    ));
    assert!(!should_wait_for_radio_extension_at_natural_end(
        3, 1, true, true
    ));
    assert!(!should_wait_for_radio_extension_at_natural_end(
        3, 2, false, true
    ));
}

#[test]
fn persisted_snapshots_rearm_track_repeat_budget_to_full_setting() {
    let settings = test_repeat_settings(4);
    let mut snapshot = PlaybackSnapshot {
        current_track: Some(ActiveTrackSnapshot {
            video_id: "video-123".to_string(),
            title: "Track".to_string(),
            artist: "Artist".to_string(),
            thumb_url: None,
            rating: SongRating::Indifferent,
        }),
        repeat_mode: RepeatMode::One,
        track_repeat_loops_left: Some(1),
        ..PlaybackSnapshot::default()
    };

    rearm_track_repeat_loops_for_snapshot(&mut snapshot, &settings);

    assert_eq!(snapshot.track_repeat_loops_left, Some(4));
}

#[test]
fn playback_start_is_marked_stalled_after_timeout_without_progress() {
    let started_at = Instant::now() - (PLAYBACK_START_TIMEOUT + Duration::from_secs(1));
    assert!(playback_start_stalled(started_at, true));
}

#[test]
fn playback_start_is_not_stalled_once_position_moves() {
    let started_at = Instant::now() - (PLAYBACK_START_TIMEOUT + Duration::from_secs(1));
    assert!(!playback_start_stalled(started_at, false));
}

#[test]
fn user_playback_start_desyncs_only_for_normal_play_requests() {
    assert!(should_desync_playback_start(
        PlaybackStartMode::Playing,
        false
    ));
    assert!(!should_desync_playback_start(
        PlaybackStartMode::Playing,
        true
    ));
    assert!(!should_desync_playback_start(
        PlaybackStartMode::Paused {
            position_millis: Some(124_056),
        },
        false,
    ));
}

#[test]
fn pending_playback_start_waits_for_desync_window() {
    let now = Instant::now();
    let due_at = now + DESYNC_PLAYBACK_START_DELAY;

    assert!(!pending_playback_start_is_ready(due_at, true, now));
    assert!(pending_playback_start_is_ready(due_at, true, due_at));
}

#[test]
fn paused_pending_playback_start_does_not_run_when_due() {
    let now = Instant::now();

    assert!(!pending_playback_start_is_ready(now, false, now));
}

#[test]
fn pending_playback_start_follows_track_after_queue_reorder() {
    let queue = vec![
        test_track("first"),
        test_track("second"),
        test_track("third"),
    ];

    assert_eq!(resolve_pending_playback_index(&queue, 0, "third"), Some(2));
}

#[test]
fn flush_keeps_waiting_on_stale_position_samples() {
    let started_at = Instant::now() - Duration::from_millis(500);
    let mut track = test_active_track(started_at);

    let stale_probe = 9_000;
    if track.awaiting_fresh_position && !playback_position_confirms_start(&track, stale_probe) {
        track.last_position_millis = stale_probe;
    }

    assert!(track.awaiting_fresh_position);
    assert_eq!(track.last_position_millis, stale_probe);
    assert_eq!(current_listened_millis(Some(stale_probe), &track), 0);
}

#[test]
fn advancing_position_clears_startup_wait() {
    let started_at = Instant::now() - Duration::from_secs(5);
    let mut track = test_active_track(started_at);
    track.last_position_millis = 5_000;
    let next_position = 5_250;

    assert!(playback_position_confirms_start(&track, next_position));
}

#[test]
fn explicit_startup_seek_confirms_resumed_position() {
    let started_at = Instant::now() - Duration::from_millis(500);
    let mut track = test_active_track(started_at);
    track.startup_seek_millis = Some(180_000);

    assert!(playback_position_confirms_start(&track, 179_500));
}

#[test]
fn explicit_startup_seek_keeps_timing_visible() {
    let started_at = Instant::now() - Duration::from_millis(500);
    let mut track = test_active_track(started_at);
    track.startup_seek_millis = Some(180_000);

    assert!(!hide_timing_for_track(&track));
}

#[test]
fn zero_position_does_not_confirm_start() {
    let started_at = Instant::now() - Duration::from_millis(500);
    let track = test_active_track(started_at);

    assert!(!playback_position_confirms_start(&track, 0));
}

#[test]
fn tiny_positive_position_does_not_confirm_start() {
    let started_at = Instant::now() - Duration::from_millis(500);
    let track = test_active_track(started_at);

    assert!(!playback_position_confirms_start(&track, 2));
}

#[test]
fn sufficiently_advanced_position_confirms_start() {
    let started_at = Instant::now() - Duration::from_millis(500);
    let track = test_active_track(started_at);

    assert!(playback_position_confirms_start(&track, 250));
}

#[test]
fn offline_cache_failures_retry_only_before_playback_really_starts() {
    assert!(should_retry_after_offline_cache_failure(
        PlaybackSource::OfflineCache,
        true
    ));
    assert!(!should_retry_after_offline_cache_failure(
        PlaybackSource::OfflineCache,
        false
    ));
    assert!(!should_retry_after_offline_cache_failure(
        PlaybackSource::RemoteStream,
        true
    ));
}

#[test]
fn tracking_retries_back_off_while_the_track_still_lacks_a_tracker() {
    assert_eq!(
        tracking_retry_delay(false, true, 0),
        Some(Duration::from_secs(2))
    );
    assert_eq!(
        tracking_retry_delay(false, true, 1),
        Some(Duration::from_secs(4))
    );
    assert_eq!(tracking_retry_delay(false, true, 2), None);
}

#[test]
fn tracking_does_not_retry_once_resolved_or_irrelevant() {
    assert_eq!(tracking_retry_delay(true, true, 0), None);
    assert_eq!(tracking_retry_delay(false, false, 0), None);
}

#[test]
fn startup_stalls_evict_local_cache_sources_before_retrying() {
    assert!(should_evict_cache_after_startup_stall(
        PlaybackSource::OfflineCache
    ));
    assert!(should_evict_cache_after_startup_stall(
        PlaybackSource::TempPrefetch
    ));
    assert!(!should_evict_cache_after_startup_stall(
        PlaybackSource::RemoteStream
    ));
}

#[test]
fn playlist_sync_keeps_removed_track_when_auto_cache_still_wants_it() {
    let base = std::env::temp_dir().join(format!("static-daemon-test-{}", Uuid::new_v4()));
    let raw_root = base.to_string_lossy().to_string();
    let cache = MusicCache::new_with_storage_root(Some(&raw_root)).expect("cache init");
    cache
        .store_audio("premium-but-cached", b"audio bytes", None)
        .expect("store audio");
    cache
        .pin_offline("premium-but-cached")
        .expect("pin cached track");

    let auto_retained_ids = HashSet::from(["premium-but-cached".to_string()]);
    remove_cached_if_unwanted_after_playlist_sync(&cache, "premium-but-cached", &auto_retained_ids);

    assert!(cache.get_cached_audio("premium-but-cached").is_some());
    assert!(!cache.is_pinned_offline("premium-but-cached"));

    let _ = fs::remove_dir_all(base);
}

#[test]
fn playlist_sync_removes_track_when_no_offline_policy_retains_it() {
    let base = std::env::temp_dir().join(format!("static-daemon-test-{}", Uuid::new_v4()));
    let raw_root = base.to_string_lossy().to_string();
    let cache = MusicCache::new_with_storage_root(Some(&raw_root)).expect("cache init");
    cache
        .store_audio("removed-track", b"audio bytes", None)
        .expect("store audio");
    cache
        .pin_offline("removed-track")
        .expect("pin cached track");

    remove_cached_if_unwanted_after_playlist_sync(&cache, "removed-track", &HashSet::new());

    assert!(cache.get_cached_audio("removed-track").is_none());

    let _ = fs::remove_dir_all(base);
}

#[test]
fn cache_media_download_retries_back_off_after_failures() {
    assert_eq!(
        cache_media_download_retry_delay(1),
        Duration::from_millis(350)
    );
    assert_eq!(
        cache_media_download_retry_delay(2),
        Duration::from_millis(1_000)
    );
}

#[test]
fn flush_ignores_waiting_time_before_first_real_position() {
    let started_at = Instant::now() - Duration::from_secs(MIN_LISTEN_SECS + 5);
    let track = test_active_track(started_at);
    assert_eq!(current_listened_millis(None, &track), 0);
    assert_eq!(current_listened_millis(Some(0), &track), 0);
}

#[test]
fn mpris_position_uses_elapsed_time_while_waiting_for_first_real_sample() {
    let started_at = Instant::now() - Duration::from_millis(1_200);
    let track = test_active_track(started_at);

    let visible = mpris_position_millis_for_track(&track, None, false).unwrap();
    assert!((1_000..=2_000).contains(&visible));
}

#[test]
fn mpris_filter_key_strips_runtime_instance_suffix() {
    assert_eq!(
        mpris_app_key_from_bus("org.mpris.MediaPlayer2.firefox.instance123"),
        Some("firefox".to_string())
    );
}

#[test]
fn mpris_filter_key_keeps_app_level_bus_name() {
    assert_eq!(
        mpris_app_key_from_bus("org.mpris.MediaPlayer2.spotify"),
        Some("spotify".to_string())
    );
    assert_eq!(mpris_app_key_from_bus(":1.52"), None);
}

#[test]
fn mpris_position_keeps_explicit_startup_seek_visible() {
    let started_at = Instant::now() - Duration::from_millis(500);
    let mut track = test_active_track(started_at);
    track.startup_seek_millis = Some(180_000);

    assert_eq!(
        mpris_position_millis_for_track(&track, Some(250), false),
        Some(180_000)
    );
}

#[test]
fn live_mpris_position_sync_waits_for_throttle_window() {
    let now = Instant::now();
    assert!(!should_sync_live_mpris_position(
        Some(1_000),
        Some(1_250),
        Some(now - Duration::from_millis(100)),
        now
    ));
}

#[test]
fn live_mpris_position_sync_runs_when_position_advances_after_interval() {
    let now = Instant::now();
    assert!(should_sync_live_mpris_position(
        Some(1_000),
        Some(1_250),
        Some(now - Duration::from_millis(300)),
        now
    ));
}

#[test]
fn paused_idle_shutdown_timer_counts_only_plain_paused_playback() {
    assert!(should_count_paused_idle_for_shutdown(
        true, true, false, false, false
    ));
    assert!(!should_count_paused_idle_for_shutdown(
        false, true, false, false, false
    ));
    assert!(!should_count_paused_idle_for_shutdown(
        true, false, false, false, false
    ));
}

#[test]
fn paused_idle_shutdown_timer_ignores_coordination_holds() {
    assert!(!should_count_paused_idle_for_shutdown(
        true, true, true, false, false
    ));
    assert!(!should_count_paused_idle_for_shutdown(
        true, true, false, true, false
    ));
    assert!(!should_count_paused_idle_for_shutdown(
        true, true, false, false, true
    ));
}

#[test]
fn natural_end_detector_counts_song_completed_near_duration() {
    assert!(has_reached_track_end(
        false,
        false,
        Some(ClockTime::from_seconds(179) + ClockTime::from_mseconds(600)),
        Some(ClockTime::from_seconds(180)),
    ));
}

#[test]
fn natural_end_detector_ignores_startup_and_pause_fade_edges() {
    assert!(!has_reached_track_end(
        false,
        true,
        Some(ClockTime::from_seconds(180)),
        Some(ClockTime::from_seconds(180)),
    ));
    assert!(!has_reached_track_end(
        true,
        false,
        Some(ClockTime::from_seconds(180)),
        Some(ClockTime::from_seconds(180)),
    ));
}

#[test]
fn volume_ramp_completion_is_deadline_based() {
    let ramp = VolumeRamp {
        started_at: Instant::now() - Duration::from_secs(3),
        start_volume: 0.5,
        target_volume: 0.0,
        duration: Duration::from_secs(2),
        completion: VolumeRampCompletion::PauseAtPosition {
            position_millis: 42,
        },
    };

    assert!(volume_ramp_is_complete(ramp, 0.25));
}

#[test]
fn volume_requests_respect_boost_setting_and_reject_nonfinite_values() {
    let mut settings = AppSettings::default();
    assert!(valid_requested_volume(0.0, &settings));
    assert!(valid_requested_volume(1.0, &settings));
    assert!(!valid_requested_volume(1.01, &settings));
    settings.volume_boost_enabled = true;
    assert!(valid_requested_volume(5.0, &settings));
    for invalid in [-0.1, 5.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(!valid_requested_volume(invalid, &settings));
    }
    settings.volume_boost_enabled = false;
    assert!(!valid_requested_volume(5.0, &settings));
}

#[test]
fn volume_ramps_reach_boosted_targets_and_handle_expired_reload_deadlines() {
    let ramp = VolumeRamp {
        started_at: Instant::now() - Duration::from_secs(1),
        start_volume: 0.0,
        target_volume: 5.0,
        duration: Duration::from_millis(200),
        completion: VolumeRampCompletion::None,
    };
    assert_eq!(volume_ramp_value(ramp), 5.0);
    let finished_pause = VolumeRamp {
        start_volume: 5.0,
        target_volume: 0.0,
        duration: Duration::ZERO,
        ..ramp
    };
    assert_eq!(volume_ramp_value(finished_pause), 0.0);
}

#[test]
fn halt_auto_zero_delay_resumes_immediately_after_peer_quiet() {
    assert!(should_resume_after_external_mpris_pause(
        CoordinationMode::HaltAuto,
        Some(Duration::ZERO),
        Duration::ZERO
    ));
}

#[test]
fn mpris_halt_auto_sidechannel_only_advertises_while_peer_is_quiet() {
    assert!(should_expose_mpris_halt_auto_sidechannel(
        true,
        CoordinationMode::HaltAuto,
        5,
        true,
        true,
        true,
        false
    ));
    assert!(!should_expose_mpris_halt_auto_sidechannel(
        true,
        CoordinationMode::HaltAuto,
        5,
        true,
        true,
        true,
        true
    ));
    assert!(!should_expose_mpris_halt_auto_sidechannel(
        false,
        CoordinationMode::HaltAuto,
        5,
        true,
        true,
        true,
        false
    ));
    assert!(!should_expose_mpris_halt_auto_sidechannel(
        true,
        CoordinationMode::Halt,
        5,
        true,
        true,
        true,
        false
    ));
    assert!(!should_expose_mpris_halt_auto_sidechannel(
        true,
        CoordinationMode::HaltAuto,
        0,
        true,
        true,
        true,
        false
    ));
    assert!(!should_expose_mpris_halt_auto_sidechannel(
        true,
        CoordinationMode::HaltAuto,
        5,
        false,
        true,
        true,
        false
    ));
    assert!(!should_expose_mpris_halt_auto_sidechannel(
        true,
        CoordinationMode::HaltAuto,
        5,
        true,
        false,
        true,
        false
    ));
    assert!(!should_expose_mpris_halt_auto_sidechannel(
        true,
        CoordinationMode::HaltAuto,
        5,
        true,
        true,
        false,
        false
    ));
}

#[test]
fn pending_mpris_transport_policy_cancels_pause_like_intents() {
    for intent in [
        PendingMprisTransportIntent::Cancel("MPRIS Pause"),
        PendingMprisTransportIntent::Cancel("MPRIS PlayPause"),
    ] {
        assert_eq!(
            pending_mpris_transport_decision(true, intent),
            PendingMprisTransportDecision::Cancel
        );
    }
}

#[test]
fn pending_mpris_transport_policy_resumes_without_applying_transport_intents() {
    for intent in [
        PendingMprisTransportIntent::ResumeWithoutApplying("Play"),
        PendingMprisTransportIntent::ResumeWithoutApplying("Next"),
        PendingMprisTransportIntent::ResumeWithoutApplying("Previous"),
        PendingMprisTransportIntent::ResumeWithoutApplying("Seek"),
        PendingMprisTransportIntent::ResumeWithoutApplying("SetPosition"),
    ] {
        assert_eq!(
            pending_mpris_transport_decision(true, intent),
            PendingMprisTransportDecision::ResumeWithoutApplying
        );
    }
}

#[test]
fn mpris_transport_policy_ignores_commands_when_resume_is_not_pending() {
    for intent in [
        PendingMprisTransportIntent::Cancel("MPRIS Pause"),
        PendingMprisTransportIntent::ResumeWithoutApplying("Seek"),
    ] {
        assert_eq!(
            pending_mpris_transport_decision(false, intent),
            PendingMprisTransportDecision::Ignore
        );
    }
}

#[test]
fn mpris_peer_transition_returns_to_peer_block_when_peer_restarts() {
    assert_eq!(
        mpris_halt_auto_peer_transition(
            CoordinationMode::HaltAuto,
            true,
            true,
            true,
            true,
            true,
            30,
            Some(Duration::from_secs(2)),
        ),
        MprisHaltAutoPeerTransition::ReturnToPeerBlock
    );
}

#[test]
fn mpris_peer_transition_arms_sidechannel_only_for_delayed_halt_auto() {
    assert_eq!(
        mpris_halt_auto_peer_transition(
            CoordinationMode::HaltAuto,
            true,
            true,
            true,
            false,
            false,
            30,
            Some(Duration::ZERO),
        ),
        MprisHaltAutoPeerTransition::ActivatePending
    );
    assert_eq!(
        mpris_halt_auto_peer_transition(
            CoordinationMode::HaltAuto,
            true,
            true,
            true,
            false,
            false,
            0,
            Some(Duration::ZERO),
        ),
        MprisHaltAutoPeerTransition::Resume
    );
}

#[test]
fn mpris_peer_transition_waits_then_resumes_pending_halt_auto() {
    assert_eq!(
        mpris_halt_auto_peer_transition(
            CoordinationMode::HaltAuto,
            true,
            true,
            true,
            false,
            true,
            30,
            Some(Duration::from_secs(29)),
        ),
        MprisHaltAutoPeerTransition::Wait
    );
    assert_eq!(
        mpris_halt_auto_peer_transition(
            CoordinationMode::HaltAuto,
            true,
            true,
            true,
            false,
            true,
            30,
            Some(Duration::from_secs(30)),
        ),
        MprisHaltAutoPeerTransition::Resume
    );
}

#[test]
fn mpris_peer_transition_clears_pending_when_auto_resume_no_longer_applies() {
    assert_eq!(
        mpris_halt_auto_peer_transition(
            CoordinationMode::Halt,
            true,
            true,
            true,
            false,
            true,
            30,
            Some(Duration::from_secs(30)),
        ),
        MprisHaltAutoPeerTransition::ClearPending
    );
    assert_eq!(
        mpris_halt_auto_peer_transition(
            CoordinationMode::Off,
            false,
            false,
            false,
            false,
            true,
            30,
            None,
        ),
        MprisHaltAutoPeerTransition::DeactivateCoordination
    );
}

#[test]
fn halt_mode_never_auto_resumes() {
    assert!(!should_resume_after_external_mpris_pause(
        CoordinationMode::Halt,
        Some(Duration::from_secs(60)),
        Duration::from_secs(5)
    ));
}

#[test]
fn halt_auto_mode_waits_for_configured_quiet_window() {
    assert!(!should_resume_after_external_mpris_pause(
        CoordinationMode::HaltAuto,
        Some(Duration::from_secs(4)),
        Duration::from_secs(5)
    ));
    assert!(should_resume_after_external_mpris_pause(
        CoordinationMode::HaltAuto,
        Some(Duration::from_secs(5)),
        Duration::from_secs(5)
    ));
}

#[test]
fn pipewire_pause_mode_can_fall_back_to_duck_immediately() {
    assert!(should_resume_after_pipewire_pause(
        CoordinationMode::Off,
        PipeWireSuppressionMode::Duck,
        None,
        Duration::from_secs(5)
    ));
}

#[test]
fn pipewire_halt_auto_waits_for_quiet_window() {
    assert!(!should_resume_after_pipewire_pause(
        CoordinationMode::HaltAuto,
        PipeWireSuppressionMode::None,
        Some(Duration::from_secs(4)),
        Duration::from_secs(5)
    ));
    assert!(should_resume_after_pipewire_pause(
        CoordinationMode::HaltAuto,
        PipeWireSuppressionMode::None,
        Some(Duration::from_secs(5)),
        Duration::from_secs(5)
    ));
}

#[test]
fn mpris_pause_suppresses_pipewire_duck() {
    assert_eq!(
        resolve_pipewire_suppression_mode(
            true,
            true,
            true,
            CoordinationMode::Off,
            false,
            true,
            true,
        ),
        PipeWireSuppressionMode::None
    );
}

#[test]
fn mpris_pause_allows_pipewire_pause_to_take_over_next() {
    assert_eq!(
        resolve_pipewire_suppression_mode(
            true,
            true,
            true,
            CoordinationMode::HaltAuto,
            false,
            true,
            true,
        ),
        PipeWireSuppressionMode::Pause
    );
}

#[test]
fn manual_pause_does_not_arm_pipewire_pause() {
    assert_eq!(
        resolve_pipewire_suppression_mode(
            true,
            true,
            true,
            CoordinationMode::HaltAuto,
            false,
            true,
            false,
        ),
        PipeWireSuppressionMode::None
    );
}

#[test]
fn pipewire_unduck_keeps_pending_pause_ramp() {
    let pause_ramp = VolumeRamp {
        started_at: Instant::now(),
        start_volume: 0.3,
        target_volume: 0.0,
        duration: Duration::from_millis(250),
        completion: VolumeRampCompletion::PauseAtPosition {
            position_millis: 12_345,
        },
    };
    let duck_ramp = VolumeRamp {
        completion: VolumeRampCompletion::None,
        ..pause_ramp
    };

    assert!(!pipewire_unduck_can_replace_volume_ramp(Some(pause_ramp)));
    assert!(pipewire_unduck_can_replace_volume_ramp(Some(duck_ramp)));
    assert!(pipewire_unduck_can_replace_volume_ramp(None));
}

#[test]
fn immediate_pause_interrupts_pending_pause_fade() {
    assert!(!pause_request_already_satisfied(
        false,
        true,
        PlaybackTransition::Immediate
    ));
    assert!(pause_request_already_satisfied(
        false,
        true,
        PlaybackTransition::Calm
    ));
    assert!(pause_request_already_satisfied(
        true,
        false,
        PlaybackTransition::Immediate
    ));
}

#[test]
fn live_mpris_seeked_hint_emits_when_crossing_second_boundary() {
    assert!(should_emit_live_mpris_seeked_hint(Some(950), Some(1_120)));
}

#[test]
fn live_mpris_seeked_hint_skips_intra_second_updates() {
    assert!(!should_emit_live_mpris_seeked_hint(
        Some(1_050),
        Some(1_240)
    ));
}

fn video_ids(tracks: &[Track]) -> Vec<&str> {
    tracks.iter().map(|track| track.video_id.as_str()).collect()
}

#[test]
fn restore_order_rebuilds_pre_shuffle_order_and_appends_newcomers() {
    let original = vec![
        test_track("a"),
        test_track("b"),
        test_track("c"),
        test_track("d"),
    ];
    // Shuffled remainder, one track played away ("a"), one newcomer appended ("x").
    let shuffled = vec![
        test_track("d"),
        test_track("x"),
        test_track("b"),
        test_track("c"),
    ];

    let restored = restore_order(shuffled, &original);

    assert_eq!(video_ids(&restored), vec!["b", "c", "d", "x"]);
}

#[test]
fn restore_order_matches_duplicates_positionally_and_stays_stable() {
    let original = vec![test_track("a"), test_track("b"), test_track("a")];
    let shuffled = vec![test_track("a"), test_track("a"), test_track("b")];

    let restored = restore_order(shuffled, &original);

    assert_eq!(video_ids(&restored), vec!["a", "b", "a"]);
}

fn test_track_by(video_id: &str, title: &str, artist: &str) -> Track {
    let mut track = test_track(video_id);
    track.title = title.to_string();
    track.artists = vec![Artist {
        name: artist.to_string(),
        id: None,
    }];
    track
}

#[test]
fn radio_shuffle_pick_promotes_untouched_track_to_target() {
    let mut queue = vec![
        test_track("a"),
        test_track("b"),
        test_track("c"),
        test_track("d"),
        test_track("e"),
        test_track("f"),
    ];

    let moved = promote_untouched_pick(&mut queue, true, Some(2), 1, Some(2), |_, _| 2);

    assert!(moved);
    // History keeps true play order; the picked track lands at the advance
    // target and the rest of the untouched tail keeps its stable order.
    assert_eq!(video_ids(&queue), vec!["a", "b", "e", "c", "d", "f"]);
}

#[test]
fn radio_shuffle_pick_draws_from_untouched_candidates_against_played_history() {
    let mut queue = vec![
        test_track("a"),
        test_track("b"),
        test_track("c"),
        test_track("d"),
        test_track("e"),
    ];
    let mut seen: Option<(Vec<String>, Vec<String>)> = None;

    promote_untouched_pick(
        &mut queue,
        true,
        Some(2),
        1,
        Some(2),
        |candidates, history| {
            seen = Some((
                candidates
                    .iter()
                    .map(|track| track.video_id.clone())
                    .collect(),
                history.iter().map(|track| track.video_id.clone()).collect(),
            ));
            0
        },
    );

    // The pick is drawn from exactly the untouched region [touched_len, queue.len())
    // — newly appended radio suggestions are immediately eligible — against the
    // played prefix in play order (current track last).
    let (candidates, history) = seen.expect("pick closure must run");
    assert_eq!(candidates, vec!["c", "d", "e"]);
    assert_eq!(history, vec!["a", "b"]);
}

#[test]
fn radio_shuffle_pick_leaves_touched_prefix_targets_alone() {
    let mut queue = vec![test_track("a"), test_track("b"), test_track("c")];
    let before = video_ids(&queue)
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();

    // Repeat-All prefix loops, repeat-One, and forward through replayed history
    // land inside the touched prefix and pass through untouched.
    let moved = promote_untouched_pick(&mut queue, true, Some(2), 0, Some(1), |_, _| {
        unreachable!("touched-prefix targets must not draw a pick")
    });

    assert!(!moved);
    assert_eq!(video_ids(&queue), before);
}

#[test]
fn radio_shuffle_pick_passes_playlist_queues_and_disabled_shuffle_through() {
    let mut queue = vec![test_track("a"), test_track("b"), test_track("c")];

    // Playlist queues (no radio touched length) never pick.
    assert!(!promote_untouched_pick(
        &mut queue,
        true,
        None,
        0,
        Some(1),
        |_, _| { unreachable!("playlist queues must not draw a pick") }
    ));
    // Shuffle off never picks.
    assert!(!promote_untouched_pick(
        &mut queue,
        false,
        Some(1),
        0,
        Some(1),
        |_, _| { unreachable!("disabled shuffle must not draw a pick") }
    ));
    // A target beyond the queue never picks.
    assert!(!promote_untouched_pick(
        &mut queue,
        true,
        Some(1),
        0,
        Some(3),
        |_, _| { unreachable!("out-of-range targets must not draw a pick") }
    ));
    // No advance target never picks.
    assert!(!promote_untouched_pick(
        &mut queue,
        true,
        Some(1),
        0,
        None,
        |_, _| { unreachable!("a stopped advance must not draw a pick") }
    ));
    assert_eq!(video_ids(&queue), vec!["a", "b", "c"]);
}

#[test]
fn offline_pick_restricts_the_sample_to_cached_candidates() {
    fn track(id: &str) -> Track {
        test_track_by(id, &format!("t-{id}"), &format!("artist-{id}"))
    }
    let candidates = [track("a"), track("b"), track("c"), track("d")];
    let history = [track("h")];

    // Only cached positions are samplable, whatever the roll says.
    let cached_mask = [false, true, false, true];
    assert_eq!(
        cached_restricted_pick_offset(&candidates, &history, &cached_mask, 0.0),
        Some(1)
    );
    assert_eq!(
        cached_restricted_pick_offset(&candidates, &history, &cached_mask, 0.999),
        Some(3)
    );
    // Nothing cached -> None, the caller's cue to fail loudly instead of
    // picking a song that cannot play.
    assert_eq!(
        cached_restricted_pick_offset(&candidates, &history, &[false; 4], 0.5),
        None
    );
    // Single cached candidate always wins.
    assert_eq!(
        cached_restricted_pick_offset(&candidates, &history, &[false, false, true, false], 0.7),
        Some(2)
    );
}

#[test]
fn advance_uses_the_stored_pick_and_falls_back_to_a_fresh_sample() {
    fn track(id: &str) -> Track {
        test_track_by(id, &format!("t-{id}"), &format!("artist-{id}"))
    }
    let candidates = [track("a"), track("b"), track("c")];
    let history = [track("h")];

    // Online, the pre-decided pick wins regardless of the roll (which would
    // land on offset 0) — the advance consumes exactly what was announced.
    let pick = resolve_shuffle_advance_pick(&candidates, &history, Some("c"), true, |_| false, 0.0);
    assert_eq!(pick.unwrap(), 2);
    // A vanished pick (removed from the queue) falls back to a fresh sample.
    let pick =
        resolve_shuffle_advance_pick(&candidates, &history, Some("zz"), true, |_| false, 0.0);
    assert_eq!(pick.unwrap(), 0);
    // Offline, a cached pre-decided pick is used directly...
    let pick = resolve_shuffle_advance_pick(
        &candidates,
        &history,
        Some("c"),
        false,
        |track| track.video_id == "c",
        0.0,
    );
    assert_eq!(pick.unwrap(), 2);
    // ...an uncached one falls back to the cached-restricted sample...
    let pick = resolve_shuffle_advance_pick(
        &candidates,
        &history,
        Some("c"),
        false,
        |track| track.video_id == "a",
        0.9,
    );
    assert_eq!(pick.unwrap(), 0);
    // ...and nothing cached fails loudly with the user-facing message.
    let err = resolve_shuffle_advance_pick(&candidates, &history, Some("c"), false, |_| false, 0.5)
        .unwrap_err();
    assert!(err.to_string().contains("offline"));
}

#[test]
fn next_track_start_redecides_the_consumed_pick_but_keeps_a_valid_one() {
    let queue = vec![
        test_track("a"),
        test_track("b"),
        test_track("c"),
        test_track("d"),
    ];

    // Consumed by an advance (no stored pick) -> the next track start samples a
    // fresh pick from the untouched region.
    let picked = refresh_shuffle_up_next_id(&queue, true, Some(2), 1, None, 0.0)
        .expect("untouched region must yield a pick");
    assert!(queue[2..].iter().any(|track| track.video_id == picked));
    // A still-valid pick is kept — its prefetch may already be underway.
    assert_eq!(
        refresh_shuffle_up_next_id(&queue, true, Some(2), 1, Some("d".to_owned()), 0.9),
        Some("d".to_owned())
    );
    // A stale pick (no longer in the untouched region) is re-sampled, not kept.
    assert_ne!(
        refresh_shuffle_up_next_id(&queue, true, Some(2), 1, Some("a".to_owned()), 0.0),
        Some("a".to_owned())
    );
    // Shuffle off, playlist queues, and an empty untouched region clear the pick.
    assert_eq!(
        refresh_shuffle_up_next_id(&queue, false, Some(2), 1, Some("d".to_owned()), 0.0),
        None
    );
    assert_eq!(
        refresh_shuffle_up_next_id(&queue, true, None, 1, Some("d".to_owned()), 0.0),
        None
    );
    assert_eq!(
        refresh_shuffle_up_next_id(&queue, true, Some(4), 1, Some("d".to_owned()), 0.0),
        None
    );
}

#[test]
fn title_similarity_treats_release_variants_as_the_same_song() {
    fn track(title: &str, artist: &str) -> Track {
        test_track_by(&format!("id-{title}-{artist}"), title, artist)
    }

    let original = track("UNETHICAL", "Faouzia");
    // Real YouTube diversity: uploader-prefixed covers, lyrics videos, orchestral
    // uploads — the original artist rides in the TITLE while the uploader holds the
    // credit.
    assert_eq!(
        title_similarity(
            &original,
            &track("Faouzia - Unethical (MAPHRA Vocal Cover)", "MAPHRA")
        ),
        1.0
    );
    assert_eq!(
        title_similarity(
            &original,
            &track("Faouzia - UNETHICAL (Lyrics)", "Addicted Lyrics")
        ),
        1.0
    );
    assert_eq!(
        title_similarity(
            &original,
            &track(
                "Faouzia – UNETHICAL | Emotional Cinematic Orchestra Version",
                "Orchestra Infinity"
            )
        ),
        1.0
    );
    // Plain release variants without artist prefixes.
    assert_eq!(
        title_similarity(&track("Aurora", "X"), &track("Aurora - Remix", "Y")),
        1.0
    );
    assert_eq!(
        title_similarity(&track("Aurora", "X"), &track("Aurora (Nightcore)", "Y")),
        1.0
    );
    // Same title, genuinely different songs: intentional collision — spacing them
    // is cheap.
    assert_eq!(
        title_similarity(
            &track("RUNWAY", "Blaq Tuxedo"),
            &track("runway", "Slowdrift")
        ),
        1.0
    );
    // Near-miss titles and unrelated titles do not repel.
    assert_eq!(
        title_similarity(&track("Runaway", "AURORA"), &track("Runway", "milet")),
        0.0
    );
    assert!(
        title_similarity(
            &track("Aurora", "X"),
            &track("Completely Different Song", "Y")
        ) < 0.2
    );
    assert_eq!(
        title_similarity(&track("", "X"), &track("Aurora", "Y")),
        0.0
    );
    // Instrument words are not descriptors: "Piano Man" must not fuse with
    // "Rocket Man".
    assert!(
        title_similarity(
            &track("Piano Man", "Billy Joel"),
            &track("Rocket Man", "Elton John")
        ) < 1.0
    );
}

#[test]
fn radio_pick_weights_repel_recent_creators_and_similar_titles() {
    let history = vec![
        test_track_by("h1", "Old Song", "Old Band"),
        test_track_by("h2", "Aurora", "Nova"),
    ];
    let candidates = vec![
        test_track_by("sameArtist", "Fresh Title", "Nova"),
        test_track_by("sameTitle", "Aurora - Remix", "Someone Else"),
        test_track_by("fresh", "Unrelated", "New Band"),
    ];

    let weights = radio_pick_weights(&candidates, &history);

    // The fresh candidate must beat both repelled ones; all stay strictly pickable.
    assert!(weights[2] > weights[0]);
    assert!(weights[2] > weights[1]);
    assert!(weights.iter().all(|weight| *weight > 0.0));

    // A repulsion further back in history weighs less than one just played.
    let reversed: Vec<Track> = history.iter().rev().cloned().collect();
    let older_hit = radio_pick_weights(&candidates, &reversed);
    assert!(older_hit[0] > weights[0]);
}

#[test]
fn history_aware_repulsion_decays_per_song_of_age() {
    fn track(id: &str, title: &str) -> Track {
        test_track_by(id, title, &format!("artist-{id}"))
    }

    // History {a, b} (b just played), candidates {a remix, b remix, c}:
    // c is untouched by repulsion, a's force decayed one step below b's.
    let history = vec![track("a", "a"), track("b", "b")];
    let candidates = vec![
        track("ar", "a - remix"),
        track("br", "b - remix"),
        track("c", "c"),
    ];
    let weights = radio_pick_weights(&candidates, &history);
    assert!(weights[2] > weights[0]);
    assert!(weights[0] > weights[1]);
    // One decay step of 0.05 on the title component (0.5 weight) = 0.025 exactly.
    assert!((weights[0] - weights[1] - 0.025).abs() < 1e-9);

    // Beyond the decay horizon, old plays exert no repulsion at all.
    let fillers: Vec<Track> = (0..25)
        .map(|index| track(&format!("f{index}"), &format!("filler {index}")))
        .collect();
    let mut ancient = vec![track("a", "a")];
    ancient.extend(fillers.iter().cloned());
    let candidate = vec![track("ar", "a - remix")];
    let faded = radio_pick_weights(&candidate, &ancient);
    let fresh = radio_pick_weights(&candidate, &fillers);
    assert!((fresh[0] - faded[0]).abs() < 1e-9);
}

#[test]
fn weighted_index_samples_proportionally_never_maximises() {
    let weights = [1.0, 3.0];
    assert_eq!(weighted_index(&weights, 0.0), 0);
    assert_eq!(weighted_index(&weights, 0.24), 0);
    // A low-weight candidate is still reachable — the roll decides, not the maximum.
    assert_eq!(weighted_index(&weights, 0.26), 1);
    assert_eq!(weighted_index(&weights, 0.999), 1);
    assert_eq!(weighted_index(&[0.0, 0.0], 0.7), 1);
}

#[test]
fn radio_shuffle_pick_with_live_rng_keeps_history_and_queue_membership() {
    // The production picker (weighted sample over the untouched candidates) must
    // never disturb the touched prefix and must keep the queue a permutation; the
    // touched length itself is passed by value, so enabling shuffle and picking
    // can never grow it — only the ordinary advance bookkeeping does.
    let touched_len = 3;
    for _ in 0..64 {
        let mut queue: Vec<Track> = ["a", "b", "c", "d", "e", "f", "g"]
            .iter()
            .map(|id| test_track(id))
            .collect();
        let mut untouched: Vec<String> = queue[touched_len..]
            .iter()
            .map(|track| track.video_id.clone())
            .collect();

        promote_untouched_pick(
            &mut queue,
            true,
            Some(touched_len),
            touched_len - 1,
            Some(touched_len),
            |candidates, history| {
                use rand::RngExt;
                let weights = radio_pick_weights(candidates, history);
                weighted_index(&weights, rand::rng().random::<f64>())
            },
        );

        assert_eq!(video_ids(&queue[..touched_len]), vec!["a", "b", "c"]);
        assert!(untouched.contains(&queue[touched_len].video_id));
        let mut remaining: Vec<String> = queue[touched_len..]
            .iter()
            .map(|track| track.video_id.clone())
            .collect();
        remaining.sort();
        untouched.sort();
        assert_eq!(remaining, untouched);
    }
}
