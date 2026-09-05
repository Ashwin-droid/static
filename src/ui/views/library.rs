//! Library views: Liked Songs, Your Playlists, playlist detail, the Offline
//! page (playlists + cached tracks), offline playlist detail.
//!
//! Spec: library.md §5.1–5.3 + §5.9–5.10 + §6,
//! views.md §6.5–6.9, content_loading.md §7.
//! GTK sources: src/ui/window/library.rs, src/ui/window/content_loading.rs
//! (offline parts), src/ui/window/views.rs (track rows / play controls).
//!
//! Notes:
//! - Likes/playlist-detail rows play from context (`PlayFromContext`) with the
//!   appropriate `PlaylistContext` (likes use the special liked-songs id).
//! - Offline rows prefer cached thumbs (`offline_thumbnail_url`) and play
//!   offline-first; the listen-again snapshot renders under
//!   `app::OFFLINE_LISTEN_AGAIN_PLAYLIST_ID`.
//! - Play-controls row: Resume (when a per-playlist persisted snapshot
//!   exists) / Play All / Shuffle / Download All (when meta) — library spec
//!   §6.1. Resume availability and the downloaded-playlist flag are disk
//!   reads, so they are checked in blocking tasks when a page loads and
//!   mirrored into `State` for the view.
//! - Rows and playlist cards are drag sources into the queue: wrapped in
//!   `mouse_area` emitting `Message::Queue(queue_panel::Msg::DragStarted {
//!   payload, position })`; the row click rides on release (a release after
//!   the drag activated is a drop, not a click).

use std::collections::HashSet;

use iced::widget::{
    Space, button, column, container, mouse_area, row, scrollable, svg, text, tooltip,
};
use iced::{Element, Length, Point, Task};

use crate::ipc::{PlaybackSnapshot, PlaylistContext, PlaylistContextSource};
use crate::models::{PlaylistSummary, Track};

use super::super::app::{
    self, App, ContentView, LoadState, Message, QueueDragPayload, QueuePlaylistSource,
};
use super::super::queue_panel::{self, DownloadPlaylistMeta};
use super::super::{content_loading, icons, player_bar, sidebar, theme, thumbnails, widgets};

/// Track row art edge (GTK `.track-art` 44×44 parity metric).
const ROW_ART_SIZE: f32 = 44.0;
/// Square cover art edge inside playlist cards (GTK `build_square_cover_art`).
const CARD_ART_SIZE: f32 = 150.0;
/// Top offset of loading/error/empty status labels (GTK `margin_top(60)`).
const STATUS_TOP_MARGIN: f32 = 60.0;
/// Icon edge inside the Resume / Play All / Shuffle / Download controls.
const CONTROL_ICON_SIZE: f32 = 14.0;
/// The YT Music liked-songs playlist id (GTK literal `"LM"`).
const LIKED_SONGS_PLAYLIST_ID: &str = "LM";
const LIKED_SONGS_TITLE: &str = "Liked Songs";

pub struct PlaylistDetail {
    pub playlist_id: String,
    pub title: String,
    pub thumbnail_url: Option<String>,
    pub tracks: LoadState<Vec<Track>>,
}

pub struct OfflinePage {
    pub playlists: Vec<PlaylistSummary>,
    /// Loose tracks shown on the landing page (collection members excluded);
    /// these also feed the page's Play All / Shuffle (GTK parity).
    pub display_tracks: Vec<Track>,
    /// Full cached track list (loose + collection members); not rendered on
    /// the landing page itself but kept alongside it (GTK parity).
    #[allow(dead_code)]
    pub all_tracks: Vec<Track>,
}

pub struct OfflinePlaylistDetail {
    pub playlist_id: String,
    pub title: String,
    pub tracks: LoadState<Vec<Track>>,
}

/// Which library page a lean view message refers to; the receiving `update`
/// resolves the page's track list / playlist context from `State` (avoids
/// cloning whole track lists into every row message at view time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Likes,
    PlaylistDetail,
    Offline,
    OfflinePlaylistDetail,
}

/// Hovered row/card key — rows are `mouse_area`s (queue drag sources), not
/// buttons, so hover feedback is tracked explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hover {
    LikesRow(usize),
    PlaylistCard(usize),
    PlaylistDetailRow(usize),
    OfflineCard(usize),
    OfflineRow(usize),
    OfflinePlaylistRow(usize),
}

pub struct State {
    pub likes: LoadState<Vec<Track>>,
    pub playlists: LoadState<Vec<PlaylistSummary>>,
    pub playlist_detail: Option<PlaylistDetail>,
    pub offline: LoadState<OfflinePage>,
    pub offline_playlist_detail: Option<OfflinePlaylistDetail>,
    /// Resume keys (`{source}:{playlist_id}`) with a persisted non-empty
    /// queue — gates the Resume button (library spec §6.1).
    pub resume_ready: HashSet<String>,
    /// Resume key currently restoring (disables that Resume button).
    pub resume_in_flight: Option<String>,
    /// Playlists `cache.is_playlist_offline` reported as fully downloaded
    /// ("Downloaded" insensitive button).
    pub downloaded_playlists: HashSet<String>,
    /// Playlists whose Download All is running (button disabled, GTK parity).
    pub downloads_in_flight: HashSet<String>,
    pub hovered: Option<Hover>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            likes: LoadState::Idle,
            playlists: LoadState::Idle,
            playlist_detail: None,
            offline: LoadState::Idle,
            offline_playlist_detail: None,
            resume_ready: HashSet::new(),
            resume_in_flight: None,
            downloaded_playlists: HashSet::new(),
            downloads_in_flight: HashSet::new(),
            hovered: None,
        }
    }
}

