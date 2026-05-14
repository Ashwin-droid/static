use super::page_builders::build_browse_document;
use super::playback::{
    effective_radio_chips, hydrate_radio_virtual_playlist_chips, merge_radio_chip_update,
};
use super::*;
use crate::models::{Artist, HomeSection};

fn sample_track(title: &str) -> Track {
    Track {
        video_id: format!("video-{title}"),
        title: title.to_string(),
        artists: vec![Artist {
            name: "Sample Artist".to_string(),
            id: None,
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

fn sample_radio_chip(label: &str, playlist_id: &str, params: Option<&str>) -> RadioChip {
    RadioChip {
        label: label.to_string(),
        playlist_id: playlist_id.to_string(),
        params: params.map(str::to_string),
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
fn effective_radio_chips_fall_back_to_memory_for_same_source() {
    let remembered = vec![
        sample_radio_chip("All", "RDseed", None),
        sample_radio_chip("Discover", "RDdiscover", Some("discover")),
    ];
    let mut memory = HashMap::new();
    memory.insert("seed-video".to_string(), remembered.clone());

    let effective = effective_radio_chips(
        Some(&RadioVirtualPlaylist {
            source_video_id: "seed-video".to_string(),
            touched_len: 1,
            chips: Vec::new(),
        }),
        &memory,
    );

    assert_eq!(effective, remembered);
}

#[test]
fn hydrate_radio_virtual_playlist_uses_previous_snapshot_chips() {
    let previous_radio = RadioVirtualPlaylist {
        source_video_id: "seed-video".to_string(),
        touched_len: 1,
        chips: vec![sample_radio_chip("All", "RDseed", None)],
    };
    let mut next_radio = Some(RadioVirtualPlaylist {
        source_video_id: "seed-video".to_string(),
        touched_len: 2,
        chips: Vec::new(),
    });

    hydrate_radio_virtual_playlist_chips(&mut next_radio, Some(&previous_radio), &HashMap::new());

    assert_eq!(
        next_radio.expect("radio queue should still exist").chips,
        previous_radio.chips
    );
}

#[test]
fn merge_radio_chip_update_preserves_existing_when_refresh_returns_empty() {
    let existing = vec![sample_radio_chip("All", "RDseed", None)];

    assert_eq!(merge_radio_chip_update(&existing, Vec::new()), existing);
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
