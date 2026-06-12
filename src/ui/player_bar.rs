//! Now-playing bar: progress slider (red, with buffer fill), transport,
//! sleep popover, halt-auto pending controls, time label, track info with
//! marquee, like/dislike/share/menu actions, volume, repeat — plus snapshot
//! application and daemon-loss recovery.
//!
//! Spec: playback.md (all sections), shell.md §5.3,
//! §8.4, §9, styles.md §3.7.
//! GTK sources: src/ui/window/playback.rs, src/ui/window/page_builders.rs
//! (§4.3 now-playing bar), src/ui/window.rs (`apply_seek_position`,
//! `reset_player_ui`, `submit_song_rating`, `spawn_song_rating_sync`).
//!
//! Invariants:
//! - Every control is a daemon command returning a `PlaybackSnapshot` that is
//!   applied back via `apply_remote_snapshot` (never mutate playback locally).
//! - Next/prev responses are dropped when a newer play request exists
//!   (`app.player.is_current_play_request`).
//! - Halt-auto Cancel/Resume are shown+enabled iff
//!   `snapshot.halt_auto_resume_pending`.
//! - Buttons are disabled ("Nothing playing") until a snapshot has a track.
//! - The GTK `sync_mpris_*` calls are not ported: the daemon owns the MPRIS
//!   bridge; the GTK helpers were stubs/echo calls.

use iced::widget::tooltip::Position as TooltipPosition;
use iced::widget::{
    Stack, button, column, container, progress_bar, row, slider, svg, text, text_input, tooltip,
};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Task, Theme};

use crate::ipc::{
    ActiveTrackSnapshot, DaemonClient, PersistedPlaybackState, PlaybackSnapshot, RepeatMode,
    current_unix_millis, store_persisted_playback_state, store_persisted_playlist_state,
};
use crate::models::Track;
use crate::network::SongRating;

use super::app::{self, App, Message, NowPlayingTrack, PlayerModel};
use super::queue_panel::{self, DetailPanelView, LyricsStatus};
use super::{icons, sidebar, theme, thumbnails, widgets};

/// Total hit-target height of the progress slider strip at the top of the bar.
const SEEK_AREA_HEIGHT: f32 = 14.0;
/// Resting trough thickness (styles spec §3.7: 3 px, 4 px on hover).
const PROGRESS_TROUGH: f32 = 3.0;
const PROGRESS_TROUGH_HOVER: f32 = 4.0;
const PROGRESS_KNOB_RADIUS: f32 = 6.0;
const VOLUME_SLIDER_WIDTH: f32 = 90.0;
const VOLUME_KNOB_RADIUS: f32 = 5.0;
const SLEEP_PANEL_WIDTH: f32 = 220.0;
const SLEEP_CUSTOM_MIN: u32 = 1;
const SLEEP_CUSTOM_MAX: u32 = 360;
const SLEEP_CUSTOM_STEP: u32 = 5;

/// Bar-local UI state (transport state lives in `App.player`; this struct
/// only holds popover/visual state and the volume send queue).
pub struct State {
    pub sleep_menu_open: bool,
    /// Custom sleep minutes entry (GTK SpinButton 1–360, step 5, initial 30).
    pub sleep_custom_minutes: String,
    /// Serializes `set_volume` daemon calls so they cannot reorder.
    pub volume_send_in_flight: bool,
    pub pending_volume: Option<f64>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            sleep_menu_open: false,
            sleep_custom_minutes: "30".to_owned(),
            volume_send_in_flight: false,
            pending_volume: None,
        }
    }
}

#[derive(Clone)]
pub enum SleepOption {
    Minutes(u32),
    AfterCurrent,
    Clear,
}

#[derive(Clone)]
pub enum Msg {
    PlayPausePressed,
    NextPressed,
    PrevPressed,
    /// Slider dragged to a 0..=1 fraction (preview only; seek on release).
    SeekChanged(f64),
    SeekReleased,
    VolumeChanged(f64),
    RepeatPressed,
    LikePressed,
    DislikePressed,
    SharePressed,
    /// Toggles the detail panel between Queue and Lyrics (shell spec §5.2).
    MenuPressed,
    SleepPressed,
    SleepCustomChanged(String),
    SleepOptionPicked(SleepOption),
    HaltAutoCancelPressed,
    HaltAutoResumePressed,

    /// Completion of any transport daemon command issued with a play-request
    /// serial (toggle/next/prev/seek/repeat/halt-auto/stop).
    DaemonResult {
        serial: u64,
        result: Result<PlaybackSnapshot, String>,
    },
    /// `set_volume` finished; drains the pending value (order-preserving).
    VolumeResult(Result<(), String>),
    /// Sleep daemon command finished; toasts per request kind.
    SleepResult {
        request: SleepOption,
        result: Result<PlaybackSnapshot, String>,
    },
    /// `NetworkState::get_song_rating` result, gated by playback session id.
    RatingSynced {
        session_id: u64,
        video_id: String,
        result: Result<SongRating, String>,
    },
    /// Optimistic like/dislike network submit finished (rollback on Err).
    RatingSubmitted {
        session_id: u64,
        video_id: String,
        previous: SongRating,
        result: Result<(), String>,
    },
    /// `DaemonClient::copy_share_url` result (toast on success).
    ShareCopied(Result<String, String>),
    /// Daemon recovery (restart + queue restore) finished.
    RecoveryFinished(Result<PlaybackSnapshot, String>),
}

fn pmsg(msg: Msg) -> Message {
    Message::Playback(msg)
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// The full now-playing bar (progress slider on top, controls row below).
pub fn view(app: &App) -> Element<'_, Message> {
    let mut bar = column![];
    if app.player_bar.sleep_menu_open {
        bar = bar.push(container(sleep_panel(app)).padding(Padding {
            top: theme::SPACE_MD,
            right: theme::SPACE_MD,
            bottom: theme::SPACE_XS,
            left: theme::SPACE_MD,
        }));
    }
    bar = bar.push(progress_strip(app)).push(controls_row(app));

    column![
        widgets::separator(),
        container(bar)
            .width(Length::Fill)
            .style(theme::now_playing_bar),
    ]
    .width(Length::Fill)
    .into()
}

