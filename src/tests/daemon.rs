use super::{
    ActiveTrack, MIN_LISTEN_SECS, MprisHaltAutoPeerTransition, PLAYBACK_START_TIMEOUT,
    PREVIOUS_TRACK_THRESHOLD_MILLIS, PendingMprisTransportDecision, PendingMprisTransportIntent,
    PipeWireSuppressionMode, PlaybackSource, PreviousAction, VolumeRamp, VolumeRampCompletion,
    apply_radio_chips_from_refresh, current_listened_millis, dedupe_radio_recommendations,
    effective_queue_len, extend_radio_touched_for_insert, hide_timing_for_track,
    mpris_halt_auto_peer_transition, mpris_position_millis_for_track, natural_end_target_index,
    next_repeat_state_after_natural_end, pause_previous_threshold_clock_for_track,
    pending_mpris_transport_decision, pipewire_unduck_can_replace_volume_ramp,
    playback_position_confirms_start, playback_start_stalled, previous_action,
    previous_threshold_millis, radio_chips_with_replacement, rearm_track_repeat_loops_for_snapshot,
    reset_previous_threshold_clock_for_track, resolve_pipewire_suppression_mode,
    should_emit_live_mpris_seeked_hint, should_evict_cache_after_startup_stall,
    should_expose_mpris_halt_auto_sidechannel, should_resume_after_external_mpris_pause,
    should_resume_after_pipewire_pause, should_retry_after_offline_cache_failure,
    should_sync_live_mpris_position, should_wait_for_radio_extension_at_natural_end,
};
use crate::ipc::{ActiveTrackSnapshot, PlaybackSnapshot, RepeatMode};
use crate::models::{Artist, Track};
use crate::models::{RadioChip, RadioVirtualPlaylist};
use crate::network::SongRating;
use crate::offline::{AppSettings, CoordinationMode};
use std::time::{Duration, Instant};

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
        previous_threshold_played_millis: 0,
        previous_threshold_resumed_at: Some(started_at),
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
fn previous_threshold_ignores_sponsorblock_jump_position() {
    let started_at = Instant::now() - Duration::from_millis(1_500);
    let mut track = test_active_track(started_at);
    track.awaiting_fresh_position = false;
    track.last_position_millis = 75_000;

    assert!(previous_threshold_millis(&track) < PREVIOUS_TRACK_THRESHOLD_MILLIS);
}

#[test]
fn previous_threshold_resets_after_restarting_current_track() {
    let started_at = Instant::now() - Duration::from_secs(10);
    let mut track = test_active_track(started_at);
    track.awaiting_fresh_position = false;

    pause_previous_threshold_clock_for_track(&mut track);
    reset_previous_threshold_clock_for_track(&mut track, true);

    assert!(previous_threshold_millis(&track) < PREVIOUS_TRACK_THRESHOLD_MILLIS);
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
