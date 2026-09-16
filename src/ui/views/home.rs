//! Home view: resume row (persisted snapshot + cross-device queue), section
//! carousels and 2-column browse grids, infinite scroll, scroll-position
//! restore, bootstrap retry state.
//!
//! Spec: views.md §6.1–6.2, content_loading.md §4,
//! shell.md §6. GTK sources: src/ui/window/content_loading.rs (home parts),
//! src/ui/window/views.rs (`render_home_view`, `render_resume_section`),
//! src/ui/window/page_builders.rs (card builders).
//!
//! Notes:
//! - Track cards on home are discovery surfaces: left-click seeds radio
//!   (`TrackRowClickBehavior::StartRadio` → `queue_panel::play_track_task`).
//! - Cached home renders instantly with continuation + scroll restored
//!   (`iced::widget::operation::scroll_to(app::CONTENT_SCROLL_ID, ...)`),
//!   clamped; forced refresh resets to 0.
//! - Listen-again metadata is persisted into the offline cache on every load.

use std::time::Duration;

use iced::widget::scrollable::AbsoluteOffset;
use iced::widget::{
    Space, button, column, container, mouse_area, row, scrollable, svg, text, tooltip,
};
use iced::{Background, Border, Color, Element, Length, Point, Task, Theme};

use crate::ipc::{PersistedPlaybackState, PlaybackSnapshot};
use crate::models::{
    CrossDeviceQueue, HomeBrowseCard, HomeBrowsePageType, HomeItem, HomePage, HomeSection, Track,
};

use super::super::app::{self, App, ContentView, Message, NavItem, PlayerModel, QueueDragPayload};
use super::super::{
    content_loading, icons, player_bar, queue_panel, sidebar, theme, thumbnails, widgets,
};

/// Track/browse row art edge in the 2-column grids (GTK `.track-art` 44×44).
const ROW_ART_SIZE: f32 = 44.0;
/// Cover-card art edge in the horizontal carousels (GTK `.cover-art` 150×150).
const COVER_ART_SIZE: f32 = 150.0;
/// Resume row art edge (GTK `.resume-drop-art` 56×56).
const RESUME_ART_SIZE: f32 = 56.0;
/// Top offset of loading/error/empty status labels (GTK `margin_top(60)`).
const STATUS_TOP_MARGIN: f32 = 60.0;
/// Icon edge inside the Play All / Shuffle controls.
const CONTROL_ICON_SIZE: f32 = 14.0;
/// iced stand-in for GTK's post-render `queue_home_lazy_load_check`: without
/// layout callbacks we cannot measure the rendered height, so short feeds are
/// auto-filled until at least this many sections exist (≈ the GTK
/// viewport+720 px fill target) or the continuation is exhausted.
const LAZY_FILL_MIN_SECTIONS: usize = 6;

pub struct State {
    /// Rendered home feed (GTK `AppState.home_cache`).
    pub page: Option<HomePage>,
    /// "From another device" resume row source (cleared with the home cache).
    pub cross_device_queue: Option<CrossDeviceQueue>,
    /// Continuously persisted while home is active; restored after re-render.
    pub scroll_position: f32,
    /// Continuation token for infinite scroll (None when exhausted).
    pub continuation: Option<String>,
    /// Guards concurrent home loads/continuations.
    pub loading: bool,
    /// Loading/error status line ("Loading recommendations...", retry text).
    pub status: Option<String>,
    /// Bootstrap retry attempt (0-based; backoff `HOME_BOOTSTRAP_BACKOFF_SECS`).
    pub bootstrap_attempt: u32,
    /// Resolved resume-row data (GTK `load_home_resume_data`, refreshed on
    /// every successful first-page home load).
    pub resume: Option<ResumeData>,
    /// True while a resume restore runs (the row renders disabled).
    pub resume_in_flight: bool,
    /// Hovered card/row `(section_index, content_index)` — rows and cards are
    /// `mouse_area`s (queue drag sources), so hover feedback is tracked
    /// explicitly like the search grids.
    pub hovered_item: Option<(usize, usize)>,
    /// Hovered Play All / Shuffle control `(section, shuffle)` — also a
    /// `mouse_area` drag source.
    pub hovered_control: Option<(usize, bool)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            page: None,
            cross_device_queue: None,
            scroll_position: 0.0,
            continuation: None,
            loading: false,
            status: None,
            bootstrap_attempt: 0,
            resume: None,
            resume_in_flight: false,
            hovered_item: None,
            hovered_control: None,
        }
    }
}

/// Where the resume row restores from (GTK `HomeResumeSource`).
#[derive(Clone)]
pub enum ResumeSource {
    Local(PlaybackSnapshot),
    CrossDevice(CrossDeviceQueue),
}

