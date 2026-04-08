pub mod browse_parser;
pub mod home_parser;
pub mod lyrics_parser;
pub mod radio_parser;
pub mod search_parser;
pub mod shared_parser;

pub(crate) use browse_parser::{
    parse_browse_tracks, parse_channel_page, parse_library_playlists, parse_playlist_tracks,
};
pub(crate) use home_parser::{parse_home, parse_home_page};
pub(crate) use radio_parser::parse_radio;
pub(crate) use search_parser::{parse_search_results, parse_search_suggestions};
