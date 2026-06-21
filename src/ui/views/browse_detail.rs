//! Browse detail view: album/playlist/podcast track lists and artist/channel
//! section pages, with back button, summary card, play controls, autoplay.
//!
//! Spec: library.md §5.4–5.8, views.md §6.3–6.4.
//! GTK sources: src/ui/window/library.rs (`activate_browse_target`,
//! `load_browse_detail`, `build_browse_summary_card`,
//! `build_browse_back_button`), src/ui/window/views.rs (`render_artist_view`).
//!
//! Notes:
//! - The back button returns to `App.browse_back_target` (Home, or the exact
//!   cached search incl. scroll); browse→browse preserves the origin.
//! - `should_play_album_as_track` / `browse_collection_context` are pure
//!   helpers pinned by GTK tests (library spec §3) — ported here.
//! - Artist pages split sections into top-songs (grid w/ StartRadio rows) and
//!   remaining carousels.

use iced::widget::{
    Space, button, column, container, mouse_area, row, scrollable, svg, text, tooltip,
};
use iced::{Element, Length, Point, Task};

use crate::ipc::{PlaybackSnapshot, PlaylistContext, PlaylistContextSource};
use crate::models::{
    BrowseSectionsPage, HomeBrowseCard, HomeBrowsePageType, HomeItem, HomeSection, Track,
};

use super::super::app::{self, App, BrowseBackTarget, Message, QueueDragPayload};
use super::super::{
    content_loading, icons, player_bar, queue_panel, sidebar, theme, thumbnails, widgets,
};
use super::search;

/// Track/browse row art edge (GTK `.track-art` 44×44 parity metric).
const ROW_ART_SIZE: f32 = 44.0;
/// Artist/channel hero art edge (GTK `render_artist_view` 180×180).
const HERO_ART_SIZE: f32 = 180.0;
/// Square art inside summary/cover cards (GTK `build_square_cover_art` 150).
const CARD_ART_SIZE: f32 = 150.0;
/// Total cover-card width (GTK `cover-card` 166 = 150 art + padding).
const CARD_WIDTH: f32 = 166.0;
/// Top offset of loading/error status labels (GTK `margin_top(60)`).
const STATUS_TOP_MARGIN: f32 = 60.0;
/// Icon edge inside the Resume / Play All / Shuffle / Download controls.
const CONTROL_ICON_SIZE: f32 = 14.0;

/// What a browse id resolved to (ported `BrowseDetailData`).
pub enum BrowseDetailData {
    Tracks(Vec<Track>),
    Sections(BrowseSectionsPage),
}

pub struct State {
    pub browse_id: Option<String>,
    pub title: String,
    pub subtitle: Option<String>,
    pub thumbnail_url: Option<String>,
    pub page_type: Option<HomeBrowsePageType>,
    pub data: Option<BrowseDetailData>,
    pub loading: bool,
    pub error: Option<String>,
    /// Play the first track once the fetch lands (direct-URL flow).
    pub autoplay_pending: bool,
    /// Drops stale fetches when the user opens another browse page.
    pub request_serial: u64,
    /// Hovered cell `(section, item)` — rows/cards are `mouse_area`s (queue
    /// drag sources), so hover feedback is tracked explicitly.
    pub hovered_item: Option<(usize, usize)>,
    /// Persisted per-playlist resume state exists (probed after every load).
    pub resume_available: bool,
    pub resume_in_flight: bool,
    /// `cache.is_playlist_offline` for the current collection.
    pub playlist_downloaded: bool,
    pub download_in_flight: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            browse_id: None,
            title: String::new(),
            subtitle: None,
            thumbnail_url: None,
            page_type: None,
            data: None,
            loading: false,
            error: None,
            autoplay_pending: false,
            request_serial: 0,
            hovered_item: None,
            resume_available: false,
            resume_in_flight: false,
            playlist_downloaded: false,
            download_in_flight: false,
        }
    }
}

#[derive(Clone)]
pub enum Msg {
    /// Browse fetch finished (tracks or sections), serial-guarded.
    Loaded {
        serial: u64,
        result: Result<LoadedData, String>,
    },
    BackPressed,
    /// Track-list page row clicked: play from the browse context at the index.
    TrackClicked(usize),
    PlayAll {
        shuffle: bool,
    },
    DownloadAll,
    /// A nested browse card was clicked (artist page carousels). Card clicks
    /// currently route through `SectionItemActivated`; kept as the direct
    /// payload-carrying form of the same action.
    #[allow(dead_code)]
    CardClicked(HomeBrowseCard),
    /// Album cards pre-fetch their tracks before navigating so single-track
    /// albums play directly without leaving the page (GTK
    /// `activate_browse_target`).
    AlbumPrecheckLoaded {
        card: HomeBrowseCard,
        tracks: Option<Vec<Track>>,
    },
    /// Artist/channel section item activated: tracks seed radio
    /// (`StartRadio`), browse cards open browse detail.
    SectionItemActivated {
        section: usize,
        item: usize,
    },
    /// Play controls of an all-track artist section (context-less queue).
    SectionPlayAll {
        section: usize,
        shuffle: bool,
    },
    /// Autoplay fallback fetch finished (first playable card's tracks).
    AutoplayFallbackLoaded {
        card: HomeBrowseCard,
        tracks: Option<Vec<Track>>,
    },
    ResumePressed,
    ResumeFinished {
        result: Result<PlaybackSnapshot, String>,
    },
    /// Post-load probe of persisted resume + offline collection state.
    MetaProbed {
        serial: u64,
        resume_available: bool,
        playlist_downloaded: bool,
    },
    ItemHovered {
        section: usize,
        item: usize,
    },
    ItemUnhovered {
        section: usize,
        item: usize,
    },
}

/// Clonable payload for `Msg::Loaded` (BrowseDetailData itself stays
/// non-clonable in state; this is the wire form).
#[derive(Clone)]
pub enum LoadedData {
    Tracks(Vec<Track>),
    Sections(BrowseSectionsPage),
}

