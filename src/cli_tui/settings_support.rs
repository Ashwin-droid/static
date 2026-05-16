use super::*;
use super::{content_loading::*, widgets::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn render_settings_page(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
        view: &TuiSettingsView,
    ) -> Vec<ClickTarget> {
        let mut targets = Vec::new();
        self.settings_field_offsets.clear();

        match &view.status {
            TuiSettingsStatus::Loading => {
                self.settings_total_height = 6;
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(
                            "Settings",
                            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(Span::styled(
                            "Loading current values from the local store...",
                            Style::default().fg(theme.muted),
                        )),
                    ]),
                    area,
                );
                return targets;
            }
            TuiSettingsStatus::Error(message) => {
                self.settings_total_height = 6;
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(
                            "Settings",
                            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(Span::styled(
                            message.clone(),
                            Style::default().fg(theme.danger),
                        )),
                    ])
                    .wrap(Wrap { trim: true }),
                    area,
                );
                return targets;
            }
            TuiSettingsStatus::Ready(page) => {
                let doc_width = area.width.saturating_sub(1).max(24);
                let hero_height = 6usize;
                let row_height = 5usize;
                let row_gap = 1usize;
                let section_gap = 1usize;
                let section_height = 2usize;

                self.settings_total_height = settings_document_height(
                    page,
                    hero_height,
                    row_height,
                    row_gap,
                    section_gap,
                    section_height,
                )
                .max(area.height as usize);
                let field_count = settings_field_count(page);
                if field_count > 0 {
                    self.home_selected = self.home_selected.min(field_count.saturating_sub(1));
                } else {
                    self.home_selected = 0;
                }
                self.clamp_home_scroll_offset();

                let visible_top = self.home_scroll_offset;
                let doc_height = self.settings_total_height.min(u16::MAX as usize) as u16;
                let doc_area = Rect::new(0, 0, doc_width, doc_height);
                let mut doc_buf = Buffer::empty(doc_area);
                fill_area(
                    &mut doc_buf,
                    doc_area,
                    Style::default().bg(theme.content_bg),
                );
                let mut y = 0usize;
                let mut field_index = 0usize;

                let hero_rect = Rect::new(0, y as u16, doc_width, hero_height as u16);
                let hero_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(theme.border_focus))
                    .style(Style::default().bg(theme.elevated_alt_bg))
                    .padding(Padding::new(1, 1, 0, 0));
                let hero_inner = hero_block.inner(hero_rect);
                render_buffer_widget(&mut doc_buf, hero_block, hero_rect);
                render_buffer_widget(
                    &mut doc_buf,
                    Paragraph::new(vec![
                        Line::from(Span::styled(
                            "Settings",
                            Style::default()
                                .fg(theme.text)
                                .add_modifier(Modifier::BOLD),
                        )),
                        Line::from(Span::styled(
                            "GTK-style controls in the TUI: toggles, dropdowns, text inputs, and numeric steppers.",
                            Style::default().fg(theme.muted),
                        )),
                        Line::from(Span::styled(
                            "Enter edits text fields. Use ←/→ or +/- for dropdowns and numbers.",
                            Style::default().fg(theme.accent),
                        )),
                    ])
                    .wrap(Wrap { trim: true }),
                    hero_inner,
                );
                y = y.saturating_add(hero_height + section_gap);

                for section in &page.sections {
                    render_buffer_widget(
                        &mut doc_buf,
                        Paragraph::new(vec![
                            Line::from(Span::styled(
                                section.title.clone(),
                                Style::default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD),
                            )),
                            Line::from(Span::styled(
                                section.subtitle.clone(),
                                Style::default().fg(theme.muted),
                            )),
                        ])
                        .wrap(Wrap { trim: true }),
                        Rect::new(0, y as u16, doc_width, section_height as u16),
                    );
                    y = y.saturating_add(section_height);

                    for field in &section.fields {
                        self.settings_field_offsets.push((y, row_height));
                        let is_selected =
                            self.focus == TuiFocus::Home && self.home_selected == field_index;
                        let row_rect = Rect::new(0, y as u16, doc_width, row_height as u16);
                        let row_block = Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(if is_selected {
                                theme.border_focus
                            } else {
                                theme.border_subtle
                            }))
                            .style(Style::default().bg(if is_selected {
                                theme.selected_bg
                            } else {
                                theme.elevated_bg
                            }))
                            .padding(Padding::new(1, 1, 0, 0));
                        let row_inner = row_block.inner(row_rect);
                        render_buffer_widget(&mut doc_buf, row_block, row_rect);

                        let columns = Layout::default()
                            .direction(Direction::Horizontal)
                            .constraints([
                                Constraint::Min(24),
                                Constraint::Length((doc_width / 3).clamp(18, 34)),
                            ])
                            .split(row_inner);
                        render_buffer_widget(
                            &mut doc_buf,
                            Paragraph::new(vec![
                                Line::from(Span::styled(
                                    field.title.clone(),
                                    Style::default()
                                        .fg(theme.text)
                                        .add_modifier(if is_selected {
                                            Modifier::BOLD
                                        } else {
                                            Modifier::empty()
                                        }),
                                )),
                                Line::from(Span::styled(
                                    shorten(&field.subtitle, columns[0].width as usize),
                                    Style::default().fg(theme.muted),
                                )),
                            ])
                            .wrap(Wrap { trim: true }),
                            columns[0],
                        );

                        match &field.control {
                            TuiSettingsControl::Toggle { key, value } => {
                                let label = if *value {
                                    "[x] Enabled"
                                } else {
                                    "[ ] Disabled"
                                };
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new(label)
                                        .style(Style::default().fg(theme.text))
                                        .alignment(Alignment::Center),
                                    columns[1],
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    columns[1],
                                    visible_top,
                                    ClickTargetKind::SettingsToggle(key),
                                );
                            }
                            TuiSettingsControl::Dropdown { key, value } => {
                                let controls = Layout::default()
                                    .direction(Direction::Horizontal)
                                    .constraints([
                                        Constraint::Length(5),
                                        Constraint::Min(8),
                                        Constraint::Length(5),
                                    ])
                                    .split(columns[1]);
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new("[<]")
                                        .style(Style::default().fg(theme.accent))
                                        .alignment(Alignment::Center),
                                    controls[0],
                                );
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new(format!("< {} >", value))
                                        .style(Style::default().fg(theme.text))
                                        .alignment(Alignment::Center),
                                    controls[1],
                                );
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new("[>]")
                                        .style(Style::default().fg(theme.accent))
                                        .alignment(Alignment::Center),
                                    controls[2],
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    controls[0],
                                    visible_top,
                                    ClickTargetKind::SettingsCycle(key, -1),
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    controls[2],
                                    visible_top,
                                    ClickTargetKind::SettingsCycle(key, 1),
                                );
                            }
                            TuiSettingsControl::Stepper { key, value } => {
                                let controls = Layout::default()
                                    .direction(Direction::Horizontal)
                                    .constraints([
                                        Constraint::Length(5),
                                        Constraint::Min(6),
                                        Constraint::Length(5),
                                    ])
                                    .split(columns[1]);
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new("[-]")
                                        .style(Style::default().fg(theme.accent))
                                        .alignment(Alignment::Center),
                                    controls[0],
                                );
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new(value.clone())
                                        .style(Style::default().fg(theme.text))
                                        .alignment(Alignment::Center),
                                    controls[1],
                                );
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new("[+]")
                                        .style(Style::default().fg(theme.accent))
                                        .alignment(Alignment::Center),
                                    controls[2],
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    controls[0],
                                    visible_top,
                                    ClickTargetKind::SettingsAdjust(key, -1),
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    controls[2],
                                    visible_top,
                                    ClickTargetKind::SettingsAdjust(key, 1),
                                );
                            }
                            TuiSettingsControl::TextInput {
                                key,
                                value,
                                placeholder,
                            } => {
                                let current_value = match &self.settings_text_edit {
                                    Some(edit)
                                        if matches!(
                                            edit.target,
                                            TuiSettingsTextTarget::Config(target_key) if target_key == *key
                                        ) =>
                                    {
                                        edit.buffer.clone()
                                    }
                                    _ => value.clone(),
                                };
                                let controls = Layout::default()
                                    .direction(Direction::Horizontal)
                                    .constraints([Constraint::Min(8), Constraint::Length(8)])
                                    .split(columns[1]);
                                let display_value = if current_value.is_empty() {
                                    placeholder.clone()
                                } else {
                                    current_value
                                };
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new(format!(
                                        "[ {} ]",
                                        shorten(
                                            &display_value,
                                            controls[0].width.saturating_sub(4) as usize
                                        )
                                    ))
                                    .style(
                                        Style::default().fg(if value.is_empty() {
                                            theme.muted
                                        } else {
                                            theme.text
                                        }),
                                    ),
                                    controls[0],
                                );
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new("[Edit]")
                                        .style(Style::default().fg(theme.accent))
                                        .alignment(Alignment::Center),
                                    controls[1],
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    controls[0],
                                    visible_top,
                                    ClickTargetKind::SettingsEdit(TuiSettingsTextTarget::Config(
                                        key,
                                    )),
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    controls[1],
                                    visible_top,
                                    ClickTargetKind::SettingsEdit(TuiSettingsTextTarget::Config(
                                        key,
                                    )),
                                );
                            }
                            TuiSettingsControl::SponsorCategory {
                                key,
                                label,
                                enabled,
                                ..
                            } => {
                                let checkbox = if *enabled { "[x]" } else { "[ ]" };
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new(format!("{checkbox} {label}"))
                                        .style(Style::default().fg(theme.text))
                                        .alignment(Alignment::Center),
                                    columns[1],
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    columns[1],
                                    visible_top,
                                    ClickTargetKind::SettingsSponsorCategory(key),
                                );
                            }
                            TuiSettingsControl::StorageRoot { value, placeholder } => {
                                let current_value = match &self.settings_text_edit {
                                    Some(edit)
                                        if matches!(
                                            edit.target,
                                            TuiSettingsTextTarget::StorageRoot
                                        ) =>
                                    {
                                        edit.buffer.clone()
                                    }
                                    _ => value.clone(),
                                };
                                let controls = Layout::default()
                                    .direction(Direction::Horizontal)
                                    .constraints([
                                        Constraint::Min(8),
                                        Constraint::Length(8),
                                        Constraint::Length(11),
                                    ])
                                    .split(columns[1]);
                                let display_value = if current_value.is_empty() {
                                    placeholder.clone()
                                } else {
                                    current_value
                                };
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new(format!(
                                        "[ {} ]",
                                        shorten(
                                            &display_value,
                                            controls[0].width.saturating_sub(4) as usize,
                                        )
                                    ))
                                    .style(
                                        Style::default().fg(if value.is_empty() {
                                            theme.muted
                                        } else {
                                            theme.text
                                        }),
                                    ),
                                    controls[0],
                                );
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new("[Edit]")
                                        .style(Style::default().fg(theme.accent))
                                        .alignment(Alignment::Center),
                                    controls[1],
                                );
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new("[Default]")
                                        .style(Style::default().fg(theme.accent))
                                        .alignment(Alignment::Center),
                                    controls[2],
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    controls[0],
                                    visible_top,
                                    ClickTargetKind::SettingsEdit(
                                        TuiSettingsTextTarget::StorageRoot,
                                    ),
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    controls[1],
                                    visible_top,
                                    ClickTargetKind::SettingsEdit(
                                        TuiSettingsTextTarget::StorageRoot,
                                    ),
                                );
                                push_settings_target(
                                    &mut targets,
                                    area,
                                    controls[2],
                                    visible_top,
                                    ClickTargetKind::SettingsStorageRootDefault,
                                );
                            }
                            TuiSettingsControl::ReadOnly { value } => {
                                render_buffer_widget(
                                    &mut doc_buf,
                                    Paragraph::new(format!(
                                        "[ {} ]",
                                        shorten(value, columns[1].width.saturating_sub(4) as usize),
                                    ))
                                    .style(Style::default().fg(theme.muted))
                                    .alignment(Alignment::Left),
                                    columns[1],
                                );
                            }
                        }

                        push_settings_target(
                            &mut targets,
                            area,
                            row_rect,
                            visible_top,
                            ClickTargetKind::SettingsField(field_index),
                        );

                        y = y.saturating_add(row_height + row_gap);
                        field_index += 1;
                    }

                    y = y.saturating_add(section_gap);
                }

                copy_buffer_viewport(&doc_buf, frame.buffer_mut(), area, visible_top);

                let mut scrollbar_state = ScrollbarState::new(self.settings_total_height)
                    .position(self.home_scroll_offset);
                frame.render_stateful_widget(
                    Scrollbar::default()
                        .orientation(ScrollbarOrientation::VerticalRight)
                        .thumb_style(Style::default().fg(theme.accent)),
                    area,
                    &mut scrollbar_state,
                );

                return targets;
            }
        }
    }

    pub(in crate::cli_tui) fn refresh_settings_view(&mut self) {
        if let TuiContentView::Settings(view) = &mut self.content_view {
            if let Ok(page) = load_settings_page_data() {
                view.status = TuiSettingsStatus::Ready(page);
            }
        }
    }

    pub(in crate::cli_tui) fn persist_setting_value(
        &mut self,
        key: &'static str,
        value: &str,
    ) -> Result<()> {
        let cache = open_tui_music_cache()?;
        let mut settings = cache.load_settings()?;
        apply_config_value(&mut settings, key, value)?;
        cache.save_settings(&settings)?;
        self.refresh_settings_view();
        match self.client.reload_settings() {
            Ok(snapshot) => self.apply_snapshot(snapshot),
            Err(err) => {
                self.status_line = format!("Saved {key}, but daemon reload needs attention: {err}");
            }
        }
        let rendered = format_config_value(&settings, key)?;
        self.status_line = format!("{key} = {rendered}");
        Ok(())
    }

    pub(in crate::cli_tui) fn toggle_boolean_setting(&mut self, key: &'static str) -> Result<()> {
        let cache = open_tui_music_cache()?;
        let settings = cache.load_settings()?;
        let next = if current_boolean_setting(&settings, key) {
            "false"
        } else {
            "true"
        };
        self.persist_setting_value(key, next)
    }

    pub(in crate::cli_tui) fn cycle_coordination_setting_delta(
        &mut self,
        key: &'static str,
        delta: isize,
    ) -> Result<()> {
        let cache = open_tui_music_cache()?;
        let settings = cache.load_settings()?;
        if let Some(current) = current_coordination_filter_setting(&settings, key) {
            let index = CoordinationFilterMode::ALL
                .iter()
                .position(|mode| *mode == current)
                .unwrap_or(0);
            let next_index = move_index(index, CoordinationFilterMode::ALL.len(), delta);
            return self
                .persist_setting_value(key, CoordinationFilterMode::ALL[next_index].as_str());
        }
        let current = current_coordination_setting(&settings, key);
        let index = CoordinationMode::ALL
            .iter()
            .position(|mode| *mode == current)
            .unwrap_or(0);
        let next_index = move_index(index, CoordinationMode::ALL.len(), delta);
        self.persist_setting_value(key, CoordinationMode::ALL[next_index].as_str())
    }

    pub(in crate::cli_tui) fn adjust_numeric_setting(
        &mut self,
        key: &'static str,
        delta: isize,
    ) -> Result<()> {
        let meta = numeric_setting_meta(key)
            .with_context(|| format!("No numeric metadata is defined for `{key}`"))?;
        let cache = open_tui_music_cache()?;
        let settings = cache.load_settings()?;
        let current = current_numeric_setting_value(&settings, key);
        let next = (current + delta as f64 * meta.step).clamp(meta.min, meta.max);
        let rendered = if meta.integer {
            format!("{}", next.round() as u32)
        } else {
            format!("{:.1}", next)
        };
        self.persist_setting_value(key, &rendered)
    }

    pub(in crate::cli_tui) fn toggle_sponsorblock_category(
        &mut self,
        key: &'static str,
    ) -> Result<()> {
        let cache = open_tui_music_cache()?;
        let settings = cache.load_settings()?;
        let mut categories = settings.sponsorblock_categories.clone();
        if categories.iter().any(|category| category == key) {
            categories.retain(|category| category != key);
        } else {
            categories.push(key.to_string());
        }
        self.persist_setting_value("sponsorblock_categories", &categories.join(","))
    }

    pub(in crate::cli_tui) fn start_settings_text_edit(&mut self, target: TuiSettingsTextTarget) {
        let Some(buffer) = self.settings_text_buffer_for_target(&target) else {
            self.status_line = "That field cannot be edited right now.".to_string();
            return;
        };
        self.settings_text_edit = Some(TuiSettingsTextEdit { target, buffer });
        self.status_line = "Editing setting inline. Enter saves, Esc cancels.".to_string();
    }

    pub(in crate::cli_tui) fn settings_text_buffer_for_target(
        &self,
        target: &TuiSettingsTextTarget,
    ) -> Option<String> {
        let TuiContentView::Settings(view) = &self.content_view else {
            return None;
        };
        let TuiSettingsStatus::Ready(page) = &view.status else {
            return None;
        };

        page.sections
            .iter()
            .flat_map(|section| section.fields.iter())
            .find_map(|field| match (&field.control, target) {
                (
                    TuiSettingsControl::TextInput { key, value, .. },
                    TuiSettingsTextTarget::Config(target_key),
                ) if key == target_key => Some(value.clone()),
                (
                    TuiSettingsControl::StorageRoot { value, .. },
                    TuiSettingsTextTarget::StorageRoot,
                ) => Some(value.clone()),
                _ => None,
            })
    }

    pub(in crate::cli_tui) fn save_settings_text_edit(
        &mut self,
        edit: TuiSettingsTextEdit,
    ) -> Result<()> {
        let trimmed = edit.buffer.trim().to_string();
        let result = match edit.target {
            TuiSettingsTextTarget::Config(key) => self.persist_setting_value(key, &trimmed),
            TuiSettingsTextTarget::StorageRoot => self.persist_storage_root_override(&trimmed),
        };

        match result {
            Ok(()) => {
                self.settings_text_edit = None;
                Ok(())
            }
            Err(err) => {
                self.status_line = format!("Save failed: {err}");
                Ok(())
            }
        }
    }

    pub(in crate::cli_tui) fn persist_storage_root_override(
        &mut self,
        raw_root: &str,
    ) -> Result<()> {
        let trimmed = raw_root.trim().to_string();
        let mut cache = open_tui_music_cache()?;
        cache.migrate_storage_root(&trimmed)?;
        if trimmed.is_empty() {
            delete_storage_root();
            self.status_line = "Static is back on the default storage root.".to_string();
        } else {
            store_storage_root(&trimmed)?;
            self.status_line = format!("Moved Static storage to {}.", shorten(&trimmed, 40));
        }
        self.refresh_settings_view();
        Ok(())
    }

    pub(in crate::cli_tui) fn reset_storage_root_override(&mut self) -> Result<()> {
        self.persist_storage_root_override("")
    }

    pub(in crate::cli_tui) fn selected_settings_field(&self) -> Option<TuiSettingsField> {
        let TuiContentView::Settings(view) = &self.content_view else {
            return None;
        };
        let TuiSettingsStatus::Ready(page) = &view.status else {
            return None;
        };
        page.sections
            .iter()
            .flat_map(|section| section.fields.iter().cloned())
            .nth(self.home_selected)
    }

    pub(in crate::cli_tui) fn activate_selected_setting(&mut self) -> Result<()> {
        let Some(field) = self.selected_settings_field() else {
            self.status_line = "Settings are still loading.".to_string();
            return Ok(());
        };

        match field.control {
            TuiSettingsControl::Toggle { key, .. } => self.toggle_boolean_setting(key),
            TuiSettingsControl::Dropdown { key, .. } => {
                self.cycle_coordination_setting_delta(key, 1)
            }
            TuiSettingsControl::Stepper { .. } => {
                self.status_line = "Use ←/→ or +/- to adjust numeric settings.".to_string();
                Ok(())
            }
            TuiSettingsControl::TextInput { key, .. } => {
                self.start_settings_text_edit(TuiSettingsTextTarget::Config(key));
                Ok(())
            }
            TuiSettingsControl::SponsorCategory { key, .. } => {
                self.toggle_sponsorblock_category(key)
            }
            TuiSettingsControl::StorageRoot { .. } => {
                self.start_settings_text_edit(TuiSettingsTextTarget::StorageRoot);
                Ok(())
            }
            TuiSettingsControl::ReadOnly { value } => {
                self.status_line = shorten(&value, 96);
                Ok(())
            }
        }
    }
}

