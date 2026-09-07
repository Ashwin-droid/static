//! Sidebar: search entry + live suggestions, nav buttons, Recently Played
//! history list, Settings button.
//!
//! Spec: shell.md §5 (layout), §7 (behaviors),
//! views.md §5.1–5.2 (search entry/suggestions), panels.md §3.4 (recent rows).
//! GTK sources: src/ui/window/page_builders.rs (`build_main_page` sidebar part,
//! `nav_button`), src/ui/window/views.rs (`load_sidebar_history`,
//! `build_sidebar_recent_item`).
//!
//! Key behaviors to port:
//! - 250 ms suggestion debounce (use `app::after` + serial check), max 8
//!   suggestions, hidden for URLs/empty/errors/not-connected, cleared on
//!   focus loss, suppressed after programmatic text set.
//! - Enter: direct YouTube/YTM URLs autoplay (`content_loading::
//!   parse_direct_music_url`), otherwise `content_loading::load_search_task`
//!   with a bumped `search_request_serial`.
//! - Nav clicks: clear the entry if it still shows the last *completed* search
//!   query, set the exclusive active nav, load the view via `content_loading`.
//! - History: capped at `app::SIDEBAR_HISTORY_LIMIT`, "No history yet" when
//!   empty, "Could not load history" on failure, "Restoring history..." before
//!   auth restore completes; rows are 200-wide buttons that start radio.

use iced::widget::{button, column, container, mouse_area, scrollable, text, text_input, tooltip};
use iced::{Element, Length, Padding, Point, Task, mouse};

use crate::models::Track;

use super::app::{self, App, ContentView, Message, NavItem, QueueDragPayload};
use super::{content_loading, queue_panel, theme, views, widgets};

const MAX_SUGGESTIONS: usize = 8;

/// Sidebar-owned state (held in `App.sidebar`).
pub struct State {
    pub search_text: String,
    pub suggestions: Vec<String>,
    pub suggestions_visible: bool,
    /// Invalidates stale suggestion fetches and pending debounces.
    pub suggestion_serial: u64,
    /// Invalidates stale full-search responses.
    pub search_request_serial: u64,
    /// One-shot: skip the next text-change reaction (programmatic set).
    pub suppress_suggestions: bool,
    /// Set when a search completes; nav clicks clear the entry if it still
    /// shows exactly this query.
    pub completed_search_query_to_clear: Option<String>,
    pub recent: RecentList,
    /// Last pointer position over the sidebar (window-space; the sidebar sits
    /// at the window origin) — drag origin + context-menu anchor.
    pub last_pointer: Point,
    pub hovered_recent: Option<usize>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            search_text: String::new(),
            suggestions: Vec::new(),
            suggestions_visible: false,
            suggestion_serial: 0,
            search_request_serial: 0,
            suppress_suggestions: false,
            completed_search_query_to_clear: None,
            recent: RecentList::Status("Restoring history...".to_owned()),
            last_pointer: Point::ORIGIN,
            hovered_recent: None,
        }
    }
}

/// The Recently Played list is either a status line or up to 200 track rows.
pub enum RecentList {
    Status(String),
    Tracks(Vec<Track>),
}

#[derive(Clone)]
pub enum Msg {
    OpenVisualizer,
    VisualizerOpened(Result<(), String>),
    SearchInput(String),
    SearchSubmitted,
    SuggestDebounceFired {
        serial: u64,
    },
    SuggestionsLoaded {
        serial: u64,
        result: Result<Vec<String>, String>,
    },
    SuggestionPicked(String),
    NavClicked(NavItem),
    HistoryLoaded(Result<Vec<Track>, String>),
    RecentTrackClicked(Track),
    PointerMoved(Point),
    RecentHovered(Option<usize>),
}