pub fn view(app: &App) -> Element<'_, Message> {
    let state = &app.browse;
    if state.loading {
        return status_view(format!("Loading \"{}\"...", state.title));
    }
    if let Some(error) = &state.error {
        return status_view(error.clone());
    }
    match &state.data {
        Some(BrowseDetailData::Tracks(tracks)) => tracks_page(app, tracks),
        Some(BrowseDetailData::Sections(page)) => sections_page(app, page),
        None => status_view(format!("Loading \"{}\"...", state.title)),
    }
}

pub fn update(app: &mut App, msg: Msg) -> Task<Message> {
    match msg {
        Msg::Loaded { serial, result } => handle_loaded(app, serial, result),

        Msg::BackPressed => navigate_back(app),

        Msg::TrackClicked(index) => {
            let press = release_pending_press(app);
            if press.was_active {
                return Task::none(); // it was a drag, not a click
            }
            let Some(BrowseDetailData::Tracks(tracks)) = &app.browse.data else {
                return Task::none();
            };
            let tracks = tracks.clone();
            let Some(track) = tracks.get(index).cloned() else {
                return Task::none();
            };
            if press_mismatches_track(&press, &track) {
                return Task::none(); // release slid in from another row
            }
            let context = current_collection_context(app);
            sidebar::add_to_recent(app, &track);
            queue_panel::play_from_context_task(app, tracks, index, context)
        }

        Msg::PlayAll { shuffle } => {
            let Some(BrowseDetailData::Tracks(tracks)) = &app.browse.data else {
                return Task::none();
            };
            if tracks.is_empty() {
                return Task::none();
            }
            let tracks = tracks.clone();
            let context = current_collection_context(app);
            queue_panel::play_all_task(app, tracks, shuffle, context)
        }

        Msg::DownloadAll => {
            if app.browse.download_in_flight || app.browse.playlist_downloaded {
                return Task::none();
            }
            let Some(BrowseDetailData::Tracks(tracks)) = &app.browse.data else {
                return Task::none();
            };
            let Some(browse_id) = app.browse.browse_id.clone() else {
                return Task::none();
            };
            let tracks = tracks.clone();
            app.browse.download_in_flight = true;
            let meta = queue_panel::DownloadPlaylistMeta {
                playlist_id: browse_id,
                title: app.browse.title.clone(),
                thumbnail_url: app.browse.thumbnail_url.clone(),
                subtitle: Some(format!("{} tracks", tracks.len())),
            };
            queue_panel::download_all_task(app, tracks, Some(meta))
        }

        Msg::CardClicked(card) => {
            let press = release_pending_press(app);
            if press.was_active || press_mismatches_card(&press, &card) {
                return Task::none();
            }
            activate_browse_card(app, card)
        }

        Msg::AlbumPrecheckLoaded { card, tracks } => {
            if let Some(tracks) = &tracks {
                if should_play_album_as_track(card.page_type, tracks.len()) {
                    if let Some(track) = tracks.first().cloned() {
                        sidebar::add_to_recent(app, &track);
                        return queue_panel::play_track_task(
                            app,
                            track.video_id.clone(),
                            track.title.clone(),
                            artist_string(&track),
                            track.thumbnail_url.clone(),
                        );
                    }
                }
            }
            content_loading::load_browse_detail_task(
                app,
                card.browse_id,
                card.title,
                card.thumbnail_url,
                card.page_type,
                false,
            )
        }

        Msg::SectionItemActivated { section, item } => {
            let press = release_pending_press(app);
            if press.was_active {
                return Task::none();
            }
            let Some(BrowseDetailData::Sections(page)) = &app.browse.data else {
                return Task::none();
            };
            let Some(entry) = page
                .sections
                .get(section)
                .and_then(|section| section.contents.get(item))
                .cloned()
            else {
                return Task::none();
            };
            match entry {
                HomeItem::Track(track) => {
                    if press_mismatches_track(&press, &track) {
                        return Task::none();
                    }
                    sidebar::add_to_recent(app, &track);
                    queue_panel::play_track_task(
                        app,
                        track.video_id.clone(),
                        track.title.clone(),
                        artist_string(&track),
                        track.thumbnail_url.clone(),
                    )
                }
                HomeItem::BrowseCard(card) => {
                    if press_mismatches_card(&press, &card) {
                        return Task::none();
                    }
                    activate_browse_card(app, card)
                }
            }
        }

        Msg::SectionPlayAll { section, shuffle } => {
            let Some(BrowseDetailData::Sections(page)) = &app.browse.data else {
                return Task::none();
            };
            let tracks = page
                .sections
                .get(section)
                .map(section_tracks)
                .unwrap_or_default();
            if tracks.is_empty() {
                return Task::none();
            }
            queue_panel::play_all_task(app, tracks, shuffle, None)
        }

        Msg::AutoplayFallbackLoaded { card, tracks } => {
            let Some(tracks) = tracks else {
                return Task::none();
            };
            let Some(first) = tracks.first().cloned() else {
                return Task::none();
            };
            let context = browse_collection_context(
                card.page_type,
                &card.browse_id,
                &card.title,
                card.thumbnail_url.as_deref(),
                card.subtitle.as_deref(),
            );
            sidebar::add_to_recent(app, &first);
            queue_panel::play_from_context_task(app, tracks, 0, context)
        }

        Msg::ResumePressed => {
            if app.browse.resume_in_flight || !app.browse.resume_available {
                return Task::none();
            }
            let Some(context) = current_collection_context(app) else {
                return Task::none();
            };
            app.browse.resume_in_flight = true;
            let daemon = app.player.daemon.clone();
            app::blocking_or_timeout(
                app::DAEMON_TASK_TIMEOUT,
                move || resume_persisted_playlist(&daemon, &context),
                |result| Message::Browse(Msg::ResumeFinished { result }),
                // A stuck restore must still clear `resume_in_flight`.
                Message::Browse(Msg::ResumeFinished {
                    result: Err("daemon resume request timed out".to_owned()),
                }),
            )
        }

        Msg::ResumeFinished { result } => {
            app.browse.resume_in_flight = false;
            match result {
                Ok(snapshot) => player_bar::apply_remote_snapshot(app, snapshot),
                Err(err) => {
                    crate::app_log!("[UI] failed to resume browse playlist: {err}");
                    Task::none()
                }
            }
        }

        Msg::MetaProbed {
            serial,
            resume_available,
            playlist_downloaded,
        } => {
            if serial == app.browse.request_serial {
                app.browse.resume_available = resume_available;
                app.browse.playlist_downloaded = playlist_downloaded;
            }
            Task::none()
        }

        Msg::ItemHovered { section, item } => {
            app.browse.hovered_item = Some((section, item));
            Task::none()
        }
        Msg::ItemUnhovered { section, item } => {
            if app.browse.hovered_item == Some((section, item)) {
                app.browse.hovered_item = None;
            }
            Task::none()
        }
    }
}

