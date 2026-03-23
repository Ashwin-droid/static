use super::*;

pub(super) fn queue_preview_data_for_payload(
    ps: &PlayerState,
    payload: &QueueDragPayload,
) -> Option<QueuePreviewData> {
    match payload {
        QueueDragPayload::QueueRow { index } => {
            let queue = ps.queue.borrow();
            let track = queue.get(*index)?;
            Some(QueuePreviewData {
                title: track.title.clone(),
                subtitle: artist_string(track),
                thumbnail_url: track.thumbnail_url.clone(),
                duration: track.duration.clone(),
            })
        }
        QueueDragPayload::Track { track } => Some(QueuePreviewData {
            title: track.title.clone(),
            subtitle: artist_string(track),
            thumbnail_url: track.thumbnail_url.clone(),
            duration: track.duration.clone(),
        }),
        QueueDragPayload::Playlist {
            title,
            subtitle,
            thumbnail_url,
            ..
        } => Some(QueuePreviewData {
            title: title.clone(),
            subtitle: subtitle.clone().unwrap_or_else(|| "Playlist".to_string()),
            thumbnail_url: thumbnail_url.clone(),
            duration: None,
        }),
        QueueDragPayload::BrowseCollection {
            title,
            subtitle,
            thumbnail_url,
            page_type,
            ..
        } => Some(QueuePreviewData {
            title: title.clone(),
            subtitle: subtitle.clone().unwrap_or_else(|| match page_type {
                HomeBrowsePageType::Album => "Album".to_string(),
                HomeBrowsePageType::Playlist => "Playlist".to_string(),
                HomeBrowsePageType::Artist => "Artist".to_string(),
                HomeBrowsePageType::UserChannel => "Channel".to_string(),
            }),
            thumbnail_url: thumbnail_url.clone(),
            duration: None,
        }),
    }
}

pub(super) fn build_queue_row_visual(
    preview: &QueuePreviewData,
    active: bool,
    with_outer_margins: bool,
    forced_width: Option<i32>,
) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    row.add_css_class("queue-row");
    row.set_hexpand(true);
    row.set_halign(Align::Fill);
    row.set_overflow(gtk::Overflow::Hidden);
    row.set_size_request(-1, 48);
    if with_outer_margins {
        row.set_margin_start(8);
        row.set_margin_end(8);
        row.set_margin_top(2);
        row.set_margin_bottom(2);
    }
    if let Some(width) = forced_width {
        row.set_width_request(width);
    }

    if active {
        row.add_css_class("queue-row-active");
    }

    let art = GtkBox::new(Orientation::Vertical, 0);
    art.add_css_class("queue-thumb");
    art.set_size_request(40, 40);
    art.set_valign(Align::Center);
    art.set_overflow(gtk::Overflow::Hidden);

    if let Some(url) = preview.thumbnail_url.as_deref() {
        let pic = Picture::new();
        pic.set_keep_aspect_ratio(true);
        pic.set_size_request(40, 40);
        art.append(&pic);
        load_thumbnail_into(&pic, url, 80);
    }

    row.append(&art);

    let info = GtkBox::new(Orientation::Vertical, 0);
    info.set_hexpand(true);
    info.set_halign(Align::Fill);
    info.set_valign(Align::Center);
    info.set_overflow(gtk::Overflow::Hidden);

    if let Some(width) = forced_width {
        let margins = if with_outer_margins { 16 } else { 0 };
        let duration = if preview.duration.is_some() { 44 } else { 0 };
        let reserved_width = (width - (40 + 8 + duration + margins)).max(72);

        let title_lbl = Label::new(Some(&preview.title));
        title_lbl.set_halign(Align::Start);
        title_lbl.add_css_class("queue-track-title");
        let title_sw = label_scroller(&title_lbl, 18);
        title_sw.set_hexpand(true);
        title_sw.set_max_content_width(reserved_width);
        info.append(&title_sw);

        let artist_lbl = Label::new(Some(&preview.subtitle));
        artist_lbl.set_halign(Align::Start);
        artist_lbl.add_css_class("queue-track-artist");
        let artist_sw = label_scroller(&artist_lbl, 16);
        artist_sw.set_hexpand(true);
        artist_sw.set_max_content_width(reserved_width);
        info.append(&artist_sw);
    } else {
        let title_lbl = Label::new(Some(&preview.title));
        title_lbl.set_halign(Align::Start);
        title_lbl.set_hexpand(true);
        title_lbl.set_wrap(false);
        title_lbl.set_single_line_mode(true);
        title_lbl.set_width_chars(1);
        title_lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title_lbl.add_css_class("queue-track-title");
        info.append(&title_lbl);

        let artist_lbl = Label::new(Some(&preview.subtitle));
        artist_lbl.set_halign(Align::Start);
        artist_lbl.set_hexpand(true);
        artist_lbl.set_wrap(false);
        artist_lbl.set_single_line_mode(true);
        artist_lbl.set_width_chars(1);
        artist_lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
        artist_lbl.add_css_class("queue-track-artist");
        info.append(&artist_lbl);
    }

    row.append(&info);

    if let Some(dur) = preview.duration.as_deref() {
        let dur_lbl = Label::new(Some(dur));
        dur_lbl.add_css_class("queue-track-duration");
        dur_lbl.set_valign(Align::Center);
        row.append(&dur_lbl);
    }

    row
}

