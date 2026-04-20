use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use gstreamer::ClockTime;
use gtk::prelude::*;
use gtk::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, DragSource, DropDown, DropTarget,
    Entry, GestureClick, Image, Label, MessageDialog, MessageType, Orientation, Overlay, Paned,
    Picture, Popover, ResponseType, Scale, ScrolledWindow, Separator, SpinButton, Stack,
    StackTransitionType, Switch, gio, glib,
};

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

const ICON_PREV: &[u8] = include_bytes!("../../assets/icons/prev.svg");
const ICON_PLAY: &[u8] = include_bytes!("../../assets/icons/play.svg");
const ICON_PAUSE: &[u8] = include_bytes!("../../assets/icons/pause.svg");
const ICON_NEXT: &[u8] = include_bytes!("../../assets/icons/next.svg");
const ICON_LIKE: &[u8] = include_bytes!("../../assets/icons/like.svg");
const ICON_DISLIKE: &[u8] = include_bytes!("../../assets/icons/dislike.svg");
const ICON_MENU: &[u8] = include_bytes!("../../assets/icons/menu.svg");
const ICON_VOLUME: &[u8] = include_bytes!("../../assets/icons/volume.svg");
const ICON_REPEAT: &[u8] = include_bytes!("../../assets/icons/repeat.svg");
const ICON_REPEAT_ONE: &[u8] = include_bytes!("../../assets/icons/repeat_one.svg");
const ICON_SHUFFLE: &[u8] = include_bytes!("../../assets/icons/shuffle.svg");
const ICON_DOWNLOAD: &[u8] = include_bytes!("../../assets/icons/download.svg");

const SIDEBAR_WIDTH: i32 = 240;
const SIDEBAR_RECENT_CONTENT_WIDTH: i32 = SIDEBAR_WIDTH - 32;
const SIDEBAR_HISTORY_LIMIT: usize = 200;
const OFFLINE_LISTEN_AGAIN_PLAYLIST_ID: &str = "__offline_listen_again__";

use crate::ipc::{
    ActiveTrackSnapshot, DaemonClient, PersistedPlaybackState, PlaybackSnapshot, PlaylistContext,
    PlaylistContextSource, RepeatMode, current_unix_millis, load_persisted_playback_state,
    load_persisted_playlist_state, store_persisted_playback_state, store_persisted_playlist_state,
};
use crate::models::{
    Artist, BrowseSectionsPage, HomeBrowseCard, HomeBrowsePageType, HomeItem, HomePage,
    HomeSection, PlaylistSummary, RadioChip, RadioVirtualPlaylist, SearchResult, SyncedLyricLine,
    SyncedLyrics, SyncedLyricsSource, Track,
};
use crate::network::{
    SPONSORBLOCK_CATEGORY_OPTIONS, SongRating, SponsorBlockClient, SponsorSegment,
    default_skip_categories, delete_cookie, download_media_bytes,
    fetch_thumbnail_bytes as network_fetch_thumbnail_bytes, get_cookie, get_storage_root,
    load_cookie_from_chromium_profile, sanitize_categories, store_cookie, store_vr_refresh_token,
};
use crate::network_state::NetworkState;
use crate::offline::{AppSettings, MusicCache, OtherMprisMode};
use crate::player::Player;

static THUMBNAIL_BYTES_CACHE: LazyLock<Mutex<HashMap<String, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
    static THUMBNAIL_TEXTURE_CACHE: RefCell<HashMap<(String, i32), gtk::gdk::Texture>> =
        RefCell::new(HashMap::new());
}
mod auth;
mod content_loading;
mod detail_panel;
mod library;
mod page_builders;
mod playback;
mod queue_actions;
mod queue_drag;
mod settings_support;
mod thumbnails;
mod views;
mod widgets;

use self::auth::*;
use self::content_loading::*;
use self::detail_panel::*;
use self::library::*;
use self::page_builders::*;
use self::playback::*;
use self::queue_actions::*;
use self::queue_drag::*;
use self::settings_support::*;
use self::thumbnails::*;
use self::views::*;
use self::widgets::*;

struct AppState {
    client: Option<Arc<NetworkState>>,
    cache: MusicCache,
    settings: AppSettings,
    home_cache: Option<HomePage>,
    home_scroll_position: f64,
    search_cache: Option<SearchPageCache>,
    content_view: ContentViewKind,
    browse_back_target: BrowseBackTarget,
}

#[derive(Debug, Clone)]
struct SearchPageCache {
    query: String,
    results: SearchResult,
    scroll_position: f64,
}

