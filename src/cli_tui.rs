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
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use image::DynamicImage;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Modifier, Style, Terminal};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Gauge, List, ListItem, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Widget, Wrap,
};
use ratatui::Frame;
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
    Resize, StatefulImage,
};

use crate::cli::{
    artist_string, format_millis, format_track_summary, play_track_with_radio, rating_label,
    restore_playback_snapshot_via_daemon, saved_network_state,
};
use crate::ipc::{
    load_persisted_playback_state, DaemonClient, PlaybackSnapshot, PlaylistContext,
    PlaylistContextSource, RepeatMode,
};
use crate::models::{
    HomeBrowseCard, HomeBrowsePageType, HomeItem, HomePage, RadioVirtualPlaylist, SyncedLyrics,
    Track,
};
use crate::network::{fetch_thumbnail_bytes, SongRating};

const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const MIN_WIDTH: u16 = 124;
const MIN_HEIGHT: u16 = 34;
const SIDEBAR_WIDTH: u16 = 30;
const DETAIL_WIDTH: u16 = 40;
const PLAYER_BAR_HEIGHT: u16 = 8;
const HOME_GAP: u16 = 2;
const RECENT_LIMIT: usize = 32;
const RECENT_ROW_HEIGHT: u16 = 2;
const QUEUE_ROW_HEIGHT: u16 = 3;

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
    warning: Color,
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
            warning: Color::Rgb(255, 196, 92),
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
}

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
    RefreshAll,
}

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
    title: String,
    meta: String,
}

#[derive(Clone)]
struct HomeBanner {
    rect: Rect,
    title: String,
    subtitle: String,
    meta: String,
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

enum HomeFeedStatus {
    Loading,
    Ready(HomePage),
    Error(String),
}

enum TuiBackgroundEvent {
    HomeLoaded(Result<HomePage, String>),
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
    DetailTab(DetailTab),
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
    focus: TuiFocus,
    detail_tab: DetailTab,
    resume_card: Option<ResumeCardData>,
    lyrics_state: LyricsState,
    status_line: String,
    last_refresh_at: Instant,
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
            focus: TuiFocus::Home,
            detail_tab: DetailTab::Queue,
            resume_card,
            lyrics_state: LyricsState::Idle,
            status_line: "Home mirrors the GTK app now: arrows move, enter plays, mouse works."
                .to_string(),
            last_refresh_at: Instant::now(),
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

            let timeout = REFRESH_INTERVAL
                .checked_sub(self.last_refresh_at.elapsed())
                .unwrap_or(Duration::ZERO);

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

