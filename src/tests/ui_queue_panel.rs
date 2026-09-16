use super::{
    DIVIDER_HEIGHT, LIST_TOP_MARGIN, QUEUE_ROW_HEIGHT, dismiss_queue_after_current_state,
    normalized_preview_slot, preview_slot_for_queue_y, queue_drag_autoscroll_velocity,
    slot_for_content_y_with, track_list_payload, track_list_subtitle,
};
use crate::models::{Artist, RadioVirtualPlaylist, Track};
use crate::ui::app::QueueDragPayload;

fn test_track(video_id: &str) -> Track {
    Track {
        video_id: video_id.to_string(),
        title: video_id.to_string(),
        artists: vec![Artist {
            name: "Artist".to_string(),
            id: None,
            page_type: None,
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

fn row_top(index: usize) -> f32 {
    LIST_TOP_MARGIN + index as f32 * QUEUE_ROW_HEIGHT
}

#[test]
fn dismiss_queue_after_current_truncates_tail_and_clears_radio_mode() {
    let mut queue = vec![
        test_track("seed"),
        test_track("tail-a"),
        test_track("tail-b"),
    ];
    let mut radio_virtual_playlist = Some(RadioVirtualPlaylist {
        source_video_id: "seed".to_string(),
        touched_len: 1,
        chips: Vec::new(),
        selected_chip_index: None,
    });

    let changed = dismiss_queue_after_current_state(&mut queue, 1, &mut radio_virtual_playlist);

    assert!(changed);
    assert_eq!(queue, vec![test_track("seed")]);
    assert!(radio_virtual_playlist.is_none());
}

#[test]
fn dismiss_queue_after_current_clears_radio_mode_without_tail_tracks() {
    let mut queue = vec![test_track("seed")];
    let mut radio_virtual_playlist = Some(RadioVirtualPlaylist {
        source_video_id: "seed".to_string(),
        touched_len: 1,
        chips: Vec::new(),
        selected_chip_index: None,
    });

    let changed = dismiss_queue_after_current_state(&mut queue, 1, &mut radio_virtual_playlist);

    assert!(changed);
    assert_eq!(queue, vec![test_track("seed")]);
    assert!(radio_virtual_playlist.is_none());
}

#[test]
fn dismiss_queue_after_current_is_noop_without_tail_or_radio() {
    let mut queue = vec![test_track("seed")];
    let mut radio_virtual_playlist = None;

    let changed = dismiss_queue_after_current_state(&mut queue, 1, &mut radio_virtual_playlist);

    assert!(!changed);
    assert_eq!(queue, vec![test_track("seed")]);
    assert!(radio_virtual_playlist.is_none());
}

#[test]
fn preview_slot_targets_row_above_in_top_half() {
    let y = row_top(1) + QUEUE_ROW_HEIGHT * 0.25;
    assert_eq!(preview_slot_for_queue_y(y, 4), 1);
}

#[test]
fn preview_slot_targets_next_slot_in_bottom_half() {
    let y = row_top(1) + QUEUE_ROW_HEIGHT * 0.75;
    assert_eq!(preview_slot_for_queue_y(y, 4), 2);
}

#[test]
fn preview_slot_clamps_to_queue_bounds() {
    assert_eq!(preview_slot_for_queue_y(-20.0, 4), 0);
    assert_eq!(preview_slot_for_queue_y(0.0, 4), 0);
    assert_eq!(preview_slot_for_queue_y(10_000.0, 4), 4);
}

#[test]
fn preview_slot_is_zero_for_empty_queue() {
    assert_eq!(preview_slot_for_queue_y(300.0, 0), 0);
}

#[test]
fn normalized_slot_filters_noop_drops_around_source() {
    assert_eq!(normalized_preview_slot(2, Some(2), 5), None);
    assert_eq!(normalized_preview_slot(3, Some(2), 5), None);
}

#[test]
fn normalized_slot_preserves_raw_slot_for_real_moves() {
    assert_eq!(normalized_preview_slot(0, Some(2), 5), Some(0));
    assert_eq!(normalized_preview_slot(4, Some(2), 5), Some(4));
    assert_eq!(normalized_preview_slot(5, Some(2), 5), Some(5));
}

#[test]
fn normalized_slot_without_source_clamps_external_payloads() {
    assert_eq!(normalized_preview_slot(3, None, 5), Some(3));
    assert_eq!(normalized_preview_slot(9, None, 5), Some(5));
}

#[test]
fn divider_hover_targets_radio_boundary_slot() {
    let divider_top = row_top(2);
    assert_eq!(
        slot_for_content_y_with(Some(2), 4, divider_top + DIVIDER_HEIGHT * 0.5),
        2
    );
}

#[test]
fn slots_below_divider_compensate_for_divider_height() {
    let first_tail_row_top = row_top(2) + DIVIDER_HEIGHT;
    assert_eq!(
        slot_for_content_y_with(Some(2), 4, first_tail_row_top + QUEUE_ROW_HEIGHT * 0.25),
        2
    );
    assert_eq!(
        slot_for_content_y_with(Some(2), 4, first_tail_row_top + QUEUE_ROW_HEIGHT * 0.75),
        3
    );
}

#[test]
fn slots_without_radio_tail_use_plain_row_math() {
    let y = row_top(1) + QUEUE_ROW_HEIGHT * 0.75;
    assert_eq!(slot_for_content_y_with(None, 4, y), 2);
    assert_eq!(
        slot_for_content_y_with(None, 4, y),
        preview_slot_for_queue_y(y, 4)
    );
}

#[test]
fn autoscroll_is_idle_outside_edge_bands() {
    assert_eq!(queue_drag_autoscroll_velocity(240.0, 480.0), 0.0);
    assert_eq!(queue_drag_autoscroll_velocity(56.0, 480.0), 0.0);
    assert_eq!(queue_drag_autoscroll_velocity(424.0, 480.0), 0.0);
}

#[test]
fn autoscroll_peaks_at_viewport_edges() {
    assert_eq!(queue_drag_autoscroll_velocity(0.0, 480.0), -28.0);
    assert_eq!(queue_drag_autoscroll_velocity(480.0, 480.0), 28.0);
}

#[test]
fn autoscroll_scales_within_the_band() {
    assert_eq!(queue_drag_autoscroll_velocity(28.0, 480.0), -18.0);
    assert_eq!(queue_drag_autoscroll_velocity(452.0, 480.0), 18.0);
}

#[test]
fn autoscroll_is_disabled_without_viewport() {
    assert_eq!(queue_drag_autoscroll_velocity(100.0, 0.0), 0.0);
}

#[test]
fn track_list_payload_is_empty_for_a_control_with_nothing_to_play() {
    assert!(track_list_payload(Vec::new(), "Videos", false).is_none());
    assert!(track_list_payload(Vec::new(), "Videos", true).is_none());
}

#[test]
fn track_list_payload_keeps_play_all_order_and_title() {
    let tracks = vec![test_track("a"), test_track("b"), test_track("c")];
    let payload = track_list_payload(tracks.clone(), "Videos", false).expect("payload");
    assert_eq!(
        payload,
        QueueDragPayload::TrackList {
            tracks,
            title: "Videos".to_owned(),
            shuffled: false,
        }
    );
}

#[test]
fn track_list_payload_shuffles_at_press_time_without_losing_tracks() {
    let tracks: Vec<Track> = (0..40).map(|i| test_track(&format!("t{i}"))).collect();
    let Some(QueueDragPayload::TrackList {
        tracks: shuffled,
        shuffled: flag,
        ..
    }) = track_list_payload(tracks.clone(), "Videos", true)
    else {
        panic!("expected a track list payload");
    };
    assert!(flag);
    let mut sorted = shuffled.clone();
    sorted.sort_by(|a, b| a.video_id.cmp(&b.video_id));
    let mut expected = tracks.clone();
    expected.sort_by(|a, b| a.video_id.cmp(&b.video_id));
    assert_eq!(sorted, expected);
    // 40 tracks landing in their original order after a shuffle is
    // astronomically unlikely; the drop must insert the shuffled order.
    assert_ne!(shuffled, tracks);
}

#[test]
fn track_list_subtitle_reports_count_and_shuffle() {
    assert_eq!(track_list_subtitle(1, false), "1 song");
    assert_eq!(track_list_subtitle(12, false), "12 songs");
    assert_eq!(track_list_subtitle(12, true), "Shuffled · 12 songs");
}