fn settings_document_height(
    page: &TuiSettingsPageData,
    hero_height: usize,
    row_height: usize,
    row_gap: usize,
    section_gap: usize,
    section_height: usize,
) -> usize {
    let mut y = hero_height.saturating_add(section_gap);
    for section in &page.sections {
        y = y.saturating_add(section_height);
        for _ in &section.fields {
            y = y.saturating_add(row_height + row_gap);
        }
        y = y.saturating_add(section_gap);
    }
    y
}

fn settings_field_count(page: &TuiSettingsPageData) -> usize {
    page.sections
        .iter()
        .map(|section| section.fields.len())
        .sum()
}

fn render_buffer_widget<W: Widget>(buf: &mut Buffer, widget: W, area: Rect) {
    widget.render(area, buf);
}

fn push_settings_target(
    targets: &mut Vec<ClickTarget>,
    viewport: Rect,
    logical_rect: Rect,
    scroll_offset: usize,
    kind: ClickTargetKind,
) {
    if let Some(rect) = scrolled_visible_rect(viewport, logical_rect, scroll_offset) {
        targets.push(ClickTarget { rect, kind });
    }
}

fn copy_buffer_viewport(
    source: &Buffer,
    target: &mut Buffer,
    viewport: Rect,
    scroll_offset: usize,
) {
    let available_rows = source
        .area
        .height
        .saturating_sub(scroll_offset.min(source.area.height as usize) as u16);
    let rows = viewport.height.min(available_rows);
    let columns = viewport.width.min(source.area.width);

    for row in 0..rows {
        let source_y = scroll_offset as u16 + row;
        let target_y = viewport.y + row;
        for column in 0..columns {
            let source_x = source.area.x + column;
            let target_x = viewport.x + column;
            let Some(source_cell) = source.cell((source_x, source_y)).cloned() else {
                continue;
            };
            if let Some(target_cell) = target.cell_mut((target_x, target_y)) {
                *target_cell = source_cell;
            }
        }
    }
}