#[derive(Debug, Clone)]
enum BrowseBackTarget {
    Home,
    Search(SearchPageCache),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentViewKind {
    Home,
    Search,
    BrowseDetail,
}

/// Tracks the currently playing song so we can record listen time when navigating away.
struct NowPlayingTrack {
    session_id: u64,
    video_id: String,
    rating: SongRating,
    last_observed_position_millis: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailPanelView {
    Queue,
    Lyrics,
}

impl DetailPanelView {
    fn stack_name(self) -> &'static str {
        match self {
            DetailPanelView::Queue => "queue",
            DetailPanelView::Lyrics => "lyrics",
        }
    }
}

fn app_client(state: &Arc<Mutex<AppState>>) -> Option<Arc<NetworkState>> {
    state.lock().unwrap().client.clone()
}

fn cached_home_page(state: &Arc<Mutex<AppState>>) -> Option<HomePage> {
    state.lock().unwrap().home_cache.clone()
}

fn store_home_cache(state: &Arc<Mutex<AppState>>, page: &HomePage) {
    state.lock().unwrap().home_cache = Some(page.clone());
}

fn invalidate_home_cache(state: &Arc<Mutex<AppState>>) {
    state.lock().unwrap().home_cache = None;
}

fn cached_home_scroll_position(state: &Arc<Mutex<AppState>>) -> f64 {
    state.lock().unwrap().home_scroll_position
}

fn store_home_scroll_position(state: &Arc<Mutex<AppState>>, position: f64) {
    state.lock().unwrap().home_scroll_position = position.max(0.0);
}

fn store_search_cache(
    state: &Arc<Mutex<AppState>>,
    query: &str,
    results: &SearchResult,
    scroll_position: f64,
) {
    let mut guard = state.lock().unwrap();
    guard.search_cache = Some(SearchPageCache {
        query: query.to_string(),
        results: results.clone(),
        scroll_position: scroll_position.max(0.0),
    });
    guard.content_view = ContentViewKind::Search;
}

fn set_content_view(state: &Arc<Mutex<AppState>>, view: ContentViewKind) {
    state.lock().unwrap().content_view = view;
}

fn selected_browse_back_target_kind(
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

fn prepare_browse_back_target(state: &Arc<Mutex<AppState>>, content_scroll: &ScrolledWindow) {
    let scroll_position = content_scroll.vadjustment().value().max(0.0);
    let mut guard = state.lock().unwrap();
    let next_kind = selected_browse_back_target_kind(
        guard.content_view,
        &guard.browse_back_target,
        guard.search_cache.is_some(),
    );

    guard.browse_back_target = match next_kind {
        ContentViewKind::Search => {
            if let Some(cache) = guard.search_cache.as_mut() {
                cache.scroll_position = scroll_position;
                BrowseBackTarget::Search(cache.clone())
            } else {
                BrowseBackTarget::Home
            }
        }
        _ => BrowseBackTarget::Home,
    };
    guard.content_view = ContentViewKind::BrowseDetail;
}

fn current_browse_back_target(state: &Arc<Mutex<AppState>>) -> BrowseBackTarget {
    state.lock().unwrap().browse_back_target.clone()
}

#[allow(dead_code)]
struct PlayerState {
    player: Player,
    // Progress
    progress_slider: Scale,
    is_seeking: Cell<bool>,
    last_mpris_length_micros: Cell<Option<i64>>,
    content_dimmer: GtkBox,
    // Transport
    prev_btn: Button,
    play_pause_btn: Button,
    next_btn: Button,
    // Time
    time_label: Label,
    // Track info
    np_thumb: Picture,
    np_title: Label,
    np_repeat_loops_left: Label,
    np_artist: Label,
    // Actions
    dislike_btn: Button,
    like_btn: Button,
    menu_btn: Button,
    // Volume
    volume_slider: Scale,
    // Modes
    repeat_btn: Button,
    repeat_mode: Cell<RepeatMode>,
    // Queue / Radio
    queue: RefCell<Vec<Track>>,
    queue_index: Cell<usize>,
    playlist_context: RefCell<Option<PlaylistContext>>,
    playlist_like_queue: Cell<bool>,
    radio_virtual_playlist: RefCell<Option<RadioVirtualPlaylist>>,
    radio_append_in_flight: Cell<bool>,
    last_radio_append_anchor: RefCell<Option<String>>,
    queue_drag_source_index: Cell<Option<usize>>,
    queue_drag_hover_viewport_y: Cell<Option<f64>>,
    queue_autoscroll_velocity: Cell<f64>,
    queue_autoscroll_source: RefCell<Option<glib::SourceId>>,
    queue_drop_preview_slot: Cell<Option<usize>>,
    queue_drag_preview_payload: RefCell<Option<QueueDragPayload>>,
    queue_drop_indicator: GtkBox,
    queue_drop_indicator_thumb: Picture,
    queue_drop_indicator_title: Label,
    queue_drop_indicator_subtitle: Label,
    queue_drop_indicator_duration: Label,
    sponsor_segments: RefCell<Vec<SponsorSegment>>,
    sponsor_segments_reported: RefCell<HashSet<String>>,
    daemon_refresh_in_flight: Cell<bool>,
    daemon_refresh_started_at: Cell<Option<Instant>>,
    daemon_refresh_serial: Cell<u64>,
    daemon_recovery_in_flight: Cell<bool>,
    play_request_serial: Cell<u64>,
    // Queue panel widgets
    queue_scroll: ScrolledWindow,
    queue_list_box: GtkBox,
    chip_bar: GtkBox,
    queue_panel: GtkBox,
    queue_dismiss_btn: Button,
    detail_panel_stack: Stack,
    detail_queue_tab_btn: Button,
    detail_lyrics_tab_btn: Button,
    detail_panel_mode: Cell<DetailPanelView>,
    lyrics_status_label: Label,
    lyrics_meta_label: Label,
    lyrics_scroll: ScrolledWindow,
    lyrics_list_box: GtkBox,
    lyrics_lines: RefCell<Vec<SyncedLyricLine>>,
    lyrics_line_widgets: RefCell<Vec<Label>>,
    lyrics_active_line: Cell<Option<usize>>,
    lyrics_request_serial: Cell<u64>,
    // Play tracking — records listen time when navigating away
    now_playing_track: RefCell<Option<NowPlayingTrack>>,
    playback_session_id: Cell<u64>,
}

struct MainPageWidgets {
    page: GtkBox,
    content_box: GtkBox,
    content_scroll: ScrolledWindow,
    player_state: Rc<PlayerState>,
    recent_list: GtkBox,
    search_entry: Entry,
    home_btn: Button,
    offline_btn: Button,
    home_pagination: Rc<HomePaginationState>,
}

#[derive(Debug, Clone)]
enum DirectMusicUrlTarget {
    Video {
        video_id: String,
    },
    Browse {
        browse_id: String,
        page_type: HomeBrowsePageType,
    },
}

struct HomePaginationState {
    continuation: RefCell<Option<String>>,
    loading: Cell<bool>,
}

enum BrowseDetailData {
    Tracks(Vec<Track>),
    Sections(BrowseSectionsPage),
}

struct ArtistSections<'a> {
    top_songs: Option<&'a HomeSection>,
    remaining: Vec<&'a HomeSection>,
}

#[derive(Debug, Clone)]
struct QueuePreviewData {
    title: String,
    subtitle: String,
    thumbnail_url: Option<String>,
    duration: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum QueueDragPayload {
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
        page_type: HomeBrowsePageType,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum QueuePlaylistSource {
    Remote,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackRowClickBehavior {
    PlayFromContext,
    StartRadio,
}

pub fn build_window(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Static")
        .default_width(1532)
        .default_height(760)
        .build();

    let stack = Stack::new();
    stack.set_transition_type(StackTransitionType::None);
    stack.set_transition_duration(0);

    let storage_root = get_storage_root();
    let cache = MusicCache::new_with_storage_root(storage_root.as_deref())
        .expect("Failed to initialise music cache");
    let cache = cache;
    let settings = cache.load_settings().unwrap_or_else(|err| {
        eprintln!("[SETTINGS] failed to load persisted settings: {err}");
        AppSettings::default()
    });
    let state: Arc<Mutex<AppState>> = Arc::new(Mutex::new(AppState {
        client: None,
        cache,
        settings,
        home_cache: None,
        home_scroll_position: 0.0,
        search_cache: None,
        content_view: ContentViewKind::Home,
        browse_back_target: BrowseBackTarget::Home,
    }));

    let onboarding = build_onboarding_page(&stack, &state);
    let vr_auth = build_vr_auth_page(&stack, &state);
    let main_widgets = build_main_page(&stack, &state);
    let startup_saved_cookie = get_cookie().filter(|cookie| !cookie.trim().is_empty());
    let has_cached_offline_library = {
        let guard = state.lock().unwrap();
        !guard.cache.cached_ids().unwrap_or_default().is_empty()
            || !guard
                .cache
                .offline_playlists()
                .unwrap_or_default()
                .is_empty()
    };
    let has_storage_root_evidence = storage_root
        .as_deref()
        .is_some_and(|root| !root.trim().is_empty())
        || has_cached_offline_library;
    let should_skip_onboarding = startup_saved_cookie.is_some() || has_storage_root_evidence;

    stack.add_named(&onboarding, Some("onboarding"));
    stack.add_named(&vr_auth, Some("vr_auth"));
    stack.add_named(&main_widgets.page, Some("main"));
    stack.set_visible_child_name(if should_skip_onboarding {
        "main"
    } else {
        "onboarding"
    });

    {
        let ps = main_widgets.player_state.clone();
        let state = state.clone();
        window.connect_close_request(move |_| {
            let snapshot = ps.player.snapshot();
            persist_resume_snapshot_from_ui_state(&ps, &state);
            if snapshot.paused && snapshot.current_track.is_some() {
                let client = ps.player.daemon_client();
                if let Err(err) = client.shutdown() {
                    eprintln!("[STATIC] failed to stop paused staticd while closing window: {err}");
                }
            }
            glib::Propagation::Proceed
        });
    }

    window.set_child(Some(&stack));

    if should_skip_onboarding {
        bootstrap_home_startup_shell(&state, &main_widgets);
    }

    {
        let state = state.clone();
        let stack = stack.clone();
        let content_box = main_widgets.content_box.clone();
        let content_scroll = main_widgets.content_scroll.clone();
        let home_pagination = main_widgets.home_pagination.clone();
        let ps = main_widgets.player_state.clone();
        let recent_list = main_widgets.recent_list.clone();
        let search_entry = main_widgets.search_entry.clone();
        let home_btn = main_widgets.home_btn.clone();
        let refresh = gtk::EventControllerKey::new();
        refresh.connect_key_pressed(move |_, key, _, modifiers| {
            let main_is_visible = stack.visible_child_name().as_deref() == Some("main");
            let wants_search = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && (key == gtk::gdk::Key::f || key == gtk::gdk::Key::F);
            let wants_refresh = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && (key == gtk::gdk::Key::r || key == gtk::gdk::Key::R);
            if wants_search && main_is_visible {
                search_entry.grab_focus();
                search_entry.select_region(0, -1);
                return glib::Propagation::Stop;
            }
            if wants_refresh && main_is_visible && home_btn.has_css_class("nav-item-active") {
                invalidate_home_cache(&state);
                load_home(
                    &state,
                    &content_box,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                    true,
                );
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(refresh);
    }

    window.present();

    // Auto-load home + history when main page becomes visible
    {
        let state = state.clone();
        let content_box = main_widgets.content_box.clone();
        let content_scroll = main_widgets.content_scroll.clone();
        let home_pagination = main_widgets.home_pagination.clone();
        let ps = main_widgets.player_state.clone();
        let recent_list = main_widgets.recent_list.clone();
        let home_btn = main_widgets.home_btn.clone();
        let offline_btn = main_widgets.offline_btn.clone();

        stack.connect_notify_local(Some("visible-child-name"), move |stack, _| {
            if stack.visible_child_name().as_deref() == Some("main") {
                clear_box(&content_box);
                load_home_with_offline_fallback(
                    &state,
                    &content_box,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                    &home_btn,
                    &offline_btn,
                );
                if app_client(&state).is_some() {
                    load_sidebar_history(&state, &recent_list, &ps);
                } else {
                    set_recent_list_status(&recent_list, "Restoring history...");
                }
            }
        });
    }

    // Keep sidebar history fresh while the live app is open so
    // plays from other devices/clients eventually show up too.
    {
        let stack = stack.clone();
        let state = state.clone();
        let recent_list = main_widgets.recent_list.clone();
        let ps = main_widgets.player_state.clone();

        glib::timeout_add_seconds_local(60, move || {
            if stack.visible_child_name().as_deref() == Some("main") && app_client(&state).is_some()
            {
                load_sidebar_history(&state, &recent_list, &ps);
            }
            glib::ControlFlow::Continue
        });
    }

    // Check keyring on startup
    {
        let stack = stack.clone();
        let state = state.clone();
        let ps = main_widgets.player_state.clone();
        let recent_list = main_widgets.recent_list.clone();
        let startup_saved_cookie = startup_saved_cookie.clone();
        glib::spawn_future_local(async move {
            if let Some(cookie) = startup_saved_cookie {
                match validate_cookie_and_build_client(cookie, "keyring cookie").await {
                    CookieValidationOutcome::Authenticated(client) => {
                        let main_was_visible =
                            stack.visible_child_name().as_deref() == Some("main");
                        activate_authenticated_client(client, &state, &stack).await;
                        if main_was_visible {
                            refresh_main_after_client_restore(&state, &recent_list, &ps);
                        }
                        return;
                    }
                    CookieValidationOutcome::Rejected => {
                        let _ = gio::spawn_blocking(delete_cookie).await;
                    }
                    CookieValidationOutcome::Deferred(client) => {
                        stage_restored_client(client, &state).await;
                        if stack.visible_child_name().as_deref() == Some("main") {
                            refresh_main_after_client_restore(&state, &recent_list, &ps);
                        }
                    }
                }
            }

            let settings = app_settings(&state);
            if !settings.auto_sync_chromium_cookies {
                return;
            }

            match gio::spawn_blocking(move || load_cookie_from_chromium_profile(&settings)).await {
                Ok(Ok(Some(edge_cookie))) => {
                    match validate_cookie_and_build_client(
                        edge_cookie.clone(),
                        "Chromium cookie auto-sync",
                    )
                    .await
                    {
                        CookieValidationOutcome::Authenticated(client) => {
                            let cookie_to_store = edge_cookie.clone();
                            let _ =
                                gio::spawn_blocking(move || store_cookie(&cookie_to_store)).await;
                            let main_was_visible =
                                stack.visible_child_name().as_deref() == Some("main");
                            activate_authenticated_client(client, &state, &stack).await;
                            if main_was_visible {
                                refresh_main_after_client_restore(&state, &recent_list, &ps);
                            }
                        }
                        CookieValidationOutcome::Rejected => {}
                        CookieValidationOutcome::Deferred(client) => {
                            stage_restored_client(client, &state).await;
                            if stack.visible_child_name().as_deref() == Some("main") {
                                refresh_main_after_client_restore(&state, &recent_list, &ps);
                            }
                        }
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(err)) => {
                    eprintln!("[AUTH] Chromium cookie auto-sync failed: {err}");
                }
                Err(err) => {
                    eprintln!("[AUTH] Chromium cookie auto-sync worker failed: {err:?}");
                }
            }
        });
    }
}

fn set_recent_list_status(recent_list: &GtkBox, message: &str) {
    clear_box(recent_list);
    let label = Label::new(Some(message));
    label.add_css_class("sidebar-dim");
    label.set_margin_top(8);
    recent_list.append(&label);
}

fn bootstrap_home_startup_shell(state: &Arc<Mutex<AppState>>, widgets: &MainPageWidgets) {
    widgets.offline_btn.remove_css_class("nav-item-active");
    widgets.home_btn.add_css_class("nav-item-active");
    set_recent_list_status(&widgets.recent_list, "Restoring history...");
    load_home_with_offline_fallback(
        state,
        &widgets.content_box,
        &widgets.content_scroll,
        &widgets.home_pagination,
        &widgets.player_state,
        &widgets.recent_list,
        &widgets.home_btn,
        &widgets.offline_btn,
    );
}

fn refresh_main_after_client_restore(
    state: &Arc<Mutex<AppState>>,
    recent_list: &GtkBox,
    ps: &Rc<PlayerState>,
) {
    load_sidebar_history(state, recent_list, ps);
}

// ============================================================
// ONBOARDING PAGE
// ============================================================

fn is_radio_queue(ps: &PlayerState) -> bool {
    ps.radio_virtual_playlist.borrow().is_some()
}

fn has_playlist_like_queue(ps: &PlayerState) -> bool {
    ps.playlist_context.borrow().is_some() || ps.playlist_like_queue.get()
}

fn can_jump_to_radio(ps: &PlayerState) -> bool {
    is_radio_queue(ps) || has_playlist_like_queue(ps) || !ps.queue.borrow().is_empty()
}

fn radio_chips(ps: &PlayerState) -> Vec<RadioChip> {
    ps.radio_virtual_playlist
        .borrow()
        .as_ref()
        .map(|radio| radio.chips.clone())
        .unwrap_or_default()
}

fn radio_source_video_id(ps: &PlayerState) -> Option<String> {
    ps.radio_virtual_playlist
        .borrow()
        .as_ref()
        .map(|radio| radio.source_video_id.clone())
}

fn playlist_context(
    playlist_id: &str,
    title: &str,
    thumbnail_url: Option<&str>,
    subtitle: Option<&str>,
    source: PlaylistContextSource,
) -> PlaylistContext {
    PlaylistContext {
        playlist_id: playlist_id.to_string(),
        title: title.to_string(),
        thumbnail_url: thumbnail_url.map(str::to_string),
        subtitle: subtitle.map(str::to_string),
        source,
    }
}

fn set_queue_context(
    ps: &PlayerState,
    playlist_context: Option<PlaylistContext>,
    radio: Option<RadioVirtualPlaylist>,
    playlist_like_queue: bool,
) {
    *ps.playlist_context.borrow_mut() = playlist_context;
    ps.playlist_like_queue.set(playlist_like_queue);
    *ps.radio_virtual_playlist.borrow_mut() = radio;
    ps.radio_append_in_flight.set(false);
    *ps.last_radio_append_anchor.borrow_mut() = None;
}

fn mark_radio_touched_through(ps: &PlayerState, index: usize) {
    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        radio.touched_len = radio.touched_len.max(index.saturating_add(1));
    }
}

fn extend_radio_touched_for_insert(ps: &PlayerState, insert_at: usize, added: usize) {
    if added == 0 {
        return;
    }

    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        let touched_len = radio.touched_len;
        let inserted_end = insert_at + added;
        if insert_at < touched_len {
            radio.touched_len = touched_len + added;
        } else {
            radio.touched_len = touched_len.max(inserted_end);
        }
    }
}

fn radio_touched_len_after_reorder(touched_len: usize, from: usize, insert_at: usize) -> usize {
    let mut touched_len = touched_len;

    if from < touched_len {
        touched_len = touched_len.saturating_sub(1);
    }

    if insert_at < touched_len {
        touched_len + 1
    } else {
        touched_len.max(insert_at.saturating_add(1))
    }
}

fn update_radio_touched_for_reorder(ps: &PlayerState, from: usize, insert_at: usize) {
    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        radio.touched_len = radio_touched_len_after_reorder(radio.touched_len, from, insert_at);
    }
}

fn shrink_radio_touched_for_remove(ps: &PlayerState, remove_at: usize) {
    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        if radio.touched_len == 0 {
            return;
        }

        if remove_at < radio.touched_len {
            radio.touched_len = radio.touched_len.saturating_sub(1);
        }
    }
}

fn radio_touched_len(ps: &PlayerState) -> Option<usize> {
    ps.radio_virtual_playlist
        .borrow()
        .as_ref()
        .map(|radio| radio.touched_len)
}

fn dedupe_radio_recommendations(prefix: &[Track], tracks: Vec<Track>) -> Vec<Track> {
    let mut seen_ids = prefix
        .iter()
        .map(|track| track.video_id.clone())
        .collect::<HashSet<_>>();
    let mut deduped = Vec::with_capacity(tracks.len());

    for track in tracks {
        if seen_ids.insert(track.video_id.clone()) {
            deduped.push(track);
        }
    }

    deduped
}

fn apply_radio_chip_update(radio: &mut RadioVirtualPlaylist, chips: Option<Vec<RadioChip>>) {
    if let Some(chips) = chips {
        radio.chips = chips;
    }
}

fn replace_radio_tail(
    ps: &PlayerState,
    prefix_len: usize,
    tracks: Vec<Track>,
    chips: Option<Vec<RadioChip>>,
) {
    let prefix_len = prefix_len.min(ps.queue.borrow().len());
    {
        let mut queue = ps.queue.borrow_mut();
        queue.truncate(prefix_len);
        let deduped_tracks = dedupe_radio_recommendations(queue.as_slice(), tracks);
        queue.extend(deduped_tracks);
    }

    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        radio.touched_len = radio.touched_len.min(prefix_len);
        apply_radio_chip_update(radio, chips);
    }
    *ps.last_radio_append_anchor.borrow_mut() = None;
}

fn append_radio_recommendations(ps: &PlayerState, tracks: Vec<Track>) -> usize {
    let mut queue = ps.queue.borrow_mut();
    let deduped_tracks = dedupe_radio_recommendations(queue.as_slice(), tracks);
    let added = deduped_tracks.len();
    queue.extend(deduped_tracks);
    added
}

fn maybe_extend_radio_queue(state: &Arc<Mutex<AppState>>, ps: &Rc<PlayerState>) {
    if !is_radio_queue(ps)
        || ps.radio_append_in_flight.get()
        || ps.repeat_mode.get() == RepeatMode::All
    {
        return;
    }

    let last_video_id = {
        let queue = ps.queue.borrow();
        let idx = ps.queue_index.get();
        if queue.is_empty() || idx + 1 != queue.len() {
            return;
        }
        queue.last().map(|track| track.video_id.clone())
    };
    let Some(last_video_id) = last_video_id else {
        return;
    };

    if ps.last_radio_append_anchor.borrow().as_deref() == Some(last_video_id.as_str()) {
        return;
    }

    *ps.last_radio_append_anchor.borrow_mut() = Some(last_video_id.clone());
    ps.radio_append_in_flight.set(true);

    let state = state.clone();
    let ps = ps.clone();
    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let vid = last_video_id.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_radio(&vid, None, None))
        })
        .await;

