//! Application shell: state, message map, update dispatch, subscriptions.
//!
//! Spec: shell.md (plus the state sections of every
//! other spec). This file is ARCHITECT-OWNED — implementers must not edit it;
//! see the migration contract.
//!
//! Model (unchanged from GTK): the playback daemon owns playback. This UI is a
//! client — every mutation is a `crate::ipc::DaemonClient` call executed on a
//! blocking thread, and the returned `PlaybackSnapshot` is applied back onto
//! the UI (`player_bar::apply_remote_snapshot`).

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::widget::{column, container, row, scrollable};
use iced::{Element, Length, Subscription, Task};

use crate::ipc::{DaemonClient, PlaybackSnapshot, PlaylistContext, RepeatMode};
use crate::models::{RadioChip, RadioVirtualPlaylist, SearchResult, Track};
use crate::network::SongRating;
use crate::network_state::NetworkState;
use crate::offline::{AppSettings, MusicCache};

use super::{content_loading, player_bar, queue_panel, sidebar, theme, thumbnails, views, widgets};

// ---------------------------------------------------------------------------
// Timing constants (parity: shell spec §10)
// ---------------------------------------------------------------------------

/// Daemon snapshot/position poll.
pub const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(400);
/// A poll that has been in flight at least this long is considered stuck.
pub const DAEMON_POLL_STUCK_AFTER: Duration = Duration::from_secs(3);
/// Max time the UI will wait for a daemon IPC worker before surfacing failure.
pub const DAEMON_TASK_TIMEOUT: Duration = Duration::from_secs(4);
/// Escape hatch for in-flight guard flags (`play_request_in_flight`,
/// `daemon_recovery_in_flight`). Their completion messages are normally
/// guaranteed by `blocking_or_timeout`, but a completion that never arrives
/// (observed in the field: a wedged guard silently dropped every poll
/// snapshot for hours) must not gate the poll pipeline forever. Anything in
/// flight longer than the worker timeout plus grace is treated as lost.
pub const GUARD_FLAG_STALE_AFTER: Duration = Duration::from_secs(7);
/// Max time close waits for resume-state persistence before closing anyway.
pub const CLOSE_PERSIST_TIMEOUT: Duration = Duration::from_secs(2);
/// UI-client lease renewal.
pub const UI_CLIENT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Sidebar "Recently Played" silent refresh.
pub const SIDEBAR_HISTORY_INTERVAL: Duration = Duration::from_secs(60);
/// Queue-drag autoscroll tick (GTK used a per-frame timer).
pub const QUEUE_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(16);
/// Search suggestion debounce.
pub const SUGGESTION_DEBOUNCE: Duration = Duration::from_millis(250);
/// Toast visibility duration (GTK `show_toast` hid after 2 s).
pub const TOAST_DURATION: Duration = Duration::from_secs(2);

pub const SIDEBAR_HISTORY_LIMIT: usize = 200;
pub const HOME_INFINITE_SCROLL_THRESHOLD: f32 = 720.0;
pub const HOME_BOOTSTRAP_BACKOFF_SECS: [u64; 3] = [1, 2, 4];
pub const OFFLINE_LISTEN_AGAIN_PLAYLIST_ID: &str = "__offline_listen_again__";

/// `text_input` id of the sidebar search entry (Ctrl+F focus target).
pub const SEARCH_INPUT_ID: &str = "sidebar-search-input";
/// `scrollable` id of the main content scroller (scroll restore target).
pub const CONTENT_SCROLL_ID: &str = "content-scroll";
/// `scrollable` id of the queue list (drag autoscroll target).
pub const QUEUE_SCROLL_ID: &str = "queue-scroll";
/// `scrollable` id of the lyrics page (active-line follow target).
pub const LYRICS_SCROLL_ID: &str = "lyrics-scroll";

// ---------------------------------------------------------------------------
// Shared navigation / view bookkeeping types (ported from GTK AppState)
// ---------------------------------------------------------------------------

/// Root page (GTK root `Stack`): onboarding / vr_auth / main.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Onboarding,
    VrAuth,
    Main,
}

/// The five sidebar nav slots. Exactly one is highlighted at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavItem {
    Home,
    Likes,
    Playlists,
    Offline,
    Settings,
}

/// What the content area is currently rendering. (GTK tracked this through a
/// mix of `ContentViewKind`, nav CSS classes and the `home-view-active`
/// marker; here it is one explicit enum.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentView {
    Home,
    Search,
    BrowseDetail,
    Likes,
    Playlists,
    PlaylistDetail,
    Offline,
    OfflinePlaylistDetail,
    Settings,
}

/// Back-target semantics, ported 1:1 (shell spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentViewKind {
    Home,
    Search,
    BrowseDetail,
}