/// Resume row contents (GTK `HomeResumeData`).
#[derive(Clone)]
pub struct ResumeData {
    pub source: ResumeSource,
    pub track: Track,
    pub meta: String,
}

impl ResumeData {
    fn header(&self) -> &'static str {
        match self.source {
            ResumeSource::Local(_) => "Resume dropped",
            ResumeSource::CrossDevice(_) => "From another device",
        }
    }
}

#[derive(Clone)]
pub enum Msg {
    /// Home + cross-device fetch finished (bootstrap or nav load).
    Loaded {
        force_refresh: bool,
        attempt: u32,
        result: Result<(Option<CrossDeviceQueue>, HomePage), String>,
    },
    /// Bootstrap backoff timer fired — try the next attempt.
    RetryTimerFired {
        attempt: u32,
    },
    /// Continuation fetch finished.
    MoreLoaded {
        result: Result<HomePage, String>,
    },
    /// A track card was clicked (starts radio).
    TrackCardClicked(Track),
    /// A browse card was clicked (album/playlist/artist → browse detail).
    BrowseCardClicked(HomeBrowseCard),
    /// Play All / Shuffle on a section (tracks resolved from state).
    SectionPlayAll {
        section: usize,
        shuffle: bool,
    },
    /// Play All / Shuffle pressed: resolve the section's tracks and register
    /// them as a queue drag; the release delivers `SectionPlayAll`.
    SectionControlPressed {
        section: usize,
        shuffle: bool,
    },
    ControlHovered {
        section: usize,
        shuffle: bool,
    },
    ControlUnhovered {
        section: usize,
        shuffle: bool,
    },
    /// Resume row: resume the persisted/cross-device queue.
    ResumeClicked,
    /// Resume row resolution finished (snapshot applied via player_bar).
    ResumeFinished {
        result: Result<PlaybackSnapshot, String>,
    },
    /// Background resume-data resolution finished (local snapshot read +
    /// cross-device precedence decided off the UI thread).
    ResumeDataResolved {
        resume: Option<ResumeData>,
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

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

pub fn view(app: &App) -> Element<'_, Message> {
    let state = &app.home;
    let Some(page) = state.page.as_ref() else {
        let message = state
            .status
            .clone()
            .unwrap_or_else(|| "Loading recommendations...".to_owned());
        return status_view(message);
    };

    let mut content = column![].spacing(theme::SPACE_XXL).width(Length::Fill);

    let resume_rendered = match resume_row(app) {
        Some(resume) => {
            content = content.push(resume);
            true
        }
        None => false,
    };

    if page.sections.is_empty() {
        return content
            .push(status_view(
                "No recommendations returned. Your cookie might be expired.".to_owned(),
            ))
            .into();
    }

    for (section_index, section) in page.sections.iter().enumerate() {
        if section_index > 0 || resume_rendered {
            content = content.push(widgets::separator());
        }
        content = content.push(widgets::section_header(&section.title));

        let tracks = section_tracks(section);
        if should_show_home_play_controls(section, &tracks) {
            content = content.push(play_controls(app, section_index));
        }
        content = content.push(section_body(app, section_index, section, tracks.len()));
    }

    content.into()
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

pub fn update(app: &mut App, msg: Msg) -> Task<Message> {
    match msg {
        Msg::Loaded {
            force_refresh,
            attempt,
            result,
        } => handle_loaded(app, force_refresh, attempt, result),
        Msg::RetryTimerFired { attempt } => handle_retry_timer(app, attempt),
        Msg::MoreLoaded { result } => handle_more_loaded(app, result),
        Msg::TrackCardClicked(track) => handle_track_card_clicked(app, track),
        Msg::BrowseCardClicked(card) => handle_browse_card_clicked(app, card),
        Msg::SectionPlayAll { section, shuffle } => {
            if release_pending_press(app).was_drag_or_foreign() {
                return Task::none();
            }
            let tracks = cached_section_tracks(app, section);
            if tracks.is_empty() {
                return Task::none();
            }
            // Home play controls carry no playlist context (views spec §6.1).
            queue_panel::play_all_task(app, tracks, shuffle, None)
        }
        Msg::SectionControlPressed { section, shuffle } => {
            let title = app
                .home
                .page
                .as_ref()
                .and_then(|page| page.sections.get(section))
                .map(|section| section.title.clone())
                .unwrap_or_default();
            let tracks = cached_section_tracks(app, section);
            match queue_panel::track_list_payload(tracks, title, shuffle) {
                Some(payload) => queue_panel::start_drag_task(app, payload),
                None => Task::none(),
            }
        }
        Msg::ControlHovered { section, shuffle } => {
            app.home.hovered_control = Some((section, shuffle));
            Task::none()
        }
        Msg::ControlUnhovered { section, shuffle } => {
            if app.home.hovered_control == Some((section, shuffle)) {
                app.home.hovered_control = None;
            }
            Task::none()
        }
        Msg::ResumeClicked => handle_resume_clicked(app),
        Msg::ResumeFinished { result } => {
            app.home.resume_in_flight = false;
            match result {
                Ok(snapshot) => {
                    // GTK parity: a successful resume consumes the stored
                    // cross-device queue; the row hides because something is
                    // playing now.
                    app.home.cross_device_queue = None;
                    player_bar::apply_remote_snapshot(app, snapshot)
                }
                Err(err) => {
                    crate::app_log!("[UI] failed to resume playback: {err}");
                    Task::none()
                }
            }
        }
        Msg::ResumeDataResolved { resume } => {
            let thumb_url = resume
                .as_ref()
                .and_then(|data| data.track.thumbnail_url.clone());
            app.home.resume = resume;
            match thumb_url {
                Some(url) => thumbnails::ensure(app, &url),
                None => Task::none(),
            }
        }
        Msg::ItemHovered { section, item } => {
            app.home.hovered_item = Some((section, item));
            Task::none()
        }
        Msg::ItemUnhovered { section, item } => {
            if app.home.hovered_item == Some((section, item)) {
                app.home.hovered_item = None;
                app.home.hovered_control = None;
            }
            Task::none()
        }
    }
}

/// First-page completion for both the startup bootstrap and nav/Ctrl+R loads
/// (content_loading spec §4.1 step 6, §4.2 step 4, §4.3).
fn handle_loaded(
    app: &mut App,
    force_refresh: bool,
    attempt: u32,
    result: Result<(Option<CrossDeviceQueue>, HomePage), String>,
) -> Task<Message> {
    app.home.loading = false;

    match result {
        Ok((cross_device_queue, page)) => {
            app.home.bootstrap_attempt = 0;
            app.home.status = None;
            app.home.continuation = page.continuation.clone();
            app.home.cross_device_queue = cross_device_queue.clone();
            app.home.hovered_item = None;
            let thumb_urls = home_thumbnail_urls(&page);
            app.home.page = Some(page);
            // GTK parity: the completion clears the content box and renders
            // home regardless of where the user navigated meanwhile.
            app.content_view = ContentView::Home;

            if force_refresh {
                app.home.scroll_position = 0.0;
            }
            let scroll_target = app.home.scroll_position;

            let thumbs = thumbnails::ensure_all(app, thumb_urls.iter().map(String::as_str));
            let lazy_fill = lazy_fill_task(app);
            Task::batch([
                thumbs,
                restore_scroll(scroll_target),
                resolve_resume_task(cross_device_queue),
                lazy_fill,
            ])
        }
        Err(err) => {
            if attempt == content_loading::NAV_LOAD_ATTEMPT {
                // Nav click / Ctrl+R: inline error label, no retry, no
                // fallback (content_loading spec §4.1).
                app.home.continuation = None;
                app.home.status = Some(err);
                app.content_view = ContentView::Home;
                return Task::none();
            }
            handle_bootstrap_failure(app, attempt, err)
        }
    }
}

/// Bootstrap retry/backoff/offline-fallback orchestration (content_loading
/// spec §4.2 steps 4–5).
fn handle_bootstrap_failure(app: &mut App, attempt: u32, err: String) -> Task<Message> {
    let last_error = if err == content_loading::NOT_CONNECTED {
        "session still restoring".to_owned()
    } else {
        err
    };

    if app.active_nav != NavItem::Home {
        // User navigated away while we were retrying — abort silently; the
        // other view owns the content area now.
        return Task::none();
    }

    let backoff = app::HOME_BOOTSTRAP_BACKOFF_SECS;
    let attempt_index = attempt as usize;
    if attempt_index < backoff.len() {
        let delay = backoff[attempt_index];
        app.home.loading = true;
        app.home.status = Some(format!(
            "Loading recommendations... Retry {}/{} in {}s",
            attempt + 1,
            backoff.len(),
            delay
        ));
        return app::after(
            Duration::from_secs(delay),
            Message::Home(Msg::RetryTimerFired {
                attempt: attempt + 1,
            }),
        );
    }

    let total_attempts = backoff.len() + 1;
    crate::app_log!(
        "[HOME] failed to load after {total_attempts} attempts, falling back offline: {last_error}"
    );
    app.home.status = None;
    app.home.bootstrap_attempt = 0;
    app.active_nav = NavItem::Offline;
    content_loading::load_offline_task(app)
}

fn handle_retry_timer(app: &mut App, attempt: u32) -> Task<Message> {
    if app.active_nav != NavItem::Home {
        app.home.loading = false;
        return Task::none();
    }
    app.home.bootstrap_attempt = attempt;
    app.home.loading = true;
    content_loading::bootstrap_home_task(app)
}

/// Continuation completion (content_loading spec §4.4): silent errors, cache
/// only extended while home is still the active view.
fn handle_more_loaded(app: &mut App, result: Result<HomePage, String>) -> Task<Message> {
    app.home.loading = false;

    if app.content_view != ContentView::Home {
        // User navigated away mid-fetch: discard, do NOT extend the cache.
        return Task::none();
    }
    let Ok(page) = result else {
        // Completely silent (GTK parity); the continuation token stays in
        // place so a later scroll retries.
        return Task::none();
    };

    app.home.continuation = page.continuation.clone();
    let thumb_urls = home_thumbnail_urls(&page);
    match app.home.page.as_mut() {
        Some(cached) => {
            cached.sections.extend(page.sections);
            cached.continuation = page.continuation;
        }
        // Invalidated mid-flight (Ctrl+R race): fresh page from the new
        // sections (content_loading spec §4.4 step 6).
        None => app.home.page = Some(page),
    }

    let thumbs = thumbnails::ensure_all(app, thumb_urls.iter().map(String::as_str));
    let lazy_fill = lazy_fill_task(app);
    Task::batch([thumbs, lazy_fill])
}

/// Left-click on a track card/row: StartRadio behavior — `play_track` seeds
/// radio from the clicked song and the track joins the sidebar recents. Also
/// the landing point of direct-URL video autoplay (content_loading.rs).
fn handle_track_card_clicked(app: &mut App, track: Track) -> Task<Message> {
    let press = release_pending_press(app);
    if press.was_active {
        return Task::none(); // it was a drag, not a click
    }
    match &press.payload {
        Some(QueueDragPayload::Track { track: pressed }) if pressed.video_id != track.video_id => {
            return Task::none(); // release slid in from another card
        }
        Some(QueueDragPayload::Track { .. }) | None => {}
        Some(_) => return Task::none(),
    }

    let play = queue_panel::play_track_task(
        app,
        track.video_id.clone(),
        track.title.clone(),
        artist_string(&track),
        track.thumbnail_url.clone(),
    );
    sidebar::add_to_recent(app, &track);
    play
}

fn handle_browse_card_clicked(app: &mut App, card: HomeBrowseCard) -> Task<Message> {
    let press = release_pending_press(app);
    if press.was_active {
        return Task::none(); // it was a drag, not a click
    }
    match &press.payload {
        Some(QueueDragPayload::BrowseCollection { browse_id, .. })
            if *browse_id != card.browse_id =>
        {
            return Task::none(); // release slid in from another card
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

/// Resume click: disable the row, then restore via daemon off the UI thread —
/// local snapshots replay volume → queue → seek → pause/resume; cross-device
/// queues go through the shared CLI restore helper (views spec §6.2).
fn handle_resume_clicked(app: &mut App) -> Task<Message> {
    if app.home.resume_in_flight {
        return Task::none();
    }
    let Some(resume) = app.home.resume.clone() else {
        return Task::none();
    };
    app.home.resume_in_flight = true;

    app::blocking_or_timeout(
        app::DAEMON_TASK_TIMEOUT,
        move || match resume.source {
            ResumeSource::Local(snapshot) => {
                crate::ipc::ensure_daemon_running().map_err(|err| err.to_string())?;
                crate::cli::restore_playback_snapshot_via_daemon(snapshot, true)
                    .map_err(|err| err.to_string())
            }
            ResumeSource::CrossDevice(queue) => {
                crate::cli::restore_cross_device_queue_via_daemon(queue)
                    .map_err(|err| err.to_string())
            }
        },
        |result| Message::Home(Msg::ResumeFinished { result }),
        Message::Home(Msg::ResumeFinished {
            result: Err("daemon resume request timed out".to_owned()),
        }),
    )
}

// ---------------------------------------------------------------------------
// Async helpers
// ---------------------------------------------------------------------------

fn restore_scroll(position: f32) -> Task<Message> {
    iced::widget::operation::scroll_to(
        app::CONTENT_SCROLL_ID,
        AbsoluteOffset {
            x: None,
            y: Some(position.max(0.0)),
        },
    )
}

/// Post-render auto-fill (GTK `queue_home_lazy_load_check` + the §4.4 step 8
/// cascade): fetch the next page while the feed is too short to scroll. Also
/// run by `content_loading`'s cached home paths (GTK ran the check after
/// every cached render, healing feeds left short by an abandoned
/// continuation).
pub fn lazy_fill_task(app: &mut App) -> Task<Message> {
    let too_short = app
        .home
        .page
        .as_ref()
        .is_some_and(|page| page.sections.len() < LAZY_FILL_MIN_SECTIONS);
    if too_short && !app.home.loading && app.home.continuation.is_some() {
        content_loading::load_more_home_task(app)
    } else {
        Task::none()
    }
}

/// Resolve the resume row source off the UI thread (GTK resolved this on the
/// main thread during every home render; here it refreshes per home load).
fn resolve_resume_task(cross_device_queue: Option<CrossDeviceQueue>) -> Task<Message> {
    app::blocking(
        move || load_home_resume_data(cross_device_queue),
        |resume| Message::Home(Msg::ResumeDataResolved { resume }),
    )
}

/// GTK `load_home_resume_data`: local-vs-cross-device precedence via
/// `crate::cli::should_use_local_resume_for_cross_device`.
fn load_home_resume_data(cross_device_queue: Option<CrossDeviceQueue>) -> Option<ResumeData> {
    let local_resume = match crate::cli::load_local_resume_state() {
        Ok(resume) => resume,
        Err(err) => {
            crate::app_log!("[UI] failed to read local resume state: {err}");
            None
        }
    };

    if crate::cli::should_use_local_resume_for_cross_device(
        cross_device_queue.as_ref(),
        local_resume.as_ref(),
    ) {
        if let Some(resume) = local_resume {
            return local_home_resume_data(resume);
        }
    }

    if let Some(queue) = cross_device_queue {
        let track = crate::cli::cross_device_selected_track(&queue)?.clone();
        let meta = queue
            .source_label
            .clone()
            .unwrap_or_else(|| "YouTube Music cross-device queue".to_string());
        return Some(ResumeData {
            source: ResumeSource::CrossDevice(queue),
            track,
            meta,
        });
    }

    local_home_resume_data(local_resume?)
}

fn local_home_resume_data(resume: PersistedPlaybackState) -> Option<ResumeData> {
    let snapshot = resume.snapshot;
    let track = crate::cli::resume_snapshot_track(&snapshot)?;
    Some(ResumeData {
        meta: crate::cli::resume_snapshot_meta(&snapshot),
        source: ResumeSource::Local(snapshot),
        track,
    })
}

// ---------------------------------------------------------------------------
// Click plumbing (mirrors the search grids: presses register a pending queue
// drag, the click rides on release)
// ---------------------------------------------------------------------------

struct ReleasedPress {
    payload: Option<QueueDragPayload>,
    was_active: bool,
}

impl ReleasedPress {
    /// A Play All / Shuffle release is a click only when the press did not
    /// become a drag and did not slide in from a row or card.
    fn was_drag_or_foreign(&self) -> bool {
        self.was_active
            || !matches!(
                self.payload,
                None | Some(QueueDragPayload::TrackList { .. })
            )
    }
}

/// Cards/rows deliver their click on `mouse_area::on_release` (the press
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

fn cached_section_tracks(app: &App, section_index: usize) -> Vec<Track> {
    app.home
        .page
        .as_ref()
        .and_then(|page| page.sections.get(section_index))
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

/// The "Resume dropped" / "From another device" row (views spec §6.2):
/// rendered only when nothing is playing AND no queue exists AND resume data
/// resolved. 56×56 art, title/artist/meta lines, trailing "Resume" badge.
fn resume_row(app: &App) -> Option<Element<'_, Message>> {
    if !should_show_dropped_resume(&app.player) {
        return None;
    }
    let data = app.home.resume.as_ref()?;

    let art = thumbnails::view_thumb(
        app,
        data.track.thumbnail_url.as_deref(),
        RESUME_ART_SIZE,
        data.track.title.chars().next(),
    );
    let info = column![
        clipped_line(
            &data.track.title,
            theme::SIZE_BODY,
            theme::TEXT_BRIGHT,
            Some(theme::FONT_SEMIBOLD),
            None
        ),
        widgets::artist_line(&data.track, theme::SIZE_CAPTION, theme::TEXT_DIM),
        clipped_line(&data.meta, theme::SIZE_MICRO, theme::TEXT_FAINT, None, None),
    ]
    .spacing(theme::SPACE_XXS)
    .width(Length::Fill);
    let badge = container(
        text("Resume")
            .size(theme::SIZE_MICRO)
            .color(theme::ACCENT_FG),
    )
    .style(resume_badge)
    .padding([theme::SPACE_XS, theme::SPACE_LG]);

    let mut btn = button(
        row![art, info, badge]
            .spacing(theme::SPACE_LG)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill),
    )
    .style(resume_row_button)
    .padding([theme::SPACE_LG, theme::SPACE_LG])
    .width(Length::Fill);
    if !app.home.resume_in_flight {
        btn = btn.on_press(Message::Home(Msg::ResumeClicked));
    }

    Some(
        column![widgets::section_header(data.header()), btn]
            .spacing(theme::SPACE_LG)
            .width(Length::Fill)
            .into(),
    )
}

/// One section body: the first section always renders as a horizontal cover
/// carousel; later sections render as a 2-col track grid (StartRadio), a
/// 2-col browse grid, or a mixed carousel (views spec §6.1).
fn section_body<'a>(
    app: &'a App,
    section_index: usize,
    section: &'a HomeSection,
    track_count: usize,
) -> Element<'a, Message> {
    if section_index == 0 {
        return cards_carousel(app, section_index, section);
    }

    let all_tracks = !section.contents.is_empty() && track_count == section.contents.len();
    let all_browse = !section.contents.is_empty() && track_count == 0;

    if all_tracks {
        let cells = section
            .contents
            .iter()
            .enumerate()
            .filter_map(|(item_index, item)| match item {
                HomeItem::Track(track) => Some(track_row(app, (section_index, item_index), track)),
                HomeItem::BrowseCard(_) => None,
            })
            .collect();
        two_col_grid(cells)
    } else if all_browse {
        let cells = section
            .contents
            .iter()
            .enumerate()
            .filter_map(|(item_index, item)| match item {
                HomeItem::BrowseCard(card) => {
                    Some(browse_row(app, (section_index, item_index), card))
                }
                HomeItem::Track(_) => None,
            })
            .collect();
        two_col_grid(cells)
    } else {
        cards_carousel(app, section_index, section)
    }
}

/// Horizontal cover-card shelf (GTK `build_horizontal_cards_scroll`): plain
/// wheel passes through to the page scroller, Shift+wheel scrolls the shelf.
fn cards_carousel<'a>(
    app: &'a App,
    section_index: usize,
    section: &'a HomeSection,
) -> Element<'a, Message> {
    let mut cards = row![]
        .spacing(theme::SPACE_LG)
        .align_y(iced::Alignment::Start);
    for (item_index, item) in section.contents.iter().enumerate() {
        let key = (section_index, item_index);
        cards = cards.push(match item {
            HomeItem::Track(track) => cover_card(app, key, track),
            HomeItem::BrowseCard(card) => browse_cover_card(app, key, card),
        });
    }