        let should_apply = is_radio_queue(&ps)
            && ps.repeat_mode.get() != RepeatMode::All
            && ps.last_radio_append_anchor.borrow().as_deref() == Some(last_video_id.as_str())
            && ps
                .queue
                .borrow()
                .last()
                .is_some_and(|track| track.video_id == last_video_id);
        if !should_apply {
            if ps.last_radio_append_anchor.borrow().as_deref() == Some(last_video_id.as_str()) {
                *ps.last_radio_append_anchor.borrow_mut() = None;
            }
            ps.radio_append_in_flight.set(false);
            return;
        }

        if let Ok(Some(Ok(radio))) = result {
            if append_radio_recommendations(&ps, radio.tracks) > 0 {
                render_queue_panel(&ps, &state);
                sync_remote_queue(&ps);
                sync_mpris_transport(&ps, &state);
            }
        }

        ps.radio_append_in_flight.set(false);
    });
}

/// Set the queue from a track list context and play from the given index.
fn play_from_context(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    tracks: &[Track],
    index: usize,
    playlist_context: Option<PlaylistContext>,
) {
    *ps.queue.borrow_mut() = tracks.to_vec();
    ps.queue_index.set(index);
    set_queue_context(ps, playlist_context, None, false);
    play_from_queue(state, ps, index);
}