pub(in crate::cli_tui) struct NumericSettingMeta {
    min: f64,
    max: f64,
    step: f64,
    integer: bool,
}

pub(in crate::cli_tui) fn toggle_setting_field(
    settings: &AppSettings,
    key: &'static str,
    title: &str,
    subtitle: &str,
) -> TuiSettingsField {
    TuiSettingsField {
        title: title.to_string(),
        subtitle: subtitle.to_string(),
        control: TuiSettingsControl::Toggle {
            key,
            value: current_boolean_setting(settings, key),
        },
    }
}

pub(in crate::cli_tui) fn dropdown_setting_field(
    settings: &AppSettings,
    key: &'static str,
    title: &str,
    subtitle: &str,
) -> TuiSettingsField {
    TuiSettingsField {
        title: title.to_string(),
        subtitle: subtitle.to_string(),
        control: TuiSettingsControl::Dropdown {
            key,
            value: current_coordination_setting(settings, key)
                .label()
                .to_string(),
        },
    }
}

pub(in crate::cli_tui) fn filter_mode_setting_field(
    settings: &AppSettings,
    key: &'static str,
    title: &str,
    subtitle: &str,
) -> TuiSettingsField {
    TuiSettingsField {
        title: title.to_string(),
        subtitle: subtitle.to_string(),
        control: TuiSettingsControl::Dropdown {
            key,
            value: current_coordination_filter_setting(settings, key)
                .unwrap_or(CoordinationFilterMode::Blacklist)
                .label()
                .to_string(),
        },
    }
}