    scrollable(container(cards).padding(iced::Padding {
        top: theme::SPACE_XS,
        bottom: theme::SPACE_LG,
        ..iced::Padding::ZERO
    }))
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

/// Track cover card (GTK `build_cover_card`): 150 px art, marquee→tooltip
/// title, faint artist line. Click starts radio; the card is a queue drag
/// source; right-click opens the shared track menu.
fn cover_card<'a>(app: &'a App, key: (usize, usize), track: &'a Track) -> Element<'a, Message> {
    let hovered = app.home.hovered_item == Some(key);
    let body = column![
        thumbnails::view_thumb(
            app,
            track.thumbnail_url.as_deref(),
            COVER_ART_SIZE,
            track.title.chars().next(),
        ),
        clipped_line(
            &track.title,
            theme::SIZE_BODY,
            theme::TEXT_BRIGHT,
            Some(theme::FONT_SEMIBOLD),
            Some(&track.title)
        ),
        widgets::artist_line(track, theme::SIZE_CAPTION, theme::TEXT_FAINT),
    ]
    .spacing(theme::SPACE_SM)
    .width(Length::Fixed(COVER_ART_SIZE));

    mouse_area(
        container(body)
            .padding(theme::SPACE_MD)
            .style(theme::card_surface(hovered)),
    )
    .interaction(iced::mouse::Interaction::Pointer)
    .on_press(Message::Queue(queue_panel::Msg::DragStarted {
        payload: QueueDragPayload::Track {
            track: track.clone(),
        },
        position: Point::ORIGIN,
    }))
    .on_release(Message::Home(Msg::TrackCardClicked(track.clone())))
    .on_right_press(Message::TrackMenu(queue_panel::MenuMsg::Open {
        track: track.clone(),
        queue_index: None,
        anchor: Point::ORIGIN,
    }))
    .on_enter(Message::Home(Msg::ItemHovered {
        section: key.0,
        item: key.1,
    }))
    .on_exit(Message::Home(Msg::ItemUnhovered {
        section: key.0,
        item: key.1,
    }))
    .into()
}

/// Browse summary cover card (GTK `build_browse_summary_card`): subtitle only
/// when present; Album/Playlist/PodcastShow cards are queue drag sources,
/// Artist/UserChannel cards are plain buttons.
fn browse_cover_card<'a>(
    app: &'a App,
    key: (usize, usize),
    card: &'a HomeBrowseCard,
) -> Element<'a, Message> {
    let mut body = column![
        thumbnails::view_thumb(
            app,
            card.thumbnail_url.as_deref(),
            COVER_ART_SIZE,
            card.title.chars().next(),
        ),
        clipped_line(
            &card.title,
            theme::SIZE_BODY,
            theme::TEXT_BRIGHT,
            Some(theme::FONT_SEMIBOLD),
            Some(&card.title)
        ),
    ]
    .spacing(theme::SPACE_SM)
    .width(Length::Fixed(COVER_ART_SIZE));
    if let Some(subtitle) = card.subtitle.as_deref().filter(|s| !s.trim().is_empty()) {
        body = body.push(clipped_line(
            subtitle,
            theme::SIZE_CAPTION,
            theme::TEXT_FAINT,
            None,
            None,
        ));
    }

    match drag_payload_for_browse_card(card) {
        Some(payload) => {
            let hovered = app.home.hovered_item == Some(key);
            mouse_area(
                container(body)
                    .padding(theme::SPACE_MD)
                    .style(theme::card_surface(hovered)),
            )
            .interaction(iced::mouse::Interaction::Pointer)
            .on_press(Message::Queue(queue_panel::Msg::DragStarted {
                payload,
                position: Point::ORIGIN,
            }))
            .on_release(Message::Home(Msg::BrowseCardClicked(card.clone())))
            .on_enter(Message::Home(Msg::ItemHovered {
                section: key.0,
                item: key.1,
            }))
            .on_exit(Message::Home(Msg::ItemUnhovered {
                section: key.0,
                item: key.1,
            }))
            .into()
        }
        None => button(body)
            .style(cover_card_button)
            .padding(theme::SPACE_MD)
            .on_press(Message::Home(Msg::BrowseCardClicked(card.clone())))
            .into(),
    }
}

