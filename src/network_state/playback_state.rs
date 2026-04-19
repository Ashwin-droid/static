use anyhow::Result;

use crate::models::SyncedLyrics;
use crate::network::{SongRating, TrackingInfo};

use super::session_state::NetworkState;

impl NetworkState {
    pub fn song_rating(&self, video_id: &str) -> Result<SongRating> {
        self.client.get_song_rating(video_id)
    }

    pub fn get_song_rating(&self, video_id: &str) -> Result<SongRating> {
        self.song_rating(video_id)
    }

    pub fn set_song_rating(&self, video_id: &str, rating: SongRating) -> Result<()> {
        self.client.rate_song(video_id, rating)
    }

    pub fn rate_song(&self, video_id: &str, rating: SongRating) -> Result<()> {
        self.set_song_rating(video_id, rating)
    }

    pub fn submit_feedback_token(&self, feedback_token: &str) -> Result<()> {
        self.client.submit_feedback_token(feedback_token)
    }

    pub fn synced_lyrics(&self, video_id: &str) -> Result<Option<SyncedLyrics>> {
        self.client.get_synced_lyrics(video_id)
    }

    pub fn get_synced_lyrics(&self, video_id: &str) -> Result<Option<SyncedLyrics>> {
        self.synced_lyrics(video_id)
    }

    pub fn audio_stream_url(&self, video_id: &str) -> Result<String> {
        self.client.get_audio_url_no_track(video_id)
    }

    pub fn get_audio_url_no_track(&self, video_id: &str) -> Result<String> {
        self.audio_stream_url(video_id)
    }

    pub fn start_playback_tracking(
        &self,
        video_id: &str,
        session_started_at: std::time::Instant,
        playlist_id: Option<&str>,
        params: Option<&str>,
    ) -> Option<TrackingInfo> {
        self.client
            .start_playback_tracking(video_id, session_started_at, playlist_id, params)
    }
}
