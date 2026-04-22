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
    CropOptions, Resize, StatefulImage,
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
};

use crate::cli::{
    artist_string, format_millis, format_track_summary, play_track_with_radio,
    restore_playback_snapshot_via_daemon, saved_network_state,
};
use crate::ipc::{
    ActiveTrackSnapshot, DaemonClient, PlaybackSnapshot, PlaylistContext, PlaylistContextSource,
    RepeatMode, load_persisted_playback_state,
};
use crate::models::{
    BrowseSectionsPage, HomeBrowseCard, HomeBrowsePageType, HomeItem, HomePage,
    RadioVirtualPlaylist, SyncedLyrics, Track,
};
use crate::network::{SongRating, fetch_thumbnail_bytes};

const UI_TICK_INTERVAL: Duration = Duration::from_millis(250);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_secs(5);
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

const PLAYER_ACTIONS: &[TuiAction] = &[
    TuiAction::Previous,
    TuiAction::TogglePause,
    TuiAction::Next,
    TuiAction::Dislike,
    TuiAction::Like,
    TuiAction::CycleRepeat,
    TuiAction::VolumeDown,
    TuiAction::VolumeUp,
];

#[derive(Clone, Copy)]
enum TuiAction {
    ResumeSaved,
    TogglePause,
    Previous,
    Next,
    Like,
    Dislike,
    Neutral,
    CycleRepeat,
    VolumeDown,
    VolumeUp,
}

const VOLUME_STEP: f64 = 0.05;

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

#[derive(Clone, Copy)]
enum TuiNavItem {
    Home,
    Likes,
    Playlists,
    Offline,
    Settings,
}

#[derive(Clone)]
enum TuiHomeItem {
    Track(Track),
    Browse(HomeBrowseCard),
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
enum ContentSnapshot {
    Home {
        selected: usize,
        scroll_offset: usize,
    },
    Browse {
        view: TuiBrowseView,
        selected: usize,
        scroll_offset: usize,
    },
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

enum TuiBackgroundEvent {
    HomeLoaded(Result<HomePage, String>),
    HomeAppended {
        continuation: String,
        result: Result<HomePage, String>,
    },
    BrowseLoaded {
        browse_id: String,
        result: Result<BrowseLoadResult, String>,
    },
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
    ContentBack,
    DetailTab(DetailTab),
    HomeSectionAction { section_index: usize, shuffle: bool },
    HomeItem(usize),
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
    queue_viewport: Rect,
    recent_viewport: Rect,
    player_progress_viewport: Rect,
}

#[derive(Clone)]
struct ResumeCardData {
    track: Track,
    meta: String,
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
    url: String,
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

impl InteractiveTui {
    fn new(client: DaemonClient) -> Result<Self> {
        let snapshot = client.state()?;
        let (background_tx, background_rx) = mpsc::channel();
        spawn_home_bootstrap(background_tx.clone());
        let resume_card = load_resume_card_data();

        let mut app = Self {
            client,
            snapshot: snapshot.clone(),
            queue_selected: snapshot
                .queue_index
                .min(snapshot.queue.len().saturating_sub(1)),
            queue_scroll_offset: 0,
            queue_viewport_rows: 10,
            recent_tracks: Vec::new(),
            recent_selected: 0,
            recent_scroll_offset: 0,
            recent_viewport_rows: 8,
            home_status: HomeFeedStatus::Loading,
            home_doc: HomeDocument::default(),
            home_selected: 0,
            home_scroll_offset: 0,
            home_viewport_rows: 12,
            home_loading_more: false,
            content_view: TuiContentView::Home,
            content_backstack: Vec::new(),
            focus: TuiFocus::Home,
            player_selected: 1,
            detail_tab: DetailTab::Queue,
            resume_card,
            lyrics_state: LyricsState::Idle,
            status_line: "Home mirrors the GTK app now: arrows move, enter plays, mouse works."
                .to_string(),
            last_snapshot_at: Instant::now(),
            last_daemon_poll_at: Instant::now(),
            feedback_undo_ready: HashSet::new(),
            click_targets: Vec::new(),
            rendered_layout: RenderedLayout::default(),
            background_tx,
            background_rx,
            thumbnail_picker: None,
            thumbnail_assets: HashMap::new(),
            thumbnail_requests: HashSet::new(),
            thumbnail_slots: HashMap::new(),
            thumbnail_mode_label: "art off".to_string(),
            pending_mouse_press: None,
            drag_state: None,
            context_menu: None,
            rendered_context_menu: RenderedContextMenu::default(),
            help_visible: false,
            rendered_help_rect: None,
        };
        app.maybe_request_lyrics();
        Ok(app)
    }

    fn run(&mut self) -> Result<()> {
        let _guard = TerminalGuard::enter()?;
        self.initialize_terminal_images();
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).context("Failed to initialize terminal UI")?;
        terminal.clear().ok();

        loop {
            self.drain_background_events();

            let mut click_targets = Vec::new();
            let mut rendered_layout = RenderedLayout::default();
            terminal
                .draw(|frame| {
                    let (targets, layout) = self.render_frame(frame);
                    click_targets = targets;
                    rendered_layout = layout;
                })
                .context("Failed to draw TUI frame")?;
            self.click_targets = click_targets;
            self.rendered_layout = rendered_layout;

            let timeout = UI_TICK_INTERVAL;

            if event::poll(timeout).context("Failed to poll terminal events")? {
                match event::read().context("Failed to read terminal event")? {
                    Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                        if self.handle_key(key_event.code)? {
                            break;
                        }
                    }
                    Event::Mouse(mouse_event) => self.handle_mouse(mouse_event)?,
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }

            if self.last_daemon_poll_at.elapsed() >= DAEMON_POLL_INTERVAL {
                self.refresh_snapshot();
            }
        }

        Ok(())
    }

    fn initialize_terminal_images(&mut self) {
        let (picker, queried_stdio) = match Picker::from_query_stdio() {
            Ok(picker) => (picker, true),
            Err(_) => (Picker::from_fontsize((10, 20)), false),
        };

        self.thumbnail_mode_label = if queried_stdio {
            format!("art {}", thumbnail_protocol_label(picker.protocol_type()))
        } else {
            format!("art {}?", thumbnail_protocol_label(picker.protocol_type()))
        };
        self.thumbnail_picker = Some(picker);
    }

    fn terminal_images_enabled(&self) -> bool {
        self.thumbnail_picker.is_some()
    }

    fn ensure_thumbnail_requested(&mut self, url: Option<&str>) {
        let Some(url) = url.filter(|url| !url.is_empty()) else {
            return;
        };
        if !self.terminal_images_enabled() {
            return;
        }
        if self.thumbnail_requests.contains(url) || self.thumbnail_assets.contains_key(url) {
            return;
        }

        let url = url.to_string();
        self.thumbnail_requests.insert(url.clone());
        self.thumbnail_assets
            .insert(url.clone(), ThumbnailAsset::Loading);
        spawn_thumbnail_load(self.background_tx.clone(), url);
    }

    fn thumbnail_source(&self, url: &str) -> Option<DynamicImage> {
        match self.thumbnail_assets.get(url) {
            Some(ThumbnailAsset::Ready(image)) => Some(image.clone()),
            _ => None,
        }
    }

    fn thumbnail_failed(&self, url: &str) -> bool {
        matches!(
            self.thumbnail_assets.get(url),
            Some(ThumbnailAsset::Failed(message)) if !message.is_empty()
        )
    }

    fn thumbnail_font_size(&self) -> (u16, u16) {
        self.thumbnail_picker
            .as_ref()
            .map(Picker::font_size)
            .unwrap_or((10, 20))
    }

    fn square_box_width_for_height(&self, outer_height: u16) -> u16 {
        if outer_height == 0 {
            return 0;
        }

        let (font_width, font_height) = self.thumbnail_font_size();
        let inner_height = outer_height.saturating_sub(2).max(1);
        let inner_width =
            ((inner_height as f64 * font_height as f64 / font_width as f64).round() as u16).max(1);
        inner_width.saturating_add(2)
    }

    fn square_thumbnail_rect(&self, area: Rect) -> Rect {
        if area.width == 0 || area.height == 0 {
            return area;
        }

        let (font_width, font_height) = self.thumbnail_font_size();

        let pixel_width = area.width as f64 * font_width as f64;
        let pixel_height = area.height as f64 * font_height as f64;

        let (target_width, target_height) = if pixel_width > pixel_height {
            let width = ((pixel_height / font_width as f64).round() as u16)
                .max(1)
                .min(area.width);
            (width, area.height)
        } else {
            let height = ((pixel_width / font_height as f64).round() as u16)
                .max(1)
                .min(area.height);
            (area.width, height)
        };

        Rect::new(
            area.x + area.width.saturating_sub(target_width) / 2,
            area.y + area.height.saturating_sub(target_height) / 2,
            target_width,
            target_height,
        )
    }

    fn render_protocol_thumbnail(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        slot: ThumbnailSlot,
        url: &str,
        resize: Resize,
    ) -> bool {
        if area.width == 0 || area.height == 0 || !self.terminal_images_enabled() {
            return false;
        }

        let needs_init = self
            .thumbnail_slots
            .get(&slot)
            .map(|state| state.url.as_str() != url)
            .unwrap_or(true);

        if needs_init {
            let Some(source) = self.thumbnail_source(url) else {
                return false;
            };
            let Some(picker) = self.thumbnail_picker.clone() else {
                return false;
            };
            self.thumbnail_slots.insert(
                slot,
                ThumbnailSlotState {
                    url: url.to_string(),
                    protocol: picker.new_resize_protocol(source),
                },
            );
        }

        let encoding_result = if let Some(slot_state) = self.thumbnail_slots.get_mut(&slot) {
            frame.render_stateful_widget(
                StatefulImage::<StatefulProtocol>::default().resize(resize),
                area,
                &mut slot_state.protocol,
            );
            slot_state.protocol.last_encoding_result()
        } else {
            None
        };

        if let Some(Err(err)) = encoding_result {
            self.status_line = format!("Thumbnail render failed: {err}");
        }

        true
    }

    fn render_cover_thumbnail(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        slot: ThumbnailSlot,
        url: Option<&str>,
        fallback_initials: String,
        theme: &TuiTheme,
        accent: bool,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let failed = url.is_some_and(|url| self.thumbnail_failed(url));
        let border = if accent && !failed {
            theme.accent
        } else {
            theme.border_subtle
        };
        let background = if accent && !failed {
            theme.accent_fill
        } else {
            theme.content_alt_bg
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border))
            .style(Style::default().bg(background));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let image_rect = self.square_thumbnail_rect(inner);

        if let Some(url) = url {
            self.ensure_thumbnail_requested(Some(url));
            if self.render_protocol_thumbnail(frame, image_rect, slot, url, Resize::Fit(None)) {
                return;
            }
        } else {
            self.thumbnail_slots.remove(&slot);
        }

        render_cover_placeholder(frame, image_rect, fallback_initials, theme);
    }

    fn render_home_card_thumbnail(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        visible_area: Rect,
        index: usize,
        card: &HomeCard,
        theme: &TuiTheme,
    ) {
        if area.width == 0
            || area.height == 0
            || visible_area.width == 0
            || visible_area.height == 0
        {
            return;
        }

        let image_rect = self.square_thumbnail_rect(area);
        let Some(visible_rect) = intersect_rect(image_rect, visible_area) else {
            return;
        };
        let url = home_card_thumbnail_url(card);
        let failed = url.is_some_and(|url| self.thumbnail_failed(url));
        if let Some(url) = url {
            self.ensure_thumbnail_requested(Some(url));
            let resize = thumbnail_resize_for_visible_rect(image_rect, visible_rect);
            if self.render_protocol_thumbnail(
                frame,
                visible_rect,
                ThumbnailSlot::HomeCard(index),
                url,
                resize,
            ) {
                return;
            }
        } else {
            self.thumbnail_slots.remove(&ThumbnailSlot::HomeCard(index));
        }

        render_home_thumbnail_fallback(
            frame,
            visible_rect,
            placeholder_initials(&card.title, &card.subtitle),
            theme,
            matches!(card.style, HomeCardStyle::Featured) && !failed,
        );
    }

    fn queue_accepts_dnd(&self) -> bool {
        self.detail_tab == DetailTab::Queue
    }

    fn drag_label(payload: &TuiDragPayload) -> String {
        match payload {
            TuiDragPayload::QueueRow { .. } => "Move in queue".to_string(),
            TuiDragPayload::Track { track } => format!("Queue {}", shorten(&track.title, 28)),
            TuiDragPayload::BrowseCollection { card } => {
                format!("Queue {}", shorten(&card.title, 28))
            }
        }
    }

    fn drag_payload_for_target(&self, kind: &ClickTargetKind) -> Option<TuiDragPayload> {
        match kind {
            ClickTargetKind::QueueItem(index) => Some(TuiDragPayload::QueueRow { index: *index }),
            ClickTargetKind::HomeItem(index) => {
                let card = self.home_doc.cards.get(*index)?;
                match &card.item {
                    TuiHomeItem::Track(track) => Some(TuiDragPayload::Track {
                        track: track.clone(),
                    }),
                    TuiHomeItem::Browse(card) => {
                        Some(TuiDragPayload::BrowseCollection { card: card.clone() })
                    }
                }
            }
            ClickTargetKind::RecentItem(index) => self
                .recent_tracks
                .get(*index)
                .cloned()
                .map(|track| TuiDragPayload::Track { track }),
            _ => None,
        }
    }

    fn set_mouse_selection(&mut self, kind: &ClickTargetKind) {
        match kind {
            ClickTargetKind::HomeItem(index) => {
                self.focus = TuiFocus::Home;
                self.home_selected = *index;
                self.ensure_home_visible();
            }
            ClickTargetKind::QueueItem(index) => {
                self.focus = TuiFocus::Queue;
                self.queue_selected = *index;
                self.ensure_queue_visible();
            }
            ClickTargetKind::RecentItem(index) => {
                self.focus = TuiFocus::Recent;
                self.recent_selected = *index;
                self.ensure_recent_visible();
            }
            _ => {}
        }
    }

    fn focused_context_menu_target(&self) -> Option<(ClickTargetKind, u16, u16)> {
        let target =
            self.click_targets
                .iter()
                .find(|target| match (&self.focus, &target.kind) {
                    (TuiFocus::Home, ClickTargetKind::HomeItem(index)) => {
                        *index == self.home_selected
                    }
                    (TuiFocus::Queue, ClickTargetKind::QueueItem(index)) => {
                        *index == self.queue_selected
                    }
                    (TuiFocus::Recent, ClickTargetKind::RecentItem(index)) => {
                        *index == self.recent_selected
                    }
                    _ => false,
                })?;

        let anchor_column = target
            .rect
            .x
            .saturating_add(target.rect.width.saturating_sub(1) / 2);
        let anchor_row = target
            .rect
            .y
            .saturating_add(target.rect.height.saturating_sub(1) / 2);
        Some((target.kind.clone(), anchor_column, anchor_row))
    }

    fn open_context_menu_for_focus(&mut self) {
        let Some((kind, anchor_column, anchor_row)) = self.focused_context_menu_target() else {
            self.status_line = "Focus a song or collection first.".to_string();
            return;
        };

        if !self.open_context_menu(&kind, anchor_column, anchor_row) {
            self.status_line = "That selection has no context menu.".to_string();
        }
    }

    fn queue_visible_len(&self) -> usize {
        self.snapshot
            .queue
            .len()
            .saturating_sub(self.queue_scroll_offset)
            .min(self.queue_viewport_rows.max(1))
    }

    fn queue_raw_slot_for_mouse_row(&self, row: u16) -> usize {
        if self.snapshot.queue.is_empty() {
            return 0;
        }

        let viewport = self.rendered_layout.queue_viewport;
        if viewport.height == 0 {
            return self.snapshot.queue.len();
        }

        if row <= viewport.y {
            return self.queue_scroll_offset.min(self.snapshot.queue.len());
        }

        let visible_len = self.queue_visible_len();
        if visible_len == 0 {
            return self.snapshot.queue.len();
        }

        let relative = row.saturating_sub(viewport.y) as usize;
        let row_height = QUEUE_ROW_HEIGHT as usize;
        let row_index = (relative / row_height).min(visible_len.saturating_sub(1));
        let within_row = relative % row_height;
        let absolute_index = self.queue_scroll_offset + row_index;
        let midpoint = QUEUE_ROW_HEIGHT as f32 / 2.0;
        let before_row = (within_row as f32) < midpoint;
        if before_row {
            absolute_index
        } else {
            absolute_index
                .saturating_add(1)
                .min(self.snapshot.queue.len())
        }
    }

    fn normalized_preview_slot_for_drag(
        &self,
        payload: &TuiDragPayload,
        raw_slot: usize,
    ) -> Option<usize> {
        match payload {
            TuiDragPayload::QueueRow { index: from } => {
                let normalized = if *from < raw_slot {
                    raw_slot.saturating_sub(1)
                } else {
                    raw_slot
                };
                (normalized != *from).then_some(raw_slot)
            }
            _ => Some(raw_slot),
        }
    }

    fn maybe_autoscroll_queue_drag(&mut self, row: u16) {
        if !self.queue_accepts_dnd() {
            return;
        }
        let viewport = self.rendered_layout.queue_viewport;
        if viewport.height == 0 || self.snapshot.queue.len() <= self.queue_viewport_rows {
            return;
        }

        let edge = QUEUE_ROW_HEIGHT.saturating_mul(2);
        if row <= viewport.y.saturating_add(edge) && self.queue_scroll_offset > 0 {
            self.queue_scroll_offset = self.queue_scroll_offset.saturating_sub(1);
        } else if row.saturating_add(edge) >= viewport.bottom()
            && self
                .queue_scroll_offset
                .saturating_add(self.queue_viewport_rows)
                < self.snapshot.queue.len()
        {
            self.queue_scroll_offset = self.queue_scroll_offset.saturating_add(1);
        }
    }

    fn update_drag_preview(&mut self, column: u16, row: u16) {
        let Some(payload) = self.drag_state.as_ref().map(|drag| drag.payload.clone()) else {
            return;
        };
        if !self.queue_accepts_dnd() || !contains(self.rendered_layout.queue_viewport, column, row)
        {
            if let Some(drag) = self.drag_state.as_mut() {
                drag.preview_slot = None;
            }
            return;
        }

        self.focus = TuiFocus::Queue;
        self.maybe_autoscroll_queue_drag(row);
        let raw_slot = self.queue_raw_slot_for_mouse_row(row);
        let preview_slot = self.normalized_preview_slot_for_drag(&payload, raw_slot);
        if let Some(drag) = self.drag_state.as_mut() {
            drag.preview_slot = preview_slot;
        }
    }

    fn clear_drag_state(&mut self) {
        self.pending_mouse_press = None;
        self.drag_state = None;
    }

    fn current_preview_slot(&self) -> Option<usize> {
        self.drag_state.as_ref().and_then(|drag| drag.preview_slot)
    }