/// Grid track row: 44 px art, truncated title with full-text tooltip,
/// ellipsized artist, optional right-aligned duration. Left-click starts
/// radio (StartRadio surface); drag source; right-click → shared track menu.
fn track_row<'a>(app: &'a App, key: (usize, usize), track: &'a Track) -> Element<'a, Message> {
    let hovered = app.home.hovered_item == Some(key);
    let art = thumbnails::view_thumb(
        app,
        track.thumbnail_url.as_deref(),
        ROW_ART_SIZE,
        track.title.chars().next(),
    );
    let info = column![
        clipped_line(
            &track.title,
            theme::SIZE_BODY,
            theme::TEXT_BRIGHT,
            Some(theme::FONT_SEMIBOLD),
            Some(&track.title)
        ),
        widgets::artist_line(track, theme::SIZE_CAPTION, theme::TEXT_DIM),
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
            .style(theme::row_surface(hovered)),
    )
    .interaction(iced::mouse::Interaction::Pointer)
    .on_press(Message::Queue(queue_panel::Msg::DragStarted {
        payload: QueueDragPayload::Track {
            track: track.clone(),
        },
        position: Point::ORIGIN,
    }))
    .on_release(Message::Home(Msg::TrackCardClicked(track.clone())))
    .on_right_press(Message::TrackMenu(queue_panel::MenuMsg::Open {
        track: track.clone(),
        queue_index: None,
        anchor: Point::ORIGIN,
    }))
    .on_enter(Message::Home(Msg::ItemHovered {
        section: key.0,
        item: key.1,
    }))
    .on_exit(Message::Home(Msg::ItemUnhovered {
        section: key.0,
        item: key.1,
    }))
    .into()
}

