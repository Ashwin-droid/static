use anyhow::Result;

use crate::models::{PlaylistEditor, PlaylistEntry, PlaylistSummary, Track};

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

impl NetworkState {
    pub fn get_editable_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        self.client.get_editable_playlists()
    }
    pub fn get_playlist_editor(&self, playlist_id: &str) -> Result<PlaylistEditor> {
        self.client.get_playlist_editor(playlist_id)
    }
    pub fn create_playlist(&self, title: &str, video_ids: &[String]) -> Result<String> {
        self.client.create_playlist(title, video_ids)
    }
    pub fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        self.client.delete_playlist(playlist_id)
    }
    pub fn add_playlist_tracks(&self, playlist_id: &str, video_ids: &[String]) -> Result<()> {
        self.client.add_playlist_tracks(playlist_id, video_ids)
    }
    pub fn remove_playlist_entries(
        &self,
        playlist_id: &str,
        entries: &[PlaylistEntry],
    ) -> Result<()> {
        self.client.remove_playlist_entries(playlist_id, entries)
    }
    pub fn move_playlist_entry(
        &self,
        playlist_id: &str,
        set_video_id: &str,
        before_set_video_id: Option<&str>,
    ) -> Result<()> {
        self.client
            .move_playlist_entry(playlist_id, set_video_id, before_set_video_id)
    }
}

impl NetworkState {
    pub fn get_listen_again_pin(
        &self,
        target: &crate::models::ListenAgainTarget,
    ) -> Result<crate::models::ListenAgainPin> {
        self.client.get_listen_again_pin(target)
    }

    pub fn set_listen_again_pin(
        &self,
        target: &crate::models::ListenAgainTarget,
        pinned: bool,
    ) -> Result<()> {
        self.client.set_listen_again_pin(target, pinned)
    }
}
