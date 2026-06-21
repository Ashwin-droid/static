//! Right detail panel: "Up Next" / "Lyrics" tabs, Dismiss Queue, radio chip
//! bar, the queue list with drag-and-drop reordering + drop indicator +
//! autoscroll, the lyrics page, queue mutations, the track context menu, and
//! download/offline actions.
//!
//! Specs: queue_actions.md (all),
//! queue_drag.md (all), panels.md §1.1–§2 (detail panel + lyrics),
//! styles.md §3.8.
//! GTK sources: src/ui/window/queue_actions.rs, src/ui/window/queue_drag.rs,
//! src/ui/window/detail_panel.rs, src/ui/window/page_builders.rs §4.4.
//!
//! DRAG-AND-DROP DESIGN (binding — do not improvise; CONTRACT.md §7):
//! mouse_area-based drag. Queue rows and draggable cards are wrapped in
//! `iced::widget::mouse_area`; `on_press` records a pending drag (payload +
//! origin point), the panel-level `mouse_area` reports `on_move` /
//! `on_release`. A drag becomes active once the pointer moves ≥ 6 px from the
//! origin. While active: hover slot = `preview_slot_for_queue_y` (row-height
//! math, ported below), rendered as an inline drop-indicator row inserted at
//! the preview slot in the queue column; autoscroll runs on the shell's 16 ms
//! tick (`Msg::AutoscrollTick`, emitted only while `State.drag.is_some()`) and
//! scrolls via `iced::widget::operation::scroll_by(app::QUEUE_SCROLL_ID, ...)`
//! with `queue_drag_autoscroll_velocity`. Release over the list calls
//! `normalized_preview_slot` then `DaemonClient::move_queue_item` /
//! `insert_into_queue`; release elsewhere cancels.
//!
//! Radio invariants: generated tail rows render below the "radio" divider
//! (`touched_len`); user-touched rows may only grow the prefix
//! (`app.player.mark_radio_touched_through`); dismissing the queue after the
//! current song exits radio mode.

use std::sync::{Arc, Mutex};

use iced::widget::{Space, button, column, container, mouse_area, row, scrollable, text};
use iced::{Alignment, Color, Element, Font, Length, Padding, Point, Task, Theme};

use crate::ipc::{
    ActiveTrackSnapshot, DaemonClient, PlaybackSnapshot, PlaylistContext, youtube_music_watch_url,
};
use crate::models::{Artist, HomeBrowsePageType, SyncedLyricLine, SyncedLyrics, Track};
use crate::network::SongRating;
use crate::network_state::NetworkState;
use crate::offline::{AppSettings, MusicCache};

use super::app::{self, App, Message, QueueDragPayload, QueuePlaylistSource};
use super::{player_bar, theme, thumbnails};

/// Queue row visual height in px (40 px art + padding); used by slot math.
pub const QUEUE_ROW_HEIGHT: f32 = 56.0;
/// Pointer travel before a press becomes a drag.
pub const DRAG_START_THRESHOLD: f32 = 6.0;
/// Height of the top/bottom autoscroll bands inside the queue viewport.
pub const AUTOSCROLL_BAND: f32 = 56.0;

/// Px scrolled per 16 ms autoscroll tick at the inner/outer band edges
/// (GTK queue_drag MIN_STEP/MAX_STEP).
const AUTOSCROLL_MIN_STEP: f32 = 8.0;
const AUTOSCROLL_MAX_STEP: f32 = 28.0;

/// Fixed geometry of the panel chrome above the queue scroll viewport; slot
/// math derives the viewport-relative pointer y from these.
const HEADER_HEIGHT: f32 = 54.0;
const TAB_HEIGHT: f32 = 34.0;
const CHIP_BAR_HEIGHT: f32 = 40.0;
/// Top padding inside the queue scrollable (GTK queue_list_box margin_top 4).
const LIST_TOP_MARGIN: f32 = 4.0;
/// Height of the "Generated from this radio" divider element.
const DIVIDER_HEIGHT: f32 = 40.0;
/// Viewport-height fallback before the first `Msg::Scrolled` reported real
/// geometry (only affects autoscroll band placement).
const FALLBACK_VIEWPORT_HEIGHT: f32 = 480.0;

/// GTK `scroll_active_lyric_into_view` kept the highlighted line inside a
/// 72 px padding band of the lyrics scroller.
const LYRICS_FOLLOW_BAND: f32 = 72.0;
/// Estimated rendered height of one lyrics line (SIZE_HEADER text at iced's
/// default 1.3 relative line height; lines are rendered unwrapped-equivalent
/// single rows spaced by SPACE_MD).
const LYRICS_LINE_HEIGHT: f32 = theme::SIZE_HEADER * 1.3;

const LYRICS_IDLE: &str = "Play something to see synced lyrics when YouTube exposes timed text.";
const LYRICS_LOADING: &str = "Loading timed lines for this track...";
const LYRICS_UNAVAILABLE: &str =
    "This track did not expose timed text, so there isn’t anything to sync yet.";
const LYRICS_FAILED: &str = "Timed lyrics could not be loaded for this track.";
const LYRICS_NOT_CONNECTED: &str = "Connect YouTube Music to load synced lyrics.";

const SEMIBOLD: Font = Font {
    weight: iced::font::Weight::Semibold,
    ..Font::DEFAULT
};
const BOLD: Font = Font {
    weight: iced::font::Weight::Bold,
    ..Font::DEFAULT
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailPanelView {
    Queue,
    Lyrics,
}

pub enum LyricsStatus {
    /// "Play something to see synced lyrics when YouTube exposes timed text."
    Idle,
    Loading,
    Loaded,
    /// "Synced lyrics unavailable" + explanation.
    Unavailable(String),
    /// "Lyrics fetch failed".
    Error(String),
    /// "Not connected" / "Connect YouTube Music to load synced lyrics."
    NotConnected,
}

pub struct LyricsState {
    pub status: LyricsStatus,
    pub meta: Option<String>,
    pub lines: Vec<SyncedLyricLine>,
    pub active_line: Option<usize>,
    pub request_serial: u64,
}

/// An in-progress drag (internal queue row or external card payload).
pub struct DragState {
    pub payload: QueueDragPayload,
    pub origin: Point,
    pub position: Point,
    /// True once the pointer moved ≥ `DRAG_START_THRESHOLD` from origin.
    pub active: bool,
}

/// Which feedback action the context menu currently offers for its track.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FeedbackPhase {
    /// Track carries no feedback token — nothing rendered.
    Hidden,
    NotInterestedAvailable,
    NotInterestedInFlight,
    UndoAvailable,
    UndoInFlight,
    /// "Not interested" succeeded with no undo token: both actions gone.
    Done,
}

/// Track context menu (right-click / menu button on rows): play next, add to
/// queue, remove, like/dislike/not-interested feedback, download, share, go to
/// album/artist (queue_actions spec §8–9).
pub struct TrackMenu {
    pub track: Track,
    /// Set when opened from a queue row (enables "Remove from queue").
    pub queue_index: Option<usize>,
    /// Pointer position at open time. Accepted v1 tradeoff: iced has no
    /// native popovers, so the menu renders as a centered window-modal card
    /// instead of anchoring here (content views pass `Point::ORIGIN` anyway);
    /// kept for future anchored positioning.
    #[allow(dead_code)]
    pub anchor: Point,
    feedback: FeedbackPhase,
    /// `None` while the blocking `is_pinned_offline` probe is in flight.
    pinned_offline: Option<bool>,
}

/// Playlist metadata for "Download All" (queue_actions spec §9.3).
#[derive(Clone)]
pub struct DownloadPlaylistMeta {
    pub playlist_id: String,
    pub title: String,
    pub thumbnail_url: Option<String>,
    pub subtitle: Option<String>,
}

pub struct State {
    pub mode: DetailPanelView,
    /// Latched true the first time a queue exists; the shell keeps the panel
    /// rendered afterwards (GTK parity: `render_queue_panel` never re-hid it).
    pub shown: bool,
    pub lyrics: LyricsState,
    /// Lyrics scrollable geometry mirrored from `Msg::LyricsScrolled` (also
    /// updated optimistically when the follow-along scroll issues a
    /// `scroll_to`, since programmatic scrolls do not publish `on_scroll`).
    pub lyrics_scroll_offset: f32,
    pub lyrics_viewport_height: f32,
    pub drag: Option<DragState>,
    /// Normalized preview slot while dragging over the list.
    pub drop_preview_slot: Option<usize>,
    /// Pointer y within the queue viewport while dragging (drives autoscroll).
    pub drag_hover_y: Option<f32>,
    /// True while a payload is dragged over the panel (background tint).
    pub drop_active: bool,
    /// Queue scrollable geometry mirrored from `Msg::Scrolled`.
    pub scroll_offset: f32,
    pub viewport_height: f32,
    pub content_height: f32,
    /// Serial guard for queue mutation daemon calls.
    pub op_serial: u64,
    /// Last pointer position in panel-local coordinates (drag origin and
    /// context-menu anchor for queue rows).
    pub panel_pointer: Point,
    pub hovered_row: Option<usize>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            mode: DetailPanelView::Queue,
            shown: false,
            lyrics: LyricsState {
                status: LyricsStatus::Idle,
                meta: None,
                lines: Vec::new(),
                active_line: None,
                request_serial: 0,
            },
            lyrics_scroll_offset: 0.0,
            lyrics_viewport_height: 0.0,
            drag: None,
            drop_preview_slot: None,
            drag_hover_y: None,
            drop_active: false,
            scroll_offset: 0.0,
            viewport_height: 0.0,
            content_height: 0.0,
            op_serial: 0,
            panel_pointer: Point::ORIGIN,
            hovered_row: None,
        }
    }
}

#[derive(Clone)]
pub enum Msg {
    TabSelected(DetailPanelView),
    DismissQueuePressed,
    RowClicked(usize),
    RowHovered(Option<usize>),
    ChipClicked(usize),
    JumpToSeedPressed,

    // Drag-and-drop (see module doc for the binding design).
    DragStarted {
        payload: QueueDragPayload,
        position: Point,
    },
    PointerMoved(Point),
    PointerReleased,
    /// Pointer left the panel (GTK DropTarget `leave`): clear preview +
    /// autoscroll + tint; the drag itself stays pending.
    PointerExited,
    /// Global left-button release that no widget handler consumed (shell
    /// subscription, active only while a drag exists): end the drag entirely,
    /// mirroring GTK's unconditional `drag_end`.
    DragAborted,
    AutoscrollTick,
    Scrolled {
        offset_y: f32,
        viewport_height: f32,
        content_height: f32,
    },
    /// Lyrics page scroll geometry (drives the active-line follow band).
    LyricsScrolled {
        offset_y: f32,
        viewport_height: f32,
    },

