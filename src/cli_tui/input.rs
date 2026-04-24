use super::widgets::*;
use super::*;

impl InteractiveTui {
    pub(in crate::cli_tui) fn handle_key(
        &mut self,
        key_event: crossterm::event::KeyEvent,
    ) -> Result<bool> {
        let code = key_event.code;

        if let Some(edit) = self.settings_text_edit.clone() {
            match code {
                KeyCode::Esc => {
                    self.settings_text_edit = None;
                    self.status_line = "Setting edit cancelled.".to_string();
                }
                KeyCode::Enter => {
                    self.save_settings_text_edit(edit)?;
                }
                KeyCode::Backspace => {
                    if let Some(active_edit) = &mut self.settings_text_edit {
                        active_edit.buffer.pop();
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some(active_edit) = &mut self.settings_text_edit {
                        active_edit.buffer.push(ch);
                    }
                }
                _ => {}
            }
            return Ok(false);
        }

        if self.search_input_active {
            match code {
                KeyCode::Esc => {
                    self.search_input_active = false;
                    self.clear_search_suggestions();
                    self.status_line = "Search cancelled.".to_string();
                }
                KeyCode::Enter => {
                    let preferred = self
                        .search_suggestions
                        .get(self.search_suggestion_selected)
                        .cloned();
                    self.submit_search(preferred);
                }
                KeyCode::Backspace => {
                    self.search_query.pop();
                    self.schedule_search_suggestions();
                }
                KeyCode::Char(ch) => {
                    self.search_query.push(ch);
                    self.schedule_search_suggestions();
                }
                KeyCode::Up => self.move_search_suggestion_selection(-1),
                KeyCode::Down => self.move_search_suggestion_selection(1),
                KeyCode::Tab => self.fill_selected_search_suggestion(),
                _ => {}
            }
            return Ok(false);
        }

        if self.context_menu.is_some() {
            match code {
                KeyCode::Esc => {
                    self.close_context_menu();
                    self.status_line = "Context menu closed.".to_string();
                    return Ok(false);
                }
                KeyCode::Up => {
                    self.move_context_menu_selection(-1);
                    return Ok(false);
                }
                KeyCode::Down => {
                    self.move_context_menu_selection(1);
                    return Ok(false);
                }
                KeyCode::Home => {
                    self.select_context_menu_item(0);
                    return Ok(false);
                }
                KeyCode::End => {
                    let last = self
                        .context_menu
                        .as_ref()
                        .map(|menu| menu.items.len().saturating_sub(1))
                        .unwrap_or(0);
                    self.select_context_menu_item(last);
                    return Ok(false);
                }
                KeyCode::Enter => {
                    self.activate_context_menu_selection()?;
                    return Ok(false);
                }
                KeyCode::Char(ch) if ('1'..='9').contains(&ch) => {
                    let index = ch as usize - '1' as usize;
                    if self
                        .context_menu
                        .as_ref()
                        .is_some_and(|menu| index < menu.items.len())
                    {
                        self.select_context_menu_item(index);
                        self.activate_context_menu_selection()?;
                    }
                    return Ok(false);
                }
                _ => {}
            }
        }

        if self.help_visible {
            match code {
                KeyCode::Esc | KeyCode::Char('h') | KeyCode::Char('q') => {
                    self.help_visible = false;
                    self.rendered_help_rect = None;
                    return Ok(false);
                }
                _ => return Ok(false),
            }
        }

        match code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('h') => {
                self.help_visible = true;
            }
            KeyCode::Char('/') => {
                self.activate_search_input();
            }
            KeyCode::Backspace | KeyCode::Char('b')
                if !matches!(&self.content_view, TuiContentView::Home) =>
            {
                self.go_back_content();
            }
            KeyCode::Tab => self.cycle_focus(),
            KeyCode::Char(' ') => self.run_action(TuiAction::TogglePause)?,
            KeyCode::Char('r') => self.refresh_current_view(),
            KeyCode::Char('1') => self.detail_tab = DetailTab::Queue,
            KeyCode::Char('2') => self.detail_tab = DetailTab::Lyrics,
            KeyCode::Char('t') => self.toggle_detail_tab(),
            KeyCode::Char('n') => self.run_action(TuiAction::Next)?,
            KeyCode::Char('p') => self.run_action(TuiAction::Previous)?,
            KeyCode::Char('a') => self.seek_relative_millis(-(SEEK_STEP_MILLIS as i64))?,
            KeyCode::Char('s') => self.seek_relative_millis(SEEK_STEP_MILLIS as i64)?,
            KeyCode::Char('l') => self.run_action(TuiAction::CycleRepeat)?,
            KeyCode::Char('f') => self.run_action(TuiAction::Like)?,
            KeyCode::Char('d') => self.run_action(TuiAction::Dislike)?,
            KeyCode::Char('u') => self.run_action(TuiAction::Neutral)?,
            KeyCode::Char('-') | KeyCode::Char('_')
                if matches!(self.content_view, TuiContentView::Settings(_))
                    && matches!(self.focus, TuiFocus::Home) =>
            {
                self.adjust_selected_setting(-1)?;
            }
            KeyCode::Char('+') | KeyCode::Char('=')
                if matches!(self.content_view, TuiContentView::Settings(_))
                    && matches!(self.focus, TuiFocus::Home) =>
            {
                self.adjust_selected_setting(1)?;
            }
            KeyCode::Char('-') | KeyCode::Char('_') => self.run_action(TuiAction::VolumeDown)?,
            KeyCode::Char('+') | KeyCode::Char('=') => self.run_action(TuiAction::VolumeUp)?,
            KeyCode::Char('m') => self.open_context_menu_for_focus(),
            KeyCode::Left => self.move_selection(NavigationDirection::Left),
            KeyCode::Right => self.move_selection(NavigationDirection::Right),
            KeyCode::Up => {
                if matches!(self.focus, TuiFocus::Player) {
                    self.run_action(TuiAction::VolumeUp)?;
                } else {
                    self.move_selection(NavigationDirection::Up);
                }
            }
            KeyCode::Down => {
                if matches!(self.focus, TuiFocus::Player) {
                    self.run_action(TuiAction::VolumeDown)?;
                } else {
                    self.move_selection(NavigationDirection::Down);
                }
            }
            KeyCode::Home => self.jump_to_start(),
            KeyCode::End => self.jump_to_end(),
            KeyCode::PageDown => self.page_move(1),
            KeyCode::PageUp => self.page_move(-1),
            KeyCode::Enter => self.activate_selection()?,
            _ => {}
        }

        Ok(false)
    }