pub(in crate::cli_tui) fn stepper_setting_field(
    settings: &AppSettings,
    key: &'static str,
    title: &str,
    subtitle: &str,
) -> Result<TuiSettingsField> {
    Ok(TuiSettingsField {
        title: title.to_string(),
        subtitle: subtitle.to_string(),
        control: TuiSettingsControl::Stepper {
            key,
            value: format_config_value(settings, key)?,
        },
    })
}

pub(in crate::cli_tui) fn text_setting_field(
    settings: &AppSettings,
    key: &'static str,
    title: &str,
    subtitle: &str,
    placeholder: &str,
) -> Result<TuiSettingsField> {
    Ok(TuiSettingsField {
        title: title.to_string(),
        subtitle: subtitle.to_string(),
        control: TuiSettingsControl::TextInput {
            key,
            value: format_config_value(settings, key)?,
            placeholder: placeholder.to_string(),
        },
    })
}

pub(in crate::cli_tui) fn current_boolean_setting(settings: &AppSettings, key: &str) -> bool {
    match key {
        "auto_sync_chromium_cookies" => settings.auto_sync_chromium_cookies,
        "auto_release_track_loops" => settings.auto_release_track_loops,
        "auto_manage_offline_cache" => settings.auto_manage_offline_cache,
        "auto_cache_listen_again" => settings.auto_cache_listen_again,
        "prefetch_enabled" => settings.prefetch_enabled,
        "prefer_local_cache_on_playback" => settings.prefer_local_cache_on_playback,
        "pipewire_ducking_enabled" => settings.pipewire_ducking_enabled,
        "sponsorblock_enabled" => settings.sponsorblock_enabled,
        _ => false,
    }
}