/// Red elapsed + white buffer fill over a hairline trough (styles spec §3.7).
fn progress_strip(app: &App) -> Element<'_, Message> {
    let snapshot = &app.player.snapshot;
    let value = app.player.seek_preview.unwrap_or_else(|| {
        match (snapshot.position_millis, snapshot.duration_millis) {
            (Some(pos), Some(dur)) if dur > 0 => (pos as f64 / dur as f64).clamp(0.0, 1.0),
            _ => 0.0,
        }
    });

    let buffer = container(
        progress_bar(0.0..=1.0, snapshot.buffered_fraction.clamp(0.0, 1.0) as f32)
            .length(Length::Fill)
            .girth(Length::Fixed(PROGRESS_TROUGH))
            .style(buffer_bar_style),
    )
    .width(Length::Fill)
    .center_y(Length::Fixed(SEEK_AREA_HEIGHT));

    let seek = slider(0.0..=1.0, value, |fraction| {
        pmsg(Msg::SeekChanged(fraction))
    })
    .step(0.01)
    .on_release(pmsg(Msg::SeekReleased))
    .height(SEEK_AREA_HEIGHT)
    .width(Length::Fill)
    .style(progress_slider_style);

    Stack::new()
        .width(Length::Fill)
        .height(Length::Fixed(SEEK_AREA_HEIGHT))
        .push(buffer)
        .push(seek)
        .into()
}

fn controls_row(app: &App) -> Element<'_, Message> {
    let snapshot = &app.player.snapshot;
    let current = snapshot.current_track.as_ref();
    let has_track = current.is_some();
    let queue_nonempty = !app.player.queue.is_empty();
    let transport_enabled = has_track && queue_nonempty;
    let rating = current_rating(app);

    // Transport cluster: prev, play/pause, next, sleep, halt-auto pending.
    let play_icon = if snapshot.paused {
        icons::play()
    } else {
        icons::pause()
    };
    let mut transport = row![
        np_icon_button(
            icons::prev(),
            20.0,
            transport_enabled.then(|| pmsg(Msg::PrevPressed)),
            false,
            theme::SPACE_SM,
        ),
        np_icon_button(
            play_icon,
            28.0,
            has_track.then(|| pmsg(Msg::PlayPausePressed)),
            false,
            theme::SPACE_XS,
        ),
        np_icon_button(
            icons::next(),
            20.0,
            transport_enabled.then(|| pmsg(Msg::NextPressed)),
            false,
            theme::SPACE_SM,
        ),
        sleep_button(app),
    ]
    .spacing(theme::SPACE_XXS)
    .align_y(Alignment::Center);
    if snapshot.halt_auto_resume_pending {
        transport = transport
            .push(halt_auto_button(
                "Cancel",
                false,
                pmsg(Msg::HaltAutoCancelPressed),
            ))
            .push(halt_auto_button(
                "Resume",
                true,
                pmsg(Msg::HaltAutoResumePressed),
            ));
    }

    let time = container(
        text(time_label_text(app))
            .font(iced::Font::MONOSPACE)
            .size(theme::SIZE_CAPTION)
            .color(theme::TEXT_DIM),
    )
    .padding(Padding {
        top: 0.0,
        right: theme::SPACE_XL,
        bottom: 0.0,
        left: theme::SPACE_LG,
    });

    // Track info: 40 px art + title (+ loops-left badge) / artist.
    let thumb_url = current.and_then(|track| track.thumb_url.as_deref());
    let fallback_letter = current.and_then(|track| track.title.chars().next());
    let art = thumbnails::view_thumb(app, thumb_url, theme::NP_THUMB_SIZE, fallback_letter);

    let title = current
        .map(|track| track.title.as_str())
        .unwrap_or("Nothing playing");
    let mut title_row = row![marquee_text(
        title,
        theme::SIZE_BODY,
        if has_track {
            theme::TEXT_BRIGHT
        } else {
            theme::TEXT_DIM
        },
        has_track,
    )]
    .spacing(theme::SPACE_MD)
    .align_y(Alignment::Center);
    if let Some(loops_left) = snapshot.track_repeat_loops_left.filter(|_| has_track) {
        title_row = title_row.push(loops_left_badge(loops_left));
    }

    let artist = current.map(|track| track.artist.as_str()).unwrap_or("");
    let info = row![
        art,
        column![
            title_row,
            marquee_text(artist, theme::SIZE_CAPTION, theme::TEXT_DIM, has_track),
        ]
        .spacing(theme::SPACE_XXS)
        .width(Length::Fill),
    ]
    .spacing(theme::SPACE_LG)
    .align_y(Alignment::Center)
    .width(Length::Fill);

    // Now-playing actions: dislike, like, share, menu.
    let actions = row![
        np_icon_button(
            icons::dislike(),
            18.0,
            has_track.then(|| pmsg(Msg::DislikePressed)),
            rating == SongRating::Dislike,
            theme::SPACE_SM,
        ),
        np_icon_button(
            icons::like(),
            18.0,
            has_track.then(|| pmsg(Msg::LikePressed)),
            rating == SongRating::Like,
            theme::SPACE_SM,
        ),
        with_tooltip(
            np_icon_button(
                icons::share(),
                18.0,
                has_track.then(|| pmsg(Msg::SharePressed)),
                false,
                theme::SPACE_SM,
            ),
            "Copy share URL".to_owned(),
        ),
        np_icon_button(
            icons::menu(),
            18.0,
            has_track.then(|| pmsg(Msg::MenuPressed)),
            false,
            theme::SPACE_SM,
        ),
    ]
    .spacing(theme::SPACE_XXS)
    .align_y(Alignment::Center);

    let volume = row![
        svg(icons::volume())
            .width(Length::Fixed(18.0))
            .height(Length::Fixed(18.0))
            .style(|_t: &Theme, _s| svg::Style {
                color: Some(theme::white(0.5)),
            }),
        slider(0.0..=1.0, app.player.volume.clamp(0.0, 1.0), |value| {
            pmsg(Msg::VolumeChanged(value))
        })
        .step(0.05)
        .width(Length::Fixed(VOLUME_SLIDER_WIDTH))
        .style(volume_slider_style),
    ]
    .spacing(theme::SPACE_XS)
    .align_y(Alignment::Center);

    let repeat_icon = match app.player.repeat_mode {
        RepeatMode::One => icons::repeat_one(),
        _ => icons::repeat(),
    };
    let repeat = np_icon_button(
        repeat_icon,
        18.0,
        Some(pmsg(Msg::RepeatPressed)),
        app.player.repeat_mode != RepeatMode::Off,
        theme::SPACE_SM,
    );

    container(
        row![
            transport,
            time,
            info,
            actions,
            container(volume).padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: theme::SPACE_LG,
            }),
            container(repeat).padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: theme::SPACE_MD,
            }),
        ]
        .spacing(0)
        .align_y(Alignment::Center)
        .width(Length::Fill),
    )
    .height(Length::Fixed(theme::NOW_PLAYING_BAR_HEIGHT))
    .align_y(iced::alignment::Vertical::Center)
    .padding(Padding {
        top: theme::SPACE_XXS,
        right: theme::SPACE_MD,
        bottom: theme::SPACE_SM,
        left: theme::SPACE_MD,
    })
    .into()
}

