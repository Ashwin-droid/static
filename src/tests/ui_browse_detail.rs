use super::{browse_collection_context, should_play_album_as_track};
use crate::ipc::PlaylistContextSource;
use crate::models::HomeBrowsePageType;

#[test]
fn single_track_albums_play_like_tracks() {
    assert!(should_play_album_as_track(HomeBrowsePageType::Album, 1));
}

#[test]
fn multi_track_albums_still_open_album_pages() {
    assert!(!should_play_album_as_track(HomeBrowsePageType::Album, 2));
}

#[test]
fn non_album_singletons_keep_their_existing_behavior() {
    assert!(!should_play_album_as_track(HomeBrowsePageType::Playlist, 1));
    assert!(!should_play_album_as_track(
        HomeBrowsePageType::PodcastShow,
        1
    ));
}

#[test]
fn playlist_pages_carry_a_browse_playlist_context() {
    let context = browse_collection_context(
        HomeBrowsePageType::Playlist,
        "VLPL123",
        "Daily Mix",
        Some("https://img.example/cover.jpg"),
        Some("24 tracks"),
    )
    .expect("playlist browse pages produce a context");
    assert_eq!(context.playlist_id, "VLPL123");
    assert_eq!(context.title, "Daily Mix");
    assert_eq!(
        context.thumbnail_url.as_deref(),
        Some("https://img.example/cover.jpg")
    );
    assert_eq!(context.subtitle.as_deref(), Some("24 tracks"));
    assert_eq!(context.source, PlaylistContextSource::Browse);
}

#[test]
fn album_pages_carry_a_browse_playlist_context() {
    let context = browse_collection_context(
        HomeBrowsePageType::Album,
        "MPREb_abc",
        "Some Album",
        None,
        None,
    )
    .expect("album browse pages produce a context");
    assert_eq!(context.playlist_id, "MPREb_abc");
    assert_eq!(context.thumbnail_url, None);
    assert_eq!(context.subtitle, None);
    assert_eq!(context.source, PlaylistContextSource::Browse);
}

#[test]
fn non_collection_pages_get_no_playlist_context() {
    for page_type in [
        HomeBrowsePageType::Artist,
        HomeBrowsePageType::UserChannel,
        HomeBrowsePageType::PodcastShow,
    ] {
        assert!(browse_collection_context(page_type, "UC123", "Name", None, None).is_none());
    }
}
