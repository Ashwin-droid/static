use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub artists: Vec<Artist>,
    pub album: Option<String>,
    pub duration: Option<String>,
    pub thumbnail_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_playlist_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_params: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_interested_feedback_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_feedback_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artist {
    pub name: String,
    /// Browse id of the artist's page, when the row linked one.
    pub id: Option<String>,
    /// Page type behind `id`: an official artist page or an uploader's user
    /// channel. Absent for older persisted rows; treated as an artist page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_type: Option<HomeBrowsePageType>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeSection {
    pub title: String,
    pub contents: Vec<HomeItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomePage {
    pub sections: Vec<HomeSection>,
    pub continuation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseSectionsPage {
    pub title: Option<String>,
    pub thumbnail_url: Option<String>,
    pub artist_like: bool,
    pub sections: Vec<HomeSection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HomeItem {
    Track(Track),
    BrowseCard(HomeBrowseCard),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeBrowseCard {
    pub browse_id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub thumbnail_url: Option<String>,
    pub page_type: HomeBrowsePageType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HomeBrowsePageType {
    Playlist,
    Album,
    Artist,
    UserChannel,
    PodcastShow,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    pub playlist_id: String,
    pub title: String,
    pub description: Option<String>,
    pub tracks: Vec<Track>,
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchResult {
    pub sections: Vec<HomeSection>,
}

/// Lightweight summary for a playlist in the user's library listing (no embedded tracks).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub playlist_id: String,
    pub title: String,
    pub thumbnail_url: Option<String>,
    pub subtitle: Option<String>,
}

/// One occurrence of a song in a remote playlist. Duplicate songs have distinct
/// set-video IDs; mutations must address the occurrence, not just the song.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistEntry {
    pub track: Track,
    pub set_video_id: Option<String>,
}

/// Fresh remote playlist data used by editors independently of playback state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistEditor {
    pub playlist_id: String,
    pub title: String,
    pub editable: bool,
    pub entries: Vec<PlaylistEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListenAgainTarget {
    Track(String),
    Playlist(String),
}

/// Server-provided commands for the account's Listen again pin, not offline storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenAgainPin {
    pub pinned: bool,
    pub pin_token: String,
    pub unpin_token: String,
}

/// A single filter chip from a radio queue (e.g. "All", "Discover", "J-pop").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadioChip {
    pub label: String,
    pub playlist_id: String,
    /// Opaque params blob required by the API to activate this filter.
    pub params: Option<String>,
}

/// The result of starting or switching a radio queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioQueue {
    pub playlist_id: String,
    pub tracks: Vec<Track>,
    pub chips: Vec<RadioChip>,
    /// Opaque token for loading more tracks.
    pub continuation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossDeviceQueue {
    pub source_label: Option<String>,
    pub last_watch_timestamp_millis: Option<u64>,
    pub playlist_id: Option<String>,
    pub tracks: Vec<Track>,
    pub selected_index: usize,
}

/// The locally-managed virtual-playlist state for a radio queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadioVirtualPlaylist {
    pub source_video_id: String,
    pub touched_len: usize,
    pub chips: Vec<RadioChip>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_chip_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedLyricLine {
    pub start_millis: u64,
    pub end_millis: Option<u64>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncedLyricsSource {
    TimedLyrics,
    Captions,
    AutoCaptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedLyrics {
    pub source: SyncedLyricsSource,
    pub language_code: Option<String>,
    pub track_name: Option<String>,
    pub lines: Vec<SyncedLyricLine>,
}