    fn apply_queue_row_reorder(&mut self, from: usize, raw_slot: usize) -> Result<()> {
        let insert_at = if from < raw_slot {
            raw_slot.saturating_sub(1)
        } else {
            raw_slot
        };
        if insert_at == from
            || from >= self.snapshot.queue.len()
            || raw_slot > self.snapshot.queue.len()
        {
            return Ok(());
        }

        let mut queue = self.snapshot.queue.clone();
        let moved_track = queue.remove(from);
        queue.insert(insert_at, moved_track.clone());

        let current_index = adjusted_reorder_index(self.snapshot.queue_index, from, insert_at);
        let selected_index = adjusted_reorder_index(self.queue_selected, from, insert_at);
        let mut radio_virtual_playlist = self.snapshot.radio_virtual_playlist.clone();
        update_radio_touched_for_reorder(&mut radio_virtual_playlist, from, insert_at);

        let snapshot = self.client.replace_queue(
            queue,
            current_index,
            self.snapshot.playlist_context.clone(),
            radio_virtual_playlist,
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.focus = TuiFocus::Queue;
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = selected_index.min(self.snapshot.queue.len().saturating_sub(1));
        self.ensure_queue_visible();
        self.status_line = format!(
            "Moved {} to slot {}.",
            shorten(&moved_track.title, 28),
            insert_at.saturating_add(1)
        );
        Ok(())
    }

    fn insert_tracks_into_queue(&mut self, tracks: Vec<Track>, raw_slot: usize) -> Result<()> {
        if tracks.is_empty() {
            self.status_line = "Nothing to insert into the queue.".to_string();
            return Ok(());
        }

        let had_queue = !self.snapshot.queue.is_empty();
        let insert_at = raw_slot.min(self.snapshot.queue.len());
        let added = tracks.len();
        let first_title = tracks
            .first()
            .map(|track| shorten(&track.title, 28))
            .unwrap_or_else(|| "tracks".to_string());
        let mut queue = self.snapshot.queue.clone();
        for (offset, track) in tracks.into_iter().enumerate() {
            queue.insert(insert_at + offset, track);
        }

        let current_index = if had_queue && insert_at <= self.snapshot.queue_index {
            self.snapshot.queue_index.saturating_add(added)
        } else {
            self.snapshot.queue_index
        };
        let mut radio_virtual_playlist = self.snapshot.radio_virtual_playlist.clone();
        extend_radio_touched_for_insert(&mut radio_virtual_playlist, insert_at, added);

        let snapshot = self.client.replace_queue(
            queue,
            current_index,
            self.snapshot.playlist_context.clone(),
            radio_virtual_playlist,
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.focus = TuiFocus::Queue;
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = insert_at.min(self.snapshot.queue.len().saturating_sub(1));
        self.ensure_queue_visible();
        self.status_line = if added == 1 {
            format!("Queued {}.", first_title)
        } else {
            format!("Queued {} tracks.", added)
        };
        Ok(())
    }

    fn handle_drag_drop(&mut self, payload: TuiDragPayload, raw_slot: usize) -> Result<()> {
        match payload {
            TuiDragPayload::QueueRow { index } => self.apply_queue_row_reorder(index, raw_slot),
            TuiDragPayload::Track { track } => self.insert_tracks_into_queue(vec![track], raw_slot),
            TuiDragPayload::BrowseCollection { card } => {
                let network =
                    saved_network_state().context("Connect YouTube Music in the GTK app first")?;
                let tracks = network
                    .get_browse_tracks(&card.browse_id)
                    .with_context(|| format!("Failed to open {}", card.title))?;
                self.insert_tracks_into_queue(tracks, raw_slot)
            }
        }
    }

    fn maybe_request_more_home(&mut self) {
        if !matches!(&self.content_view, TuiContentView::Home) {
            return;
        }

        if self.home_loading_more {
            return;
        }

        let continuation = match &self.home_status {
            HomeFeedStatus::Ready(page) => page.continuation.clone(),
            HomeFeedStatus::Loading | HomeFeedStatus::Error(_) => None,
        };
        let Some(continuation) = continuation.filter(|token| !token.is_empty()) else {
            return;
        };

        let viewport = self.home_viewport_rows.max(1);
        let remaining = self
            .home_doc
            .total_height
            .saturating_sub(self.home_scroll_offset.saturating_add(viewport));
        if remaining > HOME_LOAD_MORE_THRESHOLD_ROWS {
            return;
        }

        self.home_loading_more = true;
        self.status_line = "Loading more Home sections...".to_string();
        spawn_home_append(self.background_tx.clone(), continuation);
    }

    fn close_context_menu(&mut self) {
        self.context_menu = None;
        self.rendered_context_menu = RenderedContextMenu::default();
    }

    fn can_jump_to_radio(&self) -> bool {
        self.snapshot.radio_virtual_playlist.is_some()
            || self.snapshot.playlist_context.is_some()
            || !self.snapshot.queue.is_empty()
    }

    fn push_feedback_context_items(&self, items: &mut Vec<ContextMenuItem>, track: &Track) {
        let undo_visible = self.feedback_undo_ready.contains(&track.video_id);

        if !undo_visible {
            if let Some(token) = track.not_interested_feedback_token.clone() {
                items.push(ContextMenuItem {
                    label: "Not interested".to_string(),
                    hint: "feedback".to_string(),
                    tone: ContextMenuTone::Danger,
                    action: ContextMenuAction::SubmitFeedbackToken {
                        token,
                        verb: "Marked as not interested",
                        title: track.title.clone(),
                        video_id: track.video_id.clone(),
                        reveal_undo: true,
                    },
                });
            }
        }

        if undo_visible {
            if let Some(token) = track.undo_feedback_token.clone() {
                items.push(ContextMenuItem {
                    label: "Undo feedback".to_string(),
                    hint: "restore".to_string(),
                    tone: ContextMenuTone::Default,
                    action: ContextMenuAction::SubmitFeedbackToken {
                        token,
                        verb: "Feedback undone for",
                        title: track.title.clone(),
                        video_id: track.video_id.clone(),
                        reveal_undo: false,
                    },
                });
            }
        }
    }

    fn context_menu_track_items(&self, track: &Track) -> Vec<ContextMenuItem> {
        let mut items = vec![
            ContextMenuItem {
                label: "Play now".to_string(),
                hint: "start radio".to_string(),
                tone: ContextMenuTone::Accent,
                action: ContextMenuAction::PlayTrackNow {
                    track: track.clone(),
                },
            },
            ContextMenuItem {
                label: "Play next".to_string(),
                hint: "after current".to_string(),
                tone: ContextMenuTone::Default,
                action: ContextMenuAction::QueueTrackNext {
                    track: track.clone(),
                },
            },
            ContextMenuItem {
                label: "Add to queue".to_string(),
                hint: "respect radio tail".to_string(),
                tone: ContextMenuTone::Default,
                action: ContextMenuAction::QueueTrackLast {
                    track: track.clone(),
                },
            },
        ];

        if self.can_jump_to_radio() {
            items.push(ContextMenuItem {
                label: "Jump to".to_string(),
                hint: "radio seed".to_string(),
                tone: ContextMenuTone::Default,
                action: ContextMenuAction::JumpToTrackRadio {
                    track: track.clone(),
                },
            });
        }

        items.push(ContextMenuItem {
            label: "Probe headless".to_string(),
            hint: "no radio".to_string(),
            tone: ContextMenuTone::Default,
            action: ContextMenuAction::ProbeTrack {
                track: track.clone(),
            },
        });
        self.push_feedback_context_items(&mut items, track);

        items
    }

    fn browse_card_queueable(card: &HomeBrowseCard) -> bool {
        matches!(
            card.page_type,
            HomeBrowsePageType::Album
                | HomeBrowsePageType::Playlist
                | HomeBrowsePageType::PodcastShow
        )
    }

    fn context_menu_for_target(
        &self,
        kind: &ClickTargetKind,
        anchor_column: u16,
        anchor_row: u16,
    ) -> Option<ContextMenuState> {
        let (title, subtitle, items) = match kind {
            ClickTargetKind::QueueItem(index) => {
                let track = self.snapshot.queue.get(*index)?.clone();
                let mut items = vec![ContextMenuItem {
                    label: "Play now".to_string(),
                    hint: "keep queue".to_string(),
                    tone: ContextMenuTone::Accent,
                    action: ContextMenuAction::PlayQueueNow { index: *index },
                }];
                if self.can_jump_to_radio() {
                    items.push(ContextMenuItem {
                        label: "Jump to".to_string(),
                        hint: "radio seed".to_string(),
                        tone: ContextMenuTone::Default,
                        action: ContextMenuAction::JumpToQueueRadio { index: *index },
                    });
                }
                items.push(ContextMenuItem {
                    label: "Probe headless".to_string(),
                    hint: "no radio".to_string(),
                    tone: ContextMenuTone::Default,
                    action: ContextMenuAction::ProbeTrack {
                        track: track.clone(),
                    },
                });
                self.push_feedback_context_items(&mut items, &track);
                if *index != self.snapshot.queue_index {
                    items.push(ContextMenuItem {
                        label: "Remove from queue".to_string(),
                        hint: "delete row".to_string(),
                        tone: ContextMenuTone::Danger,
                        action: ContextMenuAction::RemoveQueueItem { index: *index },
                    });
                }
                (
                    format!("Queue #{}", index.saturating_add(1)),
                    format_track_summary(&track),
                    items,
                )
            }
            ClickTargetKind::HomeItem(index) => {
                let card = self.home_doc.cards.get(*index)?.clone();
                match card.item {
                    TuiHomeItem::Track(track) => (
                        track.title.clone(),
                        artist_string(&track),
                        self.context_menu_track_items(&track),
                    ),
                    TuiHomeItem::Browse(card) => {
                        let mut items = vec![ContextMenuItem {
                            label: "Open now".to_string(),
                            hint: browse_label(card.page_type).to_lowercase(),
                            tone: ContextMenuTone::Accent,
                            action: ContextMenuAction::OpenBrowseNow { card: card.clone() },
                        }];
                        if Self::browse_card_queueable(&card) {
                            items.push(ContextMenuItem {
                                label: "Queue next".to_string(),
                                hint: "insert collection".to_string(),
                                tone: ContextMenuTone::Default,
                                action: ContextMenuAction::QueueBrowseNext { card: card.clone() },
                            });
                            items.push(ContextMenuItem {
                                label: "Add to queue".to_string(),
                                hint: "append collection".to_string(),
                                tone: ContextMenuTone::Default,
                                action: ContextMenuAction::QueueBrowseLast { card: card.clone() },
                            });
                        }
                        (
                            card.title.clone(),
                            card.subtitle
                                .clone()
                                .unwrap_or_else(|| browse_label(card.page_type).to_string()),
                            items,
                        )
                    }
                }
            }
            ClickTargetKind::RecentItem(index) => {
                let track = self.recent_tracks.get(*index)?.clone();
                (
                    track.title.clone(),
                    artist_string(&track),
                    self.context_menu_track_items(&track),
                )
            }
            _ => return None,
        };

        if items.is_empty() {
            return None;
        }

        Some(ContextMenuState {
            title,
            subtitle,
            items,
            selected: 0,
            anchor_column,
            anchor_row,
        })
    }

    fn open_context_menu(
        &mut self,
        kind: &ClickTargetKind,
        anchor_column: u16,
        anchor_row: u16,
    ) -> bool {
        if let Some(menu) = self.context_menu_for_target(kind, anchor_column, anchor_row) {
            self.context_menu = Some(menu);
            self.status_line =
                "Context menu open. Enter runs, Esc closes, arrow keys move.".to_string();
            true
        } else {
            self.close_context_menu();
            false
        }
    }

    fn queue_next_slot(&self) -> usize {
        if self.snapshot.queue.is_empty() {
            0
        } else {
            self.snapshot
                .queue_index
                .saturating_add(1)
                .min(self.snapshot.queue.len())
        }
    }

    fn queue_last_slot(&self) -> usize {
        self.snapshot
            .radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.touched_len.min(self.snapshot.queue.len()))
            .unwrap_or(self.snapshot.queue.len())
    }

    fn jump_to_queue_radio_seed(&mut self, index: usize) -> Result<()> {
        if !self.can_jump_to_radio() {
            self.status_line = "Jump to needs an active queue or playlist context.".to_string();
            return Ok(());
        }

        let Some(seed_track) = self.snapshot.queue.get(index).cloned() else {
            self.status_line = "Queue item is gone.".to_string();
            return Ok(());
        };

        let prefix_len = self
            .snapshot
            .radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.touched_len.max(index.saturating_add(1)))
            .unwrap_or(self.snapshot.queue.len())
            .min(self.snapshot.queue.len());

        self.apply_jump_to_radio_seed(
            self.snapshot.queue.clone(),
            prefix_len,
            seed_track,
            index.min(self.snapshot.queue.len().saturating_sub(1)),
        )
    }

    fn jump_to_track_radio_seed(&mut self, track: Track) -> Result<()> {
        if !self.can_jump_to_radio() {
            self.status_line = "Jump to needs an active queue or playlist context.".to_string();
            return Ok(());
        }

        let mut queue = self.snapshot.queue.clone();
        let selected_index = if self.snapshot.radio_virtual_playlist.is_some() {
            let insert_at = self.queue_last_slot().min(queue.len());
            queue.insert(insert_at, track.clone());
            insert_at
        } else if let Some(existing_index) = queue
            .iter()
            .position(|queued| queued.video_id == track.video_id)
        {
            existing_index
        } else {
            queue.push(track.clone());
            queue.len().saturating_sub(1)
        };

        let prefix_len = if self.snapshot.radio_virtual_playlist.is_some() {
            self.snapshot
                .radio_virtual_playlist
                .as_ref()
                .map(|radio| {
                    if selected_index < radio.touched_len {
                        radio.touched_len
                    } else {
                        radio.touched_len.saturating_add(1)
                    }
                })
                .unwrap_or(selected_index.saturating_add(1))
                .min(queue.len())
        } else {
            queue.len()
        };

        self.apply_jump_to_radio_seed(queue, prefix_len, track, selected_index)
    }

    fn apply_jump_to_radio_seed(
        &mut self,
        queue: Vec<Track>,
        prefix_len: usize,
        seed_track: Track,
        selected_index: usize,
    ) -> Result<()> {
        let mut final_queue = queue.clone();
        let mut chips = Vec::new();
        let mut tail_refreshed = false;

        if let Ok(mut radio) = saved_network_state()
            .and_then(|network| network.get_radio(&seed_track.video_id, None, None))
        {
            if let Some(seed_index) = radio
                .tracks
                .iter()
                .position(|track| track.video_id == seed_track.video_id)
            {
                if seed_index > 0 {
                    let track = radio.tracks.remove(seed_index);
                    radio.tracks.insert(0, track);
                }
            } else {
                radio.tracks.insert(0, seed_track.clone());
            }

            final_queue.truncate(prefix_len.min(final_queue.len()));
            final_queue.extend(dedupe_radio_recommendations(
                final_queue.as_slice(),
                radio.tracks,
            ));
            chips = radio.chips;
            tail_refreshed = true;
        }

        let current_index = if final_queue.is_empty() {
            0
        } else {
            self.snapshot
                .queue_index
                .min(final_queue.len().saturating_sub(1))
        };
        let snapshot = self.client.replace_queue(
            final_queue,
            current_index,
            self.snapshot.playlist_context.clone(),
            Some(RadioVirtualPlaylist {
                source_video_id: seed_track.video_id.clone(),
                touched_len: prefix_len,
                chips,
            }),
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.focus = TuiFocus::Queue;
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = selected_index.min(self.snapshot.queue.len().saturating_sub(1));
        self.ensure_queue_visible();
        self.status_line = if tail_refreshed {
            format!("Jumped radio to {}.", shorten(&seed_track.title, 28))
        } else {
            format!(
                "Jumped radio to {}. Keeping the current tail for now.",
                shorten(&seed_track.title, 28)
            )
        };
        Ok(())
    }

    fn play_track_now(&mut self, track: Track) -> Result<()> {
        let snapshot = play_track_with_radio(&self.client, track.clone())?;
        self.apply_snapshot(snapshot);
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        self.status_line = format!("Playing {}", format_track_summary(&track));
        Ok(())
    }

    fn open_browse_card_now(&mut self, card: HomeBrowseCard) -> Result<()> {
        self.open_browse_detail(card);
        Ok(())
    }

    fn probe_track_now(&mut self, track: Track) -> Result<()> {
        let snapshot = self.client.probe_track(track.clone())?;
        self.apply_snapshot(snapshot);
        self.status_line = format!("Probing {}", format_track_summary(&track));
        Ok(())
    }

    fn play_queue_index_now(&mut self, index: usize) -> Result<()> {
        if self.snapshot.queue.is_empty() || index >= self.snapshot.queue.len() {
            self.status_line = "Queue item is gone.".to_string();
            return Ok(());
        }

        let snapshot = self.client.play_queue_at(
            self.snapshot.queue.clone(),
            index,
            self.snapshot.playlist_context.clone(),
            self.snapshot.radio_virtual_playlist.clone(),
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.queue_selected = index.min(self.snapshot.queue.len().saturating_sub(1));
        self.ensure_queue_visible();
        self.status_line = format!(
            "Playing {}",
            self.snapshot
                .queue
                .get(index)
                .map(format_track_summary)
                .unwrap_or_else(|| "selection".to_string())
        );
        Ok(())
    }

    fn remove_queue_item(&mut self, index: usize) -> Result<()> {
        if self.snapshot.queue.is_empty() || index >= self.snapshot.queue.len() {
            self.status_line = "Queue row is gone.".to_string();
            return Ok(());
        }
        if index == self.snapshot.queue_index {
            self.status_line = "Remove a non-playing row or drag the queue around it.".to_string();
            return Ok(());
        }

        let mut queue = self.snapshot.queue.clone();
        let removed = queue.remove(index);
        let current_index = if index < self.snapshot.queue_index {
            self.snapshot.queue_index.saturating_sub(1)
        } else {
            self.snapshot.queue_index.min(queue.len().saturating_sub(1))
        };
        let mut radio_virtual_playlist = self.snapshot.radio_virtual_playlist.clone();
        shrink_radio_touched_for_remove(&mut radio_virtual_playlist, index);
        let snapshot = self.client.replace_queue(
            queue,
            current_index,
            self.snapshot.playlist_context.clone(),
            radio_virtual_playlist,
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.queue_selected = index.min(self.snapshot.queue.len().saturating_sub(1));
        self.ensure_queue_visible();
        self.status_line = format!("Removed {}.", shorten(&removed.title, 28));
        Ok(())
    }

    fn submit_feedback_token_action(
        &mut self,
        token: String,
        verb: &'static str,
        title: String,
        video_id: String,
        reveal_undo: bool,
    ) -> Result<()> {
        saved_network_state()?.submit_feedback_token(&token)?;
        if reveal_undo {
            self.feedback_undo_ready.insert(video_id);
        } else {
            self.feedback_undo_ready.remove(&video_id);
        }
        self.status_line = format!("{verb} {}.", shorten(&title, 28));
        Ok(())
    }

    fn execute_context_menu_action(&mut self, action: ContextMenuAction) -> Result<()> {
        match action {
            ContextMenuAction::PlayTrackNow { track } => self.play_track_now(track),
            ContextMenuAction::PlayQueueNow { index } => self.play_queue_index_now(index),
            ContextMenuAction::JumpToTrackRadio { track } => self.jump_to_track_radio_seed(track),
            ContextMenuAction::JumpToQueueRadio { index } => self.jump_to_queue_radio_seed(index),
            ContextMenuAction::QueueTrackNext { track } => {
                self.handle_drag_drop(TuiDragPayload::Track { track }, self.queue_next_slot())
            }
            ContextMenuAction::QueueTrackLast { track } => {
                self.handle_drag_drop(TuiDragPayload::Track { track }, self.queue_last_slot())
            }
            ContextMenuAction::OpenBrowseNow { card } => self.open_browse_card_now(card),
            ContextMenuAction::QueueBrowseNext { card } => self.handle_drag_drop(
                TuiDragPayload::BrowseCollection { card },
                self.queue_next_slot(),
            ),
            ContextMenuAction::QueueBrowseLast { card } => self.handle_drag_drop(
                TuiDragPayload::BrowseCollection { card },
                self.queue_last_slot(),
            ),
            ContextMenuAction::ProbeTrack { track } => self.probe_track_now(track),
            ContextMenuAction::RemoveQueueItem { index } => self.remove_queue_item(index),
            ContextMenuAction::SubmitFeedbackToken {
                token,
                verb,
                title,
                video_id,
                reveal_undo,
            } => self.submit_feedback_token_action(token, verb, title, video_id, reveal_undo),
        }
    }

    fn move_context_menu_selection(&mut self, delta: isize) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        if menu.items.is_empty() {
            menu.selected = 0;
            return;
        }
        menu.selected = move_index(menu.selected, menu.items.len(), delta);
    }

    fn activate_context_menu_selection(&mut self) -> Result<()> {
        let Some(action) = self
            .context_menu
            .as_ref()
            .and_then(|menu| menu.items.get(menu.selected))
            .map(|item| item.action.clone())
        else {
            self.close_context_menu();
            return Ok(());
        };
        self.close_context_menu();
        self.execute_context_menu_action(action)
    }

    fn select_context_menu_item(&mut self, index: usize) {
        if let Some(menu) = self.context_menu.as_mut() {
            menu.selected = index.min(menu.items.len().saturating_sub(1));
        }
    }

    fn render_context_menu(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
    ) -> RenderedContextMenu {
        let Some(menu) = self.context_menu.as_ref() else {
            return RenderedContextMenu::default();
        };

        let title_width = menu.title.chars().count();
        let subtitle_width = menu.subtitle.chars().count();
        let item_width = menu
            .items
            .iter()
            .map(|item| item.label.chars().count() + item.hint.chars().count() + 6)
            .max()
            .unwrap_or(0);
        let width = (title_width.max(subtitle_width).max(item_width) as u16)
            .saturating_add(6)
            .clamp(24, area.width.saturating_sub(2).max(24));
        let body_height = menu.items.len() as u16;
        let height = (body_height + 5).clamp(6, area.height.saturating_sub(2).max(6));
        let x = if menu.anchor_column.saturating_add(width) <= area.right() {
            menu.anchor_column.min(area.right().saturating_sub(width))
        } else {
            area.right().saturating_sub(width)
        }
        .max(area.x.saturating_add(1));
        let y = if menu.anchor_row.saturating_add(height) <= area.bottom() {
            menu.anchor_row.min(area.bottom().saturating_sub(height))
        } else {
            area.bottom().saturating_sub(height)
        }
        .max(area.y.saturating_add(1));
        let menu_rect = Rect::new(x, y, width, height);

        frame.render_widget(Clear, menu_rect);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.border_focus))
                .style(Style::default().bg(theme.elevated_alt_bg)),
            menu_rect,
        );

