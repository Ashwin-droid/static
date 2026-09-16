use super::page_builders::build_browse_document;
use super::*;
use crate::models::{Artist, HomeSection};

fn sample_track(title: &str) -> Track {
    Track {
        video_id: format!("video-{title}"),
        title: title.to_string(),
        artists: vec![Artist {
            name: "Sample Artist".to_string(),
            id: None,
            page_type: None,
        }],
        album: Some("Sample Album".to_string()),
        duration: Some("3:45".to_string()),
        thumbnail_url: Some("thumb-sample".to_string()),
        playback_playlist_id: None,
        playback_params: None,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
    }
}

fn sample_browse_card(page_type: HomeBrowsePageType, title: &str) -> HomeBrowseCard {
    HomeBrowseCard {
        browse_id: format!("browse-{title}"),
        title: title.to_string(),
        subtitle: Some(format!("{title} subtitle")),
        thumbnail_url: Some("card-thumb".to_string()),
        page_type,
    }
}

#[test]
fn track_browse_pages_render_a_playable_tracks_section() {
    let view = TuiBrowseView {
        card: sample_browse_card(HomeBrowsePageType::Playlist, "Coding Mix"),
        status: BrowseViewStatus::Tracks {
            title: "Coding Mix".to_string(),
            thumbnail_url: Some("banner-thumb".to_string()),
            tracks: vec![sample_track("Alpha"), sample_track("Beta")],
        },
    };

    let doc = build_browse_document(&view, 80);
    assert_eq!(doc.banners.len(), 1);
    assert_eq!(doc.headers.len(), 1);
    assert_eq!(doc.headers[0].title, "TRACKS");
    assert!(doc.headers[0].show_play_controls);
    assert_eq!(doc.cards.len(), 2);
}

#[test]
fn artist_browse_pages_surface_top_songs_first() {
    let top_songs = HomeSection {
        title: "Top songs".to_string(),
        contents: vec![HomeItem::Track(sample_track("Hit Single"))],
    };
    let albums = HomeSection {
        title: "Albums".to_string(),
        contents: vec![HomeItem::BrowseCard(sample_browse_card(
            HomeBrowsePageType::Album,
            "Debut Album",
        ))],
    };
    let view = TuiBrowseView {
        card: sample_browse_card(HomeBrowsePageType::Artist, "Sample Artist"),
        status: BrowseViewStatus::Sections {
            title: "Sample Artist".to_string(),
            thumbnail_url: Some("artist-thumb".to_string()),
            artist_like: true,
            sections: vec![albums, top_songs],
        },
    };

    let doc = build_browse_document(&view, 90);
    assert_eq!(doc.banners.len(), 1);
    assert_eq!(doc.banners[0].eyebrow, "ARTIST");
    assert_eq!(doc.headers.len(), 2);
    assert_eq!(doc.headers[0].title, "TOP SONGS");
    assert!(doc.headers[0].show_play_controls);
}

#[test]
fn scrolled_rect_preserves_negative_viewport_offset() {
    let viewport = Rect::new(10, 5, 50, 10);
    let logical = Rect::new(2, 1, 20, 5);

    assert_eq!(
        widgets::scrolled_rect(viewport, logical, 3),
        Some(Rect::new(12, 3, 20, 5))
    );
    assert_eq!(
        widgets::scrolled_visible_rect(viewport, logical, 3),
        Some(Rect::new(12, 5, 20, 3))
    );
}

#[test]
fn signed_fill_does_not_extend_partially_clipped_cards() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 6));
    let clip = Rect::new(0, 2, 10, 4);

    widgets::fill_clipped_area_signed(
        &mut buffer,
        clip,
        1,
        0,
        4,
        3,
        Style::default().bg(Color::Red),
    );

    assert_eq!(buffer[(1, 2)].bg, Color::Red);
    assert_ne!(buffer[(1, 3)].bg, Color::Red);
}

#[test]
fn playlist_reorder_targets_occurrences_and_accounts_for_removal() {
    let editor = PlaylistEditor {
        playlist_id: "PLtest".into(),
        title: "Test".into(),
        editable: true,
        entries: ["first", "second", "third"]
            .into_iter()
            .map(|id| crate::models::PlaylistEntry {
                track: sample_track("duplicate"),
                set_video_id: Some(id.into()),
            })
            .collect(),
    };
    assert_eq!(
        playlist_edit::playlist_move_ids(&editor, 0, true),
        Some(("first".into(), Some("third".into())))
    );
    assert_eq!(
        playlist_edit::playlist_move_ids(&editor, 1, true),
        Some(("second".into(), None))
    );
    assert_eq!(
        playlist_edit::playlist_move_ids(&editor, 2, false),
        Some(("third".into(), Some("second".into())))
    );
    assert_eq!(playlist_edit::playlist_move_ids(&editor, 0, false), None);
    assert_eq!(playlist_edit::playlist_move_ids(&editor, 2, true), None);
    let mut missing = editor.clone();
    missing.entries[2].set_video_id = None;
    assert_eq!(playlist_edit::playlist_move_ids(&missing, 0, true), None);
}

#[test]
fn playlist_refresh_keeps_selected_occurrence_through_consecutive_moves() {
    let original = PlaylistEditor {
        playlist_id: "PLtest".into(),
        title: "Test".into(),
        editable: true,
        entries: ["first", "second", "third"]
            .into_iter()
            .map(|id| crate::models::PlaylistEntry {
                track: sample_track("duplicate"),
                set_video_id: Some(id.into()),
            })
            .collect(),
    };
    let mut moved = original.clone();
    moved.entries.swap(0, 1);
    let selected = playlist_edit::refreshed_playlist_selection(&original, 0, &moved);
    assert_eq!(selected, 1);
    assert_eq!(
        playlist_edit::playlist_move_ids(&moved, selected, true),
        Some(("first".into(), None))
    );
    let mut moved_again = moved.clone();
    moved_again.entries.swap(1, 2);
    assert_eq!(
        playlist_edit::refreshed_playlist_selection(&moved, selected, &moved_again),
        2
    );
    let mut removed = moved_again.clone();
    removed.entries.pop();
    assert_eq!(
        playlist_edit::refreshed_playlist_selection(&moved_again, 2, &removed),
        1
    );
    removed.entries.clear();
    assert_eq!(
        playlist_edit::refreshed_playlist_selection(&moved_again, 2, &removed),
        0
    );
}

#[test]
fn deleting_playlist_requires_explicit_confirmation_and_editable_target() {
    let mut editor = PlaylistEditor {
        playlist_id: "PLtest".into(),
        title: "Delete me".into(),
        editable: true,
        entries: vec![],
    };
    for buffer in ["", "D", "DELET", "delete", "DELETE ", "Delete me"] {
        assert!(!playlist_edit::playlist_delete_confirmed(&editor, buffer));
    }
    assert!(playlist_edit::playlist_delete_confirmed(&editor, "DELETE"));
    editor.editable = false;
    assert!(!playlist_edit::playlist_delete_confirmed(&editor, "DELETE"));
    editor.editable = true;
    editor.playlist_id.clear();
    assert!(!playlist_edit::playlist_delete_confirmed(&editor, "DELETE"));
}
