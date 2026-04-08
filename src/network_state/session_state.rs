use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};

use crate::models::{
    BrowseSectionsPage, HomePage, PlaylistSummary, RadioQueue, SearchResult, SyncedLyrics, Track,
};
use crate::network::{SongRating, TrackingInfo, YtMusicClient, get_cookie, get_vr_refresh_token};

#[derive(Default)]
pub(super) struct HomePaginationCache {
    pub(super) tokens_by_page: Vec<Option<String>>,
}

pub struct NetworkState {
    pub(super) client: Arc<YtMusicClient>,
    pub(super) home_cache: Mutex<HomePaginationCache>,
}

pub enum NetworkValidationOutcome {
    Authenticated(Arc<NetworkState>),
    Rejected,
    Deferred(Arc<NetworkState>),
}

impl NetworkState {
    pub fn new(client: YtMusicClient) -> Self {
        Self {
            client: Arc::new(client),
            home_cache: Mutex::new(HomePaginationCache {
                tokens_by_page: vec![None],
            }),
        }
    }

    pub fn from_cookie(cookie: &str) -> Result<Self> {
        let client = YtMusicClient::new(cookie)?;
        if let Some(token) = get_vr_refresh_token() {
            client.set_vr_refresh_token(&token);
        }
        Ok(Self::new(client))
    }

    pub fn from_saved_session() -> Result<Self> {
        let cookie = get_cookie().ok_or_else(|| anyhow!("No saved cookie available"))?;
        Self::from_cookie(&cookie)
    }

    pub fn check_auth(&self) -> Result<bool> {
        self.client.check_auth()
    }

    pub fn validate_cookie(cookie: String, source_label: &str) -> NetworkValidationOutcome {
        let state = match Self::from_cookie(&cookie) {
            Ok(state) => Arc::new(state),
            Err(err) => {
                crate::app_log!("[AUTH] {source_label}: invalid cookie: {err}");
                return NetworkValidationOutcome::Rejected;
            }
        };

        match state.check_auth() {
            Ok(true) => NetworkValidationOutcome::Authenticated(state),
            Ok(false) => {
                crate::app_log!("[AUTH] {source_label}: cookie is not authenticated");
                NetworkValidationOutcome::Rejected
            }
            Err(err) => {
                crate::app_log!("[AUTH] {source_label}: auth check failed: {err}");
                NetworkValidationOutcome::Deferred(state)
            }
        }
    }

    pub fn history(&self) -> Result<Vec<Track>> {
        self.client.get_history()
    }

    pub fn get_history(&self) -> Result<Vec<Track>> {
        self.history()
    }

    pub fn search(&self, query: &str) -> Result<SearchResult> {
        self.client.search(query)
    }

    pub fn get_search_suggestions(&self, query: &str) -> Result<Vec<String>> {
        self.search_suggestions(query)
    }

    pub fn search_suggestions(&self, query: &str) -> Result<Vec<String>> {
        self.client.get_search_suggestions(query)
    }

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

    pub fn library_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        self.client.get_library_playlists()
    }

    pub fn get_library_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        self.library_playlists()
    }

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

    pub fn start_vr_device_flow(&self) -> Result<(String, String, String)> {
        self.client.start_vr_device_flow()
    }

    pub fn poll_vr_device_flow(&self, device_code: &str) -> Result<String> {
        self.client.poll_vr_device_flow(device_code)
    }

    pub fn get_home_page(&self, continuation: Option<&str>) -> Result<HomePage> {
        match continuation {
            Some(continuation) => self.client.get_home_page(Some(continuation)),
            None => {
                self.reset_home_pagination();
                self.home_page(0)
            }
        }
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
