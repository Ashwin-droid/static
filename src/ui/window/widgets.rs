use super::*;

struct HoverMarqueeState {
    hovered: Cell<bool>,
    delay_source: RefCell<Option<glib::SourceId>>,
    animation_source: RefCell<Option<glib::SourceId>>,
}

fn clear_source(source: &RefCell<Option<glib::SourceId>>) {
    if let Some(source_id) = source.borrow_mut().take() {
        source_id.remove();
    }
}

fn stop_hover_marquee(sw: &ScrolledWindow, state: &Rc<HoverMarqueeState>) {
    clear_source(&state.delay_source);
    clear_source(&state.animation_source);
    sw.hadjustment().set_value(0.0);
}

fn start_hover_marquee(sw: &ScrolledWindow, state: &Rc<HoverMarqueeState>) {
    if !state.hovered.get()
        || state.delay_source.borrow().is_some()
        || state.animation_source.borrow().is_some()
    {
        return;
    }

    let sw_c = sw.clone();
    let state_c = state.clone();
    let delay_source = glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
        state_c.delay_source.borrow_mut().take();

        if !state_c.hovered.get() || !sw_c.is_mapped() {
            return glib::ControlFlow::Break;
        }

        let adj = sw_c.hadjustment();
        if adj.upper() - adj.page_size() <= 0.0 {
            return glib::ControlFlow::Break;
        }

        // State machine: 0 = scrolling right, 1 = pause at end, 2 = pause at start
        let phase = Rc::new(Cell::new(0u8));
        let pause = Rc::new(Cell::new(0i32));
        let sw_ref = sw_c.clone();
        let state_ref = state_c.clone();
        let phase_ref = phase.clone();
        let pause_ref = pause.clone();

        let animation_source =
            glib::timeout_add_local(std::time::Duration::from_millis(30), move || {
                if !state_ref.hovered.get() {
                    state_ref.animation_source.borrow_mut().take();
                    sw_ref.hadjustment().set_value(0.0);
                    return glib::ControlFlow::Break;
                }

                if !sw_ref.is_mapped() {
                    state_ref.animation_source.borrow_mut().take();
                    return glib::ControlFlow::Break;
                }

                let adj = sw_ref.hadjustment();
                let max = adj.upper() - adj.page_size();
                if max <= 0.0 {
                    state_ref.animation_source.borrow_mut().take();
                    adj.set_value(0.0);
                    return glib::ControlFlow::Break;
                }

                match phase_ref.get() {
                    0 => {
                        let next = adj.value() + 0.6;
                        if next >= max {
                            adj.set_value(max);
                            phase_ref.set(1);
                            pause_ref.set(50);
                        } else {
                            adj.set_value(next);
                        }
                    }
                    1 => {
                        let remaining = pause_ref.get() - 1;
                        pause_ref.set(remaining);
                        if remaining <= 0 {
                            adj.set_value(0.0);
                            phase_ref.set(2);
                            pause_ref.set(33);
                        }
                    }
                    _ => {
                        let remaining = pause_ref.get() - 1;
                        pause_ref.set(remaining);
                        if remaining <= 0 {
                            phase_ref.set(0);
                        }
                    }
                }

                glib::ControlFlow::Continue
            });

        *state_c.animation_source.borrow_mut() = Some(animation_source);
        glib::ControlFlow::Break
    });

    *state.delay_source.borrow_mut() = Some(delay_source);
}