#[derive(Clone)]
pub enum Msg {
    LikesLoaded(Result<Vec<Track>, String>),
    PlaylistsLoaded(Result<Vec<PlaylistSummary>, String>),
    PlaylistOpened(PlaylistSummary),
    PlaylistDetailLoaded {
        playlist_id: String,
        result: Result<Vec<Track>, String>,
    },
    OfflineLoaded(Result<OfflineLoadedData, String>),
    OfflinePlaylistOpened {
        playlist_id: String,
        title: String,
    },
    OfflinePlaylistLoaded {
        playlist_id: String,
        result: Result<Vec<Track>, String>,
    },
    /// Track row clicked: play from the surrounding context. The views emit
    /// the lean `Page*` forms below; these payload-carrying forms are kept as
    /// the direct API for cross-module callers.
    #[allow(dead_code)]
    TrackClicked {
        tracks: Vec<Track>,
        index: usize,
        context: Option<crate::ipc::PlaylistContext>,
    },
    #[allow(dead_code)]
    PlayAll {
        tracks: Vec<Track>,
        shuffle: bool,
        context: Option<crate::ipc::PlaylistContext>,
    },
    #[allow(dead_code)]
    DownloadAll {
        tracks: Vec<Track>,
        meta: Option<super::super::queue_panel::DownloadPlaylistMeta>,
    },
    /// Resume button on play-controls (per-playlist persisted snapshot).
    ResumeContext {
        context: crate::ipc::PlaylistContext,
    },

    /// Lean row click emitted by the view: resolves the page's track list and
    /// context from `State` in `update`.
    PageTrackClicked {
        page: Page,
        index: usize,
    },
    /// Lean Play All / Shuffle emitted by the view (same resolution).
    PagePlayAll {
        page: Page,
        shuffle: bool,
    },
    /// Lean Download All emitted by the view (same resolution).
    PageDownloadAll {
        page: Page,
    },
    /// "← Back to Playlists" on playlist detail.
    BackToPlaylists,
    /// "← Back to Offline" on offline playlist detail.
    BackToOffline,
    /// Persisted-resume probe finished for a page's playlist context.
    ResumeChecked {
        key: String,
        available: bool,
    },
    /// Resume restore finished; snapshot applied via the player bar.
    ResumeFinished {
        key: String,
        result: Result<PlaybackSnapshot, String>,
    },
    /// `cache.is_playlist_offline` probe finished for a download button.
    DownloadStateChecked {
        playlist_id: String,
        downloaded: bool,
    },
    Hovered(Hover),
    Unhovered(Hover),
}

/// Clonable wire form of the offline landing-page data.
#[derive(Clone)]
pub struct OfflineLoadedData {
    pub playlists: Vec<PlaylistSummary>,
    pub display_tracks: Vec<Track>,
    pub all_tracks: Vec<Track>,
}

/// Renders whichever library view `app.content_view` selects
/// (Likes / Playlists / PlaylistDetail / Offline / OfflinePlaylistDetail).
pub fn view(app: &App) -> Element<'_, Message> {
    match app.content_view {
        ContentView::Likes => likes_view(app),
        ContentView::Playlists => playlists_view(app),
        ContentView::PlaylistDetail => playlist_detail_view(app),
        ContentView::Offline => offline_view(app),
        ContentView::OfflinePlaylistDetail => offline_playlist_detail_view(app),
        // The shell only dispatches the five library views here.
        _ => column![].into(),
    }
}

