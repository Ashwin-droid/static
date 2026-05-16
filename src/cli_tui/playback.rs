use super::*;
use super::{content_loading::*, widgets::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn render_player_bar(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
        layout: &mut RenderedLayout,
    ) -> Vec<ClickTarget> {
        let mut targets = Vec::new();
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme.border_subtle))
            .style(Style::default().bg(theme.player_bg));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(4)])
            .split(inner);

        let progress_ratio = self
            .snapshot
            .duration_millis
            .filter(|duration| *duration > 0)
            .map(|duration| {
                self.displayed_position_millis().unwrap_or(0).min(duration) as f64 / duration as f64
            })
            .unwrap_or(0.0);
        layout.player_progress_viewport = rows[0];
        frame.render_widget(
            Gauge::default()
                .ratio(progress_ratio)
                .gauge_style(Style::default().fg(theme.red).bg(theme.border_subtle))
                .label(format!(
                    "{} / {}",
                    self.displayed_position_millis()
                        .map(format_millis)
                        .unwrap_or_else(|| "0:00".to_string()),
                    self.snapshot
                        .duration_millis
                        .map(format_millis)
                        .unwrap_or_else(|| "--:--".to_string())
                )),
            rows[0],
        );

        let action_column_width = if self.snapshot.halt_auto_resume_pending {
            44
        } else {
            28
        };
        let control_columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(16),
                Constraint::Min(18),
                Constraint::Length(action_column_width),
            ])
            .split(rows[1]);

        let transport = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(5),
                Constraint::Length(6),
                Constraint::Length(5),
            ])
            .split(control_columns[0]);
        targets.push(render_action_button(
            frame,
            transport[0],
            "⏮",
            TuiAction::Previous,
            theme,
            false,
            false,
            self.player_action_focused_action(TuiAction::Previous),
        ));
        targets.push(render_action_button(
            frame,
            transport[1],
            if self.snapshot.paused { "▶" } else { "⏸" },
            TuiAction::TogglePause,
            theme,
            true,
            true,
            self.player_action_focused_action(TuiAction::TogglePause),
        ));
        targets.push(render_action_button(
            frame,
            transport[2],
            "⏭",
            TuiAction::Next,
            theme,
            false,
            false,
            self.player_action_focused_action(TuiAction::Next),
        ));

        let track_info = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(self.square_box_width_for_height(rows[1].height).max(6)),
                Constraint::Min(16),
            ])
            .split(control_columns[1]);

        let current_thumb_url = self
            .snapshot
            .current_track
            .as_ref()
            .and_then(|track| track.thumb_url.clone());
        self.render_cover_thumbnail(
            frame,
            track_info[0],
            ThumbnailSlot::PlayerBar,
            current_thumb_url.as_deref(),
            current_initials(&self.snapshot),
            theme,
            true,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    shorten(
                        &self
                            .snapshot
                            .current_track
                            .as_ref()
                            .map(|track| track.title.clone())
                            .unwrap_or_else(|| "Nothing playing".to_string()),
                        track_info[1].width.saturating_sub(1) as usize,
                    ),
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    shorten(
                        &self
                            .snapshot
                            .current_track
                            .as_ref()
                            .map(|track| track.artist.clone())
                            .unwrap_or_else(|| "Static daemon is ready".to_string()),
                        track_info[1].width.saturating_sub(1) as usize,
                    ),
                    Style::default().fg(theme.muted),
                )),
                Line::from(player_bar_meta_spans(&self.snapshot)),
            ])
            .alignment(Alignment::Left),
            track_info[1],
        );

        let actions = if self.snapshot.halt_auto_resume_pending {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Length(5),
                    Constraint::Length(5),
                    Constraint::Length(5),
                    Constraint::Length(4),
                    Constraint::Length(5),
                    Constraint::Length(4),
                ])
                .split(control_columns[2])
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(5),
                    Constraint::Length(5),
                    Constraint::Length(5),
                    Constraint::Length(4),
                    Constraint::Length(5),
                    Constraint::Length(4),
                ])
                .split(control_columns[2])
        };

        let mut action_index = 0;

        if self.snapshot.halt_auto_resume_pending {
            targets.push(render_action_button(
                frame,
                actions[action_index],
                "cancel",
                TuiAction::CancelHaltAutoResume,
                theme,
                false,
                true,
                self.player_action_focused_action(TuiAction::CancelHaltAutoResume),
            ));
            action_index += 1;
            targets.push(render_action_button(
                frame,
                actions[action_index],
                "resume",
                TuiAction::ResumeHaltAutoResume,
                theme,
                true,
                true,
                self.player_action_focused_action(TuiAction::ResumeHaltAutoResume),
            ));
            action_index += 1;
        }

        targets.push(render_action_button(
            frame,
            actions[action_index],
            "👎",
            TuiAction::Dislike,
            theme,
            false,
            self.snapshot
                .current_track
                .as_ref()
                .is_some_and(|track| track.rating == SongRating::Dislike),
            self.player_action_focused_action(TuiAction::Dislike),
        ));
        action_index += 1;
        targets.push(render_action_button(
            frame,
            actions[action_index],
            "👍",
            TuiAction::Like,
            theme,
            false,
            self.snapshot
                .current_track
                .as_ref()
                .is_some_and(|track| track.rating == SongRating::Like),
            self.player_action_focused_action(TuiAction::Like),
        ));
        action_index += 1;
        targets.push(render_action_button(
            frame,
            actions[action_index],
            repeat_button_emoji(self.snapshot.repeat_mode),
            TuiAction::CycleRepeat,
            theme,
            false,
            self.snapshot.repeat_mode != RepeatMode::Off,
            self.player_action_focused_action(TuiAction::CycleRepeat),
        ));
        action_index += 1;
        targets.push(render_action_button(
            frame,
            actions[action_index],
            "−",
            TuiAction::VolumeDown,
            theme,
            false,
            false,
            self.player_action_focused_action(TuiAction::VolumeDown),
        ));
        action_index += 1;

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    format!("{:.0}%", (self.snapshot.volume * 100.0).clamp(0.0, 100.0)),
                    Style::default().fg(theme.text),
                )),
                Line::from(Span::styled("vol", Style::default().fg(theme.muted))),
            ])
            .alignment(Alignment::Center),
            actions[action_index],
        );
        action_index += 1;

        targets.push(render_action_button(
            frame,
            actions[action_index],
            "+",
            TuiAction::VolumeUp,
            theme,
            false,
            false,
            self.player_action_focused_action(TuiAction::VolumeUp),
        ));

        targets
    }

    pub(in crate::cli_tui) fn apply_snapshot(&mut self, snapshot: PlaybackSnapshot) {
        let previous_video_id = self
            .snapshot
            .current_track
            .as_ref()
            .map(|track| track.video_id.clone());
        let next_video_id = snapshot
            .current_track
            .as_ref()
            .map(|track| track.video_id.clone());

        self.snapshot = snapshot;
        let now = Instant::now();
        self.last_snapshot_at = now;
        self.last_daemon_poll_at = now;
        self.queue_selected = self
            .queue_selected
            .min(self.snapshot.queue.len().saturating_sub(1));

        if previous_video_id != next_video_id {
            self.maybe_request_lyrics();
        }
    }

    pub(in crate::cli_tui) fn current_radio_chips(&self) -> Vec<RadioChip> {
        self.snapshot
            .radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.chips.clone())
            .unwrap_or_default()
    }

    pub(in crate::cli_tui) fn playback_snapshot_from_tui_state(&self) -> PlaybackSnapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.halt_auto_resume_pending = false;
        snapshot.queue_index = snapshot
            .queue_index
            .min(snapshot.queue.len().saturating_sub(1));

        if snapshot.probe_active {
            let fallback_rating = snapshot
                .current_track
                .as_ref()
                .map(|track| track.rating)
                .unwrap_or(SongRating::Indifferent);
            snapshot.current_track = snapshot
                .queue
                .get(snapshot.queue_index)
                .map(|track| active_track_snapshot_from_track(track, fallback_rating));
            snapshot.position_millis = snapshot.probe_restore_position_millis;
            snapshot.duration_millis = None;
            snapshot.buffered_fraction = 0.0;
            snapshot.paused = true;
            snapshot.probe_active = false;
            snapshot.probe_restore_position_millis = None;
        } else {
            snapshot.position_millis = self
                .displayed_position_millis()
                .or(snapshot.position_millis);
        }

        if snapshot.current_track.is_none() {
            snapshot.current_track = snapshot
                .queue
                .get(snapshot.queue_index)
                .map(|track| active_track_snapshot_from_track(track, SongRating::Indifferent));
        }

        snapshot
    }

    pub(in crate::cli_tui) fn recover_daemon_from_tui_state(&mut self) -> Result<()> {
        let snapshot = self.playback_snapshot_from_tui_state();
        let has_recoverable_state = snapshot.current_track.is_some() || !snapshot.queue.is_empty();

        let restored = if has_recoverable_state {
            restore_playback_snapshot_via_daemon(
                snapshot.clone(),
                snapshot.current_track.is_some(),
            )?
        } else {
            let client = DaemonClient::new()?;
            client.state()?
        };

        self.client = DaemonClient::new()?;
        self.apply_snapshot(restored);
        self.status_line = if has_recoverable_state {
            "staticd restarted from the TUI state.".to_string()
        } else {
            "staticd restarted.".to_string()
        };
        Ok(())
    }

    pub(in crate::cli_tui) fn maybe_request_lyrics(&mut self) {
        let Some(video_id) = self
            .snapshot
            .current_track
            .as_ref()
            .map(|track| track.video_id.clone())
        else {
            self.lyrics_state = LyricsState::Idle;
            return;
        };

        let already_loaded = match &self.lyrics_state {
            LyricsState::Loading {
                video_id: current_video_id,
            }
            | LyricsState::Ready {
                video_id: current_video_id,
                ..
            }
            | LyricsState::Missing {
                video_id: current_video_id,
                ..
            }
            | LyricsState::Error {
                video_id: current_video_id,
                ..
            } => current_video_id == &video_id,
            LyricsState::Idle => false,
        };

        if already_loaded {
            return;
        }

        self.lyrics_state = LyricsState::Loading {
            video_id: video_id.clone(),
        };
        spawn_lyrics_load(self.background_tx.clone(), video_id);
    }

    pub(in crate::cli_tui) fn displayed_position_millis(&self) -> Option<u64> {
        let position_millis = self.snapshot.position_millis?;
        if self.snapshot.paused
            || self.snapshot.probe_active
            || self.snapshot.current_track.is_none()
        {
            return Some(position_millis);
        }

        let advanced =
            position_millis.saturating_add(self.last_snapshot_at.elapsed().as_millis() as u64);
        Some(match self.snapshot.duration_millis {
            Some(duration_millis) => advanced.min(duration_millis),
            None => advanced,
        })
    }

    pub(in crate::cli_tui) fn toggle_detail_tab(&mut self) {
        self.detail_tab = match self.detail_tab {
            DetailTab::Queue => DetailTab::Lyrics,
            DetailTab::Lyrics => DetailTab::Queue,
        };
    }

    pub(in crate::cli_tui) fn run_action_request(
        &mut self,
        action: TuiAction,
    ) -> Result<PlaybackSnapshot> {
        match action {
            TuiAction::ResumeSuggested => {
                let source = self
                    .resume_card
                    .as_ref()
                    .map(|card| card.source.clone())
                    .ok_or_else(|| anyhow::anyhow!("No resume state is available"))?;
                match source {
                    ResumeCardSource::Local(snapshot) => {
                        restore_playback_snapshot_via_daemon(snapshot, true)
                    }
                    ResumeCardSource::CrossDevice(queue) => {
                        restore_cross_device_queue_via_daemon(queue)
                    }
                }
            }
            TuiAction::TogglePause => self.client.toggle_pause(),
            TuiAction::CancelHaltAutoResume => self.client.cancel_halt_auto_resume(),
            TuiAction::ResumeHaltAutoResume => self.client.resume_halt_auto(),
            TuiAction::Previous => self.client.previous(),
            TuiAction::Next => self.client.next(),
            TuiAction::Like => self.client.rate_current_track(SongRating::Like),
            TuiAction::Dislike => self.client.rate_current_track(SongRating::Dislike),
            TuiAction::Neutral => self.client.rate_current_track(SongRating::Indifferent),
            TuiAction::CycleRepeat => {
                let next_mode = self
                    .snapshot
                    .repeat_mode
                    .next(self.snapshot.radio_virtual_playlist.is_some());
                self.client.set_repeat_mode(next_mode)
            }
            TuiAction::VolumeDown => {
                let next = (self.snapshot.volume - VOLUME_STEP).clamp(0.0, 1.0);
                self.client.set_volume(next)
            }
            TuiAction::VolumeUp => {
                let next = (self.snapshot.volume + VOLUME_STEP).clamp(0.0, 1.0);
                self.client.set_volume(next)
            }
            TuiAction::DismissQueue => {
                if !can_dismiss_queue(&self.snapshot) {
                    anyhow::bail!("No queued tail to dismiss");
                }

                self.client.dismiss_queue_after_current()
            }
        }
    }

    pub(in crate::cli_tui) fn run_action(&mut self, action: TuiAction) -> Result<()> {
        let effective_action = match action {
            TuiAction::Like
                if self
                    .snapshot
                    .current_track
                    .as_ref()
                    .is_some_and(|track| track.rating == SongRating::Like) =>
            {
                TuiAction::Neutral
            }
            TuiAction::Dislike
                if self
                    .snapshot
                    .current_track
                    .as_ref()
                    .is_some_and(|track| track.rating == SongRating::Dislike) =>
            {
                TuiAction::Neutral
            }
            _ => action,
        };

        let result = match self.run_action_request(effective_action) {
            Ok(snapshot) => Ok(snapshot),
            Err(err) if self.client.state().is_err() => {
                self.recover_daemon_from_tui_state()
                    .with_context(|| format!("Action failed: {err}"))?;
                self.run_action_request(effective_action)
                    .with_context(|| format!("Action retry failed after daemon recovery: {err}"))
            }
            Err(err) => Err(err),
        };

        match result {
            Ok(snapshot) => {
                self.apply_snapshot(snapshot);
                if matches!(effective_action, TuiAction::ResumeSuggested) {
                    self.resume_card = None;
                }
                self.status_line = format!("{} applied.", action_label(effective_action));
            }
            Err(err) => self.status_line = format!("Action failed: {err}"),
        }
        Ok(())
    }
}