    /// Completion of any queue-mutation daemon command (serial-guarded).
    QueueOpFinished {
        serial: u64,
        result: Result<PlaybackSnapshot, String>,
    },
    /// Completion of a play/probe daemon command (play-request-serial-guarded;
    /// errors surface in the now-playing title, queue_actions spec §5.1).
    PlayOpFinished {
        serial: u64,
        result: Result<PlaybackSnapshot, String>,
    },
    /// A dropped playlist/browse collection finished resolving its tracks.
    DropCollectionResolved {
        slot: usize,
        result: Result<Vec<Track>, String>,
    },
    /// `NetworkState::get_synced_lyrics` result, gated by serial + session.
    LyricsLoaded {
        serial: u64,
        session_id: u64,
        video_id: String,
        result: Result<Option<SyncedLyrics>, String>,
    },
    /// Radio tail refresh fetch finished (queue_actions spec; dedupe by
    /// anchor, drop stale fetches against newer queues). The daemon owns tail
    /// refreshes today; the handler is kept for parity with GTK's defensive
    /// reconciliation should a UI-side producer return.
    #[allow(dead_code)]
    RadioTailLoaded {
        anchor: String,
        result: Result<Vec<Track>, String>,
    },
}

/// Track context-menu messages (panel-owned; rendered as a shell overlay).
#[derive(Clone)]
pub enum MenuMsg {
    Open {
        track: Track,
        queue_index: Option<usize>,
        anchor: Point,
    },
    Dismiss,
    PlayNext,
    AddToQueue,
    RemoveFromQueue,
    StartRadio,
    Probe,
    Download {
        pin: bool,
    },
    DeleteOffline,
    Share,
    NotInterested,
    PinnedProbed {
        video_id: String,
        pinned: bool,
    },
    FeedbackSubmitted {
        undo: bool,
        video_id: String,
        result: Result<(), String>,
    },
    DownloadFinished {
        video_id: String,
        now_pinned: bool,
        result: Result<(), String>,
    },
}

fn qmsg(msg: Msg) -> Message {
    Message::Queue(msg)
}

fn mmsg(msg: MenuMsg) -> Message {
    Message::TrackMenu(msg)
}

// ---------------------------------------------------------------------------
// View / update
// ---------------------------------------------------------------------------

/// The whole right panel: tab header + Dismiss Queue, then the queue page
/// (chip bar + list + inline drop indicator) or the lyrics page.
pub fn view(app: &App) -> Element<'_, Message> {
    let body = match app.queue_panel.mode {
        DetailPanelView::Queue => queue_page(app),
        DetailPanelView::Lyrics => lyrics_page(app),
    };

    let content = column![header_row(app), body]
        .width(Length::Fill)
        .height(Length::Fill);

    mouse_area(content)
        .on_move(|position| qmsg(Msg::PointerMoved(position)))
        .on_release(qmsg(Msg::PointerReleased))
        .on_exit(qmsg(Msg::PointerExited))
        .into()
}

fn header_row(app: &App) -> Element<'_, Message> {
    let state = &app.queue_panel;
    let mut header = row![
        tab_button("Up Next", DetailPanelView::Queue, state.mode),
        tab_button("Lyrics", DetailPanelView::Lyrics, state.mode),
        Space::new().width(Length::Fill),
    ]
    .spacing(theme::SPACE_SM)
    .align_y(Alignment::Center)
    .width(Length::Fill);

    if state.mode == DetailPanelView::Queue {
        let can_dismiss = app.player.queue_index.saturating_add(1) < app.player.queue.len();
        header = header.push(
            button(text("Dismiss Queue").size(theme::SIZE_SUB))
                .style(theme::secondary_button)
                .height(Length::Fixed(TAB_HEIGHT))
                .padding([theme::SPACE_SM, theme::SPACE_LG])
                .on_press_maybe(can_dismiss.then(|| qmsg(Msg::DismissQueuePressed))),
        );
    }

    container(header)
        .height(Length::Fixed(HEADER_HEIGHT))
        .align_y(iced::alignment::Vertical::Center)
        .padding(Padding {
            top: theme::SPACE_LG,
            right: theme::SPACE_XL,
            bottom: theme::SPACE_MD,
            left: theme::SPACE_XL,
        })
        .into()
}

fn tab_button(
    label: &'static str,
    view: DetailPanelView,
    current: DetailPanelView,
) -> Element<'static, Message> {
    button(text(label).size(theme::SIZE_SUB))
        .style(theme::detail_tab(current == view))
        .height(Length::Fixed(TAB_HEIGHT))
        .padding([theme::SPACE_SM, theme::SPACE_XL])
        .on_press(qmsg(Msg::TabSelected(view)))
        .into()
}

// ---------------------------------------------------------------------------
// Queue page
// ---------------------------------------------------------------------------

fn queue_page(app: &App) -> Element<'_, Message> {
    let mut page = column![].width(Length::Fill).height(Length::Fill);
    if has_radio_chips(app) {
        page = page.push(chip_bar(app));
    }
    page.push(queue_list(app)).into()
}

fn has_radio_chips(app: &App) -> bool {
    app.player
        .radio_virtual_playlist
        .as_ref()
        .is_some_and(|vp| !vp.chips.is_empty())
}

fn chip_bar(app: &App) -> Element<'_, Message> {
    let selected = app.player.selected_radio_chip_index();
    let mut chips = row![chip_button(
        "Seed".to_owned(),
        false,
        qmsg(Msg::JumpToSeedPressed)
    )]
    .spacing(theme::SPACE_SM)
    .align_y(Alignment::Center)
    .padding(Padding {
        top: 0.0,
        right: theme::SPACE_XL,
        bottom: 0.0,
        left: theme::SPACE_XL,
    });
    for (chip_index, chip) in app.player.radio_chips().into_iter().enumerate() {
        chips = chips.push(chip_button(
            chip.label,
            selected == Some(chip_index),
            qmsg(Msg::ChipClicked(chip_index)),
        ));
    }

    container(
        scrollable(chips)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(2).scroller_width(2),
            ))
            .style(theme::thin_scrollbar)
            .width(Length::Fill),
    )
    .height(Length::Fixed(CHIP_BAR_HEIGHT))
    .align_y(iced::alignment::Vertical::Center)
    .into()
}

fn chip_button(label: String, selected: bool, on_press: Message) -> Element<'static, Message> {
    button(text(label).size(theme::SIZE_SUB))
        .style(theme::chip(selected))
        .padding([theme::SPACE_XS, theme::SPACE_14])
        .on_press(on_press)
        .into()
}

fn queue_list(app: &App) -> Element<'_, Message> {
    let state = &app.queue_panel;
    let queue = &app.player.queue;
    let tail_start = radio_tail_start(app);
    let indicator_slot = state
        .drag
        .as_ref()
        .filter(|drag| drag.active)
        .and(state.drop_preview_slot)
        .map(|slot| slot.min(queue.len()));
    let drag_source = state.drag.as_ref().and_then(|drag| match drag.payload {
        QueueDragPayload::QueueRow { index } => drag.active.then_some(index),
        _ => None,
    });
    let drag_active = state.drag.as_ref().is_some_and(|drag| drag.active);
    let menu_row = app
        .track_menu
        .as_ref()
        .and_then(|menu| menu.queue_index)
        .filter(|index| queue.get(*index).is_some());

    let mut items: Vec<Element<'_, Message>> = Vec::with_capacity(queue.len() + 2);
    for (index, track) in queue.iter().enumerate() {
        if tail_start == Some(index) {
            items.push(radio_divider());
        }
        if indicator_slot == Some(index) {
            items.push(drop_indicator_row(app));
        }
        let generated = tail_start.is_some_and(|start| index >= start);
        items.push(queue_row(
            app,
            index,
            track,
            RowVisual {
                active: index == app.player.queue_index,
                generated,
                hovered: state.hovered_row == Some(index) && !drag_active,
                lifted: drag_source == Some(index),
                dimmed: (drag_active && drag_source != Some(index))
                    || (menu_row.is_some() && menu_row != Some(index)),
                focused: menu_row == Some(index),
            },
        ));
    }
    if indicator_slot == Some(queue.len()) {
        items.push(drop_indicator_row(app));
    }

    scrollable(column(items).width(Length::Fill).padding(Padding {
        top: LIST_TOP_MARGIN,
        right: 0.0,
        bottom: theme::SPACE_MD,
        left: 0.0,
    }))
    .id(app::QUEUE_SCROLL_ID)
    .style(theme::thin_scrollbar)
    .width(Length::Fill)
    .height(Length::Fill)
    .on_scroll(|viewport: scrollable::Viewport| {
        qmsg(Msg::Scrolled {
            offset_y: viewport.absolute_offset().y,
            viewport_height: viewport.bounds().height,
            content_height: viewport.content_bounds().height,
        })
    })
    .into()
}

struct RowVisual {
    active: bool,
    generated: bool,
    hovered: bool,
    /// The internal drag source row ("lifted" look).
    lifted: bool,
    /// Sibling rows while a drag or this row's context menu is active.
    dimmed: bool,
    /// The row whose context menu is open.
    focused: bool,
}

fn queue_row<'a>(
    app: &'a App,
    index: usize,
    track: &'a Track,
    visual: RowVisual,
) -> Element<'a, Message> {
    let (title_color, artist_color) = if visual.dimmed {
        (theme::TEXT_FAINT, theme::TEXT_GHOST)
    } else if visual.active {
        (theme::ACCENT_FG, theme::TEXT_DIM)
    } else if visual.generated {
        (theme::TEXT, theme::TEXT_FAINT)
    } else {
        (theme::TEXT_BRIGHT, theme::TEXT_DIM)
    };

    let art = thumbnails::view_thumb(
        app,
        track.thumbnail_url.as_deref(),
        40.0,
        track.title.chars().next(),
    );
    let info = column![
        text(track.title.as_str())
            .size(theme::SIZE_BODY)
            .color(title_color)
            .wrapping(text::Wrapping::None),
        text(artist_string(track))
            .size(theme::SIZE_CAPTION)
            .color(artist_color)
            .wrapping(text::Wrapping::None),
    ]
    .spacing(theme::SPACE_XXS)
    .width(Length::Fill);

    let mut body = row![art, info]
        .spacing(theme::SPACE_MD)
        .align_y(Alignment::Center)
        .width(Length::Fill);
    if let Some(duration) = track.duration.as_deref() {
        body = body.push(
            text(duration)
                .font(Font::MONOSPACE)
                .size(theme::SIZE_CAPTION)
                .color(if visual.dimmed {
                    theme::TEXT_GHOST
                } else {
                    theme::TEXT_FAINT
                }),
        );
    }

    let inner = container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding([theme::SPACE_XS, theme::SPACE_MD])
        .style(queue_row_style(&visual))
        .clip(true);

    let shell = container(inner)
        .width(Length::Fill)
        .height(Length::Fixed(QUEUE_ROW_HEIGHT))
        .padding([theme::SPACE_XXS, theme::SPACE_MD]);

    mouse_area(shell)
        .interaction(iced::mouse::Interaction::Pointer)
        .on_press(qmsg(Msg::DragStarted {
            payload: QueueDragPayload::QueueRow { index },
            position: app.queue_panel.panel_pointer,
        }))
        .on_release(qmsg(Msg::RowClicked(index)))
        .on_right_press(mmsg(MenuMsg::Open {
            track: track.clone(),
            queue_index: Some(index),
            anchor: app.queue_panel.panel_pointer,
        }))
        .on_enter(qmsg(Msg::RowHovered(Some(index))))
        .on_exit(qmsg(Msg::RowHovered(None)))
        .into()
}

