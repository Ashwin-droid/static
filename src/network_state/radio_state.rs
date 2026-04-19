use anyhow::Result;

use crate::models::RadioQueue;

use super::session_state::NetworkState;

impl NetworkState {
    pub fn radio(
        &self,
        video_id: &str,
        playlist_id: Option<&str>,
        params: Option<&str>,
    ) -> Result<RadioQueue> {
        self.client.get_radio(video_id, playlist_id, params)
    }

    pub fn get_radio(
        &self,
        video_id: &str,
        playlist_id: Option<&str>,
        params: Option<&str>,
    ) -> Result<RadioQueue> {
        self.radio(video_id, playlist_id, params)
    }
}