fn reset_player_ui(ps: &PlayerState, state: &Arc<Mutex<AppState>>) {
    let _ = state;
    let _ = ps.player.stop();
    clear_sponsorblock_state(ps);
    ps.playlist_like_queue.set(false);
    ps.last_mpris_length_micros.set(None);
    ps.lyrics_request_serial
        .set(ps.lyrics_request_serial.get().wrapping_add(1));
    show_lyrics_idle(ps);
    ps.np_title.set_text("Nothing playing");
    ps.np_artist.set_text("");
    ps.np_thumb.set_paintable(None::<&gtk::gdk::Texture>);
    ps.time_label.set_text("0:00 / 0:00");
    ps.progress_slider.set_value(0.0);
    ps.progress_slider.set_fill_level(0.0);
    set_button_icon(&ps.play_pause_btn, ICON_PLAY, 28);
    ps.play_pause_btn.set_sensitive(false);
    ps.prev_btn.set_sensitive(false);
    ps.next_btn.set_sensitive(false);
    ps.like_btn.set_sensitive(false);
    ps.dislike_btn.set_sensitive(false);
    update_rating_buttons_ui(&ps.like_btn, &ps.dislike_btn, SongRating::Indifferent);
    ps.menu_btn.set_sensitive(false);
}

fn format_clock_time(t: ClockTime) -> String {
    let total_secs = t.seconds();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}:{secs:02}")
}