fn sleep_button(app: &App) -> Element<'_, Message> {
    let snapshot = &app.player.snapshot;
    let engaged = sleep_status_label(snapshot).is_some() || app.player_bar.sleep_menu_open;
    let tip = sleep_status_label(snapshot).unwrap_or_else(|| "Sleep timer".to_owned());
    with_tooltip(
        np_icon_button(
            icons::sleep(),
            18.0,
            Some(pmsg(Msg::SleepPressed)),
            engaged,
            theme::SPACE_SM,
        ),
        tip,
    )
}

/// Inline sleep popover (iced has no floating popovers; rendered as a panel
/// expanding the bar upward, anchored to the transport cluster).
fn sleep_panel(app: &App) -> Element<'_, Message> {
    let parsed = app
        .player_bar
        .sleep_custom_minutes
        .parse::<u32>()
        .ok()
        .filter(|minutes| (SLEEP_CUSTOM_MIN..=SLEEP_CUSTOM_MAX).contains(minutes));

    let mut options = column![sleep_action_button(
        "After this song",
        pmsg(Msg::SleepOptionPicked(SleepOption::AfterCurrent)),
        false,
    )]
    .spacing(theme::SPACE_SM);
    for minutes in [15u32, 30, 45, 60] {
        options = options.push(sleep_action_button_owned(
            format!("{minutes} minutes"),
            pmsg(Msg::SleepOptionPicked(SleepOption::Minutes(minutes))),
            false,
        ));
    }

    let custom = row![
        button(text("−").size(theme::SIZE_SUB))
            .style(theme::secondary_button)
            .padding([theme::SPACE_XS, theme::SPACE_MD])
            .on_press_maybe(parsed.map(|minutes| {
                pmsg(Msg::SleepCustomChanged(
                    minutes
                        .saturating_sub(SLEEP_CUSTOM_STEP)
                        .max(SLEEP_CUSTOM_MIN)
                        .to_string(),
                ))
            })),
        text_input("30", &app.player_bar.sleep_custom_minutes)
            .on_input(|value| pmsg(Msg::SleepCustomChanged(value)))
            .on_submit_maybe(
                parsed.map(|minutes| pmsg(Msg::SleepOptionPicked(SleepOption::Minutes(minutes))))
            )
            .size(theme::SIZE_SUB)
            .width(Length::Fill)
            .style(theme::entry),
        button(text("+").size(theme::SIZE_SUB))
            .style(theme::secondary_button)
            .padding([theme::SPACE_XS, theme::SPACE_MD])
            .on_press_maybe(parsed.map(|minutes| {
                pmsg(Msg::SleepCustomChanged(
                    (minutes + SLEEP_CUSTOM_STEP)
                        .min(SLEEP_CUSTOM_MAX)
                        .to_string(),
                ))
            })),
        button(text("Set").size(theme::SIZE_SUB))
            .style(theme::secondary_button)
            .padding([theme::SPACE_XS, theme::SPACE_LG])
            .on_press_maybe(
                parsed.map(|minutes| pmsg(Msg::SleepOptionPicked(SleepOption::Minutes(minutes))))
            ),
    ]
    .spacing(theme::SPACE_SM)
    .align_y(Alignment::Center);

    container(
        column![
            options,
            custom,
            sleep_action_button(
                "Off",
                pmsg(Msg::SleepOptionPicked(SleepOption::Clear)),
                true
            ),
        ]
        .spacing(theme::SPACE_MD),
    )
    .width(Length::Fixed(SLEEP_PANEL_WIDTH))
    .padding(theme::SPACE_LG)
    .style(theme::popover)
    .into()
}

fn sleep_action_button(
    label: &'static str,
    message: Message,
    danger: bool,
) -> Element<'static, Message> {
    sleep_action_button_owned(label.to_owned(), message, danger)
}

fn sleep_action_button_owned(
    label: String,
    message: Message,
    danger: bool,
) -> Element<'static, Message> {
    let mut body = text(label).size(theme::SIZE_SUB);
    if danger {
        body = body.color(theme::TEXT_DANGER);
    }
    button(body)
        .style(theme::list_row(false))
        .padding([theme::SPACE_SM, theme::SPACE_MD])
        .width(Length::Fill)
        .on_press(message)
        .into()
}

fn halt_auto_button(
    label: &'static str,
    accent: bool,
    message: Message,
) -> Element<'static, Message> {
    let style_fn: fn(&Theme, button::Status) -> button::Style = if accent {
        theme::primary_button
    } else {
        theme::secondary_button
    };
    container(
        button(text(label).size(theme::SIZE_SUB))
            .style(style_fn)
            .padding([theme::SPACE_XS, theme::SPACE_LG])
            .on_press(message),
    )
    .padding(Padding {
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
        left: theme::SPACE_XS,
    })
    .into()
}

