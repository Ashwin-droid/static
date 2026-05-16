use super::*;
use super::{page_builders::*, playback::*, widgets::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn render_help_popup(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
    ) -> Option<Rect> {
        if !self.help_visible {
            return None;
        }

        let sections = help_shortcut_sections();
        let label_width = sections
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .map(|(keys, _)| keys.chars().count())
            .max()
            .unwrap_or(6);
        let desc_width = sections
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .map(|(_, desc)| desc.chars().count())
            .max()
            .unwrap_or(12);
        let body_width = (label_width + 2 + desc_width) as u16;
        let title_width = "Keyboard shortcuts".chars().count() as u16;
        let width = body_width
            .max(title_width)
            .saturating_add(6)
            .clamp(32, area.width.saturating_sub(4).max(32));

        let mut body_rows: u16 = 0;
        for (index, (_, rows)) in sections.iter().enumerate() {
            if index > 0 {
                body_rows = body_rows.saturating_add(1);
            }
            body_rows = body_rows.saturating_add(1 + rows.len() as u16);
        }
        let height = (body_rows + 6).clamp(10, area.height.saturating_sub(2).max(10));
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let popup_rect = Rect::new(x, y, width, height);

        frame.render_widget(Clear, popup_rect);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.border_focus))
                .style(Style::default().bg(theme.elevated_alt_bg)),
            popup_rect,
        );

        let inner = Rect::new(
            popup_rect.x.saturating_add(2),
            popup_rect.y.saturating_add(1),
            popup_rect.width.saturating_sub(4),
            popup_rect.height.saturating_sub(2),
        );

        let title_line = Line::from(Span::styled(
            "Keyboard shortcuts",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ));
        let subtitle_line = Line::from(Span::styled(
            format!("Thumbnails: {}", self.thumbnail_mode_label),
            Style::default().fg(theme.muted),
        ));

        let mut lines = vec![title_line, subtitle_line, Line::from("")];
        for (section_index, (heading, rows)) in sections.iter().enumerate() {
            if section_index > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                (*heading).to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            for (keys, desc) in rows.iter() {
                let padding = label_width.saturating_sub(keys.chars().count());
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{}{}", keys, " ".repeat(padding)),
                        Style::default()
                            .fg(theme.accent_glow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled((*desc).to_string(), Style::default().fg(theme.text)),
                ]));
            }
        }

        let footer_height = 2u16;
        let body_area = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(footer_height),
        );
        frame.render_widget(Paragraph::new(lines), body_area);

        let footer = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new("h or esc close").style(Style::default().fg(theme.border_focus)),
            footer,
        );

        Some(popup_rect)
    }

    pub(in crate::cli_tui) fn render_frame(
        &mut self,
        frame: &mut Frame<'_>,
    ) -> (Vec<ClickTarget>, RenderedLayout) {
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
        self.rendered_help_rect = self.render_help_popup(frame, area, &theme);

        (click_targets, layout)
    }

    pub(in crate::cli_tui) fn render_sidebar(
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

        let show_suggestions = self.search_input_active
            && (self.search_suggestions_loading || !self.search_suggestions.is_empty());
        let suggestion_items = self.search_suggestions.len().min(6) as u16;
        let suggestion_height = if show_suggestions {
            suggestion_items.max(1).saturating_add(2)
        } else {
            0
        };
        let search_height = 5u16.saturating_add(suggestion_height);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(search_height),
                Constraint::Length(6),
                Constraint::Length(2),
                Constraint::Min(8),
                Constraint::Length(3),
            ])
            .split(inner);
        let search_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Length(suggestion_height)])
            .split(rows[0]);

        let search_active = self.search_input_active;
        let search_box = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if search_active {
                theme.border_focus
            } else {
                theme.border_subtle
            }))
            .style(Style::default().bg(theme.elevated_bg))
            .padding(Padding::new(1, 1, 0, 0));
        let search_inner = search_box.inner(search_rows[0]);
        frame.render_widget(search_box, search_rows[0]);
        let search_prompt = if self.search_query.trim().is_empty() {
            Span::styled(
                "Search songs, artists, albums...",
                Style::default().fg(theme.muted),
            )
        } else {
            Span::styled(self.search_query.as_str(), Style::default().fg(theme.text))
        };
        let search_hint = if search_active {
            if !self.search_suggestions.is_empty() {
                "↑↓ choose • Tab fill • Enter search"
            } else if self.search_suggestions_loading {
                "Fetching suggestions..."
            } else {
                "Enter to search • Esc to cancel"
            }
        } else {
            "Press / or click here"
        };
        let search_box = Block::default().style(Style::default().bg(theme.elevated_bg));
        frame.render_widget(search_box, search_inner);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(search_prompt),
                Line::from(Span::styled(
                    search_hint,
                    Style::default().fg(theme.border_focus),
                )),
            ])
            .wrap(Wrap { trim: true }),
            search_inner,
        );
        targets.push(ClickTarget {
            rect: search_rows[0],
            kind: ClickTargetKind::SearchBox,
        });

        if show_suggestions {
            let suggestions_block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.border_subtle))
                .style(Style::default().bg(theme.content_alt_bg));
            let suggestions_inner = suggestions_block.inner(search_rows[1]);
            frame.render_widget(suggestions_block, search_rows[1]);

            if self.search_suggestions_loading && self.search_suggestions.is_empty() {
                frame.render_widget(
                    Paragraph::new("Loading suggestions...")
                        .style(Style::default().fg(theme.muted)),
                    suggestions_inner,
                );
            } else {
                let visible_count = suggestions_inner
                    .height
                    .min(self.search_suggestions.len() as u16)
                    as usize;
                let max_start = self.search_suggestions.len().saturating_sub(visible_count);
                let first_visible = self
                    .search_suggestion_selected
                    .saturating_add(1)
                    .saturating_sub(visible_count)
                    .min(max_start);

                for (row_index, suggestion_index) in (first_visible..self.search_suggestions.len())
                    .take(visible_count)
                    .enumerate()
                {
                    let suggestion = &self.search_suggestions[suggestion_index];
                    let row = Rect::new(
                        suggestions_inner.x,
                        suggestions_inner.y + row_index as u16,
                        suggestions_inner.width,
                        1,
                    );
                    let selected = suggestion_index == self.search_suggestion_selected;
                    frame.render_widget(
                        Block::default().style(Style::default().bg(if selected {
                            theme.accent_fill
                        } else {
                            theme.content_alt_bg
                        })),
                        row,
                    );
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(
                                if selected { "▸ " } else { "  " },
                                Style::default().fg(if selected {
                                    theme.accent
                                } else {
                                    theme.border
                                }),
                            ),
                            Span::styled(
                                shorten(
                                    suggestion,
                                    suggestions_inner.width.saturating_sub(3) as usize,
                                ),
                                Style::default().fg(if selected {
                                    theme.accent_glow
                                } else {
                                    theme.text
                                }),
                            ),
                        ])),
                        row,
                    );
                    targets.push(ClickTarget {
                        rect: row,
                        kind: ClickTargetKind::SearchSuggestion(suggestion_index),
                    });
                }
            }
        }

        let nav_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.content_alt_bg))
            .padding(Padding::new(1, 1, 0, 0));
        let nav_inner = nav_block.inner(rows[1]);
        frame.render_widget(nav_block, rows[1]);
        let active_nav = self.current_nav_item();
        let nav_items = [
            ("Home", TuiNavItem::Home),
            ("Likes", TuiNavItem::Likes),
            ("Playlists", TuiNavItem::Playlists),
            ("Offline", TuiNavItem::Offline),
        ];
        for (index, (label, nav)) in nav_items.into_iter().enumerate() {
            let row = Rect::new(nav_inner.x, nav_inner.y + index as u16, nav_inner.width, 1);
            let active = active_nav == Some(nav);
            frame.render_widget(
                Block::default().style(Style::default().bg(if active {
                    theme.accent_fill
                } else {
                    theme.content_alt_bg
                })),
                row,
            );
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
                row,
            );
            targets.push(ClickTarget {
                rect: row,
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
            Rect::new(rows[2].x, rows[2].y + 1, rows[2].width, 1),
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
        let recent_inner = recent_block.inner(rows[3]);
        frame.render_widget(recent_block, rows[3]);
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
                rows[3],
                &mut scrollbar_state,
            );
        }

        let settings_row = rows[4];
        let settings_active = active_nav == Some(TuiNavItem::Settings);
        let settings_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if settings_active {
                theme.border_focus
            } else {
                theme.border_subtle
            }))
            .style(Style::default().bg(if settings_active {
                theme.accent_fill
            } else {
                theme.content_alt_bg
            }));
        let settings_inner = settings_block.inner(settings_row);
        frame.render_widget(settings_block, settings_row);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    if settings_active { "● " } else { "○ " },
                    Style::default().fg(if settings_active {
                        theme.accent
                    } else {
                        theme.border
                    }),
                ),
                Span::styled(
                    "Settings",
                    Style::default()
                        .fg(if settings_active {
                            theme.accent_glow
                        } else {
                            theme.text
                        })
                        .add_modifier(if settings_active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ])),
            settings_inner,
        );
        targets.push(ClickTarget {
            rect: settings_row,
            kind: ClickTargetKind::Nav(TuiNavItem::Settings),
        });

        targets
    }

    pub(in crate::cli_tui) fn render_content(
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
            .constraints([Constraint::Length(3), Constraint::Min(8)])
            .split(inner);

        let header = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.content_alt_bg));
        let header_inner = header.inner(rows[0]);
        frame.render_widget(header, rows[0]);
        self.thumbnail_slots.remove(&ThumbnailSlot::ContentHeader);

        let browse_mode = self.content_has_back_button();
        let header_label = match &self.content_view {
            TuiContentView::Home => "S  T  A  T  I  C".to_string(),
            TuiContentView::Browse(view) => format!(
                "S  T  A  T  I  C   /   {}",
                browse_label(view.card.page_type).to_uppercase()
            ),
            TuiContentView::Likes(_) => "S  T  A  T  I  C   /   LIKES".to_string(),
            TuiContentView::Playlists(_) => "S  T  A  T  I  C   /   PLAYLISTS".to_string(),
            TuiContentView::PlaylistDetail(view) => format!(
                "S  T  A  T  I  C   /   {}",
                match view.kind {
                    TuiTrackPageKind::Likes => "LIKES",
                    TuiTrackPageKind::RemotePlaylist => "PLAYLIST",
                    TuiTrackPageKind::OfflinePlaylist => "OFFLINE",
                }
            ),
            TuiContentView::Offline(_) => "S  T  A  T  I  C   /   OFFLINE".to_string(),
            TuiContentView::Search(view) => {
                format!(
                    "S  T  A  T  I  C   /   SEARCH  /  {}",
                    shorten(&view.query, 20)
                )
            }
            TuiContentView::Settings(_) => "S  T  A  T  I  C   /   SETTINGS".to_string(),
        };
        if browse_mode {
            let header_columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(10),
                    Constraint::Min(12),
                    Constraint::Length(12),
                ])
                .split(header_inner);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "[← Back]",
                    Style::default()
                        .fg(theme.accent_glow)
                        .add_modifier(Modifier::BOLD),
                )))
                .alignment(Alignment::Left),
                header_columns[0],
            );
            targets.push(ClickTarget {
                rect: header_columns[0],
                kind: ClickTargetKind::ContentBack,
            });
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    header_label,
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                )))
                .alignment(Alignment::Left),
                header_columns[1],
            );
            frame.render_widget(
                Paragraph::new(Local::now().format("%a %H:%M").to_string())
                    .style(Style::default().fg(theme.muted))
                    .alignment(Alignment::Right),
                header_columns[2],
            );
        } else {
            let header_columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(16), Constraint::Length(12)])
                .split(header_inner);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    header_label,
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                )))
                .alignment(Alignment::Left),
                header_columns[0],
            );
            frame.render_widget(
                Paragraph::new(Local::now().format("%a %H:%M").to_string())
                    .style(Style::default().fg(theme.muted))
                    .alignment(Alignment::Right),
                header_columns[1],
            );
        }

        let feed_block = Block::default()
            .style(Style::default().bg(theme.content_bg))
            .padding(Padding::new(1, 1, 0, 0));
        let feed_inner = feed_block.inner(rows[1]);
        frame.render_widget(feed_block, rows[1]);

        layout.home_viewport = feed_inner;
        self.home_viewport_rows = feed_inner.height as usize;
        if let TuiContentView::Settings(view) = &self.content_view {
            let view = view.clone();
            self.home_doc = HomeDocument::default();
            self.settings_field_offsets.clear();
            self.settings_total_height = 0;
            return self.render_settings_page(frame, feed_inner, theme, &view);
        }

        self.home_doc = match &self.content_view {
            TuiContentView::Home => build_home_document(
                &self.home_status,
                feed_inner.width,
                self.visible_resume_card(),
            ),
            TuiContentView::Browse(view) => build_browse_document(view, feed_inner.width),
            TuiContentView::Likes(view) | TuiContentView::PlaylistDetail(view) => {
                build_track_page_document(view, feed_inner.width)
            }
            TuiContentView::Playlists(view) => {
                build_collection_page_document(view, feed_inner.width)
            }
            TuiContentView::Offline(view) => build_offline_document(view, feed_inner.width),
            TuiContentView::Search(view) => build_search_document(view, feed_inner.width),
            TuiContentView::Settings(_) => HomeDocument::default(),
        };
        if !self.home_doc.cards.is_empty() {
            self.home_selected = self
                .home_selected
                .min(self.home_doc.cards.len().saturating_sub(1));
        } else {
            self.home_selected = 0;
            self.home_scroll_offset = 0;
        }
        self.clamp_home_scroll_offset();
        self.maybe_request_more_home();

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
        let resume_banner_clickable = matches!(&self.content_view, TuiContentView::Home)
            && self.visible_resume_card().is_some();
        for (banner_index, banner) in self.home_doc.banners.iter().enumerate() {
            let banner_top = banner.rect.y as usize;
            let banner_bottom = banner_top.saturating_add(banner.rect.height as usize);
            if banner_bottom <= visible_top || banner_top >= visible_bottom {
                continue;
            }
            let Some(screen_rect) = scrolled_rect(feed_inner, banner.rect, visible_top) else {
                continue;
            };
            if resume_banner_clickable && banner_index == 0 {
                if let Some(visible_rect) = intersect_rect(screen_rect, feed_inner) {
                    targets.push(ClickTarget {
                        rect: visible_rect,
                        kind: ClickTargetKind::Action(TuiAction::ResumeSuggested),
                    });
                }
            }
        }
        for header in &self.home_doc.headers {
            let header_y = feed_inner.y as i32 + header.y as i32 - visible_top as i32;
            if header_y < feed_inner.y as i32 || header_y >= feed_inner.bottom() as i32 {
                continue;
            }
            let Some(layout) = home_header_actions_layout(feed_inner, header) else {
                continue;
            };
            targets.push(ClickTarget {
                rect: Rect::new(layout.play_x, header_y as u16, layout.play_width, 1),
                kind: ClickTargetKind::HomeSectionAction {
                    section_index: header.section_index,
                    shuffle: false,
                },
            });
            targets.push(ClickTarget {
                rect: Rect::new(layout.shuffle_x, header_y as u16, layout.shuffle_width, 1),
                kind: ClickTargetKind::HomeSectionAction {
                    section_index: header.section_index,
                    shuffle: true,
                },
            });
        }
        for (index, card) in self.home_doc.cards.iter().enumerate() {
            let card_top = card.rect.y as usize;
            let card_bottom = card_top.saturating_add(card.rect.height as usize);
            if card_bottom <= visible_top || card_top >= visible_bottom {
                continue;
            }
            let Some(screen_rect) = scrolled_rect(feed_inner, card.rect, visible_top) else {
                continue;
            };
            if let Some(visible_rect) = intersect_rect(screen_rect, feed_inner) {
                targets.push(ClickTarget {
                    rect: visible_rect,
                    kind: ClickTargetKind::HomeItem(index),
                });
            }
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
            for index in 0..self.home_doc.banners.len() {
                let banner = self.home_doc.banners[index].clone();
                let Some(url) = banner.thumbnail_url.as_deref() else {
                    continue;
                };
                let banner_top = banner.rect.y as usize;
                let banner_bottom = banner_top.saturating_add(banner.rect.height as usize);
                if banner_bottom <= visible_top || banner_top >= visible_bottom {
                    continue;
                }
                let Some(screen_rect) = scrolled_rect(feed_inner, banner.rect, visible_top) else {
                    continue;
                };
                let Some(thumb_rect) = home_banner_thumbnail_rect(screen_rect) else {
                    continue;
                };
                self.render_cover_thumbnail(
                    frame,
                    thumb_rect,
                    ThumbnailSlot::ContentBanner(index),
                    Some(url),
                    placeholder_initials(&banner.title, &banner.subtitle),
                    theme,
                    banner.accent,
                );
            }
            for index in 0..self.home_doc.cards.len() {
                let card = self.home_doc.cards[index].clone();
                let card_top = card.rect.y as usize;
                let card_bottom = card_top.saturating_add(card.rect.height as usize);
                if card_bottom <= visible_top || card_top >= visible_bottom {
                    continue;
                }
                let Some(screen_rect) = scrolled_rect(feed_inner, card.rect, visible_top) else {
                    continue;
                };
                let Some(thumb_rect) = home_card_thumbnail_rect(screen_rect, card.style) else {
                    continue;
                };
                self.render_home_card_thumbnail(frame, thumb_rect, feed_inner, index, &card, theme);
            }
        }

        targets
    }
}