fn apply_seek_position(ps: &Rc<PlayerState>, target: ClockTime, state: &Arc<Mutex<AppState>>) {
    let clamped_target = ps
        .player
        .duration()
        .map(|duration| ClockTime::from_nseconds(target.nseconds().min(duration.nseconds())))
        .unwrap_or(target);

    ps.is_seeking.set(true);
    let _ = ps.player.seek(clamped_target);
    note_playback_position(ps, clamped_target.nseconds() / 1_000_000);

    if let Some(duration) = ps.player.duration() {
        let duration_ns = duration.nseconds();
        if duration_ns > 0 {
            let frac = clamped_target.nseconds() as f64 / duration_ns as f64;
            ps.progress_slider.set_value(frac.clamp(0.0, 1.0));
        }
        ps.time_label.set_text(&format!(
            "{} / {}",
            format_clock_time(clamped_target),
            format_clock_time(duration),
        ));
    }

    ps.is_seeking.set(false);
    update_active_lyric_line(ps, Some(clamped_target.nseconds() / 1_000_000));
    maybe_apply_sponsorblock_skip(state, ps, clamped_target);
}

fn clear_sponsorblock_state(ps: &PlayerState) {
    ps.sponsor_segments.borrow_mut().clear();
    ps.sponsor_segments_reported.borrow_mut().clear();
}