        let inner = Rect::new(
            menu_rect.x.saturating_add(1),
            menu_rect.y.saturating_add(1),
            menu_rect.width.saturating_sub(2),
            menu_rect.height.saturating_sub(2),
        );
        let item_rows = menu.items.len() as u16;
        let footer_height = 1u16;
        let subtitle_height = 1u16;
        let spacer_height = 1u16;
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(subtitle_height),
                Constraint::Length(item_rows),
                Constraint::Length(spacer_height),
                Constraint::Length(footer_height),
            ])
            .split(inner);

        frame.render_widget(
            Paragraph::new(shorten(&menu.title, rows[0].width as usize))
                .style(Style::default().fg(theme.text).add_modifier(Modifier::BOLD)),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(shorten(&menu.subtitle, rows[1].width as usize))
                .style(Style::default().fg(theme.muted)),
            rows[1],
        );

        let mut item_rects = Vec::with_capacity(menu.items.len());
        for (index, item) in menu.items.iter().enumerate() {
            let row = Rect::new(rows[2].x, rows[2].y + index as u16, rows[2].width, 1);
            let selected = index == menu.selected;
            let (fg, bg) = match (item.tone, selected) {
                (ContextMenuTone::Accent, true) => (theme.accent_glow, theme.accent_fill),
                (ContextMenuTone::Accent, false) => (theme.accent, theme.elevated_alt_bg),
                (ContextMenuTone::Danger, true) => (theme.text, theme.danger),
                (ContextMenuTone::Danger, false) => (theme.danger, theme.elevated_alt_bg),
                (ContextMenuTone::Default, true) => (theme.text, theme.selected_bg),
                (ContextMenuTone::Default, false) => (theme.text, theme.elevated_alt_bg),
            };
            frame.render_widget(Block::default().style(Style::default().bg(bg)), row);
            let label = shorten(
                &format!("{:>2}. {}", index.saturating_add(1), item.label),
                row.width.saturating_sub(item.hint.len() as u16 + 1) as usize,
            );
            let gap = row
                .width
                .saturating_sub(label.chars().count() as u16)
                .saturating_sub(item.hint.chars().count() as u16)
                .max(1) as usize;
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        label,
                        Style::default().fg(fg).bg(bg).add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                    ),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(
                        shorten(&item.hint, row.width as usize / 2),
                        Style::default().fg(theme.muted).bg(bg),
                    ),
                ])),
                row,
            );
            item_rects.push(row);
        }

        frame.render_widget(
            Paragraph::new("enter run  esc close  arrow keys move")
                .style(Style::default().fg(theme.border_focus)),
            rows[4],
        );

        RenderedContextMenu {
            menu_rect: Some(menu_rect),
            item_rects,
        }
    }

    fn render_help_popup(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
    ) -> Option<Rect> {
        if !self.help_visible {
            return None;
        }

        let sections = help_shortcut_sections();
        let label_width = sections
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .map(|(keys, _)| keys.chars().count())
            .max()
            .unwrap_or(6);
        let desc_width = sections
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .map(|(_, desc)| desc.chars().count())
            .max()
            .unwrap_or(12);
        let body_width = (label_width + 2 + desc_width) as u16;
        let title_width = "Keyboard shortcuts".chars().count() as u16;
        let width = body_width
            .max(title_width)
            .saturating_add(6)
            .clamp(32, area.width.saturating_sub(4).max(32));

        let mut body_rows: u16 = 0;
        for (index, (_, rows)) in sections.iter().enumerate() {
            if index > 0 {
                body_rows = body_rows.saturating_add(1);
            }
            body_rows = body_rows.saturating_add(1 + rows.len() as u16);
        }
        let height = (body_rows + 6).clamp(10, area.height.saturating_sub(2).max(10));
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let popup_rect = Rect::new(x, y, width, height);

        frame.render_widget(Clear, popup_rect);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.border_focus))
                .style(Style::default().bg(theme.elevated_alt_bg)),
            popup_rect,
        );

        let inner = Rect::new(
            popup_rect.x.saturating_add(2),
            popup_rect.y.saturating_add(1),
            popup_rect.width.saturating_sub(4),
            popup_rect.height.saturating_sub(2),
        );

        let title_line = Line::from(Span::styled(
            "Keyboard shortcuts",
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD),
        ));
        let subtitle_line = Line::from(Span::styled(
            format!("Thumbnails: {}", self.thumbnail_mode_label),
            Style::default().fg(theme.muted),
        ));

        let mut lines = vec![title_line, subtitle_line, Line::from("")];
        for (section_index, (heading, rows)) in sections.iter().enumerate() {
            if section_index > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                (*heading).to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            for (keys, desc) in rows.iter() {
                let padding = label_width.saturating_sub(keys.chars().count());
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{}{}", keys, " ".repeat(padding)),
                        Style::default()
                            .fg(theme.accent_glow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        (*desc).to_string(),
                        Style::default().fg(theme.text),
                    ),
                ]));
            }
        }

        let footer_height = 2u16;
        let body_area = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(footer_height),
        );
        frame.render_widget(Paragraph::new(lines), body_area);

        let footer = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new("h or esc close")
                .style(Style::default().fg(theme.border_focus)),
            footer,
        );

        Some(popup_rect)
    }

    fn render_frame(&mut self, frame: &mut Frame<'_>) -> (Vec<ClickTarget>, RenderedLayout) {
        let area = frame.area();
        let theme = TuiTheme::from_snapshot(&self.snapshot);
        frame.render_widget(
            Block::default().style(Style::default().bg(theme.canvas)),
            area,
        );

        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.border_focus))
                .title("Static TUI")
                .style(Style::default().bg(theme.content_bg));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        "This terminal surface wants room for the full GTK-inspired layout.",
                        Style::default()
                            .fg(theme.text)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("Current size: {} x {}", area.width, area.height),
                        Style::default().fg(theme.muted),
                    )),
                    Line::from(Span::styled(
                        format!("Recommended: at least {} x {}", MIN_WIDTH, MIN_HEIGHT),
                        Style::default().fg(theme.muted),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Widen the terminal to unlock the full home feed, detail rail, and player bar.",
                        Style::default().fg(theme.accent),
                    )),
                ])
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
                inner,
            );
            self.rendered_context_menu = RenderedContextMenu::default();
            return (Vec::new(), RenderedLayout::default());
        }

        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(60)])
            .split(area);
        let sidebar_area = columns[0];
        let shell_area = columns[1];

        let shell_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(18), Constraint::Length(PLAYER_BAR_HEIGHT)])
            .split(shell_area);
        let upper_area = shell_rows[0];
        let player_area = shell_rows[1];

        let detail_width = DETAIL_WIDTH
            .min(upper_area.width.saturating_sub(42))
            .max(30);
        let upper_columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(detail_width)])
            .split(upper_area);
        let content_area = upper_columns[0];
        let detail_area = upper_columns[1];

        let mut click_targets = Vec::new();
        let mut layout = RenderedLayout::default();

        click_targets.extend(self.render_sidebar(frame, sidebar_area, &theme, &mut layout));
        click_targets.extend(self.render_content(frame, content_area, &theme, &mut layout));
        click_targets.extend(self.render_detail_panel(frame, detail_area, &theme, &mut layout));
        click_targets.extend(self.render_player_bar(frame, player_area, &theme, &mut layout));
        self.rendered_context_menu = self.render_context_menu(frame, area, &theme);
        self.rendered_help_rect = self.render_help_popup(frame, area, &theme);

        (click_targets, layout)
    }

    fn render_sidebar(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
        layout: &mut RenderedLayout,
    ) -> Vec<ClickTarget> {
        let mut targets = Vec::new();
        let block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.sidebar_bg));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(11),
                Constraint::Length(2),
                Constraint::Min(8),
                Constraint::Length(3),
            ])
            .split(inner);

        let search_box = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.elevated_bg))
            .padding(Padding::new(1, 1, 0, 0));
        let search_inner = search_box.inner(rows[0]);
        frame.render_widget(search_box, rows[0]);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Search songs...",
                    Style::default().fg(theme.muted),
                )),
                Line::from(Span::styled(
                    "Use `static exec <query>` for now",
                    Style::default().fg(theme.border_focus),
                )),
            ]),
            search_inner,
        );

        let nav_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.content_alt_bg))
            .padding(Padding::new(1, 1, 0, 0));
        let nav_inner = nav_block.inner(rows[1]);
        frame.render_widget(nav_block, rows[1]);
        let nav_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(2),
            ])
            .split(nav_inner);
        let nav_items = [
            ("Home", TuiNavItem::Home, true),
            ("Likes", TuiNavItem::Likes, false),
            ("Playlists", TuiNavItem::Playlists, false),
            ("Offline", TuiNavItem::Offline, false),
        ];
        for (row, (label, nav, active)) in nav_rows.into_iter().zip(nav_items) {
            let row_block = Block::default()
                .style(Style::default().bg(if active {
                    theme.accent_fill
                } else {
                    theme.content_alt_bg
                }))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if active {
                    theme.border_focus
                } else {
                    theme.border_subtle
                }));
            let row_inner = row_block.inner(*row);
            frame.render_widget(row_block, *row);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        if active { "● " } else { "○ " },
                        Style::default().fg(if active { theme.accent } else { theme.border }),
                    ),
                    Span::styled(
                        label,
                        Style::default()
                            .fg(if active {
                                theme.accent_glow
                            } else {
                                theme.text
                            })
                            .add_modifier(if active {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ])),
                row_inner,
            );
            targets.push(ClickTarget {
                rect: *row,
                kind: ClickTargetKind::Nav(nav),
            });
        }

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Recently Played",
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            ))),
            rows[2],
        );

        let recent_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if self.focus == TuiFocus::Recent {
                theme.border_focus
            } else {
                theme.border_subtle
            }))
            .style(Style::default().bg(theme.elevated_bg));
        let recent_inner = recent_block.inner(rows[3]);
        frame.render_widget(recent_block, rows[3]);
        layout.recent_viewport = recent_inner;
        self.recent_viewport_rows =
            (recent_inner.height as usize / RECENT_ROW_HEIGHT as usize).max(1);

        if self.recent_tracks.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Waiting on YouTube history...",
                    Style::default().fg(theme.muted),
                )))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
                recent_inner,
            );
        } else {
            self.ensure_recent_visible();
            let end = self
                .recent_scroll_offset
                .saturating_add(self.recent_viewport_rows.max(1))
                .min(self.recent_tracks.len());
            for (offset, track) in self.recent_tracks[self.recent_scroll_offset..end]
                .iter()
                .enumerate()
            {
                let absolute_index = self.recent_scroll_offset + offset;
                let row_area = Rect::new(
                    recent_inner.x,
                    recent_inner.y + offset as u16 * RECENT_ROW_HEIGHT,
                    recent_inner.width,
                    RECENT_ROW_HEIGHT,
                );
                let is_selected = absolute_index == self.recent_selected;
                let is_current = self
                    .snapshot
                    .current_track
                    .as_ref()
                    .is_some_and(|current| current.video_id == track.video_id);
                frame.render_widget(
                    Block::default().style(Style::default().bg(if is_selected {
                        theme.selected_bg
                    } else {
                        theme.elevated_bg
                    })),
                    row_area,
                );
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(vec![
                            Span::styled(
                                if is_current {
                                    "▶ "
                                } else if is_selected {
                                    "▸ "
                                } else {
                                    "  "
                                },
                                Style::default().fg(if is_current {
                                    theme.accent
                                } else if is_selected {
                                    theme.border_focus
                                } else {
                                    theme.border
                                }),
                            ),
                            Span::styled(
                                shorten(&track.title, row_area.width.saturating_sub(3) as usize),
                                Style::default()
                                    .fg(theme.text)
                                    .add_modifier(if is_selected {
                                        Modifier::BOLD
                                    } else {
                                        Modifier::empty()
                                    }),
                            ),
                        ]),
                        Line::from(Span::styled(
                            shorten(
                                &artist_string(track),
                                row_area.width.saturating_sub(2) as usize,
                            ),
                            Style::default().fg(theme.muted),
                        )),
                    ]),
                    row_area,
                );
                targets.push(ClickTarget {
                    rect: row_area,
                    kind: ClickTargetKind::RecentItem(absolute_index),
                });
            }

            let mut scrollbar_state =
                ScrollbarState::new(self.recent_tracks.len()).position(self.recent_scroll_offset);
            frame.render_stateful_widget(
                Scrollbar::default()
                    .orientation(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent)),
                rows[3],
                &mut scrollbar_state,
            );
        }

        let settings_row = rows[4];
        let settings_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.content_alt_bg));
        let settings_inner = settings_block.inner(settings_row);
        frame.render_widget(settings_block, settings_row);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Settings", Style::default().fg(theme.text)),
                Span::raw("  "),
                Span::styled("open GTK for full config", Style::default().fg(theme.muted)),
            ])),
            settings_inner,
        );
        targets.push(ClickTarget {
            rect: settings_row,
            kind: ClickTargetKind::Nav(TuiNavItem::Settings),
        });

        targets
    }

    fn render_content(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
        layout: &mut RenderedLayout,
    ) -> Vec<ClickTarget> {
        let mut targets = Vec::new();
        let shell_block = Block::default()
            .style(Style::default().bg(theme.content_bg))
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(theme.border_subtle));
        let inner = shell_block.inner(area);
        frame.render_widget(shell_block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(8)])
            .split(inner);

        let header = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.content_alt_bg));
        let header_inner = header.inner(rows[0]);
        frame.render_widget(header, rows[0]);
        self.thumbnail_slots.remove(&ThumbnailSlot::ContentHeader);

        let browse_mode = matches!(&self.content_view, TuiContentView::Browse(_));
        let header_label = match &self.content_view {
            TuiContentView::Home => "S  T  A  T  I  C".to_string(),
            TuiContentView::Browse(view) => format!(
                "S  T  A  T  I  C   /   {}",
                browse_label(view.card.page_type).to_uppercase()
            ),
        };
        if browse_mode {
            let header_columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(10),
                    Constraint::Min(12),
                    Constraint::Length(12),
                ])
                .split(header_inner);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "[← Back]",
                    Style::default()
                        .fg(theme.accent_glow)
                        .add_modifier(Modifier::BOLD),
                )))
                .alignment(Alignment::Left),
                header_columns[0],
            );
            targets.push(ClickTarget {
                rect: header_columns[0],
                kind: ClickTargetKind::ContentBack,
            });
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    header_label,
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                )))
                .alignment(Alignment::Left),
                header_columns[1],
            );
            frame.render_widget(
                Paragraph::new(Local::now().format("%a %H:%M").to_string())
                    .style(Style::default().fg(theme.muted))
                    .alignment(Alignment::Right),
                header_columns[2],
            );
        } else {
            let header_columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(16), Constraint::Length(12)])
                .split(header_inner);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    header_label,
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                )))
                .alignment(Alignment::Left),
                header_columns[0],
            );
            frame.render_widget(
                Paragraph::new(Local::now().format("%a %H:%M").to_string())
                    .style(Style::default().fg(theme.muted))
                    .alignment(Alignment::Right),
                header_columns[1],
            );
        }

        let feed_block = Block::default()
            .style(Style::default().bg(theme.content_bg))
            .padding(Padding::new(1, 1, 0, 0));
        let feed_inner = feed_block.inner(rows[1]);
        frame.render_widget(feed_block, rows[1]);

        layout.home_viewport = feed_inner;
        self.home_viewport_rows = feed_inner.height as usize;
        self.home_doc = match &self.content_view {
            TuiContentView::Home => build_home_document(
                &self.home_status,
                feed_inner.width,
                self.visible_resume_card(),
            ),
            TuiContentView::Browse(view) => build_browse_document(view, feed_inner.width),
        };
        if !self.home_doc.cards.is_empty() {
            self.home_selected = self
                .home_selected
                .min(self.home_doc.cards.len().saturating_sub(1));
        } else {
            self.home_selected = 0;
            self.home_scroll_offset = 0;
        }
        self.clamp_home_scroll_offset();
        self.maybe_request_more_home();

        frame.render_widget(
            HomeFeedWidget {
                doc: self.home_doc.clone(),
                scroll_offset: self.home_scroll_offset,
                selected_index: self.home_selected,
                focus: self.focus == TuiFocus::Home,
                theme: theme.clone(),
                show_thumbnail_wells: self.terminal_images_enabled(),
                current_video_id: self
                    .snapshot
                    .current_track
                    .as_ref()
                    .map(|track| track.video_id.clone()),
            },
            feed_inner,
        );

        let visible_top = self.home_scroll_offset;
        let visible_bottom = visible_top.saturating_add(feed_inner.height as usize);
        let resume_banner_clickable = matches!(&self.content_view, TuiContentView::Home)
            && self.visible_resume_card().is_some();
        for (banner_index, banner) in self.home_doc.banners.iter().enumerate() {
            let banner_top = banner.rect.y as usize;
            let banner_bottom = banner_top.saturating_add(banner.rect.height as usize);
            if banner_bottom <= visible_top || banner_top >= visible_bottom {
                continue;
            }
            let screen_rect = Rect::new(
                feed_inner.x + banner.rect.x,
                feed_inner
                    .y
                    .saturating_add(banner.rect.y.saturating_sub(visible_top as u16)),
                banner.rect.width,
                banner.rect.height,
            );
            if resume_banner_clickable && banner_index == 0 {
                if let Some(visible_rect) = intersect_rect(screen_rect, feed_inner) {
                    targets.push(ClickTarget {
                        rect: visible_rect,
                        kind: ClickTargetKind::Action(TuiAction::ResumeSaved),
                    });
                }
            }
        }
        for header in &self.home_doc.headers {
            let header_y = feed_inner.y as i32 + header.y as i32 - visible_top as i32;
            if header_y < feed_inner.y as i32 || header_y >= feed_inner.bottom() as i32 {
                continue;
            }
            let Some(layout) = home_header_actions_layout(feed_inner, header) else {
                continue;
            };
            targets.push(ClickTarget {
                rect: Rect::new(layout.play_x, header_y as u16, layout.play_width, 1),
                kind: ClickTargetKind::HomeSectionAction {
                    section_index: header.section_index,
                    shuffle: false,
                },
            });
            targets.push(ClickTarget {
                rect: Rect::new(layout.shuffle_x, header_y as u16, layout.shuffle_width, 1),
                kind: ClickTargetKind::HomeSectionAction {
                    section_index: header.section_index,
                    shuffle: true,
                },
            });
        }
        for (index, card) in self.home_doc.cards.iter().enumerate() {
            let card_top = card.rect.y as usize;
            let card_bottom = card_top.saturating_add(card.rect.height as usize);
            if card_bottom <= visible_top || card_top >= visible_bottom {
                continue;
            }
            let screen_rect = Rect::new(
                feed_inner.x + card.rect.x,
                feed_inner
                    .y
                    .saturating_add(card.rect.y.saturating_sub(visible_top as u16)),
                card.rect.width,
                card.rect.height,
            );
            if let Some(visible_rect) = intersect_rect(screen_rect, feed_inner) {
                targets.push(ClickTarget {
                    rect: visible_rect,
                    kind: ClickTargetKind::HomeItem(index),
                });
            }
        }

        let mut scrollbar_state = ScrollbarState::new(self.home_doc.total_height.max(1))
            .position(self.home_scroll_offset);
        frame.render_stateful_widget(
            Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(theme.accent)),
            rows[1],
            &mut scrollbar_state,
        );

        if self.terminal_images_enabled() {
            for index in 0..self.home_doc.banners.len() {
                let banner = self.home_doc.banners[index].clone();
                let Some(url) = banner.thumbnail_url.as_deref() else {
                    continue;
                };
                let banner_top = banner.rect.y as usize;
                let banner_bottom = banner_top.saturating_add(banner.rect.height as usize);
                if banner_bottom <= visible_top || banner_top >= visible_bottom {
                    continue;
                }
                let screen_rect = Rect::new(
                    feed_inner.x + banner.rect.x,
                    feed_inner
                        .y
                        .saturating_add(banner.rect.y.saturating_sub(visible_top as u16)),
                    banner.rect.width,
                    banner.rect.height,
                );
                let Some(thumb_rect) = home_banner_thumbnail_rect(screen_rect) else {
                    continue;
                };
                self.render_cover_thumbnail(
                    frame,
                    thumb_rect,
                    ThumbnailSlot::ContentBanner(index),
                    Some(url),
                    placeholder_initials(&banner.title, &banner.subtitle),
                    theme,
                    banner.accent,
                );
            }
            for index in 0..self.home_doc.cards.len() {
                let card = self.home_doc.cards[index].clone();
                let card_top = card.rect.y as usize;
                let card_bottom = card_top.saturating_add(card.rect.height as usize);
                if card_bottom <= visible_top || card_top >= visible_bottom {
                    continue;
                }
                let screen_rect = Rect::new(
                    feed_inner.x + card.rect.x,
                    feed_inner
                        .y
                        .saturating_add(card.rect.y.saturating_sub(visible_top as u16)),
                    card.rect.width,
                    card.rect.height,
                );
                let Some(thumb_rect) = home_card_thumbnail_rect(screen_rect, card.style) else {
                    continue;
                };
                self.render_home_card_thumbnail(frame, thumb_rect, feed_inner, index, &card, theme);
            }
        }

        targets
    }

    fn render_detail_panel(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
        layout: &mut RenderedLayout,
    ) -> Vec<ClickTarget> {
        let mut targets = Vec::new();
        let panel = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(if self.focus == TuiFocus::Queue {
                theme.border_focus
            } else {
                theme.border_subtle
            }))
            .style(Style::default().bg(theme.detail_bg));
        let inner = panel.inner(area);
        frame.render_widget(panel, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(12)])
            .split(inner);

        let tab_columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Min(6),
            ])
            .split(rows[0]);
        for (area, tab, label) in [
            (tab_columns[0], DetailTab::Queue, "Up Next"),
            (tab_columns[1], DetailTab::Lyrics, "Lyrics"),
        ] {
            let active = self.detail_tab == tab;
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if active {
                    theme.border_focus
                } else {
                    theme.border_subtle
                }))
                .style(Style::default().bg(if active {
                    theme.accent_fill
                } else {
                    theme.elevated_bg
                }));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(if active {
                            theme.accent_glow
                        } else {
                            theme.muted
                        })
                        .add_modifier(if active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                )))
                .alignment(Alignment::Center),
                inner,
            );
            targets.push(ClickTarget {
                rect: area,
                kind: ClickTargetKind::DetailTab(tab),
            });
        }

        match self.detail_tab {
            DetailTab::Queue => {
                let body_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(2), Constraint::Min(6)])
                    .split(rows[1]);

                let chip_labels = queue_chip_labels(&self.snapshot);
                frame.render_widget(
                    Paragraph::new(Line::from(
                        chip_labels
                            .into_iter()
                            .enumerate()
                            .flat_map(|(index, label)| {
                                let mut spans = Vec::new();
                                if index > 0 {
                                    spans.push(Span::raw(" "));
                                }
                                spans.push(Span::styled(
                                    format!(" {} ", label),
                                    Style::default()
                                        .fg(if index == 0 {
                                            theme.accent_glow
                                        } else {
                                            theme.text
                                        })
                                        .bg(if index == 0 {
                                            theme.accent_fill
                                        } else {
                                            theme.elevated_bg
                                        }),
                                ));
                                spans
                            })
                            .collect::<Vec<_>>(),
                    )),
                    body_rows[0],
                );

                let queue_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(
                        if self.drag_state.is_some() && self.queue_accepts_dnd() {
                            theme.accent
                        } else if self.focus == TuiFocus::Queue {
                            theme.border_focus
                        } else {
                            theme.border_subtle
                        },
                    ))
                    .style(Style::default().bg(theme.elevated_bg));
                let queue_inner = queue_block.inner(body_rows[1]);
                frame.render_widget(queue_block, body_rows[1]);
                layout.queue_viewport = queue_inner;
                self.queue_viewport_rows =
                    (queue_inner.height as usize / QUEUE_ROW_HEIGHT as usize).max(1);

                if self.snapshot.queue.is_empty() {
                    frame.render_widget(
                        Paragraph::new(if self.drag_state.is_some() && self.queue_accepts_dnd() {
                            "Drop into queue."
                        } else {
                            "Queue is empty."
                        })
                        .alignment(Alignment::Center)
                        .style(Style::default().fg(theme.muted)),
                        queue_inner,
                    );
                } else {
                    self.ensure_queue_visible();
                    let end = self
                        .queue_scroll_offset
                        .saturating_add(self.queue_viewport_rows.max(1))
                        .min(self.snapshot.queue.len());

                    for (offset, track) in self.snapshot.queue[self.queue_scroll_offset..end]
                        .iter()
                        .enumerate()
                    {
                        let absolute_index = self.queue_scroll_offset + offset;
                        let row_area = Rect::new(
                            queue_inner.x,
                            queue_inner.y + offset as u16 * QUEUE_ROW_HEIGHT,
                            queue_inner.width,
                            QUEUE_ROW_HEIGHT,
                        );
                        let is_selected = absolute_index == self.queue_selected;
                        let is_current = absolute_index == self.snapshot.queue_index;
                        let is_dragging_row = self
                            .drag_state
                            .as_ref()
                            .is_some_and(|drag| matches!(drag.payload, TuiDragPayload::QueueRow { index } if index == absolute_index));
                        let is_generated = self
                            .snapshot
                            .radio_virtual_playlist
                            .as_ref()
                            .is_some_and(|radio| absolute_index >= radio.touched_len);
                        frame.render_widget(
                            Block::default().style(Style::default().bg(if is_dragging_row {
                                theme.accent_fill
                            } else if is_selected {
                                theme.selected_bg
                            } else if is_current {
                                theme.content_alt_bg
                            } else {
                                theme.elevated_bg
                            })),
                            row_area,
                        );
                        frame.render_widget(
                            Paragraph::new(vec![
                                Line::from(vec![
                                    Span::styled(
                                        if is_current {
                                            if self.snapshot.paused { "⏸ " } else { "▶ " }
                                        } else if is_generated {
                                            "≈ "
                                        } else if is_selected {
                                            "▸ "
                                        } else {
                                            "  "
                                        },
                                        Style::default().fg(if is_current {
                                            theme.accent
                                        } else if is_generated {
                                            theme.border_focus
                                        } else {
                                            theme.border
                                        }),
                                    ),
                                    Span::styled(
                                        format!("{:>2}. ", absolute_index.saturating_add(1)),
                                        Style::default().fg(theme.muted),
                                    ),
                                    Span::styled(
                                        shorten(
                                            &track.title,
                                            row_area.width.saturating_sub(6) as usize,
                                        ),
                                        Style::default().fg(theme.text).add_modifier(
                                            if is_selected {
                                                Modifier::BOLD
                                            } else {
                                                Modifier::empty()
                                            },
                                        ),
                                    ),
                                ]),
                                Line::from(Span::styled(
                                    shorten(
                                        &artist_string(track),
                                        row_area.width.saturating_sub(2) as usize,
                                    ),
                                    Style::default().fg(theme.muted),
                                )),
                                Line::from(vec![
                                    Span::styled(
                                        if is_generated {
                                            "Radio generated"
                                        } else {
                                            "Queue"
                                        },
                                        Style::default().fg(if is_generated {
                                            theme.accent
                                        } else {
                                            theme.border
                                        }),
                                    ),
                                    Span::raw("  "),
                                    Span::styled(
                                        track
                                            .duration
                                            .clone()
                                            .unwrap_or_else(|| "--:--".to_string()),
                                        Style::default().fg(theme.muted),
                                    ),
                                ]),
                            ]),
                            row_area,
                        );
                        targets.push(ClickTarget {
                            rect: row_area,
                            kind: ClickTargetKind::QueueItem(absolute_index),
                        });
                    }

                    let mut scrollbar_state = ScrollbarState::new(self.snapshot.queue.len())
                        .position(self.queue_scroll_offset);
                    frame.render_stateful_widget(
                        Scrollbar::default()
                            .orientation(ScrollbarOrientation::VerticalRight)
                            .thumb_style(Style::default().fg(theme.accent)),
                        body_rows[1],
                        &mut scrollbar_state,
                    );

                    if let Some(raw_slot) = self.current_preview_slot() {
                        render_queue_drop_indicator(
                            frame,
                            queue_inner,
                            raw_slot,
                            self.queue_scroll_offset,
                            end,
                            self.snapshot.queue.len(),
                            theme,
                            self.drag_state
                                .as_ref()
                                .map(|drag| Self::drag_label(&drag.payload))
                                .unwrap_or_else(|| "Drop".to_string()),
                        );
                    }
                }
            }
            DetailTab::Lyrics => {
                let lyrics_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(6), Constraint::Length(4)])
                    .split(rows[1]);
                let lyrics_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(if self.focus == TuiFocus::Queue {
                        theme.border_focus
                    } else {
                        theme.border_subtle
                    }))
                    .style(Style::default().bg(theme.elevated_bg));
                let lyrics_inner = lyrics_block.inner(lyrics_rows[0]);
                frame.render_widget(lyrics_block, lyrics_rows[0]);
                layout.queue_viewport = lyrics_inner;

                match &self.lyrics_state {
                    LyricsState::Idle => {
                        frame.render_widget(
                            Paragraph::new(vec![
                                Line::from(Span::styled(
                                    "Play something to see synced lyrics.",
                                    Style::default().fg(theme.text),
                                )),
                                Line::from(Span::styled(
                                    "When YouTube exposes timed text, this panel will follow along.",
                                    Style::default().fg(theme.muted),
                                )),
                            ])
                            .wrap(Wrap { trim: true }),
                            lyrics_inner,
                        );
                    }
                    LyricsState::Loading { video_id } => {
                        frame.render_widget(
                            Paragraph::new(vec![
                                Line::from(Span::styled(
                                    "Syncing lyrics...",
                                    Style::default()
                                        .fg(theme.accent)
                                        .add_modifier(Modifier::BOLD),
                                )),
                                Line::from(Span::styled(
                                    video_id,
                                    Style::default().fg(theme.muted),
                                )),
                            ]),
                            lyrics_inner,
                        );
                    }
                    LyricsState::Missing { reason, .. } => {
                        frame.render_widget(
                            Paragraph::new(reason.as_str())
                                .wrap(Wrap { trim: true })
                                .style(Style::default().fg(theme.muted)),
                            lyrics_inner,
                        );
                    }
                    LyricsState::Error { message, .. } => {
                        frame.render_widget(
                            Paragraph::new(message.as_str())
                                .wrap(Wrap { trim: true })
                                .style(Style::default().fg(theme.danger)),
                            lyrics_inner,
                        );
                    }
                    LyricsState::Ready { lyrics, .. } => {
                        let inner_rows = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([Constraint::Length(2), Constraint::Min(6)])
                            .split(lyrics_inner);
                        frame.render_widget(
                            Paragraph::new(vec![
                                Line::from(Span::styled(
                                    lyrics_meta_label(lyrics),
                                    Style::default()
                                        .fg(theme.accent)
                                        .add_modifier(Modifier::BOLD),
                                )),
                                Line::from(Span::styled(
                                    self.snapshot
                                        .current_track
                                        .as_ref()
                                        .map(|track| format!("{} — {}", track.title, track.artist))
                                        .unwrap_or_else(|| "Current track".to_string()),
                                    Style::default().fg(theme.muted),
                                )),
                            ]),
                            inner_rows[0],
                        );

                        let active_index = active_lyric_line_index(
                            lyrics,
                            self.displayed_position_millis().unwrap_or(0),
                        );
                        let visible_lines = inner_rows[1].height.max(1) as usize;
                        let start = active_index.saturating_sub(visible_lines / 2);
                        let end = start.saturating_add(visible_lines).min(lyrics.lines.len());
                        let items = lyrics.lines[start..end]
                            .iter()
                            .enumerate()
                            .map(|(offset, line)| {
                                let absolute_index = start + offset;
                                let active = absolute_index == active_index;
                                ListItem::new(Line::from(Span::styled(
                                    shorten(&line.text, inner_rows[1].width as usize),
                                    Style::default()
                                        .fg(if active { theme.text } else { theme.muted })
                                        .add_modifier(if active {
                                            Modifier::BOLD
                                        } else {
                                            Modifier::empty()
                                        }),
                                )))
                            })
                            .collect::<Vec<_>>();
                        frame.render_widget(List::new(items), inner_rows[1]);
                    }
                }

                let footer_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.border_subtle))
                    .style(Style::default().bg(theme.elevated_alt_bg));
                let footer_inner = footer_block.inner(lyrics_rows[1]);
                frame.render_widget(footer_block, lyrics_rows[1]);
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(
                            "Timed text follows current playback position.",
                            Style::default().fg(theme.text),
                        )),
                        Line::from(Span::styled(
                            "Press 1 for Up Next, 2 for Lyrics, or click the tabs.",
                            Style::default().fg(theme.muted),
                        )),
                    ]),
                    footer_inner,
                );
            }
        }

        targets
    }

    fn render_player_bar(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
        layout: &mut RenderedLayout,
    ) -> Vec<ClickTarget> {
        let mut targets = Vec::new();
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.player_bg));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(4)])
            .split(inner);

        let progress_ratio = self
            .snapshot
            .duration_millis
            .filter(|duration| *duration > 0)
            .map(|duration| {
                self.displayed_position_millis().unwrap_or(0).min(duration) as f64 / duration as f64
            })
            .unwrap_or(0.0);
        layout.player_progress_viewport = rows[0];
        frame.render_widget(
            Gauge::default()
                .ratio(progress_ratio)
                .gauge_style(Style::default().fg(theme.red).bg(theme.border_subtle))
                .label(format!(
                    "{} / {}",
                    self.displayed_position_millis()
                        .map(format_millis)
                        .unwrap_or_else(|| "0:00".to_string()),
                    self.snapshot
                        .duration_millis
                        .map(format_millis)
                        .unwrap_or_else(|| "--:--".to_string())
                )),
            rows[0],
        );

        let control_columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(16),
                Constraint::Min(26),
                Constraint::Length(28),
            ])
            .split(rows[1]);

        let transport = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(5),
                Constraint::Length(6),
                Constraint::Length(5),
            ])
            .split(control_columns[0]);
        targets.push(render_action_button(
            frame,
            transport[0],
            "⏮",
            TuiAction::Previous,
            theme,
            false,
            false,
            self.player_action_focused(0),
        ));
        targets.push(render_action_button(
            frame,
            transport[1],
            if self.snapshot.paused { "▶" } else { "⏸" },
            TuiAction::TogglePause,
            theme,
            true,
            true,
            self.player_action_focused(1),
        ));
        targets.push(render_action_button(
            frame,
            transport[2],
            "⏭",
            TuiAction::Next,
            theme,
            false,
            false,
            self.player_action_focused(2),
        ));

        let track_info = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(self.square_box_width_for_height(rows[1].height).max(6)),
                Constraint::Min(16),
            ])
            .split(control_columns[1]);

        let current_thumb_url = self
            .snapshot
            .current_track
            .as_ref()
            .and_then(|track| track.thumb_url.clone());
        self.render_cover_thumbnail(
            frame,
            track_info[0],
            ThumbnailSlot::PlayerBar,
            current_thumb_url.as_deref(),
            current_initials(&self.snapshot),
            theme,
            true,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    shorten(
                        &self
                            .snapshot
                            .current_track
                            .as_ref()
                            .map(|track| track.title.clone())
                            .unwrap_or_else(|| "Nothing playing".to_string()),
                        track_info[1].width.saturating_sub(1) as usize,
                    ),
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    shorten(
                        &self
                            .snapshot
                            .current_track
                            .as_ref()
                            .map(|track| track.artist.clone())
                            .unwrap_or_else(|| "Static daemon is ready".to_string()),
                        track_info[1].width.saturating_sub(1) as usize,
                    ),
                    Style::default().fg(theme.muted),
                )),
                Line::from(player_bar_meta_spans(&self.snapshot)),
            ])
            .alignment(Alignment::Left),
            track_info[1],
        );

        let actions = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Length(4),
                Constraint::Length(5),
                Constraint::Length(4),
            ])
            .split(control_columns[2]);

        targets.push(render_action_button(
            frame,
            actions[0],
            "👎",
            TuiAction::Dislike,
            theme,
            false,
            self.snapshot
                .current_track
                .as_ref()
                .is_some_and(|track| track.rating == SongRating::Dislike),
            self.player_action_focused(3),
        ));
        targets.push(render_action_button(
            frame,
            actions[1],
            "👍",
            TuiAction::Like,
            theme,
            false,
            self.snapshot
                .current_track
                .as_ref()
                .is_some_and(|track| track.rating == SongRating::Like),
            self.player_action_focused(4),
        ));
        targets.push(render_action_button(
            frame,
            actions[2],
            repeat_button_emoji(self.snapshot.repeat_mode),
            TuiAction::CycleRepeat,
            theme,
            false,
            self.snapshot.repeat_mode != RepeatMode::Off,
            self.player_action_focused(5),
        ));
        targets.push(render_action_button(
            frame,
            actions[3],
            "−",
            TuiAction::VolumeDown,
            theme,
            false,
            false,
            self.player_action_focused(6),
        ));

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    format!("{:.0}%", (self.snapshot.volume * 100.0).clamp(0.0, 100.0)),
                    Style::default().fg(theme.text),
                )),
                Line::from(Span::styled("vol", Style::default().fg(theme.muted))),
            ])
            .alignment(Alignment::Center),
            actions[4],
        );

        targets.push(render_action_button(
            frame,
            actions[5],
            "+",
            TuiAction::VolumeUp,
            theme,
            false,
            false,
            self.player_action_focused(7),
        ));

        targets
    }

    fn handle_key(&mut self, code: KeyCode) -> Result<bool> {
        if self.context_menu.is_some() {
            match code {
                KeyCode::Esc => {
                    self.close_context_menu();
                    self.status_line = "Context menu closed.".to_string();
                    return Ok(false);
                }
                KeyCode::Up => {
                    self.move_context_menu_selection(-1);
                    return Ok(false);
                }
                KeyCode::Down => {
                    self.move_context_menu_selection(1);
                    return Ok(false);
                }
                KeyCode::Home => {
                    self.select_context_menu_item(0);
                    return Ok(false);
                }
                KeyCode::End => {
                    let last = self
                        .context_menu
                        .as_ref()
                        .map(|menu| menu.items.len().saturating_sub(1))
                        .unwrap_or(0);
                    self.select_context_menu_item(last);
                    return Ok(false);
                }
                KeyCode::Enter => {
                    self.activate_context_menu_selection()?;
                    return Ok(false);
                }
                KeyCode::Char(ch) if ('1'..='9').contains(&ch) => {
                    let index = ch as usize - '1' as usize;
                    if self
                        .context_menu
                        .as_ref()
                        .is_some_and(|menu| index < menu.items.len())
                    {
                        self.select_context_menu_item(index);
                        self.activate_context_menu_selection()?;
                    }
                    return Ok(false);
                }
                _ => {}
            }
        }

        if self.help_visible {
            match code {
                KeyCode::Esc | KeyCode::Char('h') | KeyCode::Char('q') => {
                    self.help_visible = false;
                    self.rendered_help_rect = None;
                    return Ok(false);
                }
                _ => return Ok(false),
            }
        }

        match code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('h') => {
                self.help_visible = true;
            }
            KeyCode::Backspace | KeyCode::Char('b')
                if !matches!(&self.content_view, TuiContentView::Home) =>
            {
                self.go_back_content();
            }
            KeyCode::Tab => self.cycle_focus(),
            KeyCode::Char(' ') => self.run_action(TuiAction::TogglePause)?,
            KeyCode::Char('r') => self.run_action(TuiAction::ResumeSaved)?,
            KeyCode::Char('1') => self.detail_tab = DetailTab::Queue,
            KeyCode::Char('2') => self.detail_tab = DetailTab::Lyrics,
            KeyCode::Char('t') => self.toggle_detail_tab(),
            KeyCode::Char('n') => self.run_action(TuiAction::Next)?,
            KeyCode::Char('p') => self.run_action(TuiAction::Previous)?,
            KeyCode::Char('l') => self.run_action(TuiAction::CycleRepeat)?,
            KeyCode::Char('f') => self.run_action(TuiAction::Like)?,
            KeyCode::Char('d') => self.run_action(TuiAction::Dislike)?,
            KeyCode::Char('u') => self.run_action(TuiAction::Neutral)?,
            KeyCode::Char('-') | KeyCode::Char('_') => self.run_action(TuiAction::VolumeDown)?,
            KeyCode::Char('+') | KeyCode::Char('=') => self.run_action(TuiAction::VolumeUp)?,
            KeyCode::Char('m') => self.open_context_menu_for_focus(),
            KeyCode::Left => self.move_selection(NavigationDirection::Left),
            KeyCode::Right => self.move_selection(NavigationDirection::Right),
            KeyCode::Up => {
                if matches!(self.focus, TuiFocus::Player) {
                    self.run_action(TuiAction::VolumeUp)?;
                } else {
                    self.move_selection(NavigationDirection::Up);
                }
            }
            KeyCode::Down => {
                if matches!(self.focus, TuiFocus::Player) {
                    self.run_action(TuiAction::VolumeDown)?;
                } else {
                    self.move_selection(NavigationDirection::Down);
                }
            }
            KeyCode::Home => self.jump_to_start(),
            KeyCode::End => self.jump_to_end(),
            KeyCode::PageDown => self.page_move(1),
            KeyCode::PageUp => self.page_move(-1),
            KeyCode::Enter => self.activate_selection()?,
            _ => {}
        }

        Ok(false)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll_panel(mouse.column, mouse.row, 2),
            MouseEventKind::ScrollUp => self.scroll_panel(mouse.column, mouse.row, -2),
            MouseEventKind::Down(MouseButton::Left) => {
                if self.help_visible {
                    let clicked_inside_help = self
                        .rendered_help_rect
                        .is_some_and(|rect| contains(rect, mouse.column, mouse.row));
                    if !clicked_inside_help {
                        self.help_visible = false;
                        self.rendered_help_rect = None;
                    }
                    return Ok(());
                }
                let clicked_menu_item = self
                    .rendered_context_menu
                    .item_rects
                    .iter()
                    .position(|rect| contains(*rect, mouse.column, mouse.row));
                let clicked_inside_menu = self
                    .rendered_context_menu
                    .menu_rect
                    .is_some_and(|rect| contains(rect, mouse.column, mouse.row));
                if let Some(index) = clicked_menu_item {
                    self.select_context_menu_item(index);
                    self.activate_context_menu_selection()?;
                    return Ok(());
                }
                if self.context_menu.is_some() {
                    self.close_context_menu();
                    if clicked_inside_menu {
                        return Ok(());
                    }
                }
                self.drag_state = None;
                if let Some(target) = self
                    .click_targets
                    .iter()
                    .find(|target| contains(target.rect, mouse.column, mouse.row))
                    .cloned()
                {
                    if let Some(payload) = self.drag_payload_for_target(&target.kind) {
                        self.set_mouse_selection(&target.kind);
                        self.pending_mouse_press = Some(PendingMousePress {
                            origin_column: mouse.column,
                            origin_row: mouse.row,
                            target_kind: target.kind,
                            target_rect: target.rect,
                            payload: Some(payload),
                        });
                    } else {
                        self.pending_mouse_press = None;
                        self.activate_click_target(&target.kind)?;
                    }
                } else if contains(
                    self.rendered_layout.player_progress_viewport,
                    mouse.column,
                    mouse.row,
                ) {
                    self.pending_mouse_press = None;
                    self.seek_from_progress_click(mouse.column)?;
                } else if contains(self.rendered_layout.home_viewport, mouse.column, mouse.row) {
                    self.pending_mouse_press = None;
                    self.focus = TuiFocus::Home;
                } else if contains(self.rendered_layout.queue_viewport, mouse.column, mouse.row) {
                    self.pending_mouse_press = None;
                    self.focus = TuiFocus::Queue;
                } else if contains(
                    self.rendered_layout.recent_viewport,
                    mouse.column,
                    mouse.row,
                ) {
                    self.pending_mouse_press = None;
                    self.focus = TuiFocus::Recent;
                } else {
                    self.pending_mouse_press = None;
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.clear_drag_state();
                if let Some(target) = self
                    .click_targets
                    .iter()
                    .find(|target| contains(target.rect, mouse.column, mouse.row))
                    .cloned()
                {
                    self.set_mouse_selection(&target.kind);
                    self.open_context_menu(&target.kind, mouse.column, mouse.row);
                } else {
                    self.close_context_menu();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.close_context_menu();
                if self.drag_state.is_none() {
                    if let Some(press) = self.pending_mouse_press.as_ref() {
                        let moved =
                            mouse.column != press.origin_column || mouse.row != press.origin_row;
                        if moved {
                            if let Some(payload) = press.payload.clone() {
                                self.drag_state = Some(TuiDragState {
                                    payload,
                                    preview_slot: None,
                                });
                            }
                        }
                    }
                }
                if self.drag_state.is_some() {
                    self.update_drag_preview(mouse.column, mouse.row);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(drag) = self.drag_state.take() {
                    self.pending_mouse_press = None;
                    if let Some(raw_slot) = drag.preview_slot {
                        if let Err(err) = self.handle_drag_drop(drag.payload, raw_slot) {
                            self.status_line = format!("Drag failed: {err}");
                        }
                    }
                } else if let Some(press) = self.pending_mouse_press.take() {
                    if contains(press.target_rect, mouse.column, mouse.row) {
                        self.activate_click_target(&press.target_kind)?;
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn drain_background_events(&mut self) {
        while let Ok(event) = self.background_rx.try_recv() {
            match event {
                TuiBackgroundEvent::HomeLoaded(Ok(page)) => {
                    self.home_loading_more = false;
                    self.home_status = HomeFeedStatus::Ready(page);
                    self.home_selected = 0;
                    self.home_scroll_offset = 0;
                    self.status_line = "Home feed loaded.".to_string();
                }
                TuiBackgroundEvent::HomeLoaded(Err(err)) => {
                    self.home_loading_more = false;
                    self.home_status = HomeFeedStatus::Error(err.clone());
                    self.home_selected = 0;
                    self.home_scroll_offset = 0;
                    self.status_line = err;
                }
                TuiBackgroundEvent::HomeAppended {
                    continuation,
                    result,
                } => {
                    self.home_loading_more = false;
                    match result {
                        Ok(mut page) => {
                            let next_continuation = page
                                .continuation
                                .clone()
                                .filter(|token| token != &continuation);
                            match &mut self.home_status {
                                HomeFeedStatus::Ready(existing)
                                    if existing.continuation.as_deref()
                                        == Some(continuation.as_str()) =>
                                {
                                    existing.sections.append(&mut page.sections);
                                    existing.continuation = next_continuation;
                                    self.status_line = format!(
                                        "Home feed expanded to {} sections.",
                                        existing.sections.len()
                                    );
                                }
                                _ => {}
                            }
                        }
                        Err(err) => {
                            self.status_line = format!("Home pagination failed: {err}");
                        }
                    }
                }
                TuiBackgroundEvent::BrowseLoaded { browse_id, result } => {
                    let Some(view) = (match &mut self.content_view {
                        TuiContentView::Browse(view) => Some(view),
                        TuiContentView::Home => None,
                    }) else {
                        continue;
                    };

                    if view.card.browse_id != browse_id
                        || !matches!(&view.status, BrowseViewStatus::Loading)
                    {
                        continue;
                    }

                    match result {
                        Ok(BrowseLoadResult::Tracks(tracks)) => {
                            view.status = BrowseViewStatus::Tracks {
                                title: view.card.title.clone(),
                                thumbnail_url: view.card.thumbnail_url.clone(),
                                tracks,
                            };
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = format!("Opened {}", shorten(&view.card.title, 32));
                        }
                        Ok(BrowseLoadResult::Sections(page)) => {
                            let title = page
                                .title
                                .clone()
                                .unwrap_or_else(|| view.card.title.clone());
                            let thumbnail_url = page
                                .thumbnail_url
                                .clone()
                                .or_else(|| view.card.thumbnail_url.clone());
                            view.status = BrowseViewStatus::Sections {
                                title,
                                thumbnail_url,
                                artist_like: page.artist_like,
                                sections: page.sections,
                            };
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = format!("Opened {}", shorten(&view.card.title, 32));
                        }
                        Err(message) => {
                            view.status = BrowseViewStatus::Error(message.clone());
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = format!(
                                "Could not open {}: {message}",
                                shorten(&view.card.title, 28)
                            );
                        }
                    }
                }
                TuiBackgroundEvent::HistoryLoaded(Ok(tracks)) => {
                    self.recent_tracks = tracks.into_iter().take(RECENT_LIMIT).collect();
                    self.recent_selected = self
                        .recent_selected
                        .min(self.recent_tracks.len().saturating_sub(1));
                }
                TuiBackgroundEvent::HistoryLoaded(Err(err)) => {
                    if self.recent_tracks.is_empty() {
                        self.status_line = format!("History unavailable: {err}");
                    }
                }
                TuiBackgroundEvent::LyricsLoaded { video_id, result } => {
                    let current_matches = self
                        .snapshot
                        .current_track
                        .as_ref()
                        .is_some_and(|track| track.video_id == video_id);
                    if !current_matches {
                        continue;
                    }

                    self.lyrics_state = match result {
                        Ok(Some(lyrics)) => LyricsState::Ready { video_id, lyrics },
                        Ok(None) => LyricsState::Missing {
                            video_id,
                            reason: "YouTube did not expose timed lyrics for this track."
                                .to_string(),
                        },
                        Err(message) => LyricsState::Error { video_id, message },
                    };
                }
                TuiBackgroundEvent::ThumbnailLoaded { url, result } => {
                    let asset = match result {
                        Ok(bytes) => match image::load_from_memory(&bytes) {
                            Ok(image) => ThumbnailAsset::Ready(center_crop_square(image)),
                            Err(err) => ThumbnailAsset::Failed(err.to_string()),
                        },
                        Err(message) => ThumbnailAsset::Failed(message),
                    };

                    self.thumbnail_assets.insert(url, asset);
                }
            }
        }
    }

    fn refresh_snapshot(&mut self) {
        match self.client.state() {
            Ok(snapshot) => {
                self.apply_snapshot(snapshot);
            }
            Err(err) => {
                if let Err(recovery_err) = self.recover_daemon_from_tui_state() {
                    self.status_line =
                        format!("Daemon refresh failed: {err}. Recovery failed: {recovery_err}");
                }
            }
        }
        self.last_daemon_poll_at = Instant::now();
    }

    fn request_home_reload(&mut self) {
        self.home_status = HomeFeedStatus::Loading;
        self.home_loading_more = false;
        spawn_home_bootstrap(self.background_tx.clone());
    }

    fn push_content_snapshot(&mut self) {
        let snapshot = match &self.content_view {
            TuiContentView::Home => ContentSnapshot::Home {
                selected: self.home_selected,
                scroll_offset: self.home_scroll_offset,
            },
            TuiContentView::Browse(view) => ContentSnapshot::Browse {
                view: view.clone(),
                selected: self.home_selected,
                scroll_offset: self.home_scroll_offset,
            },
        };
        self.content_backstack.push(snapshot);
    }

    fn restore_content_snapshot(&mut self, snapshot: ContentSnapshot) {
        match snapshot {
            ContentSnapshot::Home {
                selected,
                scroll_offset,
            } => {
                self.content_view = TuiContentView::Home;
                self.home_selected = selected;
                self.home_scroll_offset = scroll_offset;
            }
            ContentSnapshot::Browse {
                view,
                selected,
                scroll_offset,
            } => {
                self.content_view = TuiContentView::Browse(view);
                self.home_selected = selected;
                self.home_scroll_offset = scroll_offset;
            }
        }
    }

    fn open_browse_detail(&mut self, card: HomeBrowseCard) {
        self.push_content_snapshot();
        self.content_view = TuiContentView::Browse(TuiBrowseView {
            card: card.clone(),
            status: BrowseViewStatus::Loading,
        });
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.home_loading_more = false;
        self.status_line = format!("Loading {}...", shorten(&card.title, 32));
        spawn_browse_load(self.background_tx.clone(), card);
    }

    fn go_back_content(&mut self) {
        if let Some(snapshot) = self.content_backstack.pop() {
            self.restore_content_snapshot(snapshot);
            self.status_line = "Returned to the previous page.".to_string();
        } else {
            self.content_view = TuiContentView::Home;
            self.home_selected = 0;
            self.home_scroll_offset = 0;
            self.status_line = "Back on Home.".to_string();
        }
    }

    fn return_home_content(&mut self) {
        while let Some(snapshot) = self.content_backstack.pop() {
            if matches!(snapshot, ContentSnapshot::Home { .. }) {
                self.restore_content_snapshot(snapshot);
                self.status_line = "Back on Home.".to_string();
                return;
            }
        }

        self.content_view = TuiContentView::Home;
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.status_line = "Back on Home.".to_string();
    }

    fn apply_snapshot(&mut self, snapshot: PlaybackSnapshot) {
        let previous_video_id = self
            .snapshot
            .current_track
            .as_ref()
            .map(|track| track.video_id.clone());
        let next_video_id = snapshot
            .current_track
            .as_ref()
            .map(|track| track.video_id.clone());

        self.snapshot = snapshot;
        let now = Instant::now();
        self.last_snapshot_at = now;
        self.last_daemon_poll_at = now;
        self.queue_selected = self
            .queue_selected
            .min(self.snapshot.queue.len().saturating_sub(1));

        if previous_video_id != next_video_id {
            self.maybe_request_lyrics();
        }
    }

    fn playback_snapshot_from_tui_state(&self) -> PlaybackSnapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.queue_index = snapshot
            .queue_index
            .min(snapshot.queue.len().saturating_sub(1));

        if snapshot.probe_active {
            let fallback_rating = snapshot
                .current_track
                .as_ref()
                .map(|track| track.rating)
                .unwrap_or(SongRating::Indifferent);
            snapshot.current_track = snapshot
                .queue
                .get(snapshot.queue_index)
                .map(|track| active_track_snapshot_from_track(track, fallback_rating));
            snapshot.position_millis = snapshot.probe_restore_position_millis;
            snapshot.duration_millis = None;
            snapshot.buffered_fraction = 0.0;
            snapshot.paused = true;
            snapshot.probe_active = false;
            snapshot.probe_restore_position_millis = None;
        } else {
            snapshot.position_millis = self
                .displayed_position_millis()
                .or(snapshot.position_millis);
        }

        if snapshot.current_track.is_none() {
            snapshot.current_track = snapshot
                .queue
                .get(snapshot.queue_index)
                .map(|track| active_track_snapshot_from_track(track, SongRating::Indifferent));
        }

        snapshot
    }

    fn recover_daemon_from_tui_state(&mut self) -> Result<()> {
        let snapshot = self.playback_snapshot_from_tui_state();
        let has_recoverable_state = snapshot.current_track.is_some() || !snapshot.queue.is_empty();

        let restored = if has_recoverable_state {
            restore_playback_snapshot_via_daemon(
                snapshot.clone(),
                snapshot.current_track.is_some(),
            )?
        } else {
            let client = DaemonClient::new()?;
            client.state()?
        };

        self.client = DaemonClient::new()?;
        self.apply_snapshot(restored);
        self.status_line = if has_recoverable_state {
            "staticd restarted from the TUI state.".to_string()
        } else {
            "staticd restarted.".to_string()
        };
        Ok(())
    }

    fn maybe_request_lyrics(&mut self) {
        let Some(video_id) = self
            .snapshot
            .current_track
            .as_ref()
            .map(|track| track.video_id.clone())
        else {
            self.lyrics_state = LyricsState::Idle;
            return;
        };

        let already_loaded = match &self.lyrics_state {
            LyricsState::Loading {
                video_id: current_video_id,
            }
            | LyricsState::Ready {
                video_id: current_video_id,
                ..
            }
            | LyricsState::Missing {
                video_id: current_video_id,
                ..
            }
            | LyricsState::Error {
                video_id: current_video_id,
                ..
            } => current_video_id == &video_id,
            LyricsState::Idle => false,
        };

        if already_loaded {
            return;
        }

        self.lyrics_state = LyricsState::Loading {
            video_id: video_id.clone(),
        };
        spawn_lyrics_load(self.background_tx.clone(), video_id);
    }

    fn displayed_position_millis(&self) -> Option<u64> {
        let position_millis = self.snapshot.position_millis?;
        if self.snapshot.paused
            || self.snapshot.probe_active
            || self.snapshot.current_track.is_none()
        {
            return Some(position_millis);
        }

        let advanced =
            position_millis.saturating_add(self.last_snapshot_at.elapsed().as_millis() as u64);
        Some(match self.snapshot.duration_millis {
            Some(duration_millis) => advanced.min(duration_millis),
            None => advanced,
        })
    }

    fn toggle_detail_tab(&mut self) {
        self.detail_tab = match self.detail_tab {
            DetailTab::Queue => DetailTab::Lyrics,
            DetailTab::Lyrics => DetailTab::Queue,
        };
    }

    fn run_action_request(&mut self, action: TuiAction) -> Result<PlaybackSnapshot> {
        match action {
            TuiAction::ResumeSaved => {
                let persisted = load_persisted_playback_state()?
                    .filter(|persisted| !persisted.snapshot.queue.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("No saved playback state is available"))?;
                restore_playback_snapshot_via_daemon(persisted.snapshot, true)
            }
            TuiAction::TogglePause => self.client.toggle_pause(),
            TuiAction::Previous => self.client.previous(),
            TuiAction::Next => self.client.next(),
            TuiAction::Like => self.client.rate_current_track(SongRating::Like),
            TuiAction::Dislike => self.client.rate_current_track(SongRating::Dislike),
            TuiAction::Neutral => self.client.rate_current_track(SongRating::Indifferent),
            TuiAction::CycleRepeat => {
                let next_mode = self
                    .snapshot
                    .repeat_mode
                    .next(self.snapshot.radio_virtual_playlist.is_some());
                self.client.set_repeat_mode(next_mode)
            }
            TuiAction::VolumeDown => {
                let next = (self.snapshot.volume - VOLUME_STEP).clamp(0.0, 1.0);
                self.client.set_volume(next)
            }
            TuiAction::VolumeUp => {
                let next = (self.snapshot.volume + VOLUME_STEP).clamp(0.0, 1.0);
                self.client.set_volume(next)
            }
        }
    }

    fn run_action(&mut self, action: TuiAction) -> Result<()> {
        let effective_action = match action {
            TuiAction::Like
                if self
                    .snapshot
                    .current_track
                    .as_ref()
                    .is_some_and(|track| track.rating == SongRating::Like) =>
            {
                TuiAction::Neutral
            }
            TuiAction::Dislike
                if self
                    .snapshot
                    .current_track
                    .as_ref()
                    .is_some_and(|track| track.rating == SongRating::Dislike) =>
            {
                TuiAction::Neutral
            }
            _ => action,
        };

        let result = match self.run_action_request(effective_action) {
            Ok(snapshot) => Ok(snapshot),
            Err(err) if self.client.state().is_err() => {
                self.recover_daemon_from_tui_state()
                    .with_context(|| format!("Action failed: {err}"))?;
                self.run_action_request(effective_action)
                    .with_context(|| format!("Action retry failed after daemon recovery: {err}"))
            }
            Err(err) => Err(err),
        };

        match result {
            Ok(snapshot) => {
                self.apply_snapshot(snapshot);
                if matches!(effective_action, TuiAction::ResumeSaved) {
                    self.resume_card = None;
                }
                self.status_line = format!("{} applied.", action_label(effective_action));
            }
            Err(err) => self.status_line = format!("Action failed: {err}"),
        }
        Ok(())
    }

    fn play_content_section(&mut self, section_index: usize, shuffle: bool) -> Result<()> {
        let (section_title, mut tracks, context) = match &self.content_view {
            TuiContentView::Home => {
                let HomeFeedStatus::Ready(page) = &self.home_status else {
                    self.status_line = "Home feed is still loading.".to_string();
                    return Ok(());
                };
                let Some(section) = page.sections.get(section_index) else {
                    self.status_line = "That Home section is no longer available.".to_string();
                    return Ok(());
                };
                (
                    section.title.clone(),
                    home_section_tracks(section),
                    None::<PlaylistContext>,
                )
            }
            TuiContentView::Browse(view) => match &view.status {
                BrowseViewStatus::Loading => {
                    self.status_line = "This page is still loading.".to_string();
                    return Ok(());
                }
                BrowseViewStatus::Error(message) => {
                    self.status_line = format!("This page failed to load: {message}");
                    return Ok(());
                }
                BrowseViewStatus::Tracks { tracks, .. } => (
                    view.card.title.clone(),
                    tracks.clone(),
                    Some(browse_playlist_context(&view.card, tracks.len())),
                ),
                BrowseViewStatus::Sections { sections, .. } => {
                    let Some(section) = sections.get(section_index) else {
                        self.status_line = "That section is no longer available.".to_string();
                        return Ok(());
                    };
                    (section.title.clone(), home_section_tracks(section), None)
                }
            },
        };

        if tracks.is_empty() {
            self.status_line = format!("{} has no playable tracks.", section_title);
            return Ok(());
        }

        if shuffle {
            use rand::seq::SliceRandom;

            let mut rng = rand::rng();
            tracks.shuffle(&mut rng);
        }

        let snapshot = self
            .client
            .play_queue_at(tracks, 0, context, None, RepeatMode::Off)?;
        self.apply_snapshot(snapshot);
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        self.status_line = if shuffle {
            format!("Shuffled {}", shorten(&section_title, 28))
        } else {
            format!("Playing all from {}", shorten(&section_title, 28))
        };
        Ok(())
    }

    fn activate_selection(&mut self) -> Result<()> {
        match self.focus {
            TuiFocus::Home => self.activate_content_selection(),
            TuiFocus::Queue => self.activate_queue_selection(),
            TuiFocus::Recent => self.activate_recent_selection(),
            TuiFocus::Player => {
                let action = PLAYER_ACTIONS
                    .get(self.player_selected)
                    .copied()
                    .unwrap_or(TuiAction::TogglePause);
                self.run_action(action)
            }
        }
    }

    fn activate_click_target(&mut self, kind: &ClickTargetKind) -> Result<()> {
        match kind {
            ClickTargetKind::Action(action) => self.run_action(*action),
            ClickTargetKind::Nav(nav) => self.handle_nav(*nav),
            ClickTargetKind::ContentBack => {
                self.go_back_content();
                Ok(())
            }
            ClickTargetKind::DetailTab(tab) => {
                self.detail_tab = *tab;
                Ok(())
            }
            ClickTargetKind::HomeSectionAction {
                section_index,
                shuffle,
            } => self.play_content_section(*section_index, *shuffle),
            ClickTargetKind::HomeItem(index) => {
                self.focus = TuiFocus::Home;
                self.home_selected = *index;
                self.ensure_home_visible();
                self.activate_content_selection()
            }
            ClickTargetKind::QueueItem(index) => {
                self.focus = TuiFocus::Queue;
                self.queue_selected = *index;
                self.ensure_queue_visible();
                self.activate_queue_selection()
            }
            ClickTargetKind::RecentItem(index) => {
                self.focus = TuiFocus::Recent;
                self.recent_selected = *index;
                self.ensure_recent_visible();
                self.activate_recent_selection()
            }
        }
    }

    fn handle_nav(&mut self, nav: TuiNavItem) -> Result<()> {
        match nav {
            TuiNavItem::Home => {
                self.focus = TuiFocus::Home;
                if matches!(&self.content_view, TuiContentView::Home) {
                    self.request_home_reload();
                    self.status_line = "Reloading Home.".to_string();
                } else {
                    self.return_home_content();
                }
            }
            TuiNavItem::Settings => {
                self.status_line = "Settings live in the GTK app for now.".to_string();
            }
            TuiNavItem::Likes | TuiNavItem::Playlists | TuiNavItem::Offline => {
                self.status_line =
                    format!("{} rendering is next on the TUI roadmap.", nav_label(nav));
            }
        }
        Ok(())
    }

    fn activate_content_selection(&mut self) -> Result<()> {
        let Some(card) = self.home_doc.cards.get(self.home_selected).cloned() else {
            self.status_line = match &self.content_view {
                TuiContentView::Home => "Home feed is still loading.".to_string(),
                TuiContentView::Browse(view) => match &view.status {
                    BrowseViewStatus::Loading => "This page is still loading.".to_string(),
                    BrowseViewStatus::Error(message) => {
                        format!("This page failed to load: {message}")
                    }
                    BrowseViewStatus::Tracks { .. } | BrowseViewStatus::Sections { .. } => {
                        "Nothing is selectable on this page yet.".to_string()
                    }
                },
            };
            return Ok(());
        };

        match card.item {
            TuiHomeItem::Browse(card) => {
                self.open_browse_detail(card);
                return Ok(());
            }
            TuiHomeItem::Track(track)
                if matches!(&self.content_view, TuiContentView::Home)
                    || matches!(
                        &self.content_view,
                        TuiContentView::Browse(TuiBrowseView {
                            status: BrowseViewStatus::Sections { .. },
                            ..
                        })
                    ) =>
            {
                let snapshot = play_track_with_radio(&self.client, track.clone())?;
                self.apply_snapshot(snapshot);
                self.detail_tab = DetailTab::Queue;
                self.status_line = format!("Playing {}", format_track_summary(&track));
            }
            TuiHomeItem::Track(track) => {
                let TuiContentView::Browse(view) = &self.content_view else {
                    return Ok(());
                };
                let BrowseViewStatus::Tracks { tracks, .. } = &view.status else {
                    return Ok(());
                };
                let start_index = tracks
                    .iter()
                    .position(|candidate| candidate.video_id == track.video_id)
                    .unwrap_or(0);
                let snapshot = self.client.play_queue_at(
                    tracks.clone(),
                    start_index,
                    Some(browse_playlist_context(&view.card, tracks.len())),
                    None,
                    RepeatMode::Off,
                )?;
                self.apply_snapshot(snapshot);
                self.detail_tab = DetailTab::Queue;
                self.status_line = format!("Playing {}", format_track_summary(&track));
            }
        }

        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        Ok(())
    }

    fn activate_queue_selection(&mut self) -> Result<()> {
        if self.snapshot.queue.is_empty() {
            self.status_line = "Queue is empty.".to_string();
            return Ok(());
        }

        let index = self
            .queue_selected
            .min(self.snapshot.queue.len().saturating_sub(1));
        let snapshot = self.client.play_queue_at(
            self.snapshot.queue.clone(),
            index,
            self.snapshot.playlist_context.clone(),
            self.snapshot.radio_virtual_playlist.clone(),
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.status_line = format!(
            "Playing {}",
            self.snapshot
                .queue
                .get(index)
                .map(format_track_summary)
                .unwrap_or_else(|| "selection".to_string())
        );
        Ok(())
    }

    fn activate_recent_selection(&mut self) -> Result<()> {
        let Some(track) = self.recent_tracks.get(self.recent_selected).cloned() else {
            self.status_line = "No recent history yet.".to_string();
            return Ok(());
        };

        let snapshot = play_track_with_radio(&self.client, track.clone())?;
        self.apply_snapshot(snapshot);
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        self.status_line = format!("Playing {}", format_track_summary(&track));
        Ok(())
    }

    fn player_action_focused(&self, index: usize) -> bool {
        matches!(self.focus, TuiFocus::Player) && self.player_selected == index
    }

    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            TuiFocus::Recent => TuiFocus::Home,
            TuiFocus::Home => TuiFocus::Queue,
            TuiFocus::Queue => TuiFocus::Player,
            TuiFocus::Player => TuiFocus::Recent,
        };
    }

    fn move_selection(&mut self, direction: NavigationDirection) {
        match self.focus {
            TuiFocus::Home => {
                if self.home_doc.cards.is_empty() {
                    return;
                }
                self.move_home_selection(direction);
                self.ensure_home_visible();
            }
            TuiFocus::Queue => {
                if self.snapshot.queue.is_empty() {
                    return;
                }
                let delta = match direction {
                    NavigationDirection::Left | NavigationDirection::Up => -1,
                    NavigationDirection::Right | NavigationDirection::Down => 1,
                };
                self.queue_selected =
                    move_index(self.queue_selected, self.snapshot.queue.len(), delta);
                self.ensure_queue_visible();
            }
            TuiFocus::Recent => {
                if self.recent_tracks.is_empty() {
                    return;
                }
                let delta = match direction {
                    NavigationDirection::Left | NavigationDirection::Up => -1,
                    NavigationDirection::Right | NavigationDirection::Down => 1,
                };
                self.recent_selected =
                    move_index(self.recent_selected, self.recent_tracks.len(), delta);
                self.ensure_recent_visible();
            }
            TuiFocus::Player => {
                let delta = match direction {
                    NavigationDirection::Left | NavigationDirection::Up => -1,
                    NavigationDirection::Right | NavigationDirection::Down => 1,
                };
                self.player_selected =
                    move_index(self.player_selected, PLAYER_ACTIONS.len(), delta);
            }
        }
    }

    fn move_home_selection(&mut self, direction: NavigationDirection) {
        let Some(current) = self.home_doc.cards.get(self.home_selected) else {
            self.home_selected = 0;
            return;
        };

        let current_y = current.rect.y as i32;
        let current_center_x = current.rect.x as i32 + current.rect.width as i32 / 2;

        let mut best_index = self.home_selected;
        let mut best_score = (i32::MAX, i32::MAX);

        for (index, candidate) in self.home_doc.cards.iter().enumerate() {
            if index == self.home_selected {
                continue;
            }

            let candidate_y = candidate.rect.y as i32;
            let candidate_center_x =
                candidate.rect.x as i32 + candidate.rect.width as i32 / 2;
            let delta_x = candidate_center_x - current_center_x;
            let delta_y = candidate_y - current_y;

            let (primary, secondary) = match direction {
                NavigationDirection::Left => {
                    if delta_y != 0 || delta_x >= 0 {
                        continue;
                    }
                    (delta_x.abs(), 0)
                }
                NavigationDirection::Right => {
                    if delta_y != 0 || delta_x <= 0 {
                        continue;
                    }
                    (delta_x.abs(), 0)
                }
                NavigationDirection::Up => {
                    if delta_y >= 0 {
                        continue;
                    }
                    (delta_y.abs(), delta_x.abs())
                }
                NavigationDirection::Down => {
                    if delta_y <= 0 {
                        continue;
                    }
                    (delta_y.abs(), delta_x.abs())
                }
            };

            if (primary, secondary) < best_score {
                best_score = (primary, secondary);
                best_index = index;
            }
        }

        self.home_selected = best_index;
    }

    fn page_move(&mut self, direction: isize) {
        match self.focus {
            TuiFocus::Home => {
                let delta = direction * self.home_viewport_rows.max(1) as isize;
                self.scroll_home(delta);
            }
            TuiFocus::Queue => {
                let delta = direction * self.queue_viewport_rows.max(1) as isize;
                self.queue_selected =
                    move_index(self.queue_selected, self.snapshot.queue.len(), delta);
                self.ensure_queue_visible();
            }
            TuiFocus::Recent => {
                let delta = direction * self.recent_viewport_rows.max(1) as isize;
                self.recent_selected =
                    move_index(self.recent_selected, self.recent_tracks.len(), delta);
                self.ensure_recent_visible();
            }
            TuiFocus::Player => {
                self.player_selected =
                    move_index(self.player_selected, PLAYER_ACTIONS.len(), direction);
            }
        }
    }

    fn jump_to_start(&mut self) {
        match self.focus {
            TuiFocus::Home => {
                self.home_selected = 0;
                self.home_scroll_offset = 0;
            }
            TuiFocus::Queue => {
                self.queue_selected = 0;
                self.queue_scroll_offset = 0;
            }
            TuiFocus::Recent => {
                self.recent_selected = 0;
                self.recent_scroll_offset = 0;
            }
            TuiFocus::Player => {
                self.player_selected = 0;
            }
        }
    }

    fn jump_to_end(&mut self) {
        match self.focus {
            TuiFocus::Home => {
                self.home_selected = self.home_doc.cards.len().saturating_sub(1);
                self.ensure_home_visible();
            }
            TuiFocus::Queue => {
                self.queue_selected = self.snapshot.queue.len().saturating_sub(1);
                self.ensure_queue_visible();
            }
            TuiFocus::Recent => {
                self.recent_selected = self.recent_tracks.len().saturating_sub(1);
                self.ensure_recent_visible();
            }
            TuiFocus::Player => {
                self.player_selected = PLAYER_ACTIONS.len().saturating_sub(1);
            }
        }
    }

    fn scroll_panel(&mut self, column: u16, row: u16, delta: isize) {
        if contains(self.rendered_layout.queue_viewport, column, row) {
            self.focus = TuiFocus::Queue;
            if self.detail_tab == DetailTab::Queue {
                self.queue_selected =
                    move_index(self.queue_selected, self.snapshot.queue.len(), delta);
                self.ensure_queue_visible();
            }
        } else if contains(self.rendered_layout.recent_viewport, column, row) {
            self.focus = TuiFocus::Recent;
            self.recent_selected =
                move_index(self.recent_selected, self.recent_tracks.len(), delta);
            self.ensure_recent_visible();
        } else {
            self.focus = TuiFocus::Home;
            self.scroll_home(delta);
        }
    }

    fn seek_from_progress_click(&mut self, column: u16) -> Result<()> {
        let Some(duration) = self
            .snapshot
            .duration_millis
            .filter(|duration| *duration > 0)
        else {
            return Ok(());
        };
        let rect = self.rendered_layout.player_progress_viewport;
        if rect.width == 0 {
            return Ok(());
        }

        let relative = column
            .saturating_sub(rect.x)
            .min(rect.width.saturating_sub(1));
        let ratio = relative as f64 / rect.width.saturating_sub(1).max(1) as f64;
        let target = (duration as f64 * ratio) as u64;
        match self.client.seek_to_millis(target) {
            Ok(snapshot) => {
                self.apply_snapshot(snapshot);
                self.status_line = format!("Seeked to {}", format_millis(target));
            }
            Err(err) => {
                self.status_line = format!("Seek failed: {err}");
            }
        }
        Ok(())
    }

    fn scroll_home(&mut self, delta: isize) {
        let max_scroll = self
            .home_doc
            .total_height
            .saturating_sub(self.home_viewport_rows.max(1));
        if delta < 0 {
            self.home_scroll_offset = self.home_scroll_offset.saturating_sub(delta.unsigned_abs());
        } else {
            self.home_scroll_offset = self
                .home_scroll_offset
                .saturating_add(delta as usize)
                .min(max_scroll);
        }
    }

    fn ensure_queue_visible(&mut self) {
        if self.snapshot.queue.is_empty() {
            self.queue_selected = 0;
            self.queue_scroll_offset = 0;
            return;
        }

        let viewport = self.queue_viewport_rows.max(1);
        if self.queue_selected < self.queue_scroll_offset {
            self.queue_scroll_offset = self.queue_selected;
        } else if self.queue_selected >= self.queue_scroll_offset.saturating_add(viewport) {
            self.queue_scroll_offset = self
                .queue_selected
                .saturating_sub(viewport.saturating_sub(1));
        }
        self.queue_scroll_offset = self
            .queue_scroll_offset
            .min(self.snapshot.queue.len().saturating_sub(viewport));
    }

    fn ensure_recent_visible(&mut self) {
        if self.recent_tracks.is_empty() {
            self.recent_selected = 0;
            self.recent_scroll_offset = 0;
            return;
        }

        let viewport = self.recent_viewport_rows.max(1);
        if self.recent_selected < self.recent_scroll_offset {
            self.recent_scroll_offset = self.recent_selected;
        } else if self.recent_selected >= self.recent_scroll_offset.saturating_add(viewport) {
            self.recent_scroll_offset = self
                .recent_selected
                .saturating_sub(viewport.saturating_sub(1));
        }
        self.recent_scroll_offset = self
            .recent_scroll_offset
            .min(self.recent_tracks.len().saturating_sub(viewport));
    }

    fn ensure_home_visible(&mut self) {
        let Some(card) = self.home_doc.cards.get(self.home_selected) else {
            self.home_selected = 0;
            self.home_scroll_offset = 0;
            return;
        };

        let card_top = card.rect.y as usize;
        let card_bottom = card_top.saturating_add(card.rect.height as usize);
        let viewport = self.home_viewport_rows.max(1);
        if card_top < self.home_scroll_offset {
            self.home_scroll_offset = card_top;
        } else if card_bottom > self.home_scroll_offset.saturating_add(viewport) {
            self.home_scroll_offset = card_bottom.saturating_sub(viewport);
        }
        self.clamp_home_scroll_offset();
    }

    fn clamp_home_scroll_offset(&mut self) {
        let viewport = self.home_viewport_rows.max(1);
        self.home_scroll_offset = self
            .home_scroll_offset
            .min(self.home_doc.total_height.saturating_sub(viewport));
    }

    fn visible_resume_card(&self) -> Option<&ResumeCardData> {
        self.resume_card
            .as_ref()
            .filter(|_| self.snapshot.current_track.is_none())
    }
}

#[derive(Clone)]
struct HomeFeedWidget {
    doc: HomeDocument,
    scroll_offset: usize,
    selected_index: usize,
    focus: bool,
    theme: TuiTheme,
    show_thumbnail_wells: bool,
    current_video_id: Option<String>,
}

impl Widget for HomeFeedWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        fill_area(buf, area, Style::default().bg(self.theme.content_bg));

        for message in &self.doc.messages {
            let y = area.y as i32 + message.y as i32 - self.scroll_offset as i32;
            if y < area.y as i32 || y >= area.bottom() as i32 {
                continue;
            }
            write_clipped(
                buf,
                area,
                area.x,
                y as u16,
                &message.text,
                if message.accent {
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(self.theme.muted)
                },
            );
        }

        for banner in &self.doc.banners {
            draw_home_banner(buf, area, self.scroll_offset, banner, &self.theme);
        }

        for header in &self.doc.headers {
            let y = area.y as i32 + header.y as i32 - self.scroll_offset as i32;
            if y < area.y as i32 || y >= area.bottom() as i32 {
                continue;
            }
            let actions_layout = home_header_actions_layout(area, header);
            let header_text = format!("{}  {}", header.title, header.meta);
            let text_right = actions_layout
                .as_ref()
                .map(|layout| layout.play_x.saturating_sub(2))
                .unwrap_or(area.right());
            let text_width = text_right.saturating_sub(area.x) as usize;
            write_clipped(
                buf,
                area,
                area.x,
                y as u16,
                &shorten(&header_text, text_width.max(1)),
                Style::default()
                    .fg(self.theme.text)
                    .add_modifier(Modifier::BOLD),
            );
            let header_text_width = text_cell_width(&header_text) as u16;
            let divider_end = actions_layout
                .as_ref()
                .map(|layout| layout.play_x.saturating_sub(2))
                .unwrap_or(area.right());
            if divider_end > area.x.saturating_add(header_text_width).saturating_add(4) {
                let divider_x = area.x.saturating_add(header_text_width).saturating_add(4);
                let divider_width = divider_end.saturating_sub(divider_x) as usize;
                write_clipped(
                    buf,
                    area,
                    divider_x,
                    y as u16,
                    &"─".repeat(divider_width),
                    Style::default().fg(self.theme.border_subtle),
                );
            }
            if let Some(layout) = actions_layout {
                write_clipped(
                    buf,
                    area,
                    layout.play_x,
                    y as u16,
                    HOME_HEADER_PLAY_ALL_LABEL,
                    Style::default()
                        .fg(self.theme.accent_glow)
                        .bg(self.theme.elevated_alt_bg)
                        .add_modifier(Modifier::BOLD),
                );
                write_clipped(
                    buf,
                    area,
                    layout.shuffle_x,
                    y as u16,
                    HOME_HEADER_SHUFFLE_ALL_LABEL,
                    Style::default()
                        .fg(self.theme.accent)
                        .bg(self.theme.elevated_bg)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }

        for (index, card) in self.doc.cards.iter().enumerate() {
            draw_home_card(
                buf,
                area,
                self.scroll_offset,
                card,
                index == self.selected_index,
                self.focus,
                self.show_thumbnail_wells,
                self.current_video_id
                    .as_deref()
                    .is_some_and(|video_id| matches!(&card.item, TuiHomeItem::Track(track) if track.video_id == video_id)),
                &self.theme,
            );
        }
    }
}

fn spawn_home_bootstrap(tx: mpsc::Sender<TuiBackgroundEvent>) {
    std::thread::spawn(move || match saved_network_state() {
        Ok(network) => {
            let home = network.get_home_page(None).map_err(|err| err.to_string());
            let history = network.get_history().map_err(|err| err.to_string());
            let _ = tx.send(TuiBackgroundEvent::HomeLoaded(home));
            let _ = tx.send(TuiBackgroundEvent::HistoryLoaded(history));
        }
        Err(_) => {
            let _ = tx.send(TuiBackgroundEvent::HomeLoaded(Err(
                "Connect YouTube Music in the GTK app to load the full Home feed.".to_string(),
            )));
            let _ = tx.send(TuiBackgroundEvent::HistoryLoaded(Ok(Vec::new())));
        }
    });
}

fn spawn_home_append(tx: mpsc::Sender<TuiBackgroundEvent>, continuation: String) {
    std::thread::spawn(move || {
        let result = saved_network_state()
            .and_then(|network| network.get_home_page(Some(&continuation)))
            .map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::HomeAppended {
            continuation,
            result,
        });
    });
}

fn spawn_browse_load(tx: mpsc::Sender<TuiBackgroundEvent>, card: HomeBrowseCard) {
    std::thread::spawn(move || {
        let browse_id = card.browse_id.clone();
        let result = saved_network_state()
            .and_then(|network| match card.page_type {
                HomeBrowsePageType::Artist | HomeBrowsePageType::UserChannel => network
                    .get_channel_page(&browse_id)
                    .map(BrowseLoadResult::Sections),
                HomeBrowsePageType::Playlist
                | HomeBrowsePageType::Album
                | HomeBrowsePageType::PodcastShow => network
                    .get_browse_tracks(&browse_id)
                    .map(BrowseLoadResult::Tracks),
            })
            .map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::BrowseLoaded { browse_id, result });
    });
}

fn spawn_lyrics_load(tx: mpsc::Sender<TuiBackgroundEvent>, video_id: String) {
    std::thread::spawn(move || {
        let result = saved_network_state()
            .and_then(|network| network.get_synced_lyrics(&video_id))
            .map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::LyricsLoaded { video_id, result });
    });
}

