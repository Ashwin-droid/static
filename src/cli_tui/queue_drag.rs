use super::*;
use super::{content_loading::*, widgets::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn queue_accepts_dnd(&self) -> bool {
        self.detail_tab == DetailTab::Queue
    }

    pub(in crate::cli_tui) fn drag_label(payload: &TuiDragPayload) -> String {
        match payload {
            TuiDragPayload::QueueRow { .. } => "Move in queue".to_string(),
            TuiDragPayload::Track { track } => format!("Queue {}", shorten(&track.title, 28)),
            TuiDragPayload::BrowseCollection { card } => {
                format!("Queue {}", shorten(&card.title, 28))
            }
            TuiDragPayload::PlaylistCollection { card } => {
                format!("Queue {}", shorten(&card.summary.title, 28))
            }
        }
    }

    pub(in crate::cli_tui) fn drag_payload_for_target(
        &self,
        kind: &ClickTargetKind,
    ) -> Option<TuiDragPayload> {
        match kind {
            ClickTargetKind::QueueItem(index) => Some(TuiDragPayload::QueueRow { index: *index }),
            ClickTargetKind::HomeItem(index) => {
                let card = self.home_doc.cards.get(*index)?;
                match &card.item {
                    TuiHomeItem::Track(track) => Some(TuiDragPayload::Track {
                        track: track.clone(),
                    }),
                    TuiHomeItem::Browse(card) => {
                        Some(TuiDragPayload::BrowseCollection { card: card.clone() })
                    }
                    TuiHomeItem::Playlist(card) => {
                        Some(TuiDragPayload::PlaylistCollection { card: card.clone() })
                    }
                }
            }
            ClickTargetKind::RecentItem(index) => self
                .recent_tracks
                .get(*index)
                .cloned()
                .map(|track| TuiDragPayload::Track { track }),
            _ => None,
        }
    }

    pub(in crate::cli_tui) fn set_mouse_selection(&mut self, kind: &ClickTargetKind) {
        match kind {
            ClickTargetKind::HomeItem(index) => {
                self.focus = TuiFocus::Home;
                self.home_selected = *index;
                self.ensure_home_visible();
            }
            ClickTargetKind::SettingsField(index) => {
                self.focus = TuiFocus::Home;
                self.home_selected = *index;
                self.ensure_home_visible();
            }
            ClickTargetKind::QueueItem(index) => {
                self.focus = TuiFocus::Queue;
                self.queue_selected = *index;
                self.ensure_queue_visible();
            }
            ClickTargetKind::RecentItem(index) => {
                self.focus = TuiFocus::Recent;
                self.recent_selected = *index;
                self.ensure_recent_visible();
            }
            _ => {}
        }
    }

    pub(in crate::cli_tui) fn focused_context_menu_target(
        &self,
    ) -> Option<(ClickTargetKind, u16, u16)> {
        let target =
            self.click_targets
                .iter()
                .find(|target| match (&self.focus, &target.kind) {
                    (TuiFocus::Home, ClickTargetKind::HomeItem(index)) => {
                        *index == self.home_selected
                    }
                    (TuiFocus::Queue, ClickTargetKind::QueueItem(index)) => {
                        *index == self.queue_selected
                    }
                    (TuiFocus::Recent, ClickTargetKind::RecentItem(index)) => {
                        *index == self.recent_selected
                    }
                    _ => false,
                })?;

        let anchor_column = target
            .rect
            .x
            .saturating_add(target.rect.width.saturating_sub(1) / 2);
        let anchor_row = target
            .rect
            .y
            .saturating_add(target.rect.height.saturating_sub(1) / 2);
        Some((target.kind.clone(), anchor_column, anchor_row))
    }

    pub(in crate::cli_tui) fn open_context_menu_for_focus(&mut self) {
        let Some((kind, anchor_column, anchor_row)) = self.focused_context_menu_target() else {
            self.status_line = "Focus a song or collection first.".to_string();
            return;
        };

        if !self.open_context_menu(&kind, anchor_column, anchor_row) {
            self.status_line = "That selection has no context menu.".to_string();
        }
    }

    pub(in crate::cli_tui) fn queue_visible_len(&self) -> usize {
        self.snapshot
            .queue
            .len()
            .saturating_sub(self.queue_scroll_offset)
            .min(self.queue_viewport_rows.max(1))
    }

    pub(in crate::cli_tui) fn queue_raw_slot_for_mouse_row(&self, row: u16) -> usize {
        if self.snapshot.queue.is_empty() {
            return 0;
        }

        let viewport = self.rendered_layout.queue_viewport;
        if viewport.height == 0 {
            return self.snapshot.queue.len();
        }

        if row <= viewport.y {
            return self.queue_scroll_offset.min(self.snapshot.queue.len());
        }

        let visible_len = self.queue_visible_len();
        if visible_len == 0 {
            return self.snapshot.queue.len();
        }

        let relative = row.saturating_sub(viewport.y) as usize;
        let row_height = QUEUE_ROW_HEIGHT as usize;
        let row_index = (relative / row_height).min(visible_len.saturating_sub(1));
        let within_row = relative % row_height;
        let absolute_index = self.queue_scroll_offset + row_index;
        let midpoint = QUEUE_ROW_HEIGHT as f32 / 2.0;
        let before_row = (within_row as f32) < midpoint;
        if before_row {
            absolute_index
        } else {
            absolute_index
                .saturating_add(1)
                .min(self.snapshot.queue.len())
        }
    }

    pub(in crate::cli_tui) fn normalized_preview_slot_for_drag(
        &self,
        payload: &TuiDragPayload,
        raw_slot: usize,
    ) -> Option<usize> {
        match payload {
            TuiDragPayload::QueueRow { index: from } => {
                let normalized = if *from < raw_slot {
                    raw_slot.saturating_sub(1)
                } else {
                    raw_slot
                };
                (normalized != *from).then_some(raw_slot)
            }
            _ => Some(raw_slot),
        }
    }

    pub(in crate::cli_tui) fn maybe_autoscroll_queue_drag(&mut self, row: u16) {
        if !self.queue_accepts_dnd() {
            return;
        }
        let viewport = self.rendered_layout.queue_viewport;
        if viewport.height == 0 || self.snapshot.queue.len() <= self.queue_viewport_rows {
            return;
        }

        let edge = QUEUE_ROW_HEIGHT.saturating_mul(2);
        if row <= viewport.y.saturating_add(edge) && self.queue_scroll_offset > 0 {
            self.queue_scroll_offset = self.queue_scroll_offset.saturating_sub(1);
        } else if row.saturating_add(edge) >= viewport.bottom()
            && self
                .queue_scroll_offset
                .saturating_add(self.queue_viewport_rows)
                < self.snapshot.queue.len()
        {
            self.queue_scroll_offset = self.queue_scroll_offset.saturating_add(1);
        }
    }

    pub(in crate::cli_tui) fn update_drag_preview(&mut self, column: u16, row: u16) {
        let Some(payload) = self.drag_state.as_ref().map(|drag| drag.payload.clone()) else {
            return;
        };
        if !self.queue_accepts_dnd() || !contains(self.rendered_layout.queue_viewport, column, row)
        {
            if let Some(drag) = self.drag_state.as_mut() {
                drag.preview_slot = None;
            }
            return;
        }

        self.focus = TuiFocus::Queue;
        self.maybe_autoscroll_queue_drag(row);
        let raw_slot = self.queue_raw_slot_for_mouse_row(row);
        let preview_slot = self.normalized_preview_slot_for_drag(&payload, raw_slot);
        if let Some(drag) = self.drag_state.as_mut() {
            drag.preview_slot = preview_slot;
        }
    }

    pub(in crate::cli_tui) fn clear_drag_state(&mut self) {
        self.pending_mouse_press = None;
        self.drag_state = None;
    }

    pub(in crate::cli_tui) fn current_preview_slot(&self) -> Option<usize> {
        self.drag_state.as_ref().and_then(|drag| drag.preview_slot)
    }

    pub(in crate::cli_tui) fn apply_queue_row_reorder(
        &mut self,
        from: usize,
        raw_slot: usize,
    ) -> Result<()> {
        let insert_at = if from < raw_slot {
            raw_slot.saturating_sub(1)
        } else {
            raw_slot
        };
        if insert_at == from
            || from >= self.snapshot.queue.len()
            || raw_slot > self.snapshot.queue.len()
        {
            return Ok(());
        }

        let mut queue = self.snapshot.queue.clone();
        let moved_track = queue.remove(from);
        queue.insert(insert_at, moved_track.clone());

        let current_index = adjusted_reorder_index(self.snapshot.queue_index, from, insert_at);
        let selected_index = adjusted_reorder_index(self.queue_selected, from, insert_at);
        let mut radio_virtual_playlist = self.snapshot.radio_virtual_playlist.clone();
        update_radio_touched_for_reorder(&mut radio_virtual_playlist, from, insert_at);

        let snapshot = self.client.replace_queue(
            queue,
            current_index,
            self.snapshot.playlist_context.clone(),
            radio_virtual_playlist,
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.focus = TuiFocus::Queue;
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = selected_index.min(self.snapshot.queue.len().saturating_sub(1));
        self.ensure_queue_visible();
        self.status_line = format!(
            "Moved {} to slot {}.",
            shorten(&moved_track.title, 28),
            insert_at.saturating_add(1)
        );
        Ok(())
    }

    pub(in crate::cli_tui) fn insert_tracks_into_queue(
        &mut self,
        tracks: Vec<Track>,
        raw_slot: usize,
    ) -> Result<()> {
        if tracks.is_empty() {
            self.status_line = "Nothing to insert into the queue.".to_string();
            return Ok(());
        }

        let had_queue = !self.snapshot.queue.is_empty();
        let insert_at = raw_slot.min(self.snapshot.queue.len());
        let added = tracks.len();
        let first_title = tracks
            .first()
            .map(|track| shorten(&track.title, 28))
            .unwrap_or_else(|| "tracks".to_string());
        let mut queue = self.snapshot.queue.clone();
        for (offset, track) in tracks.into_iter().enumerate() {
            queue.insert(insert_at + offset, track);
        }

        let current_index = if had_queue && insert_at <= self.snapshot.queue_index {
            self.snapshot.queue_index.saturating_add(added)
        } else {
            self.snapshot.queue_index
        };
        let mut radio_virtual_playlist = self.snapshot.radio_virtual_playlist.clone();
        extend_radio_touched_for_insert(&mut radio_virtual_playlist, insert_at, added);

        let snapshot = self.client.replace_queue(
            queue,
            current_index,
            self.snapshot.playlist_context.clone(),
            radio_virtual_playlist,
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.focus = TuiFocus::Queue;
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = insert_at.min(self.snapshot.queue.len().saturating_sub(1));
        self.ensure_queue_visible();
        self.status_line = if added == 1 {
            format!("Queued {}.", first_title)
        } else {
            format!("Queued {} tracks.", added)
        };
        Ok(())
    }

    pub(in crate::cli_tui) fn handle_drag_drop(
        &mut self,
        payload: TuiDragPayload,
        raw_slot: usize,
    ) -> Result<()> {
        match payload {
            TuiDragPayload::QueueRow { index } => self.apply_queue_row_reorder(index, raw_slot),
            TuiDragPayload::Track { track } => self.insert_tracks_into_queue(vec![track], raw_slot),
            TuiDragPayload::BrowseCollection { card } => {
                let network =
                    saved_network_state().context("Connect YouTube Music in the GTK app first")?;
                let tracks = network
                    .get_browse_tracks(&card.browse_id)
                    .with_context(|| format!("Failed to open {}", card.title))?;
                self.insert_tracks_into_queue(tracks, raw_slot)
            }
            TuiDragPayload::PlaylistCollection { card } => {
                let tracks = load_playlist_card_tracks(&card)
                    .with_context(|| format!("Failed to open {}", card.summary.title))?;
                self.insert_tracks_into_queue(tracks, raw_slot)
            }
        }
    }
}

pub(in crate::cli_tui) fn adjusted_reorder_index(
    current: usize,
    from: usize,
    insert_at: usize,
) -> usize {
    if from == current {
        insert_at
    } else if from < current && insert_at >= current {
        current.saturating_sub(1)
    } else if from > current && insert_at <= current {
        current.saturating_add(1)
    } else {
        current
    }
}

pub(in crate::cli_tui) fn extend_radio_touched_for_insert(
    radio_virtual_playlist: &mut Option<RadioVirtualPlaylist>,
    insert_at: usize,
    added: usize,
) {
    if added == 0 {
        return;
    }

    if let Some(radio) = radio_virtual_playlist.as_mut() {
        let touched_len = radio.touched_len;
        let inserted_end = insert_at + added;
        if insert_at < touched_len {
            radio.touched_len = touched_len + added;
        } else {
            radio.touched_len = touched_len.max(inserted_end);
        }
    }
}

pub(in crate::cli_tui) fn radio_touched_len_after_reorder(
    touched_len: usize,
    from: usize,
    insert_at: usize,
) -> usize {
    let mut touched_len = touched_len;

    if from < touched_len {
        touched_len = touched_len.saturating_sub(1);
    }

    if insert_at < touched_len {
        touched_len + 1
    } else {
        touched_len.max(insert_at.saturating_add(1))
    }
}

pub(in crate::cli_tui) fn update_radio_touched_for_reorder(
    radio_virtual_playlist: &mut Option<RadioVirtualPlaylist>,
    from: usize,
    insert_at: usize,
) {
    if let Some(radio) = radio_virtual_playlist.as_mut() {
        radio.touched_len = radio_touched_len_after_reorder(radio.touched_len, from, insert_at);
    }
}

pub(in crate::cli_tui) fn shrink_radio_touched_for_remove(
    radio_virtual_playlist: &mut Option<RadioVirtualPlaylist>,
    remove_at: usize,
) {
    if let Some(radio) = radio_virtual_playlist.as_mut() {
        if radio.touched_len == 0 {
            return;
        }

        if remove_at < radio.touched_len {
            radio.touched_len = radio.touched_len.saturating_sub(1);
        }
    }
}
