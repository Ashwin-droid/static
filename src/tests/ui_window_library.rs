use super::should_play_album_as_track;
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
