use super::*;
use super::{page_builders::*, playback::*, queue_drag::*, views::*, widgets::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn close_context_menu(&mut self) {
        self.context_menu = None;
        self.rendered_context_menu = RenderedContextMenu::default();
    }

    pub(in crate::cli_tui) fn can_jump_to_radio(&self) -> bool {
        self.snapshot.radio_virtual_playlist.is_some()
            || self.snapshot.playlist_context.is_some()
            || !self.snapshot.queue.is_empty()
    }

    pub(in crate::cli_tui) fn push_feedback_context_items(
        &self,
        items: &mut Vec<ContextMenuItem>,
        track: &Track,
    ) {
        let undo_visible = self.feedback_undo_ready.contains(&track.video_id);

        if !undo_visible {
            if let Some(token) = track.not_interested_feedback_token.clone() {
                items.push(ContextMenuItem {
                    label: "Not interested".to_string(),
                    hint: "feedback".to_string(),
                    tone: ContextMenuTone::Danger,
                    action: ContextMenuAction::SubmitFeedbackToken {
                        token,
                        verb: "Marked as not interested",
                        title: track.title.clone(),
                        video_id: track.video_id.clone(),
                        reveal_undo: true,
                    },
                });
            }
        }

        if undo_visible {
            if let Some(token) = track.undo_feedback_token.clone() {
                items.push(ContextMenuItem {
                    label: "Undo feedback".to_string(),
                    hint: "restore".to_string(),
                    tone: ContextMenuTone::Default,
                    action: ContextMenuAction::SubmitFeedbackToken {
                        token,
                        verb: "Feedback undone for",
                        title: track.title.clone(),
                        video_id: track.video_id.clone(),
                        reveal_undo: false,
                    },
                });
            }
        }
    }

    pub(in crate::cli_tui) fn context_menu_track_items(
        &self,
        track: &Track,
    ) -> Vec<ContextMenuItem> {
        let mut items = vec![
            ContextMenuItem {
                label: "Play now".to_string(),
                hint: "start radio".to_string(),
                tone: ContextMenuTone::Accent,
                action: ContextMenuAction::PlayTrackNow {
                    track: track.clone(),
                },
            },
            ContextMenuItem {
                label: "Play next".to_string(),
                hint: "after current".to_string(),
                tone: ContextMenuTone::Default,
                action: ContextMenuAction::QueueTrackNext {
                    track: track.clone(),
                },
            },
            ContextMenuItem {
                label: "Add to queue".to_string(),
                hint: "respect radio tail".to_string(),
                tone: ContextMenuTone::Default,
                action: ContextMenuAction::QueueTrackLast {
                    track: track.clone(),
                },
            },
        ];

        if self.can_jump_to_radio() {
            items.push(ContextMenuItem {
                label: "Jump to".to_string(),
                hint: "radio seed".to_string(),
                tone: ContextMenuTone::Default,
                action: ContextMenuAction::JumpToTrackRadio {
                    track: track.clone(),
                },
            });
        }

        items.push(ContextMenuItem {
            label: "Probe headless".to_string(),
            hint: "no radio".to_string(),
            tone: ContextMenuTone::Default,
            action: ContextMenuAction::ProbeTrack {
                track: track.clone(),
            },
        });
        self.push_feedback_context_items(&mut items, track);

        items
    }

    pub(in crate::cli_tui) fn browse_card_queueable(card: &HomeBrowseCard) -> bool {
        matches!(
            card.page_type,
            HomeBrowsePageType::Album
                | HomeBrowsePageType::Playlist
                | HomeBrowsePageType::PodcastShow
        )
    }

    pub(in crate::cli_tui) fn context_menu_for_target(
        &self,
        kind: &ClickTargetKind,
        anchor_column: u16,
        anchor_row: u16,
    ) -> Option<ContextMenuState> {
        let (title, subtitle, items) = match kind {
            ClickTargetKind::QueueItem(index) => {
                let track = self.snapshot.queue.get(*index)?.clone();
                let mut items = vec![ContextMenuItem {
                    label: "Play now".to_string(),
                    hint: "keep queue".to_string(),
                    tone: ContextMenuTone::Accent,
                    action: ContextMenuAction::PlayQueueNow { index: *index },
                }];
                if self.can_jump_to_radio() {
                    items.push(ContextMenuItem {
                        label: "Jump to".to_string(),
                        hint: "radio seed".to_string(),
                        tone: ContextMenuTone::Default,
                        action: ContextMenuAction::JumpToQueueRadio { index: *index },
                    });
                }
                items.push(ContextMenuItem {
                    label: "Probe headless".to_string(),
                    hint: "no radio".to_string(),
                    tone: ContextMenuTone::Default,
                    action: ContextMenuAction::ProbeTrack {
                        track: track.clone(),
                    },
                });
                self.push_feedback_context_items(&mut items, &track);
                if *index != self.snapshot.queue_index {
                    items.push(ContextMenuItem {
                        label: "Remove from queue".to_string(),
                        hint: "delete row".to_string(),
                        tone: ContextMenuTone::Danger,
                        action: ContextMenuAction::RemoveQueueItem { index: *index },
                    });
                }
                (
                    format!("Queue #{}", index.saturating_add(1)),
                    format_track_summary(&track),
                    items,
                )
            }
            ClickTargetKind::HomeItem(index) => {
                let card = self.home_doc.cards.get(*index)?.clone();
                match card.item {
                    TuiHomeItem::Track(track) => (
                        track.title.clone(),
                        artist_string(&track),
                        self.context_menu_track_items(&track),
                    ),
                    TuiHomeItem::Browse(card) => {
                        let mut items = vec![ContextMenuItem {
                            label: "Open now".to_string(),
                            hint: browse_label(card.page_type).to_lowercase(),
                            tone: ContextMenuTone::Accent,
                            action: ContextMenuAction::OpenBrowseNow { card: card.clone() },
                        }];
                        if Self::browse_card_queueable(&card) {
                            items.push(ContextMenuItem {
                                label: "Queue next".to_string(),
                                hint: "insert collection".to_string(),
                                tone: ContextMenuTone::Default,
                                action: ContextMenuAction::QueueBrowseNext { card: card.clone() },
                            });
                            items.push(ContextMenuItem {
                                label: "Add to queue".to_string(),
                                hint: "append collection".to_string(),
                                tone: ContextMenuTone::Default,
                                action: ContextMenuAction::QueueBrowseLast { card: card.clone() },
                            });
                        }
                        (
                            card.title.clone(),
                            card.subtitle
                                .clone()
                                .unwrap_or_else(|| browse_label(card.page_type).to_string()),
                            items,
                        )
                    }
                    TuiHomeItem::Playlist(card) => (
                        card.summary.title.clone(),
                        card.summary
                            .subtitle
                            .clone()
                            .unwrap_or_else(|| playlist_source_label(card.source).to_string()),
                        Vec::new(),
                    ),
                }
            }
            ClickTargetKind::RecentItem(index) => {
                let track = self.recent_tracks.get(*index)?.clone();
                (
                    track.title.clone(),
                    artist_string(&track),
                    self.context_menu_track_items(&track),
                )
            }
            _ => return None,
        };

        if items.is_empty() {
            return None;
        }

        Some(ContextMenuState {
            title,
            subtitle,
            items,
            selected: 0,
            anchor_column,
            anchor_row,
        })
    }

    pub(in crate::cli_tui) fn open_context_menu(
        &mut self,
        kind: &ClickTargetKind,
        anchor_column: u16,
        anchor_row: u16,
    ) -> bool {
        if let Some(menu) = self.context_menu_for_target(kind, anchor_column, anchor_row) {
            self.context_menu = Some(menu);
            self.status_line =
                "Context menu open. Enter runs, Esc closes, arrow keys move.".to_string();
            true
        } else {
            self.close_context_menu();
            false
        }
    }

    pub(in crate::cli_tui) fn queue_next_slot(&self) -> usize {
        if self.snapshot.queue.is_empty() {
            0
        } else {
            self.snapshot
                .queue_index
                .saturating_add(1)
                .min(self.snapshot.queue.len())
        }
    }

    pub(in crate::cli_tui) fn queue_last_slot(&self) -> usize {
        self.snapshot
            .radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.touched_len.min(self.snapshot.queue.len()))
            .unwrap_or(self.snapshot.queue.len())
    }

    pub(in crate::cli_tui) fn jump_to_queue_radio_seed(&mut self, index: usize) -> Result<()> {
        if !self.can_jump_to_radio() {
            self.status_line = "Jump to needs an active queue or playlist context.".to_string();
            return Ok(());
        }

        let Some(seed_track) = self.snapshot.queue.get(index).cloned() else {
            self.status_line = "Queue item is gone.".to_string();
            return Ok(());
        };

        let prefix_len = self
            .snapshot
            .radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.touched_len.max(index.saturating_add(1)))
            .unwrap_or(self.snapshot.queue.len())
            .min(self.snapshot.queue.len());

        self.apply_jump_to_radio_seed(
            self.snapshot.queue.clone(),
            prefix_len,
            seed_track,
            index.min(self.snapshot.queue.len().saturating_sub(1)),
        )
    }

    pub(in crate::cli_tui) fn jump_to_track_radio_seed(&mut self, track: Track) -> Result<()> {
        if !self.can_jump_to_radio() {
            self.status_line = "Jump to needs an active queue or playlist context.".to_string();
            return Ok(());
        }

        let mut queue = self.snapshot.queue.clone();
        let selected_index = if self.snapshot.radio_virtual_playlist.is_some() {
            let insert_at = self.queue_last_slot().min(queue.len());
            queue.insert(insert_at, track.clone());
            insert_at
        } else if let Some(existing_index) = queue
            .iter()
            .position(|queued| queued.video_id == track.video_id)
        {
            existing_index
        } else {
            queue.push(track.clone());
            queue.len().saturating_sub(1)
        };

        let prefix_len = if self.snapshot.radio_virtual_playlist.is_some() {
            self.snapshot
                .radio_virtual_playlist
                .as_ref()
                .map(|radio| {
                    if selected_index < radio.touched_len {
                        radio.touched_len
                    } else {
                        radio.touched_len.saturating_add(1)
                    }
                })
                .unwrap_or(selected_index.saturating_add(1))
                .min(queue.len())
        } else {
            queue.len()
        };

        self.apply_jump_to_radio_seed(queue, prefix_len, track, selected_index)
    }

    pub(in crate::cli_tui) fn apply_jump_to_radio_seed(
        &mut self,
        queue: Vec<Track>,
        prefix_len: usize,
        seed_track: Track,
        selected_index: usize,
    ) -> Result<()> {
        let mut final_queue = queue.clone();
        let mut chips = Vec::new();
        let mut tail_refreshed = false;

        if let Ok(mut radio) = saved_network_state()
            .and_then(|network| network.get_radio(&seed_track.video_id, None, None))
        {
            if let Some(seed_index) = radio
                .tracks
                .iter()
                .position(|track| track.video_id == seed_track.video_id)
            {
                if seed_index > 0 {
                    let track = radio.tracks.remove(seed_index);
                    radio.tracks.insert(0, track);
                }
            } else {
                radio.tracks.insert(0, seed_track.clone());
            }

            final_queue.truncate(prefix_len.min(final_queue.len()));
            final_queue.extend(dedupe_radio_recommendations(
                final_queue.as_slice(),
                radio.tracks,
            ));
            chips = radio.chips;
            tail_refreshed = true;
        }

        let current_index = if final_queue.is_empty() {
            0
        } else {
            self.snapshot
                .queue_index
                .min(final_queue.len().saturating_sub(1))
        };
        let snapshot = self.client.replace_queue(
            final_queue,
            current_index,
            self.snapshot.playlist_context.clone(),
            Some(RadioVirtualPlaylist {
                source_video_id: seed_track.video_id.clone(),
                touched_len: prefix_len,
                chips,
            }),
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.focus = TuiFocus::Queue;
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = selected_index.min(self.snapshot.queue.len().saturating_sub(1));
        self.ensure_queue_visible();
        self.status_line = if tail_refreshed {
            format!("Jumped radio to {}.", shorten(&seed_track.title, 28))
        } else {
            format!(
                "Jumped radio to {}. Keeping the current tail for now.",
                shorten(&seed_track.title, 28)
            )
        };
        Ok(())
    }

    pub(in crate::cli_tui) fn activate_radio_chip(&mut self, chip_index: usize) -> Result<()> {
        let Some(radio) = self.snapshot.radio_virtual_playlist.clone() else {
            self.status_line = "Radio chips only appear for radio queues.".to_string();
            return Ok(());
        };
        let current_chips = self.current_radio_chips();
        let Some(chip) = current_chips.get(chip_index).cloned() else {
            self.status_line = "That radio filter is no longer available.".to_string();
            return Ok(());
        };
        let source_video_id = radio.source_video_id;
        let prefix_len = radio.touched_len.max(1);
        let source_track = self
            .snapshot
            .queue
            .iter()
            .find(|track| track.video_id == source_video_id)
            .cloned()
            .or_else(|| self.snapshot.queue.first().cloned());

        let mut refreshed = saved_network_state()?.get_radio(
            &source_video_id,
            Some(&chip.playlist_id),
            chip.params.as_deref(),
        )?;

        if let Some(source_index) = refreshed
            .tracks
            .iter()
            .position(|track| track.video_id == source_video_id)
        {
            if source_index > 0 {
                let track = refreshed.tracks.remove(source_index);
                refreshed.tracks.insert(0, track);
            }
        } else if let Some(source_track) = source_track {
            refreshed.tracks.insert(0, source_track);
        }

        let mut queue = self.snapshot.queue.clone();
        queue.truncate(prefix_len.min(queue.len()));
        queue.extend(dedupe_radio_recommendations(
            queue.as_slice(),
            refreshed.tracks,
        ));

        let current_index = self.snapshot.queue_index.min(queue.len().saturating_sub(1));
        let snapshot = self.client.replace_queue(
            queue,
            current_index,
            self.snapshot.playlist_context.clone(),
            Some(RadioVirtualPlaylist {
                source_video_id,
                touched_len: prefix_len,
                chips: merge_radio_chip_update(&current_chips, refreshed.chips),
            }),
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.status_line = format!("Switched radio to {}.", shorten(&chip.label, 24));
        Ok(())
    }

    pub(in crate::cli_tui) fn play_track_now(&mut self, track: Track) -> Result<()> {
        let snapshot = play_track_with_radio(&self.client, track.clone())?;
        self.apply_snapshot(snapshot);
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        self.status_line = format!("Playing {}", format_track_summary(&track));
        Ok(())
    }

    pub(in crate::cli_tui) fn open_browse_card_now(&mut self, card: HomeBrowseCard) -> Result<()> {
        self.open_browse_detail(card);
        Ok(())
    }

    pub(in crate::cli_tui) fn probe_track_now(&mut self, track: Track) -> Result<()> {
        let snapshot = self.client.probe_track(track.clone())?;
        self.apply_snapshot(snapshot);
        self.status_line = format!("Probing {}", format_track_summary(&track));
        Ok(())
    }

    pub(in crate::cli_tui) fn play_queue_index_now(&mut self, index: usize) -> Result<()> {
        if self.snapshot.queue.is_empty() || index >= self.snapshot.queue.len() {
            self.status_line = "Queue item is gone.".to_string();
            return Ok(());
        }

        let snapshot = self.client.play_queue_at(
            self.snapshot.queue.clone(),
            index,
            self.snapshot.playlist_context.clone(),
            self.snapshot.radio_virtual_playlist.clone(),
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.queue_selected = index.min(self.snapshot.queue.len().saturating_sub(1));
        self.ensure_queue_visible();
        self.status_line = format!(
            "Playing {}",
            self.snapshot
                .queue
                .get(index)
                .map(format_track_summary)
                .unwrap_or_else(|| "selection".to_string())
        );
        Ok(())
    }

    pub(in crate::cli_tui) fn remove_queue_item(&mut self, index: usize) -> Result<()> {
        if self.snapshot.queue.is_empty() || index >= self.snapshot.queue.len() {
            self.status_line = "Queue row is gone.".to_string();
            return Ok(());
        }
        if index == self.snapshot.queue_index {
            self.status_line = "Remove a non-playing row or drag the queue around it.".to_string();
            return Ok(());
        }

        let mut queue = self.snapshot.queue.clone();
        let removed = queue.remove(index);
        let current_index = if index < self.snapshot.queue_index {
            self.snapshot.queue_index.saturating_sub(1)
        } else {
            self.snapshot.queue_index.min(queue.len().saturating_sub(1))
        };
        let mut radio_virtual_playlist = self.snapshot.radio_virtual_playlist.clone();
        shrink_radio_touched_for_remove(&mut radio_virtual_playlist, index);
        let snapshot = self.client.replace_queue(
            queue,
            current_index,
            self.snapshot.playlist_context.clone(),
            radio_virtual_playlist,
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.queue_selected = index.min(self.snapshot.queue.len().saturating_sub(1));
        self.ensure_queue_visible();
        self.status_line = format!("Removed {}.", shorten(&removed.title, 28));
        Ok(())
    }

    pub(in crate::cli_tui) fn submit_feedback_token_action(
        &mut self,
        token: String,
        verb: &'static str,
        title: String,
        video_id: String,
        reveal_undo: bool,
    ) -> Result<()> {
        saved_network_state()?.submit_feedback_token(&token)?;
        if reveal_undo {
            self.feedback_undo_ready.insert(video_id);
        } else {
            self.feedback_undo_ready.remove(&video_id);
        }
        self.status_line = format!("{verb} {}.", shorten(&title, 28));
        Ok(())
    }

    pub(in crate::cli_tui) fn execute_context_menu_action(
        &mut self,
        action: ContextMenuAction,
    ) -> Result<()> {
        match action {
            ContextMenuAction::PlayTrackNow { track } => self.play_track_now(track),
            ContextMenuAction::PlayQueueNow { index } => self.play_queue_index_now(index),
            ContextMenuAction::JumpToTrackRadio { track } => self.jump_to_track_radio_seed(track),
            ContextMenuAction::JumpToQueueRadio { index } => self.jump_to_queue_radio_seed(index),
            ContextMenuAction::QueueTrackNext { track } => {
                self.handle_drag_drop(TuiDragPayload::Track { track }, self.queue_next_slot())
            }
            ContextMenuAction::QueueTrackLast { track } => {
                self.handle_drag_drop(TuiDragPayload::Track { track }, self.queue_last_slot())
            }
            ContextMenuAction::OpenBrowseNow { card } => self.open_browse_card_now(card),
            ContextMenuAction::QueueBrowseNext { card } => self.handle_drag_drop(
                TuiDragPayload::BrowseCollection { card },
                self.queue_next_slot(),
            ),
            ContextMenuAction::QueueBrowseLast { card } => self.handle_drag_drop(
                TuiDragPayload::BrowseCollection { card },
                self.queue_last_slot(),
            ),
            ContextMenuAction::ProbeTrack { track } => self.probe_track_now(track),
            ContextMenuAction::RemoveQueueItem { index } => self.remove_queue_item(index),
            ContextMenuAction::SubmitFeedbackToken {
                token,
                verb,
                title,
                video_id,
                reveal_undo,
            } => self.submit_feedback_token_action(token, verb, title, video_id, reveal_undo),
        }
    }

    pub(in crate::cli_tui) fn move_context_menu_selection(&mut self, delta: isize) {
        let Some(menu) = self.context_menu.as_mut() else {
            return;
        };
        if menu.items.is_empty() {
            menu.selected = 0;
            return;
        }
        menu.selected = move_index(menu.selected, menu.items.len(), delta);
    }

    pub(in crate::cli_tui) fn activate_context_menu_selection(&mut self) -> Result<()> {
        let Some(action) = self
            .context_menu
            .as_ref()
            .and_then(|menu| menu.items.get(menu.selected))
            .map(|item| item.action.clone())
        else {
            self.close_context_menu();
            return Ok(());
        };
        self.close_context_menu();
        self.execute_context_menu_action(action)
    }

    pub(in crate::cli_tui) fn select_context_menu_item(&mut self, index: usize) {
        if let Some(menu) = self.context_menu.as_mut() {
            menu.selected = index.min(menu.items.len().saturating_sub(1));
        }
    }

    pub(in crate::cli_tui) fn render_context_menu(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
    ) -> RenderedContextMenu {
        let Some(menu) = self.context_menu.as_ref() else {
            return RenderedContextMenu::default();
        };

        let title_width = menu.title.chars().count();
        let subtitle_width = menu.subtitle.chars().count();
        let item_width = menu
            .items
            .iter()
            .map(|item| item.label.chars().count() + item.hint.chars().count() + 6)
            .max()
            .unwrap_or(0);
        let width = (title_width.max(subtitle_width).max(item_width) as u16)
            .saturating_add(6)
            .clamp(24, area.width.saturating_sub(2).max(24));
        let body_height = menu.items.len() as u16;
        let height = (body_height + 5).clamp(6, area.height.saturating_sub(2).max(6));
        let x = if menu.anchor_column.saturating_add(width) <= area.right() {
            menu.anchor_column.min(area.right().saturating_sub(width))
        } else {
            area.right().saturating_sub(width)
        }
        .max(area.x.saturating_add(1));
        let y = if menu.anchor_row.saturating_add(height) <= area.bottom() {
            menu.anchor_row.min(area.bottom().saturating_sub(height))
        } else {
            area.bottom().saturating_sub(height)
        }
        .max(area.y.saturating_add(1));
        let menu_rect = Rect::new(x, y, width, height);

        frame.render_widget(Clear, menu_rect);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.border_focus))
                .style(Style::default().bg(theme.elevated_alt_bg)),
            menu_rect,
        );

        let inner = Rect::new(
            menu_rect.x.saturating_add(1),
            menu_rect.y.saturating_add(1),
            menu_rect.width.saturating_sub(2),
            menu_rect.height.saturating_sub(2),
        );
        let item_rows = menu.items.len() as u16;
        let footer_height = 1u16;
        let subtitle_height = 1u16;
        let spacer_height = 1u16;
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(subtitle_height),
                Constraint::Length(item_rows),
                Constraint::Length(spacer_height),
                Constraint::Length(footer_height),
            ])
            .split(inner);

        frame.render_widget(
            Paragraph::new(shorten(&menu.title, rows[0].width as usize))
                .style(Style::default().fg(theme.text).add_modifier(Modifier::BOLD)),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(shorten(&menu.subtitle, rows[1].width as usize))
                .style(Style::default().fg(theme.muted)),
            rows[1],
        );

        let mut item_rects = Vec::with_capacity(menu.items.len());
        for (index, item) in menu.items.iter().enumerate() {
            let row = Rect::new(rows[2].x, rows[2].y + index as u16, rows[2].width, 1);
            let selected = index == menu.selected;
            let (fg, bg) = match (item.tone, selected) {
                (ContextMenuTone::Accent, true) => (theme.accent_glow, theme.accent_fill),
                (ContextMenuTone::Accent, false) => (theme.accent, theme.elevated_alt_bg),
                (ContextMenuTone::Danger, true) => (theme.text, theme.danger),
                (ContextMenuTone::Danger, false) => (theme.danger, theme.elevated_alt_bg),
                (ContextMenuTone::Default, true) => (theme.text, theme.selected_bg),
                (ContextMenuTone::Default, false) => (theme.text, theme.elevated_alt_bg),
            };
            frame.render_widget(Block::default().style(Style::default().bg(bg)), row);
            let label = shorten(
                &format!("{:>2}. {}", index.saturating_add(1), item.label),
                row.width.saturating_sub(item.hint.len() as u16 + 1) as usize,
            );
            let gap = row
                .width
                .saturating_sub(label.chars().count() as u16)
                .saturating_sub(item.hint.chars().count() as u16)
                .max(1) as usize;
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        label,
                        Style::default().fg(fg).bg(bg).add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                    ),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(
                        shorten(&item.hint, row.width as usize / 2),
                        Style::default().fg(theme.muted).bg(bg),
                    ),
                ])),
                row,
            );
            item_rects.push(row);
        }

        frame.render_widget(
            Paragraph::new("enter run  esc close  arrow keys move")
                .style(Style::default().fg(theme.border_focus)),
            rows[4],
        );

        RenderedContextMenu {
            menu_rect: Some(menu_rect),
            item_rects,
        }
    }
}