// ---------------------------------------------------------------------------
// Load handling
// ---------------------------------------------------------------------------

fn handle_loaded(app: &mut App, serial: u64, result: Result<LoadedData, String>) -> Task<Message> {
    if serial != app.browse.request_serial {
        return Task::none(); // a newer browse page superseded this fetch
    }
    app.browse.loading = false;
    app.browse.hovered_item = None;
    app.browse.resume_available = false;
    app.browse.resume_in_flight = false;
    app.browse.playlist_downloaded = false;
    app.browse.download_in_flight = false;

    match result {
        Err(err) => {
            app.browse.autoplay_pending = false;
            app.browse.error = Some(err);
            Task::none()
        }
        Ok(LoadedData::Tracks(tracks)) => tracks_loaded(app, serial, tracks),
        Ok(LoadedData::Sections(page)) => sections_loaded(app, page),
    }
}

fn tracks_loaded(app: &mut App, serial: u64, tracks: Vec<Track>) -> Task<Message> {
    let autoplay = std::mem::take(&mut app.browse.autoplay_pending);
    let page_type = app.browse.page_type.unwrap_or(HomeBrowsePageType::Playlist);
    let browse_id = app.browse.browse_id.clone().unwrap_or_default();
    let title = app.browse.title.clone();
    let thumbnail_url = app.browse.thumbnail_url.clone();

    // Single-track albums play immediately like a song instead of opening an
    // album page (library spec §5.4). GTK never navigated; the iced loader
    // already did, so play the track and return to the back target. GTK ran
    // this precheck on the autoplay (direct-URL) path too — a pasted MPRE URL
    // resolving to a 1-track album seeds radio instead of playing a 1-track
    // album context.
    if should_play_album_as_track(page_type, tracks.len()) {
        if let Some(track) = tracks.first().cloned() {
            sidebar::add_to_recent(app, &track);
            let play = queue_panel::play_track_task(
                app,
                track.video_id.clone(),
                track.title.clone(),
                artist_string(&track),
                track.thumbnail_url.clone(),
            );
            return Task::batch([play, navigate_back(app)]);
        }
    }

    let subtitle = format!("{} tracks", tracks.len());
    let context = browse_collection_context(
        page_type,
        &browse_id,
        &title,
        thumbnail_url.as_deref(),
        Some(&subtitle),
    );
    app.browse.subtitle = Some(subtitle);

    let mut tasks = Vec::new();
    if autoplay {
        if let Some(first) = tracks.first().cloned() {
            sidebar::add_to_recent(app, &first);
            tasks.push(queue_panel::play_from_context_task(
                app,
                tracks.clone(),
                0,
                context.clone(),
            ));
        }
    }

    let thumb_urls: Vec<String> = tracks
        .iter()
        .filter_map(|track| track.thumbnail_url.clone())
        .collect();
    app.browse.data = Some(BrowseDetailData::Tracks(tracks));
    tasks.push(thumbnails::ensure_all(
        app,
        thumb_urls.iter().map(String::as_str),
    ));
    tasks.push(probe_collection_meta_task(
        app, serial, context, page_type, browse_id,
    ));
    Task::batch(tasks)
}

fn sections_loaded(app: &mut App, page: BrowseSectionsPage) -> Task<Message> {
    let autoplay = std::mem::take(&mut app.browse.autoplay_pending);
    if let Some(title) = page.title.clone() {
        app.browse.title = title;
    }
    if page.thumbnail_url.is_some() {
        app.browse.thumbnail_url = page.thumbnail_url.clone();
    }

    let mut tasks = Vec::new();
    if autoplay && !page.sections.is_empty() {
        let flat_tracks = flatten_section_tracks(&page.sections);
        if let Some(first) = flat_tracks.first().cloned() {
            sidebar::add_to_recent(app, &first);
            tasks.push(queue_panel::play_from_context_task(
                app,
                flat_tracks,
                0,
                None,
            ));
        } else if let Some(card) = first_playable_browse_card(&page.sections) {
            tasks.push(autoplay_fallback_task(app, card));
        }
    }

    let mut thumb_urls: Vec<String> = page
        .sections
        .iter()
        .flat_map(|section| section.contents.iter())
        .filter_map(|item| match item {
            HomeItem::Track(track) => track.thumbnail_url.clone(),
            HomeItem::BrowseCard(card) => card.thumbnail_url.clone(),
        })
        .collect();
    if let Some(url) = app.browse.thumbnail_url.clone() {
        thumb_urls.push(url);
    }
    app.browse.data = Some(BrowseDetailData::Sections(page));
    tasks.push(thumbnails::ensure_all(
        app,
        thumb_urls.iter().map(String::as_str),
    ));
    Task::batch(tasks)
}

/// Post-load disk probe: per-playlist persisted resume state + offline
/// collection state (GTK checked both synchronously while building the page).
fn probe_collection_meta_task(
    app: &App,
    serial: u64,
    context: Option<PlaylistContext>,
    page_type: HomeBrowsePageType,
    browse_id: String,
) -> Task<Message> {
    let downloadable = matches!(
        page_type,
        HomeBrowsePageType::Playlist | HomeBrowsePageType::Album
    );
    if context.is_none() && !downloadable {
        return Task::none();
    }
    let cache = app.cache.clone();
    app::blocking(
        move || {
            let resume_available = context.as_ref().is_some_and(|context| {
                match crate::ipc::load_persisted_playlist_state(context) {
                    Ok(Some(persisted)) => !persisted.snapshot.queue.is_empty(),
                    Ok(None) => false,
                    Err(err) => {
                        crate::app_log!(
                            "[UI] failed to load persisted playlist state for {}: {err}",
                            context.playlist_id
                        );
                        false
                    }
                }
            });
            let playlist_downloaded = downloadable
                && cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_playlist_offline(&browse_id);
            (resume_available, playlist_downloaded)
        },
        move |(resume_available, playlist_downloaded)| {
            Message::Browse(Msg::MetaProbed {
                serial,
                resume_available,
                playlist_downloaded,
            })
        },
    )
}