impl ContentView {
    pub fn kind(self) -> ContentViewKind {
        match self {
            ContentView::Search => ContentViewKind::Search,
            ContentView::BrowseDetail => ContentViewKind::BrowseDetail,
            _ => ContentViewKind::Home,
        }
    }
}

#[derive(Clone)]
pub struct SearchPageCache {
    pub query: String,
    pub results: SearchResult,
    pub scroll_position: f32,
}

#[derive(Clone)]
pub enum BrowseBackTarget {
    Home,
    Search(SearchPageCache),
}

/// Pure back-target rule, ported from GTK (unit-tested there):
/// - `Search` while a search cache exists → back target is Search.
/// - `BrowseDetail` → keep the existing target's kind (browse→browse preserves
///   the original origin).
/// - anything else → Home.
pub fn selected_browse_back_target_kind(
    current_view: ContentViewKind,
    existing_target: &BrowseBackTarget,
    has_search_cache: bool,
) -> ContentViewKind {
    match current_view {
        ContentViewKind::Search if has_search_cache => ContentViewKind::Search,
        ContentViewKind::BrowseDetail => match existing_target {
            BrowseBackTarget::Search(_) => ContentViewKind::Search,
            BrowseBackTarget::Home => ContentViewKind::Home,
        },
        _ => ContentViewKind::Home,
    }
}

/// Play-tracking record tying async rating/lyrics results to the right track.
#[derive(Debug, Clone)]
pub struct NowPlayingTrack {
    pub session_id: u64,
    pub video_id: String,
    pub rating: SongRating,
    pub last_observed_position_millis: u64,
}

/// Transient bottom-center toast over the content area.
#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub serial: u64,
}

/// Serialized drag payload — same shapes the GTK DnD JSON used; with iced the
/// payload travels in-process inside `queue_panel::DragState`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueueDragPayload {
    QueueRow {
        index: usize,
    },
    Track {
        track: Track,
    },
    Playlist {
        playlist_id: String,
        title: String,
        thumbnail_url: Option<String>,
        subtitle: Option<String>,
        source: QueuePlaylistSource,
    },
    BrowseCollection {
        browse_id: String,
        title: String,
        thumbnail_url: Option<String>,
        subtitle: Option<String>,
        page_type: crate::models::HomeBrowsePageType,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum QueuePlaylistSource {
    Remote,
    Offline,
}

/// Track-row left-click behavior (views spec §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackRowClickBehavior {
    PlayFromContext,
    StartRadio,
}

/// Generic remote-data slot used by per-view states.
#[derive(Debug, Clone, Default)]
pub enum LoadState<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

// ---------------------------------------------------------------------------
// PlayerModel — the daemon mirror (GTK PlayerState minus widgets)
// ---------------------------------------------------------------------------

pub struct PlayerModel {
    /// Connection to staticd. Clone it into blocking tasks for every command.
    pub daemon: DaemonClient,
    /// Last snapshot returned by any daemon command or poll (daemon truth).
    pub snapshot: PlaybackSnapshot,

    // Local queue mirror (reconciled from snapshots; optimistically updated
    // by queue actions for responsiveness).
    pub queue: Vec<Track>,
    pub queue_index: usize,
    pub playlist_context: Option<PlaylistContext>,
    pub playlist_like_queue: bool,
    pub radio_virtual_playlist: Option<RadioVirtualPlaylist>,
    pub radio_append_in_flight: bool,
    pub last_radio_append_anchor: Option<String>,
    pub repeat_mode: RepeatMode,

    // Transport UI state.
    pub volume: f64,
    pub is_seeking: bool,
    /// Slider fraction shown while the user is dragging the progress slider.
    pub seek_preview: Option<f64>,

    // Play tracking.
    pub now_playing: Option<NowPlayingTrack>,
    pub playback_session_id: u64,
    pub play_request_serial: u64,

    // Poll bookkeeping.
    pub daemon_refresh_in_flight: bool,
    pub daemon_refresh_started_at: Option<Instant>,
    pub daemon_refresh_serial: u64,
    pub daemon_recovery_in_flight: bool,
    pub daemon_recovery_started_at: Option<Instant>,
    pub play_request_in_flight: Option<u64>,
    pub play_request_started_at: Option<Instant>,
}

impl PlayerModel {
    fn new(daemon: DaemonClient) -> Self {
        let snapshot = daemon.state().unwrap_or_default();
        let volume = snapshot.volume;
        Self {
            daemon,
            snapshot,
            queue: Vec::new(),
            queue_index: 0,
            playlist_context: None,
            playlist_like_queue: false,
            radio_virtual_playlist: None,
            radio_append_in_flight: false,
            last_radio_append_anchor: None,
            repeat_mode: RepeatMode::Off,
            volume,
            is_seeking: false,
            seek_preview: None,
            now_playing: None,
            playback_session_id: 0,
            play_request_serial: 0,
            daemon_refresh_in_flight: false,
            daemon_refresh_started_at: None,
            daemon_refresh_serial: 0,
            daemon_recovery_in_flight: false,
            daemon_recovery_started_at: None,
            play_request_in_flight: None,
            play_request_started_at: None,
        }
    }

