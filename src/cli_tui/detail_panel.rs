use super::*;
use super::{playback::*, widgets::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn render_detail_panel(
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
                Constraint::Min(0),
                Constraint::Length(15),
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

        let can_dismiss = can_dismiss_queue(&self.snapshot);
        let dismiss_area = tab_columns[3];
        let dismiss_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if can_dismiss {
                theme.border_focus
            } else {
                theme.border_subtle
            }))
            .style(Style::default().bg(theme.elevated_bg));
        let dismiss_inner = dismiss_block.inner(dismiss_area);
        frame.render_widget(dismiss_block, dismiss_area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Dismiss Queue",
                Style::default().fg(if can_dismiss { theme.text } else { theme.muted }),
            )))
            .alignment(Alignment::Center),
            dismiss_inner,
        );
        if can_dismiss {
            targets.push(ClickTarget {
                rect: dismiss_area,
                kind: ClickTargetKind::Action(TuiAction::DismissQueue),
            });
        }

        match self.detail_tab {
            DetailTab::Queue => {
                let body_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(6)])
                    .split(rows[1]);

                let chip_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.border_subtle))
                    .style(Style::default().bg(theme.elevated_alt_bg));
                let chip_inner = chip_block.inner(body_rows[0]);
                frame.render_widget(chip_block, body_rows[0]);
                layout.radio_chip_viewport = body_rows[0];

                let chips = self.current_radio_chips();
                if chips.is_empty() {
                    self.radio_chip_scroll_offset = 0;
                    frame.render_widget(
                        Paragraph::new(current_context_label(&self.snapshot))
                            .style(Style::default().fg(theme.muted)),
                        chip_inner,
                    );
                } else {
                    let selected_chip_index = self
                        .snapshot
                        .radio_virtual_playlist
                        .as_ref()
                        .and_then(|radio| radio.selected_chip_index);
                    let start = self
                        .radio_chip_scroll_offset
                        .min(chips.len().saturating_sub(1));
                    self.radio_chip_scroll_offset = start;
                    let hidden_before = start > 0;
                    let left_gutter_width = if hidden_before && chip_inner.width > 2 {
                        chip_inner.width.min(2)
                    } else {
                        0
                    };
                    let mut next_x = chip_inner.x.saturating_add(left_gutter_width);
                    let mut visible_chips = Vec::new();
                    let mut has_hidden_after = false;
                    for (index, chip) in chips.iter().enumerate().skip(start) {
                        let label = format!(" {} ", chip.label);
                        let width = label.chars().count() as u16;
                        if width == 0 {
                            continue;
                        }
                        if next_x.saturating_add(width) > chip_inner.right() {
                            has_hidden_after = true;
                            break;
                        }
                        visible_chips.push((
                            index,
                            label,
                            Rect::new(next_x, chip_inner.y, width, chip_inner.height),
                        ));
                        next_x = next_x.saturating_add(width + 1);
                    }

                    if left_gutter_width > 0 {
                        let scroll_left_rect = Rect::new(
                            chip_inner.x,
                            chip_inner.y,
                            left_gutter_width,
                            chip_inner.height,
                        );
                        frame.render_widget(
                            Paragraph::new("‹")
                                .style(Style::default().fg(theme.accent))
                                .alignment(Alignment::Left),
                            scroll_left_rect,
                        );
                        targets.push(ClickTarget {
                            rect: scroll_left_rect,
                            kind: ClickTargetKind::RadioChipScroll(-1),
                        });
                    }

                    if has_hidden_after && chip_inner.width > 1 {
                        let right_gutter_width = chip_inner.width.min(2);
                        let scroll_right_rect = Rect::new(
                            chip_inner.right().saturating_sub(right_gutter_width),
                            chip_inner.y,
                            right_gutter_width,
                            chip_inner.height,
                        );
                        while visible_chips
                            .last()
                            .is_some_and(|(_, _, rect)| rect.right() > scroll_right_rect.x)
                        {
                            visible_chips.pop();
                        }
                        frame.render_widget(
                            Paragraph::new("›")
                                .style(Style::default().fg(theme.accent))
                                .alignment(Alignment::Right),
                            scroll_right_rect,
                        );
                        targets.push(ClickTarget {
                            rect: scroll_right_rect,
                            kind: ClickTargetKind::RadioChipScroll(1),
                        });
                    }

                    for (index, label, rect) in visible_chips {
                        let style = if selected_chip_index == Some(index) {
                            Style::default().fg(theme.text).bg(theme.accent_fill)
                        } else {
                            Style::default().fg(theme.text).bg(theme.elevated_bg)
                        };
                        frame.render_widget(Paragraph::new(label).style(style), rect);
                        targets.push(ClickTarget {
                            rect,
                            kind: ClickTargetKind::RadioChip(index),
                        });
                    }
                }

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
}