/// Grid browse row: track-row look-alike with the page-type fallback
/// subtitle, no duration, no context menu. Album/Playlist/PodcastShow rows
/// are queue drag sources; Artist/UserChannel rows are plain buttons.
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
        column![
            clipped_line(
                &card.title,
                theme::SIZE_BODY,
                theme::TEXT_BRIGHT,
                Some(theme::FONT_SEMIBOLD),
                Some(&card.title)
            ),
            clipped_line(&subtitle, theme::SIZE_CAPTION, theme::TEXT_DIM, None, None),
        ]
        .spacing(theme::SPACE_XXS)
        .width(Length::Fill),
    ]
    .spacing(theme::SPACE_LG)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill);

    match drag_payload_for_browse_card(card) {
        Some(payload) => {
            let hovered = app.home.hovered_item == Some(key);
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
            .on_release(Message::Home(Msg::BrowseCardClicked(card.clone())))
            .on_enter(Message::Home(Msg::ItemHovered {
                section: key.0,
                item: key.1,
            }))
            .on_exit(Message::Home(Msg::ItemUnhovered {
                section: key.0,
                item: key.1,
            }))
            .into()
        }
        None => button(content)
            .style(theme::row_button)
            .padding([theme::SPACE_SM, theme::SPACE_MD])
            .width(Length::Fill)
            .on_press(Message::Home(Msg::BrowseCardClicked(card.clone())))
            .into(),
    }
}

