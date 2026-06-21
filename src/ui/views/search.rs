//! Search results view: results header + summary line, per-section bodies
//! (track grids, browse grids, mixed grids), play controls, scroll cache.
//!
//! Spec: views.md §5.3–5.4, content_loading.md §5.
//! GTK sources: src/ui/window/content_loading.rs (`load_search`,
//! `render_cached_search_page`), src/ui/window/views.rs
//! (`render_search_view` + section grids).
//!
//! Notes:
//! - Search track rows use `TrackRowClickBehavior::StartRadio`; mixed grids
//!   index into the tracks-only list.
//! - Rendering a result stores `SearchPageCache` on `App.search` and sets
//!   `content_view = Search`; browse detail opened from here returns to this
//!   exact search (query, results, scroll) via the back target.

use iced::widget::{
    Space, button, column, container, mouse_area, row, scrollable, svg, text, tooltip,
};
use iced::{Element, Length, Point, Task};

use crate::models::{
    HomeBrowseCard, HomeBrowsePageType, HomeItem, HomeSection, SearchResult, Track,
};

use super::super::app::{self, App, ContentView, Message, QueueDragPayload, SearchPageCache};
use super::super::{content_loading, icons, queue_panel, sidebar, theme, thumbnails, widgets};

/// Track/browse row art edge (GTK `.track-art` 44×44 parity metric).
const ROW_ART_SIZE: f32 = 44.0;
/// Top offset of loading/error/empty status labels (GTK `margin_top(60)`).
const STATUS_TOP_MARGIN: f32 = 60.0;
/// Icon edge inside the Play All / Shuffle controls.
const CONTROL_ICON_SIZE: f32 = 14.0;

pub struct State {
    /// Last rendered search (query + results + scroll). Mirrored into
    /// `BrowseBackTarget::Search` when entering a browse detail.
    pub cache: Option<SearchPageCache>,
    /// Query currently being fetched ("Searching \"{q}\"...").
    pub loading_query: Option<String>,
    /// Inline error ("Search error: {e}" / "Not connected.").
    pub error: Option<String>,
    /// Hovered grid cell `(section_index, content_index)` — drives the row
    /// hover tint (rows are `mouse_area`s, not buttons, so they can be queue
    /// drag sources; hover feedback is tracked explicitly).
    pub hovered_item: Option<(usize, usize)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            cache: None,
            loading_query: None,
            error: None,
            hovered_item: None,
        }
    }
}

