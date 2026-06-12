//! Thumbnail fetching + caching.
//!
//! Spec: playback.md §5.1, shell.md §3.9.
//! GTK source: src/ui/window/thumbnails.rs (`load_thumbnail_into`).
//!
//! Design (CONTRACT.md §8): a single in-memory cache of decoded
//! `iced::widget::image::Handle`s keyed by URL, owned by `App.thumbnails`.
//! Requests are issued from UPDATE code only (views are read-only): when a
//! data set loads (home page, queue snapshot, search results...), the owning
//! update batches `ensure(...)` tasks for the URLs it is about to render.
//! `view_thumb` renders the cached handle, or a letter-fallback placeholder
//! (styles spec: `.cover-art-letter` / `.track-art-letter`) while missing.
//! Bytes are fetched via `crate::network::fetch_thumbnail_bytes` (which has
//! its own process-wide byte cache) inside `app::blocking`.

use std::collections::HashMap;

use iced::gradient;
use iced::widget::{container, image, text};
use iced::{Background, Border, ContentFit, Degrees, Element, Font, Gradient, Task, Theme};

use super::app::{self, App, Message};
use super::theme;

const LARGE_ART_MIN: f32 = 100.0;
const NP_ART_MAX: f32 = 40.0;

const COVER_LETTER_FONT: Font = Font {
    weight: iced::font::Weight::ExtraBold,
    ..Font::DEFAULT
};
const TRACK_LETTER_FONT: Font = Font {
    weight: iced::font::Weight::Bold,
    ..Font::DEFAULT
};

pub enum Entry {
    Loading,
    Loaded(image::Handle),
    Failed,
}

#[derive(Default)]
pub struct Cache {
    pub entries: HashMap<String, Entry>,
}

impl Cache {
    pub fn handle(&self, url: &str) -> Option<image::Handle> {
        match self.entries.get(url) {
            Some(Entry::Loaded(handle)) => Some(handle.clone()),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub enum Msg {
    Fetched {
        url: String,
        result: Option<Vec<u8>>,
    },
}

/// Start fetching `url` unless it is already cached/loading. Returns the
/// fetch task (or `Task::none()` when nothing to do). A previously `Failed`
/// URL is retried (GTK parity: every rebuild re-attempted failed loads).
pub fn ensure(app: &mut App, url: &str) -> Task<Message> {
    if url.is_empty() {
        return Task::none();
    }
    match app.thumbnails.entries.get(url) {
        Some(Entry::Loading) | Some(Entry::Loaded(_)) => return Task::none(),
        Some(Entry::Failed) | None => {}
    }
    app.thumbnails
        .entries
        .insert(url.to_string(), Entry::Loading);

    let fetch_url = url.to_string();
    let msg_url = url.to_string();
    app::blocking(
        move || {
            let bytes = if let Some(path) = fetch_url.strip_prefix("file://") {
                std::fs::read(path).ok()?
            } else {
                crate::network::fetch_thumbnail_bytes(&fetch_url)?
            };
            ::image::guess_format(&bytes).ok()?;
            Some(bytes)
        },
        move |result| {
            Message::Thumbnail(Msg::Fetched {
                url: msg_url.clone(),
                result,
            })
        },
    )
}

/// Batch `ensure` over every thumbnail URL in an iterator (convenience for
/// loaders that just rendered a page of items).
pub fn ensure_all<'a>(app: &mut App, urls: impl Iterator<Item = &'a str>) -> Task<Message> {
    let tasks: Vec<Task<Message>> = urls
        .map(|url| ensure(app, url))
        .filter(|task| task.units() > 0)
        .collect();
    if tasks.is_empty() {
        Task::none()
    } else {
        Task::batch(tasks)
    }
}

pub fn update(app: &mut App, msg: Msg) -> Task<Message> {
    match msg {
        Msg::Fetched { url, result } => {
            let entry = match result {
                Some(bytes) => Entry::Loaded(image::Handle::from_bytes(bytes)),
                None => Entry::Failed,
            };
            app.thumbnails.entries.insert(url, entry);
            Task::none()
        }
    }
}

/// Render a square thumbnail at `size` px with the token radius, or a
/// letter-fallback placeholder when the URL is missing/unloaded.
pub fn view_thumb<'a>(
    app: &'a App,
    url: Option<&'a str>,
    size: f32,
    fallback_letter: Option<char>,
) -> Element<'a, Message> {
    if let Some(handle) = url.and_then(|u| app.thumbnails.handle(u)) {
        return image(handle)
            .width(size)
            .height(size)
            .content_fit(ContentFit::Cover)
            .border_radius(art_radius(size))
            .into();
    }

    let letter = fallback_letter
        .map(|c| c.to_uppercase().collect::<String>())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "♫".to_string());
    let (letter_size, letter_color, letter_font) = if size >= LARGE_ART_MIN {
        (theme::SIZE_HERO, theme::white(0.10), COVER_LETTER_FONT)
    } else {
        (theme::SIZE_SECTION, theme::white(0.12), TRACK_LETTER_FONT)
    };

    container(
        text(letter)
            .size(letter_size)
            .font(letter_font)
            .color(letter_color),
    )
    .center(size)
    .style(art_placeholder(size))
    .into()
}

fn art_radius(size: f32) -> f32 {
    if size >= LARGE_ART_MIN {
        theme::RADIUS_CARD
    } else if size >= 48.0 {
        theme::RADIUS_LG
    } else if size > NP_ART_MAX {
        theme::RADIUS_MD
    } else {
        theme::RADIUS_XS
    }
}

fn art_placeholder(size: f32) -> impl Fn(&Theme) -> container::Style {
    let radius = art_radius(size);
    move |_t| {
        let background = if size <= NP_ART_MAX {
            Background::Color(theme::white(0.05))
        } else {
            Background::Gradient(Gradient::Linear(
                gradient::Linear::new(Degrees(160.0))
                    .add_stop(0.0, theme::ACCENT_BG_GHOST)
                    .add_stop(1.0, theme::BG_CARD),
            ))
        };
        container::Style {
            background: Some(background),
            border: Border {
                color: theme::HAIRLINE,
                width: 1.0,
                radius: radius.into(),
            },
            ..container::Style::default()
        }
    }
}
