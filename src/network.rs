pub mod media_fetch_network;
pub mod session_network;
pub mod sponsorblock_network;
pub mod tracking_network;
pub mod vr_auth_network;
pub mod ytm_api_network;

pub use media_fetch_network::{download_media_bytes, fetch_thumbnail_bytes};
pub use session_network::{
    SecretStoreError, delete_cookie, delete_storage_root, get_cookie, get_storage_root,
    get_vr_refresh_token, load_cookie_from_chromium_profile, store_cookie, store_storage_root,
    store_vr_refresh_token,
};
pub use sponsorblock_network::{
    SPONSORBLOCK_CATEGORY_OPTIONS, SponsorBlockClient, SponsorSegment, default_skip_categories,
    filter_segments_for_categories, normalize_segments, sanitize_categories,
};
pub use tracking_network::TrackingInfo;
pub use ytm_api_network::SongRating;
pub(crate) use ytm_api_network::YtMusicClient;

#[cfg(test)]
mod architecture_guard;