/// Flat circular icon button; svg color tracks disabled/engaged state since
/// the svg widget does not inherit the button text color.
fn np_icon_button(
    handle: svg::Handle,
    size: f32,
    on_press: Option<Message>,
    engaged: bool,
    padding: f32,
) -> Element<'static, Message> {
    let enabled = on_press.is_some();
    let color = if !enabled {
        theme::TEXT_GHOST
    } else if engaged {
        theme::ACCENT_FG
    } else {
        theme::TEXT
    };
    let icon = svg(handle)
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_t: &Theme, _s| svg::Style { color: Some(color) });
    let style_fn: fn(&Theme, button::Status) -> button::Style = if engaged {
        theme::icon_button_active
    } else {
        theme::icon_button
    };
    let mut btn = button(icon).style(style_fn).padding(padding);
    if let Some(message) = on_press {
        btn = btn.on_press(message);
    }
    btn.into()
}

/// Marquee approximation (contract §9): single-line clipped text with the full
/// text in a hover tooltip while a track is current.
fn marquee_text(
    value: &str,
    size: f32,
    color: Color,
    tooltip_enabled: bool,
) -> Element<'static, Message> {
    let clipped: Element<'static, Message> = container(
        text(value.to_owned())
            .size(size)
            .color(color)
            .wrapping(text::Wrapping::None),
    )
    .width(Length::Fill)
    .clip(true)
    .into();
    if tooltip_enabled && !value.trim().is_empty() {
        with_tooltip(clipped, value.to_owned())
    } else {
        clipped
    }
}

fn with_tooltip(content: Element<'static, Message>, tip: String) -> Element<'static, Message> {
    tooltip(
        content,
        container(text(tip).size(theme::SIZE_SUB))
            .padding([theme::SPACE_XS, theme::SPACE_MD])
            .style(theme::popover),
        TooltipPosition::Top,
    )
    .gap(theme::SPACE_XS)
    .into()
}

fn loops_left_badge(loops_left: u32) -> Element<'static, Message> {
    let label = if loops_left == 1 { "loop" } else { "loops" };
    container(
        text(format!("{loops_left} {label} left"))
            .size(theme::SIZE_MICRO)
            .color(theme::ACCENT_FG),
    )
    .padding(Padding {
        top: theme::BADGE_PAD_V,
        right: theme::BADGE_PAD_H,
        bottom: theme::BADGE_PAD_V,
        left: theme::BADGE_PAD_H,
    })
    .style(|_t: &Theme| iced::widget::container::Style {
        background: Some(Background::Color(theme::ACCENT_BG_FAINT)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: theme::RADIUS_PILL.into(),
        },
        ..iced::widget::container::Style::default()
    })
    .into()
}

fn time_label_text(app: &App) -> String {
    let snapshot = &app.player.snapshot;
    if let (Some(fraction), Some(duration)) = (app.player.seek_preview, snapshot.duration_millis) {
        let position = ((fraction * duration as f64).round() as u64).min(duration);
        return format!(
            "{} / {}",
            format_clock_time(position),
            format_clock_time(duration)
        );
    }
    match (snapshot.position_millis, snapshot.duration_millis) {
        (Some(pos), Some(dur)) => {
            format!("{} / {}", format_clock_time(pos), format_clock_time(dur))
        }
        (Some(pos), None) => format!("{} / --:--", format_clock_time(pos)),
        (None, Some(dur)) => format!("0:00 / {}", format_clock_time(dur)),
        (None, None) => match app.player.now_playing.as_ref() {
            // Parity §8.18: avoid flicker when a snapshot carries no timing.
            Some(np) if snapshot.current_track.is_some() => {
                format!(
                    "{} / --:--",
                    format_clock_time(np.last_observed_position_millis)
                )
            }
            _ => "0:00 / 0:00".to_owned(),
        },
    }
}

// ---------------------------------------------------------------------------
// Slider styles (private, token-built; styles spec §3.7)
// ---------------------------------------------------------------------------

