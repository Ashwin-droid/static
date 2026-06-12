//! Content views rendered inside the main scrollable area.
//!
//! The dispatcher below is ARCHITECT-OWNED; each view module is an
//! implementation area (see the migration contract).

pub mod browse_detail;
pub mod home;
pub mod library;
pub mod search;
pub mod settings;

use iced::Element;

use super::app::{App, ContentView, Message};

/// Render the current content view (the GTK `content_box` rebuild-in-place
/// becomes a plain match over `App.content_view`).
pub fn view(app: &App) -> Element<'_, Message> {
    match app.content_view {
        ContentView::Home => home::view(app),
        ContentView::Search => search::view(app),
        ContentView::BrowseDetail => browse_detail::view(app),
        ContentView::Likes
        | ContentView::Playlists
        | ContentView::PlaylistDetail
        | ContentView::Offline
        | ContentView::OfflinePlaylistDetail => library::view(app),
        ContentView::Settings => settings::view(app),
    }
}