pub(in crate::cli_tui) fn active_track_snapshot_from_track(
    track: &Track,
    rating: SongRating,
) -> ActiveTrackSnapshot {
    ActiveTrackSnapshot {
        video_id: track.video_id.clone(),
        title: track.title.clone(),
        artist: artist_string(track),
        thumb_url: track.thumbnail_url.clone(),
        rating,
    }
}

pub(in crate::cli_tui) fn current_context_label(snapshot: &PlaybackSnapshot) -> String {
    if let Some(context) = snapshot.playlist_context.as_ref() {
        return context.title.clone();
    }
    if snapshot.radio_virtual_playlist.is_some() {
        return "Radio queue".to_string();
    }
    if snapshot.queue.len() > 1 {
        return format!("Queue · {} tracks", snapshot.queue.len());
    }
    "Single track".to_string()
}

pub(in crate::cli_tui) fn repeat_loops_left_label(snapshot: &PlaybackSnapshot) -> Option<String> {
    snapshot
        .track_repeat_loops_left
        .filter(|_| snapshot.current_track.is_some())
        .map(|loops_left| {
            let loop_label = if loops_left == 1 { "loop" } else { "loops" };
            format!("{loops_left} {loop_label} left")
        })
}

pub(in crate::cli_tui) fn player_bar_meta_spans(snapshot: &PlaybackSnapshot) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        current_context_label(snapshot),
        Style::default().fg(Color::Rgb(130, 160, 255)),
    )];

    if let Some(loop_label) = repeat_loops_left_label(snapshot) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            loop_label,
            Style::default().fg(Color::Rgb(208, 221, 255)),
        ));
    }

    spans
}