fn progress_slider_style(_t: &Theme, status: slider::Status) -> slider::Style {
    let engaged = matches!(status, slider::Status::Hovered | slider::Status::Dragged);
    slider::Style {
        rail: slider::Rail {
            // Elapsed is the lone non-accent red; trough+buffer render in the
            // progress bar layered underneath.
            backgrounds: (
                Background::Color(theme::PROGRESS_RED),
                Background::Color(Color::TRANSPARENT),
            ),
            width: if engaged {
                PROGRESS_TROUGH_HOVER
            } else {
                PROGRESS_TROUGH
            },
            border: Border::default(),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle {
                radius: PROGRESS_KNOB_RADIUS,
            },
            // Knob fades in on hover/drag only (GTK: opacity 0 -> 1).
            background: Background::Color(if engaged {
                theme::PROGRESS_RED
            } else {
                Color::TRANSPARENT
            }),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

fn volume_slider_style(_t: &Theme, _status: slider::Status) -> slider::Style {
    slider::Style {
        rail: slider::Rail {
            backgrounds: (
                Background::Color(theme::TEXT_BRIGHT),
                Background::Color(theme::white(0.15)),
            ),
            width: PROGRESS_TROUGH,
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: theme::RADIUS_HAIR.into(),
            },
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle {
                radius: VOLUME_KNOB_RADIUS,
            },
            background: Background::Color(theme::TEXT_BRIGHT),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

fn buffer_bar_style(_t: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(theme::white(0.12)),
        bar: Background::Color(theme::white(0.25)),
        border: Border::default(),
    }
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

pub fn update(app: &mut App, msg: Msg) -> Task<Message> {
    match msg {
        Msg::PlayPausePressed => {
            if app.player.snapshot.current_track.is_none() {
                return Task::none();
            }
            // Optimistic icon flip; the returned snapshot reconciles.
            app.player.snapshot.paused = !app.player.snapshot.paused;
            transport_command(app, false, |daemon| daemon.toggle_pause())
        }
        Msg::NextPressed => transport_command(app, true, |daemon| daemon.next()),
        Msg::PrevPressed => transport_command(app, true, |daemon| daemon.previous()),

        Msg::SeekChanged(fraction) => {
            // No-op when duration is unknown (playback spec §3.3).
            if app.player.snapshot.duration_millis.is_some() {
                app.player.is_seeking = true;
                app.player.seek_preview = Some(fraction.clamp(0.0, 1.0));
            }
            Task::none()
        }
        Msg::SeekReleased => {
            let preview = app.player.seek_preview.take();
            app.player.is_seeking = false;
            let (Some(fraction), Some(duration)) = (preview, app.player.snapshot.duration_millis)
            else {
                return Task::none();
            };
            let target = ((fraction * duration as f64).round() as u64).min(duration);
            // Optimistic position + lyric line; the snapshot reconciles.
            app.player.snapshot.position_millis = Some(target);
            note_playback_position(app, target);
            let follow = queue_panel::sync_active_lyric_line(app, Some(target));
            Task::batch([
                follow,
                transport_command(app, false, move |daemon| daemon.seek_to_millis(target)),
            ])
        }

        Msg::VolumeChanged(value) => {
            let value = value.clamp(0.0, 1.0);
            app.player.volume = value;
            if app.player_bar.volume_send_in_flight {
                app.player_bar.pending_volume = Some(value);
                Task::none()
            } else {
                app.player_bar.volume_send_in_flight = true;
                send_volume_task(app, value)
            }
        }
        Msg::VolumeResult(result) => {
            if let Err(err) = result {
                crate::app_log!("[UI] failed to set volume: {err}");
            }
            if let Some(value) = app.player_bar.pending_volume.take() {
                send_volume_task(app, value)
            } else {
                app.player_bar.volume_send_in_flight = false;
                Task::none()
            }
        }

        Msg::RepeatPressed => {
            let mode = app.player.repeat_mode.next(app.player.is_radio_queue());
            app.player.repeat_mode = mode;
            transport_command(app, false, move |daemon| daemon.set_repeat_mode(mode))
        }

        Msg::LikePressed => {
            let next = if current_rating(app) == SongRating::Like {
                SongRating::Indifferent
            } else {
                SongRating::Like
            };
            submit_song_rating(app, next)
        }
        Msg::DislikePressed => {
            let next = if current_rating(app) == SongRating::Dislike {
                SongRating::Indifferent
            } else {
                SongRating::Dislike
            };
            submit_song_rating(app, next)
        }

        Msg::SharePressed => {
            let daemon = app.player.daemon.clone();
            app::blocking(
                move || daemon.copy_share_url().map_err(|err| err.to_string()),
                |result| pmsg(Msg::ShareCopied(result)),
            )
        }
        Msg::ShareCopied(result) => match result {
            Ok(url) => Task::batch([iced::clipboard::write(url), app.show_toast("Copied!")]),
            Err(err) => app.show_toast(format!("Share failed: {err}")),
        },

        Msg::MenuPressed => {
            let next = match app.queue_panel.mode {
                DetailPanelView::Queue => DetailPanelView::Lyrics,
                DetailPanelView::Lyrics => DetailPanelView::Queue,
            };
            app.queue_panel.mode = next;
            if next == DetailPanelView::Lyrics {
                app.queue_panel.drop_preview_slot = None;
            }
            Task::none()
        }

        Msg::SleepPressed => {
            app.player_bar.sleep_menu_open = !app.player_bar.sleep_menu_open;
            Task::none()
        }
        Msg::SleepCustomChanged(value) => {
            let digits: String = value.chars().filter(char::is_ascii_digit).take(3).collect();
            app.player_bar.sleep_custom_minutes = digits;
            Task::none()
        }
        Msg::SleepOptionPicked(request) => {
            app.player_bar.sleep_menu_open = false;
            let daemon = app.player.daemon.clone();
            let work_request = request.clone();
            app::blocking(
                move || {
                    match work_request {
                        SleepOption::Minutes(minutes) => daemon.set_sleep_timer(minutes),
                        SleepOption::AfterCurrent => daemon.set_sleep_after_current(),
                        SleepOption::Clear => daemon.clear_sleep_timer(),
                    }
                    .map_err(|err| err.to_string())
                },
                move |result| {
                    pmsg(Msg::SleepResult {
                        request: request.clone(),
                        result,
                    })
                },
            )
        }
        Msg::SleepResult { request, result } => match result {
            Ok(snapshot) => {
                let message = match request {
                    SleepOption::Minutes(minutes) => format!("Sleep set for {minutes} minutes"),
                    SleepOption::AfterCurrent => "Sleep after this song".to_owned(),
                    SleepOption::Clear => "Sleep timer off".to_owned(),
                };
                Task::batch([
                    apply_remote_snapshot(app, snapshot),
                    app.show_toast(message),
                ])
            }
            Err(err) => app.show_toast(format!("Sleep failed: {err}")),
        },

        Msg::HaltAutoCancelPressed => {
            transport_command(app, false, |daemon| daemon.cancel_halt_auto_resume())
        }
        Msg::HaltAutoResumePressed => {
            transport_command(app, false, |daemon| daemon.resume_halt_auto())
        }

        Msg::DaemonResult { serial, result } => {
            if !app.player.is_current_play_request(serial) {
                return Task::none(); // superseded by a newer play request
            }
            match result {
                Ok(snapshot) => apply_remote_snapshot(app, snapshot),
                Err(err) => {
                    crate::app_log!("[UI] transport daemon command failed: {err}");
                    Task::none()
                }
            }
        }

        Msg::RatingSynced {
            session_id,
            video_id,
            result,
        } => {
            match result {
                Ok(rating) => apply_song_rating(app, session_id, rating),
                Err(err) => {
                    crate::app_log!("[RATING] failed to fetch rating for {video_id}: {err}")
                }
            }
            Task::none()
        }
        Msg::RatingSubmitted {
            session_id,
            video_id,
            previous,
            result,
        } => {
            if let Err(err) = result {
                crate::app_log!("[RATING] failed to set rating for {video_id}: {err}");
                apply_song_rating(app, session_id, previous);
            }
            Task::none()
        }

        Msg::RecoveryFinished(result) => {
            app.player.daemon_recovery_in_flight = false;
            match result {
                Ok(snapshot) => {
                    crate::app_log!(
                        "[UI] restarted staticd and restored playback from live UI state"
                    );
                    apply_remote_snapshot(app, snapshot)
                }
                Err(err) => {
                    crate::app_log!("[UI] failed to restore staticd from live UI state: {err}");
                    Task::none()
                }
            }
        }
    }
}

/// Run a daemon transport command on a worker; the result is serial-guarded
/// (`DaemonResult`). `bump_serial` marks the command as a new play request
/// (next/prev) which invalidates older in-flight transport results.
fn transport_command<F>(app: &mut App, bump_serial: bool, command: F) -> Task<Message>
where
    F: FnOnce(&DaemonClient) -> anyhow::Result<PlaybackSnapshot> + Send + 'static,
{
    let serial = if bump_serial {
        app.player.next_play_request_serial()
    } else {
        app.player.play_request_serial
    };
    let daemon = app.player.daemon.clone();
    app::blocking(
        move || command(&daemon).map_err(|err| err.to_string()),
        move |result| pmsg(Msg::DaemonResult { serial, result }),
    )
}

fn send_volume_task(app: &App, volume: f64) -> Task<Message> {
    let daemon = app.player.daemon.clone();
    app::blocking_or(
        move || {
            daemon
                .set_volume(volume)
                .map(|_| ())
                .map_err(|err| err.to_string())
        },
        |result| pmsg(Msg::VolumeResult(result)),
        // A panicking worker must still clear `volume_send_in_flight`.
        pmsg(Msg::VolumeResult(Err("volume worker panicked".to_owned()))),
    )
}

// ---------------------------------------------------------------------------
// Ratings
// ---------------------------------------------------------------------------

fn current_rating(app: &App) -> SongRating {
    app.player
        .now_playing
        .as_ref()
        .map(|np| np.rating)
        .unwrap_or(SongRating::Indifferent)
}

/// Session-guarded rating write (skipped when the track changed meanwhile).
fn apply_song_rating(app: &mut App, session_id: u64, rating: SongRating) {
    if let Some(np) = app.player.now_playing.as_mut() {
        if np.session_id == session_id {
            np.rating = rating;
        }
    }
}

/// Optimistic rating submit with rollback (playback spec §3.6). When not
/// logged in, GTK applies then immediately rolls back — net no-op, so the
/// submit is skipped entirely here.
fn submit_song_rating(app: &mut App, desired: SongRating) -> Task<Message> {
    let Some((session_id, video_id, previous)) = app
        .player
        .now_playing
        .as_ref()
        .map(|np| (np.session_id, np.video_id.clone(), np.rating))
    else {
        return Task::none();
    };
    let Some(client) = app.app_client() else {
        return Task::none();
    };

    apply_song_rating(app, session_id, desired);

    let work_video_id = video_id.clone();
    app::blocking(
        move || {
            client
                .rate_song(&work_video_id, desired)
                .map_err(|err| err.to_string())
        },
        move |result| {
            pmsg(Msg::RatingSubmitted {
                session_id,
                video_id: video_id.clone(),
                previous,
                result,
            })
        },
    )
}

/// Background fetch of the actual rating for a freshly-started track.
fn spawn_song_rating_sync(app: &App, video_id: String, session_id: u64) -> Task<Message> {
    let Some(client) = app.app_client() else {
        return Task::none(); // not logged in: silently nothing (parity §5.2)
    };
    let work_video_id = video_id.clone();
    app::blocking(
        move || {
            client
                .get_song_rating(&work_video_id)
                .map_err(|err| err.to_string())
        },
        move |result| {
            pmsg(Msg::RatingSynced {
                session_id,
                video_id: video_id.clone(),
                result,
            })
        },
    )
}

// ---------------------------------------------------------------------------
// Lyrics kicks (state lives in queue_panel; this file owns the triggers)
// ---------------------------------------------------------------------------

/// Bump the lyrics serial, show the loading state and fetch synced lyrics for
/// a freshly-started track (GTK `spawn_synced_lyrics_sync`).
fn spawn_synced_lyrics_sync(app: &mut App, video_id: String, session_id: u64) -> Task<Message> {
    let lyrics = &mut app.queue_panel.lyrics;
    lyrics.request_serial = lyrics.request_serial.wrapping_add(1);
    let serial = lyrics.request_serial;
    lyrics.lines.clear();
    lyrics.meta = None;
    lyrics.active_line = None;

    let Some(client) = app.app_client() else {
        app.queue_panel.lyrics.status = LyricsStatus::NotConnected;
        return Task::none();
    };
    app.queue_panel.lyrics.status = LyricsStatus::Loading;

    let work_video_id = video_id.clone();
    app::blocking(
        move || {
            client
                .get_synced_lyrics(&work_video_id)
                .map_err(|err| err.to_string())
        },
        move |result| {
            Message::Queue(queue_panel::Msg::LyricsLoaded {
                serial,
                session_id,
                video_id: video_id.clone(),
                result,
            })
        },
    )
}

fn reset_lyrics_to_idle(app: &mut App) {
    let lyrics = &mut app.queue_panel.lyrics;
    lyrics.request_serial = lyrics.request_serial.wrapping_add(1);
    lyrics.status = LyricsStatus::Idle;
    lyrics.lines.clear();
    lyrics.meta = None;
    lyrics.active_line = None;
}

fn note_playback_position(app: &mut App, position_millis: u64) {
    if position_millis == 0 {
        return;
    }
    if let Some(np) = app.player.now_playing.as_mut() {
        np.last_observed_position_millis = position_millis;
    }
}

// ---------------------------------------------------------------------------
// Snapshot application (playback spec §4)
// ---------------------------------------------------------------------------

/// Apply a daemon snapshot onto the UI mirror: reconcile queue/index/radio/
/// playlist context/repeat mode/volume, detect track changes (bump
/// `playback_session_id`, kick rating+lyrics fetch tasks,
/// `sidebar::add_to_recent`), update halt-auto/loops-left/sleep state, and
/// drive lyric highlighting from the position. Returns follow-up tasks
/// (thumbnail/rating/lyrics fetches).
pub fn apply_remote_snapshot(app: &mut App, snapshot: PlaybackSnapshot) -> Task<Message> {
    let mut tasks: Vec<Task<Message>> = Vec::new();

    let queue_changed =
        app.player.queue != snapshot.queue || app.player.queue_index != snapshot.queue_index;
    if queue_changed {
        app.player.queue = snapshot.queue.clone();
        app.player.queue_index = snapshot.queue_index;
        let urls: Vec<String> = snapshot
            .queue
            .iter()
            .filter_map(|track| track.thumbnail_url.clone())
            .collect();
        tasks.push(thumbnails::ensure_all(app, urls.iter().map(String::as_str)));
    }

    if app.player.radio_virtual_playlist != snapshot.radio_virtual_playlist {
        let is_radio = snapshot.radio_virtual_playlist.is_some();
        app.player.radio_virtual_playlist = snapshot.radio_virtual_playlist.clone();
        if !is_radio {
            app.player.radio_append_in_flight = false;
            app.player.last_radio_append_anchor = None;
        }
    }

    if app.player.playlist_context != snapshot.playlist_context {
        app.player.playlist_context = snapshot.playlist_context.clone();
    }

    app.player.repeat_mode = snapshot.repeat_mode;

    match snapshot.current_track.as_ref() {
        Some(track) => {
            let is_new_track = app
                .player
                .now_playing
                .as_ref()
                .map(|np| np.video_id.as_str())
                != Some(track.video_id.as_str());
            let persisted_rating = app
                .player
                .now_playing
                .as_ref()
                .filter(|np| np.video_id == track.video_id)
                .map(|np| np.rating)
                .unwrap_or(track.rating);
            let existing_session = app.player.now_playing.as_ref().map(|np| np.session_id);
            let session_id = if is_new_track {
                app.player.playback_session_id = app.player.playback_session_id.wrapping_add(1);
                app.player.playback_session_id
            } else {
                existing_session.unwrap_or_else(|| {
                    app.player.playback_session_id = app.player.playback_session_id.wrapping_add(1);
                    app.player.playback_session_id
                })
            };

            app.player.now_playing = Some(NowPlayingTrack {
                session_id,
                video_id: track.video_id.clone(),
                rating: persisted_rating,
                last_observed_position_millis: snapshot.position_millis.unwrap_or(0),
            });

            if let Some(url) = track.thumb_url.clone() {
                tasks.push(thumbnails::ensure(app, &url));
            }

            if is_new_track {
                if let Some(queue_track) = app
                    .player
                    .queue
                    .iter()
                    .find(|queued| queued.video_id == track.video_id)
                    .cloned()
                {
                    sidebar::add_to_recent(app, &queue_track);
                }
                tasks.push(spawn_song_rating_sync(
                    app,
                    track.video_id.clone(),
                    session_id,
                ));
                tasks.push(spawn_synced_lyrics_sync(
                    app,
                    track.video_id.clone(),
                    session_id,
                ));
            }

            if !app.player.is_seeking {
                tasks.push(queue_panel::sync_active_lyric_line(
                    app,
                    snapshot.position_millis,
                ));
            }
        }
        None => {
            let had_track = app.player.now_playing.take().is_some();
            if had_track {
                reset_lyrics_to_idle(app);
            }
        }
    }

    if (app.player.volume - snapshot.volume).abs() > 0.01 {
        app.player.volume = snapshot.volume;
    }

    // Daemon truth last: the view derives slider/time/buffer/sleep/halt-auto/
    // loops-left/transport state directly from this snapshot.
    app.player.snapshot = snapshot;

    if tasks.is_empty() {
        Task::none()
    } else {
        Task::batch(tasks)
    }
}

// ---------------------------------------------------------------------------
// Reset / persistence / recovery
// ---------------------------------------------------------------------------

/// Reset the bar to the "Nothing playing" state (shell spec §11
/// `reset_player_ui`): stop the daemon, clear contexts, bump lyrics serial,
/// disable all action buttons.
pub fn reset_player_ui(app: &mut App) -> Task<Message> {
    app.player.playlist_like_queue = false;
    app.player.now_playing = None;
    app.player.is_seeking = false;
    app.player.seek_preview = None;
    app.player.snapshot.current_track = None;
    app.player.snapshot.position_millis = None;
    app.player.snapshot.duration_millis = None;
    app.player.snapshot.buffered_fraction = 0.0;
    app.player.snapshot.paused = true;
    app.player.snapshot.halt_auto_resume_pending = false;
    app.player.snapshot.track_repeat_loops_left = None;
    reset_lyrics_to_idle(app);
    transport_command(app, false, |daemon| daemon.stop())
}

/// Build the resume snapshot to persist on window close (shell spec §9 /
/// playback spec §5.6): cheap clones on the UI thread; the disk writes happen
/// in [`write_resume_snapshot`] on the close-path blocking worker.
pub fn resume_persist_payload(app: &App) -> Option<PersistedPlaybackState> {
    let mut snapshot = playback_snapshot_from_ui_state(&app.player);
    let has_recoverable_state = snapshot.current_track.is_some() || !snapshot.queue.is_empty();
    if !has_recoverable_state {
        return None;
    }

    snapshot.track_repeat_loops_left = if snapshot.repeat_mode == RepeatMode::One
        && snapshot.current_track.is_some()
        && app.settings.auto_release_track_loops
    {
        Some(app.settings.auto_release_track_loops_after.max(1))
    } else {
        None
    };

    Some(PersistedPlaybackState {
        snapshot,
        saved_at_unix_millis: current_unix_millis(),
    })
}

/// Disk writes for the close-time resume snapshot: global slot + per-playlist
/// slot when a playlist context exists. Best-effort (errors logged only); runs
/// on the same blocking worker as the close-path unregister/shutdown so a busy
/// playback-state DB cannot stall the event loop.
pub fn write_resume_snapshot(persisted: &PersistedPlaybackState) {
    if let Err(err) = store_persisted_playback_state(persisted) {
        crate::app_log!("[UI] failed to persist resume snapshot while closing: {err}");
    }
    if let Some(playlist_context) = persisted.snapshot.playlist_context.as_ref() {
        if let Err(err) = store_persisted_playlist_state(playlist_context, persisted) {
            crate::app_log!(
                "[UI] failed to persist playlist resume state for {}: {err}",
                playlist_context.playlist_id
            );
        }
    }
}

/// Daemon died mid-session: rebuild a snapshot from live UI state and restart
/// staticd with it (shell spec §8.4). Paused-with-track restores frozen
/// (`restore_queue_at(..., autoplay=false, paused=true, position)`).
pub fn maybe_restore_daemon_from_ui_state(app: &mut App) -> Task<Message> {
    if app.player.daemon_recovery_in_flight {
        return Task::none();
    }

    let snapshot = playback_snapshot_from_ui_state(&app.player);
    let has_recoverable_state = snapshot.current_track.is_some() || !snapshot.queue.is_empty();
    if !has_recoverable_state {
        return Task::none();
    }

    let resume_playback = snapshot.current_track.is_some();
    app.player.daemon_recovery_in_flight = true;

    app::blocking_or(
        move || {
            let restored = if snapshot.paused && snapshot.current_track.is_some() {
                restore_paused_playback_snapshot_via_daemon(snapshot)
            } else {
                restore_playback_snapshot_via_daemon(snapshot, resume_playback)
            };
            restored.map_err(|err| err.to_string())
        },
        |result| pmsg(Msg::RecoveryFinished(result)),
        // A panicking restore must still clear `daemon_recovery_in_flight`,
        // otherwise every future poll snapshot would be dropped (GTK cleared
        // the flag before matching on the join result).
        pmsg(Msg::RecoveryFinished(Err(
            "daemon recovery worker panicked".to_owned(),
        ))),
    )
}

/// Merge the live UI mirror over the cached daemon snapshot (GTK
/// `playback_snapshot_from_ui_state`): probe snapshots become paused regular
/// snapshots at the pre-probe position; the last observed position wins when
/// known; a current track is synthesized from the queue when missing.
fn playback_snapshot_from_ui_state(player: &PlayerModel) -> PlaybackSnapshot {
    let mut snapshot = player.snapshot.clone();
    snapshot.halt_auto_resume_pending = false;

    let use_live_ui_queue = !player.queue.is_empty();
    if use_live_ui_queue {
        snapshot.queue = player.queue.clone();
        snapshot.queue_index = player
            .queue_index
            .min(snapshot.queue.len().saturating_sub(1));
        snapshot.playlist_context = player.playlist_context.clone();
        snapshot.radio_virtual_playlist = player.radio_virtual_playlist.clone();
        snapshot.repeat_mode = player.repeat_mode;
        if snapshot.repeat_mode != RepeatMode::One {
            snapshot.track_repeat_loops_left = None;
        }
    } else {
        snapshot.queue_index = snapshot
            .queue_index
            .min(snapshot.queue.len().saturating_sub(1));
    }

    snapshot.volume = player.volume;

    if snapshot.probe_active {
        snapshot.current_track = snapshot
            .queue
            .get(snapshot.queue_index)
            .map(active_track_snapshot_from_track);
        snapshot.position_millis = snapshot.probe_restore_position_millis;
        snapshot.duration_millis = None;
        snapshot.buffered_fraction = 0.0;
        snapshot.paused = true;
        snapshot.probe_active = false;
        snapshot.probe_restore_position_millis = None;
    } else {
        snapshot.position_millis = player
            .now_playing
            .as_ref()
            .map(|np| np.last_observed_position_millis)
            .filter(|position| *position > 0)
            .or(snapshot.position_millis);
    }

    if snapshot.current_track.is_none() {
        snapshot.current_track = snapshot
            .queue
            .get(snapshot.queue_index)
            .map(active_track_snapshot_from_track);
    }

    snapshot
}

fn active_track_snapshot_from_track(track: &Track) -> ActiveTrackSnapshot {
    ActiveTrackSnapshot {
        video_id: track.video_id.clone(),
        title: track.title.clone(),
        artist: artist_string(track),
        thumb_url: track.thumbnail_url.clone(),
        rating: SongRating::Indifferent,
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

fn restore_playback_snapshot_via_daemon(
    snapshot: PlaybackSnapshot,
    resume_playback: bool,
) -> anyhow::Result<PlaybackSnapshot> {
    crate::ipc::ensure_daemon_running()?;
    let client = DaemonClient::new()?;
    let queue_index = snapshot
        .queue_index
        .min(snapshot.queue.len().saturating_sub(1));

    let mut restored = client.set_volume(snapshot.volume)?;
    if snapshot.queue.is_empty() {
        return Ok(restored);
    }

    restored = if resume_playback {
        client.play_queue_at(
            snapshot.queue.clone(),
            queue_index,
            snapshot.playlist_context.clone(),
            snapshot.radio_virtual_playlist.clone(),
            snapshot.repeat_mode,
        )?
    } else {
        client.replace_queue(
            snapshot.queue.clone(),
            queue_index,
            snapshot.playlist_context.clone(),
            snapshot.radio_virtual_playlist.clone(),
            snapshot.repeat_mode,
        )?
    };

    if resume_playback {
        if let Some(position_millis) = snapshot.position_millis.filter(|position| *position > 0) {
            restored = client.seek_to_millis(position_millis)?;
        }
        if snapshot.paused {
            restored = client.pause()?;
        } else if restored.paused {
            restored = client.resume()?;
        }
    }

    Ok(restored)
}

fn restore_paused_playback_snapshot_via_daemon(
    snapshot: PlaybackSnapshot,
) -> anyhow::Result<PlaybackSnapshot> {
    crate::ipc::ensure_daemon_running()?;
    let client = DaemonClient::new()?;
    let queue_index = snapshot
        .queue_index
        .min(snapshot.queue.len().saturating_sub(1));

    let restored = client.set_volume(snapshot.volume)?;
    if snapshot.queue.is_empty() {
        return Ok(restored);
    }

    client.restore_queue_at(
        snapshot.queue.clone(),
        queue_index,
        snapshot.playlist_context.clone(),
        snapshot.radio_virtual_playlist.clone(),
        snapshot.repeat_mode,
        false,
        true,
        snapshot.position_millis,
    )
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format milliseconds as `M:SS` (seconds zero-padded, minutes not).
pub fn format_clock_time(millis: u64) -> String {
    let total_secs = millis / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}:{secs:02}")
}

/// "Sleep after this song" / "Sleep in M:SS" (H:MM:SS when ≥ 1 h); None when
/// no sleep state is active.
fn sleep_status_label(snapshot: &PlaybackSnapshot) -> Option<String> {
    if snapshot.sleep_after_current {
        Some("Sleep after this song".to_owned())
    } else {
        snapshot
            .sleep_remaining_secs
            .map(|secs| format!("Sleep in {}", format_sleep_secs(secs)))
    }
}

fn format_sleep_secs(total_seconds: u64) -> String {
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3600;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
