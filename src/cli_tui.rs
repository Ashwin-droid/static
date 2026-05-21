use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Local;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use image::DynamicImage;
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Modifier, Style, Terminal};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Gauge, List, ListItem, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Widget, Wrap,
};
use ratatui_image::{
    Resize, StatefulImage,
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
};

use crate::cli::{
    apply_config_value, artist_string, format_config_value, format_millis, format_track_summary,
    load_local_resume_state, play_track_with_radio, restore_cross_device_queue_via_daemon,
    restore_paused_playback_snapshot_via_daemon, restore_playback_snapshot_via_daemon,
    resume_snapshot_meta, resume_snapshot_track, saved_network_state,
    should_use_local_resume_for_cross_device,
};
use crate::ipc::{
    ActiveTrackSnapshot, CoordinationTargetSnapshot, CoordinationTargetsSnapshot, DaemonClient,
    PersistedPlaybackState, PlaybackSnapshot, PlaylistContext, PlaylistContextSource, RepeatMode,
};
use crate::models::{
    BrowseSectionsPage, CrossDeviceQueue, HomeBrowseCard, HomeBrowsePageType, HomeItem, HomePage,
    PlaylistSummary, RadioChip, SearchResult, SyncedLyrics, Track,
};
use crate::network::{
    SPONSORBLOCK_CATEGORY_OPTIONS, SongRating, delete_storage_root, fetch_thumbnail_bytes,
    get_storage_root, store_storage_root,
};
use crate::offline::{AppSettings, CoordinationFilterMode, CoordinationMode, MusicCache};

const UI_TICK_INTERVAL: Duration = Duration::from_millis(250);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(400);
const MIN_WIDTH: u16 = 124;
const MIN_HEIGHT: u16 = 34;
const SIDEBAR_WIDTH: u16 = 30;
const DETAIL_WIDTH: u16 = 40;
const PLAYER_BAR_HEIGHT: u16 = 6;
const HOME_GAP: u16 = 2;
const RECENT_LIMIT: usize = 32;
const RECENT_ROW_HEIGHT: u16 = 2;
const QUEUE_ROW_HEIGHT: u16 = 3;
const HOME_LOAD_MORE_THRESHOLD_ROWS: usize = 14;
const HOME_HEADER_PLAY_ALL_LABEL: &str = "[▶ Play all]";
const HOME_HEADER_SHUFFLE_ALL_LABEL: &str = "[🔀 Shuffle]";
const HOME_BANNER_HEIGHT: u16 = 7;

mod actions;
mod content_loading;
mod context_menu;
mod detail_panel;
mod input;
mod lifecycle;
mod page_builders;
mod playback;
mod queue_drag;
mod rendering;
mod search;
mod settings_support;
mod thumbnails;
mod views;
mod widgets;