/// Direct-URL autoplay fallback (library spec §5.5.a): an artist page with no
/// flat tracks plays its first playable card's tracks instead.
fn autoplay_fallback_task(app: &App, card: HomeBrowseCard) -> Task<Message> {
    let client = app.app_client();
    let fetch_card = card.clone();
    app::blocking(
        move || {
            let client = client?;
            match fetch_card.page_type {
                HomeBrowsePageType::Playlist
                | HomeBrowsePageType::Album
                | HomeBrowsePageType::PodcastShow => {
                    client.get_browse_tracks(&fetch_card.browse_id).ok()
                }
                HomeBrowsePageType::Artist | HomeBrowsePageType::UserChannel => client
                    .get_channel_page(&fetch_card.browse_id)
                    .ok()
                    .map(|page| flatten_section_tracks(&page.sections)),
            }
        },
        move |tracks| {
            Message::Browse(Msg::AutoplayFallbackLoaded {
                card: card.clone(),
                tracks,
            })
        },
    )
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

/// GTK `activate_browse_target`: Album cards pre-fetch their tracks without
/// any loading UI so single-track albums play directly (no navigation, no
/// back-target mutation); everything else opens browse detail immediately.
fn activate_browse_card(app: &mut App, card: HomeBrowseCard) -> Task<Message> {
    if card.page_type != HomeBrowsePageType::Album {
        return content_loading::load_browse_detail_task(
            app,
            card.browse_id,
            card.title,
            card.thumbnail_url,
            card.page_type,
            false,
        );
    }
    let client = app.app_client();
    let browse_id = card.browse_id.clone();
    app::blocking(
        move || client.and_then(|ytm| ytm.get_browse_tracks(&browse_id).ok()),
        move |tracks| {
            Message::Browse(Msg::AlbumPrecheckLoaded {
                card: card.clone(),
                tracks,
            })
        },
    )
}

/// The back button's action: re-render the cached search (incl. scroll) or
/// go Home (library spec §5.7).
fn navigate_back(app: &mut App) -> Task<Message> {
    match app.browse_back_target.clone() {
        BrowseBackTarget::Search(cache) => search::show_cached_page(app, cache),
        BrowseBackTarget::Home => content_loading::load_home_task(app, false),
    }
}

// ---------------------------------------------------------------------------
// Resume (library spec §6.1 item 1)
// ---------------------------------------------------------------------------

/// Reload the persisted snapshot and replay it through the daemon: volume →
/// `play_queue_at` → seek → pause/resume (GTK
/// `restore_playback_snapshot_via_daemon(snapshot, true)`).
fn resume_persisted_playlist(
    daemon: &crate::ipc::DaemonClient,
    context: &PlaylistContext,
) -> Result<PlaybackSnapshot, String> {
    let persisted = crate::ipc::load_persisted_playlist_state(context)
        .map_err(|err| err.to_string())?
        .filter(|persisted| !persisted.snapshot.queue.is_empty())
        .ok_or_else(|| "No saved state for this playlist".to_owned())?;
    let snapshot = persisted.snapshot;

    crate::ipc::ensure_daemon_running().map_err(|err| err.to_string())?;
    let queue_index = snapshot
        .queue_index
        .min(snapshot.queue.len().saturating_sub(1));

    let mut restored = daemon
        .set_volume(snapshot.volume)
        .map_err(|err| err.to_string())?;
    if snapshot.queue.is_empty() {
        return Ok(restored);
    }

    restored = daemon
        .play_queue_at(
            snapshot.queue.clone(),
            queue_index,
            snapshot.playlist_context.clone(),
            snapshot.radio_virtual_playlist.clone(),
            snapshot.repeat_mode,
        )
        .map_err(|err| err.to_string())?;

    if let Some(position_millis) = snapshot.position_millis.filter(|position| *position > 0) {
        restored = daemon
            .seek_to_millis(position_millis)
            .map_err(|err| err.to_string())?;
    }
    if snapshot.paused {
        restored = daemon.pause().map_err(|err| err.to_string())?;
    } else if restored.paused {
        restored = daemon.resume().map_err(|err| err.to_string())?;
    }
    Ok(restored)
}

// ---------------------------------------------------------------------------
// Click plumbing (mouse_area rows deliver clicks on release; CONTRACT.md §7.2)
// ---------------------------------------------------------------------------

struct ReleasedPress {
    payload: Option<QueueDragPayload>,
    was_active: bool,
}

/// Rows/cards register a pending queue drag on press; a release before the
/// drag activated is the normal click. The pending drag state is cleared
/// either way because the queue panel never sees releases over the content
/// area.
fn release_pending_press(app: &mut App) -> ReleasedPress {
    let drag = app.queue_panel.drag.take();
    app.queue_panel.drop_preview_slot = None;
    app.queue_panel.drag_hover_y = None;
    app.queue_panel.drop_active = false;
    match drag {
        Some(drag) => ReleasedPress {
            was_active: drag.active,
            payload: Some(drag.payload),
        },
        None => ReleasedPress {
            payload: None,
            was_active: false,
        },
    }
}

fn press_mismatches_track(press: &ReleasedPress, track: &Track) -> bool {
    match &press.payload {
        Some(QueueDragPayload::Track { track: pressed }) => pressed.video_id != track.video_id,
        None => false,
        Some(_) => true,
    }
}

fn press_mismatches_card(press: &ReleasedPress, card: &HomeBrowseCard) -> bool {
    match &press.payload {
        Some(QueueDragPayload::BrowseCollection { browse_id, .. }) => *browse_id != card.browse_id,
        None => false,
        Some(_) => true,
    }
}

// ---------------------------------------------------------------------------
// View composition
// ---------------------------------------------------------------------------

fn status_view(message: String) -> Element<'static, Message> {
    container(text(message).size(theme::SIZE_BODY).color(theme::TEXT_DIM))
        .width(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center)
        .padding(iced::Padding {
            top: STATUS_TOP_MARGIN,
            ..iced::Padding::ZERO
        })
        .into()
}

