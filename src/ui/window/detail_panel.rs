use super::*;

pub(super) fn detail_tab_button(label: &str, active: bool) -> Button {
    let button = Button::with_label(label);
    button.add_css_class("detail-tab-btn");
    if active {
        button.add_css_class("detail-tab-btn-active");
    }
    button
}

pub(super) fn set_detail_panel_view(ps: &PlayerState, view: DetailPanelView) {
    ps.detail_panel_mode.set(view);
    ps.detail_panel_stack
        .set_visible_child_name(view.stack_name());
    ps.queue_dismiss_btn
        .set_visible(view == DetailPanelView::Queue);

    if view == DetailPanelView::Queue {
        ps.detail_queue_tab_btn
            .add_css_class("detail-tab-btn-active");
        ps.detail_lyrics_tab_btn
            .remove_css_class("detail-tab-btn-active");
    } else {
        ps.detail_lyrics_tab_btn
            .add_css_class("detail-tab-btn-active");
        ps.detail_queue_tab_btn
            .remove_css_class("detail-tab-btn-active");
        clear_queue_drop_preview(ps);
        set_queue_drop_active(ps, false);
    }
}

pub(super) fn synced_lyrics_meta_text(lyrics: &SyncedLyrics) -> String {
    let source = match lyrics.source {
        SyncedLyricsSource::Captions => "Synced captions".to_string(),
        SyncedLyricsSource::AutoCaptions => "Auto-synced captions".to_string(),
    };
    let track_name = lyrics
        .track_name
        .as_deref()
        .filter(|name| !name.trim().is_empty());
    let language = lyrics
        .language_code
        .as_deref()
        .filter(|code| !code.trim().is_empty());

    [Some(source.as_str()), track_name, language]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("  •  ")
}

pub(super) fn show_lyrics_status(ps: &PlayerState, meta: Option<&str>, message: &str) {
    clear_box(&ps.lyrics_list_box);
    ps.lyrics_line_widgets.borrow_mut().clear();
    ps.lyrics_lines.borrow_mut().clear();
    ps.lyrics_active_line.set(None);

    if let Some(meta) = meta.filter(|meta| !meta.trim().is_empty()) {
        ps.lyrics_meta_label.set_text(meta);
        ps.lyrics_meta_label.set_visible(true);
    } else {
        ps.lyrics_meta_label.set_text("");
        ps.lyrics_meta_label.set_visible(false);
    }

    ps.lyrics_status_label.set_text(message);
    ps.lyrics_status_label.set_visible(true);
    ps.lyrics_scroll.set_visible(false);
}

pub(super) fn show_lyrics_loading(ps: &PlayerState) {
    show_lyrics_status(
        ps,
        Some("Syncing lyrics"),
        "Loading timed lines for this track...",
    );
}

pub(super) fn show_lyrics_idle(ps: &PlayerState) {
    show_lyrics_status(
        ps,
        None,
        "Play something to see synced lyrics when YouTube exposes timed text.",
    );
}

pub(super) fn scroll_active_lyric_into_view(ps: &PlayerState, line: &Label) {
    if ps.detail_panel_mode.get() != DetailPanelView::Lyrics {
        return;
    }

    let adjustment = ps.lyrics_scroll.vadjustment();
    let allocation = line.allocation();
    let top = allocation.y() as f64;
    let bottom = top + allocation.height() as f64;
    let padding = 72.0;
    let current_top = adjustment.value();
    let current_bottom = current_top + adjustment.page_size();
    let upper = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());

    let target = if top < current_top + padding {
        top - padding
    } else if bottom > current_bottom - padding {
        bottom - adjustment.page_size() + padding
    } else {
        return;
    };

    adjustment.set_value(target.clamp(adjustment.lower(), upper));
}

pub(super) fn update_active_lyric_line(ps: &PlayerState, position_millis: Option<u64>) {
    let next_index = position_millis.and_then(|position| {
        let lines = ps.lyrics_lines.borrow();
        lines
            .iter()
            .enumerate()
            .find(|(_, line)| {
                let end = line.end_millis.unwrap_or(u64::MAX);
                position >= line.start_millis && position < end
            })
            .map(|(index, _)| index)
            .or_else(|| {
                lines
                    .iter()
                    .enumerate()
                    .rfind(|(_, line)| position >= line.start_millis)
                    .map(|(index, _)| index)
            })
    });

    let previous_index = ps.lyrics_active_line.replace(next_index);
    if previous_index == next_index {
        return;
    }

    if let Some(previous_index) = previous_index {
        if let Some(previous_line) = ps.lyrics_line_widgets.borrow().get(previous_index).cloned() {
            previous_line.remove_css_class("lyrics-line-active");
        }
    }

    if let Some(next_index) = next_index {
        if let Some(next_line) = ps.lyrics_line_widgets.borrow().get(next_index).cloned() {
            next_line.add_css_class("lyrics-line-active");
            scroll_active_lyric_into_view(ps, &next_line);
        }
    }
}

pub(super) fn render_synced_lyrics(
    ps: &PlayerState,
    lyrics: &SyncedLyrics,
    position_millis: Option<u64>,
) {
    clear_box(&ps.lyrics_list_box);

    let mut line_widgets = Vec::with_capacity(lyrics.lines.len());
    for line in &lyrics.lines {
        let label = Label::new(Some(&line.text));
        label.add_css_class("lyrics-line");
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_halign(Align::Start);
        label.set_xalign(0.0);
        label.set_selectable(false);
        ps.lyrics_list_box.append(&label);
        line_widgets.push(label);
    }

    *ps.lyrics_lines.borrow_mut() = lyrics.lines.clone();
    *ps.lyrics_line_widgets.borrow_mut() = line_widgets;
    ps.lyrics_active_line.set(None);

    let meta = synced_lyrics_meta_text(lyrics);
    if meta.is_empty() {
        ps.lyrics_meta_label.set_text("");
        ps.lyrics_meta_label.set_visible(false);
    } else {
        ps.lyrics_meta_label.set_text(&meta);
        ps.lyrics_meta_label.set_visible(true);
    }

    ps.lyrics_status_label.set_visible(false);
    ps.lyrics_scroll.set_visible(true);
    update_active_lyric_line(ps, position_millis);
}

pub(super) fn lyrics_request_is_current(
    ps: &PlayerState,
    session_id: u64,
    request_serial: u64,
    video_id: &str,
) -> bool {
    if ps.lyrics_request_serial.get() != request_serial {
        return false;
    }

    ps.now_playing_track
        .borrow()
        .as_ref()
        .is_some_and(|track| track.session_id == session_id && track.video_id == video_id)
}