pub fn update(app: &mut App, msg: Msg) -> Task<Message> {
    match msg {
        Msg::LikesLoaded(result) => match result {
            Ok(tracks) => {
                let urls = track_thumbnail_urls(&tracks);
                let non_empty = !tracks.is_empty();
                app.library.likes = LoadState::Ready(tracks);
                let mut tasks = vec![thumbnails::ensure_all(app, urls.iter().map(String::as_str))];
                if non_empty {
                    tasks.push(check_resume_task(likes_context()));
                    tasks.push(check_download_state_task(
                        app,
                        LIKED_SONGS_PLAYLIST_ID.to_owned(),
                    ));
                }
                Task::batch(tasks)
            }
            Err(err) => {
                app.library.likes = LoadState::Failed(err);
                Task::none()
            }
        },

        Msg::PlaylistsLoaded(result) => match result {
            Ok(playlists) => {
                let urls: Vec<String> = playlists
                    .iter()
                    .filter_map(|playlist| playlist.thumbnail_url.clone())
                    .collect();
                app.library.playlists = LoadState::Ready(playlists);
                thumbnails::ensure_all(app, urls.iter().map(String::as_str))
            }
            Err(err) => {
                app.library.playlists = LoadState::Failed(err);
                Task::none()
            }
        },

        Msg::PlaylistOpened(playlist) => {
            let press = release_pending_press(app);
            if press.was_active {
                return Task::none(); // it was a drag, not a click
            }
            match &press.payload {
                Some(QueueDragPayload::Playlist { playlist_id, .. })
                    if *playlist_id != playlist.playlist_id =>
                {
                    return Task::none(); // release slid in from another card
                }
                Some(QueueDragPayload::Playlist { .. }) | None => {}
                Some(_) => return Task::none(),
            }
            content_loading::load_playlist_detail_task(
                app,
                playlist.playlist_id,
                playlist.title,
                playlist.thumbnail_url,
            )
        }

        Msg::PlaylistDetailLoaded {
            playlist_id,
            result,
        } => {
            let Some(detail) = app.library.playlist_detail.as_mut() else {
                return Task::none();
            };
            if detail.playlist_id != playlist_id {
                return Task::none(); // a newer detail load superseded this one
            }
            match result {
                Ok(tracks) => {
                    let urls = track_thumbnail_urls(&tracks);
                    let non_empty = !tracks.is_empty();
                    detail.tracks = LoadState::Ready(tracks);
                    let context = playlist_detail_context(detail);
                    let mut tasks =
                        vec![thumbnails::ensure_all(app, urls.iter().map(String::as_str))];
                    if non_empty {
                        tasks.push(check_resume_task(context));
                        tasks.push(check_download_state_task(app, playlist_id.clone()));
                    }
                    tasks.push(super::super::playlists::page_opened(app, &playlist_id));
                    Task::batch(tasks)
                }
                Err(err) => {
                    detail.tracks = LoadState::Failed(err);
                    Task::none()
                }
            }
        }

        Msg::OfflineLoaded(result) => match result {
            Ok(data) => {
                let mut urls: Vec<String> = data
                    .playlists
                    .iter()
                    .filter_map(|playlist| playlist.thumbnail_url.clone())
                    .collect();
                urls.extend(track_thumbnail_urls(&data.display_tracks));
                app.library.offline = LoadState::Ready(OfflinePage {
                    playlists: data.playlists,
                    display_tracks: data.display_tracks,
                    all_tracks: data.all_tracks,
                });
                thumbnails::ensure_all(app, urls.iter().map(String::as_str))
            }
            Err(err) => {
                app.library.offline = LoadState::Failed(format!("Offline load failed: {err}"));
                Task::none()
            }
        },

        Msg::OfflinePlaylistOpened { playlist_id, title } => {
            let press = release_pending_press(app);
            if press.was_active {
                return Task::none();
            }
            match &press.payload {
                Some(QueueDragPayload::Playlist {
                    playlist_id: pressed,
                    ..
                }) if *pressed != playlist_id => {
                    return Task::none();
                }
                Some(QueueDragPayload::Playlist { .. }) | None => {}
                Some(_) => return Task::none(),
            }
            content_loading::load_offline_playlist_detail_task(app, playlist_id, title)
        }

        Msg::OfflinePlaylistLoaded {
            playlist_id,
            result,
        } => {
            let Some(detail) = app.library.offline_playlist_detail.as_mut() else {
                return Task::none();
            };
            if detail.playlist_id != playlist_id {
                return Task::none();
            }
            match result {
                Ok(tracks) => {
                    let urls = track_thumbnail_urls(&tracks);
                    let non_empty = !tracks.is_empty();
                    detail.tracks = LoadState::Ready(tracks);
                    let context = offline_detail_context(detail);
                    let mut tasks =
                        vec![thumbnails::ensure_all(app, urls.iter().map(String::as_str))];
                    if non_empty {
                        tasks.push(check_resume_task(context));
                    }
                    Task::batch(tasks)
                }
                Err(err) => {
                    detail.tracks =
                        LoadState::Failed(format!("Offline playlist load failed: {err}"));
                    Task::none()
                }
            }
        }

        Msg::TrackClicked {
            tracks,
            index,
            context,
        } => play_track_context(app, tracks, index, context),

        Msg::PageTrackClicked { page, index } => {
            if page == Page::PlaylistDetail {
                if let Some(task) = super::super::playlists::drop_on_row(app, index) {
                    return task; // a reorder drag landed on this row
                }
            }
            let (tracks, context) = page_play_data(app, page);
            play_track_context(app, tracks, index, context)
        }

        Msg::PlayAll {
            tracks,
            shuffle,
            context,
        } => {
            if tracks.is_empty() {
                return Task::none();
            }
            queue_panel::play_all_task(app, tracks, shuffle, context)
        }

        Msg::PagePlayAll { page, shuffle } => {
            let (tracks, context) = page_play_data(app, page);
            if tracks.is_empty() {
                return Task::none();
            }
            queue_panel::play_all_task(app, tracks, shuffle, context)
        }

        Msg::DownloadAll { tracks, meta } => {
            if tracks.is_empty() {
                return Task::none();
            }
            if let Some(meta) = &meta {
                app.library
                    .downloads_in_flight
                    .insert(meta.playlist_id.clone());
            }
            queue_panel::download_all_task(app, tracks, meta)
        }

        Msg::PageDownloadAll { page } => {
            let Some(meta) = page_download_meta(app, page) else {
                return Task::none();
            };
            let (tracks, _) = page_play_data(app, page);
            if tracks.is_empty() {
                return Task::none();
            }
            app.library
                .downloads_in_flight
                .insert(meta.playlist_id.clone());
            queue_panel::download_all_task(app, tracks, Some(meta))
        }

        Msg::ResumeContext { context } => resume_task(app, context),

        Msg::ResumeChecked { key, available } => {
            if available {
                app.library.resume_ready.insert(key);
            } else {
                app.library.resume_ready.remove(&key);
            }
            Task::none()
        }

        Msg::ResumeFinished { key, result } => {
            if app.library.resume_in_flight.as_deref() == Some(key.as_str()) {
                app.library.resume_in_flight = None;
            }
            match result {
                Ok(snapshot) => {
                    // GTK kept the Resume button insensitive after a
                    // successful resume until the page was rebuilt; drop the
                    // key so it is re-probed on the next page load instead of
                    // re-enabling immediately.
                    app.library.resume_ready.remove(&key);
                    player_bar::apply_remote_snapshot(app, snapshot)
                }
                Err(err) => {
                    crate::app_log!("[UI] failed to resume playlist {key}: {err}");
                    Task::none()
                }
            }
        }

        Msg::DownloadStateChecked {
            playlist_id,
            downloaded,
        } => {
            if downloaded {
                // Also the completion signal from `download_all_task`: clear the
                // in-flight marker so the button flips to "Downloaded" without a
                // page reload, and mirror into the browse-detail flags when the
                // download was started from a browse page.
                app.library.downloads_in_flight.remove(&playlist_id);
                if app.browse.browse_id.as_deref() == Some(playlist_id.as_str()) {
                    app.browse.download_in_flight = false;
                    app.browse.playlist_downloaded = true;
                }
                app.library.downloaded_playlists.insert(playlist_id);
            } else {
                app.library.downloaded_playlists.remove(&playlist_id);
            }
            Task::none()
        }

        Msg::BackToPlaylists => content_loading::load_playlists_task(app),
        Msg::BackToOffline => content_loading::load_offline_task(app),

        Msg::Hovered(hover) => {
            app.library.hovered = Some(hover);
            Task::none()
        }
        Msg::Unhovered(hover) => {
            if app.library.hovered == Some(hover) {
                app.library.hovered = None;
            }
            Task::none()
        }
    }
}

