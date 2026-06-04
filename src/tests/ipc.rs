use super::{
    ActiveTrackSnapshot, DaemonRequest, PLAYBACK_STATE_ROW_KEY, PersistedPlaybackState,
    PlaybackSnapshot, PlaylistContext, PlaylistContextSource, clear_persisted_state_at_path,
    current_unix_millis, load_persisted_state_at_path, store_persisted_state_at_path,
};
use crate::ipc::RepeatMode;
use crate::models::Track;
use crate::network::SongRating;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_db_path() -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir()
        .join(format!("static-ipc-test-{suffix}"))
        .join("data.db")
}

#[test]
fn persisted_playback_state_round_trips_through_database() {
    let path = unique_temp_db_path();
    let snapshot = PlaybackSnapshot {
        current_track: Some(ActiveTrackSnapshot {
            video_id: "video-123".to_string(),
            title: "Track".to_string(),
            artist: "Artist".to_string(),
            thumb_url: Some("https://example.com/thumb.jpg".to_string()),
            rating: SongRating::Like,
        }),
        queue: Vec::new(),
        queue_index: 0,
        playlist_context: Some(PlaylistContext {
            playlist_id: "PL123".to_string(),
            title: "Coding Mix".to_string(),
            thumbnail_url: Some("https://example.com/playlist.jpg".to_string()),
            subtitle: Some("42 songs".to_string()),
            source: PlaylistContextSource::Remote,
        }),
        radio_virtual_playlist: None,
        repeat_mode: RepeatMode::All,
        track_repeat_loops_left: Some(3),
        paused: true,
        volume: 0.75,
        position_millis: Some(12_345),
        duration_millis: Some(67_890),
        buffered_fraction: 0.5,
        halt_auto_resume_pending: false,
        probe_active: false,
        probe_restore_position_millis: None,
        sleep_remaining_secs: None,
        sleep_after_current: false,
    };
    let persisted = PersistedPlaybackState {
        snapshot,
        saved_at_unix_millis: current_unix_millis(),
    };

    store_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY, &persisted).unwrap();
    let loaded = load_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY).unwrap();

    assert!(loaded.is_some());
    let loaded = loaded.unwrap();
    assert_eq!(loaded.saved_at_unix_millis, persisted.saved_at_unix_millis);
    assert_eq!(loaded.snapshot.queue, persisted.snapshot.queue);
    assert_eq!(loaded.snapshot.queue_index, persisted.snapshot.queue_index);
    assert_eq!(
        loaded
            .snapshot
            .playlist_context
            .as_ref()
            .map(|context| context.playlist_id.as_str()),
        persisted
            .snapshot
            .playlist_context
            .as_ref()
            .map(|context| context.playlist_id.as_str())
    );
    assert_eq!(loaded.snapshot.repeat_mode, persisted.snapshot.repeat_mode);
    assert_eq!(
        loaded.snapshot.track_repeat_loops_left,
        persisted.snapshot.track_repeat_loops_left
    );
    assert_eq!(loaded.snapshot.paused, persisted.snapshot.paused);
    assert_eq!(loaded.snapshot.volume, persisted.snapshot.volume);
    assert_eq!(
        loaded.snapshot.position_millis,
        persisted.snapshot.position_millis
    );
    assert_eq!(
        loaded.snapshot.duration_millis,
        persisted.snapshot.duration_millis
    );
    assert_eq!(
        loaded.snapshot.buffered_fraction,
        persisted.snapshot.buffered_fraction
    );
    assert_eq!(
        loaded
            .snapshot
            .current_track
            .as_ref()
            .map(|track| &track.video_id),
        persisted
            .snapshot
            .current_track
            .as_ref()
            .map(|track| &track.video_id)
    );

    let _ = fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
}

#[test]
fn clearing_persisted_playback_state_removes_database_row() {
    let path = unique_temp_db_path();
    let persisted = PersistedPlaybackState {
        snapshot: PlaybackSnapshot::default(),
        saved_at_unix_millis: current_unix_millis(),
    };

    store_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY, &persisted).unwrap();
    clear_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY).unwrap();

    assert!(
        load_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY)
            .unwrap()
            .is_none()
    );

    let _ = fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
}

#[test]
fn playlist_state_rows_do_not_clobber_global_resume_row() {
    let path = unique_temp_db_path();
    let global = PersistedPlaybackState {
        snapshot: PlaybackSnapshot::default(),
        saved_at_unix_millis: current_unix_millis(),
    };
    let playlist_context = PlaylistContext {
        playlist_id: "PL456".to_string(),
        title: "Late Night".to_string(),
        thumbnail_url: None,
        subtitle: Some("19 songs".to_string()),
        source: PlaylistContextSource::Offline,
    };
    let playlist = PersistedPlaybackState {
        snapshot: PlaybackSnapshot {
            queue: vec![Track {
                video_id: "track-1".to_string(),
                title: "One".to_string(),
                artists: Vec::new(),
                album: None,
                duration: None,
                thumbnail_url: None,
                playback_playlist_id: None,
                playback_params: None,
                not_interested_feedback_token: None,
                undo_feedback_token: None,
            }],
            queue_index: 0,
            playlist_context: Some(playlist_context.clone()),
            ..PlaybackSnapshot::default()
        },
        saved_at_unix_millis: current_unix_millis(),
    };

    store_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY, &global).unwrap();
    store_persisted_state_at_path(&path, &playlist_context.storage_key(), &playlist).unwrap();

    let loaded_global = load_persisted_state_at_path(&path, PLAYBACK_STATE_ROW_KEY).unwrap();
    let loaded_playlist =
        load_persisted_state_at_path(&path, &playlist_context.storage_key()).unwrap();

    assert!(loaded_global.is_some());
    assert!(loaded_playlist.is_some());
    assert_eq!(loaded_global.unwrap().snapshot.queue.len(), 0);
    assert_eq!(loaded_playlist.unwrap().snapshot.queue.len(), 1);

    let _ = fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
}

#[test]
fn play_queue_at_requests_default_to_reporting_when_flag_is_missing() {
    let request = serde_json::from_value::<DaemonRequest>(json!({
        "PlayQueueAt": {
            "queue": [],
            "index": 0,
            "playlist_context": null,
            "radio_virtual_playlist": null,
            "repeat_mode": "Off"
        }
    }))
    .expect("request should deserialize");

    match request {
        DaemonRequest::PlayQueueAt {
            pause_other_mpris_players,
            start_paused,
            start_position_millis,
            suppress_reporting,
            ..
        } => {
            assert!(pause_other_mpris_players);
            assert!(!start_paused);
            assert_eq!(start_position_millis, None);
            assert!(!suppress_reporting);
        }
        other => panic!("unexpected request variant: {other:?}"),
    }
}

#[test]
fn play_queue_at_requests_can_start_paused_without_peer_mpris_pause() {
    let request = serde_json::from_value::<DaemonRequest>(json!({
        "PlayQueueAt": {
            "queue": [],
            "index": 0,
            "playlist_context": null,
            "radio_virtual_playlist": null,
            "repeat_mode": "Off",
            "pause_other_mpris_players": false,
            "start_paused": true,
            "start_position_millis": 42_000
        }
    }))
    .expect("request should deserialize");

    match request {
        DaemonRequest::PlayQueueAt {
            pause_other_mpris_players,
            start_paused,
            start_position_millis,
            ..
        } => {
            assert!(!pause_other_mpris_players);
            assert!(start_paused);
            assert_eq!(start_position_millis, Some(42_000));
        }
        other => panic!("unexpected request variant: {other:?}"),
    }
}