#[derive(Clone)]
pub enum Msg {
    /// Search fetch finished; dropped when `request_id` is stale.
    Loaded {
        request_id: u64,
        query: String,
        result: Result<SearchResult, String>,
    },
    /// Track row clicked: start radio from the selected song. The
    /// view emits the lean `Section*` forms; these payload-carrying forms are
    /// kept as the direct API for cross-module callers.
    #[allow(dead_code)]
    TrackClicked {
        tracks: Vec<Track>,
        index: usize,
    },
    BrowseRowClicked(HomeBrowseCard),
    #[allow(dead_code)]
    PlayAll {
        tracks: Vec<Track>,
        shuffle: bool,
    },
    /// Lean row click emitted by the view: resolves the section's tracks-only
    /// list from the cache in `update` (avoids cloning whole track lists into
    /// every row's message at view time).
    SectionTrackClicked {
        section: usize,
        index: usize,
    },
    /// Lean Play All / Shuffle emitted by the view (same resolution).
    SectionPlayAll {
        section: usize,
        shuffle: bool,
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

pub fn view(app: &App) -> Element<'_, Message> {
    let state = &app.search;
    if let Some(query) = &state.loading_query {
        return status_view(format!("Searching \"{query}\"..."));
    }
    if let Some(error) = &state.error {
        return status_view(error.clone());
    }
    match &state.cache {
        Some(cache) => results_view(app, cache),
        None => status_view("Search for songs, artists and albums.".to_owned()),
    }
}

pub fn update(app: &mut App, msg: Msg) -> Task<Message> {
    match msg {
        Msg::Loaded {
            request_id,
            query,
            result,
        } => {
            if request_id != app.sidebar.search_request_serial {
                // A newer search/autoplay superseded this fetch.
                return Task::none();
            }
            app.search.loading_query = None;
            match result {
                Ok(results) => {
                    // Arms the sidebar auto-clear: the next nav click empties
                    // the entry iff it still contains exactly this query.
                    app.sidebar.completed_search_query_to_clear = Some(query.clone());
                    show_cached_page(
                        app,
                        SearchPageCache {
                            query,
                            results,
                            scroll_position: 0.0,
                        },
                    )
                }
                Err(err) => {
                    // `load_search_task` already labels errors ("Search error:
                    // {e}" / "Not connected.") — display verbatim (GTK §5.3).
                    app.search.error = Some(err);
                    // GTK parity: the completion clears the content box and
                    // renders the error label regardless of where the user is.
                    app.content_view = ContentView::Search;
                    Task::none()
                }
            }
        }

        Msg::TrackClicked { tracks, index } => start_radio_from_track(app, tracks, index),
        Msg::SectionTrackClicked { section, index } => {
            let tracks = cached_section_tracks(app, section);
            start_radio_from_track(app, tracks, index)
        }

        Msg::BrowseRowClicked(card) => {
            let press = release_pending_press(app);
            if press.was_active {
                return Task::none(); // it was a drag, not a click
            }
            match &press.payload {
                Some(QueueDragPayload::BrowseCollection { browse_id, .. })
                    if *browse_id != card.browse_id =>
                {
                    return Task::none(); // release slid in from another row
                }
                Some(QueueDragPayload::BrowseCollection { .. }) | None => {}
                Some(_) => return Task::none(),
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

        Msg::PlayAll { tracks, shuffle } => {
            if tracks.is_empty() {
                return Task::none();
            }
            // Search play controls carry no playlist context (views spec §5.4).
            queue_panel::play_all_task(app, tracks, shuffle, None)
        }
        Msg::SectionPlayAll { section, shuffle } => {
            let tracks = cached_section_tracks(app, section);
            if tracks.is_empty() {
                return Task::none();
            }
            queue_panel::play_all_task(app, tracks, shuffle, None)
        }

        Msg::ItemHovered { section, item } => {
            app.search.hovered_item = Some((section, item));
            Task::none()
        }
        Msg::ItemUnhovered { section, item } => {
            if app.search.hovered_item == Some((section, item)) {
                app.search.hovered_item = None;
            }
            Task::none()
        }
    }
}

/// Synchronous equivalent of GTK `render_cached_search_page`: stores the
/// cache (fresh results use scroll 0; "← Back to Search" passes the offset
/// captured when the browse page was entered), kills home pagination, flips
/// the content view to Search, restores the scroll offset and warms the
/// thumbnails. Also used by browse detail's back button.
pub fn show_cached_page(app: &mut App, cache: SearchPageCache) -> Task<Message> {
    // Search kills home pagination state (content_loading spec §5.2).
    app.home.continuation = None;
    app.home.loading = false;

    app.search.loading_query = None;
    app.search.error = None;
    app.search.hovered_item = None;

    let scroll_position = cache.scroll_position.max(0.0);
    let thumb_urls = result_thumbnail_urls(&cache.results);
    app.search.cache = Some(cache);
    app.content_view = ContentView::Search;

    Task::batch([
        iced::widget::operation::scroll_to(
            app::CONTENT_SCROLL_ID,
            scrollable::AbsoluteOffset {
                x: None,
                y: Some(scroll_position),
            },
        ),
        thumbnails::ensure_all(app, thumb_urls.iter().map(String::as_str)),
    ])
}

// ---------------------------------------------------------------------------
// Click plumbing
// ---------------------------------------------------------------------------

/// Left-click on a track row: start radio from the clicked song and prepend
/// the track to the sidebar recents.
fn start_radio_from_track(app: &mut App, tracks: Vec<Track>, index: usize) -> Task<Message> {
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
    queue_panel::play_track_radio_task(app, track)
}

struct ReleasedPress {
    payload: Option<QueueDragPayload>,
    was_active: bool,
}

/// Rows deliver their click on `mouse_area::on_release` (the press registered
/// a pending queue drag). A release before the drag activated is the row's
/// normal click (CONTRACT.md §7.2); either way the pending drag state is
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

fn cached_section_tracks(app: &App, section_index: usize) -> Vec<Track> {
    app.search
        .cache
        .as_ref()
        .and_then(|cache| cache.results.sections.get(section_index))
        .map(section_tracks)
        .unwrap_or_default()
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

fn results_view<'a>(app: &'a App, cache: &'a SearchPageCache) -> Element<'a, Message> {
    let results = &cache.results;
    let total: usize = results
        .sections
        .iter()
        .map(|section| section.contents.len())
        .sum();

    let mut page = column![
        text(format!(
            "Results for \"{}\" \u{2014} {} results",
            cache.query, total
        ))
        .size(theme::SIZE_SECTION)
        .color(theme::TEXT_BRIGHT)
    ]
    .spacing(theme::SPACE_XXL)
    .width(Length::Fill);

    if total == 0 {
        return page
            .push(
                container(
                    text("No results found.")
                        .size(theme::SIZE_BODY)
                        .color(theme::TEXT_DIM),
                )
                .width(Length::Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .padding(iced::Padding {
                    top: theme::SPACE_XXL,
                    ..iced::Padding::ZERO
                }),
            )
            .into();
    }

    if let Some(summary) = results_summary(results) {
        page = page.push(text(summary).size(theme::SIZE_BODY).color(theme::TEXT_DIM));
    }

    let mut rendered_sections = 0usize;
    for (section_index, section) in results.sections.iter().enumerate() {
        if section.contents.is_empty() {
            continue;
        }
        if rendered_sections > 0 {
            page = page.push(widgets::separator());
        }
        if !section.title.trim().is_empty() {
            page = page.push(
                container(
                    text(&section.title)
                        .size(theme::SIZE_SECTION)
                        .color(theme::TEXT_BRIGHT),
                )
                .padding(iced::Padding {
                    top: theme::SPACE_MD,
                    ..iced::Padding::ZERO
                }),
            );
        }
        page = page.push(section_body(app, section_index, section));
        rendered_sections += 1;
    }

    page.into()
}

/// One section body: optional Play All / Shuffle controls (> 1 track), then a
/// 2-column grid preserving the original item order. Track rows index into
/// the section's tracks-only list so mixed grids play the contiguous track
/// context at the right offset; all-track and all-browse sections are the
/// degenerate cases of the same layout (views spec §5.4).
fn section_body<'a>(
    app: &'a App,
    section_index: usize,
    section: &'a HomeSection,
) -> Element<'a, Message> {
    let track_count = section
        .contents
        .iter()
        .filter(|item| matches!(item, HomeItem::Track(_)))
        .count();

    let mut body = column![].spacing(theme::SPACE_LG).width(Length::Fill);
    if track_count > 1 {
        body = body.push(play_controls(section_index));
    }

    let mut cells: Vec<Element<'a, Message>> = Vec::with_capacity(section.contents.len());
    let mut track_index = 0usize;
    for (item_index, item) in section.contents.iter().enumerate() {
        let key = (section_index, item_index);
        match item {
            HomeItem::Track(track) => {
                cells.push(track_row(
                    app,
                    key,
                    track,
                    Msg::SectionTrackClicked {
                        section: section_index,
                        index: track_index,
                    },
                ));
                track_index += 1;
            }
            HomeItem::BrowseCard(card) => cells.push(browse_row(app, key, card)),
        }
    }

    body.push(two_col_grid(cells)).into()
}

fn play_controls(section_index: usize) -> Element<'static, Message> {
    row![
        control_button(
            icons::play(),
            "Play All",
            Msg::SectionPlayAll {
                section: section_index,
                shuffle: false,
            },
        ),
        control_button(
            icons::shuffle(),
            "Shuffle",
            Msg::SectionPlayAll {
                section: section_index,
                shuffle: true,
            },
        ),
    ]
    .spacing(theme::SPACE_MD)
    .into()
}

fn control_button(icon: svg::Handle, label: &'static str, msg: Msg) -> Element<'static, Message> {
    button(
        row![
            widgets::icon(icon, CONTROL_ICON_SIZE),
            text(label).size(theme::SIZE_BODY),
        ]
        .spacing(theme::SPACE_SM)
        .align_y(iced::Alignment::Center),
    )
    .style(theme::secondary_button)
    .padding([theme::SPACE_SM, theme::SPACE_LG])
    .on_press(Message::Search(msg))
    .into()
}

/// Track row: 44 px art (letter fallback), truncated title with full-text
/// tooltip (marquee stand-in), ellipsized artist line, optional right-aligned
/// duration. The row is a queue drag source; the click rides on release
/// (see `release_pending_press`); right-click opens the shared track menu.
fn track_row<'a>(
    app: &'a App,
    key: (usize, usize),
    track: &'a Track,
    click: Msg,
) -> Element<'a, Message> {
    let hovered = app.search.hovered_item == Some(key);
    let art = thumbnails::view_thumb(
        app,
        track.thumbnail_url.as_deref(),
        ROW_ART_SIZE,
        track.title.chars().next(),
    );
    let info = column![
        clipped_title(&track.title),
        clipped_caption(artist_string(track))
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
    .on_release(Message::Search(click))
    .on_right_press(Message::TrackMenu(queue_panel::MenuMsg::Open {
        track: track.clone(),
        queue_index: None,
        anchor: Point::ORIGIN,
    }))
    .on_enter(Message::Search(Msg::ItemHovered {
        section: key.0,
        item: key.1,
    }))
    .on_exit(Message::Search(Msg::ItemUnhovered {
        section: key.0,
        item: key.1,
    }))
    .into()
}

/// Browse row: track-row look-alike with the page-type fallback subtitle, no
/// duration and no context menu (views spec §7.4). Album/Playlist/PodcastShow
/// rows are queue drag sources; Artist/UserChannel rows are plain buttons.
fn browse_row<'a>(
    app: &'a App,
    key: (usize, usize),
    card: &'a HomeBrowseCard,
) -> Element<'a, Message> {
    let subtitle = card
        .subtitle
        .clone()
        .filter(|s| !s.trim().is_empty())
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

