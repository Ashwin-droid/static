use super::dismiss_queue_after_current_state;
use crate::models::{Artist, RadioVirtualPlaylist, Track};

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