/// Render the whole fixed-width sidebar column (search shell, nav buttons,
/// "Recently Played" header + scrollable list, Settings button).
pub fn view(app: &App) -> Element<'_, Message> {
    let state = &app.sidebar;

    let mut search_shell = column![search_entry(state)]
        .spacing(theme::SPACE_SM)
        .padding(Padding {
            top: theme::SPACE_XL,
            right: theme::SPACE_LG,
            bottom: theme::SPACE_LG,
            left: theme::SPACE_LG,
        });
    if state.suggestions_visible && !state.suggestions.is_empty() {
        search_shell = search_shell.push(suggestions_panel(state));
    }

    let nav = column![
        nav_button("Home", NavItem::Home, app.active_nav),
        nav_button("Likes", NavItem::Likes, app.active_nav),
        nav_button("Playlists", NavItem::Playlists, app.active_nav),
        nav_button("Offline", NavItem::Offline, app.active_nav),
    ]
    .spacing(theme::SPACE_XXS)
    .padding(Padding {
        top: theme::SPACE_XS,
        right: theme::SPACE_MD,
        bottom: theme::SPACE_XS,
        left: theme::SPACE_MD,
    });

    let recent_header = container(
        text("RECENTLY PLAYED")
            .size(theme::SIZE_CAPTION)
            .color(theme::TEXT_FAINT)
            .font(theme::FONT_BOLD),
    )
    .padding(Padding {
        top: theme::SPACE_LG,
        right: theme::SPACE_LG,
        bottom: 0.0,
        left: theme::SPACE_LG,
    });

    let recent_scroll = scrollable(container(recent_list(app)).width(Length::Fill).padding(
        Padding {
            top: theme::SPACE_XS,
            right: theme::SPACE_MD,
            bottom: theme::SPACE_XS,
            left: theme::SPACE_MD,
        },
    ))
    .style(theme::thin_scrollbar)
    .width(Length::Fill)
    .height(Length::Fill);

    let bottom = container(column![
        button(text("Visualiser").size(theme::SIZE_BODY))
            .width(Length::Fill)
            .padding(theme::SPACE_MD)
            .style(theme::nav_item(false))
            .on_press(Message::Sidebar(Msg::OpenVisualizer)),
        nav_button("Settings", NavItem::Settings, app.active_nav),
    ])
    .width(Length::Fill)
    .padding(theme::SPACE_MD);

    let body = column![
        search_shell,
        nav,
        widgets::separator(),
        recent_header,
        recent_scroll,
        widgets::separator(),
        bottom,
    ]
    .width(Length::Fill)
    .height(Length::Fill);

    mouse_area(body)
        .on_move(|position| Message::Sidebar(Msg::PointerMoved(position)))
        .into()
}