// ---------------------------------------------------------------------------
// Click plumbing
// ---------------------------------------------------------------------------

/// Left-click on a track row: play the page's track list from the clicked
/// index under the page's playlist context (daemon `play_queue_at`) and
/// prepend the track to the sidebar recents (views spec §2.4).
fn play_track_context(
    app: &mut App,
    tracks: Vec<Track>,
    index: usize,
    context: Option<PlaylistContext>,
) -> Task<Message> {
    let press = release_pending_press(app);
    if press.was_active {
        return Task::none(); // it was a drag, not a click
    }
    let Some(track) = tracks.get(index).cloned() else {
        return Task::none();
    };
    match &press.payload {
        Some(QueueDragPayload::Track { track: pressed }) if pressed.video_id != track.video_id => {
            return Task::none(); // release slid in from another row
        }
        Some(QueueDragPayload::Track { .. }) | None => {}
        Some(_) => return Task::none(),
    }
    sidebar::add_to_recent(app, &track);
    queue_panel::play_from_context_task(app, tracks, index, context)
}

struct ReleasedPress {
    payload: Option<QueueDragPayload>,
    was_active: bool,
}

/// Rows/cards deliver their click on `mouse_area::on_release` (the press
/// registered a pending queue drag). A release before the drag activated is
/// the normal click (CONTRACT.md §7.2); either way the pending drag state is
/// cleared because the queue panel never sees releases over the content area.
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

// ---------------------------------------------------------------------------
// Page data resolution (lean message handlers)
// ---------------------------------------------------------------------------

fn page_play_data(app: &App, page: Page) -> (Vec<Track>, Option<PlaylistContext>) {
    match page {
        Page::Likes => match &app.library.likes {
            LoadState::Ready(tracks) => (tracks.clone(), Some(likes_context())),
            _ => (Vec::new(), None),
        },
        Page::PlaylistDetail => match &app.library.playlist_detail {
            Some(detail) => match &detail.tracks {
                LoadState::Ready(tracks) => (tracks.clone(), Some(playlist_detail_context(detail))),
                _ => (Vec::new(), None),
            },
            None => (Vec::new(), None),
        },
        Page::Offline => match &app.library.offline {
            // GTK parity: the offline landing page plays its loose-track list.
            LoadState::Ready(offline) => (offline.display_tracks.clone(), None),
            _ => (Vec::new(), None),
        },
        Page::OfflinePlaylistDetail => match &app.library.offline_playlist_detail {
            Some(detail) => match &detail.tracks {
                LoadState::Ready(tracks) => (tracks.clone(), Some(offline_detail_context(detail))),
                _ => (Vec::new(), None),
            },
            None => (Vec::new(), None),
        },
    }
}

fn page_download_meta(app: &App, page: Page) -> Option<DownloadPlaylistMeta> {
    match page {
        Page::Likes => Some(DownloadPlaylistMeta {
            playlist_id: LIKED_SONGS_PLAYLIST_ID.to_owned(),
            title: LIKED_SONGS_TITLE.to_owned(),
            thumbnail_url: None,
            subtitle: None,
        }),
        Page::PlaylistDetail => {
            let detail = app.library.playlist_detail.as_ref()?;
            let subtitle = match &detail.tracks {
                LoadState::Ready(tracks) => Some(format!("{} tracks", tracks.len())),
                _ => None,
            };
            Some(DownloadPlaylistMeta {
                playlist_id: detail.playlist_id.clone(),
                title: detail.title.clone(),
                thumbnail_url: detail.thumbnail_url.clone(),
                subtitle,
            })
        }
        // Offline pages never show Download All (library spec §5.9–5.10).
        Page::Offline | Page::OfflinePlaylistDetail => None,
    }
}

fn likes_context() -> PlaylistContext {
    PlaylistContext {
        playlist_id: LIKED_SONGS_PLAYLIST_ID.to_owned(),
        title: LIKED_SONGS_TITLE.to_owned(),
        thumbnail_url: None,
        subtitle: None,
        source: PlaylistContextSource::Remote,
    }
}

fn playlist_detail_context(detail: &PlaylistDetail) -> PlaylistContext {
    PlaylistContext {
        playlist_id: detail.playlist_id.clone(),
        title: detail.title.clone(),
        thumbnail_url: detail.thumbnail_url.clone(),
        subtitle: None,
        source: PlaylistContextSource::Remote,
    }
}