pub(super) fn build_queue_virtual_divider() -> GtkBox {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.add_css_class("queue-virtual-divider");

    let label = Label::new(Some("Generated from this radio"));
    label.add_css_class("queue-virtual-divider-label");
    label.set_halign(Align::Start);
    root.append(&label);

    root
}

pub(super) fn is_queue_row_widget(widget: &gtk::Widget) -> bool {
    widget.has_css_class("queue-row-btn")
}

pub(super) fn queue_drag_autoscroll_velocity(viewport_y: f64, viewport_height: f64) -> f64 {
    const EDGE_ZONE: f64 = 72.0;
    const MIN_STEP: f64 = 8.0;
    const MAX_STEP: f64 = 28.0;

    if viewport_height <= 0.0 {
        return 0.0;
    }

    let edge = EDGE_ZONE.min((viewport_height / 2.0).max(24.0));
    let clamped_y = viewport_y.clamp(0.0, viewport_height);

    if clamped_y < edge {
        let intensity = (1.0 - (clamped_y / edge)).clamp(0.0, 1.0);
        -(MIN_STEP + ((MAX_STEP - MIN_STEP) * intensity))
    } else if clamped_y > viewport_height - edge {
        let distance = (viewport_height - clamped_y).clamp(0.0, edge);
        let intensity = (1.0 - (distance / edge)).clamp(0.0, 1.0);
        MIN_STEP + ((MAX_STEP - MIN_STEP) * intensity)
    } else {
        0.0
    }
}

pub(super) fn stop_queue_autoscroll(ps: &PlayerState) {
    ps.queue_drag_hover_viewport_y.set(None);
    ps.queue_autoscroll_velocity.set(0.0);
    if let Some(source_id) = ps.queue_autoscroll_source.borrow_mut().take() {
        source_id.remove();
    }
}

pub(super) fn ensure_queue_autoscroll(ps: &Rc<PlayerState>) {
    if ps.queue_autoscroll_source.borrow().is_some() {
        return;
    }

    let ps_for_timer = ps.clone();
    let source_id = glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
        let Some(viewport_y) = ps_for_timer.queue_drag_hover_viewport_y.get() else {
            ps_for_timer.queue_autoscroll_source.borrow_mut().take();
            return glib::ControlFlow::Break;
        };

        let velocity = ps_for_timer.queue_autoscroll_velocity.get();
        if velocity.abs() <= f64::EPSILON
            || ps_for_timer.queue_drag_preview_payload.borrow().is_none()
        {
            ps_for_timer.queue_autoscroll_source.borrow_mut().take();
            return glib::ControlFlow::Break;
        }

        let adjustment = ps_for_timer.queue_scroll.vadjustment();
        let lower = adjustment.lower();
        let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
        if upper <= lower {
            ps_for_timer.queue_autoscroll_source.borrow_mut().take();
            return glib::ControlFlow::Break;
        }

        let current = adjustment.value();
        let target = (current + velocity).clamp(lower, upper);
        if (target - current).abs() <= f64::EPSILON {
            return glib::ControlFlow::Continue;
        }

        adjustment.set_value(target);

        let content_y = viewport_y + target;
        let raw_slot = preview_slot_for_queue_y(&ps_for_timer, content_y);
        let preview_slot = normalized_preview_slot(&ps_for_timer, raw_slot);
        set_queue_drop_preview(&ps_for_timer, preview_slot);

        glib::ControlFlow::Continue
    });

    ps.queue_autoscroll_source.replace(Some(source_id));
}