fn queue_row_style(visual: &RowVisual) -> impl Fn(&Theme) -> container::Style + use<> {
    let (background, border_color) = if visual.lifted {
        (Some(theme::ACCENT_BG_FAINT), theme::ACCENT_BORDER)
    } else if visual.focused {
        (Some(theme::white(0.08)), Color::TRANSPARENT)
    } else if visual.active {
        (Some(theme::white(0.08)), Color::TRANSPARENT)
    } else if visual.hovered {
        (Some(theme::white(0.06)), Color::TRANSPARENT)
    } else if visual.generated {
        (
            Some(Color {
                a: 0.04,
                ..theme::ACCENT
            }),
            Color::TRANSPARENT,
        )
    } else {
        (None, Color::TRANSPARENT)
    };
    move |_t| container::Style {
        background: background.map(iced::Background::Color),
        border: iced::Border {
            color: border_color,
            width: if border_color == Color::TRANSPARENT {
                0.0
            } else {
                1.0
            },
            radius: theme::RADIUS_XS.into(),
        },
        ..container::Style::default()
    }
}

/// "Generated from this radio" boundary divider (queue_drag spec §3.3).
fn radio_divider() -> Element<'static, Message> {
    container(
        column![
            container(Space::new().width(Length::Fill).height(Length::Fixed(1.0))).style(
                |_t: &Theme| container::Style {
                    background: Some(iced::Background::Color(Color {
                        a: 0.16,
                        ..theme::ACCENT
                    })),
                    ..container::Style::default()
                }
            ),
            container(
                text("GENERATED FROM THIS RADIO")
                    .size(theme::SIZE_MICRO)
                    .font(BOLD)
                    .color(theme::ACCENT_TEXT_DIM),
            )
            .padding(Padding {
                top: theme::SPACE_MD,
                right: 0.0,
                bottom: 0.0,
                left: 0.0,
            }),
        ]
        .width(Length::Fill),
    )
    .height(Length::Fixed(DIVIDER_HEIGHT))
    .padding(Padding {
        top: theme::SPACE_LG,
        right: theme::SPACE_LG,
        bottom: theme::SPACE_XS,
        left: theme::SPACE_LG,
    })
    .into()
}

/// Inline ghost row at the previewed insertion slot (CONTRACT.md §7.4).
fn drop_indicator_row(app: &App) -> Element<'_, Message> {
    let drag = app.queue_panel.drag.as_ref();
    let preview = drag.and_then(|drag| queue_preview_data_for_payload(app, &drag.payload));

    let body: Element<'_, Message> = match preview {
        Some(preview) => {
            let art = thumbnails::view_thumb(
                app,
                drag.and_then(|drag| payload_thumbnail_url(app, &drag.payload)),
                40.0,
                preview.title.chars().next(),
            );
            let info = column![
                text(preview.title.clone())
                    .size(theme::SIZE_BODY)
                    .color(theme::TEXT_BRIGHT)
                    .wrapping(text::Wrapping::None),
                text(preview.subtitle.clone())
                    .size(theme::SIZE_CAPTION)
                    .color(theme::TEXT_DIM)
                    .wrapping(text::Wrapping::None),
            ]
            .spacing(theme::SPACE_XXS)
            .width(Length::Fill);
            let mut body = row![art, info]
                .spacing(theme::SPACE_MD)
                .align_y(Alignment::Center)
                .width(Length::Fill);
            if let Some(duration) = preview.duration {
                body = body.push(
                    text(duration)
                        .font(Font::MONOSPACE)
                        .size(theme::SIZE_CAPTION)
                        .color(theme::TEXT_FAINT),
                );
            }
            body.into()
        }
        None => text("Drop into queue")
            .size(theme::SIZE_BODY)
            .color(theme::ACCENT_FG)
            .into(),
    };

    container(
        container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([theme::SPACE_XS, theme::SPACE_MD])
            .style(theme::drop_indicator)
            .clip(true),
    )
    .width(Length::Fill)
    .height(Length::Fixed(QUEUE_ROW_HEIGHT))
    .padding([theme::SPACE_XXS, theme::SPACE_MD])
    .into()
}

// ---------------------------------------------------------------------------
// Lyrics page (panels spec §2.4–2.7)
// ---------------------------------------------------------------------------

fn lyrics_page(app: &App) -> Element<'_, Message> {
    let lyrics = &app.queue_panel.lyrics;
    let (meta, status): (Option<String>, Option<String>) = match &lyrics.status {
        LyricsStatus::Idle => (None, Some(LYRICS_IDLE.to_owned())),
        LyricsStatus::Loading => (
            Some("Syncing lyrics".to_owned()),
            Some(LYRICS_LOADING.to_owned()),
        ),
        LyricsStatus::Unavailable(message) => (
            Some("Synced lyrics unavailable".to_owned()),
            Some(message.clone()),
        ),
        LyricsStatus::Error(message) => (
            Some("Lyrics fetch failed".to_owned()),
            Some(message.clone()),
        ),
        LyricsStatus::NotConnected => (
            Some("Not connected".to_owned()),
            Some(LYRICS_NOT_CONNECTED.to_owned()),
        ),
        LyricsStatus::Loaded => (lyrics.meta.clone(), None),
    };

    let mut page = column![].width(Length::Fill).height(Length::Fill);
    if let Some(meta) = meta.filter(|meta| !meta.trim().is_empty()) {
        page = page.push(
            container(
                text(meta.to_uppercase())
                    .size(theme::SIZE_CAPTION)
                    .font(BOLD)
                    .color(theme::ACCENT_TEXT_DIM),
            )
            .padding(Padding {
                top: theme::SPACE_14,
                right: theme::SPACE_XL,
                bottom: theme::SPACE_MD,
                left: theme::SPACE_XL,
            }),
        );
    }

    if let Some(status) = status {
        return page
            .push(
                container(text(status).size(theme::SIZE_BODY).color(theme::TEXT_DIM)).padding(
                    Padding {
                        top: theme::SPACE_14,
                        right: theme::SPACE_XL,
                        bottom: theme::SPACE_XL,
                        left: theme::SPACE_XL,
                    },
                ),
            )
            .into();
    }

    let lines = column(lyrics.lines.iter().enumerate().map(|(index, line)| {
        let active = lyrics.active_line == Some(index);
        text(line.text.as_str())
            .size(theme::SIZE_HEADER)
            .font(SEMIBOLD)
            .color(if active {
                theme::TEXT_BRIGHT
            } else {
                theme::TEXT_FAINT
            })
            .into()
    }))
    .spacing(theme::SPACE_MD)
    .width(Length::Fill)
    .padding(Padding {
        top: theme::SPACE_14,
        right: theme::SPACE_18,
        bottom: theme::SPACE_18,
        left: theme::SPACE_XL,
    });

    page.push(
        scrollable(lines)
            .id(app::LYRICS_SCROLL_ID)
            .style(theme::thin_scrollbar)
            .width(Length::Fill)
            .height(Length::Fill)
            .on_scroll(|viewport: scrollable::Viewport| {
                qmsg(Msg::LyricsScrolled {
                    offset_y: viewport.absolute_offset().y,
                    viewport_height: viewport.bounds().height,
                })
            }),
    )
    .into()
}

fn synced_lyrics_meta_text(lyrics: &SyncedLyrics) -> String {
    let source = match lyrics.source {
        crate::models::SyncedLyricsSource::TimedLyrics => "Timed lyrics",
        crate::models::SyncedLyricsSource::Captions => "Synced captions",
        crate::models::SyncedLyricsSource::AutoCaptions => "Auto-synced captions",
    };
    let mut parts = vec![source.to_owned()];
    if let Some(name) = lyrics
        .track_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        parts.push(name.to_owned());
    }
    if let Some(lang) = lyrics
        .language_code
        .as_deref()
        .map(str::trim)
        .filter(|lang| !lang.is_empty())
    {
        parts.push(lang.to_owned());
    }
    parts.join("  •  ")
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