fn offline_detail_context(detail: &OfflinePlaylistDetail) -> PlaylistContext {
    PlaylistContext {
        playlist_id: detail.playlist_id.clone(),
        title: detail.title.clone(),
        thumbnail_url: None,
        subtitle: None,
        source: PlaylistContextSource::Offline,
    }
}

/// Resume buttons key persisted state per `{source}:{playlist_id}` (the same
/// shape as the IPC storage key — the same playlist opened via browse vs
/// library resumes independently).
fn resume_key(context: &PlaylistContext) -> String {
    let segment = match context.source {
        PlaylistContextSource::Remote => "remote",
        PlaylistContextSource::Offline => "offline",
        PlaylistContextSource::Browse => "browse",
    };
    format!("{segment}:{}", context.playlist_id)
}

// ---------------------------------------------------------------------------
// Background probes + resume restore
// ---------------------------------------------------------------------------

/// Probe `crate::ipc::load_persisted_playlist_state` for a page's context;
/// load errors are logged and treated as "no resume" (GTK parity).
fn check_resume_task(context: PlaylistContext) -> Task<Message> {
    let key = resume_key(&context);
    app::blocking(
        move || match crate::ipc::load_persisted_playlist_state(&context) {
            Ok(Some(persisted)) => !persisted.snapshot.queue.is_empty(),
            Ok(None) => false,
            Err(err) => {
                crate::app_log!(
                    "[UI] failed to load persisted playlist state for {}: {err}",
                    context.playlist_id
                );
                false
            }
        },
        move |available| {
            Message::Library(Msg::ResumeChecked {
                key: key.clone(),
                available,
            })
        },
    )
}

fn check_download_state_task(app: &App, playlist_id: String) -> Task<Message> {
    let cache = app.cache.clone();
    app::blocking(
        move || {
            let downloaded = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_playlist_offline(&playlist_id);
            (playlist_id, downloaded)
        },
        |(playlist_id, downloaded)| {
            Message::Library(Msg::DownloadStateChecked {
                playlist_id,
                downloaded,
            })
        },
    )
}

/// Resume click (library spec §6.1): disable the button, re-load the persisted
/// state on a blocking thread ("No saved state for this playlist" if gone or
/// empty) and restore it through the daemon; the returned snapshot is applied
/// by `Msg::ResumeFinished`, failure re-enables the button.
fn resume_task(app: &mut App, context: PlaylistContext) -> Task<Message> {
    let key = resume_key(&context);
    if app.library.resume_in_flight.is_some() {
        return Task::none();
    }
    app.library.resume_in_flight = Some(key.clone());
    let panic_key = key.clone();
    app::blocking_or_timeout(
        app::DAEMON_TASK_TIMEOUT,
        move || {
            let persisted = crate::ipc::load_persisted_playlist_state(&context)
                .map_err(|err| err.to_string())?
                .filter(|persisted| !persisted.snapshot.queue.is_empty())
                .ok_or_else(|| "No saved state for this playlist".to_owned())?;
            restore_playback_snapshot_via_daemon(persisted.snapshot).map_err(|err| err.to_string())
        },
        move |result| {
            Message::Library(Msg::ResumeFinished {
                key: key.clone(),
                result,
            })
        },
        // A stuck restore must still clear `resume_in_flight`.
        Message::Library(Msg::ResumeFinished {
            key: panic_key,
            result: Err("daemon resume request timed out".to_owned()),
        }),
    )
}

/// Exact GTK daemon restore sequence (playback.rs): ensure daemon → volume →
/// `play_queue_at` (clamped index) → optional seek → pause/resume per the
/// persisted snapshot.
fn restore_playback_snapshot_via_daemon(
    snapshot: PlaybackSnapshot,
) -> anyhow::Result<PlaybackSnapshot> {
    crate::ipc::ensure_daemon_running()?;
    let client = crate::ipc::DaemonClient::new()?;
    let queue_index = snapshot
        .queue_index
        .min(snapshot.queue.len().saturating_sub(1));

    let mut restored = client.set_volume(snapshot.volume)?;
    if snapshot.queue.is_empty() {
        return Ok(restored);
    }

    restored = client.play_queue_at(
        snapshot.queue.clone(),
        queue_index,
        snapshot.playlist_context.clone(),
        snapshot.radio_virtual_playlist.clone(),
        snapshot.repeat_mode,
    )?;

    if let Some(position_millis) = snapshot.position_millis.filter(|position| *position > 0) {
        restored = client.seek_to_millis(position_millis)?;
    }
    if snapshot.paused {
        restored = client.pause()?;
    } else if restored.paused {
        restored = client.resume()?;
    }

    Ok(restored)
}

// ---------------------------------------------------------------------------
// View composition
// ---------------------------------------------------------------------------

fn likes_view(app: &App) -> Element<'_, Message> {
    match &app.library.likes {
        LoadState::Idle | LoadState::Loading => status_view("Loading liked songs...".to_owned()),
        LoadState::Failed(err) => status_view(err.clone()),
        LoadState::Ready(tracks) => {
            let mut page = column![widgets::section_header(LIKED_SONGS_TITLE)]
                .spacing(theme::SPACE_LG)
                .width(Length::Fill);
            if tracks.is_empty() {
                return page.push(empty_state("No liked songs yet.")).into();
            }
            page = page.push(count_label(format!("{} liked songs", tracks.len())));
            page = page.push(play_controls(
                app,
                Page::Likes,
                Some(likes_context()),
                Some(LIKED_SONGS_PLAYLIST_ID),
            ));
            page.push(track_grid(app, tracks, Page::Likes, Hover::LikesRow))
                .into()
        }
    }
}