pub(in crate::cli_tui) fn lyrics_meta_label(lyrics: &SyncedLyrics) -> String {
    let source = match lyrics.source {
        crate::models::SyncedLyricsSource::TimedLyrics => "TIMED LYRICS",
        crate::models::SyncedLyricsSource::Captions => "CAPTIONS",
        crate::models::SyncedLyricsSource::AutoCaptions => "AUTO CAPTIONS",
    };
    if let Some(language_code) = lyrics.language_code.as_deref() {
        format!("{source} · {}", language_code.to_uppercase())
    } else {
        source.to_string()
    }
}

pub(in crate::cli_tui) fn active_lyric_line_index(
    lyrics: &SyncedLyrics,
    position_millis: u64,
) -> usize {
    lyrics
        .lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| {
            let active_until = line
                .end_millis
                .or_else(|| lyrics.lines.get(index + 1).map(|next| next.start_millis))
                .unwrap_or(u64::MAX);
            (position_millis >= line.start_millis && position_millis < active_until)
                .then_some(index)
        })
        .unwrap_or(0)
}

pub(in crate::cli_tui) fn repeat_button_emoji(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::One => "🔂",
        RepeatMode::Off | RepeatMode::All => "🔁",
    }
}

pub(in crate::cli_tui) fn help_shortcut_sections()
-> &'static [(&'static str, &'static [(&'static str, &'static str)])] {
    const PLAYBACK: &[(&str, &str)] = &[
        ("space", "Play / pause"),
        ("n", "Next track"),
        ("p", "Previous track"),
        ("a / s", "Seek -10s / +10s"),
        ("f", "Like (favourite)"),
        ("d", "Dislike"),
        ("u", "Clear rating"),
        ("l", "Cycle loop / repeat"),
        ("↑ / ↓", "Volume (Player focus only)"),
        ("- / +", "Volume down / up"),
    ];
    const NAVIGATION: &[(&str, &str)] = &[
        ("tab", "Cycle focus"),
        ("arrows", "Move selection"),
        ("enter", "Activate selection"),
        ("m", "Open context menu"),
        ("b / backspace", "Go back"),
        ("home / end", "Jump to start / end"),
        ("pgup / pgdn", "Scroll one page"),
        ("1 / 2 / t", "Queue · Lyrics · toggle"),
    ];
    const APP: &[(&str, &str)] = &[
        ("r", "Refresh current page"),
        ("h", "Toggle this help"),
        ("q / esc", "Quit"),
    ];
    &[
        ("Playback", PLAYBACK),
        ("Navigation", NAVIGATION),
        ("App", APP),
    ]
}

pub(in crate::cli_tui) fn action_label(action: TuiAction) -> &'static str {
    match action {
        TuiAction::ResumeSuggested => "Resume",
        TuiAction::TogglePause => "Toggle",
        TuiAction::CancelHaltAutoResume => "Cancel resume",
        TuiAction::ResumeHaltAutoResume => "Resume now",
        TuiAction::Previous => "Previous",
        TuiAction::Next => "Next",
        TuiAction::Like => "Like",
        TuiAction::Dislike => "Dislike",
        TuiAction::Neutral => "Neutral",
        TuiAction::CycleRepeat => "Repeat",
        TuiAction::VolumeDown => "Volume down",
        TuiAction::VolumeUp => "Volume up",
        TuiAction::DismissQueue => "Dismiss queue",
    }
}

pub(in crate::cli_tui) fn can_dismiss_queue(snapshot: &PlaybackSnapshot) -> bool {
    snapshot.queue_index.saturating_add(1) < snapshot.queue.len()
}