pub fn update(app: &mut App, msg: Msg) -> Task<Message> {
    match msg {
        Msg::TabSelected(view) => {
            app.queue_panel.mode = view;
            if view == DetailPanelView::Lyrics {
                // GTK set_detail_panel_view: leaving Queue cancels drop visuals.
                app.queue_panel.drop_preview_slot = None;
                app.queue_panel.drag_hover_y = None;
                app.queue_panel.drop_active = false;
                // Bring the highlighted line back into the follow band.
                return scroll_lyrics_to_active(app);
            }
            Task::none()
        }

        Msg::DismissQueuePressed => dismiss_queue_after_current_task(app),

        Msg::RowClicked(index) => {
            if app
                .queue_panel
                .drag
                .as_ref()
                .is_some_and(|drag| drag.active)
            {
                // An active drag releases through PointerReleased (the drop).
                return Task::none();
            }
            let pending = app.queue_panel.drag.take();
            clear_drop_visuals(app);
            match pending {
                Some(DragState {
                    payload: QueueDragPayload::QueueRow { index: pressed },
                    ..
                }) if pressed == index => play_from_queue(app, index),
                _ => Task::none(), // release slid in from elsewhere
            }
        }

        Msg::RowHovered(index) => {
            app.queue_panel.hovered_row = index;
            Task::none()
        }

        Msg::ChipClicked(chip_index) => {
            queue_op(app, move |daemon| daemon.activate_radio_chip(chip_index))
        }

        Msg::JumpToSeedPressed => {
            let Some(seed_video_id) = app
                .player
                .radio_virtual_playlist
                .as_ref()
                .map(|vp| vp.source_video_id.clone())
            else {
                return Task::none();
            };
            let index = app
                .player
                .queue
                .iter()
                .position(|track| track.video_id == seed_video_id)
                .unwrap_or(app.player.queue_index);
            let viewport = effective_viewport_height(&app.queue_panel);
            let y = (LIST_TOP_MARGIN + index as f32 * QUEUE_ROW_HEIGHT
                - (viewport - QUEUE_ROW_HEIGHT) / 2.0)
                .max(0.0);
            iced::widget::operation::scroll_to(
                app::QUEUE_SCROLL_ID,
                scrollable::AbsoluteOffset {
                    x: None,
                    y: Some(y),
                },
            )
        }

        Msg::DragStarted { payload, position } => {
            let mut tasks = Vec::new();
            if let Some(url) = queue_preview_data_for_payload(app, &payload)
                .and_then(|preview| preview.thumbnail_url)
            {
                tasks.push(thumbnails::ensure(app, &url));
            }
            app.queue_panel.drag = Some(DragState {
                payload,
                origin: position,
                position,
                active: false,
            });
            app.queue_panel.drop_preview_slot = None;
            app.queue_panel.drag_hover_y = None;
            Task::batch(tasks)
        }

        Msg::PointerMoved(position) => {
            app.queue_panel.panel_pointer = position;
            let list_top = list_top(app);
            let scroll_offset = app.queue_panel.scroll_offset;
            let source_index = drag_source_index(&app.queue_panel);
            let queue_len = app.player.queue.len();
            let tail = radio_tail_start(app);
            let mode = app.queue_panel.mode;

            let Some(drag) = app.queue_panel.drag.as_mut() else {
                return Task::none();
            };
            drag.position = position;
            if !matches!(drag.payload, QueueDragPayload::QueueRow { .. })
                && mode != DetailPanelView::Queue
            {
                // GTK queue_accepts_external_dnd: while the Lyrics tab is
                // shown the panel refuses external payloads — no activation,
                // no tint, no slot tracking over the invisible queue list.
                app.queue_panel.drag_hover_y = None;
                app.queue_panel.drop_preview_slot = None;
                app.queue_panel.drop_active = false;
                return Task::none();
            }
            if !drag.active && position.distance(drag.origin) >= DRAG_START_THRESHOLD {
                drag.active = true;
            }
            if !drag.active {
                return Task::none();
            }

            app.queue_panel.drop_active = true;
            let hover_y = position.y - list_top;
            if hover_y >= 0.0 {
                app.queue_panel.drag_hover_y = Some(hover_y);
                let content_y = hover_y + scroll_offset;
                let raw_slot = slot_for_content_y_with(tail, queue_len, content_y);
                app.queue_panel.drop_preview_slot =
                    normalized_preview_slot(raw_slot, source_index, queue_len);
            } else {
                // Over the header/chip bar: no slot, no autoscroll.
                app.queue_panel.drag_hover_y = None;
                app.queue_panel.drop_preview_slot = None;
            }
            Task::none()
        }

        Msg::PointerReleased => {
            let drag = app.queue_panel.drag.take();
            let hover_y = app.queue_panel.drag_hover_y.take();
            let scroll_offset = app.queue_panel.scroll_offset;
            clear_drop_visuals(app);
            let Some(drag) = drag else {
                return Task::none();
            };
            if !drag.active {
                return Task::none(); // plain click, resolved by the row
            }
            if !matches!(drag.payload, QueueDragPayload::QueueRow { .. })
                && app.queue_panel.mode != DetailPanelView::Queue
            {
                // GTK queue_accepts_external_dnd: external drops are refused
                // while the Lyrics tab is shown (cancel, never an invisible
                // insert into the hidden queue list).
                return Task::none();
            }
            let Some(hover_y) = hover_y else {
                return Task::none(); // released outside the list: cancel
            };
            let raw_slot = slot_for_content_y_with(
                radio_tail_start(app),
                app.player.queue.len(),
                hover_y + scroll_offset,
            );
            handle_drop_payload_task(app, drag.payload, raw_slot)
        }

        Msg::PointerExited => {
            // GTK DropTarget leave: stop autoscroll + clear preview/tint; the
            // pending drag itself survives (the pointer may come back).
            app.queue_panel.drag_hover_y = None;
            app.queue_panel.drop_preview_slot = None;
            app.queue_panel.drop_active = false;
            app.queue_panel.hovered_row = None;
            Task::none()
        }

        Msg::DragAborted => {
            // Global release fallback (GTK drag_end parity): any left-button
            // release that no row/card/panel handler consumed ends the drag.
            app.queue_panel.drag = None;
            clear_drop_visuals(app);
            Task::none()
        }

        Msg::AutoscrollTick => {
            if !app
                .queue_panel
                .drag
                .as_ref()
                .is_some_and(|drag| drag.active)
            {
                return Task::none();
            }
            let Some(hover_y) = app.queue_panel.drag_hover_y else {
                return Task::none();
            };
            let viewport = effective_viewport_height(&app.queue_panel);
            let velocity = queue_drag_autoscroll_velocity(hover_y, viewport);
            if velocity.abs() <= f32::EPSILON {
                return Task::none();
            }

            let content_height = effective_content_height(app);
            let max_offset = (content_height - viewport).max(0.0);
            let target = (app.queue_panel.scroll_offset + velocity).clamp(0.0, max_offset);
            if (target - app.queue_panel.scroll_offset).abs() <= f32::EPSILON {
                return Task::none(); // pinned at a bound; keep ticking idly
            }
            app.queue_panel.scroll_offset = target;

            // Re-target the preview under the moving content (GTK §9.3 step 6).
            let raw_slot = slot_for_content_y_with(
                radio_tail_start(app),
                app.player.queue.len(),
                hover_y + target,
            );
            app.queue_panel.drop_preview_slot = normalized_preview_slot(
                raw_slot,
                drag_source_index(&app.queue_panel),
                app.player.queue.len(),
            );

            iced::widget::operation::scroll_by(
                app::QUEUE_SCROLL_ID,
                scrollable::AbsoluteOffset {
                    x: 0.0,
                    y: velocity,
                },
            )
        }

        Msg::Scrolled {
            offset_y,
            viewport_height,
            content_height,
        } => {
            app.queue_panel.scroll_offset = offset_y.max(0.0);
            app.queue_panel.viewport_height = viewport_height;
            app.queue_panel.content_height = content_height;
            Task::none()
        }

        Msg::LyricsScrolled {
            offset_y,
            viewport_height,
        } => {
            app.queue_panel.lyrics_scroll_offset = offset_y.max(0.0);
            app.queue_panel.lyrics_viewport_height = viewport_height;
            Task::none()
        }

        Msg::QueueOpFinished { serial, result } => {
            if serial != app.queue_panel.op_serial {
                return Task::none(); // superseded by a newer queue mutation
            }
            match result {
                Ok(snapshot) => {
                    if app.queue_panel.drag.is_none() {
                        // Render hygiene (queue_drag spec §12.17).
                        app.queue_panel.drop_preview_slot = None;
                    }
                    player_bar::apply_remote_snapshot(app, snapshot)
                }
                Err(err) => {
                    // GTK drops queue-mutation errors silently; keep a log.
                    crate::app_log!("[QUEUE] queue mutation failed: {err}");
                    Task::none()
                }
            }
        }

        Msg::PlayOpFinished { serial, result } => {
            app.player.clear_play_request_in_flight(serial);
            if !app.player.is_current_play_request(serial) {
                return Task::none();
            }
            match result {
                Ok(snapshot) => player_bar::apply_remote_snapshot(app, snapshot),
                Err(err) => {
                    surface_play_error(app, err);
                    Task::none()
                }
            }
        }

        Msg::DropCollectionResolved { slot, result } => match result {
            Ok(tracks) => insert_tracks_task(app, tracks, slot),
            Err(err) => {
                crate::app_log!("[QUEUE] failed to expand collection drop: {err}");
                Task::none()
            }
        },

        Msg::LyricsLoaded {
            serial,
            session_id,
            video_id,
            result,
        } => {
            if serial != app.queue_panel.lyrics.request_serial {
                return Task::none();
            }
            let current = app
                .player
                .now_playing
                .as_ref()
                .is_some_and(|np| np.session_id == session_id && np.video_id == video_id);
            if !current {
                return Task::none();
            }

            let lyrics_state = &mut app.queue_panel.lyrics;
            match result {
                Ok(Some(lyrics)) => {
                    lyrics_state.meta = Some(synced_lyrics_meta_text(&lyrics));
                    lyrics_state.lines = lyrics.lines;
                    lyrics_state.active_line = None;
                    lyrics_state.status = LyricsStatus::Loaded;
                    let position = app.player.snapshot.position_millis;
                    return sync_active_lyric_line(app, position);
                }
                Ok(None) => {
                    lyrics_state.lines.clear();
                    lyrics_state.meta = None;
                    lyrics_state.active_line = None;
                    lyrics_state.status = LyricsStatus::Unavailable(LYRICS_UNAVAILABLE.to_owned());
                }
                Err(err) => {
                    crate::app_log!("[LYRICS] failed to fetch synced lyrics for {video_id}: {err}");
                    lyrics_state.lines.clear();
                    lyrics_state.meta = None;
                    lyrics_state.active_line = None;
                    lyrics_state.status = LyricsStatus::Error(LYRICS_FAILED.to_owned());
                }
            }
            Task::none()
        }

        Msg::RadioTailLoaded { anchor, result } => {
            app.player.radio_append_in_flight = false;
            let tracks = match result {
                Ok(tracks) => tracks,
                Err(err) => {
                    crate::app_log!("[RADIO] tail refresh failed for {anchor}: {err}");
                    return Task::none();
                }
            };
            // Drop stale fetches racing a newer queue: the anchor must still
            // be the last requested one and radio mode must still be active.
            if app.player.last_radio_append_anchor.as_deref() != Some(anchor.as_str())
                || !app.player.is_radio_queue()
            {
                return Task::none();
            }
            let existing: std::collections::HashSet<&str> = app
                .player
                .queue
                .iter()
                .map(|track| track.video_id.as_str())
                .collect();
            let fresh: Vec<Track> = tracks
                .into_iter()
                .filter(|track| !existing.contains(track.video_id.as_str()))
                .collect();
            let append_at = app.player.queue.len();
            insert_tracks_task(app, fresh, append_at)
        }
    }
}

fn clear_drop_visuals(app: &mut App) {
    app.queue_panel.drop_preview_slot = None;
    app.queue_panel.drag_hover_y = None;
    app.queue_panel.drop_active = false;
}

fn drag_source_index(state: &State) -> Option<usize> {
    state.drag.as_ref().and_then(|drag| match drag.payload {
        QueueDragPayload::QueueRow { index } => Some(index),
        _ => None,
    })
}

/// y offset of the queue scroll viewport's top inside the panel.
fn list_top(app: &App) -> f32 {
    HEADER_HEIGHT
        + if has_radio_chips(app) {
            CHIP_BAR_HEIGHT
        } else {
            0.0
        }
}

fn effective_viewport_height(state: &State) -> f32 {
    if state.viewport_height > 0.0 {
        state.viewport_height
    } else {
        FALLBACK_VIEWPORT_HEIGHT
    }
}