pub fn update(app: &mut App, msg: Msg) -> Task<Message> {
    match msg {
        Msg::OpenVisualizer => Task::perform(
            async { crate::visualizer::launch().map_err(|err| err.to_string()) },
            |result| Message::Sidebar(Msg::VisualizerOpened(result)),
        ),
        Msg::VisualizerOpened(result) => match result {
            Ok(()) => Task::none(),
            Err(err) => app.show_toast(err),
        },
        Msg::SearchInput(value) => {
            app.sidebar.search_text = value;
            if app.sidebar.suppress_suggestions {
                app.sidebar.suppress_suggestions = false;
                return Task::none();
            }
            hide_suggestions(&mut app.sidebar);
            let serial = app.sidebar.suggestion_serial;
            let query = app.sidebar.search_text.trim();
            if query.is_empty() || content_loading::parse_direct_music_url(query).is_some() {
                return Task::none();
            }
            app::after(
                app::SUGGESTION_DEBOUNCE,
                Message::Sidebar(Msg::SuggestDebounceFired { serial }),
            )
        }

        Msg::SuggestDebounceFired { serial } => {
            if serial != app.sidebar.suggestion_serial {
                return Task::none();
            }
            let query = app.sidebar.search_text.trim().to_owned();
            if query.is_empty() {
                return Task::none();
            }
            content_loading::load_suggestions_task(app, query, serial)
        }

        Msg::SuggestionsLoaded { serial, result } => {
            if serial != app.sidebar.suggestion_serial {
                return Task::none();
            }
            match result {
                Ok(suggestions) => {
                    let suggestions: Vec<String> =
                        suggestions.into_iter().take(MAX_SUGGESTIONS).collect();
                    app.sidebar.suggestions_visible = !suggestions.is_empty();
                    app.sidebar.suggestions = suggestions;
                }
                Err(err) => {
                    crate::app_log!("[SEARCH] failed to fetch search suggestions: {err}");
                    app.sidebar.suggestions.clear();
                    app.sidebar.suggestions_visible = false;
                }
            }
            Task::none()
        }

        Msg::SuggestionPicked(suggestion) => {
            hide_suggestions(&mut app.sidebar);
            app.sidebar.search_text = suggestion.clone();
            Task::batch([
                iced::widget::operation::focus(app::SEARCH_INPUT_ID),
                iced::widget::operation::move_cursor_to_end(app::SEARCH_INPUT_ID),
                run_sidebar_search(app, suggestion),
            ])
        }

        Msg::SearchSubmitted => {
            hide_suggestions(&mut app.sidebar);
            let query = app.sidebar.search_text.clone();
            run_sidebar_search(app, query)
        }

        Msg::NavClicked(item) => {
            clear_completed_search(&mut app.sidebar);
            hide_suggestions(&mut app.sidebar);
            app.active_nav = item;
            match item {
                NavItem::Home => {
                    app.content_view = ContentView::Home;
                    content_loading::load_home_task(app, false)
                }
                NavItem::Likes => {
                    app.content_view = ContentView::Likes;
                    content_loading::load_likes_task(app)
                }
                NavItem::Playlists => {
                    app.content_view = ContentView::Playlists;
                    content_loading::load_playlists_task(app)
                }
                NavItem::Offline => {
                    app.content_view = ContentView::Offline;
                    content_loading::load_offline_task(app)
                }
                NavItem::Settings => {
                    app.content_view = ContentView::Settings;
                    Task::done(Message::Settings(views::settings::Msg::Opened))
                }
            }
        }

        Msg::HistoryLoaded(result) => {
            app.sidebar.recent = match result {
                Ok(tracks) if tracks.is_empty() => RecentList::Status("No history yet".to_owned()),
                Ok(mut tracks) => {
                    tracks.truncate(app::SIDEBAR_HISTORY_LIMIT);
                    RecentList::Tracks(tracks)
                }
                Err(err) => {
                    crate::app_log!("[SIDEBAR] failed to load history: {err}");
                    RecentList::Status("Could not load history".to_owned())
                }
            };
            Task::none()
        }

        Msg::RecentTrackClicked(track) => {
            let drag_was_active = app.queue_panel.drag.as_ref().is_some_and(|d| d.active);
            clear_queue_drag(app);
            if drag_was_active {
                return Task::none();
            }
            hide_suggestions(&mut app.sidebar);
            let artist = artist_string(&track);
            queue_panel::play_track_task(
                app,
                track.video_id.clone(),
                track.title.clone(),
                artist,
                track.thumbnail_url.clone(),
            )
        }

        Msg::PointerMoved(position) => {
            app.sidebar.last_pointer = position;
            Task::none()
        }

        Msg::RecentHovered(index) => {
            app.sidebar.hovered_recent = index;
            Task::none()
        }
    }
}

/// Background `NetworkState::get_history()` mapped to
/// `Message::Sidebar(Msg::HistoryLoaded)`. Triggered at startup (post-restore),
/// on the 60 s history tick, and after auth restore while main is visible.
pub fn load_history_task(app: &App) -> Task<Message> {
    let client = app.app_client();
    app::blocking(
        move || match client {
            Some(client) => client.get_history().map_err(|err| err.to_string()),
            None => Err("YouTube Music is not connected".to_owned()),
        },
        |result| Message::Sidebar(Msg::HistoryLoaded(result)),
    )
}