fn spawn_thumbnail_load(tx: mpsc::Sender<TuiBackgroundEvent>, url: String) {
    std::thread::spawn(move || {
        let result = fetch_thumbnail_bytes(&url)
            .ok_or_else(|| format!("Could not fetch thumbnail bytes from {url}"));
        let _ = tx.send(TuiBackgroundEvent::ThumbnailLoaded { url, result });
    });
}

fn load_resume_card_data() -> Option<ResumeCardData> {
    let persisted = load_persisted_playback_state().ok().flatten()?;
    if persisted.snapshot.queue.is_empty() {
        return None;
    }

    let track = resume_snapshot_track(&persisted.snapshot)?;
    Some(ResumeCardData {
        track,
        meta: resume_snapshot_meta(&persisted.snapshot),
    })
}

fn resume_snapshot_track(snapshot: &PlaybackSnapshot) -> Option<Track> {
    snapshot
        .queue
        .get(
            snapshot
                .queue_index
                .min(snapshot.queue.len().saturating_sub(1)),
        )
        .cloned()
        .or_else(|| {
            snapshot.current_track.as_ref().map(|track| Track {
                video_id: track.video_id.clone(),
                title: track.title.clone(),
                artists: vec![crate::models::Artist {
                    name: track.artist.clone(),
                    id: None,
                }],
                album: None,
                duration: None,
                thumbnail_url: track.thumb_url.clone(),
                playback_playlist_id: None,
                playback_params: None,
                not_interested_feedback_token: None,
                undo_feedback_token: None,
            })
        })
}