fn effective_content_height(app: &App) -> f32 {
    if app.queue_panel.content_height > 0.0 {
        return app.queue_panel.content_height;
    }
    let mut height = LIST_TOP_MARGIN + app.player.queue.len() as f32 * QUEUE_ROW_HEIGHT;
    if radio_tail_start(app).is_some() {
        height += DIVIDER_HEIGHT;
    }
    if app.queue_panel.drop_preview_slot.is_some() {
        height += QUEUE_ROW_HEIGHT;
    }
    height
}

/// First generated-tail row index, when a radio tail exists (queue_actions
/// spec §4 step 5).
fn radio_tail_start(app: &App) -> Option<usize> {
    if !app.player.is_radio_queue() {
        return None;
    }
    let len = app.player.queue.len();
    let touched = app.player.radio_touched_len().min(len);
    (touched < len).then_some(touched)
}

/// Divider-aware slot math: the uniform-grid `preview_slot_for_queue_y` plus
/// the GTK rule that hovering the radio divider targets the boundary slot.
fn slot_for_content_y_with(tail_start: Option<usize>, row_count: usize, content_y: f32) -> usize {
    if let Some(tail_start) = tail_start {
        let divider_top = LIST_TOP_MARGIN + tail_start as f32 * QUEUE_ROW_HEIGHT;
        if content_y >= divider_top {
            if content_y < divider_top + DIVIDER_HEIGHT {
                return tail_start;
            }
            return preview_slot_for_queue_y(content_y - DIVIDER_HEIGHT, row_count);
        }
    }
    preview_slot_for_queue_y(content_y, row_count)
}

/// Borrowed thumbnail URL for a drag payload that lives in `App` state (the
/// drop indicator's art must not borrow from a temporary preview struct).
fn payload_thumbnail_url<'a>(app: &'a App, payload: &'a QueueDragPayload) -> Option<&'a str> {
    match payload {
        QueueDragPayload::QueueRow { index } => {
            app.player.queue.get(*index)?.thumbnail_url.as_deref()
        }
        QueueDragPayload::Track { track } => track.thumbnail_url.as_deref(),
        QueueDragPayload::Playlist { thumbnail_url, .. }
        | QueueDragPayload::BrowseCollection { thumbnail_url, .. } => thumbnail_url.as_deref(),
    }
}

struct QueuePreviewData {
    title: String,
    subtitle: String,
    thumbnail_url: Option<String>,
    duration: Option<String>,
}