/// Play All + Shuffle (home sections never carry Download All or a playlist
/// context — views spec §6.1).
/// Both controls are queue drag sources: dropping one into the queue inserts
/// the section's (shuffled) tracks at the drop slot.
fn play_controls(app: &App, section_index: usize) -> Element<'static, Message> {
    row![
        control_button(app, icons::play(), "Play All", section_index, false),
        control_button(app, icons::shuffle(), "Shuffle", section_index, true),
    ]
    .spacing(theme::SPACE_MD)
    .into()
}

fn control_button(
    app: &App,
    icon: svg::Handle,
    label: &'static str,
    section: usize,
    shuffle: bool,
) -> Element<'static, Message> {
    let hovered = app.home.hovered_control == Some((section, shuffle));
    widgets::drag_control(
        icon,
        CONTROL_ICON_SIZE,
        label,
        hovered,
        widgets::DragControlEvents {
            press: Message::Home(Msg::SectionControlPressed { section, shuffle }),
            release: Message::Home(Msg::SectionPlayAll { section, shuffle }),
            enter: Message::Home(Msg::ControlHovered { section, shuffle }),
            exit: Message::Home(Msg::ControlUnhovered { section, shuffle }),
        },
    )
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

/// Single-line truncated label; passing `tooltip_text` adds the full text on
/// hover (the contract's stand-in for the GTK hover marquee).
fn clipped_line<'a>(
    label: &str,
    size: f32,
    color: Color,
    font: Option<iced::Font>,
    tooltip_text: Option<&str>,
) -> Element<'a, Message> {
    let mut line_text = text(label.to_owned())
        .size(size)
        .color(color)
        .wrapping(text::Wrapping::None);
    if let Some(font) = font {
        line_text = line_text.font(font);
    }
    let line = container(line_text).width(Length::Fill).clip(true);
    match tooltip_text {
        Some(full) => tooltip(
            line,
            container(text(full.to_owned()).size(theme::SIZE_SUB))
                .style(theme::popover)
                .padding([theme::SPACE_XS, theme::SPACE_MD]),
            tooltip::Position::Top,
        )
        .into(),
        None => line.into(),
    }
}

