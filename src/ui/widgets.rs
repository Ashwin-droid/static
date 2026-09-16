//! Small shared visual components built purely from `theme` tokens.
//!
//! ARCHITECT-OWNED and complete — implementers must not edit this file (report
//! needed additions instead; see the migration contract). Anything
//! area-specific (track rows, cover cards, queue rows, settings cards) is
//! built privately inside the owning area file using these primitives.

use iced::widget::{Space, button, container, mouse_area, rich_text, row, span, svg, text};
use iced::{Color, Element, Length};

use crate::models::Track;

use super::app::{ArtistLink, Message};
use super::theme;

/// 1px horizontal hairline (replaces GTK `Separator`).
pub fn separator() -> Element<'static, Message> {
    container(Space::new().width(Length::Fill).height(Length::Fixed(1.0)))
        .style(theme::hairline)
        .into()
}

/// 1px vertical hairline.
#[allow(dead_code)] // shared design-system helper kept for future views
pub fn vseparator() -> Element<'static, Message> {
    container(Space::new().width(Length::Fixed(1.0)).height(Length::Fill))
        .style(theme::hairline)
        .into()
}

/// The "S  T  A  T  I  C" content header bar (shell spec §5).
pub fn content_header() -> Element<'static, Message> {
    container(
        text("S  T  A  T  I  C")
            .size(theme::SIZE_HEADER)
            .font(theme::FONT_HEAVY)
            .color(theme::white(0.5)),
    )
    .width(Length::Fill)
    .align_x(iced::alignment::Horizontal::Center)
    .padding([theme::SPACE_LG, 0.0])
    .into()
}

/// Pill toast body (positioning is done by the shell overlay in `app.rs`).
pub fn toast(message: &str) -> Element<'_, Message> {
    container(text(message).size(theme::SIZE_SUB).font(theme::FONT_BOLD))
        .style(theme::toast)
        .padding([theme::SPACE_MD, theme::SPACE_XL])
        .into()
}

/// Section header label ("Listen again", "Results for ...", card titles).
pub fn section_header(label: &str) -> Element<'_, Message> {
    text(label)
        .size(theme::SIZE_SECTION)
        .font(theme::FONT_BOLD)
        .color(theme::TEXT_BRIGHT)
        .into()
}

/// Dimmed status/empty/loading label.
pub fn dim_label(label: &str) -> Element<'_, Message> {
    text(label)
        .size(theme::SIZE_BODY)
        .color(theme::TEXT_DIM)
        .into()
}

/// A bare icon at the given square size.
pub fn icon(handle: svg::Handle, size: f32) -> Element<'static, Message> {
    svg(handle)
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .into()
}

/// One-line artist caption. Every artist with a browse id is an underlined
/// link that opens its artist or channel page; the rest is plain text. Links are
/// padding-less buttons so the enclosing row/card (a `mouse_area` drag
/// source) sees neither the press nor the release of a link click.
pub fn artist_line(track: &Track, size: f32, color: Color) -> Element<'static, Message> {
    let plain = |label: String| {
        text(label)
            .size(size)
            .color(color)
            .wrapping(text::Wrapping::None)
    };
    let mut line = row![].align_y(iced::Alignment::Center);
    if track.artists.is_empty() {
        line = line.push(plain("Unknown artist".to_owned()));
    }
    for (index, artist) in track.artists.iter().enumerate() {
        if index > 0 {
            line = line.push(plain(", ".to_owned()));
        }
        match &artist.id {
            Some(browse_id) => {
                let name = rich_text::<(), Message, iced::Theme, iced::Renderer>([span(
                    artist.name.clone(),
                )
                .underline(true)])
                .size(size)
                .wrapping(text::Wrapping::None);
                line = line.push(
                    button(name)
                        .style(theme::link_text(color))
                        .padding(0)
                        .on_press(Message::ArtistLinkClicked(ArtistLink {
                            browse_id: browse_id.clone(),
                            name: artist.name.clone(),
                            page_type: artist
                                .page_type
                                .unwrap_or(crate::models::HomeBrowsePageType::Artist),
                        })),
                );
            }
            None => line = line.push(plain(artist.name.clone())),
        }
    }
    container(line).width(Length::Fill).clip(true).into()
}

/// Messages a `drag_control` emits (its press registers a queue drag, so the
/// click lands on release and hover is tracked by the owning view).
pub struct DragControlEvents {
    pub press: Message,
    pub release: Message,
    pub enter: Message,
    pub exit: Message,
}

/// Icon + label control styled like `secondary_button` but built on a
/// `mouse_area` so it doubles as a queue drag source (Play All / Shuffle).
pub fn drag_control(
    handle: svg::Handle,
    icon_size: f32,
    label: &'static str,
    hovered: bool,
    events: DragControlEvents,
) -> Element<'static, Message> {
    let content = row![icon(handle, icon_size), text(label).size(theme::SIZE_BODY)]
        .spacing(theme::SPACE_SM)
        .align_y(iced::Alignment::Center);
    mouse_area(
        container(content)
            .padding([theme::SPACE_SM, theme::SPACE_LG])
            .style(theme::secondary_surface(hovered)),
    )
    .interaction(iced::mouse::Interaction::Pointer)
    .on_press(events.press)
    .on_release(events.release)
    .on_enter(events.enter)
    .on_exit(events.exit)
    .into()
}

/// Circular transparent icon button. `on_press: None` renders it disabled —
/// the resting state of every transport/action button until a snapshot
/// provides a track (shell spec §5.3).
#[allow(dead_code)] // shared design-system helper kept for future views
pub fn icon_button(
    handle: svg::Handle,
    size: f32,
    on_press: Option<Message>,
) -> Element<'static, Message> {
    let mut btn = button(icon(handle, size))
        .style(theme::icon_button)
        .padding(theme::SPACE_MD);
    if let Some(message) = on_press {
        btn = btn.on_press(message);
    }
    btn.into()
}

/// Icon button carrying the accent "engaged" state (active repeat, liked...).
#[allow(dead_code)] // shared design-system helper kept for future views
pub fn icon_button_active(
    handle: svg::Handle,
    size: f32,
    on_press: Option<Message>,
) -> Element<'static, Message> {
    let mut btn = button(icon(handle, size))
        .style(theme::icon_button_active)
        .padding(theme::SPACE_MD);
    if let Some(message) = on_press {
        btn = btn.on_press(message);
    }
    btn.into()
}