fn queue_preview_data_for_payload(
    app: &App,
    payload: &QueueDragPayload,
) -> Option<QueuePreviewData> {
    match payload {
        QueueDragPayload::QueueRow { index } => {
            let track = app.player.queue.get(*index)?;
            Some(QueuePreviewData {
                title: track.title.clone(),
                subtitle: artist_string(track),
                thumbnail_url: track.thumbnail_url.clone(),
                duration: track.duration.clone(),
            })
        }
        QueueDragPayload::Track { track } => Some(QueuePreviewData {
            title: track.title.clone(),
            subtitle: artist_string(track),
            thumbnail_url: track.thumbnail_url.clone(),
            duration: track.duration.clone(),
        }),
        QueueDragPayload::Playlist {
            title,
            subtitle,
            thumbnail_url,
            ..
        } => Some(QueuePreviewData {
            title: title.clone(),
            subtitle: subtitle.clone().unwrap_or_else(|| "Playlist".to_owned()),
            thumbnail_url: thumbnail_url.clone(),
            duration: None,
        }),
        QueueDragPayload::BrowseCollection {
            title,
            subtitle,
            thumbnail_url,
            page_type,
            ..
        } => Some(QueuePreviewData {
            title: title.clone(),
            subtitle: subtitle.clone().unwrap_or_else(|| {
                match page_type {
                    HomeBrowsePageType::Album => "Album",
                    HomeBrowsePageType::Playlist => "Playlist",
                    HomeBrowsePageType::Artist => "Artist",
                    HomeBrowsePageType::UserChannel => "Channel",
                    HomeBrowsePageType::PodcastShow => "Show",
                }
                .to_owned()
            }),
            thumbnail_url: thumbnail_url.clone(),
            duration: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// Track context menu (queue_actions spec §8–9)
// ---------------------------------------------------------------------------

/// Context-menu overlay layer (only composed by the shell while
/// `app.track_menu.is_some()`).
pub fn context_menu_layer(app: &App) -> Element<'_, Message> {
    let Some(menu) = app.track_menu.as_ref() else {
        return Space::new().into();
    };

    let mut items: Vec<Element<'_, Message>> = vec![menu_header(app, menu)];

    if menu.queue_index.is_none() {
        items.push(menu_action(
            "Play next",
            Some(mmsg(MenuMsg::PlayNext)),
            false,
        ));
        items.push(menu_action(
            "Add to queue",
            Some(mmsg(MenuMsg::AddToQueue)),
            false,
        ));
    }
    if app.player.can_jump_to_radio() {
        items.push(menu_action(
            "Jump to",
            Some(mmsg(MenuMsg::StartRadio)),
            false,
        ));
    }
    items.push(menu_action("Probe", Some(mmsg(MenuMsg::Probe)), false));

    match menu.feedback {
        FeedbackPhase::Hidden | FeedbackPhase::Done => {}
        FeedbackPhase::NotInterestedAvailable => {
            items.push(menu_action(
                "Not interested",
                Some(mmsg(MenuMsg::NotInterested)),
                false,
            ));
        }
        FeedbackPhase::NotInterestedInFlight => {
            items.push(menu_action("Not interested", None, false));
        }
        FeedbackPhase::UndoAvailable => {
            items.push(menu_action(
                "Undo",
                Some(mmsg(MenuMsg::NotInterested)),
                false,
            ));
        }
        FeedbackPhase::UndoInFlight => {
            items.push(menu_action("Undo", None, false));
        }
    }

    match menu.pinned_offline {
        None => items.push(menu_action("Download", None, false)),
        Some(true) => items.push(menu_action(
            "Delete from offline",
            Some(mmsg(MenuMsg::DeleteOffline)),
            true,
        )),
        Some(false) => items.push(menu_action(
            "Download",
            Some(mmsg(MenuMsg::Download { pin: true })),
            false,
        )),
    }

    items.push(menu_action("Share", Some(mmsg(MenuMsg::Share)), false));

    if let Some(index) = menu.queue_index {
        if index != app.player.queue_index {
            items.push(menu_action(
                "Remove from queue",
                Some(mmsg(MenuMsg::RemoveFromQueue)),
                true,
            ));
        }
    }

    let card = container(column(items).spacing(theme::SPACE_XXS).width(Length::Fill))
        .width(Length::Fixed(280.0))
        .padding(theme::SPACE_SM)
        .style(theme::context_menu);

    mouse_area(
        container(card)
            .center(Length::Fill)
            .style(theme::content_dim_overlay),
    )
    .on_press(mmsg(MenuMsg::Dismiss))
    .on_right_press(mmsg(MenuMsg::Dismiss))
    .into()
}

fn menu_header<'a>(app: &'a App, menu: &'a TrackMenu) -> Element<'a, Message> {
    let art = thumbnails::view_thumb(
        app,
        menu.track.thumbnail_url.as_deref(),
        40.0,
        menu.track.title.chars().next(),
    );
    container(
        row![
            art,
            column![
                text(menu.track.title.as_str())
                    .size(theme::SIZE_BODY)
                    .color(theme::TEXT_BRIGHT)
                    .wrapping(text::Wrapping::None),
                text(artist_string(&menu.track))
                    .size(theme::SIZE_CAPTION)
                    .color(theme::TEXT_DIM)
                    .wrapping(text::Wrapping::None),
            ]
            .spacing(theme::SPACE_XXS)
            .width(Length::Fill),
        ]
        .spacing(theme::SPACE_MD)
        .align_y(Alignment::Center)
        .width(Length::Fill),
    )
    .padding(Padding {
        top: theme::SPACE_SM,
        right: theme::SPACE_MD,
        bottom: theme::SPACE_MD,
        left: theme::SPACE_MD,
    })
    .clip(true)
    .into()
}

fn menu_action(
    label: &'static str,
    on_press: Option<Message>,
    danger: bool,
) -> Element<'static, Message> {
    let enabled = on_press.is_some();
    let color = if !enabled {
        theme::TEXT_GHOST
    } else if danger {
        theme::TEXT_DANGER
    } else {
        theme::TEXT
    };
    let mut btn = button(text(label).size(theme::SIZE_BODY).color(color))
        .style(theme::list_row(false))
        .width(Length::Fill)
        .padding([theme::SPACE_MD, theme::SPACE_XL]);
    if let Some(message) = on_press {
        btn = btn.on_press(message);
    }
    btn.into()
}

pub fn update_menu(app: &mut App, msg: MenuMsg) -> Task<Message> {
    match msg {
        MenuMsg::Open {
            track,
            queue_index,
            anchor,
        } => {
            let feedback = if track.not_interested_feedback_token.is_some() {
                FeedbackPhase::NotInterestedAvailable
            } else {
                FeedbackPhase::Hidden
            };
            let video_id = track.video_id.clone();
            app.track_menu = Some(TrackMenu {
                track,
                queue_index,
                anchor,
                feedback,
                pinned_offline: None,
            });

            let cache = app.cache.clone();
            let probe_video_id = video_id.clone();
            app::blocking(
                move || {
                    cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_pinned_offline(&probe_video_id)
                },
                move |pinned| {
                    mmsg(MenuMsg::PinnedProbed {
                        video_id: video_id.clone(),
                        pinned,
                    })
                },
            )
        }

        MenuMsg::Dismiss => {
            app.track_menu = None;
            Task::none()
        }

        MenuMsg::PlayNext => {
            let Some(menu) = app.track_menu.take() else {
                return Task::none();
            };
            queue_play_next_task(app, menu.track)
        }

        MenuMsg::AddToQueue => {
            let Some(menu) = app.track_menu.take() else {
                return Task::none();
            };
            queue_add_to_end_task(app, menu.track)
        }

        MenuMsg::RemoveFromQueue => {
            let Some(menu) = app.track_menu.take() else {
                return Task::none();
            };
            let Some(index) = menu.queue_index else {
                return Task::none();
            };
            if index == app.player.queue_index {
                return Task::none(); // current-row protection
            }
            queue_remove_task(app, index)
        }

        MenuMsg::StartRadio => {
            let Some(menu) = app.track_menu.take() else {
                return Task::none();
            };
            match menu.queue_index {
                Some(index) => jump_to_radio_seed(app, index),
                None => jump_to_radio_seed_from_track(app, menu.track),
            }
        }

        MenuMsg::Probe => {
            let Some(menu) = app.track_menu.take() else {
                return Task::none();
            };
            probe_track_task(app, menu.track)
        }

        MenuMsg::Download { pin } => {
            let Some(menu) = app.track_menu.take() else {
                return Task::none();
            };
            let client = app.app_client();
            let cache = app.cache.clone();
            let track = menu.track;
            let video_id = track.video_id.clone();
            app::blocking(
                move || {
                    if ensure_track_available_offline(
                        client.as_deref(),
                        &cache,
                        &track,
                        pin,
                        "DOWNLOAD",
                    ) {
                        crate::app_log!(
                            "[DOWNLOAD] pinned {} for offline playback",
                            track.video_id
                        );
                        Ok(())
                    } else {
                        Err("download failed".to_owned())
                    }
                },
                move |result| {
                    mmsg(MenuMsg::DownloadFinished {
                        video_id: video_id.clone(),
                        now_pinned: true,
                        result,
                    })
                },
            )
        }

        MenuMsg::DeleteOffline => {
            let Some(menu) = app.track_menu.take() else {
                return Task::none();
            };
            let cache = app.cache.clone();
            let video_id = menu.track.video_id.clone();
            let work_video_id = video_id.clone();
            app::blocking(
                move || {
                    cache
                        .lock()
                        .unwrap()
                        .remove_cached(&work_video_id)
                        .map_err(|err| err.to_string())
                },
                move |result| {
                    mmsg(MenuMsg::DownloadFinished {
                        video_id: video_id.clone(),
                        now_pinned: false,
                        result,
                    })
                },
            )
        }

        MenuMsg::Share => {
            let Some(menu) = app.track_menu.take() else {
                return Task::none();
            };
            let url = youtube_music_watch_url(&menu.track.video_id);
            Task::batch([iced::clipboard::write(url), app.show_toast("Copied!")])
        }

        MenuMsg::NotInterested => {
            let Some(menu) = app.track_menu.as_mut() else {
                return Task::none();
            };
            let (token, undo) = match menu.feedback {
                FeedbackPhase::NotInterestedAvailable => {
                    (menu.track.not_interested_feedback_token.clone(), false)
                }
                FeedbackPhase::UndoAvailable => (menu.track.undo_feedback_token.clone(), true),
                _ => return Task::none(), // hidden / already in flight
            };
            let Some(token) = token else {
                return Task::none();
            };

            menu.feedback = if undo {
                FeedbackPhase::UndoInFlight
            } else {
                FeedbackPhase::NotInterestedInFlight
            };
            let video_id = menu.track.video_id.clone();
            let client = app.app_client();
            app::blocking(
                move || match client.as_deref() {
                    Some(ytm) => ytm
                        .submit_feedback_token(&token)
                        .map_err(|err| err.to_string()),
                    None => Err("YouTube Music is not connected".to_owned()),
                },
                move |result| {
                    mmsg(MenuMsg::FeedbackSubmitted {
                        undo,
                        video_id: video_id.clone(),
                        result,
                    })
                },
            )
        }

        MenuMsg::PinnedProbed { video_id, pinned } => {
            if let Some(menu) = app.track_menu.as_mut() {
                if menu.track.video_id == video_id {
                    menu.pinned_offline = Some(pinned);
                }
            }
            Task::none()
        }

        MenuMsg::FeedbackSubmitted {
            undo,
            video_id,
            result,
        } => {
            let Some(menu) = app
                .track_menu
                .as_mut()
                .filter(|menu| menu.track.video_id == video_id)
            else {
                return Task::none();
            };
            match result {
                Ok(()) => {
                    menu.feedback = if undo {
                        FeedbackPhase::NotInterestedAvailable
                    } else if menu.track.undo_feedback_token.is_some() {
                        FeedbackPhase::UndoAvailable
                    } else {
                        FeedbackPhase::Done
                    };
                }
                Err(err) => {
                    if undo {
                        crate::app_log!("[FEEDBACK] failed to undo not interested: {err}");
                        menu.feedback = FeedbackPhase::UndoAvailable;
                    } else {
                        crate::app_log!("[FEEDBACK] failed to mark track as not interested: {err}");
                        menu.feedback = FeedbackPhase::NotInterestedAvailable;
                    }
                }
            }
            Task::none()
        }

        MenuMsg::DownloadFinished {
            video_id,
            now_pinned,
            result,
        } => {
            match &result {
                Ok(()) if now_pinned => {
                    crate::app_log!("[DOWNLOAD] {video_id} is available offline")
                }
                Ok(()) => crate::app_log!("[DOWNLOAD] removed {video_id} from offline cache"),
                Err(err) => {
                    crate::app_log!("[DOWNLOAD] offline action failed for {video_id}: {err}")
                }
            }
            if result.is_ok() {
                if let Some(menu) = app.track_menu.as_mut() {
                    if menu.track.video_id == video_id {
                        menu.pinned_offline = Some(now_pinned);
                    }
                }
            }
            Task::none()
        }
    }
}

// ---------------------------------------------------------------------------
// Queue mutation task builders (queue_actions spec §5; exact daemon calls).
// Shared by sidebar/views — call these instead of talking to the daemon.
// ---------------------------------------------------------------------------

/// "Play now" from a full track — seeds radio (`start_radio_from_seed`).
pub fn play_track_radio_task(app: &mut App, seed: Track) -> Task<Message> {
    let video_id = seed.video_id.clone();
    let title = seed.title.clone();
    let artist = artist_string(&seed);
    let thumbnail_url = seed.thumbnail_url.clone();

    let flush =
        flush_now_playing_eagerly(app, &video_id, &title, &artist, thumbnail_url.as_deref());
    let repeat_mode = app.player.repeat_mode;
    Task::batch([
        flush,
        play_op(app, move |daemon| {
            daemon.start_radio_from_seed(vec![seed], 0, 0, 1, None, repeat_mode, true)
        }),
    ])
}

/// "Play now" from a bare id — seeds radio (`start_radio_from_seed`).
pub fn play_track_task(
    app: &mut App,
    video_id: String,
    title: String,
    artist: String,
    thumbnail_url: Option<String>,
) -> Task<Message> {
    let seed = Track {
        video_id: video_id.clone(),
        title: title.clone(),
        artists: vec![Artist {
            name: artist.clone(),
            id: None,
        }],
        album: None,
        duration: None,
        thumbnail_url: thumbnail_url.clone(),
        playback_playlist_id: None,
        playback_params: None,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
    };

    play_track_radio_task(app, seed)
}

/// Replace queue with `tracks` at `index` under an optional playlist context
/// (`play_queue_at`). GTK `play_from_context` always passed
/// `playlist_like_queue = false`; only `play_all` derived it from the context.
pub fn play_from_context_task(
    app: &mut App,
    tracks: Vec<Track>,
    index: usize,
    playlist_context: Option<PlaylistContext>,
) -> Task<Message> {
    play_from_context_with_flag(app, tracks, index, playlist_context, false)
}

pub fn play_all_task(
    app: &mut App,
    tracks: Vec<Track>,
    shuffle: bool,
    playlist_context: Option<PlaylistContext>,
) -> Task<Message> {
    if tracks.is_empty() {
        return Task::none();
    }
    let mut queue = tracks;
    if shuffle {
        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        queue.shuffle(&mut rng);
    }
    // GTK `play_all`: a contextless Play All marks the queue playlist-like.
    let playlist_like_queue = playlist_context.is_none();
    play_from_context_with_flag(app, queue, 0, playlist_context, playlist_like_queue)
}

fn play_from_context_with_flag(
    app: &mut App,
    tracks: Vec<Track>,
    index: usize,
    playlist_context: Option<PlaylistContext>,
    playlist_like_queue: bool,
) -> Task<Message> {
    if tracks.is_empty() || index >= tracks.len() {
        return Task::none();
    }
    app.player.queue = tracks;
    app.player.queue_index = index;
    app.player
        .set_queue_context(playlist_context, None, playlist_like_queue);
    play_from_queue(app, index)
}

/// Temporary listen without queue mutation (`probe_track`).
pub fn probe_track_task(app: &mut App, track: Track) -> Task<Message> {
    let title = track.title.clone();
    let artist = artist_string(&track);
    let thumbnail_url = track.thumbnail_url.clone();
    let flush = flush_now_playing_eagerly(
        app,
        &track.video_id,
        &title,
        &artist,
        thumbnail_url.as_deref(),
    );
    Task::batch([flush, play_op(app, move |daemon| daemon.probe_track(track))])
}

pub fn queue_play_next_task(app: &mut App, track: Track) -> Task<Message> {
    let insert_at = app.player.queue_index.saturating_add(1);
    insert_tracks_task(app, vec![track], insert_at)
}

/// Radio boundary rule: in radio mode "add to queue" appends at the end of the
/// touched prefix (just before the generated tail), not the true queue end.
pub fn queue_add_to_end_task(app: &mut App, track: Track) -> Task<Message> {
    let queue_len = app.player.queue.len();
    let insert_at = if app.player.is_radio_queue() {
        app.player.radio_touched_len().min(queue_len)
    } else {
        queue_len
    };
    insert_tracks_task(app, vec![track], insert_at)
}

pub fn queue_remove_task(app: &mut App, index: usize) -> Task<Message> {
    queue_op(app, move |daemon| daemon.remove_from_queue(index))
}

pub fn dismiss_queue_after_current_task(app: &mut App) -> Task<Message> {
    queue_op(app, |daemon| daemon.dismiss_queue_after_current())
}

/// Handle a dropped payload at a raw slot (move/insert/resolve collection).
pub fn handle_drop_payload_task(
    app: &mut App,
    payload: QueueDragPayload,
    raw_slot: usize,
) -> Task<Message> {
    let queue_len = app.player.queue.len();
    match payload {
        QueueDragPayload::QueueRow { index: from } => {
            let insert_at = if from < raw_slot {
                raw_slot.saturating_sub(1)
            } else {
                raw_slot
            };
            if insert_at == from || from >= queue_len || raw_slot > queue_len {
                return Task::none(); // no-op / stale drop
            }
            // The RAW slot travels to the daemon; it re-normalizes and adjusts
            // the radio touched boundary (queue_drag spec §7.4).
            queue_op(app, move |daemon| daemon.move_queue_item(from, raw_slot))
        }
        QueueDragPayload::Track { track } => insert_tracks_task(app, vec![track], raw_slot),
        payload @ (QueueDragPayload::Playlist { .. }
        | QueueDragPayload::BrowseCollection { .. }) => {
            let client = app.app_client();
            let cache = app.cache.clone();
            let settings = app.settings.clone();
            app::blocking(
                move || {
                    resolve_dragged_collection_tracks(
                        &payload,
                        client.as_deref(),
                        &cache,
                        &settings,
                    )
                },
                move |result| {
                    qmsg(Msg::DropCollectionResolved {
                        slot: raw_slot,
                        result,
                    })
                },
            )
        }
    }
}

pub fn download_all_task(
    app: &App,
    tracks: Vec<Track>,
    meta: Option<DownloadPlaylistMeta>,
) -> Task<Message> {
    if tracks.is_empty() {
        return Task::none();
    }
    let client = app.app_client();
    let cache = app.cache.clone();
    app::blocking(
        move || {
            let count = tracks.len();
            crate::app_log!("[DOWNLOAD] queuing {count} tracks for offline download");
            for (i, track) in tracks.iter().enumerate() {
                let vid = &track.video_id;
                crate::app_log!("[DOWNLOAD] {}/{count} downloading {vid}...", i + 1);
                if ensure_track_available_offline(
                    client.as_deref(),
                    &cache,
                    track,
                    true,
                    "DOWNLOAD",
                ) {
                    crate::app_log!("[DOWNLOAD] {}/{count} ready: {vid}", i + 1);
                }
            }

            if let Some(meta) = &meta {
                // Cache the cover to disk for offline display, but persist the
                // remote URL in the DB (not a `file://` path) so it stays
                // recoverable if the file is later evicted — and matches the
                // cover the Playlists view shows. The offline page prefers the
                // local file when present, else this remote URL.
                if let Some(url) = meta.thumbnail_url.as_deref() {
                    if let Some(bytes) = crate::network::fetch_thumbnail_bytes(url) {
                        let path = cache
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .offline_playlist_cover_path(&meta.playlist_id);
                        let _ = std::fs::write(&path, &bytes);
                    }
                }

                let video_ids: Vec<String> =
                    tracks.iter().map(|track| track.video_id.clone()).collect();
                let guard = cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Err(err) = guard.store_offline_playlist(
                    &meta.playlist_id,
                    &meta.title,
                    meta.thumbnail_url.as_deref(),
                    meta.subtitle.as_deref(),
                    &video_ids,
                ) {
                    crate::app_log!("[DOWNLOAD] failed to store playlist meta: {err}");
                } else {
                    crate::app_log!(
                        "[DOWNLOAD] stored playlist meta: {} ({} tracks)",
                        meta.title,
                        video_ids.len()
                    );
                }
            }

            crate::app_log!("[DOWNLOAD] finished all {count} tracks");
            meta.map(|meta| meta.playlist_id)
        },
        |playlist_id| match playlist_id {
            Some(playlist_id) => {
                Message::Library(super::views::library::Msg::DownloadStateChecked {
                    playlist_id,
                    downloaded: true,
                })
            }
            None => Message::Noop,
        },
    )
}

// ---------------------------------------------------------------------------
// Private play/queue plumbing
// ---------------------------------------------------------------------------

/// GTK `play_from_queue`: eager queue index + radio promotion + now-playing
/// flush, then `play_queue_at` with the touched-len-updated local mirrors.
fn play_from_queue(app: &mut App, index: usize) -> Task<Message> {
    let Some(track) = app.player.queue.get(index).cloned() else {
        return Task::none();
    };
    app.player.queue_index = index;
    app.player.mark_radio_touched_through(index);

    let title = track.title.clone();
    let artist = artist_string(&track);
    let flush = flush_now_playing_eagerly(
        app,
        &track.video_id,
        &title,
        &artist,
        track.thumbnail_url.as_deref(),
    );

    let queue = app.player.queue.clone();
    let playlist_context = app.player.playlist_context.clone();
    let radio_virtual_playlist = app.player.radio_virtual_playlist.clone();
    let repeat_mode = app.player.repeat_mode;
    Task::batch([
        flush,
        play_op(app, move |daemon| {
            daemon.play_queue_at(
                queue,
                index,
                playlist_context,
                radio_virtual_playlist,
                repeat_mode,
            )
        }),
    ])
}

/// GTK `jump_to_radio_seed` (queue_actions spec §5.11): re-seed radio from a
/// queue row without restarting playback.
fn jump_to_radio_seed(app: &mut App, index: usize) -> Task<Message> {
    if index >= app.player.queue.len() {
        return Task::none();
    }
    let queue = app.player.queue.clone();
    let prefix_len = if app.player.is_radio_queue() {
        app.player
            .radio_touched_len()
            .max(index.saturating_add(1))
            .min(queue.len())
    } else {
        queue.len()
    };
    start_radio_from_seed_queue_task(app, queue, index, prefix_len)
}

/// GTK `jump_to_radio_seed_from_track`: insert at the touched boundary (radio)
/// or end (non-radio, unless already queued), then re-seed.
fn jump_to_radio_seed_from_track(app: &mut App, track: Track) -> Task<Message> {
    let mut queue = app.player.queue.clone();
    if !app.player.is_radio_queue() {
        if let Some(index) = queue
            .iter()
            .position(|queued| queued.video_id == track.video_id)
        {
            let prefix_len = queue.len();
            return start_radio_from_seed_queue_task(app, queue, index, prefix_len);
        }
        queue.push(track);
        let insert_at = queue.len().saturating_sub(1);
        let prefix_len = queue.len();
        return start_radio_from_seed_queue_task(app, queue, insert_at, prefix_len);
    }

    let queue_len = queue.len();
    let insert_at = app.player.radio_touched_len().min(queue_len);
    queue.insert(insert_at, track);
    let prefix_len = app
        .player
        .radio_touched_len()
        .max(insert_at.saturating_add(1))
        .min(queue.len());
    start_radio_from_seed_queue_task(app, queue, insert_at, prefix_len)
}

/// `start_radio_from_seed(queue, current_index, seed_index, prefix_len, ctx,
/// repeat, play=false)` — jumping to a seed does NOT restart playback.
fn start_radio_from_seed_queue_task(
    app: &mut App,
    queue: Vec<Track>,
    seed_index: usize,
    prefix_len: usize,
) -> Task<Message> {
    let current_index = app.player.queue_index.min(queue.len().saturating_sub(1));
    let playlist_context = app.player.playlist_context.clone();
    let repeat_mode = app.player.repeat_mode;
    app.player.playlist_like_queue = false;
    queue_op(app, move |daemon| {
        daemon.start_radio_from_seed(
            queue,
            current_index,
            seed_index,
            prefix_len,
            playlist_context,
            repeat_mode,
            false,
        )
    })
}

fn insert_tracks_task(app: &mut App, tracks: Vec<Track>, insert_at: usize) -> Task<Message> {
    if tracks.is_empty() {
        return Task::none();
    }
    queue_op(app, move |daemon| {
        daemon.insert_into_queue(tracks, insert_at)
    })
}

/// Run a queue-mutation daemon command on a worker; results are guarded by
/// `State.op_serial` (CONTRACT.md §5).
fn queue_op<F>(app: &mut App, command: F) -> Task<Message>
where
    F: FnOnce(&DaemonClient) -> anyhow::Result<PlaybackSnapshot> + Send + 'static,
{
    app.queue_panel.op_serial = app.queue_panel.op_serial.wrapping_add(1);
    let serial = app.queue_panel.op_serial;
    let daemon = app.player.daemon.clone();
    app::blocking_or_timeout(
        app::DAEMON_TASK_TIMEOUT,
        move || command(&daemon).map_err(|err| err.to_string()),
        move |result| qmsg(Msg::QueueOpFinished { serial, result }),
        qmsg(Msg::QueueOpFinished {
            serial,
            result: Err("daemon queue operation timed out".to_owned()),
        }),
    )
}

/// Run a play/probe daemon command guarded by a fresh play-request serial.
fn play_op<F>(app: &mut App, command: F) -> Task<Message>
where
    F: FnOnce(&DaemonClient) -> anyhow::Result<PlaybackSnapshot> + Send + 'static,
{
    let serial = app.player.next_play_request_serial();
    app.player.mark_play_request_in_flight(serial);
    let daemon = app.player.daemon.clone();
    app::blocking_or_timeout(
        app::DAEMON_TASK_TIMEOUT,
        move || command(&daemon).map_err(|err| err.to_string()),
        move |result| qmsg(Msg::PlayOpFinished { serial, result }),
        qmsg(Msg::PlayOpFinished {
            serial,
            result: Err("daemon playback request timed out".to_owned()),
        }),
    )
}

/// GTK eager now-playing flush before a play/probe request: synthesized
/// current track, zeroed timer/progress, paused play icon, lyrics → loading.
/// Returns the eager thumbnail fetch task.
fn flush_now_playing_eagerly(
    app: &mut App,
    video_id: &str,
    title: &str,
    artist: &str,
    thumbnail_url: Option<&str>,
) -> Task<Message> {
    let lyrics = &mut app.queue_panel.lyrics;
    lyrics.request_serial = lyrics.request_serial.wrapping_add(1);
    lyrics.status = LyricsStatus::Loading;
    lyrics.lines.clear();
    lyrics.meta = None;
    lyrics.active_line = None;

    app.player.snapshot.current_track = Some(ActiveTrackSnapshot {
        video_id: video_id.to_owned(),
        title: title.to_owned(),
        artist: artist.to_owned(),
        thumb_url: thumbnail_url.map(str::to_owned),
        rating: SongRating::Indifferent,
    });
    app.player.snapshot.position_millis = Some(0);
    app.player.snapshot.duration_millis = None;
    app.player.snapshot.buffered_fraction = 0.0;
    app.player.snapshot.paused = true;
    app.player.snapshot.track_repeat_loops_left = None;

    match thumbnail_url {
        Some(url) => thumbnails::ensure(app, url),
        None => Task::none(),
    }
}

/// GTK parity §5.1: play/probe daemon errors surface in the now-playing title
/// (the next daemon poll snapshot reconciles).
fn surface_play_error(app: &mut App, err: String) {
    crate::app_log!("[QUEUE] playback request failed: {err}");
    app.player.snapshot.current_track = Some(ActiveTrackSnapshot {
        video_id: String::new(),
        title: err,
        artist: String::new(),
        thumb_url: None,
        rating: SongRating::Indifferent,
    });
}

/// GTK `update_active_lyric_line`: pick the line covering the position (else
/// the last line that already started) and, when the highlight moved while
/// the Lyrics tab is visible, scroll it back into the 72 px follow band.
/// Shared by the queue panel (lyrics load) and the player bar (seek + poll).
pub fn sync_active_lyric_line(app: &mut App, position_millis: Option<u64>) -> Task<Message> {
    let lyrics = &mut app.queue_panel.lyrics;
    let previous = lyrics.active_line;
    lyrics.active_line = position_millis.and_then(|position| {
        lyrics
            .lines
            .iter()
            .enumerate()
            .find(|(_, line)| {
                let end = line.end_millis.unwrap_or(u64::MAX);
                position >= line.start_millis && position < end
            })
            .map(|(index, _)| index)
            .or_else(|| {
                lyrics
                    .lines
                    .iter()
                    .enumerate()
                    .rfind(|(_, line)| position >= line.start_millis)
                    .map(|(index, _)| index)
            })
    });

    if app.queue_panel.lyrics.active_line != previous
        && app.queue_panel.mode == DetailPanelView::Lyrics
    {
        scroll_lyrics_to_active(app)
    } else {
        Task::none()
    }
}

/// GTK `scroll_active_lyric_into_view`: keep the active line inside a
/// [`LYRICS_FOLLOW_BAND`] padding band of the lyrics scroller. Line geometry
/// is estimated from the fixed line height/spacing (iced exposes no per-child
/// layout query); manual scrolls re-sync via `Msg::LyricsScrolled`.
fn scroll_lyrics_to_active(app: &mut App) -> Task<Message> {
    let Some(index) = app.queue_panel.lyrics.active_line else {
        return Task::none();
    };
    let stride = LYRICS_LINE_HEIGHT + theme::SPACE_MD;
    let line_top = theme::SPACE_14 + index as f32 * stride;
    let line_bottom = line_top + LYRICS_LINE_HEIGHT;

    let viewport = if app.queue_panel.lyrics_viewport_height > 0.0 {
        app.queue_panel.lyrics_viewport_height
    } else {
        FALLBACK_VIEWPORT_HEIGHT
    };
    let band = LYRICS_FOLLOW_BAND.min(viewport / 2.0);
    let offset = app.queue_panel.lyrics_scroll_offset;

    let target = if line_top < offset + band {
        Some((line_top - band).max(0.0))
    } else if line_bottom > offset + viewport - band {
        Some((line_bottom - viewport + band).max(0.0))
    } else {
        None // already inside the band
    };
    let Some(target) = target else {
        return Task::none();
    };

    // `operation::scroll_to` does not publish `on_scroll`; mirror the offset
    // optimistically so the next follow decision uses the new position.
    app.queue_panel.lyrics_scroll_offset = target;
    iced::widget::operation::scroll_to(
        app::LYRICS_SCROLL_ID,
        scrollable::AbsoluteOffset {
            x: None,
            y: Some(target),
        },
    )
}

// ---------------------------------------------------------------------------
// Collection resolution + offline downloads (blocking; queue_drag spec §7.3,
// queue_actions spec §9)
// ---------------------------------------------------------------------------

fn resolve_dragged_collection_tracks(
    payload: &QueueDragPayload,
    client: Option<&NetworkState>,
    cache: &Arc<Mutex<MusicCache>>,
    settings: &AppSettings,
) -> Result<Vec<Track>, String> {
    match payload {
        QueueDragPayload::Playlist {
            playlist_id,
            source: QueuePlaylistSource::Offline,
            ..
        } => Ok(load_offline_collection_tracks(cache, settings, playlist_id)),
        QueueDragPayload::Playlist {
            playlist_id,
            source: QueuePlaylistSource::Remote,
            ..
        } => client
            .ok_or_else(|| "Not connected.".to_owned())?
            .get_playlist(playlist_id)
            .map_err(|err| err.to_string()),
        QueueDragPayload::BrowseCollection { browse_id, .. } => client
            .ok_or_else(|| "Not connected.".to_owned())?
            .get_browse_tracks(browse_id)
            .map_err(|err| err.to_string()),
        QueueDragPayload::QueueRow { .. } | QueueDragPayload::Track { .. } => Ok(Vec::new()),
    }
}

/// Offline collection → tracks via the local cache (including the synthetic
/// `__offline_listen_again__` playlist); never errors.
fn load_offline_collection_tracks(
    cache: &Arc<Mutex<MusicCache>>,
    settings: &AppSettings,
    playlist_id: &str,
) -> Vec<Track> {
    let cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let video_ids = if playlist_id == app::OFFLINE_LISTEN_AGAIN_PLAYLIST_ID {
        cache
            .offline_listen_again_track_ids(settings)
            .unwrap_or_default()
    } else {
        cache
            .offline_playlist_track_ids(playlist_id)
            .unwrap_or_default()
    };

    video_ids
        .into_iter()
        .map(|video_id| {
            let thumbnail_url = cache
                .get_cached_art(&video_id)
                .map(|path| format!("file://{}", path.display()))
                .or_else(|| cache.get_thumbnail_url(&video_id));
            let (title, artist) = cache
                .get_track_meta(&video_id)
                .unwrap_or_else(|| (video_id.clone(), String::new()));
            let artists = if artist.is_empty() {
                Vec::new()
            } else {
                vec![Artist {
                    name: artist,
                    id: None,
                }]
            };
            Track {
                video_id,
                title,
                artists,
                album: None,
                duration: None,
                thumbnail_url,
                playback_playlist_id: None,
                playback_params: None,
                not_interested_feedback_token: None,
                undo_feedback_token: None,
            }
        })
        .collect()
}

/// Blocking download pipeline (queue_actions spec §9.2): cache hit short
/// circuit (+ optional pin + SponsorBlock hydration), else metadata record →
/// audio URL → media bytes → art bytes → store + pin + SponsorBlock.
fn ensure_track_available_offline(
    client: Option<&NetworkState>,
    cache: &Arc<Mutex<MusicCache>>,
    track: &Track,
    pin: bool,
    log_prefix: &str,
) -> bool {
    let vid = &track.video_id;

    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_cached(vid) {
            if pin {
                let _ = guard.pin_offline(vid);
            }
            drop(guard);
            hydrate_offline_sponsorblock(cache, vid);
            return true;
        }
    }

    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = guard.record_play(
            vid,
            &track.title,
            &artist_string(track),
            history_thumbnail_candidate(track.thumbnail_url.as_deref()),
            0,
        );
    }

    let Some(client) = client else {
        return false;
    };
    let url = match client.get_audio_url_no_track(vid) {
        Ok(url) => url,
        Err(err) => {
            crate::app_log!("[{log_prefix}] failed URL for {vid}: {err}");
            return false;
        }
    };

    let audio = match crate::network::download_media_bytes(&url, Some("bytes=0-")) {
        Ok(bytes) => bytes,
        Err(err) => {
            crate::app_log!("[{log_prefix}] download failed for {vid}: {err}");
            return false;
        }
    };

    let art = track
        .thumbnail_url
        .as_deref()
        .and_then(crate::network::fetch_thumbnail_bytes);

    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = guard.store_audio(vid, &audio, art.as_deref());
        if pin {
            let _ = guard.pin_offline(vid);
        }
    }
    hydrate_offline_sponsorblock(cache, vid);
    true
}

