use super::{
    DirectMusicUrlTarget, artist_string, extract_listen_again_tracks, history_thumbnail_candidate,
    parse_direct_music_url,
};
use crate::models::{Artist, HomeBrowseCard, HomeBrowsePageType, HomeItem, HomeSection, Track};

fn track(video_id: &str, title: &str, artists: &[&str]) -> Track {
    Track {
        video_id: video_id.to_string(),
        title: title.to_string(),
        artists: artists
            .iter()
            .map(|name| Artist {
                name: name.to_string(),
                id: None,
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

fn video(target: Option<DirectMusicUrlTarget>) -> String {
    match target {
        Some(DirectMusicUrlTarget::Video { video_id }) => video_id,
        other => panic!("expected video target, got {other:?}"),
    }
}

fn browse(target: Option<DirectMusicUrlTarget>) -> (String, HomeBrowsePageType) {
    match target {
        Some(DirectMusicUrlTarget::Browse {
            browse_id,
            page_type,
        }) => (browse_id, page_type),
        other => panic!("expected browse target, got {other:?}"),
    }
}

#[test]
fn watch_urls_resolve_to_video_targets() {
    assert_eq!(
        video(parse_direct_music_url(
            "https://music.youtube.com/watch?v=abc123XYZ_-"
        )),
        "abc123XYZ_-"
    );
    assert_eq!(
        video(parse_direct_music_url(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&feature=share"
        )),
        "dQw4w9WgXcQ"
    );
}

#[test]
fn short_youtu_be_links_resolve_to_video_targets() {
    assert_eq!(
        video(parse_direct_music_url("https://youtu.be/dQw4w9WgXcQ")),
        "dQw4w9WgXcQ"
    );
    assert_eq!(
        video(parse_direct_music_url("youtu.be/dQw4w9WgXcQ?t=42")),
        "dQw4w9WgXcQ"
    );
}

#[test]
fn scheme_less_urls_are_normalized_before_parsing() {
    assert_eq!(
        video(parse_direct_music_url("music.youtube.com/watch?v=abc")),
        "abc"
    );
    assert_eq!(
        video(parse_direct_music_url("www.youtube.com/watch?v=abc")),
        "abc"
    );
}

#[test]
fn watch_list_urls_without_video_become_browse_targets() {
    let (browse_id, page_type) = browse(parse_direct_music_url(
        "https://music.youtube.com/watch?list=RDCLAK5uy_abc",
    ));
    assert_eq!(browse_id, "VLRDCLAK5uy_abc");
    assert_eq!(page_type, HomeBrowsePageType::Playlist);
}

#[test]
fn playlist_urls_normalize_the_vl_prefix_once() {
    let (browse_id, page_type) = browse(parse_direct_music_url(
        "https://music.youtube.com/playlist?list=OLAK5uy_xyz",
    ));
    assert_eq!(browse_id, "VLOLAK5uy_xyz");
    assert_eq!(page_type, HomeBrowsePageType::Playlist);

    let (already_prefixed, _) = browse(parse_direct_music_url(
        "https://music.youtube.com/playlist?list=VLPLxyz",
    ));
    assert_eq!(already_prefixed, "VLPLxyz");
}

#[test]
fn page_type_inference_uses_the_raw_list_id_gtk_parity_wart() {
    // GTK infers the page type from the UN-normalized list id, so a plain
    // "PL..." playlist link infers Artist while still browsing "VLPL...".
    let (browse_id, page_type) = browse(parse_direct_music_url(
        "https://music.youtube.com/playlist?list=PL123",
    ));
    assert_eq!(browse_id, "VLPL123");
    assert_eq!(page_type, HomeBrowsePageType::Artist);
}

#[test]
fn browse_and_channel_paths_infer_page_types_from_id_prefixes() {
    let (browse_id, page_type) = browse(parse_direct_music_url(
        "https://music.youtube.com/browse/MPREb_abc",
    ));
    assert_eq!(browse_id, "MPREb_abc");
    assert_eq!(page_type, HomeBrowsePageType::Album);

    let (_, podcast) = browse(parse_direct_music_url(
        "https://music.youtube.com/browse/MPSPabc",
    ));
    assert_eq!(podcast, HomeBrowsePageType::PodcastShow);

    let (_, channel) = browse(parse_direct_music_url(
        "https://music.youtube.com/channel/UCabc",
    ));
    assert_eq!(channel, HomeBrowsePageType::UserChannel);

    let (_, artist) = browse(parse_direct_music_url(
        "https://music.youtube.com/browse/FEmusic_whatever",
    ));
    assert_eq!(artist, HomeBrowsePageType::Artist);
}

#[test]
fn bare_known_id_prefixes_are_browse_references() {
    let (browse_id, page_type) = browse(parse_direct_music_url("UCabcdef"));
    assert_eq!(browse_id, "UCabcdef");
    assert_eq!(page_type, HomeBrowsePageType::UserChannel);

    let (_, album) = browse(parse_direct_music_url("MPREb_xyz"));
    assert_eq!(album, HomeBrowsePageType::Album);

    let (_, olak) = browse(parse_direct_music_url("OLAK5uy_abc"));
    assert_eq!(olak, HomeBrowsePageType::Playlist);

    let (_, radio) = browse(parse_direct_music_url("/RDAMVMabc/"));
    assert_eq!(radio, HomeBrowsePageType::Playlist);
}

#[test]
fn slash_prefixed_browse_references_parse_without_a_host() {
    let (browse_id, page_type) = browse(parse_direct_music_url("/browse/MPREb_42"));
    assert_eq!(browse_id, "MPREb_42");
    assert_eq!(page_type, HomeBrowsePageType::Album);

    let (channel_id, channel_type) = browse(parse_direct_music_url("/channel/UC42/"));
    assert_eq!(channel_id, "UC42");
    assert_eq!(channel_type, HomeBrowsePageType::UserChannel);
}

#[test]
fn plain_queries_and_foreign_hosts_are_not_url_targets() {
    assert_eq!(parse_direct_music_url("rain by face"), None);
    assert_eq!(
        parse_direct_music_url("https://example.com/watch?v=abc"),
        None
    );
    assert_eq!(
        parse_direct_music_url("https://music.youtube.com/library"),
        None
    );
    assert_eq!(parse_direct_music_url(""), None);
    assert_eq!(parse_direct_music_url("/browse/"), None);
}

#[test]
fn watch_url_with_video_and_list_prefers_the_video() {
    assert_eq!(
        video(parse_direct_music_url(
            "https://music.youtube.com/watch?v=abc&list=PL123"
        )),
        "abc"
    );
}

#[test]
fn listen_again_extraction_prefers_the_exact_title() {
    let sections = vec![
        HomeSection {
            title: "More from listen again radio".to_string(),
            contents: vec![HomeItem::Track(track("wrong", "Wrong", &[]))],
        },
        HomeSection {
            title: "LISTEN AGAIN".to_string(),
            contents: vec![
                HomeItem::Track(track("right", "Right", &[])),
                HomeItem::BrowseCard(HomeBrowseCard {
                    browse_id: "MPREb_x".to_string(),
                    title: "Album".to_string(),
                    subtitle: None,
                    thumbnail_url: None,
                    page_type: HomeBrowsePageType::Album,
                }),
            ],
        },
    ];

    let tracks = extract_listen_again_tracks(&sections);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].video_id, "right");
}

#[test]
fn listen_again_extraction_falls_back_to_a_containing_title() {
    let sections = vec![
        HomeSection {
            title: "Quick picks".to_string(),
            contents: vec![HomeItem::Track(track("other", "Other", &[]))],
        },
        HomeSection {
            title: "More to listen again to".to_string(),
            contents: vec![HomeItem::Track(track("fallback", "Fallback", &[]))],
        },
    ];

    let tracks = extract_listen_again_tracks(&sections);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].video_id, "fallback");
}

#[test]
fn listen_again_extraction_is_empty_without_a_matching_section() {
    let sections = vec![HomeSection {
        title: "Quick picks".to_string(),
        contents: vec![HomeItem::Track(track("other", "Other", &[]))],
    }];
    assert!(extract_listen_again_tracks(&sections).is_empty());
}

#[test]
fn artist_string_joins_names_and_falls_back_to_unknown() {
    assert_eq!(
        artist_string(&track("v", "T", &["A", "B"])),
        "A, B".to_string()
    );
    assert_eq!(artist_string(&track("v", "T", &[])), "Unknown artist");
}

#[test]
fn history_thumbnails_never_point_at_local_files() {
    assert_eq!(
        history_thumbnail_candidate(Some("https://i.ytimg.com/x.jpg")),
        Some("https://i.ytimg.com/x.jpg")
    );
    assert_eq!(history_thumbnail_candidate(Some("file:///tmp/x.jpg")), None);
    assert_eq!(history_thumbnail_candidate(None), None);
}
