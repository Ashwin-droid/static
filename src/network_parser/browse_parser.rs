use anyhow::Result;
use serde_json::Value;

use crate::models::{BrowseSectionsPage, PlaylistSummary, Track};

pub(crate) fn parse_browse_tracks(json: &Value) -> Result<Vec<Track>> {
    super::shared_parser::parse_browse_tracks(json)
}

pub(crate) fn parse_channel_page(json: &Value) -> Result<BrowseSectionsPage> {
    super::shared_parser::parse_channel_page(json)
}

pub(crate) fn parse_library_playlists(json: &Value) -> Result<Vec<PlaylistSummary>> {
    super::shared_parser::parse_library_playlists(json)
}

pub(crate) fn parse_playlist_tracks(json: &Value) -> Result<Vec<Track>> {
    super::shared_parser::parse_playlist_tracks(json)
}