/// GTK `ensure_offline_sponsorblock_cache`: hydrate SponsorBlock segments for
/// a cached track into the offline cache (best-effort, errors logged).
fn hydrate_offline_sponsorblock(cache: &Arc<Mutex<MusicCache>>, video_id: &str) {
    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.is_cached(video_id) {
            return;
        }
        match guard.get_offline_sponsor_segments(video_id) {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(err) => {
                crate::app_log!(
                    "[SPONSORBLOCK] failed to read offline cache for {video_id}: {err}"
                );
            }
        }
    }

    let client = match crate::network::SponsorBlockClient::new() {
        Ok(client) => client,
        Err(err) => {
            crate::app_log!("[SPONSORBLOCK] failed to initialize client for {video_id}: {err}");
            return;
        }
    };
    let segments = match client
        .fetch_skip_segments(video_id, &crate::network::default_skip_categories())
    {
        Ok(segments) => segments,
        Err(err) => {
            crate::app_log!("[SPONSORBLOCK] failed to hydrate offline cache for {video_id}: {err}");
            return;
        }
    };

    let guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Err(err) = guard.store_offline_sponsor_segments(video_id, &segments) {
        crate::app_log!("[SPONSORBLOCK] failed to store offline cache for {video_id}: {err}");
    }
}