    match drag_payload_for_browse_card(card) {
        Some(payload) => {
            let hovered = app.search.hovered_item == Some(key);
            mouse_area(
                container(content)
                    .padding([theme::SPACE_SM, theme::SPACE_MD])
                    .width(Length::Fill)
                    .style(theme::row_surface(hovered)),
            )
            .interaction(iced::mouse::Interaction::Pointer)
            .on_press(Message::Queue(queue_panel::Msg::DragStarted {
                payload,
                position: Point::ORIGIN,
            }))
            .on_release(Message::Search(Msg::BrowseRowClicked(card.clone())))
            .on_enter(Message::Search(Msg::ItemHovered {
                section: key.0,
                item: key.1,
            }))
            .on_exit(Message::Search(Msg::ItemUnhovered {
                section: key.0,
                item: key.1,
            }))
            .into()
        }
        None => button(content)
            .style(theme::row_button)
            .padding([theme::SPACE_SM, theme::SPACE_MD])
            .width(Length::Fill)
            .on_press(Message::Search(Msg::BrowseRowClicked(card.clone())))
            .into(),
    }
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

/// Single-line truncated title with the full text in a tooltip (the contract's
/// stand-in for the GTK hover marquee).
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
// Pure helpers (ported from GTK views.rs)
// ---------------------------------------------------------------------------

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

/// "{n} tracks  •  {n} playlists  •  ..." in the fixed order tracks /
/// playlists / artists / albums / channels / shows; only non-zero parts;
/// `None` when everything is zero (GTK `search_results_summary`).
fn results_summary(results: &SearchResult) -> Option<String> {
    let mut tracks = 0usize;
    let mut playlists = 0usize;
    let mut artists = 0usize;
    let mut albums = 0usize;
    let mut channels = 0usize;
    let mut shows = 0usize;

    for section in &results.sections {
        for item in &section.contents {
            match item {
                HomeItem::Track(_) => tracks += 1,
                HomeItem::BrowseCard(card) => match card.page_type {
                    HomeBrowsePageType::Playlist => playlists += 1,
                    HomeBrowsePageType::Album => albums += 1,
                    HomeBrowsePageType::Artist => artists += 1,
                    HomeBrowsePageType::UserChannel => channels += 1,
                    HomeBrowsePageType::PodcastShow => shows += 1,
                },
            }
        }
    }

    let mut parts = Vec::new();
    if tracks > 0 {
        parts.push(format!("{tracks} tracks"));
    }
    if playlists > 0 {
        parts.push(format!("{playlists} playlists"));
    }
    if artists > 0 {
        parts.push(format!("{artists} artists"));
    }
    if albums > 0 {
        parts.push(format!("{albums} albums"));
    }
    if channels > 0 {
        parts.push(format!("{channels} channels"));
    }
    if shows > 0 {
        parts.push(format!("{shows} shows"));
    }

    (!parts.is_empty()).then(|| parts.join("  \u{2022}  "))
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

fn result_thumbnail_urls(results: &SearchResult) -> Vec<String> {
    results
        .sections
        .iter()
        .flat_map(|section| section.contents.iter())
        .filter_map(|item| match item {
            HomeItem::Track(track) => track.thumbnail_url.clone(),
            HomeItem::BrowseCard(card) => card.thumbnail_url.clone(),
        })
        .collect()
}