pub(super) fn update_queue_drag_hover(ps: &Rc<PlayerState>, viewport_y: Option<f64>) {
    ps.queue_drag_hover_viewport_y.set(viewport_y);

    let velocity = viewport_y
        .map(|y| queue_drag_autoscroll_velocity(y, ps.queue_scroll.allocated_height() as f64))
        .unwrap_or(0.0);
    ps.queue_autoscroll_velocity.set(velocity);

    if velocity.abs() <= f64::EPSILON {
        if let Some(source_id) = ps.queue_autoscroll_source.borrow_mut().take() {
            source_id.remove();
        }
        return;
    }

    ensure_queue_autoscroll(ps);
}

pub(super) fn refresh_queue_drop_indicator(ps: &PlayerState) {
    let preview = ps
        .queue_drag_preview_payload
        .borrow()
        .as_ref()
        .and_then(|payload| queue_preview_data_for_payload(ps, payload));

    if let Some(preview) = preview {
        ps.queue_drop_indicator_title.set_text(&preview.title);
        ps.queue_drop_indicator_subtitle.set_text(&preview.subtitle);
        ps.queue_drop_indicator_duration
            .set_text(preview.duration.as_deref().unwrap_or(""));
        if let Some(url) = preview.thumbnail_url.as_deref() {
            ps.queue_drop_indicator_thumb.set_visible(true);
            load_thumbnail_into(&ps.queue_drop_indicator_thumb, url, 80);
        } else {
            ps.queue_drop_indicator_thumb
                .set_paintable(Option::<&gtk::gdk::Paintable>::None);
            ps.queue_drop_indicator_thumb.set_visible(false);
        }
    } else {
        ps.queue_drop_indicator_title.set_text("Drop into queue");
        ps.queue_drop_indicator_subtitle.set_text("");
        ps.queue_drop_indicator_duration.set_text("");
        ps.queue_drop_indicator_thumb
            .set_paintable(Option::<&gtk::gdk::Paintable>::None);
        ps.queue_drop_indicator_thumb.set_visible(false);
    }
}

pub(super) fn clear_queue_drop_preview(ps: &PlayerState) {
    if ps.queue_drop_preview_slot.take().is_none() {
        return;
    }

    ps.queue_drop_indicator.set_visible(false);

    let mut child = ps.queue_list_box.first_child();
    while let Some(widget) = child {
        widget.remove_css_class("queue-drop-gap-before");
        widget.remove_css_class("queue-drop-gap-after");
        child = widget.next_sibling();
    }
}

pub(super) fn set_queue_drop_active(ps: &PlayerState, active: bool) {
    if active {
        ps.queue_panel.add_css_class("queue-drop-active");
    } else {
        ps.queue_panel.remove_css_class("queue-drop-active");
        ps.queue_list_box.remove_css_class("queue-drop-empty");
    }
}

pub(super) fn set_queue_drop_preview(ps: &PlayerState, slot: Option<usize>) {
    if ps.queue_drop_preview_slot.get() == slot {
        return;
    }

    clear_queue_drop_preview(ps);

    let Some(slot) = slot else { return };
    let queue_len = ps.queue.borrow().len();
    if queue_len == 0 {
        ps.queue_list_box.add_css_class("queue-drop-empty");
        refresh_queue_drop_indicator(ps);
        ps.queue_drop_indicator
            .set_margin_top(ps.queue_list_box.margin_top());
        ps.queue_drop_indicator.set_visible(true);
        ps.queue_drop_preview_slot.set(Some(0));
        return;
    }
    ps.queue_list_box.remove_css_class("queue-drop-empty");

    let slot = slot.min(queue_len);
    let mut row_widgets: Vec<gtk::Widget> = Vec::new();
    let mut collect = ps.queue_list_box.first_child();
    while let Some(widget) = collect {
        if is_queue_row_widget(&widget) {
            row_widgets.push(widget.clone());
        }
        collect = widget.next_sibling();
    }

    if slot >= queue_len {
        if let Some(last_row) = row_widgets.last() {
            last_row.add_css_class("queue-drop-gap-after");
        }
    } else if let Some(target_row) = row_widgets.get(slot) {
        target_row.add_css_class("queue-drop-gap-before");
    }

    let mut offset = ps.queue_list_box.margin_top() as i32;
    let mut row_index = 0usize;
    let mut child = ps.queue_list_box.first_child();
    while let Some(widget) = child {
        if is_queue_row_widget(&widget) && row_index >= slot {
            break;
        }
        offset += widget.allocated_height().max(0);
        if is_queue_row_widget(&widget) {
            row_index += 1;
        }
        child = widget.next_sibling();
    }

    refresh_queue_drop_indicator(ps);
    ps.queue_drop_indicator.set_margin_top(offset);
    ps.queue_drop_indicator.set_visible(true);
    ps.queue_drop_preview_slot.set(Some(slot));
}