fn resume_snapshot_meta(snapshot: &PlaybackSnapshot) -> String {
    let queue_label = if snapshot.radio_virtual_playlist.is_some() {
        "Radio queue"
    } else if snapshot.playlist_context.is_some() {
        "Playlist"
    } else if snapshot.queue.len() > 1 {
        "Queue"
    } else {
        "Track"
    };
    match snapshot.position_millis {
        Some(position_millis) if position_millis > 0 => {
            let action = if snapshot.paused {
                "Paused at"
            } else {
                "Resume at"
            };
            format!(
                "{queue_label} • {action} {}",
                format_millis(position_millis)
            )
        }
        _ => queue_label.to_string(),
    }
}

fn build_home_document(
    status: &HomeFeedStatus,
    width: u16,
    resume_card: Option<&ResumeCardData>,
) -> HomeDocument {
    let usable_width = width.max(24);
    let mut doc = HomeDocument::default();
    let mut y = 0usize;

    if let Some(resume_card) = resume_card {
        doc.banners.push(HomeBanner {
            rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
            eyebrow: "RESUME".to_string(),
            title: format!("Resume {}", resume_card.track.title),
            subtitle: artist_string(&resume_card.track),
            meta: resume_card.meta.clone(),
            hint: Some("Press r or click here to restore".to_string()),
            thumbnail_url: resume_card.track.thumbnail_url.clone(),
            accent: true,
        });
        y += HOME_BANNER_HEIGHT as usize + 1;
    }

    match status {
        HomeFeedStatus::Loading => {
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: "Loading recommendations from YouTube Music...".to_string(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        HomeFeedStatus::Error(message) => {
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: message.clone(),
                accent: true,
            });
            doc.messages.push(HomeMessage {
                y: y + 3,
                text: "The queue and player controls still work headlessly.".to_string(),
                accent: false,
            });
            doc.total_height = y.saturating_add(6);
            return doc;
        }
        HomeFeedStatus::Ready(page) => {
            if page.sections.is_empty() {
                doc.messages.push(HomeMessage {
                    y: y + 1,
                    text: "No home sections were returned.".to_string(),
                    accent: true,
                });
                doc.total_height = y.saturating_add(4);
                return doc;
            }

            for (section_index, section) in page.sections.iter().enumerate() {
                if section_index > 0 || !doc.banners.is_empty() {
                    y += 1;
                }
                let tracks = home_section_tracks(section);
                let browse_cards = home_section_browse_cards(section);
                let section_style = if section_index == 0 {
                    HomeCardStyle::Featured
                } else if tracks.len() == section.contents.len() && !tracks.is_empty() {
                    HomeCardStyle::TrackRow
                } else if browse_cards.len() == section.contents.len() && !browse_cards.is_empty() {
                    HomeCardStyle::BrowseRow
                } else {
                    HomeCardStyle::Shelf
                };
                let show_play_controls = home_section_should_show_play_controls(section, &tracks);
                doc.headers.push(HomeHeader {
                    y,
                    section_index,
                    title: section.title.to_uppercase(),
                    meta: format!(
                        "· {}",
                        section_meta_label(section_style, section.contents.len())
                    ),
                    show_play_controls,
                });
                y += 2;

                let items = if tracks.len() == section.contents.len() {
                    tracks
                        .into_iter()
                        .map(TuiHomeItem::Track)
                        .collect::<Vec<_>>()
                } else if browse_cards.len() == section.contents.len() {
                    browse_cards
                        .into_iter()
                        .map(TuiHomeItem::Browse)
                        .collect::<Vec<_>>()
                } else {
                    section
                        .contents
                        .iter()
                        .filter_map(|item| match item {
                            HomeItem::Track(track) => Some(TuiHomeItem::Track(track.clone())),
                            HomeItem::BrowseCard(card) => Some(TuiHomeItem::Browse(card.clone())),
                        })
                        .collect::<Vec<_>>()
                };
                let columns = home_columns(usable_width, section_style, items.len());
                let card_height = home_card_height(section_style);
                let card_width =
                    ((usable_width.saturating_sub(HOME_GAP * columns.saturating_sub(1))) / columns)
                        .max(match section_style {
                            HomeCardStyle::Featured => 24,
                            HomeCardStyle::TrackRow => 26,
                            HomeCardStyle::BrowseRow | HomeCardStyle::Shelf => 22,
                        });

                for chunk in items.chunks(columns as usize) {
                    for (column, item) in chunk.iter().enumerate() {
                        let x = column as u16 * (card_width + HOME_GAP);
                        let (title, subtitle, meta, badge) = home_card_copy(item, section_style);
                        doc.cards.push(HomeCard {
                            rect: Rect::new(x, y as u16, card_width, card_height),
                            item: item.clone(),
                            title,
                            subtitle,
                            meta,
                            badge,
                            style: section_style,
                        });
                    }
                    y += card_height as usize + 1;
                }
            }

            doc.total_height = y.saturating_add(1);
        }
    }

    doc
}

