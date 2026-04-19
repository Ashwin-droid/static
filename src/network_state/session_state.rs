use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};

use crate::network::{YtMusicClient, get_cookie, get_vr_refresh_token};

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

    pub fn start_vr_device_flow(&self) -> Result<(String, String, String)> {
        self.client.start_vr_device_flow()
    }

    pub fn poll_vr_device_flow(&self, device_code: &str) -> Result<String> {
        self.client.poll_vr_device_flow(device_code)
    }
}