// ---------------------------------------------------------------------------
// Private styles (built from theme tokens only)
// ---------------------------------------------------------------------------

/// Button equivalent of `card_surface` for non-draggable browse cover cards.
fn cover_card_button(_t: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => theme::white(0.05),
        button::Status::Pressed => theme::ACCENT_BG_GHOST,
        _ => Color::TRANSPARENT,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: theme::TEXT,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: theme::RADIUS_CARD.into(),
        },
        ..button::Style::default()
    }
}

/// The resume row surface (GTK `.track-row.resume-drop-row`).
fn resume_row_button(_t: &Theme, status: button::Status) -> button::Style {
    let (bg, border_color) = match status {
        button::Status::Hovered => (theme::white(0.08), theme::ACCENT_BG),
        button::Status::Pressed => (theme::BG_CARD_PRESSED, theme::ACCENT_BORDER),
        button::Status::Disabled => (theme::white(0.02), theme::BORDER_CARD),
        _ => (theme::BG_CARD, theme::BORDER_CARD),
    };
    let mut style = button::Style {
        background: Some(Background::Color(bg)),
        text_color: theme::TEXT,
        border: Border {
            color: border_color,
            width: 1.0,
            radius: theme::RADIUS_XL.into(),
        },
        ..button::Style::default()
    };
    if matches!(status, button::Status::Disabled) {
        style.text_color = theme::TEXT_FAINT;
    }
    style
}