pub(super) fn attach_hover_marquee<W: IsA<gtk::Widget>>(
    hover_widget: &W,
    sw: &ScrolledWindow,
    label: Option<&Label>,
) {
    let state = Rc::new(HoverMarqueeState {
        hovered: Cell::new(false),
        delay_source: RefCell::new(None),
        animation_source: RefCell::new(None),
    });

    let motion = gtk::EventControllerMotion::new();
    {
        let sw = sw.clone();
        let state = state.clone();
        motion.connect_enter(move |_, _, _| {
            state.hovered.set(true);
            start_hover_marquee(&sw, &state);
        });
    }
    {
        let sw = sw.clone();
        let state = state.clone();
        motion.connect_leave(move |_| {
            state.hovered.set(false);
            stop_hover_marquee(&sw, &state);
        });
    }
    hover_widget.add_controller(motion);

    if let Some(label) = label {
        let sw = sw.clone();
        let state = state.clone();
        label.connect_label_notify(move |_| {
            stop_hover_marquee(&sw, &state);
            start_hover_marquee(&sw, &state);
        });
    }
}

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
    label.set_can_target(false);
    sw.set_child(Some(label));

    sw
}

/// Wrap a label in a ScrolledWindow clipped to the sidebar width.
pub(super) fn scrolling_label(text: &str, css_class: &str, height: i32) -> ScrolledWindow {
    let label = Label::new(Some(text));
    label.add_css_class(css_class);
    label.set_halign(Align::Start);
    let sw = label_scroller(&label, height);
    // Keep sidebar labels from inflating the sidebar's preferred width while
    // still allowing the label to scroll inside the fixed 240 px sidebar.
    sw.set_max_content_width(SIDEBAR_RECENT_CONTENT_WIDTH);

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
    attach_hover_marquee(&btn, &title_sw, None);
    row.append(&title_sw);

    let artist_str = artist_string(track);
    let artist_sw = scrolling_label(&artist_str, "sidebar-recent-artist", 18);
    attach_hover_marquee(&btn, &artist_sw, None);
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

    let popover = Popover::new();
    popover.add_css_class("context-menu");
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);

    let pop_box = GtkBox::new(Orientation::Vertical, 0);

    let play_next_btn = Button::with_label("Play next");
    play_next_btn.add_css_class("popover-action-btn");
    {
        let track = track.clone();
        let ps = ps.clone();
        let state = state.clone();
        let popover = popover.clone();
        play_next_btn.connect_clicked(move |_| {
            popover.popdown();
            queue_play_next(track.clone(), &ps, &state);
        });
    }
    pop_box.append(&play_next_btn);

    let add_queue_btn = Button::with_label("Add to queue");
    add_queue_btn.add_css_class("popover-action-btn");
    {
        let track = track.clone();
        let ps = ps.clone();
        let state = state.clone();
        let popover = popover.clone();
        add_queue_btn.connect_clicked(move |_| {
            popover.popdown();
            queue_add_to_end(track.clone(), &ps, &state);
        });
    }
    pop_box.append(&add_queue_btn);

    let jump_btn = Button::with_label("Jump to");
    jump_btn.add_css_class("popover-action-btn");
    jump_btn.set_visible(can_jump_to_radio(ps));
    {
        let track = track.clone();
        let ps = ps.clone();
        let state = state.clone();
        let popover = popover.clone();
        jump_btn.connect_clicked(move |_| {
            popover.popdown();
            jump_to_radio_seed_from_track(track.clone(), &ps, &state);
        });
    }
    pop_box.append(&jump_btn);

    let probe_btn = Button::with_label("Probe");
    probe_btn.add_css_class("popover-action-btn");
    {
        let track = track.clone();
        let ps = ps.clone();
        let state = state.clone();
        let popover = popover.clone();
        probe_btn.connect_clicked(move |_| {
            popover.popdown();
            probe_track(track.clone(), &ps, &state);
        });
    }
    pop_box.append(&probe_btn);

    append_track_feedback_actions(&pop_box, track, state);
    append_download_action(&pop_box, track, state, &popover);

    popover.set_child(Some(&pop_box));
    popover.set_parent(&btn);

    let gesture = GestureClick::new();
    gesture.set_button(3);
    {
        let popover = popover.clone();
        let jump_btn = jump_btn.clone();
        let ps = ps.clone();
        gesture.connect_pressed(move |_, _, _, _| {
            jump_btn.set_visible(can_jump_to_radio(&ps));
            popover.popup();
        });
    }
    btn.add_controller(gesture);

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