fn empty_label(message: &'static str) -> Element<'static, Message> {
    container(text(message).size(theme::SIZE_BODY).color(theme::TEXT_DIM))
        .width(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center)
        .padding(iced::Padding {
            top: theme::SPACE_XXL,
            ..iced::Padding::ZERO
        })
        .into()
}

/// "← Back to Search" / "← Back to Home" honoring `App.browse_back_target`.
fn back_button(app: &App) -> Element<'_, Message> {
    let label = match &app.browse_back_target {
        BrowseBackTarget::Search(_) => "\u{2190} Back to Search",
        BrowseBackTarget::Home => "\u{2190} Back to Home",
    };
    button(text(label).size(theme::SIZE_BODY))
        .style(theme::nav_item(false))
        .padding([theme::SPACE_SM, theme::SPACE_LG])
        .on_press(Message::Browse(Msg::BackPressed))
        .into()
}

/// Album / playlist / podcast-show track-list page (views spec §6.4).
fn tracks_page<'a>(app: &'a App, tracks: &'a [Track]) -> Element<'a, Message> {
    let state = &app.browse;
    let mut page = column![back_button(app), widgets::section_header(&state.title)]
        .spacing(theme::SPACE_LG)
        .width(Length::Fill);

    if tracks.is_empty() {
        return page
            .push(empty_label("No playable tracks found for this page."))
            .into();
    }

    page = page.push(
        text(format!("{} tracks", tracks.len()))
            .size(theme::SIZE_BODY)
            .color(theme::TEXT_DIM),
    );
    page = page.push(play_controls(state));

    let cells = tracks
        .iter()
        .enumerate()
        .map(|(index, track)| track_row(app, (0, index), track, Msg::TrackClicked(index)))
        .collect();
    page.push(two_col_grid(cells)).into()
}

/// Resume (when persisted) / Play All / Shuffle / Download All controls
/// (library spec §6.1). Download appears only for Playlist/Album pages.
fn play_controls(state: &State) -> Element<'static, Message> {
    let mut controls = row![]
        .spacing(theme::SPACE_MD)
        .align_y(iced::Alignment::Center);

    if state.resume_available {
        controls = controls.push(control_button(
            icons::play(),
            "Resume",
            (!state.resume_in_flight).then_some(Msg::ResumePressed),
        ));
    }
    controls = controls.push(control_button(
        icons::play(),
        "Play All",
        Some(Msg::PlayAll { shuffle: false }),
    ));
    controls = controls.push(control_button(
        icons::shuffle(),
        "Shuffle",
        Some(Msg::PlayAll { shuffle: true }),
    ));

    if matches!(
        state.page_type,
        Some(HomeBrowsePageType::Playlist | HomeBrowsePageType::Album)
    ) {
        let (label, msg) = if state.playlist_downloaded {
            ("Downloaded", None)
        } else if state.download_in_flight {
            ("Download All", None)
        } else {
            ("Download All", Some(Msg::DownloadAll))
        };
        controls = controls.push(control_button(icons::download(), label, msg));
    }

    controls.into()
}

fn artist_play_controls(section_index: usize) -> Element<'static, Message> {
    row![
        control_button(
            icons::play(),
            "Play All",
            Some(Msg::SectionPlayAll {
                section: section_index,
                shuffle: false,
            }),
        ),
        control_button(
            icons::shuffle(),
            "Shuffle",
            Some(Msg::SectionPlayAll {
                section: section_index,
                shuffle: true,
            }),
        ),
    ]
    .spacing(theme::SPACE_MD)
    .into()
}

/// `on_press: None` renders the button disabled (Resume in flight,
/// Downloaded / download in flight).
fn control_button(
    icon: svg::Handle,
    label: &'static str,
    msg: Option<Msg>,
) -> Element<'static, Message> {
    let mut btn = button(
        row![
            widgets::icon(icon, CONTROL_ICON_SIZE),
            text(label).size(theme::SIZE_BODY),
        ]
        .spacing(theme::SPACE_SM)
        .align_y(iced::Alignment::Center),
    )
    .style(theme::secondary_button)
    .padding([theme::SPACE_SM, theme::SPACE_LG]);
    if let Some(msg) = msg {
        btn = btn.on_press(Message::Browse(msg));
    }
    btn.into()
}

/// Artist / channel sections page (views spec §6.3).
fn sections_page<'a>(app: &'a App, page: &'a BrowseSectionsPage) -> Element<'a, Message> {
    let state = &app.browse;
    let mut body = column![back_button(app)]
        .spacing(theme::SPACE_LG)
        .width(Length::Fill);

    if page.sections.is_empty() {
        return body
            .push(empty_label("No sections found for this channel."))
            .into();
    }

    let sections = split_artist_sections(&page.sections);
    let use_artist_chrome = state.page_type == Some(HomeBrowsePageType::Artist)
        || page.artist_like
        || is_artist_sections(&page.sections);
    let eyebrow = if use_artist_chrome {
        "Artist"
    } else {
        "Channel"
    };

    body = body.push(hero(app, eyebrow, &sections));

    if let Some((section_index, top_songs)) = sections.top_songs {
        body = body.push(widgets::separator());
        body = body.push(widgets::section_header(&top_songs.title));
        body = body.push(section_track_grid(app, section_index, top_songs));
    }

    for (section_index, section) in &sections.remaining {
        body = body.push(widgets::separator());
        body = body.push(widgets::section_header(&section.title));

        let track_count = section
            .contents
            .iter()
            .filter(|item| matches!(item, HomeItem::Track(_)))
            .count();
        let card_count = section.contents.len() - track_count;

        if track_count == section.contents.len() && track_count > 0 {
            body = body.push(artist_play_controls(*section_index));
            body = body.push(section_track_grid(app, *section_index, section));
        } else if card_count == section.contents.len() && card_count > 0 {
            body = body.push(section_browse_grid(app, *section_index, section));
        } else {
            body = body.push(section_carousel(app, *section_index, section));
        }
    }

    body.into()
}