fn push_document_section(
    doc: &mut HomeDocument,
    y: &mut usize,
    usable_width: u16,
    section_index: usize,
    title: &str,
    items: &[TuiHomeItem],
    section_style: HomeCardStyle,
    show_play_controls: bool,
) {
    if items.is_empty() {
        return;
    }

    doc.headers.push(HomeHeader {
        y: *y,
        section_index,
        title: title.to_uppercase(),
        meta: format!("· {}", section_meta_label(section_style, items.len())),
        show_play_controls,
    });
    *y += 2;

    let columns = home_columns(usable_width, section_style, items.len());
    let card_height = home_card_height(section_style);
    let card_width = ((usable_width.saturating_sub(HOME_GAP * columns.saturating_sub(1)))
        / columns)
        .max(match section_style {
            HomeCardStyle::Featured => 24,
            HomeCardStyle::TrackRow => 26,
            HomeCardStyle::BrowseRow | HomeCardStyle::Shelf => 22,
        });

    for chunk in items.chunks(columns as usize) {
        for (column, item) in chunk.iter().enumerate() {
            let x = column as u16 * (card_width + HOME_GAP);
            let (title, subtitle, meta, badge) = home_card_copy(item, section_style);
            doc.cards.push(HomeCard {
                rect: Rect::new(x, *y as u16, card_width, card_height),
                item: item.clone(),
                title,
                subtitle,
                meta,
                badge,
                style: section_style,
            });
        }
        *y += card_height as usize + 1;
    }
}