    // --- shared helpers ported from GTK shell §11 ---

    pub fn is_radio_queue(&self) -> bool {
        self.radio_virtual_playlist.is_some()
    }

    pub fn has_playlist_like_queue(&self) -> bool {
        self.playlist_context.is_some() || self.playlist_like_queue
    }

    pub fn can_jump_to_radio(&self) -> bool {
        self.is_radio_queue() || self.has_playlist_like_queue() || !self.queue.is_empty()
    }

    pub fn radio_chips(&self) -> Vec<RadioChip> {
        self.radio_virtual_playlist
            .as_ref()
            .map(|vp| vp.chips.clone())
            .unwrap_or_default()
    }

    pub fn selected_radio_chip_index(&self) -> Option<usize> {
        self.radio_virtual_playlist
            .as_ref()
            .and_then(|vp| vp.selected_chip_index)
    }

    pub fn radio_touched_len(&self) -> usize {
        self.radio_virtual_playlist
            .as_ref()
            .map(|vp| vp.touched_len)
            .unwrap_or(0)
    }

    /// Radio boundary rule: anything the user played/touched becomes part of
    /// the intentional prefix — `touched_len` may only grow.
    pub fn mark_radio_touched_through(&mut self, index: usize) {
        if let Some(vp) = self.radio_virtual_playlist.as_mut() {
            vp.touched_len = vp.touched_len.max(index + 1);
        }
    }

    /// Sets all three queue-context fields and clears the radio append guards.
    pub fn set_queue_context(
        &mut self,
        playlist_context: Option<PlaylistContext>,
        radio: Option<RadioVirtualPlaylist>,
        playlist_like_queue: bool,
    ) {
        self.playlist_context = playlist_context;
        self.radio_virtual_playlist = radio;
        self.playlist_like_queue = playlist_like_queue;
        self.radio_append_in_flight = false;
        self.last_radio_append_anchor = None;
    }

    /// Bump and return the play-request serial; stale next/prev/play responses
    /// are dropped via `is_current_play_request`.
    pub fn next_play_request_serial(&mut self) -> u64 {
        self.play_request_serial += 1;
        self.play_request_serial
    }

    pub fn is_current_play_request(&self, serial: u64) -> bool {
        self.play_request_serial == serial
    }

    pub fn mark_play_request_in_flight(&mut self, serial: u64) {
        self.play_request_in_flight = Some(serial);
        self.play_request_started_at = Some(Instant::now());
    }

    pub fn clear_play_request_in_flight(&mut self, serial: u64) {
        if self.play_request_in_flight == Some(serial) {
            self.play_request_in_flight = None;
            self.play_request_started_at = None;
        }
    }

    /// True when a guard flag has outlived any possible completion message
    /// (see [`GUARD_FLAG_STALE_AFTER`]); `None` counts as stale so a flag can
    /// never wedge just because its timestamp went missing.
    fn guard_flag_is_stale(started_at: Option<Instant>) -> bool {
        started_at
            .map(|t| t.elapsed() >= GUARD_FLAG_STALE_AFTER)
            .unwrap_or(true)
    }

    pub fn position_millis(&self) -> Option<u64> {
        self.snapshot.position_millis
    }

    pub fn duration_millis(&self) -> Option<u64> {
        self.snapshot.duration_millis
    }