fn ensure_offline_sponsorblock_cache(
    state: &Arc<Mutex<AppState>>,
    video_id: &str,
) -> Result<Option<(Vec<SponsorSegment>, &'static str)>, String> {
    let all_categories = default_skip_categories();
    {
        let guard = state.lock().unwrap();
        if !guard.cache.is_cached(video_id) {
            return Ok(None);
        }

        match guard.cache.get_offline_sponsor_segments(video_id) {
            Ok(Some(segments)) => return Ok(Some((segments, "offline-cache"))),
            Ok(None) => {}
            Err(err) => {
                eprintln!("[SPONSORBLOCK] failed to read offline cache for {video_id}: {err}");
            }
        }
    }

    let client = SponsorBlockClient::new()
        .map_err(|err| format!("Failed to initialize SponsorBlock client for {video_id}: {err}"))?;

    let segments = client
        .fetch_skip_segments(video_id, &all_categories)
        .map_err(|err| {
            format!("Failed to hydrate offline SponsorBlock cache for {video_id}: {err}")
        })?;

    let guard = state.lock().unwrap();
    if let Err(err) = guard
        .cache
        .store_offline_sponsor_segments(video_id, &segments)
    {
        eprintln!("[SPONSORBLOCK] failed to store offline cache for {video_id}: {err}");
    }

    Ok(Some((segments, "offline-hydrated")))
}