/// Never persist local-file art into the play-history DB.
fn history_thumbnail_candidate(url: Option<&str>) -> Option<&str> {
    url.filter(|value| !value.starts_with("file://"))
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

// ---------------------------------------------------------------------------
// Pure slot/autoscroll math (ported 1:1; pinned by GTK tests —
// queue_actions spec §2.3, queue_drag spec §6.3 + §9.1)
// ---------------------------------------------------------------------------

/// Raw insertion slot for a pointer y inside the queue content (row-height
/// based; clamped to 0..=row_count). The pointer in the top half of row `i`
/// targets slot `i`, the bottom half targets `i + 1`.
pub fn preview_slot_for_queue_y(content_y: f32, row_count: usize) -> usize {
    if row_count == 0 {
        return 0;
    }
    let y = content_y - LIST_TOP_MARGIN;
    if y <= 0.0 {
        return 0;
    }
    let slot = ((y + QUEUE_ROW_HEIGHT / 2.0) / QUEUE_ROW_HEIGHT).floor() as usize;
    slot.min(row_count)
}

/// Normalize a raw slot against the drag source (drops that would be no-ops
/// return None; slots after the source shift down by one).
pub fn normalized_preview_slot(
    raw_slot: usize,
    source_index: Option<usize>,
    queue_len: usize,
) -> Option<usize> {
    let raw_slot = raw_slot.min(queue_len);
    match source_index {
        Some(from) => {
            let normalized = if from < raw_slot {
                raw_slot.saturating_sub(1)
            } else {
                raw_slot
            };
            (normalized != from).then_some(raw_slot)
        }
        None => Some(raw_slot),
    }
}

/// Signed px-per-tick autoscroll velocity from the pointer's viewport y.
pub fn queue_drag_autoscroll_velocity(viewport_y: f32, viewport_height: f32) -> f32 {
    if viewport_height <= 0.0 {
        return 0.0;
    }

    let edge = AUTOSCROLL_BAND.min((viewport_height / 2.0).max(24.0));
    let clamped_y = viewport_y.clamp(0.0, viewport_height);

    if clamped_y < edge {
        let intensity = (1.0 - (clamped_y / edge)).clamp(0.0, 1.0);
        -(AUTOSCROLL_MIN_STEP + ((AUTOSCROLL_MAX_STEP - AUTOSCROLL_MIN_STEP) * intensity))
    } else if clamped_y > viewport_height - edge {
        let distance = (viewport_height - clamped_y).clamp(0.0, edge);
        let intensity = (1.0 - (distance / edge)).clamp(0.0, 1.0);
        AUTOSCROLL_MIN_STEP + ((AUTOSCROLL_MAX_STEP - AUTOSCROLL_MIN_STEP) * intensity)
    } else {
        0.0
    }
}

/// Reference model for the daemon-side dismiss semantics (queue_actions spec
/// §2.2): truncate the tail AND always exit radio mode.
#[cfg(test)]
fn dismiss_queue_after_current_state(
    queue: &mut Vec<Track>,
    keep_len: usize,
    radio_virtual_playlist: &mut Option<crate::models::RadioVirtualPlaylist>,
) -> bool {
    let mut changed = false;

    if queue.len() > keep_len {
        queue.truncate(keep_len);
        changed = true;
    }

    if radio_virtual_playlist.take().is_some() {
        changed = true;
    }

    changed
}

#[cfg(test)]
#[path = "../tests/ui_queue_panel.rs"]
mod tests;
