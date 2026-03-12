use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub artists: Vec<Artist>,
    pub album: Option<String>,
    pub duration: Option<String>,
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artist {
    pub name: String,
    pub id: Option<String>,
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

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
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
}

/// Lightweight summary for a playlist in the user's library listing (no embedded tracks).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub playlist_id: String,
    pub title: String,
    pub thumbnail_url: Option<String>,
    pub subtitle: Option<String>,
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

/// The locally-managed virtual-playlist state for a radio queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadioVirtualPlaylist {
    pub source_video_id: String,
    pub touched_len: usize,
    pub chips: Vec<RadioChip>,
}