fn refresh_sponsorblock_for_current_track(state: &Arc<Mutex<AppState>>, ps: &Rc<PlayerState>) {
    let _ = state;
    clear_sponsorblock_state(ps);
    crate::player::notify_daemon_settings_changed();
}

fn maybe_apply_sponsorblock_skip(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    position: ClockTime,
) {
    let _ = (state, ps, position);
}

fn apply_song_rating(ps: &Rc<PlayerState>, session_id: u64, rating: SongRating) {
    let mut now_playing = ps.now_playing_track.borrow_mut();
    let Some(track) = now_playing.as_mut() else {
        return;
    };
    if track.session_id != session_id {
        return;
    }
    track.rating = rating;
    update_rating_buttons_ui(&ps.like_btn, &ps.dislike_btn, rating);
}

fn spawn_song_rating_sync(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    video_id: &str,
    session_id: u64,
) {
    let state = state.clone();
    let ps = ps.clone();
    let video_id = video_id.to_string();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let video_id_bg = video_id.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_song_rating(&video_id_bg))
        })
        .await;

        match result {
            Ok(Some(Ok(rating))) => apply_song_rating(&ps, session_id, rating),
            Ok(Some(Err(err))) => {
                eprintln!("[RATING] failed to fetch rating for {video_id}: {err}")
            }
            _ => {}
        }
    });
}

fn spawn_synced_lyrics_sync(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    video_id: &str,
    session_id: u64,
) {
    let request_serial = ps.lyrics_request_serial.get().wrapping_add(1);
    ps.lyrics_request_serial.set(request_serial);
    show_lyrics_loading(ps);

    let state = state.clone();
    let ps = ps.clone();
    let video_id = video_id.to_string();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let video_id_bg = video_id.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_synced_lyrics(&video_id_bg))
        })
        .await;

        if !lyrics_request_is_current(&ps, session_id, request_serial, &video_id) {
            return;
        }

        match result {
            Ok(Some(Ok(Some(lyrics)))) => {
                let position_millis = ps
                    .player
                    .position()
                    .map(|position| position.nseconds() / 1_000_000)
                    .or_else(|| ps.player.snapshot().position_millis);
                render_synced_lyrics(&ps, &lyrics, position_millis);
            }
            Ok(Some(Ok(None))) => show_lyrics_status(
                &ps,
                Some("Synced lyrics unavailable"),
                "This track did not expose timed text, so there isn’t anything to sync yet.",
            ),
            Ok(Some(Err(err))) => {
                eprintln!("[LYRICS] failed to fetch synced lyrics for {video_id}: {err}");
                show_lyrics_status(
                    &ps,
                    Some("Lyrics fetch failed"),
                    "Timed lyrics could not be loaded for this track.",
                );
            }
            Ok(None) => show_lyrics_status(
                &ps,
                Some("Not connected"),
                "Connect YouTube Music to load synced lyrics.",
            ),
            Err(err) => {
                eprintln!("[LYRICS] timed lyric worker failed for {video_id}: {err:?}");
                show_lyrics_status(
                    &ps,
                    Some("Lyrics fetch failed"),
                    "Timed lyrics could not be loaded for this track.",
                );
            }
        }
    });
}

fn spawn_sponsorblock_sync(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    video_id: &str,
    session_id: u64,
) {
    let _ = (state, ps, video_id, session_id);
}