fn playlists_view(app: &App) -> Element<'_, Message> {
    match &app.library.playlists {
        LoadState::Idle | LoadState::Loading => status_view("Loading playlists...".to_owned()),
        LoadState::Failed(err) => status_view(err.clone()),
        LoadState::Ready(playlists) => {
            let page = column![
                row![
                    widgets::section_header("Your Playlists"),
                    Space::new().width(Length::Fill),
                    super::super::playlists::create_button(),
                ]
                .align_y(iced::Alignment::Center)
                .width(Length::Fill)
            ]
            .spacing(theme::SPACE_LG)
            .width(Length::Fill);
            if playlists.is_empty() {
                return page.push(empty_state("No playlists found.")).into();
            }
            let cards: Vec<Element<'_, Message>> = playlists
                .iter()
                .enumerate()
                .map(|(index, playlist)| {
                    playlist_card(
                        app,
                        Hover::PlaylistCard(index),
                        playlist,
                        QueuePlaylistSource::Remote,
                        Msg::PlaylistOpened(playlist.clone()),
                        Some(super::super::playlists::card_menu_message(playlist)),
                        true,
                    )
                })
                .collect();
            page.push(
                iced::widget::Row::with_children(cards)
                    .spacing(theme::SPACE_LG)
                    .wrap()
                    .vertical_spacing(theme::SPACE_LG),
            )
            .into()
        }
    }
}

fn playlist_detail_view(app: &App) -> Element<'_, Message> {
    let Some(detail) = app.library.playlist_detail.as_ref() else {
        return status_view("Loading...".to_owned());
    };
    match &detail.tracks {
        LoadState::Idle | LoadState::Loading => {
            status_view(format!("Loading \"{}\"...", detail.title))
        }
        LoadState::Failed(err) => status_view(err.clone()),
        LoadState::Ready(tracks) => {
            let mut page = column![
                back_button("\u{2190} Back to Playlists", Msg::BackToPlaylists),
                widgets::section_header(&detail.title),
            ]
            .spacing(theme::SPACE_LG)
            .width(Length::Fill);
            if tracks.is_empty() {
                let controls = super::super::playlists::page_controls(app);
                if !controls.is_empty() {
                    page = page
                        .push(iced::widget::Row::with_children(controls).spacing(theme::SPACE_MD));
                }
                return page.push(empty_state("This playlist is empty.")).into();
            }
            page = page.push(count_label(format!("{} tracks", tracks.len())));
            page = page.push(play_controls(
                app,
                Page::PlaylistDetail,
                Some(playlist_detail_context(detail)),
                Some(detail.playlist_id.as_str()),
            ));
            super::super::playlists::track_pointer(page.push(track_grid(
                app,
                tracks,
                Page::PlaylistDetail,
                Hover::PlaylistDetailRow,
            )))
        }
    }
}

fn offline_view(app: &App) -> Element<'_, Message> {
    match &app.library.offline {
        LoadState::Idle | LoadState::Loading => {
            status_view("Loading offline library...".to_owned())
        }
        LoadState::Failed(err) => status_view(err.clone()),
        LoadState::Ready(offline) => {
            if offline.display_tracks.is_empty() && offline.playlists.is_empty() {
                return status_view(
                    "No offline songs yet. Play songs to build your cache.".to_owned(),
                );
            }

            let mut page = column![widgets::section_header("Offline")]
                .spacing(theme::SPACE_LG)
                .width(Length::Fill);

            if !offline.playlists.is_empty() {
                page = page.push(widgets::section_header("Offline Collections"));
                let cards: Vec<Element<'_, Message>> = offline
                    .playlists
                    .iter()
                    .enumerate()
                    .map(|(index, playlist)| {
                        playlist_card(
                            app,
                            Hover::OfflineCard(index),
                            playlist,
                            QueuePlaylistSource::Offline,
                            Msg::OfflinePlaylistOpened {
                                playlist_id: playlist.playlist_id.clone(),
                                title: playlist.title.clone(),
                            },
                            None,
                            false,
                        )
                    })
                    .collect();
                page = page.push(horizontal_cards(cards));
                page = page.push(widgets::separator());
            }

            if !offline.display_tracks.is_empty() {
                page = page.push(count_label(format!(
                    "{} songs available offline",
                    offline.display_tracks.len()
                )));
                page = page.push(play_controls(app, Page::Offline, None, None));
                // Single-column list (GTK parity: NOT the 2-col grid).
                let mut list = column![].spacing(theme::SPACE_SM).width(Length::Fill);
                for (index, track) in offline.display_tracks.iter().enumerate() {
                    list = list.push(track_row(
                        app,
                        Hover::OfflineRow(index),
                        track,
                        Msg::PageTrackClicked {
                            page: Page::Offline,
                            index,
                        },
                        None,
                    ));
                }
                page = page.push(list);
            }

            page.into()
        }
    }
}

