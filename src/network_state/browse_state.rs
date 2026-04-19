use anyhow::Result;

use crate::models::{BrowseSectionsPage, Track};

use super::session_state::NetworkState;

impl NetworkState {
    pub fn playlist(&self, playlist_id: &str) -> Result<Vec<Track>> {
        self.client.get_playlist(playlist_id)
    }

    pub fn get_playlist(&self, playlist_id: &str) -> Result<Vec<Track>> {
        self.playlist(playlist_id)
    }

    pub fn browse_tracks(&self, browse_id: &str) -> Result<Vec<Track>> {
        self.client.get_browse_tracks(browse_id)
    }

    pub fn get_browse_tracks(&self, browse_id: &str) -> Result<Vec<Track>> {
        self.browse_tracks(browse_id)
    }

    pub fn browse_sections(&self, browse_id: &str) -> Result<BrowseSectionsPage> {
        self.client.get_channel_page(browse_id)
    }

    pub fn get_channel_page(&self, browse_id: &str) -> Result<BrowseSectionsPage> {
        self.browse_sections(browse_id)
    }
}
