mod cover_art_network;
pub mod media_fetch_network;
pub mod session_network;
pub mod sponsorblock_network;
pub mod tracking_network;
pub mod vr_auth_network;
pub mod ytm_api_network;

pub(crate) use cover_art_network::{MAX_COVER_BYTES, fetch_cover_art_bytes};

pub use media_fetch_network::{
    download_media_bytes, fetch_thumbnail_bytes, stream_media_to_writer,
};
pub use session_network::{
    delete_cookie, delete_storage_root, get_cookie, get_storage_root, get_vr_refresh_token,
    load_cookies_from_chromium_profiles, store_cookie, store_storage_root, store_vr_refresh_token,
};
pub use sponsorblock_network::{
    SPONSORBLOCK_CATEGORY_OPTIONS, SponsorBlockClient, SponsorSegment, default_skip_categories,
    filter_segments_for_categories, normalize_segments, sanitize_categories,
};
pub use tracking_network::TrackingInfo;
pub(crate) use ytm_api_network::YtMusicClient;
pub use ytm_api_network::{SongRating, extract_video_id_from_input};

#[cfg(test)]
#[path = "tests/network_architecture_guard.rs"]
mod architecture_guard;