    pub fn buffered_fraction(&self) -> f64 {
        self.snapshot.buffered_fraction
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum Message {
    // Shell timers / daemon poll.
    PollTick,
    DaemonState {
        serial: u64,
        result: Result<PlaybackSnapshot, String>,
    },
    HeartbeatTick,
    HeartbeatFinished(Result<(), String>),
    HistoryTick,

    // Window / keyboard shell behavior.
    CloseRequested(iced::window::Id),
    ClosePersisted(iced::window::Id),
    /// External `org.freedesktop.Application.Activate` (daemon reopen / dock
    /// click): surface and focus the existing window.
    RaiseRequested,
    FocusSearch,
    RefreshHome,
    ContentScrolled {
        offset_y: f32,
        viewport_height: f32,
        content_height: f32,
    },
    ToastExpired {
        serial: u64,
    },
    Noop,

    // Per-area messages.
    Playback(player_bar::Msg),
    Sidebar(sidebar::Msg),
    Queue(queue_panel::Msg),
    TrackMenu(queue_panel::MenuMsg),
    Home(views::home::Msg),
    Search(views::search::Msg),
    Browse(views::browse_detail::Msg),
    Library(views::library::Msg),
    Settings(views::settings::Msg),
    Auth(views::settings::AuthMsg),
    Thumbnail(thumbnails::Msg),
}

/// Run blocking work (daemon IPC / network / disk) off the UI thread and map
/// its output back into a `Message`. This is THE async convention of the app
/// — mirror of GTK's `glib::spawn_future_local` + `gio::spawn_blocking`.
pub fn blocking<T, F, M>(work: F, map: M) -> Task<Message>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
    M: Fn(T) -> Message + Send + Sync + 'static,
{
    Task::perform(
        async move { tokio::task::spawn_blocking(work).await },
        move |joined| match joined {
            Ok(value) => map(value),
            Err(err) => {
                crate::app_log!("[UI] blocking worker failed: {err}");
                Message::Noop
            }
        },
    )
}

/// Like [`blocking`], but a panicking worker delivers `on_panic` instead of
/// being swallowed as `Noop`. Use this for any task whose completion message
/// resets an in-flight guard (`auth.connecting`, `home.loading`,
/// `resume_in_flight`, `volume_send_in_flight`, daemon recovery, ...) so a
/// worker panic cannot permanently wedge that feature for the session.
pub fn blocking_or<T, F, M>(work: F, map: M, on_panic: Message) -> Task<Message>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
    M: Fn(T) -> Message + Send + Sync + 'static,
{
    Task::perform(
        async move { tokio::task::spawn_blocking(work).await },
        move |joined| match joined {
            Ok(value) => map(value),
            Err(err) => {
                crate::app_log!("[UI] blocking worker failed: {err}");
                on_panic.clone()
            }
        },
    )
}

/// Run blocking work with a UI-visible timeout. The worker may continue in
/// Tokio's blocking pool after the timeout, but the UI receives `on_timeout`
/// promptly and serial guards ignore any later stale completion.
pub fn blocking_or_timeout<T, F, M>(
    timeout: Duration,
    work: F,
    map: M,
    on_timeout: Message,
) -> Task<Message>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
    M: Fn(T) -> Message + Send + Sync + 'static,
{
    Task::perform(
        async move { tokio::time::timeout(timeout, tokio::task::spawn_blocking(work)).await },
        move |timed| match timed {
            Ok(Ok(value)) => map(value),
            Ok(Err(err)) => {
                crate::app_log!("[UI] blocking worker failed: {err}");
                on_timeout.clone()
            }
            Err(_) => {
                crate::app_log!("[UI] blocking worker timed out after {:?}", timeout);
                on_timeout.clone()
            }
        },
    )
}

/// Fire a message after a delay (debounces, toast expiry, retry backoff).
pub fn after(delay: Duration, message: Message) -> Task<Message> {
    Task::perform(async move { tokio::time::sleep(delay).await }, move |_| {
        message.clone()
    })
}

fn spawn_close_daemon_cleanup(daemon: DaemonClient, client_id: String, shutdown_daemon: bool) {
    let _ = std::thread::Builder::new()
        .name("static-ui-close-cleanup".to_owned())
        .spawn(move || {
            if let Err(err) = daemon.unregister_ui_client(&client_id) {
                crate::app_log!("[STATIC] failed to unregister UI client: {err}");
            }
            if shutdown_daemon {
                if let Err(err) = daemon.shutdown() {
                    crate::app_log!("[STATIC] failed to shut daemon down: {err}");
                }
            }
        });
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

pub struct App {
    pub screen: Screen,

    // Session / persistence (GTK AppState).
    pub client: Option<Arc<NetworkState>>,
    pub cache: Arc<Mutex<MusicCache>>,
    pub settings: AppSettings,
    pub ui_client_id: String,

    // Daemon mirror.
    pub player: PlayerModel,

    // Navigation.
    pub active_nav: NavItem,
    pub content_view: ContentView,
    pub browse_back_target: BrowseBackTarget,

    // Overlays.
    pub toast: Option<Toast>,
    pub toast_serial: u64,
    pub track_menu: Option<queue_panel::TrackMenu>,

    // Per-area state (owned by the respective implementation files).
    pub player_bar: player_bar::State,
    pub sidebar: sidebar::State,
    pub queue_panel: queue_panel::State,
    pub home: views::home::State,
    pub search: views::search::State,
    pub browse: views::browse_detail::State,
    pub library: views::library::State,
    pub settings_page: views::settings::State,
    pub auth: views::settings::AuthState,
    pub thumbnails: thumbnails::Cache,
    pub closing: bool,
}

impl App {
    /// Application boot (GTK `startup` + `build_window`, shell spec §1–2).
    pub fn boot() -> (Self, Task<Message>) {
        let _ = crate::logging::reset_session_log();

        let storage_root = crate::network::get_storage_root();
        let cache = MusicCache::new_with_storage_root(storage_root.as_deref())
            .expect("Failed to initialise music cache");
        let settings = cache.load_settings().unwrap_or_else(|err| {
            crate::app_log!("[SETTINGS] failed to load persisted settings: {err}");
            AppSettings::default()
        });

        if let Err(err) = crate::ipc::ensure_daemon_running() {
            crate::app_log!("[STATIC] failed to ensure playback daemon: {err}");
        }
        // Hard requirement (parity): the GUI cannot run without the daemon.
        let daemon = DaemonClient::new().expect("Failed to connect to staticd");

        let ui_client_id = format!("iced-{}", uuid::Uuid::new_v4());
        if let Err(err) = daemon.register_ui_client(&ui_client_id) {
            crate::app_log!("[STATIC] failed to register UI client with staticd: {err}");
        }

        // Onboarding skip rule (shell spec §2 step 10).
        let startup_saved_cookie = crate::network::get_cookie().filter(|c| !c.trim().is_empty());
        let has_cached_offline_library = cache
            .cached_ids()
            .map(|ids| !ids.is_empty())
            .unwrap_or(false)
            || cache
                .offline_playlists()
                .map(|p| !p.is_empty())
                .unwrap_or(false);
        let has_storage_root_evidence = storage_root
            .as_deref()
            .map(|r| !r.trim().is_empty())
            .unwrap_or(false)
            || has_cached_offline_library;
        let should_skip_onboarding = startup_saved_cookie.is_some() || has_storage_root_evidence;

        let mut app = App {
            screen: if should_skip_onboarding {
                Screen::Main
            } else {
                Screen::Onboarding
            },
            client: None,
            cache: Arc::new(Mutex::new(cache)),
            settings,
            ui_client_id,
            player: PlayerModel::new(daemon),
            active_nav: NavItem::Home,
            content_view: ContentView::Home,
            browse_back_target: BrowseBackTarget::Home,
            toast: None,
            toast_serial: 0,
            track_menu: None,
            player_bar: player_bar::State::default(),
            sidebar: sidebar::State::default(),
            queue_panel: queue_panel::State::default(),
            home: views::home::State::default(),
            search: views::search::State::default(),
            browse: views::browse_detail::State::default(),
            library: views::library::State::default(),
            settings_page: views::settings::State::default(),
            auth: views::settings::AuthState::default(),
            thumbnails: thumbnails::Cache::default(),
            closing: false,
        };

        let mut startup = vec![views::settings::startup_auth_restore_task(
            &app,
            startup_saved_cookie,
        )];
        if should_skip_onboarding {
            // Home loading starts immediately, before auth has been validated
            // (shell spec §6.2); falls back to the Offline view after 4 failed
            // attempts.
            startup.push(content_loading::bootstrap_home_task(&mut app));
        }

        (app, Task::batch(startup))
    }

    // -----------------------------------------------------------------------
    // Update
    // -----------------------------------------------------------------------

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.dispatch(message);
        // GTK parity: render_queue_panel made the panel visible on the first
        // queue render and never hid it again — latch visibility so a later
        // empty-queue snapshot keeps the (empty) panel instead of reflowing
        // the content area (also keeps the empty-queue drop target alive).
        if !self.player.queue.is_empty() {
            self.queue_panel.shown = true;
        }
        task
    }

    fn dispatch(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PollTick => self.poll_daemon(),
            Message::DaemonState { serial, result } => self.handle_daemon_state(serial, result),

            Message::HeartbeatTick => {
                let daemon = self.player.daemon.clone();
                let client_id = self.ui_client_id.clone();
                blocking(
                    move || {
                        daemon
                            .register_ui_client(&client_id)
                            .map_err(|err| err.to_string())
                    },
                    Message::HeartbeatFinished,
                )
            }
            Message::HeartbeatFinished(result) => {
                if let Err(err) = result {
                    crate::app_log!(
                        "[STATIC] failed to refresh UI client lease with staticd: {err}"
                    );
                }
                Task::none()
            }

            Message::HistoryTick => {
                if self.screen == Screen::Main && self.client.is_some() {
                    sidebar::load_history_task(self)
                } else {
                    Task::none()
                }
            }

            Message::CloseRequested(window) => {
                if self.closing {
                    return iced::window::close(window);
                }
                self.closing = true;
                // Shell spec §9: persist resume snapshot, release the UI
                // lease, shut the daemon down only when paused with a track.
                // The snapshot payload is built on the UI thread. Only resume
                // persistence blocks window close; daemon IPC cleanup is
                // best-effort so a wedged daemon cannot pin the UI open.
                let resume_payload = player_bar::resume_persist_payload(self);
                let snapshot = self.player.snapshot.clone();
                let shutdown_daemon = snapshot.paused && snapshot.current_track.is_some();
                let daemon = self.player.daemon.clone();
                let client_id = self.ui_client_id.clone();
                spawn_close_daemon_cleanup(daemon, client_id, shutdown_daemon);
                blocking_or_timeout(
                    CLOSE_PERSIST_TIMEOUT,
                    move || {
                        if let Some(persisted) = resume_payload.as_ref() {
                            player_bar::write_resume_snapshot(persisted);
                        }
                    },
                    move |_| Message::ClosePersisted(window),
                    Message::ClosePersisted(window),
                )
            }
            Message::ClosePersisted(window) => iced::window::close(window),

            Message::RaiseRequested => {
                // Un-minimize, then bring to front and focus. On Wayland the
                // compositor may refuse self-focus, so also flag for attention
                // as a fallback (urgency hint / dock highlight).
                iced::window::oldest().and_then(|window| {
                    Task::batch([
                        iced::window::set_mode(window, iced::window::Mode::Windowed),
                        iced::window::gain_focus(window),
                        iced::window::request_user_attention(
                            window,
                            Some(iced::window::UserAttention::Informational),
                        ),
                    ])
                })
            }

            Message::FocusSearch => {
                // Ctrl+F (main page only): focus the search entry, select all.
                if self.screen != Screen::Main {
                    return Task::none();
                }
                Task::batch([
                    iced::widget::operation::focus(SEARCH_INPUT_ID),
                    iced::widget::operation::select_all(SEARCH_INPUT_ID),
                ])
            }

            Message::RefreshHome => {
                // Ctrl+R (main page, Home active only): drop home +
                // cross-device caches and reload with scroll reset.
                if self.screen != Screen::Main || self.active_nav != NavItem::Home {
                    return Task::none();
                }
                self.invalidate_home_cache();
                self.home.scroll_position = 0.0;
                content_loading::load_home_task(self, true)
            }

            Message::ContentScrolled {
                offset_y,
                viewport_height,
                content_height,
            } => self.handle_content_scrolled(offset_y, viewport_height, content_height),

            Message::ToastExpired { serial } => {
                if self.toast.as_ref().is_some_and(|t| t.serial == serial) {
                    self.toast = None;
                }
                Task::none()
            }

            Message::Noop => Task::none(),

            Message::Playback(msg) => player_bar::update(self, msg),
            Message::Sidebar(msg) => sidebar::update(self, msg),
            Message::Queue(msg) => queue_panel::update(self, msg),
            Message::TrackMenu(msg) => queue_panel::update_menu(self, msg),
            Message::Home(msg) => views::home::update(self, msg),
            Message::Search(msg) => views::search::update(self, msg),
            Message::Browse(msg) => views::browse_detail::update(self, msg),
            Message::Library(msg) => views::library::update(self, msg),
            Message::Settings(msg) => views::settings::update(self, msg),
            Message::Auth(msg) => views::settings::update_auth(self, msg),
            Message::Thumbnail(msg) => thumbnails::update(self, msg),
        }
    }

    /// 400 ms daemon poll (shell spec §10). Skips when a refresh is already in
    /// flight unless it looks stuck (≥ 3 s), drops stale responses by serial.
    fn poll_daemon(&mut self) -> Task<Message> {
        if self.player.play_request_in_flight.is_some() {
            if !PlayerModel::guard_flag_is_stale(self.player.play_request_started_at) {
                return Task::none();
            }
            crate::app_log!("[UI] play request completion never arrived; force-clearing the guard");
            self.player.play_request_in_flight = None;
            self.player.play_request_started_at = None;
        }
        if self.player.daemon_refresh_in_flight {
            let stuck = self
                .player
                .daemon_refresh_started_at
                .map(|t| t.elapsed() >= DAEMON_POLL_STUCK_AFTER)
                .unwrap_or(false);
            if !stuck {
                return Task::none();
            }
            crate::app_log!("[UI] daemon state poll timed out locally; retrying");
        }
        self.player.daemon_refresh_serial += 1;
        let serial = self.player.daemon_refresh_serial;
        self.player.daemon_refresh_in_flight = true;
        self.player.daemon_refresh_started_at = Some(Instant::now());

        let daemon = self.player.daemon.clone();
        blocking_or_timeout(
            DAEMON_TASK_TIMEOUT,
            move || daemon.state().map_err(|err| err.to_string()),
            move |result| Message::DaemonState { serial, result },
            Message::DaemonState {
                serial,
                result: Err("daemon state poll timed out".to_owned()),
            },
        )
    }

    fn handle_daemon_state(
        &mut self,
        serial: u64,
        result: Result<PlaybackSnapshot, String>,
    ) -> Task<Message> {
        if serial != self.player.daemon_refresh_serial {
            return Task::none(); // stale poll response
        }
        self.player.daemon_refresh_in_flight = false;
        self.player.daemon_refresh_started_at = None;

        match result {
            Err(err) => {
                crate::app_log!("[UI] daemon state poll failed: {err}");
                player_bar::maybe_restore_daemon_from_ui_state(self)
            }
            Ok(snapshot) => {
                if self.player.daemon_recovery_in_flight {
                    if !PlayerModel::guard_flag_is_stale(self.player.daemon_recovery_started_at) {
                        return Task::none();
                    }
                    // Observed in the field: a recovery whose completion
                    // message (even the timeout backstop) never arrived kept
                    // this flag set and silently dropped every poll snapshot
                    // until the app was restarted.
                    crate::app_log!(
                        "[UI] daemon recovery result never arrived; force-clearing the guard"
                    );
                    self.player.daemon_recovery_in_flight = false;
                    self.player.daemon_recovery_started_at = None;
                }
                player_bar::apply_remote_snapshot(self, snapshot)
            }
        }
    }

    fn handle_content_scrolled(
        &mut self,
        offset_y: f32,
        viewport_height: f32,
        content_height: f32,
    ) -> Task<Message> {
        match self.content_view {
            ContentView::Home => {
                self.home.scroll_position = offset_y.max(0.0);
                // Home infinite scroll (shell spec §6.5).
                let remaining = content_height - offset_y - viewport_height;
                if remaining <= HOME_INFINITE_SCROLL_THRESHOLD
                    && !self.home.loading
                    && self.home.continuation.is_some()
                {
                    return content_loading::load_more_home_task(self);
                }
                Task::none()
            }
            ContentView::Search => {
                if let Some(cache) = self.search.cache.as_mut() {
                    cache.scroll_position = offset_y.max(0.0);
                }
                Task::none()
            }
            _ => Task::none(),
        }
    }

    // -----------------------------------------------------------------------
    // Shared shell helpers (ported from GTK AppState accessors)
    // -----------------------------------------------------------------------

    pub fn app_client(&self) -> Option<Arc<NetworkState>> {
        self.client.clone()
    }

    /// Clears the home cache AND the cross-device queue (Ctrl+R, client swap).
    pub fn invalidate_home_cache(&mut self) {
        self.home.page = None;
        self.home.cross_device_queue = None;
    }

    /// Called right before entering any browse detail page (shell spec §3.2):
    /// captures the originating view (Home or the exact search incl. scroll)
    /// so the browse back button can return there.
    pub fn prepare_browse_back_target(&mut self) {
        let kind = selected_browse_back_target_kind(
            self.content_view.kind(),
            &self.browse_back_target,
            self.search.cache.is_some(),
        );
        self.browse_back_target = match kind {
            ContentViewKind::Search => match self.search.cache.clone() {
                Some(cache) => BrowseBackTarget::Search(cache),
                None => BrowseBackTarget::Home,
            },
            _ => BrowseBackTarget::Home,
        };
        self.content_view = ContentView::BrowseDetail;
    }

    /// Show a transient toast over the content area; older hide timers are
    /// invalidated by serial.
    pub fn show_toast(&mut self, message: impl Into<String>) -> Task<Message> {
        self.toast_serial += 1;
        let serial = self.toast_serial;
        self.toast = Some(Toast {
            text: message.into(),
            serial,
        });
        after(TOAST_DURATION, Message::ToastExpired { serial })
    }

    // -----------------------------------------------------------------------
    // Subscriptions
    // -----------------------------------------------------------------------

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            iced::time::every(DAEMON_POLL_INTERVAL).map(|_| Message::PollTick),
            iced::time::every(UI_CLIENT_HEARTBEAT_INTERVAL).map(|_| Message::HeartbeatTick),
            iced::time::every(SIDEBAR_HISTORY_INTERVAL).map(|_| Message::HistoryTick),
            iced::window::close_requests().map(Message::CloseRequested),
            // Remote activation (daemon reopen / GNOME dock) -> raise window.
            super::raise_subscription(),
            // Press-filtered shortcut listener: non-shortcut keyboard events
            // produce no message at all (no per-keystroke Noop dispatch).
            iced::event::listen_with(map_keyboard_event),
        ];
        if self.queue_panel.drag.is_some() {
            subs.push(
                iced::time::every(QUEUE_AUTOSCROLL_INTERVAL)
                    .map(|_| Message::Queue(queue_panel::Msg::AutoscrollTick)),
            );
            // GTK DragSource always fired `drag_end` wherever the button was
            // released. Catch left-button releases no widget handled (content
            // padding, header, player bar, sidebar whitespace, ...) so a drag
            // can never stay pending and later fire a stale drop. Releases
            // that a row/card/panel handler consumed resolve there first; the
            // abort then sees no drag and is a no-op.
            subs.push(iced::event::listen_with(map_drag_release_event));
        }
        Subscription::batch(subs)
    }

    // -----------------------------------------------------------------------
    // View
    // -----------------------------------------------------------------------

    pub fn view(&self) -> Element<'_, Message> {
        match self.screen {
            Screen::Onboarding => views::settings::onboarding_view(self),
            Screen::VrAuth => views::settings::vr_auth_view(self),
            Screen::Main => self.main_view(),
        }
    }

    /// Main page composition (shell spec §5): fixed 240 px sidebar | content
    /// column (header / scrollable content + overlays / now-playing bar) |
    /// queue panel (hidden until a queue exists).
    fn main_view(&self) -> Element<'_, Message> {
        let sidebar = container(sidebar::view(self))
            .width(Length::Fixed(theme::SIDEBAR_WIDTH))
            .height(Length::Fill)
            .style(theme::sidebar);

        let content_scroll = scrollable(container(views::view(self)).padding(iced::Padding {
            top: theme::SPACE_PAGE,
            bottom: theme::SPACE_PAGE,
            left: theme::SPACE_PAGE_H,
            right: theme::SPACE_PAGE_H,
        }))
        .id(CONTENT_SCROLL_ID)
        .style(theme::thin_scrollbar)
        .width(Length::Fill)
        .height(Length::Fill)
        .on_scroll(|viewport: scrollable::Viewport| Message::ContentScrolled {
            offset_y: viewport.absolute_offset().y,
            viewport_height: viewport.bounds().height,
            content_height: viewport.content_bounds().height,
        });

        let mut content_layers = iced::widget::Stack::new()
            .width(Length::Fill)
            .height(Length::Fill)
            .push(content_scroll);
        if let Some(toast) = &self.toast {
            content_layers = content_layers.push(
                container(widgets::toast(&toast.text))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Center)
                    .align_y(iced::alignment::Vertical::Bottom)
                    .padding(theme::SPACE_PAGE),
            );
        }

        let content_column = column![
            widgets::content_header(),
            widgets::separator(),
            content_layers,
            player_bar::view(self),
        ]
        .width(Length::Fill)
        .height(Length::Fill);

        let mut body = row![
            sidebar,
            container(content_column)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(theme::content_area),
        ];
        // Hidden until a queue first exists, then latched visible (GTK kept
        // the panel up once rendered, even if a later snapshot emptied it).
        if !self.player.queue.is_empty() || self.queue_panel.shown {
            body = body.push(
                container(queue_panel::view(self))
                    .width(Length::Fixed(theme::QUEUE_PANEL_WIDTH))
                    .height(Length::Fill)
                    .style(if self.queue_panel.drop_active {
                        theme::queue_panel_drop_active
                    } else {
                        theme::queue_panel
                    }),
            );
        }

        let body = body.width(Length::Fill).height(Length::Fill);

        // The track context menu is modal for the whole window (GTK popovers
        // auto-dismissed on any outside click anywhere): the dim + dismiss
        // overlay must cover the sidebar, queue panel and player bar too.
        if self.track_menu.is_some() {
            iced::widget::Stack::new()
                .width(Length::Fill)
                .height(Length::Fill)
                .push(body)
                .push(queue_panel::context_menu_layer(self))
                .into()
        } else {
            body.into()
        }
    }
}

/// Window-level keyboard shortcuts (shell spec §7.6). Screen gating happens in
/// `update` because this is a plain fn without access to state. Only ignored
/// (widget-uncaptured) Ctrl+F / Ctrl+R presses produce a message.
fn map_keyboard_event(
    event: iced::Event,
    status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    use iced::keyboard::{Event, Key};
    if status == iced::event::Status::Captured {
        return None;
    }
    if let iced::Event::Keyboard(Event::KeyPressed { key, modifiers, .. }) = event {
        if modifiers.command() {
            return match key.as_ref() {
                Key::Character("f") | Key::Character("F") => Some(Message::FocusSearch),
                Key::Character("r") | Key::Character("R") => Some(Message::RefreshHome),
                _ => None,
            };
        }
    }
    None
}

/// Global left-button release watcher, subscribed only while a queue drag is
/// pending/active: ends the drag wherever the release lands (GTK
/// `DragSource::connect_drag_end` parity). Handlers that consume the release
/// (queue rows, cards, the panel) run first via the widget tree; this catches
/// everything else.
fn map_drag_release_event(
    event: iced::Event,
    _status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
            Some(Message::Queue(queue_panel::Msg::DragAborted))
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "../tests/ui_app.rs"]
mod tests;
