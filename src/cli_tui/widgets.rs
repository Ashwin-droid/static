use super::*;
use super::{page_builders::*, views::*};

pub(in crate::cli_tui) struct HomeFeedWidget {
    pub(in crate::cli_tui) doc: HomeDocument,
    pub(in crate::cli_tui) scroll_offset: usize,
    pub(in crate::cli_tui) selected_index: usize,
    pub(in crate::cli_tui) focus: bool,
    pub(in crate::cli_tui) theme: TuiTheme,
    pub(in crate::cli_tui) show_thumbnail_wells: bool,
    pub(in crate::cli_tui) current_video_id: Option<String>,
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
pub(in crate::cli_tui) fn render_action_button(
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

pub(in crate::cli_tui) fn render_cover_placeholder(
    frame: &mut Frame<'_>,
    area: Rect,
    initials: String,
    theme: &TuiTheme,
) {
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

pub(in crate::cli_tui) fn render_home_thumbnail_fallback(
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

pub(in crate::cli_tui) fn draw_home_banner(
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

pub(in crate::cli_tui) fn draw_home_card(
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

    fill_clipped_area_signed(buf, clip, x, y, width, height, Style::default().bg(bg));
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

pub(in crate::cli_tui) fn draw_home_thumbnail_well(
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

pub(in crate::cli_tui) fn render_queue_drop_indicator(
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

pub(in crate::cli_tui) fn draw_box(
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

pub(in crate::cli_tui) fn fill_area(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buf[(x, y)].set_style(style).set_symbol(" ");
        }
    }
}

pub(in crate::cli_tui) fn fill_clipped_area(
    buf: &mut Buffer,
    clip: Rect,
    area: Rect,
    style: Style,
) {
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

pub(in crate::cli_tui) fn fill_clipped_area_signed(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    y: i32,
    width: u16,
    height: u16,
    style: Style,
) {
    let bottom = y.saturating_add(height as i32);
    let start_y = y.max(clip.y as i32);
    let end_y = bottom.min(clip.bottom() as i32);
    if start_y >= end_y {
        return;
    }

    fill_clipped_area(
        buf,
        clip,
        Rect::new(x, start_y as u16, width, (end_y - start_y) as u16),
        style,
    );
}

pub(in crate::cli_tui) fn write_clipped(
    buf: &mut Buffer,
    clip: Rect,
    x: u16,
    y: u16,
    text: &str,
    style: Style,
) {
    if y < clip.y || y >= clip.bottom() || x >= clip.right() {
        return;
    }

    let max_width = clip.right().saturating_sub(x) as usize;
    let rendered = shorten(text, max_width);
    buf.set_stringn(x, y, rendered, max_width, style);
}

pub(in crate::cli_tui) fn home_card_copy(
    item: &TuiHomeItem,
    style: HomeCardStyle,
) -> (String, String, String, String) {
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
        TuiHomeItem::Playlist(card) => (
            card.summary.title.clone(),
            card.summary
                .subtitle
                .clone()
                .unwrap_or_else(|| playlist_source_label(card.source).to_string()),
            "Open collection".to_string(),
            playlist_source_label(card.source).to_string(),
        ),
    }
}

pub(in crate::cli_tui) fn home_card_thumbnail_url(card: &HomeCard) -> Option<&str> {
    match &card.item {
        TuiHomeItem::Track(track) => track.thumbnail_url.as_deref(),
        TuiHomeItem::Browse(card) => card.thumbnail_url.as_deref(),
        TuiHomeItem::Playlist(card) => card.summary.thumbnail_url.as_deref(),
    }
}

pub(in crate::cli_tui) fn home_banner_thumbnail_width(width: u16, height: u16) -> u16 {
    if width < 36 || height < 5 {
        return 0;
    }

    let preferred = height.saturating_sub(2).min(12);
    let max_width = width.saturating_sub(22);
    preferred.min(max_width)
}

pub(in crate::cli_tui) fn home_banner_thumbnail_rect(screen_rect: Rect) -> Option<Rect> {
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

pub(in crate::cli_tui) fn home_card_thumbnail_width(width: u16, style: HomeCardStyle) -> u16 {
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

pub(in crate::cli_tui) fn home_card_thumbnail_rect(
    screen_rect: Rect,
    style: HomeCardStyle,
) -> Option<Rect> {
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

pub(in crate::cli_tui) fn placeholder_initials(primary: &str, secondary: &str) -> String {
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

pub(in crate::cli_tui) fn current_initials(snapshot: &PlaybackSnapshot) -> String {
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

pub(in crate::cli_tui) fn move_index(current: usize, len: usize, delta: isize) -> usize {
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

pub(in crate::cli_tui) fn shorten(value: &str, width: usize) -> String {
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

pub(in crate::cli_tui) fn contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

pub(in crate::cli_tui) fn intersect_rect(a: Rect, b: Rect) -> Option<Rect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = a.right().min(b.right());
    let bottom = a.bottom().min(b.bottom());
    if left >= right || top >= bottom {
        return None;
    }

    Some(Rect::new(left, top, right - left, bottom - top))
}

pub(in crate::cli_tui) fn scrolled_rect(
    viewport: Rect,
    logical_rect: Rect,
    scroll_offset: usize,
) -> Option<Rect> {
    let left = viewport.x as i32 + logical_rect.x as i32;
    let right = left + logical_rect.width as i32;
    let top = viewport.y as i32 + logical_rect.y as i32 - scroll_offset as i32;
    let bottom = top + logical_rect.height as i32;

    if left >= u16::MAX as i32 || top >= u16::MAX as i32 || right <= 0 || bottom <= 0 {
        return None;
    }

    let screen_left = left.max(0) as u16;
    let screen_top = top.max(0) as u16;
    let screen_right = right.min(u16::MAX as i32) as u16;
    let screen_bottom = bottom.min(u16::MAX as i32) as u16;
    if screen_left >= screen_right || screen_top >= screen_bottom {
        return None;
    }

    Some(Rect::new(
        screen_left,
        screen_top,
        screen_right - screen_left,
        screen_bottom - screen_top,
    ))
}

pub(in crate::cli_tui) fn scrolled_visible_rect(
    viewport: Rect,
    logical_rect: Rect,
    scroll_offset: usize,
) -> Option<Rect> {
    scrolled_rect(viewport, logical_rect, scroll_offset)
        .and_then(|screen_rect| intersect_rect(screen_rect, viewport))
}

pub(in crate::cli_tui) fn text_cell_width(value: &str) -> usize {
    value.chars().count()
}

pub(in crate::cli_tui) fn home_header_actions_layout(
    area: Rect,
    header: &HomeHeader,
) -> Option<HomeHeaderActionsLayout> {
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