/// Prepend a just-played track to the recent list, dedupe by video id, cap at
/// the history limit (queue_actions spec §13).
pub fn add_to_recent(app: &mut App, track: &Track) {
    if !matches!(app.sidebar.recent, RecentList::Tracks(_)) {
        app.sidebar.recent = RecentList::Tracks(Vec::new());
    }
    let RecentList::Tracks(tracks) = &mut app.sidebar.recent else {
        return;
    };
    tracks.retain(|existing| existing.video_id != track.video_id);
    tracks.insert(0, track.clone());
    tracks.truncate(app::SIDEBAR_HISTORY_LIMIT);
}

// ---------------------------------------------------------------------------
// Private behavior helpers
// ---------------------------------------------------------------------------

/// GTK `run_sidebar_search`: trim, discard pending completed-search clear,
/// bump the request serial, then direct-URL autoplay or a full search.
fn run_sidebar_search(app: &mut App, query: String) -> Task<Message> {
    let query = query.trim().to_owned();
    if query.is_empty() {
        return Task::none();
    }
    app.sidebar.completed_search_query_to_clear = None;
    app.sidebar.search_request_serial = app.sidebar.search_request_serial.wrapping_add(1);
    let request_id = app.sidebar.search_request_serial;
    match content_loading::parse_direct_music_url(&query) {
        Some(target) => content_loading::autoplay_direct_music_url_task(app, target),
        None => content_loading::load_search_task(app, query, request_id),
    }
}

/// Invalidate pending debounces/fetches and hide the suggestions panel.
fn hide_suggestions(state: &mut State) {
    state.suggestion_serial = state.suggestion_serial.wrapping_add(1);
    state.suggestions.clear();
    state.suggestions_visible = false;
}

/// GTK `clear_completed_sidebar_search`: the pending clear is consumed on any
/// nav click; the entry only empties when it still shows exactly that query.
fn clear_completed_search(state: &mut State) {
    let Some(query) = state.completed_search_query_to_clear.take() else {
        return;
    };
    if state.search_text.trim() == query {
        state.search_text.clear();
        hide_suggestions(state);
    }
}

/// Recent rows can resolve a drag started on themselves (plain click) or a
/// drag released over the sidebar (cancel); either way the panel drag ends.
fn clear_queue_drag(app: &mut App) {
    app.queue_panel.drag = None;
    app.queue_panel.drop_preview_slot = None;
    app.queue_panel.drag_hover_y = None;
    app.queue_panel.drop_active = false;
}

/// GTK `queue_accepts_external_dnd`: panel visible (queue non-empty) and the
/// "Up Next" tab active.
fn queue_accepts_external_drag(app: &App) -> bool {
    !app.player.queue.is_empty() && app.queue_panel.mode == queue_panel::DetailPanelView::Queue
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
// Private view helpers
// ---------------------------------------------------------------------------

fn search_entry(state: &State) -> Element<'_, Message> {
    text_input("Search songs...", &state.search_text)
        .id(app::SEARCH_INPUT_ID)
        .on_input(|value| Message::Sidebar(Msg::SearchInput(value)))
        .on_submit(Message::Sidebar(Msg::SearchSubmitted))
        .style(theme::entry)
        .size(theme::SIZE_BODY)
        .padding(Padding {
            top: theme::SPACE_MD,
            right: theme::SPACE_LG,
            bottom: theme::SPACE_MD,
            left: theme::SPACE_LG,
        })
        .into()
}

fn suggestions_panel(state: &State) -> Element<'_, Message> {
    container(
        column(state.suggestions.iter().map(|suggestion| {
            button(
                text(suggestion.as_str())
                    .size(theme::SIZE_BODY)
                    .wrapping(text::Wrapping::None),
            )
            .style(theme::list_row(false))
            .width(Length::Fill)
            .padding(Padding {
                top: theme::SPACE_MD,
                right: theme::SPACE_LG,
                bottom: theme::SPACE_MD,
                left: theme::SPACE_LG,
            })
            .on_press(Message::Sidebar(Msg::SuggestionPicked(suggestion.clone())))
            .into()
        }))
        .width(Length::Fill),
    )
    .style(theme::suggestions_panel)
    .padding(theme::SPACE_XS)
    .width(Length::Fill)
    .into()
}

