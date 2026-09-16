use crate::models::{Artist, RadioChip, Track};

use super::{chip_rows, match_chip_label, origin_label, queue_rows};

fn track(video_id: &str, title: &str, artists: &[&str]) -> Track {
    Track {
        video_id: video_id.to_string(),
        title: title.to_string(),
        artists: artists
            .iter()
            .map(|name| Artist {
                name: name.to_string(),
                id: None,
                page_type: None,
            })
            .collect(),
        album: None,
        duration: None,
        thumbnail_url: None,
        playback_playlist_id: None,
        playback_params: None,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
    }
}

fn chip(label: &str) -> RadioChip {
    RadioChip {
        label: label.to_string(),
        playlist_id: "PL".to_string(),
        params: None,
    }
}

#[test]
fn origin_label_without_radio_is_always_touched() {
    for index in [0, 1, 5, 100] {
        assert_eq!(origin_label(index, None), "touched");
    }
}

#[test]
fn origin_label_splits_at_touched_len_boundary() {
    assert_eq!(origin_label(0, Some(2)), "touched");
    assert_eq!(origin_label(1, Some(2)), "touched");
    assert_eq!(origin_label(2, Some(2)), "radio_generated");
    assert_eq!(origin_label(3, Some(2)), "radio_generated");
}

#[test]
fn origin_label_touched_len_zero_marks_everything_radio_generated() {
    assert_eq!(origin_label(0, Some(0)), "radio_generated");
    assert_eq!(origin_label(9, Some(0)), "radio_generated");
}

#[test]
fn queue_rows_render_fields_and_origin_boundary() {
    let queue = vec![
        track("id0", "Song Zero", &["Solo"]),
        track("id1", "Song One", &["A", "B"]),
        track("id2", "Song Two", &[]),
    ];
    let rows = queue_rows(&queue, Some(2));
    assert_eq!(
        rows,
        vec![
            vec![
                "Song Zero".to_string(),
                "Solo".to_string(),
                "id0".to_string(),
                "touched".to_string(),
            ],
            vec![
                "Song One".to_string(),
                "A, B".to_string(),
                "id1".to_string(),
                "touched".to_string(),
            ],
            vec![
                "Song Two".to_string(),
                "Unknown artist".to_string(),
                "id2".to_string(),
                "radio_generated".to_string(),
            ],
        ]
    );
}

#[test]
fn queue_rows_without_radio_marks_all_touched() {
    let queue = vec![track("id0", "Zero", &["X"]), track("id1", "One", &["Y"])];
    for row in queue_rows(&queue, None) {
        assert_eq!(row[3], "touched");
    }
}

#[test]
fn queue_rows_touched_len_beyond_queue_marks_all_touched() {
    let queue = vec![track("id0", "Zero", &["X"])];
    for row in queue_rows(&queue, Some(10)) {
        assert_eq!(row[3], "touched");
    }
}

#[test]
fn chip_rows_flag_selected_index() {
    let chips = vec![chip("Popular"), chip("Deep cuts"), chip("Familiar")];
    let rows = chip_rows(&chips, Some(1));
    assert_eq!(
        rows,
        vec![
            vec!["Popular".to_string(), "false".to_string()],
            vec!["Deep cuts".to_string(), "true".to_string()],
            vec!["Familiar".to_string(), "false".to_string()],
        ]
    );
}

#[test]
fn chip_rows_without_selection_are_all_false() {
    let chips = vec![chip("Popular"), chip("Deep cuts")];
    for row in chip_rows(&chips, None) {
        assert_eq!(row[1], "false");
    }
}

#[test]
fn match_chip_label_is_trimmed_and_case_insensitive() {
    let chips = vec![chip("Popular"), chip("Deep cuts")];
    assert_eq!(match_chip_label(&chips, "Popular"), Some(0));
    assert_eq!(match_chip_label(&chips, "popular"), Some(0));
    assert_eq!(match_chip_label(&chips, "  DEEP CUTS  "), Some(1));
    assert_eq!(match_chip_label(&chips, "unknown"), None);
    assert_eq!(match_chip_label(&[], "Popular"), None);
}

#[test]
fn match_chip_label_first_match_wins() {
    let chips = vec![chip("Same"), chip("same")];
    assert_eq!(match_chip_label(&chips, "SAME"), Some(0));
}