fn build_browse_document(view: &TuiBrowseView, width: u16) -> HomeDocument {
    let usable_width = width.max(24);
    let mut doc = HomeDocument::default();
    let mut y = 0usize;
    let browse_badge = browse_label(view.card.page_type).to_uppercase();

    match &view.status {
        BrowseViewStatus::Loading => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: browse_badge,
                title: view.card.title.clone(),
                subtitle: view
                    .card
                    .subtitle
                    .clone()
                    .unwrap_or_else(|| "Loading page details...".to_string()),
                meta: "Fetching the page layout and playable items".to_string(),
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: view.card.thumbnail_url.clone(),
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: "Loading recommendations, tracks, and related shelves...".to_string(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        BrowseViewStatus::Error(message) => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: browse_badge,
                title: view.card.title.clone(),
                subtitle: view
                    .card
                    .subtitle
                    .clone()
                    .unwrap_or_else(|| "Browse page".to_string()),
                meta: "This page could not be loaded".to_string(),
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: view.card.thumbnail_url.clone(),
                accent: false,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: message.clone(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        BrowseViewStatus::Tracks {
            title,
            thumbnail_url,
            tracks,
        } => {
            let track_count = tracks.len();
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: browse_badge,
                title: title.clone(),
                subtitle: view.card.subtitle.clone().unwrap_or_else(|| {
                    format!("{} page", browse_label(view.card.page_type).to_lowercase())
                }),
                meta: if track_count == 1 {
                    "1 track ready to play".to_string()
                } else {
                    format!("{track_count} tracks ready to play")
                },
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: thumbnail_url.clone(),
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;

            if tracks.is_empty() {
                doc.messages.push(HomeMessage {
                    y: y + 1,
                    text: "No playable tracks were returned for this page.".to_string(),
                    accent: true,
                });
                doc.total_height = y.saturating_add(4);
                return doc;
            }

            let items = tracks
                .iter()
                .cloned()
                .map(TuiHomeItem::Track)
                .collect::<Vec<_>>();
            push_document_section(
                &mut doc,
                &mut y,
                usable_width,
                0,
                "Tracks",
                &items,
                HomeCardStyle::TrackRow,
                true,
            );
        }
        BrowseViewStatus::Sections {
            title,
            thumbnail_url,
            artist_like,
            sections,
        } => {
            let top_songs_index = sections
                .iter()
                .position(|section| section.title == "Top songs");
            let ordered_indices = top_songs_index
                .into_iter()
                .chain((0..sections.len()).filter(|index| Some(*index) != top_songs_index))
                .collect::<Vec<_>>();
            let featured_sections = ordered_indices
                .len()
                .saturating_sub(top_songs_index.is_some() as usize);
            let top_song_count = top_songs_index
                .and_then(|index| sections.get(index))
                .map(home_section_tracks)
                .map(|tracks| tracks.len());
            let use_artist_chrome = view.card.page_type == HomeBrowsePageType::Artist
                || *artist_like
                || is_artist_sections(sections);
            let eyebrow = if use_artist_chrome {
                "ARTIST"
            } else {
                "CHANNEL"
            };
            let summary = if let Some(count) = top_song_count {
                format!("{count} top songs  •  {featured_sections} featured sections")
            } else if sections.len() == 1 {
                "1 featured section".to_string()
            } else {
                format!("{} featured sections", sections.len())
            };
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: eyebrow.to_string(),
                title: title.clone(),
                subtitle: view
                    .card
                    .subtitle
                    .clone()
                    .unwrap_or_else(|| format!("{} page", eyebrow.to_ascii_lowercase())),
                meta: summary,
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: thumbnail_url.clone(),
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;

            if sections.is_empty() {
                doc.messages.push(HomeMessage {
                    y: y + 1,
                    text: "No sections were returned for this page.".to_string(),
                    accent: true,
                });
                doc.total_height = y.saturating_add(4);
                return doc;
            }

            for (ordinal, index) in ordered_indices.into_iter().enumerate() {
                let Some(section) = sections.get(index) else {
                    continue;
                };
                if ordinal > 0 {
                    y += 1;
                }

                let tracks = home_section_tracks(section);
                let browse_cards = home_section_browse_cards(section);
                let (items, section_style) = if tracks.len() == section.contents.len()
                    && !tracks.is_empty()
                {
                    (
                        tracks
                            .into_iter()
                            .map(TuiHomeItem::Track)
                            .collect::<Vec<_>>(),
                        HomeCardStyle::TrackRow,
                    )
                } else if browse_cards.len() == section.contents.len() && !browse_cards.is_empty() {
                    (
                        browse_cards
                            .into_iter()
                            .map(TuiHomeItem::Browse)
                            .collect::<Vec<_>>(),
                        HomeCardStyle::BrowseRow,
                    )
                } else {
                    (
                        section
                            .contents
                            .iter()
                            .filter_map(|item| match item {
                                HomeItem::Track(track) => Some(TuiHomeItem::Track(track.clone())),
                                HomeItem::BrowseCard(card) => {
                                    Some(TuiHomeItem::Browse(card.clone()))
                                }
                            })
                            .collect::<Vec<_>>(),
                        HomeCardStyle::Shelf,
                    )
                };
                let show_play_controls = home_section_tracks(section).len()
                    == section.contents.len()
                    && !home_section_tracks(section).is_empty();
                push_document_section(
                    &mut doc,
                    &mut y,
                    usable_width,
                    index,
                    &section.title,
                    &items,
                    section_style,
                    show_play_controls,
                );
            }
        }
    }

    doc.total_height = y.saturating_add(1);
    doc
}

fn render_action_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    action: TuiAction,
    theme: &TuiTheme,
    primary: bool,
    active: bool,
    focused: bool,
) -> ClickTarget {
    let mut style = Style::default().bg(theme.elevated_bg).fg(theme.text);
    let mut border = theme.border;
    if primary {
        style = style.bg(theme.accent_fill).fg(Color::Rgb(255, 255, 255));
        border = theme.accent;
    } else if active {
        style = style.bg(theme.selected_bg).fg(theme.accent);
        border = theme.accent;
    }
    if focused {
        border = theme.border_focus;
    }
    let border_style = if focused {
        Style::default().fg(border).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(border)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default()
                .fg(style.fg.unwrap_or(theme.text))
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        inner,
    );
    ClickTarget {
        rect: area,
        kind: ClickTargetKind::Action(action),
    }
}

fn render_cover_placeholder(frame: &mut Frame<'_>, area: Rect, initials: String, theme: &TuiTheme) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.accent_fill)),
        area,
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                initials,
                Style::default()
                    .fg(Color::Rgb(255, 255, 255))
                    .add_modifier(Modifier::BOLD),
            )),
        ])
        .alignment(Alignment::Center),
        area,
    );
}