    pub(in crate::cli_tui) fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll_panel(mouse.column, mouse.row, 2),
            MouseEventKind::ScrollUp => self.scroll_panel(mouse.column, mouse.row, -2),
            MouseEventKind::Down(MouseButton::Left) => {
                if self.help_visible {
                    let clicked_inside_help = self
                        .rendered_help_rect
                        .is_some_and(|rect| contains(rect, mouse.column, mouse.row));
                    if !clicked_inside_help {
                        self.help_visible = false;
                        self.rendered_help_rect = None;
                    }
                    return Ok(());
                }
                let clicked_menu_item = self
                    .rendered_context_menu
                    .item_rects
                    .iter()
                    .position(|rect| contains(*rect, mouse.column, mouse.row));
                let clicked_inside_menu = self
                    .rendered_context_menu
                    .menu_rect
                    .is_some_and(|rect| contains(rect, mouse.column, mouse.row));
                if let Some(index) = clicked_menu_item {
                    self.select_context_menu_item(index);
                    self.activate_context_menu_selection()?;
                    return Ok(());
                }
                if self.context_menu.is_some() {
                    self.close_context_menu();
                    if clicked_inside_menu {
                        return Ok(());
                    }
                }
                self.drag_state = None;
                if let Some(target) = self
                    .click_targets
                    .iter()
                    .find(|target| contains(target.rect, mouse.column, mouse.row))
                    .cloned()
                {
                    if let Some(payload) = self.drag_payload_for_target(&target.kind) {
                        self.set_mouse_selection(&target.kind);
                        self.pending_mouse_press = Some(PendingMousePress {
                            origin_column: mouse.column,
                            origin_row: mouse.row,
                            target_kind: target.kind,
                            target_rect: target.rect,
                            payload: Some(payload),
                        });
                    } else {
                        self.pending_mouse_press = None;
                        self.activate_click_target(&target.kind)?;
                    }
                } else if contains(
                    self.rendered_layout.player_progress_viewport,
                    mouse.column,
                    mouse.row,
                ) {
                    self.pending_mouse_press = None;
                    self.seek_from_progress_click(mouse.column)?;
                } else if contains(self.rendered_layout.home_viewport, mouse.column, mouse.row) {
                    self.pending_mouse_press = None;
                    self.focus = TuiFocus::Home;
                } else if contains(self.rendered_layout.queue_viewport, mouse.column, mouse.row) {
                    self.pending_mouse_press = None;
                    self.focus = TuiFocus::Queue;
                } else if contains(
                    self.rendered_layout.recent_viewport,
                    mouse.column,
                    mouse.row,
                ) {
                    self.pending_mouse_press = None;
                    self.focus = TuiFocus::Recent;
                } else {
                    self.pending_mouse_press = None;
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.clear_drag_state();
                if let Some(target) = self
                    .click_targets
                    .iter()
                    .find(|target| contains(target.rect, mouse.column, mouse.row))
                    .cloned()
                {
                    self.set_mouse_selection(&target.kind);
                    self.open_context_menu(&target.kind, mouse.column, mouse.row);
                } else {
                    self.close_context_menu();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.close_context_menu();
                if self.drag_state.is_none() {
                    if let Some(press) = self.pending_mouse_press.as_ref() {
                        let moved =
                            mouse.column != press.origin_column || mouse.row != press.origin_row;
                        if moved {
                            if let Some(payload) = press.payload.clone() {
                                self.drag_state = Some(TuiDragState {
                                    payload,
                                    preview_slot: None,
                                });
                            }
                        }
                    }
                }
                if self.drag_state.is_some() {
                    self.update_drag_preview(mouse.column, mouse.row);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(drag) = self.drag_state.take() {
                    self.pending_mouse_press = None;
                    if let Some(raw_slot) = drag.preview_slot {
                        if let Err(err) = self.handle_drag_drop(drag.payload, raw_slot) {
                            self.status_line = format!("Drag failed: {err}");
                        }
                    }
                } else if let Some(press) = self.pending_mouse_press.take() {
                    if contains(press.target_rect, mouse.column, mouse.row) {
                        self.activate_click_target(&press.target_kind)?;
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    pub(in crate::cli_tui) fn adjust_selected_setting(&mut self, delta: isize) -> Result<()> {
        let Some(field) = self.selected_settings_field() else {
            return Ok(());
        };

        match field.control {
            TuiSettingsControl::Toggle { key, .. } => self.toggle_boolean_setting(key),
            TuiSettingsControl::Dropdown { key, .. } => {
                self.cycle_coordination_setting_delta(key, delta)
            }
            TuiSettingsControl::Stepper { key, .. } => self.adjust_numeric_setting(key, delta),
            TuiSettingsControl::SponsorCategory { key, .. } => {
                self.toggle_sponsorblock_category(key)
            }
            TuiSettingsControl::TextInput { .. }
            | TuiSettingsControl::StorageRoot { .. }
            | TuiSettingsControl::ReadOnly { .. } => Ok(()),
        }
    }

    pub(in crate::cli_tui) fn player_action_focused(&self, index: usize) -> bool {
        matches!(self.focus, TuiFocus::Player) && self.player_selected == index
    }

    pub(in crate::cli_tui) fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            TuiFocus::Recent => TuiFocus::Home,
            TuiFocus::Home => TuiFocus::Queue,
            TuiFocus::Queue => TuiFocus::Player,
            TuiFocus::Player => TuiFocus::Recent,
        };
    }

    pub(in crate::cli_tui) fn move_selection(&mut self, direction: NavigationDirection) {
        match self.focus {
            TuiFocus::Home => {
                if matches!(self.content_view, TuiContentView::Settings(_)) {
                    match direction {
                        NavigationDirection::Up => {
                            self.home_selected = self.home_selected.saturating_sub(1);
                            self.ensure_home_visible();
                        }
                        NavigationDirection::Down => {
                            let count = self.settings_field_offsets.len();
                            if count > 0 {
                                self.home_selected =
                                    (self.home_selected + 1).min(count.saturating_sub(1));
                                self.ensure_home_visible();
                            }
                        }
                        NavigationDirection::Left => {
                            let _ = self.adjust_selected_setting(-1);
                        }
                        NavigationDirection::Right => {
                            let _ = self.adjust_selected_setting(1);
                        }
                    }
                    return;
                }
                if self.home_doc.cards.is_empty() {
                    return;
                }
                self.move_home_selection(direction);
                self.ensure_home_visible();
            }
            TuiFocus::Queue => {
                if self.snapshot.queue.is_empty() {
                    return;
                }
                let delta = match direction {
                    NavigationDirection::Left | NavigationDirection::Up => -1,
                    NavigationDirection::Right | NavigationDirection::Down => 1,
                };
                self.queue_selected =
                    move_index(self.queue_selected, self.snapshot.queue.len(), delta);
                self.ensure_queue_visible();
            }
            TuiFocus::Recent => {
                if self.recent_tracks.is_empty() {
                    return;
                }
                let delta = match direction {
                    NavigationDirection::Left | NavigationDirection::Up => -1,
                    NavigationDirection::Right | NavigationDirection::Down => 1,
                };
                self.recent_selected =
                    move_index(self.recent_selected, self.recent_tracks.len(), delta);
                self.ensure_recent_visible();
            }
            TuiFocus::Player => {
                let delta = match direction {
                    NavigationDirection::Left | NavigationDirection::Up => -1,
                    NavigationDirection::Right | NavigationDirection::Down => 1,
                };
                self.player_selected =
                    move_index(self.player_selected, PLAYER_ACTIONS.len(), delta);
            }
        }
    }

    pub(in crate::cli_tui) fn move_home_selection(&mut self, direction: NavigationDirection) {
        let Some(current) = self.home_doc.cards.get(self.home_selected) else {
            self.home_selected = 0;
            return;
        };

        let current_y = current.rect.y as i32;
        let current_center_x = current.rect.x as i32 + current.rect.width as i32 / 2;

        let mut best_index = self.home_selected;
        let mut best_score = (i32::MAX, i32::MAX);

        for (index, candidate) in self.home_doc.cards.iter().enumerate() {
            if index == self.home_selected {
                continue;
            }

            let candidate_y = candidate.rect.y as i32;
            let candidate_center_x = candidate.rect.x as i32 + candidate.rect.width as i32 / 2;
            let delta_x = candidate_center_x - current_center_x;
            let delta_y = candidate_y - current_y;

            let (primary, secondary) = match direction {
                NavigationDirection::Left => {
                    if delta_y != 0 || delta_x >= 0 {
                        continue;
                    }
                    (delta_x.abs(), 0)
                }
                NavigationDirection::Right => {
                    if delta_y != 0 || delta_x <= 0 {
                        continue;
                    }
                    (delta_x.abs(), 0)
                }
                NavigationDirection::Up => {
                    if delta_y >= 0 {
                        continue;
                    }
                    (delta_y.abs(), delta_x.abs())
                }
                NavigationDirection::Down => {
                    if delta_y <= 0 {
                        continue;
                    }
                    (delta_y.abs(), delta_x.abs())
                }
            };

            if (primary, secondary) < best_score {
                best_score = (primary, secondary);
                best_index = index;
            }
        }

        self.home_selected = best_index;
    }

    pub(in crate::cli_tui) fn page_move(&mut self, direction: isize) {
        match self.focus {
            TuiFocus::Home => {
                if matches!(self.content_view, TuiContentView::Settings(_)) {
                    let rows = (self.home_viewport_rows / 6).max(1) as isize;
                    let count = self.settings_field_offsets.len();
                    self.home_selected =
                        move_index(self.home_selected, count.max(1), direction * rows);
                    self.ensure_home_visible();
                    return;
                }
                let delta = direction * self.home_viewport_rows.max(1) as isize;
                self.scroll_home(delta);
            }
            TuiFocus::Queue => {
                let delta = direction * self.queue_viewport_rows.max(1) as isize;
                self.queue_selected =
                    move_index(self.queue_selected, self.snapshot.queue.len(), delta);
                self.ensure_queue_visible();
            }
            TuiFocus::Recent => {
                let delta = direction * self.recent_viewport_rows.max(1) as isize;
                self.recent_selected =
                    move_index(self.recent_selected, self.recent_tracks.len(), delta);
                self.ensure_recent_visible();
            }
            TuiFocus::Player => {
                self.player_selected =
                    move_index(self.player_selected, PLAYER_ACTIONS.len(), direction);
            }
        }
    }

    pub(in crate::cli_tui) fn jump_to_start(&mut self) {
        match self.focus {
            TuiFocus::Home => {
                self.home_selected = 0;
                self.home_scroll_offset = 0;
            }
            TuiFocus::Queue => {
                self.queue_selected = 0;
                self.queue_scroll_offset = 0;
            }
            TuiFocus::Recent => {
                self.recent_selected = 0;
                self.recent_scroll_offset = 0;
            }
            TuiFocus::Player => {
                self.player_selected = 0;
            }
        }
    }

    pub(in crate::cli_tui) fn jump_to_end(&mut self) {
        match self.focus {
            TuiFocus::Home => {
                self.home_selected = if matches!(self.content_view, TuiContentView::Settings(_)) {
                    self.settings_field_offsets.len().saturating_sub(1)
                } else {
                    self.home_doc.cards.len().saturating_sub(1)
                };
                self.ensure_home_visible();
            }
            TuiFocus::Queue => {
                self.queue_selected = self.snapshot.queue.len().saturating_sub(1);
                self.ensure_queue_visible();
            }
            TuiFocus::Recent => {
                self.recent_selected = self.recent_tracks.len().saturating_sub(1);
                self.ensure_recent_visible();
            }
            TuiFocus::Player => {
                self.player_selected = PLAYER_ACTIONS.len().saturating_sub(1);
            }
        }
    }

    pub(in crate::cli_tui) fn scroll_panel(&mut self, column: u16, row: u16, delta: isize) {
        if contains(self.rendered_layout.radio_chip_viewport, column, row) {
            self.scroll_radio_chips(delta.signum());
        } else if contains(self.rendered_layout.queue_viewport, column, row) {
            self.focus = TuiFocus::Queue;
            if self.detail_tab == DetailTab::Queue {
                self.queue_selected =
                    move_index(self.queue_selected, self.snapshot.queue.len(), delta);
                self.ensure_queue_visible();
            }
        } else if contains(self.rendered_layout.recent_viewport, column, row) {
            self.focus = TuiFocus::Recent;
            self.recent_selected =
                move_index(self.recent_selected, self.recent_tracks.len(), delta);
            self.ensure_recent_visible();
        } else {
            self.focus = TuiFocus::Home;
            self.scroll_home(delta);
        }
    }

    pub(in crate::cli_tui) fn seek_from_progress_click(&mut self, column: u16) -> Result<()> {
        let Some(duration) = self
            .snapshot
            .duration_millis
            .filter(|duration| *duration > 0)
        else {
            return Ok(());
        };
        let rect = self.rendered_layout.player_progress_viewport;
        if rect.width == 0 {
            return Ok(());
        }

        let relative = column
            .saturating_sub(rect.x)
            .min(rect.width.saturating_sub(1));
        let ratio = relative as f64 / rect.width.saturating_sub(1).max(1) as f64;
        let target = (duration as f64 * ratio) as u64;
        self.seek_to_millis(target);
        Ok(())
    }

    pub(in crate::cli_tui) fn seek_relative_millis(&mut self, delta_millis: i64) -> Result<()> {
        let Some(current_position) = self.displayed_position_millis() else {
            self.status_line = "Nothing seekable is playing right now.".to_string();
            return Ok(());
        };

        let upper_bound = self.snapshot.duration_millis.unwrap_or(u64::MAX) as i128;
        let target = (current_position as i128 + delta_millis as i128).clamp(0, upper_bound) as u64;
        self.seek_to_millis(target);
        Ok(())
    }

    pub(in crate::cli_tui) fn seek_to_millis(&mut self, target: u64) {
        match self.client.seek_to_millis(target) {
            Ok(snapshot) => {
                self.apply_snapshot(snapshot);
                self.status_line = format!("Seeked to {}", format_millis(target));
            }
            Err(err) => {
                self.status_line = format!("Seek failed: {err}");
            }
        }
    }

    pub(in crate::cli_tui) fn scroll_home(&mut self, delta: isize) {
        let total_height = if matches!(self.content_view, TuiContentView::Settings(_)) {
            self.settings_total_height
        } else {
            self.home_doc.total_height
        };
        let max_scroll = total_height.saturating_sub(self.home_viewport_rows.max(1));
        if delta < 0 {
            self.home_scroll_offset = self.home_scroll_offset.saturating_sub(delta.unsigned_abs());
        } else {
            self.home_scroll_offset = self
                .home_scroll_offset
                .saturating_add(delta as usize)
                .min(max_scroll);
        }
    }

    pub(in crate::cli_tui) fn scroll_radio_chips(&mut self, delta: isize) {
        let chip_count = self.current_radio_chips().len();
        if chip_count == 0 {
            self.radio_chip_scroll_offset = 0;
            return;
        }
        self.radio_chip_scroll_offset =
            move_index(self.radio_chip_scroll_offset, chip_count, delta);
    }

    pub(in crate::cli_tui) fn ensure_queue_visible(&mut self) {
        if self.snapshot.queue.is_empty() {
            self.queue_selected = 0;
            self.queue_scroll_offset = 0;
            return;
        }

        let viewport = self.queue_viewport_rows.max(1);
        if self.queue_selected < self.queue_scroll_offset {
            self.queue_scroll_offset = self.queue_selected;
        } else if self.queue_selected >= self.queue_scroll_offset.saturating_add(viewport) {
            self.queue_scroll_offset = self
                .queue_selected
                .saturating_sub(viewport.saturating_sub(1));
        }
        self.queue_scroll_offset = self
            .queue_scroll_offset
            .min(self.snapshot.queue.len().saturating_sub(viewport));
    }

    pub(in crate::cli_tui) fn ensure_recent_visible(&mut self) {
        if self.recent_tracks.is_empty() {
            self.recent_selected = 0;
            self.recent_scroll_offset = 0;
            return;
        }

        let viewport = self.recent_viewport_rows.max(1);
        if self.recent_selected < self.recent_scroll_offset {
            self.recent_scroll_offset = self.recent_selected;
        } else if self.recent_selected >= self.recent_scroll_offset.saturating_add(viewport) {
            self.recent_scroll_offset = self
                .recent_selected
                .saturating_sub(viewport.saturating_sub(1));
        }
        self.recent_scroll_offset = self
            .recent_scroll_offset
            .min(self.recent_tracks.len().saturating_sub(viewport));
    }

    pub(in crate::cli_tui) fn ensure_home_visible(&mut self) {
        if matches!(self.content_view, TuiContentView::Settings(_)) {
            let Some((field_top, field_height)) =
                self.settings_field_offsets.get(self.home_selected).copied()
            else {
                self.home_selected = 0;
                self.home_scroll_offset = 0;
                return;
            };

            let field_bottom = field_top.saturating_add(field_height);
            let viewport = self.home_viewport_rows.max(1);
            if field_top < self.home_scroll_offset {
                self.home_scroll_offset = field_top;
            } else if field_bottom > self.home_scroll_offset.saturating_add(viewport) {
                self.home_scroll_offset = field_bottom.saturating_sub(viewport);
            }
            self.clamp_home_scroll_offset();
            return;
        }

        let Some(card) = self.home_doc.cards.get(self.home_selected) else {
            self.home_selected = 0;
            self.home_scroll_offset = 0;
            return;
        };

        let card_top = card.rect.y as usize;
        let card_bottom = card_top.saturating_add(card.rect.height as usize);
        let viewport = self.home_viewport_rows.max(1);
        if card_top < self.home_scroll_offset {
            self.home_scroll_offset = card_top;
        } else if card_bottom > self.home_scroll_offset.saturating_add(viewport) {
            self.home_scroll_offset = card_bottom.saturating_sub(viewport);
        }
        self.clamp_home_scroll_offset();
    }

    pub(in crate::cli_tui) fn clamp_home_scroll_offset(&mut self) {
        let viewport = self.home_viewport_rows.max(1);
        let total_height = if matches!(self.content_view, TuiContentView::Settings(_)) {
            self.settings_total_height
        } else {
            self.home_doc.total_height
        };
        self.home_scroll_offset = self
            .home_scroll_offset
            .min(total_height.saturating_sub(viewport));
    }

    pub(in crate::cli_tui) fn visible_resume_card(&self) -> Option<&ResumeCardData> {
        self.resume_card
            .as_ref()
            .filter(|_| self.snapshot.current_track.is_none())
    }
}