/// Hero: 180×180 art (letter fallback), eyebrow, name, "{k} top songs  •
/// {m} featured sections" summary, top-songs play controls.
fn hero<'a>(
    app: &'a App,
    eyebrow: &'static str,
    sections: &ArtistSections<'a>,
) -> Element<'a, Message> {
    let state = &app.browse;
    let art = thumbnails::view_thumb(
        app,
        state.thumbnail_url.as_deref(),
        HERO_ART_SIZE,
        state.title.chars().next(),
    );

    let summary = match sections.top_songs {
        Some((_, top_songs)) => format!(
            "{} top songs  \u{2022}  {} featured sections",
            section_tracks(top_songs).len(),
            sections.remaining.len()
        ),
        None => format!("{} featured sections", sections.remaining.len()),
    };

    let mut meta = column![
        text(eyebrow)
            .size(theme::SIZE_CAPTION)
            .color(theme::TEXT_DIM),
        text(&state.title)
            .size(theme::SIZE_HEADER)
            .color(theme::TEXT_HEADING),
        text(summary).size(theme::SIZE_BODY).color(theme::TEXT_DIM),
    ]
    .spacing(theme::SPACE_MD)
    .width(Length::Fill);

    if let Some((section_index, top_songs)) = sections.top_songs {
        if !section_tracks(top_songs).is_empty() {
            meta = meta.push(artist_play_controls(section_index));
        }
    }

    row![art, meta]
        .spacing(theme::SPACE_XXL)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill)
        .into()
}

/// 2-column grid of `StartRadio` track rows (top songs / all-track sections).
fn section_track_grid<'a>(
    app: &'a App,
    section_index: usize,
    section: &'a HomeSection,
) -> Element<'a, Message> {
    let cells = section
        .contents
        .iter()
        .enumerate()
        .filter_map(|(item_index, item)| match item {
            HomeItem::Track(track) => Some(track_row(
                app,
                (section_index, item_index),
                track,
                Msg::SectionItemActivated {
                    section: section_index,
                    item: item_index,
                },
            )),
            HomeItem::BrowseCard(_) => None,
        })
        .collect();
    two_col_grid(cells)
}

/// 2-column grid of browse rows (all-card sections, views spec §7.4).
fn section_browse_grid<'a>(
    app: &'a App,
    section_index: usize,
    section: &'a HomeSection,
) -> Element<'a, Message> {
    let cells = section
        .contents
        .iter()
        .enumerate()
        .filter_map(|(item_index, item)| match item {
            HomeItem::BrowseCard(card) => Some(browse_row(app, (section_index, item_index), card)),
            HomeItem::Track(_) => None,
        })
        .collect();
    two_col_grid(cells)
}

/// Horizontal cover-card carousel (mixed sections).
fn section_carousel<'a>(
    app: &'a App,
    section_index: usize,
    section: &'a HomeSection,
) -> Element<'a, Message> {
    let mut cards = row![]
        .spacing(theme::SPACE_XL)
        .align_y(iced::Alignment::Start);
    for (item_index, item) in section.contents.iter().enumerate() {
        cards = cards.push(match item {
            HomeItem::Track(track) => track_card(app, (section_index, item_index), track),
            HomeItem::BrowseCard(card) => browse_card(app, (section_index, item_index), card),
        });
    }

    scrollable(container(cards).padding(iced::Padding {
        bottom: theme::SPACE_LG,
        ..iced::Padding::ZERO
    }))
    .direction(scrollable::Direction::Horizontal(
        scrollable::Scrollbar::new()
            .width(theme::SPACE_XS)
            .scroller_width(theme::SPACE_XS)
            .spacing(theme::SPACE_XS),
    ))
    .style(theme::thin_scrollbar)
    .width(Length::Fill)
    .into()
}

/// Track row: 44 px art, truncated title with tooltip (marquee stand-in),
/// ellipsized artist, optional duration. Queue drag source; click rides on
/// release; right-click opens the shared track menu.
fn track_row<'a>(
    app: &'a App,
    key: (usize, usize),
    track: &'a Track,
    click: Msg,
) -> Element<'a, Message> {
    let hovered = app.browse.hovered_item == Some(key);
    let art = thumbnails::view_thumb(
        app,
        track.thumbnail_url.as_deref(),
        ROW_ART_SIZE,
        track.title.chars().next(),
    );
    let info = column![
        clipped_title(&track.title),
        clipped_caption(artist_string(track)),
    ]
    .spacing(theme::SPACE_XXS)
    .width(Length::Fill);
    let mut content = row![art, info]
        .spacing(theme::SPACE_LG)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill);
    if let Some(duration) = &track.duration {
        content = content.push(
            text(duration)
                .font(iced::Font::MONOSPACE)
                .size(theme::SIZE_CAPTION)
                .color(theme::TEXT_DIM),
        );
    }

    mouse_area(
        container(content)
            .padding([theme::SPACE_SM, theme::SPACE_MD])
            .width(Length::Fill)
            .style(theme::row_surface(hovered)),
    )
    .interaction(iced::mouse::Interaction::Pointer)
    .on_press(Message::Queue(queue_panel::Msg::DragStarted {
        payload: QueueDragPayload::Track {
            track: track.clone(),
        },
        position: Point::ORIGIN,
    }))
    .on_release(Message::Browse(click))
    .on_right_press(Message::TrackMenu(queue_panel::MenuMsg::Open {
        track: track.clone(),
        queue_index: None,
        anchor: Point::ORIGIN,
    }))
    .on_enter(Message::Browse(Msg::ItemHovered {
        section: key.0,
        item: key.1,
    }))
    .on_exit(Message::Browse(Msg::ItemUnhovered {
        section: key.0,
        item: key.1,
    }))
    .into()
}