/// The "Resume" pill badge (GTK `.resume-drop-badge`).
fn resume_badge(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme::ACCENT_BG_FAINT)),
        text_color: Some(theme::ACCENT_FG),
        border: Border {
            color: theme::ACCENT_BG,
            width: 1.0,
            radius: theme::RADIUS_PILL.into(),
        },
        ..container::Style::default()
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (ported from GTK views.rs / playback.rs)
// ---------------------------------------------------------------------------

/// GTK `should_show_dropped_resume`: nothing playing and nothing queued (per
/// both the daemon snapshot and the live UI queue mirror).
fn should_show_dropped_resume(player: &PlayerModel) -> bool {
    player.snapshot.current_track.is_none()
        && player.snapshot.queue.is_empty()
        && player.queue.is_empty()
}

/// GTK `should_show_home_play_controls`: tracks non-empty AND (all-track
/// section OR a "Listen again" title).
fn should_show_home_play_controls(section: &HomeSection, tracks: &[Track]) -> bool {
    !tracks.is_empty()
        && (tracks.len() == section.contents.len() || is_listen_again_section(&section.title))
}

fn is_listen_again_section(title: &str) -> bool {
    title.eq_ignore_ascii_case("Listen again")
        || title.to_ascii_lowercase().contains("listen again")
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

fn home_thumbnail_urls(page: &HomePage) -> Vec<String> {
    page.sections
        .iter()
        .flat_map(|section| section.contents.iter())
        .filter_map(|item| match item {
            HomeItem::Track(track) => track.thumbnail_url.clone(),
            HomeItem::BrowseCard(card) => card.thumbnail_url.clone(),
        })
        .collect()
}