pub(super) fn preview_slot_for_pointer(row_index: usize, y: f64, row_height: i32) -> usize {
    if y < (row_height.max(1) as f64 / 2.0) {
        row_index
    } else {
        row_index + 1
    }
}

pub(super) fn preview_slot_for_queue_y(ps: &PlayerState, y: f64) -> usize {
    let mut child = ps.queue_list_box.first_child();
    let mut row_index = 0usize;
    let mut cursor_y = 0.0f64;

    while let Some(widget) = child {
        let height = widget.allocated_height().max(1) as f64;
        if is_queue_row_widget(&widget) {
            let midpoint = cursor_y + (height / 2.0);
            if y < midpoint {
                return row_index;
            }
            row_index += 1;
        } else if y < cursor_y + height {
            return row_index;
        }
        cursor_y += height;
        child = widget.next_sibling();
    }

    row_index
}

pub(super) fn normalized_preview_slot(ps: &PlayerState, raw_slot: usize) -> Option<usize> {
    ps.queue_drag_source_index
        .get()
        .and_then(|from| {
            let normalized = if from < raw_slot {
                raw_slot.saturating_sub(1)
            } else {
                raw_slot
            };
            (normalized != from).then_some(raw_slot)
        })
        .or(Some(raw_slot))
}

pub(super) fn queue_drag_provider(payload: &QueueDragPayload) -> Option<gtk::gdk::ContentProvider> {
    let json = serde_json::to_string(payload).ok()?;
    Some(gtk::gdk::ContentProvider::for_value(&json.to_value()))
}

pub(super) fn drag_preview_size(ps: &PlayerState) -> (i32, i32) {
    if let Some(first_row) = ps.queue_list_box.first_child() {
        let row_width = first_row.allocated_width();
        let row_height = first_row.allocated_height();
        if row_width > 0 {
            return (row_width, row_height.max(52));
        }
    }

    let list_width = ps.queue_list_box.allocated_width();
    if list_width > 0 {
        return (list_width, 52);
    }

    let queue_width = ps.queue_panel.allocated_width();
    if queue_width > 0 {
        ((queue_width - 24).clamp(220, 420), 52)
    } else {
        (360, 52)
    }
}

pub(super) fn build_drag_preview_widget(
    preview: &QueuePreviewData,
    width: i32,
    height: i32,
) -> GtkBox {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.add_css_class("queue-row-btn");
    root.add_css_class("drag-preview-shell");
    root.set_halign(Align::Start);
    root.set_valign(Align::Start);
    root.set_overflow(gtk::Overflow::Hidden);
    root.set_size_request(width, height);
    root.append(&build_queue_row_visual(preview, false, false, Some(width)));
    root
}

pub(super) fn set_drag_icon_from_preview(
    drag: &gtk::gdk::Drag,
    preview: &QueuePreviewData,
    width: i32,
    height: i32,
) {
    let icon = gtk::DragIcon::for_drag(drag);
    let widget = build_drag_preview_widget(preview, width, height);
    icon.set_child(Some(&widget));
}

pub(super) fn parse_queue_drag_payload(value: &glib::Value) -> Option<QueueDragPayload> {
    let json = value.get::<String>().ok()?;
    serde_json::from_str(&json).ok()
}

pub(super) fn queue_accepts_external_dnd(ps: &PlayerState) -> bool {
    ps.queue_panel.is_visible() && ps.detail_panel_mode.get() == DetailPanelView::Queue
}

pub(super) fn attach_queue_drag_source(
    widget: &impl IsA<gtk::Widget>,
    payload: QueueDragPayload,
    ps: &Rc<PlayerState>,
) {
    let drag_source = DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::MOVE);

    {
        let payload = payload.clone();
        let ps = ps.clone();
        drag_source.connect_prepare(move |_, _, _| {
            if !queue_accepts_external_dnd(&ps) {
                return None;
            }
            queue_drag_provider(&payload)
        });
    }

    {
        let payload = payload.clone();
        let ps = ps.clone();
        drag_source.connect_drag_begin(move |_, drag| {
            ps.queue_drag_preview_payload.replace(Some(payload.clone()));
            if let Some(preview) = queue_preview_data_for_payload(&ps, &payload) {
                let (width, height) = drag_preview_size(&ps);
                set_drag_icon_from_preview(drag, &preview, width, height);
            }
        });
    }

    {
        let ps = ps.clone();
        drag_source.connect_drag_end(move |_, _, _| {
            ps.queue_drag_preview_payload.replace(None);
            stop_queue_autoscroll(&ps);
            clear_queue_drop_preview(&ps);
            set_queue_drop_active(&ps, false);
        });
    }

    widget.as_ref().add_controller(drag_source);
}