pub(in crate::cli_tui) fn current_coordination_setting(
    settings: &AppSettings,
    key: &str,
) -> CoordinationMode {
    match key {
        "other_mpris_mode" => settings.other_mpris_mode,
        "pipewire_pause_mode" => settings.pipewire_pause_mode,
        _ => CoordinationMode::Off,
    }
}

pub(in crate::cli_tui) fn current_coordination_filter_setting(
    settings: &AppSettings,
    key: &str,
) -> Option<CoordinationFilterMode> {
    match key {
        "other_mpris_filter_mode" => Some(settings.other_mpris_filter_mode),
        "pipewire_filter_mode" => Some(settings.pipewire_filter_mode),
        _ => None,
    }
}

pub(in crate::cli_tui) fn current_numeric_setting_value(settings: &AppSettings, key: &str) -> f64 {
    match key {
        "auto_release_track_loops_after" => settings.auto_release_track_loops_after as f64,
        "listen_again_cache_days" => settings.listen_again_cache_days as f64,
        "offline_cache_target" => settings.offline_cache_target as f64,
        "offline_rewatch_threshold" => settings.offline_rewatch_threshold as f64,
        "score_decay_window_days" => settings.score_decay_window_days as f64,
        "minimum_offline_score" => settings.minimum_offline_score,
        "prefetch_ahead" => settings.prefetch_ahead as f64,
        "other_mpris_halt_auto_resume_secs" => settings.other_mpris_halt_auto_resume_secs as f64,
        "pipewire_pause_halt_auto_resume_secs" => {
            settings.pipewire_pause_halt_auto_resume_secs as f64
        }
        "pipewire_ducking_threshold_secs" => settings.pipewire_ducking_threshold_secs as f64,
        "pipewire_ducking_volume" => settings.pipewire_ducking_volume,
        _ => 0.0,
    }
}