/// Browse row: track-row look-alike with the page-type fallback subtitle, no
/// duration and no context menu. Collection rows are queue drag sources;
/// artist/channel rows are not draggable.
fn browse_row<'a>(
    app: &'a App,
    key: (usize, usize),
    card: &'a HomeBrowseCard,
) -> Element<'a, Message> {
    let hovered = app.browse.hovered_item == Some(key);
    let subtitle = card
        .subtitle
        .clone()
        .filter(|subtitle| !subtitle.trim().is_empty())
        .unwrap_or_else(|| browse_fallback_subtitle(card.page_type).to_owned());
    let art = thumbnails::view_thumb(
        app,
        card.thumbnail_url.as_deref(),
        ROW_ART_SIZE,
        card.title.chars().next(),
    );
    let content = row![
        art,
        column![clipped_title(&card.title), clipped_caption(subtitle)]
            .spacing(theme::SPACE_XXS)
            .width(Length::Fill),
    ]
    .spacing(theme::SPACE_LG)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill);

    let mut area = mouse_area(
        container(content)
            .padding([theme::SPACE_SM, theme::SPACE_MD])
            .width(Length::Fill)
            .style(theme::row_surface(hovered)),
    )
    .interaction(iced::mouse::Interaction::Pointer)
    .on_release(Message::Browse(Msg::SectionItemActivated {
        section: key.0,
        item: key.1,
    }))
    .on_enter(Message::Browse(Msg::ItemHovered {
        section: key.0,
        item: key.1,
    }))
    .on_exit(Message::Browse(Msg::ItemUnhovered {
        section: key.0,
        item: key.1,
    }));
    if let Some(payload) = drag_payload_for_browse_card(card) {
        area = area.on_press(Message::Queue(queue_panel::Msg::DragStarted {
            payload,
            position: Point::ORIGIN,
        }));
    }
    area.into()
}

/// Cover card for a Track item in a carousel: left-click seeds radio,
/// right-click opens the track menu, drag inserts into the queue.
fn track_card<'a>(app: &'a App, key: (usize, usize), track: &'a Track) -> Element<'a, Message> {
    let hovered = app.browse.hovered_item == Some(key);
    let content = summary_card_content(
        app,
        &track.title,
        Some(artist_string(track)),
        track.thumbnail_url.as_deref(),
    );

    mouse_area(
        container(content)
            .padding(theme::SPACE_MD)
            .width(Length::Fixed(CARD_WIDTH))
            .style(theme::card_surface(hovered)),
    )
    .interaction(iced::mouse::Interaction::Pointer)
    .on_press(Message::Queue(queue_panel::Msg::DragStarted {
        payload: QueueDragPayload::Track {
            track: track.clone(),
        },
        position: Point::ORIGIN,
    }))
    .on_release(Message::Browse(Msg::SectionItemActivated {
        section: key.0,
        item: key.1,
    }))
    .on_right_press(Message::TrackMenu(queue_panel::MenuMsg::Open {
        track: track.clone(),
        queue_index: None,
        anchor: Point::ORIGIN,
    }))
    .on_enter(Message::Browse(Msg::ItemHovered {
        section: key.0,
        item: key.1,
    }))
    .on_exit(Message::Browse(Msg::ItemUnhovered {
        section: key.0,
        item: key.1,
    }))
    .into()
}

/// Generic browse summary card (GTK `build_browse_summary_card`): 150 px art,
/// truncated title (tooltip), optional subtitle. Collection cards are queue
/// drag sources; artist/channel cards are not.
fn browse_card<'a>(
    app: &'a App,
    key: (usize, usize),
    card: &'a HomeBrowseCard,
) -> Element<'a, Message> {
    let hovered = app.browse.hovered_item == Some(key);
    let content = summary_card_content(
        app,
        &card.title,
        card.subtitle.clone().filter(|s| !s.trim().is_empty()),
        card.thumbnail_url.as_deref(),
    );

    let mut area = mouse_area(
        container(content)
            .padding(theme::SPACE_MD)
            .width(Length::Fixed(CARD_WIDTH))
            .style(theme::card_surface(hovered)),
    )
    .interaction(iced::mouse::Interaction::Pointer)
    .on_release(Message::Browse(Msg::SectionItemActivated {
        section: key.0,
        item: key.1,
    }))
    .on_enter(Message::Browse(Msg::ItemHovered {
        section: key.0,
        item: key.1,
    }))
    .on_exit(Message::Browse(Msg::ItemUnhovered {
        section: key.0,
        item: key.1,
    }));
    if let Some(payload) = drag_payload_for_browse_card(card) {
        area = area.on_press(Message::Queue(queue_panel::Msg::DragStarted {
            payload,
            position: Point::ORIGIN,
        }));
    }
    area.into()
}

fn summary_card_content<'a>(
    app: &'a App,
    title: &'a str,
    subtitle: Option<String>,
    thumbnail_url: Option<&'a str>,
) -> Element<'a, Message> {
    let art = thumbnails::view_thumb(app, thumbnail_url, CARD_ART_SIZE, title.chars().next());
    let mut card = column![art, card_title(title)]
        .spacing(theme::SPACE_SM)
        .width(Length::Fixed(CARD_ART_SIZE));
    if let Some(subtitle) = subtitle {
        card = card.push(
            container(
                text(subtitle)
                    .size(theme::SIZE_CAPTION)
                    .color(theme::TEXT_DIM)
                    .wrapping(text::Wrapping::None),
            )
            .width(Length::Fixed(CARD_ART_SIZE))
            .clip(true),
        );
    }
    card.into()
}

fn card_title(title: &str) -> Element<'static, Message> {
    let label = container(
        text(title.to_owned())
            .size(theme::SIZE_BODY)
            .font(theme::FONT_SEMIBOLD)
            .color(theme::TEXT_BRIGHT)
            .wrapping(text::Wrapping::None),
    )
    .width(Length::Fixed(CARD_ART_SIZE))
    .clip(true);
    tooltip(
        label,
        container(text(title.to_owned()).size(theme::SIZE_SUB))
            .style(theme::popover)
            .padding([theme::SPACE_XS, theme::SPACE_MD]),
        tooltip::Position::Top,
    )
    .into()
}