fn offline_playlist_detail_view(app: &App) -> Element<'_, Message> {
    let Some(detail) = app.library.offline_playlist_detail.as_ref() else {
        return status_view("Loading...".to_owned());
    };
    match &detail.tracks {
        LoadState::Idle | LoadState::Loading => {
            status_view(format!("Loading \"{}\"...", detail.title))
        }
        LoadState::Failed(err) => status_view(err.clone()),
        LoadState::Ready(tracks) => {
            let mut page = column![
                back_button("\u{2190} Back to Offline", Msg::BackToOffline),
                widgets::section_header(&detail.title),
            ]
            .spacing(theme::SPACE_LG)
            .width(Length::Fill);
            if tracks.is_empty() {
                return page
                    .push(empty_state("No tracks downloaded for this playlist."))
                    .into();
            }
            page = page.push(count_label(format!("{} tracks", tracks.len())));
            // No Download button — already offline; Resume CAN appear via the
            // persisted Offline-source context (library spec §5.10).
            page = page.push(play_controls(
                app,
                Page::OfflinePlaylistDetail,
                Some(offline_detail_context(detail)),
                None,
            ));
            page.push(track_grid(
                app,
                tracks,
                Page::OfflinePlaylistDetail,
                Hover::OfflinePlaylistRow,
            ))
            .into()
        }
    }
}

// ---------------------------------------------------------------------------
// Shared page pieces
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

fn empty_state(message: &str) -> Element<'static, Message> {
    container(
        text(message.to_owned())
            .size(theme::SIZE_BODY)
            .color(theme::TEXT_DIM),
    )
    .width(Length::Fill)
    .align_x(iced::alignment::Horizontal::Center)
    .padding(iced::Padding {
        top: theme::SPACE_XXL,
        ..iced::Padding::ZERO
    })
    .into()
}

fn back_button(label: &'static str, msg: Msg) -> Element<'static, Message> {
    container(
        button(text(label).size(theme::SIZE_BODY))
            .style(theme::nav_item(false))
            .padding([theme::SPACE_SM, theme::SPACE_LG])
            .on_press(Message::Library(msg)),
    )
    .align_x(iced::alignment::Horizontal::Left)
    .into()
}

/// Play-controls row (library spec §6.1): Resume (persisted snapshot) /
/// Play All / Shuffle / Download All ("Downloaded" insensitive once the
/// playlist is fully cached; disabled while a download runs).
fn play_controls<'a>(
    app: &'a App,
    page: Page,
    context: Option<PlaylistContext>,
    download_playlist_id: Option<&str>,
) -> Element<'a, Message> {
    let mut controls = row![]
        .spacing(theme::SPACE_MD)
        .align_y(iced::Alignment::Center);

    if let Some(context) = context {
        let key = resume_key(&context);
        if app.library.resume_ready.contains(&key) {
            let busy = app.library.resume_in_flight.as_deref() == Some(key.as_str());
            let mut resume = button(control_content(icons::play(), "Resume"))
                .style(theme::secondary_button)
                .padding([theme::SPACE_SM, theme::SPACE_LG]);
            if !busy {
                resume = resume.on_press(Message::Library(Msg::ResumeContext { context }));
            }
            controls = controls.push(resume);
        }
    }

    controls = controls.push(control_button(
        icons::play(),
        "Play All",
        Msg::PagePlayAll {
            page,
            shuffle: false,
        },
    ));
    controls = controls.push(control_button(
        icons::shuffle(),
        "Shuffle",
        Msg::PagePlayAll {
            page,
            shuffle: true,
        },
    ));

    if let Some(playlist_id) = download_playlist_id {
        let downloaded = app.library.downloaded_playlists.contains(playlist_id);
        let busy = app.library.downloads_in_flight.contains(playlist_id);
        let label = if downloaded {
            "Downloaded"
        } else {
            "Download All"
        };
        let mut download = button(control_content(icons::download(), label))
            .style(theme::secondary_button)
            .padding([theme::SPACE_SM, theme::SPACE_LG]);
        if !downloaded && !busy {
            download = download.on_press(Message::Library(Msg::PageDownloadAll { page }));
        }
        controls = controls.push(download);
    }

    if page == Page::PlaylistDetail {
        for control in super::super::playlists::page_controls(app) {
            controls = controls.push(control);
        }
    }

    controls.into()
}

fn control_content(icon: svg::Handle, label: &'static str) -> Element<'static, Message> {
    row![
        widgets::icon(icon, CONTROL_ICON_SIZE),
        text(label).size(theme::SIZE_BODY),
    ]
    .spacing(theme::SPACE_SM)
    .align_y(iced::Alignment::Center)
    .into()
}

fn control_button(icon: svg::Handle, label: &'static str, msg: Msg) -> Element<'static, Message> {
    button(control_content(icon, label))
        .style(theme::secondary_button)
        .padding([theme::SPACE_SM, theme::SPACE_LG])
        .on_press(Message::Library(msg))
        .into()
}

/// Homogeneous 2-column track grid: item `i` lands at column `i % 2`, row
/// `i / 2` (GTK `gtk::Grid` parity); a trailing odd cell keeps half width.
fn track_grid<'a>(
    app: &'a App,
    tracks: &'a [Track],
    page: Page,
    hover_key: fn(usize) -> Hover,
) -> Element<'a, Message> {
    let mut grid = column![].spacing(theme::SPACE_MD).width(Length::Fill);
    let mut cells = tracks.iter().enumerate().map(|(index, track)| {
        track_row(
            app,
            hover_key(index),
            track,
            Msg::PageTrackClicked { page, index },
            (page == Page::PlaylistDetail).then_some(index),
        )
    });
    while let Some(left) = cells.next() {
        let right: Element<'a, Message> = match cells.next() {
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

/// Track row: 44 px art (letter fallback), truncated title with full-text
/// tooltip (marquee stand-in), ellipsized artist, optional duration. The row
/// is a queue drag source; the click rides on release; right-click opens the
/// shared track menu (Play next / Add to queue / Jump to / Probe / feedback /
/// download — owned by queue_panel). `playlist_index` marks a row of the
/// playlist page: its press doubles as the reorder drag source and its menu
/// carries "Remove from playlist".
fn track_row<'a>(
    app: &'a App,
    hover: Hover,
    track: &'a Track,
    click: Msg,
    playlist_index: Option<usize>,
) -> Element<'a, Message> {
    let hovered = app.library.hovered == Some(hover);
    let drop = match playlist_index {
        Some(index) => super::super::playlists::row_drop(app, index, hovered),
        None => theme::RowDrop::None,
    };
    let (press, menu) = match playlist_index {
        Some(index) => (
            super::super::playlists::row_press(index, track),
            super::super::playlists::row_menu(index, track),
        ),
        None => (
            Message::Queue(queue_panel::Msg::DragStarted {
                payload: QueueDragPayload::Track {
                    track: track.clone(),
                },
                position: Point::ORIGIN,
            }),
            Message::TrackMenu(queue_panel::MenuMsg::Open {
                track: track.clone(),
                queue_index: None,
                anchor: Point::ORIGIN,
            }),
        ),
    };
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
                .font(theme::FONT_MONO)
                .size(theme::SIZE_CAPTION)
                .color(theme::TEXT_DIM),
        );
    }

    mouse_area(
        container(content)
            .padding([theme::SPACE_SM, theme::SPACE_MD])
            .width(Length::Fill)
            .style(theme::row_surface_drop(hovered, drop)),
    )
    .interaction(iced::mouse::Interaction::Pointer)
    .on_press(press)
    .on_release(Message::Library(click))
    .on_right_press(menu)
    .on_enter(Message::Library(Msg::Hovered(hover)))
    .on_exit(Message::Library(Msg::Unhovered(hover)))
    .into()
}