fn submit_song_rating(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    desired_rating: SongRating,
) {
    let Some((session_id, video_id, previous_rating)) = ps
        .now_playing_track
        .borrow()
        .as_ref()
        .map(|track| (track.session_id, track.video_id.clone(), track.rating))
    else {
        return;
    };

    apply_song_rating(ps, session_id, desired_rating);

    let state = state.clone();
    let ps = ps.clone();
    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let video_id_bg = video_id.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.rate_song(&video_id_bg, desired_rating))
        })
        .await;

        match result {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(err))) => {
                eprintln!("[RATING] failed to set rating for {video_id}: {err}");
                apply_song_rating(&ps, session_id, previous_rating);
            }
            _ => apply_song_rating(&ps, session_id, previous_rating),
        }
    });
}

fn maybe_flush_watchtime_progress(ps: &PlayerState) {
    let _ = ps;
}

// ============================================================
// DATA LOADING
// ============================================================

/// Restarts the scroll animation whenever the label text changes.

fn artist_string(track: &Track) -> String {
    if track.artists.is_empty() {
        "Unknown artist".to_string()
    } else {
        track
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn clear_box(container: &GtkBox) {
    container.remove_css_class("home-view-active");
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BrowseBackTarget, ContentViewKind, apply_radio_chip_update, dedupe_radio_recommendations,
        radio_touched_len_after_reorder, selected_browse_back_target_kind,
    };
    use crate::models::{Artist, RadioChip, RadioVirtualPlaylist, Track};

    fn test_track(video_id: &str) -> Track {
        Track {
            video_id: video_id.to_string(),
            title: video_id.to_string(),
            artists: vec![Artist {
                name: "Artist".to_string(),
                id: None,
            }],
            album: None,
            duration: None,
            thumbnail_url: None,
            playback_playlist_id: None,
            playback_params: None,
            not_interested_feedback_token: None,
            undo_feedback_token: None,
        }
    }

    fn test_chip(label: &str, playlist_id: &str, params: Option<&str>) -> RadioChip {
        RadioChip {
            label: label.to_string(),
            playlist_id: playlist_id.to_string(),
            params: params.map(str::to_string),
        }
    }

    #[test]
    fn reordering_generated_track_into_touched_prefix_extends_boundary() {
        assert_eq!(radio_touched_len_after_reorder(3, 5, 1), 4);
    }

    #[test]
    fn reordering_touched_track_farther_down_marks_drop_location() {
        assert_eq!(radio_touched_len_after_reorder(3, 1, 5), 6);
    }

    #[test]
    fn reordering_within_touched_prefix_preserves_boundary_width() {
        assert_eq!(radio_touched_len_after_reorder(4, 1, 2), 4);
    }

    #[test]
    fn radio_recommendations_drop_tracks_already_in_virtual_prefix() {
        let prefix = vec![test_track("seed"), test_track("already-touched")];
        let deduped = dedupe_radio_recommendations(
            &prefix,
            vec![
                test_track("already-touched"),
                test_track("fresh"),
                test_track("fresh"),
            ],
        );

        assert_eq!(deduped, vec![test_track("fresh")]);
    }

    #[test]
    fn radio_chip_update_preserves_existing_chips_when_not_replaced() {
        let existing_chips = vec![
            test_chip("All", "RDseed", None),
            test_chip("Discover", "RDdiscover", Some("discover-params")),
        ];
        let mut radio = RadioVirtualPlaylist {
            source_video_id: "seed".to_string(),
            touched_len: 1,
            chips: existing_chips.clone(),
        };

        apply_radio_chip_update(&mut radio, None);

        assert_eq!(radio.chips, existing_chips);
    }

    #[test]
    fn radio_chip_update_replaces_existing_chips_when_provided() {
        let mut radio = RadioVirtualPlaylist {
            source_video_id: "seed".to_string(),
            touched_len: 1,
            chips: vec![test_chip("All", "RDseed", None)],
        };
        let replacement_chips = vec![test_chip("J-pop", "RDjpop", Some("jpop-params"))];

        apply_radio_chip_update(&mut radio, Some(replacement_chips.clone()));

        assert_eq!(radio.chips, replacement_chips);
    }

    #[test]
    fn search_pages_capture_search_as_browse_back_target() {
        assert_eq!(
            selected_browse_back_target_kind(
                ContentViewKind::Search,
                &BrowseBackTarget::Home,
                true,
            ),
            ContentViewKind::Search
        );
    }

    #[test]
    fn nested_browse_pages_keep_existing_search_return_target() {
        assert_eq!(
            selected_browse_back_target_kind(
                ContentViewKind::BrowseDetail,
                &BrowseBackTarget::Search(super::SearchPageCache {
                    query: "rain by face".to_string(),
                    results: super::SearchResult::default(),
                    scroll_position: 320.0,
                }),
                true,
            ),
            ContentViewKind::Search
        );
    }

    #[test]
    fn non_search_pages_fall_back_to_home_back_target() {
        assert_eq!(
            selected_browse_back_target_kind(
                ContentViewKind::Home,
                &BrowseBackTarget::Search(super::SearchPageCache {
                    query: "rain by face".to_string(),
                    results: super::SearchResult::default(),
                    scroll_position: 320.0,
                }),
                true,
            ),
            ContentViewKind::Home
        );
    }
}