pub(in crate::cli_tui) fn numeric_setting_meta(key: &str) -> Option<NumericSettingMeta> {
    match key {
        "auto_release_track_loops_after" => Some(NumericSettingMeta {
            min: 1.0,
            max: 100.0,
            step: 1.0,
            integer: true,
        }),
        "listen_again_cache_days" => Some(NumericSettingMeta {
            min: 1.0,
            max: 365.0,
            step: 1.0,
            integer: true,
        }),
        "offline_cache_target" => Some(NumericSettingMeta {
            min: 0.0,
            max: 500.0,
            step: 5.0,
            integer: true,
        }),
        "offline_rewatch_threshold" => Some(NumericSettingMeta {
            min: 0.0,
            max: 50.0,
            step: 1.0,
            integer: true,
        }),
        "score_decay_window_days" => Some(NumericSettingMeta {
            min: 1.0,
            max: 365.0,
            step: 1.0,
            integer: true,
        }),
        "minimum_offline_score" => Some(NumericSettingMeta {
            min: 0.0,
            max: 999.0,
            step: 0.1,
            integer: false,
        }),
        "prefetch_ahead" => Some(NumericSettingMeta {
            min: 0.0,
            max: 12.0,
            step: 1.0,
            integer: true,
        }),
        "other_mpris_halt_auto_resume_secs" => Some(NumericSettingMeta {
            min: 0.0,
            max: 60.0,
            step: 1.0,
            integer: true,
        }),
        "pipewire_pause_halt_auto_resume_secs" => Some(NumericSettingMeta {
            min: 0.0,
            max: 60.0,
            step: 1.0,
            integer: true,
        }),
        "pipewire_ducking_threshold_secs" => Some(NumericSettingMeta {
            min: 1.0,
            max: 30.0,
            step: 1.0,
            integer: true,
        }),
        "pipewire_ducking_volume" => Some(NumericSettingMeta {
            min: 0.0,
            max: 1.0,
            step: 0.1,
            integer: false,
        }),
        _ => None,
    }
}