            if self.last_refresh_at.elapsed() >= REFRESH_INTERVAL {
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
                StatefulImage::<StatefulProtocol>::default().resize(Resize::Fit(None)),
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
            if self.render_protocol_thumbnail(frame, image_rect, slot, url) {
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
        index: usize,
        card: &HomeCard,
        theme: &TuiTheme,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let image_rect = self.square_thumbnail_rect(area);
        let url = home_card_thumbnail_url(card);
        let failed = url.is_some_and(|url| self.thumbnail_failed(url));
        if let Some(url) = url {
            self.ensure_thumbnail_requested(Some(url));
            if self.render_protocol_thumbnail(
                frame,
                image_rect,
                ThumbnailSlot::HomeCard(index),
                url,
            ) {
                return;
            }
        } else {
            self.thumbnail_slots.remove(&ThumbnailSlot::HomeCard(index));
        }

        render_home_thumbnail_fallback(
            frame,
            image_rect,
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
        self.last_refresh_at = Instant::now();
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
        self.last_refresh_at = Instant::now();
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

    fn close_context_menu(&mut self) {
        self.context_menu = None;
        self.rendered_context_menu = RenderedContextMenu::default();
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
            ContextMenuItem {
                label: "Probe headless".to_string(),
                hint: "no radio".to_string(),
                tone: ContextMenuTone::Default,
                action: ContextMenuAction::ProbeTrack {
                    track: track.clone(),
                },
            },
        ];

        if let Some(token) = track.not_interested_feedback_token.clone() {
            items.push(ContextMenuItem {
                label: "Not interested".to_string(),
                hint: "feedback".to_string(),
                tone: ContextMenuTone::Danger,
                action: ContextMenuAction::SubmitFeedbackToken {
                    token,
                    verb: "Marked as not interested",
                    title: track.title.clone(),
                },
            });
        }
        if let Some(token) = track.undo_feedback_token.clone() {
            items.push(ContextMenuItem {
                label: "Undo feedback".to_string(),
                hint: "restore".to_string(),
                tone: ContextMenuTone::Default,
                action: ContextMenuAction::SubmitFeedbackToken {
                    token,
                    verb: "Feedback undone for",
                    title: track.title.clone(),
                },
            });
        }

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
                let mut items = vec![
                    ContextMenuItem {
                        label: "Play now".to_string(),
                        hint: "keep queue".to_string(),
                        tone: ContextMenuTone::Accent,
                        action: ContextMenuAction::PlayQueueNow { index: *index },
                    },
                    ContextMenuItem {
                        label: "Probe headless".to_string(),
                        hint: "no radio".to_string(),
                        tone: ContextMenuTone::Default,
                        action: ContextMenuAction::ProbeTrack {
                            track: track.clone(),
                        },
                    },
                ];
                if let Some(token) = track.not_interested_feedback_token.clone() {
                    items.push(ContextMenuItem {
                        label: "Not interested".to_string(),
                        hint: "feedback".to_string(),
                        tone: ContextMenuTone::Danger,
                        action: ContextMenuAction::SubmitFeedbackToken {
                            token,
                            verb: "Marked as not interested",
                            title: track.title.clone(),
                        },
                    });
                }
                if let Some(token) = track.undo_feedback_token.clone() {
                    items.push(ContextMenuItem {
                        label: "Undo feedback".to_string(),
                        hint: "restore".to_string(),
                        tone: ContextMenuTone::Default,
                        action: ContextMenuAction::SubmitFeedbackToken {
                            token,
                            verb: "Feedback undone for",
                            title: track.title.clone(),
                        },
                    });
                }
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
                "Context menu open. Enter runs, esc closes, arrows or j/k move.".to_string();
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

    fn play_track_now(&mut self, track: Track) -> Result<()> {
        let snapshot = play_track_with_radio(&self.client, track.clone())?;
        self.apply_snapshot(snapshot);
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        self.status_line = format!("Playing {}", format_track_summary(&track));
        self.last_refresh_at = Instant::now();
        Ok(())
    }

    fn open_browse_card_now(&mut self, card: HomeBrowseCard) -> Result<()> {
        let network = saved_network_state()?;
        let tracks = network
            .get_browse_tracks(&card.browse_id)
            .with_context(|| format!("Failed to open {}", card.title))?;
        if tracks.is_empty() {
            self.status_line = format!("{} has no playable tracks yet.", card.title);
            return Ok(());
        }

        let context = PlaylistContext {
            playlist_id: card.browse_id.clone(),
            title: card.title.clone(),
            thumbnail_url: card.thumbnail_url.clone(),
            subtitle: card.subtitle.clone(),
            source: PlaylistContextSource::Browse,
        };
        let snapshot =
            self.client
                .play_queue_at(tracks, 0, Some(context), None, RepeatMode::Off)?;
        self.apply_snapshot(snapshot);
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        self.status_line = format!("Opened {}", card.title);
        self.last_refresh_at = Instant::now();
        Ok(())
    }

    fn probe_track_now(&mut self, track: Track) -> Result<()> {
        let snapshot = self.client.probe_track(track.clone())?;
        self.apply_snapshot(snapshot);
        self.status_line = format!("Probing {}", format_track_summary(&track));
        self.last_refresh_at = Instant::now();
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
        self.last_refresh_at = Instant::now();
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
        self.last_refresh_at = Instant::now();
        Ok(())
    }

    fn submit_feedback_token_action(
        &mut self,
        token: String,
        verb: &'static str,
        title: String,
    ) -> Result<()> {
        saved_network_state()?.submit_feedback_token(&token)?;
        self.status_line = format!("{verb} {}.", shorten(&title, 28));
        self.last_refresh_at = Instant::now();
        Ok(())
    }

    fn execute_context_menu_action(&mut self, action: ContextMenuAction) -> Result<()> {
        match action {
            ContextMenuAction::PlayTrackNow { track } => self.play_track_now(track),
            ContextMenuAction::PlayQueueNow { index } => self.play_queue_index_now(index),
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
            ContextMenuAction::SubmitFeedbackToken { token, verb, title } => {
                self.submit_feedback_token_action(token, verb, title)
            }
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
            Paragraph::new("enter run  esc close  j/k move")
                .style(Style::default().fg(theme.border_focus)),
            rows[4],
        );

        RenderedContextMenu {
            menu_rect: Some(menu_rect),
            item_rects,
        }
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
                Constraint::Length(5),
                Constraint::Length(4),
                Constraint::Length(11),
                Constraint::Length(2),
                Constraint::Min(8),
                Constraint::Length(3),
            ])
            .split(inner);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        "S  T  A  T  I  C",
                        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        "HEADLESS",
                        Style::default()
                            .fg(theme.accent)
                            .bg(theme.accent_fill)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(Span::styled(
                    "YouTube Music, natively.",
                    Style::default().fg(theme.muted),
                )),
                Line::from(vec![
                    Span::styled("dbus ", Style::default().fg(theme.accent_glow)),
                    Span::styled("daemon", Style::default().fg(theme.muted)),
                    Span::raw("  "),
                    Span::styled(
                        playback_status(&self.snapshot).to_uppercase(),
                        Style::default()
                            .fg(status_color(theme, &self.snapshot))
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
            ]),
            rows[0],
        );

        let search_box = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.elevated_bg))
            .padding(Padding::new(1, 1, 0, 0));
        let search_inner = search_box.inner(rows[1]);
        frame.render_widget(search_box, rows[1]);
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
        let nav_inner = nav_block.inner(rows[2]);
        frame.render_widget(nav_block, rows[2]);
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
            rows[3],
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
        let recent_inner = recent_block.inner(rows[4]);
        frame.render_widget(recent_block, rows[4]);
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
                rows[4],
                &mut scrollbar_state,
            );
        }

        let settings_row = rows[5];
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
            .constraints([Constraint::Length(6), Constraint::Min(8)])
            .split(inner);

        let header = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.content_alt_bg));
        let header_inner = header.inner(rows[0]);
        frame.render_widget(header, rows[0]);
        let header_art_width = self.square_box_width_for_height(header_inner.height).max(6);
        let header_show_thumbnail =
            self.terminal_images_enabled() && header_inner.width >= 24 + header_art_width;
        let (header_copy_area, header_thumbnail_area) = if header_show_thumbnail {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(24), Constraint::Length(header_art_width)])
                .split(header_inner);
            (columns[0], Some(columns[1]))
        } else {
            (header_inner, None)
        };
        let header_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(header_copy_area);

        let home_meta = match &self.home_status {
            HomeFeedStatus::Loading => "refreshing home feed".to_string(),
            HomeFeedStatus::Ready(page) => format!("{} sections live", page.sections.len()),
            HomeFeedStatus::Error(_) => "home feed offline".to_string(),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "HOME",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(home_meta, Style::default().fg(theme.muted)),
                Span::raw("   "),
                Span::styled(
                    Local::now().format("%a %H:%M").to_string(),
                    Style::default().fg(theme.muted),
                ),
            ]))
            .alignment(Alignment::Left),
            header_rows[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "S  T  A  T  I  C",
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    "alternate-screen control room",
                    Style::default().fg(theme.border_focus),
                ),
            ])),
            header_rows[1],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    self.snapshot
                        .current_track
                        .as_ref()
                        .map(|track| shorten(&track.title, header_rows[2].width as usize / 2))
                        .unwrap_or_else(|| "Nothing playing".to_string()),
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    self.snapshot
                        .current_track
                        .as_ref()
                        .map(|track| track.artist.clone())
                        .unwrap_or_else(|| "Daemon ready for headless playback".to_string()),
                    Style::default().fg(theme.muted),
                ),
            ])),
            header_rows[2],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("enter", Style::default().fg(theme.accent)),
                Span::styled(" play selected", Style::default().fg(theme.muted)),
                Span::raw("   "),
                Span::styled("space", Style::default().fg(theme.accent)),
                Span::styled(" pause/resume", Style::default().fg(theme.muted)),
                Span::raw("   "),
                Span::styled("1/2", Style::default().fg(theme.accent)),
                Span::styled(" queue/lyrics", Style::default().fg(theme.muted)),
            ])),
            header_rows[3],
        );
        if let Some(header_thumbnail_area) = header_thumbnail_area {
            let current_thumb_url = self
                .snapshot
                .current_track
                .as_ref()
                .and_then(|track| track.thumb_url.clone());
            self.render_cover_thumbnail(
                frame,
                header_thumbnail_area,
                ThumbnailSlot::ContentHeader,
                current_thumb_url.as_deref(),
                current_initials(&self.snapshot),
                theme,
                true,
            );
        } else {
            self.thumbnail_slots.remove(&ThumbnailSlot::ContentHeader);
        }

        let feed_block = Block::default()
            .style(Style::default().bg(theme.content_bg))
            .padding(Padding::new(1, 1, 0, 0));
        let feed_inner = feed_block.inner(rows[1]);
        frame.render_widget(feed_block, rows[1]);

        layout.home_viewport = feed_inner;
        self.home_viewport_rows = feed_inner.height as usize;
        self.home_doc = build_home_document(
            &self.home_status,
            feed_inner.width,
            self.visible_resume_card(),
        );
        if !self.home_doc.cards.is_empty() {
            self.home_selected = self
                .home_selected
                .min(self.home_doc.cards.len().saturating_sub(1));
        } else {
            self.home_selected = 0;
            self.home_scroll_offset = 0;
        }
        self.ensure_home_visible();

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
        for banner in &self.home_doc.banners {
            let banner_top = banner.rect.y as usize;
            let banner_bottom = banner_top.saturating_add(banner.rect.height as usize);
            if banner_bottom <= visible_top || banner_top >= visible_bottom {
                continue;
            }
            let translated_y = feed_inner
                .y
                .saturating_add(banner.rect.y.saturating_sub(visible_top as u16));
            targets.push(ClickTarget {
                rect: Rect::new(
                    feed_inner.x + banner.rect.x,
                    translated_y,
                    banner.rect.width,
                    banner.rect.height.min(feed_inner.height),
                ),
                kind: ClickTargetKind::Action(TuiAction::ResumeSaved),
            });
        }
        for (index, card) in self.home_doc.cards.iter().enumerate() {
            let card_top = card.rect.y as usize;
            let card_bottom = card_top.saturating_add(card.rect.height as usize);
            if card_bottom <= visible_top || card_top >= visible_bottom {
                continue;
            }
            let translated_y = feed_inner
                .y
                .saturating_add(card.rect.y.saturating_sub(visible_top as u16));
            targets.push(ClickTarget {
                rect: Rect::new(
                    feed_inner.x + card.rect.x,
                    translated_y,
                    card.rect.width,
                    card.rect.height.min(feed_inner.height),
                ),
                kind: ClickTargetKind::HomeItem(index),
            });
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
            for index in 0..self.home_doc.cards.len() {
                let card = self.home_doc.cards[index].clone();
                let card_top = card.rect.y as usize;
                let card_bottom = card_top.saturating_add(card.rect.height as usize);
                if card_bottom <= visible_top || card_top >= visible_bottom {
                    continue;
                }
                let translated_y = feed_inner
                    .y
                    .saturating_add(card.rect.y.saturating_sub(visible_top as u16));
                let screen_rect = Rect::new(
                    feed_inner.x + card.rect.x,
                    translated_y,
                    card.rect.width,
                    card.rect.height.min(feed_inner.height),
                );
                let Some(thumb_rect) = home_card_thumbnail_rect(screen_rect, card.style) else {
                    continue;
                };
                self.render_home_card_thumbnail(frame, thumb_rect, index, &card, theme);
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
            .constraints([
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(4),
            ])
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
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{} tracks", self.snapshot.queue.len()),
                    Style::default().fg(theme.muted),
                ),
                Span::raw("  "),
                Span::styled(
                    repeat_button_label(self.snapshot.repeat_mode),
                    Style::default().fg(theme.accent),
                ),
            ]))
            .alignment(Alignment::Right),
            tab_columns[2],
        );

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
                                            if self.snapshot.paused {
                                                "⏸ "
                                            } else {
                                                "▶ "
                                            }
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

                let footer_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.border_subtle))
                    .style(Style::default().bg(theme.elevated_alt_bg));
                let footer_inner = footer_block.inner(rows[2]);
                frame.render_widget(footer_block, rows[2]);

                let selected_track = self.snapshot.queue.get(self.queue_selected).or_else(|| {
                    self.snapshot.current_track.as_ref().and_then(|current| {
                        self.snapshot
                            .queue
                            .iter()
                            .find(|track| track.video_id == current.video_id)
                    })
                });
                let selection_lines = if let Some(track) = selected_track {
                    vec![
                        Line::from(Span::styled(
                            shorten(&track.title, footer_inner.width as usize),
                            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(vec![
                            Span::styled(
                                shorten(&artist_string(track), footer_inner.width as usize / 2),
                                Style::default().fg(theme.muted),
                            ),
                            Span::raw("  "),
                            Span::styled(
                                format!(
                                    "{}  rating {}",
                                    repeat_mode_label(self.snapshot.repeat_mode),
                                    self.snapshot
                                        .current_track
                                        .as_ref()
                                        .map(|current| rating_label(current.rating))
                                        .unwrap_or("-")
                                ),
                                Style::default().fg(theme.accent),
                            ),
                        ]),
                    ]
                } else {
                    vec![Line::from(Span::styled(
                        "No queue selection yet.",
                        Style::default().fg(theme.muted),
                    ))]
                };
                frame.render_widget(
                    Paragraph::new(selection_lines).wrap(Wrap { trim: true }),
                    footer_inner,
                );
            }
            DetailTab::Lyrics => {
                let lyrics_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(if self.focus == TuiFocus::Queue {
                        theme.border_focus
                    } else {
                        theme.border_subtle
                    }))
                    .style(Style::default().bg(theme.elevated_bg));
                let lyrics_inner = lyrics_block.inner(rows[1]);
                frame.render_widget(lyrics_block, rows[1]);
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
                            self.snapshot.position_millis.unwrap_or(0),
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
                let footer_inner = footer_block.inner(rows[2]);
                frame.render_widget(footer_block, rows[2]);
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
            .constraints([
                Constraint::Length(1),
                Constraint::Length(4),
                Constraint::Length(1),
            ])
            .split(inner);

        let progress_ratio = self
            .snapshot
            .duration_millis
            .filter(|duration| *duration > 0)
            .map(|duration| {
                self.snapshot.position_millis.unwrap_or(0).min(duration) as f64 / duration as f64
            })
            .unwrap_or(0.0);
        layout.player_progress_viewport = rows[0];
        frame.render_widget(
            Gauge::default()
                .ratio(progress_ratio)
                .gauge_style(Style::default().fg(theme.red).bg(theme.border_subtle))
                .label(format!(
                    "{} / {}",
                    self.snapshot
                        .position_millis
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
                Constraint::Length(26),
                Constraint::Min(26),
                Constraint::Length(44),
            ])
            .split(rows[1]);

        let transport = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Length(6),
                Constraint::Length(6),
            ])
            .split(control_columns[0]);
        targets.push(render_action_button(
            frame,
            transport[0],
            "Prev",
            TuiAction::Previous,
            theme,
            false,
            false,
        ));
        targets.push(render_action_button(
            frame,
            transport[1],
            if self.snapshot.paused {
                "Play"
            } else {
                "Pause"
            },
            TuiAction::TogglePause,
            theme,
            true,
            true,
        ));
        targets.push(render_action_button(
            frame,
            transport[2],
            "Next",
            TuiAction::Next,
            theme,
            false,
            false,
        ));

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    playback_status(&self.snapshot).to_uppercase(),
                    Style::default()
                        .fg(status_color(theme, &self.snapshot))
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!(
                        "{}/{}",
                        self.snapshot.queue_index.saturating_add(1),
                        self.snapshot.queue.len().max(1)
                    ),
                    Style::default().fg(theme.muted),
                )),
            ])
            .alignment(Alignment::Center),
            transport[3],
        );

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
                Line::from(Span::styled(
                    current_context_label(&self.snapshot),
                    Style::default().fg(theme.accent),
                )),
            ])
            .alignment(Alignment::Left),
            track_info[1],
        );

        let actions = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(6),
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Length(4),
            ])
            .split(control_columns[2]);

        targets.push(render_action_button(
            frame,
            actions[0],
            "Dis",
            TuiAction::Dislike,
            theme,
            false,
            self.snapshot
                .current_track
                .as_ref()
                .is_some_and(|track| track.rating == SongRating::Dislike),
        ));
        targets.push(render_action_button(
            frame,
            actions[1],
            "Like",
            TuiAction::Like,
            theme,
            false,
            self.snapshot
                .current_track
                .as_ref()
                .is_some_and(|track| track.rating == SongRating::Like),
        ));
        targets.push(render_action_button(
            frame,
            actions[2],
            repeat_button_label(self.snapshot.repeat_mode),
            TuiAction::CycleRepeat,
            theme,
            false,
            self.snapshot.repeat_mode != RepeatMode::Off,
        ));
        targets.push(render_action_button(
            frame,
            actions[3],
            "Sync",
            TuiAction::RefreshAll,
            theme,
            false,
            false,
        ));
        targets.push(render_action_button(
            frame,
            actions[4],
            "Resume",
            TuiAction::ResumeSaved,
            theme,
            false,
            self.resume_card.is_some(),
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
            actions[5],
        );

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(&self.status_line, Style::default().fg(theme.muted)),
                Span::raw("   "),
                Span::styled(
                    "tab focus  enter play  space toggle  r resume  mouse enabled",
                    Style::default().fg(theme.border_focus),
                ),
                Span::raw("   "),
                Span::styled(
                    &self.thumbnail_mode_label,
                    Style::default().fg(theme.accent),
                ),
            ]))
            .wrap(Wrap { trim: true }),
            rows[2],
        );

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
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_context_menu_selection(-1);
                    return Ok(false);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_context_menu_selection(1);
                    return Ok(false);
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.select_context_menu_item(0);
                    return Ok(false);
                }
                KeyCode::End | KeyCode::Char('G') => {
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

        match code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(true),
            KeyCode::Tab => self.cycle_focus(),
            KeyCode::Char(' ') => self.run_action(TuiAction::TogglePause)?,
            KeyCode::Char('r') => self.run_action(TuiAction::ResumeSaved)?,
            KeyCode::Char('1') => self.detail_tab = DetailTab::Queue,
            KeyCode::Char('2') => self.detail_tab = DetailTab::Lyrics,
            KeyCode::Char('t') => self.toggle_detail_tab(),
            KeyCode::Char('n') => self.run_action(TuiAction::Next)?,
            KeyCode::Char('p') => self.run_action(TuiAction::Previous)?,
            KeyCode::Char('l') => self.run_action(TuiAction::Like)?,
            KeyCode::Char('d') => self.run_action(TuiAction::Dislike)?,
            KeyCode::Char('u') => self.run_action(TuiAction::Neutral)?,
            KeyCode::Char('R') => self.run_action(TuiAction::RefreshAll)?,
            KeyCode::Left => self.move_selection(NavigationDirection::Left),
            KeyCode::Right => self.move_selection(NavigationDirection::Right),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(NavigationDirection::Down),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(NavigationDirection::Up),
            KeyCode::Home | KeyCode::Char('g') => self.jump_to_start(),
            KeyCode::End | KeyCode::Char('G') => self.jump_to_end(),
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
                    self.home_status = HomeFeedStatus::Ready(page);
                    self.home_selected = 0;
                    self.home_scroll_offset = 0;
                    self.status_line = "Home feed loaded.".to_string();
                }
                TuiBackgroundEvent::HomeLoaded(Err(err)) => {
                    self.home_status = HomeFeedStatus::Error(err.clone());
                    self.home_selected = 0;
                    self.home_scroll_offset = 0;
                    self.status_line = err;
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
                self.status_line = format!("Daemon refresh failed: {err}");
            }
        }
        self.last_refresh_at = Instant::now();
    }

    fn request_home_reload(&mut self) {
        self.home_status = HomeFeedStatus::Loading;
        spawn_home_bootstrap(self.background_tx.clone());
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
        self.queue_selected = self
            .queue_selected
            .min(self.snapshot.queue.len().saturating_sub(1));

        if previous_video_id != next_video_id {
            self.maybe_request_lyrics();
        }
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

    fn toggle_detail_tab(&mut self) {
        self.detail_tab = match self.detail_tab {
            DetailTab::Queue => DetailTab::Lyrics,
            DetailTab::Lyrics => DetailTab::Queue,
        };
    }

    fn run_action(&mut self, action: TuiAction) -> Result<()> {
        let result = match action {
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
            TuiAction::RefreshAll => {
                self.request_home_reload();
                self.client.state()
            }
        };

        match result {
            Ok(snapshot) => {
                self.apply_snapshot(snapshot);
                if matches!(action, TuiAction::ResumeSaved) {
                    self.resume_card = None;
                }
                self.status_line = format!("{} applied.", action_label(action));
            }
            Err(err) => self.status_line = format!("Action failed: {err}"),
        }

        self.last_refresh_at = Instant::now();
        Ok(())
    }

    fn activate_selection(&mut self) -> Result<()> {
        match self.focus {
            TuiFocus::Home => self.activate_home_selection(),
            TuiFocus::Queue => self.activate_queue_selection(),
            TuiFocus::Recent => self.activate_recent_selection(),
        }
    }

    fn activate_click_target(&mut self, kind: &ClickTargetKind) -> Result<()> {
        match kind {
            ClickTargetKind::Action(action) => self.run_action(*action),
            ClickTargetKind::Nav(nav) => self.handle_nav(*nav),
            ClickTargetKind::DetailTab(tab) => {
                self.detail_tab = *tab;
                Ok(())
            }
            ClickTargetKind::HomeItem(index) => {
                self.focus = TuiFocus::Home;
                self.home_selected = *index;
                self.ensure_home_visible();
                self.activate_home_selection()
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
                self.request_home_reload();
                self.status_line = "Reloading Home.".to_string();
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

    fn activate_home_selection(&mut self) -> Result<()> {
        let Some(card) = self.home_doc.cards.get(self.home_selected).cloned() else {
            self.status_line = "Home feed is still loading.".to_string();
            return Ok(());
        };

        match card.item {
            TuiHomeItem::Track(track) => {
                let snapshot = play_track_with_radio(&self.client, track.clone())?;
                self.apply_snapshot(snapshot);
                self.detail_tab = DetailTab::Queue;
                self.status_line = format!("Playing {}", format_track_summary(&track));
            }
            TuiHomeItem::Browse(card) => {
                let network = saved_network_state()?;
                let tracks = network
                    .get_browse_tracks(&card.browse_id)
                    .with_context(|| format!("Failed to open {}", card.title))?;
                if tracks.is_empty() {
                    self.status_line = format!("{} has no playable tracks yet.", card.title);
                    return Ok(());
                }
                let context = PlaylistContext {
                    playlist_id: card.browse_id.clone(),
                    title: card.title.clone(),
                    thumbnail_url: card.thumbnail_url.clone(),
                    subtitle: card.subtitle.clone(),
                    source: PlaylistContextSource::Browse,
                };
                let snapshot =
                    self.client
                        .play_queue_at(tracks, 0, Some(context), None, RepeatMode::Off)?;
                self.apply_snapshot(snapshot);
                self.detail_tab = DetailTab::Queue;
                self.status_line = format!("Opened {}", card.title);
            }
        }

        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        self.last_refresh_at = Instant::now();
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
        self.last_refresh_at = Instant::now();
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
        self.last_refresh_at = Instant::now();
        Ok(())
    }

    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            TuiFocus::Home => TuiFocus::Queue,
            TuiFocus::Queue => TuiFocus::Recent,
            TuiFocus::Recent => TuiFocus::Home,
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
                    NavigationDirection::Up | NavigationDirection::Left => -1,
                    NavigationDirection::Down | NavigationDirection::Right => 1,
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
                    NavigationDirection::Up | NavigationDirection::Left => -1,
                    NavigationDirection::Down | NavigationDirection::Right => 1,
                };
                self.recent_selected =
                    move_index(self.recent_selected, self.recent_tracks.len(), delta);
                self.ensure_recent_visible();
            }
        }
    }

    fn move_home_selection(&mut self, direction: NavigationDirection) {
        let Some(current) = self.home_doc.cards.get(self.home_selected) else {
            self.home_selected = 0;
            return;
        };

        let current_center_x = current.rect.x as i32 + current.rect.width as i32 / 2;
        let current_center_y = current.rect.y as i32 + current.rect.height as i32 / 2;
        let mut best_index = self.home_selected;
        let mut best_score = (i32::MAX, i32::MAX);

        for (index, candidate) in self.home_doc.cards.iter().enumerate() {
            if index == self.home_selected {
                continue;
            }

            let candidate_center_x = candidate.rect.x as i32 + candidate.rect.width as i32 / 2;
            let candidate_center_y = candidate.rect.y as i32 + candidate.rect.height as i32 / 2;
            let delta_x = candidate_center_x - current_center_x;
            let delta_y = candidate_center_y - current_center_y;

            let qualifies = match direction {
                NavigationDirection::Left => delta_x < 0,
                NavigationDirection::Right => delta_x > 0,
                NavigationDirection::Up => delta_y < 0,
                NavigationDirection::Down => delta_y > 0,
            };
            if !qualifies {
                continue;
            }

            let primary = match direction {
                NavigationDirection::Left | NavigationDirection::Right => delta_x.abs(),
                NavigationDirection::Up | NavigationDirection::Down => delta_y.abs(),
            };
            let secondary = match direction {
                NavigationDirection::Left | NavigationDirection::Right => delta_y.abs(),
                NavigationDirection::Up | NavigationDirection::Down => delta_x.abs(),
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
            write_clipped(
                buf,
                area,
                area.x,
                y as u16,
                &format!("{}  {}", header.title, header.meta),
                Style::default()
                    .fg(self.theme.text)
                    .add_modifier(Modifier::BOLD),
            );
            if area.width > header.title.len() as u16 + header.meta.len() as u16 + 6 {
                let divider_x = area.x + (header.title.len() + header.meta.len() + 4) as u16;
                let divider_width = area.right().saturating_sub(divider_x) as usize;
                write_clipped(
                    buf,
                    area,
                    divider_x,
                    y as u16,
                    &"─".repeat(divider_width),
                    Style::default().fg(self.theme.border_subtle),
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
            rect: Rect::new(0, y as u16, usable_width, 5),
            title: format!("Resume {}", resume_card.track.title),
            subtitle: artist_string(&resume_card.track),
            meta: resume_card.meta.clone(),
        });
        y += 6;
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
                doc.headers.push(HomeHeader {
                    y,
                    title: section.title.to_uppercase(),
                    meta: format!(
                        "· {}",
                        section_meta_label(section_style, section.contents.len())
                    ),
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

fn render_action_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    action: TuiAction,
    theme: &TuiTheme,
    primary: bool,
    active: bool,
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
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
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
    let bg = theme.accent_fill;
    draw_box(
        buf,
        clip,
        x,
        y,
        banner.rect.width,
        banner.rect.height,
        theme.accent,
        bg,
    );

    let inner_x = x + 2;
    let inner_width = banner.rect.width.saturating_sub(4) as usize;
    write_clipped(
        buf,
        clip,
        inner_x,
        y.max(clip.y as i32) as u16 + 1,
        &banner.title,
        Style::default()
            .fg(theme.accent_glow)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    );
    write_clipped(
        buf,
        clip,
        inner_x,
        y.max(clip.y as i32) as u16 + 2,
        &shorten(&banner.subtitle, inner_width),
        Style::default().fg(theme.text).bg(bg),
    );
    write_clipped(
        buf,
        clip,
        inner_x,
        y.max(clip.y as i32) as u16 + 3,
        &shorten(
            &format!("{}  •  press r or click to restore", banner.meta),
            inner_width,
        ),
        Style::default().fg(theme.muted).bg(bg),
    );
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

fn playback_status(snapshot: &PlaybackSnapshot) -> &'static str {
    if snapshot.probe_active {
        "probe"
    } else if snapshot.current_track.is_some() {
        if snapshot.paused {
            "paused"
        } else {
            "playing"
        }
    } else {
        "stopped"
    }
}

fn status_color(theme: &TuiTheme, snapshot: &PlaybackSnapshot) -> Color {
    match playback_status(snapshot) {
        "playing" => theme.positive,
        "paused" => theme.warning,
        "probe" => theme.accent,
        _ => theme.muted,
    }
}

fn repeat_mode_label(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "repeat off",
        RepeatMode::All => "repeat all",
        RepeatMode::One => "repeat one",
    }
}

fn repeat_button_label(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "Loop",
        RepeatMode::All => "Loop+",
        RepeatMode::One => "Loop1",
    }
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
        TuiAction::RefreshAll => "Refresh",
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
