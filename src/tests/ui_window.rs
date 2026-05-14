use super::{
    BrowseBackTarget, ContentViewKind, apply_radio_chip_update, dedupe_radio_recommendations,
    radio_touched_len_after_reorder, selected_browse_back_target_kind,
};
use crate::models::{Artist, RadioChip, RadioVirtualPlaylist, Track};

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

fn test_chip(label: &str, playlist_id: &str, params: Option<&str>) -> RadioChip {
    RadioChip {
        label: label.to_string(),
        playlist_id: playlist_id.to_string(),
        params: params.map(str::to_string),
    }
}

#[test]
fn reordering_generated_track_into_touched_prefix_extends_boundary() {
    assert_eq!(radio_touched_len_after_reorder(3, 5, 1), 4);
}

#[test]
fn reordering_touched_track_farther_down_marks_drop_location() {
    assert_eq!(radio_touched_len_after_reorder(3, 1, 5), 6);
}

#[test]
fn reordering_within_touched_prefix_preserves_boundary_width() {
    assert_eq!(radio_touched_len_after_reorder(4, 1, 2), 4);
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
fn radio_chip_update_preserves_existing_chips_when_not_replaced() {
    let existing_chips = vec![
        test_chip("All", "RDseed", None),
        test_chip("Discover", "RDdiscover", Some("discover-params")),
    ];
    let mut radio = RadioVirtualPlaylist {
        source_video_id: "seed".to_string(),
        touched_len: 1,
        chips: existing_chips.clone(),
    };

    apply_radio_chip_update(&mut radio, None);

    assert_eq!(radio.chips, existing_chips);
}

#[test]
fn radio_chip_update_replaces_existing_chips_when_provided() {
    let mut radio = RadioVirtualPlaylist {
        source_video_id: "seed".to_string(),
        touched_len: 1,
        chips: vec![test_chip("All", "RDseed", None)],
    };
    let replacement_chips = vec![test_chip("J-pop", "RDjpop", Some("jpop-params"))];

    apply_radio_chip_update(&mut radio, Some(replacement_chips.clone()));

    assert_eq!(radio.chips, replacement_chips);
}

#[test]
fn search_pages_capture_search_as_browse_back_target() {
    assert_eq!(
        selected_browse_back_target_kind(ContentViewKind::Search, &BrowseBackTarget::Home, true,),
        ContentViewKind::Search
    );
}

#[test]
fn nested_browse_pages_keep_existing_search_return_target() {
    assert_eq!(
        selected_browse_back_target_kind(
            ContentViewKind::BrowseDetail,
            &BrowseBackTarget::Search(super::SearchPageCache {
                query: "rain by face".to_string(),
                results: super::SearchResult::default(),
                scroll_position: 320.0,
            }),
            true,
        ),
        ContentViewKind::Search
    );
}

#[test]
fn non_search_pages_fall_back_to_home_back_target() {
    assert_eq!(
        selected_browse_back_target_kind(
            ContentViewKind::Home,
            &BrowseBackTarget::Search(super::SearchPageCache {
                query: "rain by face".to_string(),
                results: super::SearchResult::default(),
                scroll_position: 320.0,
            }),
            true,
        ),
        ContentViewKind::Home
    );
}