fn coordination_target_list_value(targets: Option<&[CoordinationTargetSnapshot]>) -> String {
    let Some(targets) = targets else {
        return "Unavailable".to_string();
    };

    if targets.is_empty() {
        return "None".to_string();
    }

    targets
        .iter()
        .take(6)
        .map(|target| {
            if target.filterable {
                format!("{} -> {}", target.label, target.key)
            } else {
                format!("{} -> no stable key", target.label)
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

pub(in crate::cli_tui) fn build_tui_settings_sections(
    settings: &AppSettings,
    cache: &MusicCache,
    coordination_targets: Option<&CoordinationTargetsSnapshot>,
) -> Result<Vec<TuiSettingsSection>> {
    let storage_root_override = get_storage_root().unwrap_or_default();
    let default_storage_root = std::env::var("HOME")
        .map(|home| format!("{home}/Music/Static"))
        .unwrap_or_else(|_| "/tmp/Music/Static".to_string());
    let mut coordination_fields = vec![
        dropdown_setting_field(
            settings,
            "other_mpris_mode",
            "MPRIS coordination",
            "Choose whether Static halts, auto-resumes, or ignores peer players.",
        ),
        stepper_setting_field(
            settings,
            "other_mpris_halt_auto_resume_secs",
            "Halt auto delay",
            "Delay before Static resumes after peer media goes quiet.",
        )?,
        filter_mode_setting_field(
            settings,
            "other_mpris_filter_mode",
            "MPRIS filter mode",
            "Switch between blacklist and whitelist matching for peer media apps.",
        ),
        text_setting_field(
            settings,
            "other_mpris_filter_list",
            "MPRIS filter list",
            "Comma-separated app-level identifiers matched against app key, identity, and desktop entry.",
            "No MPRIS entries",
        )?,
        TuiSettingsField {
            title: "Current MPRIS players".to_string(),
            subtitle: "Keys you can add to the MPRIS filter list.".to_string(),
            control: TuiSettingsControl::ReadOnly {
                value: coordination_target_list_value(
                    coordination_targets.map(|targets| targets.mpris.as_slice()),
                ),
            },
        },
        toggle_setting_field(
            settings,
            "pipewire_ducking_enabled",
            "PipeWire ducking",
            "Lower or pause Static when other desktop audio becomes active.",
        ),
        dropdown_setting_field(
            settings,
            "pipewire_pause_mode",
            "PipeWire coordination",
            "Choose whether PipeWire activity ducks, halts, or auto-resumes.",
        ),
        stepper_setting_field(
            settings,
            "pipewire_pause_halt_auto_resume_secs",
            "PipeWire halt auto delay",
            "Delay before Static comes back after PipeWire activity stops.",
        )?,
        stepper_setting_field(
            settings,
            "pipewire_ducking_threshold_secs",
            "PipeWire trigger delay",
            "How long foreign audio must stay active before coordination starts.",
        )?,
        stepper_setting_field(
            settings,
            "pipewire_ducking_volume",
            "PipeWire duck volume",
            "Temporary playback level while foreign audio is active.",
        )?,
        filter_mode_setting_field(
            settings,
            "pipewire_filter_mode",
            "PipeWire filter mode",
            "Switch between blacklist and whitelist matching for foreign audio streams.",
        ),
        text_setting_field(
            settings,
            "pipewire_filter_list",
            "PipeWire filter list",
            "Comma-separated stable identifiers matched against app name, binary, media, and node labels.",
            "No PipeWire entries",
        )?,
        TuiSettingsField {
            title: "Current PipeWire streams".to_string(),
            subtitle: "Keys you can add to the PipeWire filter list.".to_string(),
            control: TuiSettingsControl::ReadOnly {
                value: coordination_target_list_value(
                    coordination_targets.map(|targets| targets.pipewire.as_slice()),
                ),
            },
        },
    ];
    if coordination_targets.is_none() {
        coordination_fields.push(TuiSettingsField {
            title: "Target discovery".to_string(),
            subtitle: "Open the daemon-backed settings page again after staticd starts."
                .to_string(),
            control: TuiSettingsControl::ReadOnly {
                value: "No daemon target snapshot available".to_string(),
            },
        });
    }

    Ok(vec![
        TuiSettingsSection {
            title: "Playback & Cache".to_string(),
            subtitle: "How local cache, loops, and prefetch behave.".to_string(),
            fields: vec![
                toggle_setting_field(
                    settings,
                    "prefer_local_cache_on_playback",
                    "Prefer local cache on playback",
                    "Play offline or prefetched audio first when it exists, then fall back to streaming.",
                ),
                toggle_setting_field(
                    settings,
                    "auto_release_track_loops",
                    "Auto-release Track loops",
                    "Release repeat-one automatically after a few natural loops.",
                ),
                stepper_setting_field(
                    settings,
                    "auto_release_track_loops_after",
                    "Auto-release Track loops after",
                    "How many natural self-loops happen before Track repeat turns off.",
                )?,
                toggle_setting_field(
                    settings,
                    "prefetch_enabled",
                    "Prefetch upcoming tracks",
                    "Warm the local temp cache ahead of the queue.",
                ),
                stepper_setting_field(
                    settings,
                    "prefetch_ahead",
                    "Prefetch ahead",
                    "How many tracks ahead Static stages locally.",
                )?,
            ],
        },
        TuiSettingsSection {
            title: "Offline Manager".to_string(),
            subtitle: "Automatic offline scoring and retention.".to_string(),
            fields: vec![
                toggle_setting_field(
                    settings,
                    "auto_manage_offline_cache",
                    "Auto-manage offline cache",
                    "Let Static decide what to keep downloaded automatically.",
                ),
                toggle_setting_field(
                    settings,
                    "auto_cache_listen_again",
                    "Auto-cache Listen again",
                    "Bias the manager toward recently resurfacing songs.",
                ),
                stepper_setting_field(
                    settings,
                    "listen_again_cache_days",
                    "Listen again window",
                    "How long Listen again activity influences auto-downloads.",
                )?,
                stepper_setting_field(
                    settings,
                    "offline_cache_target",
                    "Offline cache target",
                    "Approximate number of auto-managed tracks to keep around.",
                )?,
                stepper_setting_field(
                    settings,
                    "offline_rewatch_threshold",
                    "Offline after rewatches",
                    "How many replays make a song eligible for heuristic offline caching.",
                )?,
                stepper_setting_field(
                    settings,
                    "score_decay_window_days",
                    "Score decay window",
                    "How slowly songs cool off after their last play.",
                )?,
                stepper_setting_field(
                    settings,
                    "minimum_offline_score",
                    "Minimum offline score",
                    "Anything below this score is ignored or evicted.",
                )?,
            ],
        },
        TuiSettingsSection {
            title: "Coordination".to_string(),
            subtitle: "How Static reacts to other media and audio streams.".to_string(),
            fields: coordination_fields,
        },
        TuiSettingsSection {
            title: "Import & Filtering".to_string(),
            subtitle: "Cookie import, SponsorBlock, and local paths.".to_string(),
            fields: {
                let mut fields = vec![
                    toggle_setting_field(
                        settings,
                        "auto_sync_chromium_cookies",
                        "Auto-sync Chromium cookies",
                        "Pull cookies from Chromium-family browsers at startup if needed.",
                    ),
                    text_setting_field(
                        settings,
                        "chromium_cookie_path",
                        "Chromium cookie DB path",
                        "Leave empty to auto-detect the browser profile.",
                        "Auto-detect browser profile",
                    )?,
                    toggle_setting_field(
                        settings,
                        "sponsorblock_enabled",
                        "SponsorBlock enabled",
                        "Skip configured sponsor or filler segments automatically.",
                    ),
                ];
                for option in SPONSORBLOCK_CATEGORY_OPTIONS {
                    fields.push(TuiSettingsField {
                        title: format!("Skip {}", option.label),
                        subtitle: option.description.to_string(),
                        control: TuiSettingsControl::SponsorCategory {
                            key: option.key,
                            label: option.label,
                            enabled: settings
                                .sponsorblock_categories
                                .iter()
                                .any(|category| category == option.key),
                        },
                    });
                }
                fields.push(TuiSettingsField {
                    title: "Static storage root".to_string(),
                    subtitle: "Move offline audio, artwork, and temp cache together. Leave blank to use the default root.".to_string(),
                    control: TuiSettingsControl::StorageRoot {
                        value: storage_root_override,
                        placeholder: default_storage_root,
                    },
                });
                fields.push(TuiSettingsField {
                    title: "Cache root".to_string(),
                    subtitle: "Active cache database and artwork location.".to_string(),
                    control: TuiSettingsControl::ReadOnly {
                        value: cache.base_dir().display().to_string(),
                    },
                });
                fields.push(TuiSettingsField {
                    title: "Offline store".to_string(),
                    subtitle: "Audio files available without the network.".to_string(),
                    control: TuiSettingsControl::ReadOnly {
                        value: cache.offline_dir().display().to_string(),
                    },
                });
                fields.push(TuiSettingsField {
                    title: "Temp prefetch cache".to_string(),
                    subtitle: "Short-lived prefetched media storage.".to_string(),
                    control: TuiSettingsControl::ReadOnly {
                        value: cache.temp_dir().display().to_string(),
                    },
                });
                fields
            },
        },
    ])
}