/// Homogeneous 2-column grid: item `i` lands at column `i % 2`, row `i / 2`
/// (GTK `gtk::Grid` parity); a trailing odd cell keeps its half width.
fn two_col_grid(cells: Vec<Element<'_, Message>>) -> Element<'_, Message> {
    let mut grid = column![].spacing(theme::SPACE_MD).width(Length::Fill);
    let mut cells = cells.into_iter();
    while let Some(left) = cells.next() {
        let right: Element<'_, Message> = match cells.next() {
            Some(cell) => cell,
            None => Space::new().width(Length::Fill).into(),
        };
        grid = grid.push(
            row![
                container(left).width(Length::FillPortion(1)),
                container(right).width(Length::FillPortion(1)),
            ]
            .spacing(theme::SPACE_LG)
            .width(Length::Fill),
        );
    }
    grid.into()
}

/// Single-line truncated title with the full text in a tooltip (the
/// contract's stand-in for the GTK hover marquee).
fn clipped_title(title: &str) -> Element<'static, Message> {
    let label = container(
        text(title.to_owned())
            .size(theme::SIZE_BODY)
            .font(theme::FONT_SEMIBOLD)
            .color(theme::TEXT_BRIGHT)
            .wrapping(text::Wrapping::None),
    )
    .width(Length::Fill)
    .clip(true);
    tooltip(
        label,
        container(text(title.to_owned()).size(theme::SIZE_SUB))
            .style(theme::popover)
            .padding([theme::SPACE_XS, theme::SPACE_MD]),
        tooltip::Position::Top,
    )
    .into()
}

fn clipped_caption(label: String) -> Element<'static, Message> {
    container(
        text(label)
            .size(theme::SIZE_CAPTION)
            .color(theme::TEXT_DIM)
            .wrapping(text::Wrapping::None),
    )
    .width(Length::Fill)
    .clip(true)
    .into()
}

// ---------------------------------------------------------------------------
// Pure helpers (ported from GTK; library spec §3, views spec §6.3)
// ---------------------------------------------------------------------------

/// Single-track albums opened from cards play as a bare track (radio seed).
pub fn should_play_album_as_track(page_type: HomeBrowsePageType, track_count: usize) -> bool {
    matches!(page_type, HomeBrowsePageType::Album) && track_count == 1
}

/// Browse collections that behave like playlists get a `PlaylistContext`.
pub fn browse_collection_context(
    page_type: HomeBrowsePageType,
    browse_id: &str,
    title: &str,
    thumbnail_url: Option<&str>,
    subtitle: Option<&str>,
) -> Option<PlaylistContext> {
    matches!(
        page_type,
        HomeBrowsePageType::Playlist | HomeBrowsePageType::Album
    )
    .then(|| PlaylistContext {
        playlist_id: browse_id.to_owned(),
        title: title.to_owned(),
        thumbnail_url: thumbnail_url.map(str::to_owned),
        subtitle: subtitle.map(str::to_owned),
        source: PlaylistContextSource::Browse,
    })
}

/// The current page's collection context (Playlist/Album only).
fn current_collection_context(app: &App) -> Option<PlaylistContext> {
    let state = &app.browse;
    let page_type = state.page_type?;
    let browse_id = state.browse_id.as_deref()?;
    browse_collection_context(
        page_type,
        browse_id,
        &state.title,
        state.thumbnail_url.as_deref(),
        state.subtitle.as_deref(),
    )
}

struct ArtistSections<'a> {
    /// The first section titled exactly "Top songs", with its original index.
    top_songs: Option<(usize, &'a HomeSection)>,
    /// Everything else in order (including later "Top songs" duplicates).
    remaining: Vec<(usize, &'a HomeSection)>,
}

fn split_artist_sections(sections: &[HomeSection]) -> ArtistSections<'_> {
    let mut top_songs = None;
    let mut remaining = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        if top_songs.is_none() && section.title == "Top songs" {
            top_songs = Some((index, section));
        } else {
            remaining.push((index, section));
        }
    }
    ArtistSections {
        top_songs,
        remaining,
    }
}

fn is_artist_sections(sections: &[HomeSection]) -> bool {
    sections.iter().any(|section| {
        matches!(
            section.title.as_str(),
            "Top songs" | "Albums" | "Singles & EPs" | "Featured on" | "Fans might also like"
        )
    })
}

fn section_tracks(section: &HomeSection) -> Vec<Track> {
    section
        .contents
        .iter()
        .filter_map(|item| match item {
            HomeItem::Track(track) => Some(track.clone()),
            HomeItem::BrowseCard(_) => None,
        })
        .collect()
}

fn flatten_section_tracks(sections: &[HomeSection]) -> Vec<Track> {
    sections.iter().flat_map(section_tracks).collect()
}

fn first_playable_browse_card(sections: &[HomeSection]) -> Option<HomeBrowseCard> {
    sections.iter().find_map(|section| {
        section.contents.iter().find_map(|item| match item {
            HomeItem::BrowseCard(card) => Some(card.clone()),
            HomeItem::Track(_) => None,
        })
    })
}

fn browse_fallback_subtitle(page_type: HomeBrowsePageType) -> &'static str {
    match page_type {
        HomeBrowsePageType::Album => "Album",
        HomeBrowsePageType::Playlist => "Playlist",
        HomeBrowsePageType::Artist => "Artist",
        HomeBrowsePageType::UserChannel => "Channel",
        HomeBrowsePageType::PodcastShow => "Show",
    }
}

/// Artist/UserChannel cards are not draggable (GTK
/// `queue_drag_payload_for_browse_card`).
fn drag_payload_for_browse_card(card: &HomeBrowseCard) -> Option<QueueDragPayload> {
    match card.page_type {
        HomeBrowsePageType::Album
        | HomeBrowsePageType::Playlist
        | HomeBrowsePageType::PodcastShow => Some(QueueDragPayload::BrowseCollection {
            browse_id: card.browse_id.clone(),
            title: card.title.clone(),
            thumbnail_url: card.thumbnail_url.clone(),
            subtitle: card.subtitle.clone(),
            page_type: card.page_type,
        }),
        HomeBrowsePageType::Artist | HomeBrowsePageType::UserChannel => None,
    }
}

fn artist_string(track: &Track) -> String {
    if track.artists.is_empty() {
        "Unknown artist".to_owned()
    } else {
        track
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
#[path = "../../tests/ui_browse_detail.rs"]
mod tests;