pub fn run() -> Result<()> {
    let client = DaemonClient::new()?;
    let mut app = InteractiveTui::new(client)?;
    app.run()
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("Failed to enable raw terminal mode")?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)
            .context("Failed to enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

#[derive(Clone)]
struct TuiTheme {
    canvas: Color,
    sidebar_bg: Color,
    content_bg: Color,
    content_alt_bg: Color,
    detail_bg: Color,
    player_bg: Color,
    elevated_bg: Color,
    elevated_alt_bg: Color,
    border: Color,
    border_subtle: Color,
    border_focus: Color,
    text: Color,
    muted: Color,
    accent: Color,
    accent_fill: Color,
    accent_glow: Color,
    red: Color,
    positive: Color,
    danger: Color,
    selected_bg: Color,
}

impl TuiTheme {
    fn from_snapshot(_snapshot: &PlaybackSnapshot) -> Self {
        Self {
            canvas: Color::Rgb(10, 11, 16),
            sidebar_bg: Color::Rgb(14, 14, 20),
            content_bg: Color::Rgb(14, 14, 20),
            content_alt_bg: Color::Rgb(18, 20, 28),
            detail_bg: Color::Rgb(26, 26, 26),
            player_bg: Color::Rgb(29, 29, 29),
            elevated_bg: Color::Rgb(21, 22, 31),
            elevated_alt_bg: Color::Rgb(24, 28, 42),
            border: Color::Rgb(52, 56, 73),
            border_subtle: Color::Rgb(34, 37, 48),
            border_focus: Color::Rgb(130, 160, 255),
            text: Color::Rgb(240, 240, 245),
            muted: Color::Rgb(150, 154, 170),
            accent: Color::Rgb(130, 160, 255),
            accent_fill: Color::Rgb(42, 51, 83),
            accent_glow: Color::Rgb(208, 221, 255),
            red: Color::Rgb(255, 0, 0),
            positive: Color::Rgb(127, 211, 161),
            danger: Color::Rgb(255, 120, 120),
            selected_bg: Color::Rgb(31, 34, 46),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TuiFocus {
    Home,
    Queue,
    Recent,
    Player,
}

const BASE_PLAYER_ACTIONS: &[TuiAction] = &[
    TuiAction::Previous,
    TuiAction::TogglePause,
    TuiAction::Next,
    TuiAction::Dislike,
    TuiAction::Like,
    TuiAction::CycleRepeat,
    TuiAction::VolumeDown,
    TuiAction::VolumeUp,
];

fn player_actions(snapshot: &PlaybackSnapshot) -> Vec<TuiAction> {
    let mut actions = Vec::with_capacity(BASE_PLAYER_ACTIONS.len() + 2);
    actions.extend_from_slice(&BASE_PLAYER_ACTIONS[..3]);
    if snapshot.halt_auto_resume_pending {
        actions.push(TuiAction::CancelHaltAutoResume);
        actions.push(TuiAction::ResumeHaltAutoResume);
    }
    actions.extend_from_slice(&BASE_PLAYER_ACTIONS[3..]);
    actions
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TuiAction {
    ResumeSuggested,
    TogglePause,
    CancelHaltAutoResume,
    ResumeHaltAutoResume,
    Previous,
    Next,
    Like,
    Dislike,
    Neutral,
    CycleRepeat,
    VolumeDown,
    VolumeUp,
    DismissQueue,
}

const VOLUME_STEP: f64 = 0.05;
const SEEK_STEP_MILLIS: u64 = 10_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    Queue,
    Lyrics,
}

#[derive(Clone, Copy)]
enum NavigationDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TuiNavItem {
    Home,
    Likes,
    Playlists,
    Offline,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TuiPlaylistSource {
    Remote,
    Offline,
}

#[derive(Clone)]
struct TuiPlaylistCard {
    summary: PlaylistSummary,
    source: TuiPlaylistSource,
}

#[derive(Clone)]
enum TuiHomeItem {
    Track(Track),
    Browse(HomeBrowseCard),
    Playlist(TuiPlaylistCard),
}

#[derive(Clone)]
struct HomeHeader {
    y: usize,
    section_index: usize,
    title: String,
    meta: String,
    show_play_controls: bool,
}

#[derive(Clone, Copy)]
struct HomeHeaderActionsLayout {
    play_x: u16,
    play_width: u16,
    shuffle_x: u16,
    shuffle_width: u16,
}

#[derive(Clone)]
struct HomeBanner {
    rect: Rect,
    eyebrow: String,
    title: String,
    subtitle: String,
    meta: String,
    hint: Option<String>,
    thumbnail_url: Option<String>,
    accent: bool,
}

#[derive(Clone)]
struct HomeMessage {
    y: usize,
    text: String,
    accent: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HomeCardStyle {
    Featured,
    TrackRow,
    BrowseRow,
    Shelf,
}

#[derive(Clone)]
struct HomeCard {
    rect: Rect,
    section_index: usize,
    item: TuiHomeItem,
    title: String,
    subtitle: String,
    meta: String,
    badge: String,
    style: HomeCardStyle,
}

#[derive(Default, Clone)]
struct HomeDocument {
    total_height: usize,
    headers: Vec<HomeHeader>,
    banners: Vec<HomeBanner>,
    cards: Vec<HomeCard>,
    messages: Vec<HomeMessage>,
}

#[derive(Clone)]
enum TuiContentView {
    Home,
    Browse(TuiBrowseView),
    Likes(TuiTrackPageView),
    Playlists(TuiCollectionPageView),
    PlaylistDetail(TuiTrackPageView),
    Offline(TuiOfflineView),
    Search(TuiSearchView),
    Settings(TuiSettingsView),
}

#[derive(Clone)]
struct TuiBrowseView {
    card: HomeBrowseCard,
    status: BrowseViewStatus,
}

#[derive(Clone)]
enum BrowseViewStatus {
    Loading,
    Tracks {
        title: String,
        thumbnail_url: Option<String>,
        tracks: Vec<Track>,
    },
    Sections {
        title: String,
        thumbnail_url: Option<String>,
        artist_like: bool,
        sections: Vec<crate::models::HomeSection>,
    },
    Error(String),
}

#[derive(Clone)]
struct ContentSnapshot {
    view: TuiContentView,
    selected: usize,
    scroll_offset: usize,
}

enum HomeFeedStatus {
    Loading,
    Ready(HomePage),
    Error(String),
}

#[derive(Clone)]
enum BrowseLoadResult {
    Tracks(Vec<Track>),
    Sections(BrowseSectionsPage),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TuiTrackPageKind {
    Likes,
    RemotePlaylist,
    OfflinePlaylist,
}

#[derive(Clone)]
struct TuiTrackPageView {
    kind: TuiTrackPageKind,
    key: String,
    title: String,
    subtitle: String,
    thumbnail_url: Option<String>,
    status: TuiTrackPageStatus,
}

#[derive(Clone)]
enum TuiTrackPageStatus {
    Loading,
    Ready {
        tracks: Vec<Track>,
        context: Option<PlaylistContext>,
    },
    Error(String),
}

#[derive(Clone)]
struct TuiCollectionPageView {
    title: String,
    subtitle: String,
    status: TuiCollectionPageStatus,
}

#[derive(Clone)]
enum TuiCollectionPageStatus {
    Loading,
    Ready { items: Vec<TuiPlaylistCard> },
    Error(String),
}

#[derive(Clone)]
struct TuiOfflineView {
    status: TuiOfflineStatus,
}

#[derive(Clone)]
enum TuiOfflineStatus {
    Loading,
    Ready {
        playlists: Vec<TuiPlaylistCard>,
        tracks: Vec<Track>,
        all_track_count: usize,
    },
    Error(String),
}

#[derive(Clone)]
struct TuiSearchView {
    query: String,
    status: TuiSearchStatus,
}

#[derive(Clone)]
enum TuiSearchStatus {
    Loading,
    Ready(SearchResult),
    Error(String),
}

#[derive(Clone)]
struct TuiSettingsView {
    status: TuiSettingsStatus,
}

#[derive(Clone)]
enum TuiSettingsStatus {
    Loading,
    Ready(TuiSettingsPageData),
    Error(String),
}

#[derive(Clone)]
struct TuiSettingsPageData {
    sections: Vec<TuiSettingsSection>,
}

#[derive(Clone)]
struct TuiSettingsSection {
    title: String,
    subtitle: String,
    fields: Vec<TuiSettingsField>,
}

#[derive(Clone)]
struct TuiSettingsField {
    title: String,
    subtitle: String,
    control: TuiSettingsControl,
}

#[derive(Clone)]
enum TuiSettingsControl {
    Toggle {
        key: &'static str,
        value: bool,
    },
    Dropdown {
        key: &'static str,
        value: String,
    },
    Stepper {
        key: &'static str,
        value: String,
    },
    TextInput {
        key: &'static str,
        value: String,
        placeholder: String,
    },
    SponsorCategory {
        key: &'static str,
        label: &'static str,
        enabled: bool,
    },
    StorageRoot {
        value: String,
        placeholder: String,
    },
    ReadOnly {
        value: String,
    },
}

#[derive(Clone)]
enum TuiSettingsTextTarget {
    Config(&'static str),
    StorageRoot,
}

#[derive(Clone)]
struct TuiSettingsTextEdit {
    target: TuiSettingsTextTarget,
    buffer: String,
}

enum TuiBackgroundEvent {
    HomeLoaded(Result<HomePage, String>),
    CrossDeviceQueueLoaded(Result<Option<CrossDeviceQueue>, String>),
    HomeAppended {
        continuation: String,
        result: Result<HomePage, String>,
    },
    BrowseLoaded {
        browse_id: String,
        result: Result<BrowseLoadResult, String>,
    },
    LikesLoaded(Result<Vec<Track>, String>),
    PlaylistsLoaded(Result<Vec<PlaylistSummary>, String>),
    TrackPageLoaded {
        key: String,
        result: Result<Vec<Track>, String>,
    },
    OfflineLoaded(Result<(Vec<PlaylistSummary>, Vec<Track>, Vec<Track>), String>),
    SearchLoaded {
        query: String,
        result: Result<SearchResult, String>,
    },
    SearchSuggestionsLoaded {
        request_id: u64,
        query: String,
        result: Result<Vec<String>, String>,
    },
    SettingsLoaded(Result<TuiSettingsPageData, String>),
    HistoryLoaded(Result<Vec<Track>, String>),
    LyricsLoaded {
        video_id: String,
        result: Result<Option<SyncedLyrics>, String>,
    },
    ThumbnailLoaded {
        url: String,
        result: Result<Vec<u8>, String>,
    },
}

#[derive(Clone)]
enum ClickTargetKind {
    Action(TuiAction),
    Nav(TuiNavItem),
    SearchBox,
    SearchSuggestion(usize),
    ContentBack,
    DetailTab(DetailTab),
    RadioChipScroll(isize),
    RadioChip(usize),
    HomeSectionAction { section_index: usize, shuffle: bool },
    HomeItem(usize),
    SettingsField(usize),
    SettingsToggle(&'static str),
    SettingsCycle(&'static str, isize),
    SettingsAdjust(&'static str, isize),
    SettingsEdit(TuiSettingsTextTarget),
    SettingsStorageRootDefault,
    SettingsSponsorCategory(&'static str),
    QueueItem(usize),
    RecentItem(usize),
}

#[derive(Clone)]
struct ClickTarget {
    rect: Rect,
    kind: ClickTargetKind,
}

#[derive(Clone)]
enum TuiDragPayload {
    QueueRow { index: usize },
    Track { track: Track },
    BrowseCollection { card: HomeBrowseCard },
    PlaylistCollection { card: TuiPlaylistCard },
}

struct PendingMousePress {
    origin_column: u16,
    origin_row: u16,
    target_kind: ClickTargetKind,
    target_rect: Rect,
    payload: Option<TuiDragPayload>,
}

struct TuiDragState {
    payload: TuiDragPayload,
    preview_slot: Option<usize>,
}

#[derive(Clone)]
enum ContextMenuAction {
    PlayTrackNow {
        track: Track,
    },
    PlayQueueNow {
        index: usize,
    },
    JumpToTrackRadio {
        track: Track,
    },
    JumpToQueueRadio {
        index: usize,
    },
    QueueTrackNext {
        track: Track,
    },
    QueueTrackLast {
        track: Track,
    },
    OpenBrowseNow {
        card: HomeBrowseCard,
    },
    QueueBrowseNext {
        card: HomeBrowseCard,
    },
    QueueBrowseLast {
        card: HomeBrowseCard,
    },
    ProbeTrack {
        track: Track,
    },
    RemoveQueueItem {
        index: usize,
    },
    SubmitFeedbackToken {
        token: String,
        verb: &'static str,
        title: String,
        video_id: String,
        reveal_undo: bool,
    },
}

#[derive(Clone, Copy)]
enum ContextMenuTone {
    Default,
    Accent,
    Danger,
}

#[derive(Clone)]
struct ContextMenuItem {
    label: String,
    hint: String,
    tone: ContextMenuTone,
    action: ContextMenuAction,
}

struct ContextMenuState {
    title: String,
    subtitle: String,
    items: Vec<ContextMenuItem>,
    selected: usize,
    anchor_column: u16,
    anchor_row: u16,
}

#[derive(Clone, Default)]
struct RenderedContextMenu {
    menu_rect: Option<Rect>,
    item_rects: Vec<Rect>,
}

#[derive(Clone, Default)]
struct RenderedLayout {
    home_viewport: Rect,
    radio_chip_viewport: Rect,
    queue_viewport: Rect,
    recent_viewport: Rect,
    player_progress_viewport: Rect,
}

#[derive(Clone)]
struct ResumeCardData {
    source: ResumeCardSource,
    track: Track,
    meta: String,
}

#[derive(Clone)]
enum ResumeCardSource {
    Local(PlaybackSnapshot),
    CrossDevice(CrossDeviceQueue),
}

impl ResumeCardData {
    fn eyebrow(&self) -> &'static str {
        match self.source {
            ResumeCardSource::Local(_) => "RESUME",
            ResumeCardSource::CrossDevice(_) => "FROM ANOTHER DEVICE",
        }
    }

    fn title(&self) -> String {
        let verb = match self.source {
            ResumeCardSource::Local(_) => "Resume",
            ResumeCardSource::CrossDevice(_) => "Continue",
        };
        format!("{verb} {}", self.track.title)
    }

    fn hint(&self) -> &'static str {
        match self.source {
            ResumeCardSource::Local(_) => "Click here to restore",
            ResumeCardSource::CrossDevice(_) => "Click here to resume the YouTube Music queue",
        }
    }
}

enum LyricsState {
    Idle,
    Loading {
        video_id: String,
    },
    Ready {
        video_id: String,
        lyrics: SyncedLyrics,
    },
    Missing {
        video_id: String,
        reason: String,
    },
    Error {
        video_id: String,
        message: String,
    },
}

enum ThumbnailAsset {
    Loading,
    Ready(DynamicImage),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ThumbnailSlot {
    ContentHeader,
    ContentBanner(usize),
    PlayerBar,
    HomeCard(usize),
}

struct ThumbnailSlotState {
    key: String,
    protocol: StatefulProtocol,
}

struct InteractiveTui {
    client: DaemonClient,
    snapshot: PlaybackSnapshot,
    queue_selected: usize,
    queue_scroll_offset: usize,
    queue_viewport_rows: usize,
    recent_tracks: Vec<Track>,
    recent_selected: usize,
    recent_scroll_offset: usize,
    recent_viewport_rows: usize,
    home_status: HomeFeedStatus,
    home_doc: HomeDocument,
    home_selected: usize,
    home_scroll_offset: usize,
    home_viewport_rows: usize,
    home_loading_more: bool,
    content_view: TuiContentView,
    content_backstack: Vec<ContentSnapshot>,
    search_query: String,
    search_input_active: bool,
    search_suggestions: Vec<String>,
    search_suggestions_loading: bool,
    search_suggestion_selected: usize,
    search_suggestion_request_id: u64,
    search_suggestion_debounce_started_at: Option<Instant>,
    search_suggestion_inflight: Option<(u64, String)>,
    submitted_search_query_to_clear: Option<String>,
    completed_search_query_to_clear: Option<String>,
    radio_chip_scroll_offset: usize,
    settings_field_offsets: Vec<(usize, usize)>,
    settings_total_height: usize,
    settings_text_edit: Option<TuiSettingsTextEdit>,
    focus: TuiFocus,
    player_selected: usize,
    detail_tab: DetailTab,
    resume_card: Option<ResumeCardData>,
    lyrics_state: LyricsState,
    status_line: String,
    last_snapshot_at: Instant,
    last_daemon_poll_at: Instant,
    feedback_undo_ready: HashSet<String>,
    click_targets: Vec<ClickTarget>,
    rendered_layout: RenderedLayout,
    background_tx: mpsc::Sender<TuiBackgroundEvent>,
    background_rx: mpsc::Receiver<TuiBackgroundEvent>,
    thumbnail_picker: Option<Picker>,
    thumbnail_assets: HashMap<String, ThumbnailAsset>,
    thumbnail_requests: HashSet<String>,
    thumbnail_slots: HashMap<ThumbnailSlot, ThumbnailSlotState>,
    thumbnail_mode_label: String,
    pending_mouse_press: Option<PendingMousePress>,
    drag_state: Option<TuiDragState>,
    context_menu: Option<ContextMenuState>,
    rendered_context_menu: RenderedContextMenu,
    help_visible: bool,
    rendered_help_rect: Option<Rect>,
}

#[cfg(test)]
#[path = "tests/cli_tui.rs"]
mod tests;