/// 150 px square-art summary card (GTK `build_browse_summary_card` /
/// `build_offline_playlist_card`). Online cards get the tooltip title
/// (marquee stand-in); offline cards keep the plain clipped label (GTK
/// parity: no marquee). Cards are queue drag sources with a Playlist payload
/// carrying the right source.
fn playlist_card<'a>(
    app: &'a App,
    hover: Hover,
    playlist: &'a PlaylistSummary,
    source: QueuePlaylistSource,
    click: Msg,
    menu: Option<Message>,
    tooltip_title: bool,
) -> Element<'a, Message> {
    let hovered = app.library.hovered == Some(hover);
    let art = thumbnails::view_thumb(
        app,
        playlist.thumbnail_url.as_deref(),
        CARD_ART_SIZE,
        playlist.title.chars().next(),
    );

    let title: Element<'a, Message> = if tooltip_title {
        clipped_title(&playlist.title)
    } else {
        container(
            text(&playlist.title)
                .size(theme::SIZE_BODY)
                .font(theme::FONT_SEMIBOLD)
                .color(theme::TEXT_BRIGHT)
                .wrapping(text::Wrapping::None),
        )
        .width(Length::Fill)
        .clip(true)
        .into()
    };

    let mut info = column![title]
        .spacing(theme::SPACE_XXS)
        .width(Length::Fixed(CARD_ART_SIZE));
    if let Some(subtitle) = playlist
        .subtitle
        .as_deref()
        .filter(|subtitle| !subtitle.trim().is_empty())
    {
        info = info.push(clipped_caption(subtitle.to_owned()));
    }

    let body = column![art, info]
        .spacing(theme::SPACE_SM)
        .width(Length::Fixed(CARD_ART_SIZE));

    let mut area = mouse_area(
        container(body)
            .padding(theme::SPACE_MD)
            .style(theme::card_surface(hovered)),
    )
    .interaction(iced::mouse::Interaction::Pointer)
    .on_press(Message::Queue(queue_panel::Msg::DragStarted {
        payload: QueueDragPayload::Playlist {
            playlist_id: playlist.playlist_id.clone(),
            title: playlist.title.clone(),
            thumbnail_url: playlist.thumbnail_url.clone(),
            subtitle: playlist.subtitle.clone(),
            source,
        },
        position: Point::ORIGIN,
    }))
    .on_release(Message::Library(click))
    .on_enter(Message::Library(Msg::Hovered(hover)))
    .on_exit(Message::Library(Msg::Unhovered(hover)));
    if let Some(menu) = menu {
        area = area.on_right_press(menu);
    }
    area.into()
}

/// Horizontal card rail (GTK `build_horizontal_cards_scroll`): the page itself
/// scrolls vertically, the rail scrolls horizontally.
fn horizontal_cards(cards: Vec<Element<'_, Message>>) -> Element<'_, Message> {
    scrollable(
        iced::widget::Row::with_children(cards)
            .spacing(theme::SPACE_LG)
            .padding(iced::Padding {
                bottom: theme::SPACE_LG,
                ..iced::Padding::ZERO
            }),
    )
    .direction(scrollable::Direction::Horizontal(
        scrollable::Scrollbar::new()
            .width(theme::SPACE_XS)
            .scroller_width(theme::SPACE_XS)
            .margin(theme::SPACE_XXS),
    ))
    .style(theme::thin_scrollbar)
    .width(Length::Fill)
    .into()
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
// Pure helpers (ported from GTK window.rs)
// ---------------------------------------------------------------------------

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

fn count_label(label: String) -> Element<'static, Message> {
    text(label)
        .size(theme::SIZE_BODY)
        .color(theme::TEXT_DIM)
        .into()
}

fn track_thumbnail_urls(tracks: &[Track]) -> Vec<String> {
    tracks
        .iter()
        .filter_map(|track| track.thumbnail_url.clone())
        .collect()
}
