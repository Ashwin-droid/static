use anyhow::Result;

use crate::models::{PlaylistSummary, Track};

use super::session_state::NetworkState;

impl NetworkState {
    pub fn history(&self) -> Result<Vec<Track>> {
        self.client.get_history()
    }

    pub fn get_history(&self) -> Result<Vec<Track>> {
        self.history()
    }

    pub fn library_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        self.client.get_library_playlists()
    }

    pub fn get_library_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        self.library_playlists()
    }
}