pub(super) fn insert_tracks_into_queue(
    tracks: Vec<Track>,
    insert_at: usize,
    mark_as_touched: bool,
    ps: &Rc<PlayerState>,
    state: &Arc<Mutex<AppState>>,
) {
    if tracks.is_empty() {
        return;
    }

    let added = tracks.len();
    {
        let mut queue = ps.queue.borrow_mut();
        let had_queue = !queue.is_empty();
        let insert_at = insert_at.min(queue.len());
        for (offset, track) in tracks.into_iter().enumerate() {
            queue.insert(insert_at + offset, track);
        }
        if had_queue && insert_at <= ps.queue_index.get() {
            ps.queue_index.set(ps.queue_index.get() + added);
        }
    }
    if mark_as_touched {
        extend_radio_touched_for_insert(ps, insert_at, added);
    }
    *ps.last_radio_append_anchor.borrow_mut() = None;

    render_queue_panel(ps, state);
    sync_remote_queue(ps);
    sync_mpris_transport(ps, state);
}

pub(super) fn resolve_dragged_collection_tracks(
    payload: &QueueDragPayload,
    state: &Arc<Mutex<AppState>>,
) -> Result<Vec<Track>, String> {
    match payload {
        QueueDragPayload::Playlist {
            playlist_id,
            source,
            ..
        } => match source {
            QueuePlaylistSource::Offline => Ok(load_offline_playlist_tracks(state, playlist_id)),
            QueuePlaylistSource::Remote => {
                let Some(client) = app_client(state) else {
                    return Err("Not connected.".to_string());
                };
                client.get_playlist(playlist_id).map_err(|e| e.to_string())
            }
        },
        QueueDragPayload::BrowseCollection { browse_id, .. } => {
            let Some(client) = app_client(state) else {
                return Err("Not connected.".to_string());
            };
            client
                .get_browse_tracks(browse_id)
                .map_err(|e| e.to_string())
        }
        _ => Ok(Vec::new()),
    }
}

pub(super) fn handle_queue_drop_payload(
    payload: QueueDragPayload,
    raw_slot: usize,
    ps: &Rc<PlayerState>,
    state: &Arc<Mutex<AppState>>,
) -> bool {
    match payload {
        QueueDragPayload::QueueRow { index: from } => {
            let insert_at = if from < raw_slot {
                raw_slot.saturating_sub(1)
            } else {
                raw_slot
            };
            if insert_at == from {
                return false;
            }
            {
                let mut q = ps.queue.borrow_mut();
                if from >= q.len() || raw_slot > q.len() {
                    return false;
                }
                let track = q.remove(from);
                q.insert(insert_at, track);

                let current = ps.queue_index.get();
                if from == current {
                    ps.queue_index.set(insert_at);
                } else if from < current && insert_at >= current {
                    ps.queue_index.set(current - 1);
                } else if from > current && insert_at <= current {
                    ps.queue_index.set(current + 1);
                }
            }
            update_radio_touched_for_reorder(ps, from, insert_at);
            *ps.last_radio_append_anchor.borrow_mut() = None;

            render_queue_panel(ps, state);
            sync_remote_queue(ps);
            sync_mpris_transport(ps, state);
            true
        }
        QueueDragPayload::Track { track } => {
            insert_tracks_into_queue(vec![track], raw_slot, true, ps, state);
            true
        }
        payload @ QueueDragPayload::Playlist { .. }
        | payload @ QueueDragPayload::BrowseCollection { .. } => {
            let ps = ps.clone();
            let state = state.clone();
            glib::spawn_future_local(async move {
                let payload_bg = payload.clone();
                let state_bg = state.clone();
                let result = gio::spawn_blocking(move || {
                    resolve_dragged_collection_tracks(&payload_bg, &state_bg)
                })
                .await;

                match result {
                    Ok(Ok(tracks)) => insert_tracks_into_queue(tracks, raw_slot, true, &ps, &state),
                    Ok(Err(err)) => eprintln!("[QUEUE] failed to expand collection drop: {err}"),
                    Err(err) => eprintln!("[QUEUE] failed to load collection drop: {err:?}"),
                }
            });
            true
        }
    }
}