fn render_home_thumbnail_fallback(
    frame: &mut Frame<'_>,
    area: Rect,
    initials: String,
    theme: &TuiTheme,
    accent: bool,
) {
    let background = if accent {
        theme.accent_fill
    } else {
        theme.content_alt_bg
    };
    frame.render_widget(
        Block::default().style(Style::default().bg(background)),
        area,
    );
    frame.render_widget(
        Paragraph::new(initials).alignment(Alignment::Center).style(
            Style::default()
                .fg(theme.accent_glow)
                .bg(background)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

fn draw_home_banner(
    buf: &mut Buffer,
    clip: Rect,
    scroll_offset: usize,
    banner: &HomeBanner,
    theme: &TuiTheme,
) {
    let screen_y = banner.rect.y as i32 - scroll_offset as i32;
    if screen_y + banner.rect.height as i32 <= 0 || screen_y >= clip.height as i32 {
        return;
    }

    let x = clip.x + banner.rect.x;
    let y = clip.y as i32 + screen_y;
    let bg = if banner.accent {
        theme.accent_fill
    } else {
        theme.elevated_bg
    };
    let border = if banner.accent {
        theme.accent
    } else {
        theme.border_subtle
    };
    draw_box(
        buf,
        clip,
        x,
        y,
        banner.rect.width,
        banner.rect.height,
        border,
        bg,
    );

    let inner_x = x + 2;
    let media_width = home_banner_thumbnail_width(banner.rect.width, banner.rect.height);
    let text_width = banner
        .rect
        .width
        .saturating_sub(4)
        .saturating_sub(media_width.saturating_add((media_width > 0) as u16))
        as usize;
    if media_width > 0 {
        let media_x = x + banner.rect.width.saturating_sub(media_width + 2);
        draw_home_thumbnail_well(
            buf,
            clip,
            media_x,
            y + 1,
            media_width,
            banner.rect.height.saturating_sub(2),
            &placeholder_initials(&banner.title, &banner.subtitle),
            theme,
            banner.accent,
        );
    }

    write_clipped(
        buf,
        clip,
        inner_x,
        y.max(clip.y as i32) as u16 + 1,
        &shorten(&banner.eyebrow, text_width),
        Style::default()
            .fg(if banner.accent {
                theme.accent_glow
            } else {
                theme.muted
            })
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    );
    write_clipped(
        buf,
        clip,
        inner_x,
        y.max(clip.y as i32) as u16 + 2,
        &shorten(&banner.title, text_width),
        Style::default()
            .fg(theme.text)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    );
    write_clipped(
        buf,
        clip,
        inner_x,
        y.max(clip.y as i32) as u16 + 3,
        &shorten(&banner.subtitle, text_width),
        Style::default().fg(theme.text).bg(bg),
    );
    write_clipped(
        buf,
        clip,
        inner_x,
        y.max(clip.y as i32) as u16 + 4,
        &shorten(&banner.meta, text_width),
        Style::default().fg(theme.muted).bg(bg),
    );
    if let Some(hint) = banner.hint.as_deref() {
        write_clipped(
            buf,
            clip,
            inner_x,
            y.max(clip.y as i32) as u16 + 5,
            &shorten(hint, text_width),
            Style::default().fg(theme.border_focus).bg(bg),
        );
    }
}

fn draw_home_card(
    buf: &mut Buffer,
    clip: Rect,
    scroll_offset: usize,
    card: &HomeCard,
    selected: bool,
    focus: bool,
    show_thumbnail_wells: bool,
    current: bool,
    theme: &TuiTheme,
) {
    let screen_y = card.rect.y as i32 - scroll_offset as i32;
    if screen_y + card.rect.height as i32 <= 0 || screen_y >= clip.height as i32 {
        return;
    }

    let x = clip.x + card.rect.x;
    let y = clip.y as i32 + screen_y;
    let width = card.rect.width;
    let height = card.rect.height;
    if width < 4 || height < 3 {
        return;
    }

    let base_bg = match card.style {
        HomeCardStyle::Featured => theme.elevated_alt_bg,
        HomeCardStyle::TrackRow => theme.elevated_bg,
        HomeCardStyle::BrowseRow => theme.content_alt_bg,
        HomeCardStyle::Shelf => theme.elevated_bg,
    };
    let bg = if selected { theme.selected_bg } else { base_bg };
    let border = if current {
        theme.positive
    } else if selected && focus {
        theme.border_focus
    } else if matches!(card.style, HomeCardStyle::Featured) {
        theme.accent
    } else {
        theme.border_subtle
    };

    fill_clipped_area(
        buf,
        clip,
        Rect::new(x, y.max(clip.y as i32) as u16, width, height),
        Style::default().bg(bg),
    );
    draw_box(buf, clip, x, y, width, height, border, bg);

    let inner_x = x + 1;
    let media_width = if show_thumbnail_wells {
        home_card_thumbnail_width(width, card.style)
    } else {
        0
    };
    let text_width = width
        .saturating_sub(2)
        .saturating_sub(media_width.saturating_add((media_width > 0) as u16))
        as usize;
    let title_style = Style::default()
        .fg(theme.text)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let subtitle_style = Style::default().fg(theme.muted).bg(bg);
    let meta_style = Style::default()
        .fg(if current {
            theme.positive
        } else {
            theme.accent_glow
        })
        .bg(bg);
    let badge_style = Style::default()
        .fg(if matches!(card.style, HomeCardStyle::Featured) {
            theme.accent_glow
        } else {
            theme.accent
        })
        .bg(bg)
        .add_modifier(Modifier::BOLD);

    if media_width > 0 {
        let media_x = x + width.saturating_sub(1 + media_width);
        draw_home_thumbnail_well(
            buf,
            clip,
            media_x,
            y + 1,
            media_width,
            height.saturating_sub(2),
            &placeholder_initials(&card.title, &card.subtitle),
            theme,
            matches!(card.style, HomeCardStyle::Featured),
        );
    }

    if y + 1 >= clip.y as i32 {
        write_clipped(
            buf,
            clip,
            inner_x,
            (y + 1) as u16,
            &shorten(&card.badge, text_width),
            badge_style,
        );
    }

    match card.style {
        HomeCardStyle::Featured => {
            let wrapped_title = wrap_text(&card.title, text_width, 2);
            for (offset, line) in wrapped_title.iter().enumerate() {
                write_clipped(
                    buf,
                    clip,
                    inner_x,
                    (y + 2 + offset as i32) as u16,
                    line,
                    title_style,
                );
            }
            write_clipped(
                buf,
                clip,
                inner_x,
                (y + 4) as u16,
                &shorten(&card.subtitle, text_width),
                subtitle_style,
            );
            write_clipped(
                buf,
                clip,
                inner_x,
                (y + 5) as u16,
                &shorten(&card.meta, text_width),
                meta_style,
            );
        }
        HomeCardStyle::TrackRow => {
            write_clipped(
                buf,
                clip,
                inner_x,
                (y + 2) as u16,
                &shorten(&card.title, text_width),
                title_style,
            );
            write_clipped(
                buf,
                clip,
                inner_x,
                (y + 3) as u16,
                &shorten(&format!("{}  •  {}", card.subtitle, card.meta), text_width),
                subtitle_style,
            );
        }
        HomeCardStyle::BrowseRow | HomeCardStyle::Shelf => {
            write_clipped(
                buf,
                clip,
                inner_x,
                (y + 2) as u16,
                &shorten(&card.title, text_width),
                title_style,
            );
            write_clipped(
                buf,
                clip,
                inner_x,
                (y + 3) as u16,
                &shorten(&card.subtitle, text_width),
                subtitle_style,
            );
            write_clipped(
                buf,
                clip,
                inner_x,
                (y + 4) as u16,
                &shorten(&card.meta, text_width),
                meta_style,
            );
        }
    }
}

fn draw_home_thumbnail_well(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    y: i32,
    width: u16,
    height: u16,
    initials: &str,
    theme: &TuiTheme,
    accent: bool,
) {
    if width == 0 || height == 0 {
        return;
    }

    let background = if accent {
        theme.accent_fill
    } else {
        theme.content_alt_bg
    };
    for row in 0..height {
        let screen_y = y + row as i32;
        if screen_y < clip.y as i32 || screen_y >= clip.bottom() as i32 {
            continue;
        }
        write_clipped(
            buf,
            clip,
            x,
            screen_y as u16,
            &" ".repeat(width as usize),
            Style::default().bg(background),
        );
    }

    let center_y = y + height as i32 / 2;
    if center_y >= clip.y as i32 && center_y < clip.bottom() as i32 {
        let offset = width.saturating_sub(initials.chars().count() as u16) / 2;
        write_clipped(
            buf,
            clip,
            x.saturating_add(offset),
            center_y as u16,
            initials,
            Style::default()
                .fg(theme.accent_glow)
                .bg(background)
                .add_modifier(Modifier::BOLD),
        );
    }
}

fn render_queue_drop_indicator(
    frame: &mut Frame<'_>,
    queue_inner: Rect,
    raw_slot: usize,
    scroll_offset: usize,
    visible_end: usize,
    queue_len: usize,
    theme: &TuiTheme,
    label: String,
) {
    if queue_inner.width == 0 || queue_inner.height == 0 {
        return;
    }

    let y = if queue_len == 0 || raw_slot <= scroll_offset {
        queue_inner.y
    } else if raw_slot >= visible_end {
        queue_inner.bottom().saturating_sub(1)
    } else {
        queue_inner.y + (raw_slot.saturating_sub(scroll_offset) as u16 * QUEUE_ROW_HEIGHT)
    };

    let area = Rect::new(queue_inner.x, y, queue_inner.width.saturating_sub(1), 1);
    if area.width == 0 {
        return;
    }

    frame.render_widget(
        Block::default().style(Style::default().bg(theme.accent)),
        area,
    );
    frame.render_widget(
        Paragraph::new(shorten(&format!(" {} ", label), area.width as usize))
            .style(
                Style::default()
                    .fg(theme.canvas)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Left),
        area,
    );
}

fn adjusted_reorder_index(current: usize, from: usize, insert_at: usize) -> usize {
    if from == current {
        insert_at
    } else if from < current && insert_at >= current {
        current.saturating_sub(1)
    } else if from > current && insert_at <= current {
        current.saturating_add(1)
    } else {
        current
    }
}

fn extend_radio_touched_for_insert(
    radio_virtual_playlist: &mut Option<RadioVirtualPlaylist>,
    insert_at: usize,
    added: usize,
) {
    if added == 0 {
        return;
    }

    if let Some(radio) = radio_virtual_playlist.as_mut() {
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

fn update_radio_touched_for_reorder(
    radio_virtual_playlist: &mut Option<RadioVirtualPlaylist>,
    from: usize,
    insert_at: usize,
) {
    if let Some(radio) = radio_virtual_playlist.as_mut() {
        radio.touched_len = radio_touched_len_after_reorder(radio.touched_len, from, insert_at);
    }
}

fn shrink_radio_touched_for_remove(
    radio_virtual_playlist: &mut Option<RadioVirtualPlaylist>,
    remove_at: usize,
) {
    if let Some(radio) = radio_virtual_playlist.as_mut() {
        if radio.touched_len == 0 {
            return;
        }

        if remove_at < radio.touched_len {
            radio.touched_len = radio.touched_len.saturating_sub(1);
        }
    }
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

fn draw_box(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    y: i32,
    width: u16,
    height: u16,
    border: Color,
    bg: Color,
) {
    if width < 2 || height < 2 {
        return;
    }

    let top = "─".repeat(width.saturating_sub(2) as usize);
    let bottom = top.clone();
    if y >= clip.y as i32 {
        write_clipped(
            buf,
            clip,
            x,
            y as u16,
            &format!("╭{}╮", top),
            Style::default().fg(border).bg(bg),
        );
    }
    for row in 1..height.saturating_sub(1) {
        let screen_y = y + row as i32;
        if screen_y < clip.y as i32 || screen_y >= clip.bottom() as i32 {
            continue;
        }
        write_clipped(
            buf,
            clip,
            x,
            screen_y as u16,
            "│",
            Style::default().fg(border).bg(bg),
        );
        write_clipped(
            buf,
            clip,
            x + width.saturating_sub(1),
            screen_y as u16,
            "│",
            Style::default().fg(border).bg(bg),
        );
        if width > 2 {
            write_clipped(
                buf,
                clip,
                x + 1,
                screen_y as u16,
                &" ".repeat(width.saturating_sub(2) as usize),
                Style::default().bg(bg),
            );
        }
    }
    let bottom_y = y + height as i32 - 1;
    if bottom_y >= clip.y as i32 && bottom_y < clip.bottom() as i32 {
        write_clipped(
            buf,
            clip,
            x,
            bottom_y as u16,
            &format!("╰{}╯", bottom),
            Style::default().fg(border).bg(bg),
        );
    }
}

fn fill_area(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buf[(x, y)].set_style(style).set_symbol(" ");
        }
    }
}

fn fill_clipped_area(buf: &mut Buffer, clip: Rect, area: Rect, style: Style) {
    let start_y = area.y.max(clip.y);
    let end_y = area.bottom().min(clip.bottom());
    let start_x = area.x.max(clip.x);
    let end_x = area.right().min(clip.right());

    for y in start_y..end_y {
        for x in start_x..end_x {
            buf[(x, y)].set_style(style).set_symbol(" ");
        }
    }
}

fn write_clipped(buf: &mut Buffer, clip: Rect, x: u16, y: u16, text: &str, style: Style) {
    if y < clip.y || y >= clip.bottom() || x >= clip.right() {
        return;
    }

    let max_width = clip.right().saturating_sub(x) as usize;
    let rendered = shorten(text, max_width);
    buf.set_stringn(x, y, rendered, max_width, style);
}

fn home_card_copy(item: &TuiHomeItem, style: HomeCardStyle) -> (String, String, String, String) {
    match item {
        TuiHomeItem::Track(track) => (
            track.title.clone(),
            artist_string(track),
            track
                .album
                .clone()
                .or_else(|| track.duration.clone())
                .unwrap_or_else(|| "Start radio".to_string()),
            match style {
                HomeCardStyle::Featured => track
                    .duration
                    .clone()
                    .unwrap_or_else(|| "TRACK".to_string()),
                _ => "TRACK".to_string(),
            },
        ),
        TuiHomeItem::Browse(card) => (
            card.title.clone(),
            card.subtitle
                .clone()
                .unwrap_or_else(|| browse_label(card.page_type).to_string()),
            format!("Open {}", browse_label(card.page_type).to_lowercase()),
            browse_label(card.page_type).to_uppercase(),
        ),
    }
}

fn home_card_thumbnail_url(card: &HomeCard) -> Option<&str> {
    match &card.item {
        TuiHomeItem::Track(track) => track.thumbnail_url.as_deref(),
        TuiHomeItem::Browse(card) => card.thumbnail_url.as_deref(),
    }
}

fn home_banner_thumbnail_width(width: u16, height: u16) -> u16 {
    if width < 36 || height < 5 {
        return 0;
    }

    let preferred = height.saturating_sub(2).min(12);
    let max_width = width.saturating_sub(22);
    preferred.min(max_width)
}

fn home_banner_thumbnail_rect(screen_rect: Rect) -> Option<Rect> {
    let width = home_banner_thumbnail_width(screen_rect.width, screen_rect.height);
    let height = screen_rect.height.saturating_sub(2);
    if width == 0 || height == 0 {
        return None;
    }

    Some(Rect::new(
        screen_rect.right().saturating_sub(width + 2),
        screen_rect.y.saturating_add(1),
        width,
        height,
    ))
}

fn home_card_thumbnail_width(width: u16, style: HomeCardStyle) -> u16 {
    let inner_width = width.saturating_sub(2);
    if inner_width < 14 {
        return 0;
    }

    let preferred = match style {
        HomeCardStyle::Featured => 12,
        HomeCardStyle::TrackRow => 8,
        HomeCardStyle::BrowseRow | HomeCardStyle::Shelf => 9,
    };
    let max_width = inner_width.saturating_sub(10);
    preferred.min(max_width)
}

fn home_card_thumbnail_rect(screen_rect: Rect, style: HomeCardStyle) -> Option<Rect> {
    let width = home_card_thumbnail_width(screen_rect.width, style);
    let height = screen_rect.height.saturating_sub(2);
    if width == 0 || height == 0 {
        return None;
    }

    Some(Rect::new(
        screen_rect.right().saturating_sub(1 + width),
        screen_rect.y.saturating_add(1),
        width,
        height,
    ))
}

fn placeholder_initials(primary: &str, secondary: &str) -> String {
    let first = primary
        .chars()
        .find(|ch| ch.is_alphanumeric())
        .unwrap_or('S');
    let second = secondary
        .chars()
        .find(|ch| ch.is_alphanumeric())
        .unwrap_or(first);
    format!("{}{}", first.to_uppercase(), second.to_uppercase())
}

fn current_initials(snapshot: &PlaybackSnapshot) -> String {
    let title = snapshot
        .current_track
        .as_ref()
        .map(|track| track.title.as_str())
        .unwrap_or("Static");
    let artist = snapshot
        .current_track
        .as_ref()
        .map(|track| track.artist.as_str())
        .unwrap_or("Daemon");
    placeholder_initials(title, artist)
}

fn active_track_snapshot_from_track(track: &Track, rating: SongRating) -> ActiveTrackSnapshot {
    ActiveTrackSnapshot {
        video_id: track.video_id.clone(),
        title: track.title.clone(),
        artist: artist_string(track),
        thumb_url: track.thumbnail_url.clone(),
        rating,
    }
}

fn center_crop_square(image: DynamicImage) -> DynamicImage {
    let width = image.width();
    let height = image.height();
    let size = width.min(height);
    if size == 0 || width == height {
        return image;
    }

    let x = (width.saturating_sub(size)) / 2;
    let y = (height.saturating_sub(size)) / 2;
    image.crop_imm(x, y, size, size)
}

fn home_columns(width: u16, style: HomeCardStyle, item_count: usize) -> u16 {
    if width < 44 {
        return 1;
    }
    match style {
        HomeCardStyle::Featured => ((width + HOME_GAP) / (26 + HOME_GAP))
            .max(1)
            .min(item_count.max(1) as u16),
        HomeCardStyle::TrackRow | HomeCardStyle::BrowseRow => {
            if width >= 72 {
                2.min(item_count.max(1) as u16)
            } else {
                1
            }
        }
        HomeCardStyle::Shelf => ((width + HOME_GAP) / (28 + HOME_GAP))
            .max(1)
            .min(item_count.max(1) as u16),
    }
}

fn home_card_height(style: HomeCardStyle) -> u16 {
    match style {
        HomeCardStyle::Featured => 7,
        HomeCardStyle::TrackRow => 5,
        HomeCardStyle::BrowseRow | HomeCardStyle::Shelf => 6,
    }
}

fn section_meta_label(style: HomeCardStyle, items: usize) -> String {
    let label = match style {
        HomeCardStyle::Featured => "featured shelf",
        HomeCardStyle::TrackRow => "track grid",
        HomeCardStyle::BrowseRow => "browse grid",
        HomeCardStyle::Shelf => "mixed shelf",
    };
    format!("{label} · {items} items")
}

fn queue_chip_labels(snapshot: &PlaybackSnapshot) -> Vec<String> {
    let mut labels = vec!["All".to_string()];
    if let Some(context) = snapshot.playlist_context.as_ref() {
        labels.push(context.title.clone());
    } else if snapshot.radio_virtual_playlist.is_some() {
        labels.push("Radio".to_string());
    }
    if let Some(radio) = snapshot.radio_virtual_playlist.as_ref() {
        labels.extend(radio.chips.iter().take(3).map(|chip| chip.label.clone()));
    }
    labels
}

fn current_context_label(snapshot: &PlaybackSnapshot) -> String {
    if let Some(context) = snapshot.playlist_context.as_ref() {
        return context.title.clone();
    }
    if snapshot.radio_virtual_playlist.is_some() {
        return "Radio queue".to_string();
    }
    if snapshot.queue.len() > 1 {
        return format!("Queue · {} tracks", snapshot.queue.len());
    }
    "Single track".to_string()
}

fn repeat_loops_left_label(snapshot: &PlaybackSnapshot) -> Option<String> {
    snapshot
        .track_repeat_loops_left
        .filter(|_| snapshot.current_track.is_some())
        .map(|loops_left| {
            let loop_label = if loops_left == 1 { "loop" } else { "loops" };
            format!("{loops_left} {loop_label} left")
        })
}

fn player_bar_meta_spans(snapshot: &PlaybackSnapshot) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        current_context_label(snapshot),
        Style::default().fg(Color::Rgb(130, 160, 255)),
    )];

    if let Some(loop_label) = repeat_loops_left_label(snapshot) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            loop_label,
            Style::default().fg(Color::Rgb(208, 221, 255)),
        ));
    }

    spans
}

fn lyrics_meta_label(lyrics: &SyncedLyrics) -> String {
    let source = match lyrics.source {
        crate::models::SyncedLyricsSource::TimedLyrics => "TIMED LYRICS",
        crate::models::SyncedLyricsSource::Captions => "CAPTIONS",
        crate::models::SyncedLyricsSource::AutoCaptions => "AUTO CAPTIONS",
    };
    if let Some(language_code) = lyrics.language_code.as_deref() {
        format!("{source} · {}", language_code.to_uppercase())
    } else {
        source.to_string()
    }
}

fn active_lyric_line_index(lyrics: &SyncedLyrics, position_millis: u64) -> usize {
    lyrics
        .lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| {
            let active_until = line
                .end_millis
                .or_else(|| lyrics.lines.get(index + 1).map(|next| next.start_millis))
                .unwrap_or(u64::MAX);
            (position_millis >= line.start_millis && position_millis < active_until)
                .then_some(index)
        })
        .unwrap_or(0)
}

fn wrap_text(value: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut current = String::new();

    for word in value.split_whitespace() {
        let projected = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };

        if projected > width && !current.is_empty() {
            lines.push(current);
            current = word.to_string();
            if lines.len() == max_lines {
                break;
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }

    if lines.len() < max_lines && !current.is_empty() {
        lines.push(current);
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
    }
    if let Some(last) = lines.last_mut() {
        *last = shorten(last, width);
    }
    lines
}

fn home_section_tracks(section: &crate::models::HomeSection) -> Vec<Track> {
    section
        .contents
        .iter()
        .filter_map(|item| match item {
            HomeItem::Track(track) => Some(track.clone()),
            _ => None,
        })
        .collect()
}

fn home_section_should_show_play_controls(
    section: &crate::models::HomeSection,
    tracks: &[Track],
) -> bool {
    !tracks.is_empty()
        && (tracks.len() == section.contents.len() || is_listen_again_section(&section.title))
}

fn is_listen_again_section(title: &str) -> bool {
    title.eq_ignore_ascii_case("Listen again")
        || title.to_ascii_lowercase().contains("listen again")
}

fn is_artist_sections(sections: &[crate::models::HomeSection]) -> bool {
    sections.iter().any(|section| {
        matches!(
            section.title.as_str(),
            "Top songs" | "Albums" | "Singles & EPs" | "Featured on" | "Fans might also like"
        )
    })
}

fn home_section_browse_cards(section: &crate::models::HomeSection) -> Vec<HomeBrowseCard> {
    section
        .contents
        .iter()
        .filter_map(|item| match item {
            HomeItem::BrowseCard(card) => Some(card.clone()),
            _ => None,
        })
        .collect()
}

fn browse_label(page_type: HomeBrowsePageType) -> &'static str {
    match page_type {
        HomeBrowsePageType::Playlist => "Playlist",
        HomeBrowsePageType::Album => "Album",
        HomeBrowsePageType::Artist => "Artist",
        HomeBrowsePageType::UserChannel => "Channel",
        HomeBrowsePageType::PodcastShow => "Podcast",
    }
}

fn browse_playlist_context(card: &HomeBrowseCard, track_count: usize) -> PlaylistContext {
    let subtitle = if track_count == 1 {
        "1 track".to_string()
    } else {
        format!("{track_count} tracks")
    };
    PlaylistContext {
        playlist_id: card.browse_id.clone(),
        title: card.title.clone(),
        thumbnail_url: card.thumbnail_url.clone(),
        subtitle: Some(subtitle),
        source: PlaylistContextSource::Browse,
    }
}

fn repeat_button_emoji(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::One => "🔂",
        RepeatMode::Off | RepeatMode::All => "🔁",
    }
}

fn help_shortcut_sections() -> &'static [(&'static str, &'static [(&'static str, &'static str)])] {
    const PLAYBACK: &[(&str, &str)] = &[
        ("space", "Play / pause"),
        ("n", "Next track"),
        ("p", "Previous track"),
        ("f", "Like (favourite)"),
        ("d", "Dislike"),
        ("u", "Clear rating"),
        ("l", "Cycle loop / repeat"),
        ("r", "Resume saved session"),
        ("↑ / ↓", "Volume (Player focus only)"),
        ("- / +", "Volume down / up"),
    ];
    const NAVIGATION: &[(&str, &str)] = &[
        ("tab", "Cycle focus"),
        ("arrows", "Move selection"),
        ("enter", "Activate selection"),
        ("m", "Open context menu"),
        ("b / backspace", "Go back"),
        ("home / end", "Jump to start / end"),
        ("pgup / pgdn", "Scroll one page"),
        ("1 / 2 / t", "Queue · Lyrics · toggle"),
    ];
    const APP: &[(&str, &str)] = &[
        ("h", "Toggle this help"),
        ("q / esc", "Quit"),
    ];
    &[
        ("Playback", PLAYBACK),
        ("Navigation", NAVIGATION),
        ("App", APP),
    ]
}

fn action_label(action: TuiAction) -> &'static str {
    match action {
        TuiAction::ResumeSaved => "Resume",
        TuiAction::TogglePause => "Toggle",
        TuiAction::Previous => "Previous",
        TuiAction::Next => "Next",
        TuiAction::Like => "Like",
        TuiAction::Dislike => "Dislike",
        TuiAction::Neutral => "Neutral",
        TuiAction::CycleRepeat => "Repeat",
        TuiAction::VolumeDown => "Volume down",
        TuiAction::VolumeUp => "Volume up",
    }
}

fn nav_label(nav: TuiNavItem) -> &'static str {
    match nav {
        TuiNavItem::Home => "Home",
        TuiNavItem::Likes => "Likes",
        TuiNavItem::Playlists => "Playlists",
        TuiNavItem::Offline => "Offline",
        TuiNavItem::Settings => "Settings",
    }
}

fn thumbnail_protocol_label(protocol: ProtocolType) -> &'static str {
    match protocol {
        ProtocolType::Halfblocks => "fallback",
        ProtocolType::Sixel => "sixel",
        ProtocolType::Kitty => "kitty",
        ProtocolType::Iterm2 => "iterm2",
    }
}

fn move_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current
            .saturating_add(delta as usize)
            .min(len.saturating_sub(1))
    }
}

fn shorten(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= width {
        return value.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }

    let mut output = chars[..width - 1].iter().collect::<String>();
    output.push('…');
    output
}

fn contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

fn text_cell_width(value: &str) -> usize {
    value.chars().count()
}

fn home_header_actions_layout(area: Rect, header: &HomeHeader) -> Option<HomeHeaderActionsLayout> {
    if !header.show_play_controls || area.height == 0 {
        return None;
    }

    let play_width = text_cell_width(HOME_HEADER_PLAY_ALL_LABEL) as u16;
    let shuffle_width = text_cell_width(HOME_HEADER_SHUFFLE_ALL_LABEL) as u16;
    let total_width = play_width.saturating_add(1).saturating_add(shuffle_width);
    if total_width.saturating_add(6) > area.width {
        return None;
    }

    let shuffle_x = area.right().saturating_sub(shuffle_width);
    let play_x = shuffle_x.saturating_sub(1 + play_width);
    if play_x < area.x {
        return None;
    }

    Some(HomeHeaderActionsLayout {
        play_x,
        play_width,
        shuffle_x,
        shuffle_width,
    })
}

fn thumbnail_resize_for_visible_rect(full_rect: Rect, visible_rect: Rect) -> Resize {
    if visible_rect == full_rect {
        return Resize::Fit(None);
    }

    Resize::Crop(Some(CropOptions {
        clip_top: visible_rect.y > full_rect.y,
        clip_left: visible_rect.x > full_rect.x,
    }))
}

fn intersect_rect(a: Rect, b: Rect) -> Option<Rect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = a.right().min(b.right());
    let bottom = a.bottom().min(b.bottom());
    if left >= right || top >= bottom {
        return None;
    }

    Some(Rect::new(left, top, right - left, bottom - top))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Artist, HomeSection};

    fn sample_track(title: &str) -> Track {
        Track {
            video_id: format!("video-{title}"),
            title: title.to_string(),
            artists: vec![Artist {
                name: "Sample Artist".to_string(),
                id: None,
            }],
            album: Some("Sample Album".to_string()),
            duration: Some("3:45".to_string()),
            thumbnail_url: Some("thumb-sample".to_string()),
            playback_playlist_id: None,
            playback_params: None,
            not_interested_feedback_token: None,
            undo_feedback_token: None,
        }
    }

    fn sample_browse_card(page_type: HomeBrowsePageType, title: &str) -> HomeBrowseCard {
        HomeBrowseCard {
            browse_id: format!("browse-{title}"),
            title: title.to_string(),
            subtitle: Some(format!("{title} subtitle")),
            thumbnail_url: Some("card-thumb".to_string()),
            page_type,
        }
    }

    #[test]
    fn track_browse_pages_render_a_playable_tracks_section() {
        let view = TuiBrowseView {
            card: sample_browse_card(HomeBrowsePageType::Playlist, "Coding Mix"),
            status: BrowseViewStatus::Tracks {
                title: "Coding Mix".to_string(),
                thumbnail_url: Some("banner-thumb".to_string()),
                tracks: vec![sample_track("Alpha"), sample_track("Beta")],
            },
        };

        let doc = build_browse_document(&view, 80);
        assert_eq!(doc.banners.len(), 1);
        assert_eq!(doc.headers.len(), 1);
        assert_eq!(doc.headers[0].title, "TRACKS");
        assert!(doc.headers[0].show_play_controls);
        assert_eq!(doc.cards.len(), 2);
    }

    #[test]
    fn artist_browse_pages_surface_top_songs_first() {
        let top_songs = HomeSection {
            title: "Top songs".to_string(),
            contents: vec![HomeItem::Track(sample_track("Hit Single"))],
        };
        let albums = HomeSection {
            title: "Albums".to_string(),
            contents: vec![HomeItem::BrowseCard(sample_browse_card(
                HomeBrowsePageType::Album,
                "Debut Album",
            ))],
        };
        let view = TuiBrowseView {
            card: sample_browse_card(HomeBrowsePageType::Artist, "Sample Artist"),
            status: BrowseViewStatus::Sections {
                title: "Sample Artist".to_string(),
                thumbnail_url: Some("artist-thumb".to_string()),
                artist_like: true,
                sections: vec![albums, top_songs],
            },
        };

        let doc = build_browse_document(&view, 90);
        assert_eq!(doc.banners.len(), 1);
        assert_eq!(doc.banners[0].eyebrow, "ARTIST");
        assert_eq!(doc.headers.len(), 2);
        assert_eq!(doc.headers[0].title, "TOP SONGS");
        assert!(doc.headers[0].show_play_controls);
    }
}