fn nav_button(
    label: &'static str,
    item: NavItem,
    active_nav: NavItem,
) -> Element<'static, Message> {
    button(text(label).size(theme::SIZE_BODY))
        .style(theme::nav_item(active_nav == item))
        .width(Length::Fill)
        .padding(Padding {
            top: theme::SPACE_MD,
            right: theme::SPACE_LG,
            bottom: theme::SPACE_MD,
            left: theme::SPACE_LG,
        })
        .on_press(Message::Sidebar(Msg::NavClicked(item)))
        .into()
}

fn recent_list(app: &App) -> Element<'_, Message> {
    match &app.sidebar.recent {
        RecentList::Status(status) => container(
            text(status.as_str())
                .size(theme::SIZE_SUB)
                .color(theme::TEXT_GHOST),
        )
        .padding(Padding {
            top: theme::SPACE_MD,
            right: theme::SPACE_XS,
            bottom: theme::SPACE_MD,
            left: theme::SPACE_XS,
        })
        .into(),
        RecentList::Tracks(tracks) => column(
            tracks
                .iter()
                .enumerate()
                .map(|(index, track)| recent_row(app, index, track)),
        )
        .spacing(theme::SPACE_XXS)
        .width(Length::Fill)
        .into(),
    }
}

fn recent_row<'a>(app: &'a App, index: usize, track: &'a Track) -> Element<'a, Message> {
    let state = &app.sidebar;
    let hovered = state.hovered_recent == Some(index);
    let artist = artist_string(track);

    let body = container(
        column![
            text(track.title.as_str())
                .size(theme::SIZE_SUB)
                .color(theme::TEXT)
                .wrapping(text::Wrapping::None),
            text(artist.clone())
                .size(theme::SIZE_MICRO)
                .color(theme::TEXT_FAINT)
                .wrapping(text::Wrapping::None),
        ]
        .spacing(theme::SPACE_XXS),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: theme::SPACE_SM,
        right: theme::SPACE_MD,
        bottom: theme::SPACE_SM,
        left: theme::SPACE_MD,
    })
    .style(recent_row_style(hovered))
    .clip(true);

    let mut area = mouse_area(body)
        .on_release(Message::Sidebar(Msg::RecentTrackClicked(track.clone())))
        .on_right_press(Message::TrackMenu(queue_panel::MenuMsg::Open {
            track: track.clone(),
            queue_index: None,
            anchor: state.last_pointer,
        }))
        .on_enter(Message::Sidebar(Msg::RecentHovered(Some(index))))
        .on_exit(Message::Sidebar(Msg::RecentHovered(None)))
        .interaction(mouse::Interaction::Pointer);
    if queue_accepts_external_drag(app) {
        area = area.on_press(Message::Queue(queue_panel::Msg::DragStarted {
            payload: QueueDragPayload::Track {
                track: track.clone(),
            },
            position: state.last_pointer,
        }));
    }

    // GTK hover-marquee equivalent (contract §9): truncated text + tooltip.
    tooltip(
        area,
        container(text(format!("{} — {artist}", track.title)).size(theme::SIZE_SUB))
            .style(theme::popover)
            .padding(Padding {
                top: theme::SPACE_SM,
                right: theme::SPACE_LG,
                bottom: theme::SPACE_SM,
                left: theme::SPACE_LG,
            }),
        tooltip::Position::FollowCursor,
    )
    .into()
}

fn recent_row_style(hovered: bool) -> impl Fn(&iced::Theme) -> iced::widget::container::Style {
    move |_t| iced::widget::container::Style {
        background: hovered.then(|| iced::Background::Color(theme::white(0.05))),
        border: iced::Border {
            color: iced::Color::TRANSPARENT,
            width: 0.0,
            radius: theme::RADIUS_SM.into(),
        },
        ..iced::widget::container::Style::default()
    }
}
