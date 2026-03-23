use super::*;

pub(super) fn label_scroller(label: &Label, height: i32) -> ScrolledWindow {
    // Prevent the label from wrapping — it must stay single-line for marquee to work
    // and to avoid GTK height-for-width mismatches.
    label.set_wrap(false);
    label.set_single_line_mode(true);

    let sw = ScrolledWindow::new();
    sw.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    sw.set_hexpand(true);
    sw.set_size_request(1, height);
    sw.set_min_content_height(height);
    sw.set_vexpand(false);
    sw.set_propagate_natural_width(false);
    sw.set_propagate_natural_height(false);
    sw.set_min_content_width(0);
    sw.set_can_target(false);
    label.set_can_target(false);
    sw.set_child(Some(label));

    // Start marquee when text changes
    let sw_c = sw.clone();
    label.connect_label_notify(move |_| {
        // Reset scroll position
        sw_c.hadjustment().set_value(0.0);
        // After 1s, start scrolling if needed
        start_marquee(&sw_c);
    });

    sw
}

pub(super) fn start_marquee(sw: &ScrolledWindow) {
    let sw_c = sw.clone();
    glib::timeout_add_local_once(std::time::Duration::from_secs(1), move || {
        let adj = sw_c.hadjustment();
        if adj.upper() - adj.page_size() <= 0.0 {
            return;
        }
        let state = Rc::new(Cell::new(0u8));
        let pause = Rc::new(Cell::new(0i32));
        let sw_ref = sw_c;
        glib::timeout_add_local(std::time::Duration::from_millis(30), move || {
            if !sw_ref.is_mapped() {
                return glib::ControlFlow::Break;
            }
            let adj = sw_ref.hadjustment();
            let max = adj.upper() - adj.page_size();
            if max <= 0.0 {
                return glib::ControlFlow::Break;
            }
            match state.get() {
                0 => {
                    let v = adj.value() + 0.6;
                    if v >= max {
                        adj.set_value(max);
                        state.set(1);
                        pause.set(50);
                    } else {
                        adj.set_value(v);
                    }
                }
                1 => {
                    let t = pause.get() - 1;
                    pause.set(t);
                    if t <= 0 {
                        adj.set_value(0.0);
                        state.set(2);
                        pause.set(33);
                    }
                }
                _ => {
                    let t = pause.get() - 1;
                    pause.set(t);
                    if t <= 0 {
                        state.set(0);
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    });
}

/// Wrap a label in a ScrolledWindow that auto-scrolls (marquee) after 1 s if the text overflows.
pub(super) fn scrolling_label(text: &str, css_class: &str, height: i32) -> ScrolledWindow {
    let sw = ScrolledWindow::new();
    sw.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    sw.set_hexpand(true);
    sw.set_size_request(1, height);
    sw.set_vexpand(false);
    sw.set_propagate_natural_width(false);
    sw.set_min_content_width(0);
    sw.set_can_target(false);
    // Keep marquee labels from inflating the sidebar's preferred width while
    // still allowing the label to scroll inside the fixed 240 px sidebar.
    sw.set_max_content_width(SIDEBAR_RECENT_CONTENT_WIDTH);

    let label = Label::new(Some(text));
    label.add_css_class(css_class);
    label.set_halign(Align::Start);
    label.set_can_target(false);
    sw.set_child(Some(&label));

    // After 1 s, if text overflows, start a smooth scroll loop
    let sw_c = sw.clone();
    glib::timeout_add_local_once(std::time::Duration::from_secs(1), move || {
        let adj = sw_c.hadjustment();
        if adj.upper() - adj.page_size() <= 0.0 {
            return; // text fits — nothing to do
        }
        // State machine: 0 = scrolling right, 1 = pause at end, 2 = pause at start
        let state = Rc::new(Cell::new(0u8));
        let pause = Rc::new(Cell::new(0i32));
        let sw_ref = sw_c;
        glib::timeout_add_local(std::time::Duration::from_millis(30), move || {
            if !sw_ref.is_mapped() {
                return glib::ControlFlow::Break;
            }
            let adj = sw_ref.hadjustment();
            let max = adj.upper() - adj.page_size();
            if max <= 0.0 {
                return glib::ControlFlow::Break;
            }
            match state.get() {
                0 => {
                    let v = adj.value() + 0.6;
                    if v >= max {
                        adj.set_value(max);
                        state.set(1);
                        pause.set(50); // ~1.5 s pause at end
                    } else {
                        adj.set_value(v);
                    }
                }
                1 => {
                    let t = pause.get() - 1;
                    pause.set(t);
                    if t <= 0 {
                        adj.set_value(0.0);
                        state.set(2);
                        pause.set(33); // ~1 s pause at start
                    }
                }
                _ => {
                    let t = pause.get() - 1;
                    pause.set(t);
                    if t <= 0 {
                        state.set(0);
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    });

    sw
}

pub(super) fn build_sidebar_recent_item(
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
) -> Button {
    let btn = Button::new();
    btn.add_css_class("sidebar-recent-item");
    btn.set_hexpand(true);
    btn.set_halign(Align::Fill);

    let row = GtkBox::new(Orientation::Vertical, 1);
    row.set_hexpand(true);

    let title_sw = scrolling_label(&track.title, "sidebar-recent-title", 22);
    row.append(&title_sw);

    let artist_str = artist_string(track);
    let artist_sw = scrolling_label(&artist_str, "sidebar-recent-artist", 18);
    row.append(&artist_sw);

    btn.set_child(Some(&row));

    {
        let state = state.clone();
        let ps = ps.clone();
        let video_id = track.video_id.clone();
        let title = track.title.clone();
        let artist_str = artist_str.clone();
        let thumb_url = track.thumbnail_url.clone();

        btn.connect_clicked(move |_| {
            play_track(
                &state,
                &video_id,
                &title,
                &artist_str,
                thumb_url.as_deref(),
                &ps,
            );
        });
    }

    attach_queue_drag_source(
        &btn,
        QueueDragPayload::Track {
            track: track.clone(),
        },
        ps,
    );

    btn
}

// ============================================================
// HELPERS
// ============================================================
