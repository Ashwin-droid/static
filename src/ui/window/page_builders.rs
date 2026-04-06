use super::*;

pub(super) fn build_onboarding_page(stack: &Stack, state: &Arc<Mutex<AppState>>) -> GtkBox {
    let page = GtkBox::new(Orientation::Vertical, 0);
    page.set_valign(Align::Center);
    page.set_halign(Align::Center);
    page.set_width_request(480);

    let content = GtkBox::new(Orientation::Vertical, 16);
    content.add_css_class("onboarding-card");

    let logo_bytes = glib::Bytes::from_static(include_bytes!("../../../assets/static.png"));
    let stream = gio::MemoryInputStream::from_bytes(&logo_bytes);
    if let Ok(pixbuf) =
        gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(&stream, 96, 96, true, gio::Cancellable::NONE)
    {
        let texture = gtk::gdk::Texture::for_pixbuf(&pixbuf);
        let logo = Picture::for_paintable(&texture);
        logo.set_size_request(96, 96);
        logo.set_halign(Align::Center);
        logo.set_margin_bottom(4);
        content.append(&logo);
    }

    let title = Label::new(Some("Static"));
    title.add_css_class("onboarding-title");
    content.append(&title);

    let subtitle = Label::new(Some("YouTube Music, natively."));
    subtitle.add_css_class("onboarding-subtitle");
    content.append(&subtitle);

    content.append(&Separator::new(Orientation::Horizontal));

    let instructions = Label::new(Some("To get started, paste your YouTube Music cookie:"));
    instructions.add_css_class("onboarding-instructions");
    instructions.set_halign(Align::Start);
    instructions.set_wrap(true);
    content.append(&instructions);

    let steps_label = Label::new(Some(
        "1. Open music.youtube.com in your browser\n\
         2. Open DevTools (F12) \u{2192} Network tab\n\
         3. Reload the page\n\
         4. Click any request to music.youtube.com\n\
         5. Copy the \u{2018}Cookie\u{2019} header value",
    ));
    steps_label.add_css_class("onboarding-steps");
    steps_label.set_halign(Align::Start);
    steps_label.set_wrap(true);
    content.append(&steps_label);

    let cookie_entry = Entry::builder()
        .placeholder_text("Paste cookie header here...")
        .hexpand(true)
        .visibility(false)
        .build();
    cookie_entry.add_css_class("onboarding-entry");
    content.append(&cookie_entry);

    let connect_btn = Button::with_label("Connect");
    connect_btn.add_css_class("connect-btn");
    content.append(&connect_btn);

    let status_label = Label::new(None);
    status_label.add_css_class("onboarding-status");
    content.append(&status_label);

    page.append(&content);

    {
        let state = state.clone();
        let stack = stack.clone();
        let cookie_entry_for_click = cookie_entry.clone();
        let cookie_entry_for_activate = cookie_entry.clone();
        let status_label = status_label.clone();
        let connect_btn_inner = connect_btn.clone();

        let do_connect = move || {
            let cookie_text = cookie_entry_for_click.text().to_string();
            if cookie_text.is_empty() {
                status_label.set_text("Please paste a cookie first");
                return;
            }

            connect_btn_inner.set_sensitive(false);
            connect_btn_inner.set_label("Connecting...");
            status_label.set_text("");

            match YtMusicClient::new(&cookie_text) {
                Ok(ytm) => {
                    {
                        let mut guard = state.lock().unwrap();
                        guard.client = Some(Arc::new(ytm));
                        guard.home_cache = None;
                    }
                    let state = state.clone();
                    let stack = stack.clone();
                    let cookie_text = cookie_text.clone();

                    glib::spawn_future_local(async move {
                        let state_bg = state.clone();
                        let result = gio::spawn_blocking(move || {
                            let Some(client) = app_client(&state_bg) else {
                                return Err(anyhow::anyhow!("Not connected"));
                            };
                            client.check_auth()
                        })
                        .await;

                        let cookie_for_save = cookie_text.clone();
                        let _ = gio::spawn_blocking(move || {
                            let _ = store_cookie(&cookie_for_save);
                        })
                        .await;

                        match result {
                            Ok(Ok(true)) | Ok(Ok(false)) => {
                                stack.set_visible_child_name("vr_auth");
                            }
                            Ok(Err(e)) => {
                                eprintln!(
                                    "[AUTH] onboarding cookie auth check failed, continuing offline-first: {e}"
                                );
                                stack.set_visible_child_name("vr_auth");
                            }
                            Err(e) => {
                                eprintln!(
                                    "[AUTH] onboarding cookie auth worker failed, continuing offline-first: {e:?}"
                                );
                                stack.set_visible_child_name("vr_auth");
                            }
                        }
                    });
                }
                Err(e) => {
                    status_label.set_text(&format!("Invalid cookie: {e}"));
                    connect_btn_inner.set_sensitive(true);
                    connect_btn_inner.set_label("Connect");
                }
            }
        };

        let do_connect_click = do_connect.clone();
        connect_btn.connect_clicked(move |_| do_connect_click());
        cookie_entry_for_activate.connect_activate(move |_| do_connect());
    }

    page
}

// ============================================================
// VR AUTH PAGE (optional step 2)
// ============================================================

pub(super) fn build_vr_auth_page(stack: &Stack, state: &Arc<Mutex<AppState>>) -> GtkBox {
    let page = GtkBox::new(Orientation::Vertical, 0);
    page.set_valign(Align::Center);
    page.set_halign(Align::Center);
    page.set_width_request(480);

    let content = GtkBox::new(Orientation::Vertical, 16);
    content.add_css_class("onboarding-card");

    let title = Label::new(Some("Premium Playback (Optional)"));
    title.add_css_class("onboarding-title");
    content.append(&title);

    let subtitle = Label::new(Some(
        "If you have YouTube Music Premium, sign in to play\npremium-only tracks. You can skip this step.",
    ));
    subtitle.add_css_class("onboarding-subtitle");
    subtitle.set_wrap(true);
    content.append(&subtitle);

    content.append(&Separator::new(Orientation::Horizontal));

    let code_label = Label::new(None);
    code_label.add_css_class("section-header");
    code_label.set_selectable(true);
    content.append(&code_label);

    let instructions_label = Label::new(None);
    instructions_label.add_css_class("onboarding-steps");
    instructions_label.set_wrap(true);
    instructions_label.set_halign(Align::Start);
    content.append(&instructions_label);

    let status_label = Label::new(None);
    status_label.add_css_class("onboarding-status");
    content.append(&status_label);

    let btn_box = GtkBox::new(Orientation::Horizontal, 12);
    btn_box.set_halign(Align::Center);

    let sign_in_btn = Button::with_label("Sign In");
    sign_in_btn.add_css_class("connect-btn");

    let skip_btn = Button::with_label("Skip");
    skip_btn.add_css_class("nav-item");

    btn_box.append(&sign_in_btn);
    btn_box.append(&skip_btn);
    content.append(&btn_box);

    page.append(&content);

    // Skip → go straight to main
    {
        let stack = stack.clone();
        skip_btn.connect_clicked(move |_| {
            stack.set_visible_child_name("main");
        });
    }

    // Sign In → start device flow
    {
        let state = state.clone();
        let stack = stack.clone();
        let code_label = code_label.clone();
        let instructions_label = instructions_label.clone();
        let status_label = status_label.clone();
        let sign_in_btn = sign_in_btn.clone();

        sign_in_btn.clone().connect_clicked(move |_| {
            sign_in_btn.set_sensitive(false);
            sign_in_btn.set_label("Starting...");
            status_label.set_text("");

            let state = state.clone();
            let stack = stack.clone();
            let code_label = code_label.clone();
            let instructions_label = instructions_label.clone();
            let status_label = status_label.clone();
            let sign_in_btn = sign_in_btn.clone();

            glib::spawn_future_local(async move {
                let state_bg = state.clone();
                let result = gio::spawn_blocking(move || {
                    let Some(client) = app_client(&state_bg) else {
                        return Err(anyhow::anyhow!("Not connected"));
                    };
                    client.start_vr_device_flow()
                }).await;

                match result {
                    Ok(Ok((device_code, user_code, verification_url))) => {
                        code_label.set_text(&user_code);
                        instructions_label.set_text(&format!(
                            "1. Go to {verification_url}\n2. Enter the code above\n3. Sign in with your Google account\n4. Click \"Done\" below when finished"
                        ));
                        sign_in_btn.set_label("Done");
                        sign_in_btn.set_sensitive(true);

                        // Rewire button to poll
                        let state = state.clone();
                        let stack = stack.clone();
                        let status_label = status_label.clone();
                        let sign_in_btn_inner = sign_in_btn.clone();
                        // Disconnect old handler by replacing with new
                        sign_in_btn.connect_clicked(move |btn| {
                            btn.set_sensitive(false);
                            btn.set_label("Checking...");

                            let state = state.clone();
                            let stack = stack.clone();
                            let status_label = status_label.clone();
                            let sign_in_btn = sign_in_btn_inner.clone();
                            let device_code = device_code.clone();

                            glib::spawn_future_local(async move {
                                let state_bg = state.clone();
                                let dc = device_code.clone();
                                let result = gio::spawn_blocking(move || {
                                    let Some(client) = app_client(&state_bg) else {
                                        return Err(anyhow::anyhow!("Not connected"));
                                    };
                                    client.poll_vr_device_flow(&dc)
                                }).await;

                                match result {
                                    Ok(Ok(refresh_token)) => {
                                        let token = refresh_token.clone();
                                        let _ = gio::spawn_blocking(move || {
                                            let _ = store_vr_refresh_token(&token);
                                        }).await;
                                        stack.set_visible_child_name("main");
                                    }
                                    Ok(Err(e)) => {
                                        let msg = e.to_string();
                                        if msg.contains("authorization_pending") {
                                            status_label.set_text("Still waiting... complete sign-in in your browser, then click Done again.");
                                        } else {
                                            status_label.set_text(&format!("Error: {msg}"));
                                        }
                                        sign_in_btn.set_sensitive(true);
                                        sign_in_btn.set_label("Done");
                                    }
                                    Err(e) => {
                                        status_label.set_text(&format!("Error: {e:?}"));
                                        sign_in_btn.set_sensitive(true);
                                        sign_in_btn.set_label("Done");
                                    }
                                }
                            });
                        });
                    }
                    Ok(Err(e)) => {
                        status_label.set_text(&format!("Failed: {e}"));
                        sign_in_btn.set_sensitive(true);
                        sign_in_btn.set_label("Sign In");
                    }
                    Err(e) => {
                        status_label.set_text(&format!("Error: {e:?}"));
                        sign_in_btn.set_sensitive(true);
                        sign_in_btn.set_label("Sign In");
                    }
                }
            });
        });
    }

    page
}

// ============================================================
// MAIN PAGE
// ============================================================

pub(super) fn build_main_page(stack: &Stack, state: &Arc<Mutex<AppState>>) -> MainPageWidgets {
    fn next_search_request_id(search_request_serial: &Rc<Cell<u64>>) -> u64 {
        let next = search_request_serial.get().wrapping_add(1);
        search_request_serial.set(next);
        next
    }

    fn run_sidebar_search(
        state: &Arc<Mutex<AppState>>,
        content_box: &GtkBox,
        content_scroll: &ScrolledWindow,
        home_pagination: &Rc<HomePaginationState>,
        query: &str,
        ps: &Rc<PlayerState>,
        recent_list: &GtkBox,
        search_request_serial: &Rc<Cell<u64>>,
    ) {
        let query = query.trim().to_string();
        if query.is_empty() {
            return;
        }

        let request_id = next_search_request_id(search_request_serial);
        if let Some(target) = parse_direct_music_url(&query) {
            autoplay_direct_music_url(
                state,
                content_box,
                content_scroll,
                home_pagination,
                &query,
                target,
                ps,
                recent_list,
            );
        } else {
            load_search(
                state,
                content_box,
                content_scroll,
                home_pagination,
                &query,
                ps,
                recent_list,
                Some((search_request_serial.clone(), request_id)),
            );
        }
    }

    let page = GtkBox::new(Orientation::Vertical, 0);

    // Use a Paned so the sidebar has a truly fixed width (not just a minimum)
    let paned = Paned::new(Orientation::Horizontal);
    paned.set_vexpand(true);
    paned.set_position(SIDEBAR_WIDTH);
    paned.set_shrink_start_child(false);
    paned.set_resize_start_child(false);
    paned.set_wide_handle(false);

    // Right side of paned: content + queue in a regular hbox
    let hbox = GtkBox::new(Orientation::Horizontal, 0);
    hbox.set_hexpand(true);

    // ======= SIDEBAR (glass) =======
    let sidebar = GtkBox::new(Orientation::Vertical, 0);
    sidebar.add_css_class("sidebar");
    sidebar.set_overflow(gtk::Overflow::Hidden);

    // Search
    let search_shell = GtkBox::new(Orientation::Vertical, 6);
    search_shell.set_margin_start(12);
    search_shell.set_margin_end(12);
    search_shell.set_margin_top(16);
    search_shell.set_margin_bottom(12);

    let search_entry = Entry::builder().placeholder_text("Search songs...").build();
    search_entry.add_css_class("sidebar-search");
    search_shell.append(&search_entry);

    let search_suggestions_panel = GtkBox::new(Orientation::Vertical, 0);
    search_suggestions_panel.add_css_class("search-suggestions-panel");
    search_suggestions_panel.set_focusable(false);
    search_suggestions_panel.set_visible(false);

    let search_suggestions_box = GtkBox::new(Orientation::Vertical, 0);
    search_suggestions_box.add_css_class("search-suggestions-list");
    search_suggestions_box.set_focusable(false);
    search_suggestions_panel.append(&search_suggestions_box);
    search_shell.append(&search_suggestions_panel);
    sidebar.append(&search_shell);

    // Nav items
    let nav_box = GtkBox::new(Orientation::Vertical, 2);
    nav_box.add_css_class("nav-section");

    let home_btn = nav_button("Home", true);
    nav_box.append(&home_btn);

    let likes_btn = nav_button("Likes", false);
    nav_box.append(&likes_btn);

    let playlists_btn = nav_button("Playlists", false);
    nav_box.append(&playlists_btn);

    let offline_btn = nav_button("Offline", false);
    nav_box.append(&offline_btn);

    sidebar.append(&nav_box);
    sidebar.append(&Separator::new(Orientation::Horizontal));

    // Recently played (from YouTube history)
    let recent_header = Label::new(Some("Recently Played"));
    recent_header.add_css_class("sidebar-section-header");
    recent_header.set_halign(Align::Start);
    recent_header.set_margin_start(12);
    recent_header.set_margin_top(12);
    sidebar.append(&recent_header);

    let recent_list = GtkBox::new(Orientation::Vertical, 2);
    recent_list.set_margin_start(8);
    recent_list.set_margin_end(8);
    recent_list.set_margin_top(4);

    let recent_placeholder = Label::new(Some("Loading history..."));
    recent_placeholder.add_css_class("sidebar-dim");
    recent_placeholder.set_margin_top(8);
    recent_list.append(&recent_placeholder);

    let recent_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    recent_scroll.set_min_content_width(0);
    recent_scroll.set_max_content_width(SIDEBAR_WIDTH);
    recent_scroll.set_propagate_natural_width(false);
    recent_scroll.set_child(Some(&recent_list));
    install_vertical_scroll_capture(&sidebar, &recent_scroll, "sidebar-shell");
    install_vertical_scroll_capture(&recent_scroll, &recent_scroll, "sidebar-scroll");
    install_vertical_scroll_capture(&recent_list, &recent_scroll, "sidebar-list");
    sidebar.append(&recent_scroll);

    // Bottom: Settings
    sidebar.append(&Separator::new(Orientation::Horizontal));
    let bottom_box = GtkBox::new(Orientation::Horizontal, 8);
    bottom_box.add_css_class("sidebar-bottom");

    let settings_btn = Button::with_label("Settings");
    settings_btn.add_css_class("nav-item");
    settings_btn.set_hexpand(true);

    bottom_box.append(&settings_btn);
    sidebar.append(&bottom_box);
    paned.set_start_child(Some(&sidebar));

    // ======= CONTENT AREA (not glass) =======
    let content_outer = GtkBox::new(Orientation::Vertical, 0);
    content_outer.set_hexpand(true);
    content_outer.add_css_class("content-area");

    // Header
    let header_box = GtkBox::new(Orientation::Horizontal, 0);
    header_box.add_css_class("content-header-bar");

    let header_title = Label::new(Some("S  T  A  T  I  C"));
    header_title.add_css_class("content-header");
    header_title.set_hexpand(true);
    header_title.set_halign(Align::Center);
    header_box.append(&header_title);

    content_outer.append(&header_box);
    content_outer.append(&Separator::new(Orientation::Horizontal));

    // Scrollable content
    let scrolled = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .build();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_min_content_width(0);
    scrolled.set_propagate_natural_width(false);

    let content_box = GtkBox::new(Orientation::Vertical, 20);
    content_box.set_hexpand(true);
    content_box.set_halign(Align::Fill);
    content_box.set_valign(Align::Start);
    content_box.set_margin_top(24);
    content_box.set_margin_bottom(24);
    content_box.set_margin_start(28);
    content_box.set_margin_end(28);

    let home_pagination = Rc::new(HomePaginationState {
        continuation: RefCell::new(None),
        loading: Cell::new(false),
    });

    scrolled.set_child(Some(&content_box));
    install_vertical_scroll_capture(&scrolled, &scrolled, "content-scroll");
    install_vertical_scroll_capture(&content_box, &scrolled, "content-box");

    let content_overlay = Overlay::new();
    content_overlay.set_hexpand(true);
    content_overlay.set_vexpand(true);
    content_overlay.set_child(Some(&scrolled));

    let content_dimmer = GtkBox::new(Orientation::Vertical, 0);
    content_dimmer.add_css_class("content-dim-overlay");
    content_dimmer.set_hexpand(true);
    content_dimmer.set_vexpand(true);
    content_dimmer.set_halign(Align::Fill);
    content_dimmer.set_valign(Align::Fill);
    content_dimmer.set_visible(false);
    content_dimmer.set_can_target(false);
    content_overlay.add_overlay(&content_dimmer);

    content_outer.append(&content_overlay);

    // ======= NOW PLAYING BAR (YTM-style) =======
    let np_bar = GtkBox::new(Orientation::Vertical, 0);
    np_bar.add_css_class("now-playing-bar");

    // Row 1: Progress slider (full width) with buffer fill level
    let progress_slider = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 0.01);
    progress_slider.set_draw_value(false);
    progress_slider.set_hexpand(true);
    progress_slider.set_show_fill_level(true);
    progress_slider.set_restrict_to_fill_level(false);
    progress_slider.set_fill_level(0.0);
    progress_slider.add_css_class("progress-slider");
    np_bar.append(&progress_slider);

    // Row 2: All controls
    let controls_box = GtkBox::new(Orientation::Horizontal, 0);
    controls_box.set_margin_start(8);
    controls_box.set_margin_end(8);
    controls_box.set_margin_top(2);
    controls_box.set_margin_bottom(6);
    controls_box.set_valign(Align::Center);
    controls_box.set_size_request(-1, 62);

    // -- Left: Transport controls --
    let transport_box = GtkBox::new(Orientation::Horizontal, 0);
    transport_box.set_valign(Align::Center);

    let prev_btn = icon_button(ICON_PREV, 20, "np-icon-btn");
    prev_btn.set_sensitive(false);
    transport_box.append(&prev_btn);

    let play_pause_btn = icon_button(ICON_PLAY, 28, "np-icon-btn");
    play_pause_btn.add_css_class("np-play-btn");
    play_pause_btn.set_sensitive(false);
    transport_box.append(&play_pause_btn);

    let next_btn = icon_button(ICON_NEXT, 20, "np-icon-btn");
    next_btn.set_sensitive(false);
    transport_box.append(&next_btn);

    controls_box.append(&transport_box);

    // -- Time --
    let time_label = Label::new(Some("0:00 / 0:00"));
    time_label.add_css_class("time-label");
    time_label.set_margin_start(12);
    time_label.set_margin_end(16);
    controls_box.append(&time_label);

    // -- Center: Track info (expands) --
    let track_info_box = GtkBox::new(Orientation::Horizontal, 10);
    track_info_box.set_hexpand(true);
    track_info_box.set_valign(Align::Center);

    let np_thumb_frame = GtkBox::new(Orientation::Vertical, 0);
    np_thumb_frame.add_css_class("np-thumb-frame");
    np_thumb_frame.set_size_request(40, 40);
    np_thumb_frame.set_overflow(gtk::Overflow::Hidden);
    np_thumb_frame.set_valign(Align::Center);

    let np_thumb = Picture::new();
    np_thumb.set_keep_aspect_ratio(true);
    np_thumb.set_size_request(40, 40);
    np_thumb_frame.append(&np_thumb);
    track_info_box.append(&np_thumb_frame);

    let text_box = GtkBox::new(Orientation::Vertical, 0);
    text_box.set_valign(Align::Center);
    text_box.set_size_request(-1, 58);

    let title_row = GtkBox::new(Orientation::Horizontal, 8);
    title_row.set_hexpand(true);
    title_row.set_valign(Align::Center);

    let np_title = Label::new(Some("Nothing playing"));
    np_title.add_css_class("np-track-title");
    np_title.set_halign(Align::Start);
    let np_title_sw = label_scroller(&np_title, 22);
    attach_hover_marquee(&text_box, &np_title_sw, Some(&np_title));
    title_row.append(&np_title_sw);

    let np_repeat_loops_left = Label::new(None);
    np_repeat_loops_left.add_css_class("np-track-repeat-count");
    np_repeat_loops_left.set_halign(Align::Start);
    np_repeat_loops_left.set_valign(Align::Center);
    np_repeat_loops_left.set_visible(false);
    title_row.append(&np_repeat_loops_left);

    text_box.append(&title_row);

    let np_artist = Label::new(None);
    np_artist.add_css_class("np-track-artist");
    np_artist.set_halign(Align::Start);
    let np_artist_sw = label_scroller(&np_artist, 18);
    attach_hover_marquee(&text_box, &np_artist_sw, Some(&np_artist));
    text_box.append(&np_artist_sw);

    track_info_box.append(&text_box);
    controls_box.append(&track_info_box);

    // -- Right: Actions --
    let actions_box = GtkBox::new(Orientation::Horizontal, 0);
    actions_box.set_valign(Align::Center);

    let dislike_btn = icon_button(ICON_DISLIKE, 18, "np-icon-btn");
    dislike_btn.set_sensitive(false);
    actions_box.append(&dislike_btn);

    let like_btn = icon_button(ICON_LIKE, 18, "np-icon-btn");
    like_btn.set_sensitive(false);
    actions_box.append(&like_btn);

    let menu_btn = icon_button(ICON_MENU, 18, "np-icon-btn");
    menu_btn.set_sensitive(false);
    actions_box.append(&menu_btn);

    controls_box.append(&actions_box);

    // -- Volume --
    let vol_box = GtkBox::new(Orientation::Horizontal, 4);
    vol_box.set_valign(Align::Center);
    vol_box.set_margin_start(12);

    let vol_icon = svg_image(ICON_VOLUME, 18);
    vol_icon.set_opacity(0.5);
    vol_box.append(&vol_icon);

    let volume_slider = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 0.05);
    volume_slider.set_value(1.0);
    volume_slider.set_draw_value(false);
    volume_slider.set_size_request(90, -1);
    volume_slider.add_css_class("volume-slider");
    vol_box.append(&volume_slider);

    controls_box.append(&vol_box);

    // -- Modes --
    let modes_box = GtkBox::new(Orientation::Horizontal, 0);
    modes_box.set_valign(Align::Center);
    modes_box.set_margin_start(8);

    let repeat_btn = icon_button(ICON_REPEAT, 18, "np-icon-btn");
    modes_box.append(&repeat_btn);

    controls_box.append(&modes_box);

    np_bar.append(&controls_box);
    content_outer.append(&np_bar);

    hbox.append(&content_outer);

    // ======= DETAIL PANEL (right side: queue + synced lyrics) =======
    let queue_panel = GtkBox::new(Orientation::Vertical, 0);
    queue_panel.add_css_class("queue-panel");
    queue_panel.set_visible(false);

    let queue_header_row = GtkBox::new(Orientation::Horizontal, 8);
    queue_header_row.set_margin_start(16);
    queue_header_row.set_margin_end(16);
    queue_header_row.set_margin_top(12);
    queue_header_row.set_margin_bottom(8);

    let panel_tabs = GtkBox::new(Orientation::Horizontal, 6);
    panel_tabs.set_hexpand(true);

    let queue_tab_btn = detail_tab_button("Up Next", true);
    panel_tabs.append(&queue_tab_btn);

    let lyrics_tab_btn = detail_tab_button("Lyrics", false);
    panel_tabs.append(&lyrics_tab_btn);

    queue_header_row.append(&panel_tabs);

    let dismiss_queue_btn = Button::with_label("Dismiss Queue");
    dismiss_queue_btn.add_css_class("settings-secondary-btn");
    dismiss_queue_btn.set_halign(Align::End);
    dismiss_queue_btn.set_sensitive(false);
    queue_header_row.append(&dismiss_queue_btn);

    queue_panel.append(&queue_header_row);

    let detail_panel_stack = Stack::new();
    detail_panel_stack.set_vexpand(true);
    detail_panel_stack.set_transition_type(StackTransitionType::Crossfade);
    detail_panel_stack.set_hexpand(true);
    detail_panel_stack.set_vhomogeneous(false);
    detail_panel_stack.set_hhomogeneous(false);

    let queue_page = GtkBox::new(Orientation::Vertical, 0);

    // Chip filter bar (horizontal scrollable)
    let chip_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .build();
    chip_scroll.set_size_request(-1, 40);

    let chip_bar = GtkBox::new(Orientation::Horizontal, 6);
    chip_bar.set_margin_start(16);
    chip_bar.set_margin_end(16);
    chip_scroll.set_child(Some(&chip_bar));
    queue_page.append(&chip_scroll);

    // Scrollable queue list
    let queue_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    install_vertical_scroll_passthrough(&chip_scroll, &queue_scroll, "queue-chip-scroll");
    let queue_overlay = Overlay::new();
    let queue_list_box = GtkBox::new(Orientation::Vertical, 0);
    queue_list_box.set_margin_top(4);
    queue_scroll.set_child(Some(&queue_list_box));
    let queue_drop_indicator = GtkBox::new(Orientation::Vertical, 0);
    queue_drop_indicator.add_css_class("queue-drop-indicator");
    queue_drop_indicator.set_halign(Align::Fill);
    queue_drop_indicator.set_valign(Align::Start);
    queue_drop_indicator.set_can_target(false);
    queue_drop_indicator.set_visible(false);

    let queue_drop_indicator_row = GtkBox::new(Orientation::Horizontal, 8);
    queue_drop_indicator_row.add_css_class("queue-drop-indicator-row");
    queue_drop_indicator_row.set_margin_start(8);
    queue_drop_indicator_row.set_margin_end(8);
    queue_drop_indicator_row.set_margin_top(2);
    queue_drop_indicator_row.set_margin_bottom(2);

    let queue_drop_indicator_thumb = Picture::new();
    queue_drop_indicator_thumb.add_css_class("queue-drop-indicator-thumb");
    queue_drop_indicator_thumb.set_keep_aspect_ratio(true);
    queue_drop_indicator_thumb.set_size_request(40, 40);
    queue_drop_indicator_row.append(&queue_drop_indicator_thumb);

    let queue_drop_indicator_info = GtkBox::new(Orientation::Vertical, 0);
    queue_drop_indicator_info.set_hexpand(true);
    queue_drop_indicator_info.set_valign(Align::Center);

    let queue_drop_indicator_title = Label::new(None);
    queue_drop_indicator_title.set_halign(Align::Start);
    queue_drop_indicator_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    queue_drop_indicator_title.add_css_class("queue-track-title");
    queue_drop_indicator_info.append(&queue_drop_indicator_title);

    let queue_drop_indicator_subtitle = Label::new(None);
    queue_drop_indicator_subtitle.set_halign(Align::Start);
    queue_drop_indicator_subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
    queue_drop_indicator_subtitle.add_css_class("queue-track-artist");
    queue_drop_indicator_info.append(&queue_drop_indicator_subtitle);

    queue_drop_indicator_row.append(&queue_drop_indicator_info);

    let queue_drop_indicator_duration = Label::new(None);
    queue_drop_indicator_duration.add_css_class("queue-track-duration");
    queue_drop_indicator_duration.set_valign(Align::Center);
    queue_drop_indicator_row.append(&queue_drop_indicator_duration);

    queue_drop_indicator.append(&queue_drop_indicator_row);

    queue_overlay.set_child(Some(&queue_scroll));
    queue_overlay.add_overlay(&queue_drop_indicator);
    queue_page.append(&queue_overlay);

    detail_panel_stack.add_named(&queue_page, Some(DetailPanelView::Queue.stack_name()));

    let lyrics_page = GtkBox::new(Orientation::Vertical, 0);
    lyrics_page.add_css_class("lyrics-panel");

    let lyrics_meta_label = Label::new(None);
    lyrics_meta_label.add_css_class("lyrics-meta");
    lyrics_meta_label.set_halign(Align::Start);
    lyrics_meta_label.set_margin_start(16);
    lyrics_meta_label.set_margin_end(16);
    lyrics_meta_label.set_margin_top(14);
    lyrics_meta_label.set_margin_bottom(8);
    lyrics_meta_label.set_wrap(true);
    lyrics_meta_label.set_visible(false);
    lyrics_page.append(&lyrics_meta_label);

    let lyrics_status_label = Label::new(Some(
        "Play something to see synced lyrics when YouTube exposes timed text.",
    ));
    lyrics_status_label.add_css_class("lyrics-status");
    lyrics_status_label.set_halign(Align::Start);
    lyrics_status_label.set_margin_start(16);
    lyrics_status_label.set_margin_end(16);
    lyrics_status_label.set_margin_top(14);
    lyrics_status_label.set_wrap(true);
    lyrics_page.append(&lyrics_status_label);

    let lyrics_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    lyrics_scroll.set_visible(false);

    let lyrics_list_box = GtkBox::new(Orientation::Vertical, 8);
    lyrics_list_box.add_css_class("lyrics-list");
    lyrics_list_box.set_margin_start(16);
    lyrics_list_box.set_margin_end(18);
    lyrics_list_box.set_margin_top(14);
    lyrics_list_box.set_margin_bottom(18);
    lyrics_scroll.set_child(Some(&lyrics_list_box));
    lyrics_page.append(&lyrics_scroll);

    detail_panel_stack.add_named(&lyrics_page, Some(DetailPanelView::Lyrics.stack_name()));
    queue_panel.append(&detail_panel_stack);

    hbox.append(&queue_panel);
    paned.set_end_child(Some(&hbox));
    page.append(&paned);

    // ======= PLAYER STATE =======
    let ps = Rc::new(PlayerState {
        player: Player::new(),
        progress_slider: progress_slider.clone(),
        is_seeking: Cell::new(false),
        last_mpris_length_micros: Cell::new(None),
        content_dimmer: content_dimmer.clone(),
        prev_btn: prev_btn.clone(),
        play_pause_btn: play_pause_btn.clone(),
        next_btn: next_btn.clone(),
        time_label: time_label.clone(),
        np_thumb: np_thumb.clone(),
        np_title: np_title.clone(),
        np_repeat_loops_left: np_repeat_loops_left.clone(),
        np_artist: np_artist.clone(),
        dislike_btn: dislike_btn.clone(),
        like_btn: like_btn.clone(),
        menu_btn: menu_btn.clone(),
        volume_slider: volume_slider.clone(),
        repeat_btn: repeat_btn.clone(),
        repeat_mode: Cell::new(RepeatMode::Off),
        queue: RefCell::new(Vec::new()),
        queue_index: Cell::new(0),
        playlist_context: RefCell::new(None),
        playlist_like_queue: Cell::new(false),
        radio_virtual_playlist: RefCell::new(None),
        radio_append_in_flight: Cell::new(false),
        last_radio_append_anchor: RefCell::new(None),
        queue_drag_source_index: Cell::new(None),
        queue_drag_hover_viewport_y: Cell::new(None),
        queue_autoscroll_velocity: Cell::new(0.0),
        queue_autoscroll_source: RefCell::new(None),
        queue_drop_preview_slot: Cell::new(None),
        queue_drag_preview_payload: RefCell::new(None),
        queue_drop_indicator: queue_drop_indicator.clone(),
        queue_drop_indicator_thumb: queue_drop_indicator_thumb.clone(),
        queue_drop_indicator_title: queue_drop_indicator_title.clone(),
        queue_drop_indicator_subtitle: queue_drop_indicator_subtitle.clone(),
        queue_drop_indicator_duration: queue_drop_indicator_duration.clone(),
        sponsor_segments: RefCell::new(Vec::new()),
        sponsor_segments_reported: RefCell::new(HashSet::new()),
        daemon_refresh_in_flight: Cell::new(false),
        daemon_refresh_started_at: Cell::new(None),
        daemon_refresh_serial: Cell::new(0),
        daemon_recovery_in_flight: Cell::new(false),
        play_request_serial: Cell::new(0),
        queue_scroll: queue_scroll.clone(),
        queue_list_box: queue_list_box.clone(),
        chip_bar: chip_bar.clone(),
        queue_panel: queue_panel.clone(),
        queue_dismiss_btn: dismiss_queue_btn.clone(),
        detail_panel_stack: detail_panel_stack.clone(),
        detail_queue_tab_btn: queue_tab_btn.clone(),
        detail_lyrics_tab_btn: lyrics_tab_btn.clone(),
        detail_panel_mode: Cell::new(DetailPanelView::Queue),
        lyrics_status_label: lyrics_status_label.clone(),
        lyrics_meta_label: lyrics_meta_label.clone(),
        lyrics_scroll: lyrics_scroll.clone(),
        lyrics_list_box: lyrics_list_box.clone(),
        lyrics_lines: RefCell::new(Vec::new()),
        lyrics_line_widgets: RefCell::new(Vec::new()),
        lyrics_active_line: Cell::new(None),
        lyrics_request_serial: Cell::new(0),
        now_playing_track: RefCell::new(None),
        playback_session_id: Cell::new(0),
    });

    set_detail_panel_view(&ps, DetailPanelView::Queue);

    {
        let ps = ps.clone();
        let state = state.clone();
        dismiss_queue_btn.connect_clicked(move |_| {
            dismiss_queue_after_current(&ps, &state);
        });
    }

    {
        let ps = ps.clone();
        queue_tab_btn.connect_clicked(move |_| {
            set_detail_panel_view(&ps, DetailPanelView::Queue);
        });
    }

    {
        let ps = ps.clone();
        lyrics_tab_btn.connect_clicked(move |_| {
            set_detail_panel_view(&ps, DetailPanelView::Lyrics);
        });
    }

    {
        let ps_enter = ps.clone();
        let state = state.clone();
        let drop_target = DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
        drop_target.connect_enter(move |_, _, y| {
            set_queue_drop_active(&ps_enter, true);
            let raw_slot = preview_slot_for_queue_y(&ps_enter, y);
            let preview_slot = normalized_preview_slot(&ps_enter, raw_slot);
            set_queue_drop_preview(&ps_enter, preview_slot);
            let viewport_y = y - ps_enter.queue_scroll.vadjustment().value();
            update_queue_drag_hover(&ps_enter, Some(viewport_y));
            gtk::gdk::DragAction::MOVE
        });

        let ps_motion = ps.clone();
        drop_target.connect_motion(move |_, _, y| {
            set_queue_drop_active(&ps_motion, true);
            let raw_slot = preview_slot_for_queue_y(&ps_motion, y);
            let preview_slot = normalized_preview_slot(&ps_motion, raw_slot);
            set_queue_drop_preview(&ps_motion, preview_slot);
            let viewport_y = y - ps_motion.queue_scroll.vadjustment().value();
            update_queue_drag_hover(&ps_motion, Some(viewport_y));
            gtk::gdk::DragAction::MOVE
        });

        let ps_leave = ps.clone();
        drop_target.connect_leave(move |_| {
            stop_queue_autoscroll(&ps_leave);
            clear_queue_drop_preview(&ps_leave);
            set_queue_drop_active(&ps_leave, false);
        });

        let ps_drop = ps.clone();
        drop_target.connect_drop(move |_, value, _, y| {
            let Some(payload) = parse_queue_drag_payload(value) else {
                return false;
            };
            stop_queue_autoscroll(&ps_drop);
            clear_queue_drop_preview(&ps_drop);
            set_queue_drop_active(&ps_drop, false);
            let raw_slot = preview_slot_for_queue_y(&ps_drop, y);
            handle_queue_drop_payload(payload, raw_slot, &ps_drop, &state)
        });

        ps.queue_list_box.add_controller(drop_target);
    }

    // ======= WIRE SIGNALS =======

    // Play/Pause toggle
    {
        let ps = ps.clone();
        let state = state.clone();
        play_pause_btn.connect_clicked(move |btn| {
            if let Ok(snapshot) = ps.player.toggle_pause() {
                let paused = snapshot.paused;
                apply_remote_snapshot(&ps, &state, snapshot);
                set_button_icon(btn, if paused { ICON_PLAY } else { ICON_PAUSE }, 28);
            }
            sync_mpris_transport(&ps, &state);
        });
    }

    {
        let ps = ps.clone();
        menu_btn.connect_clicked(move |_| {
            let next_view = match ps.detail_panel_mode.get() {
                DetailPanelView::Queue => DetailPanelView::Lyrics,
                DetailPanelView::Lyrics => DetailPanelView::Queue,
            };
            set_detail_panel_view(&ps, next_view);
        });
    }

    // Repeat mode toggle
    {
        let ps = ps.clone();
        let state = state.clone();
        repeat_btn.connect_clicked(move |btn| {
            let new_mode = ps.repeat_mode.get().next(is_radio_queue(&ps));
            ps.repeat_mode.set(new_mode);
            update_repeat_button_ui(btn, new_mode);
            if let Ok(snapshot) = ps.player.set_repeat_mode(new_mode) {
                apply_remote_snapshot(&ps, &state, snapshot);
            }
            sync_mpris_transport(&ps, &state);
        });
    }

    // Next track
    {
        let ps = ps.clone();
        let state = state.clone();
        next_btn.connect_clicked(move |_| {
            let request_serial = next_play_request_serial(&ps);
            let client = ps.player.daemon_client();
            let ps = ps.clone();
            let state = state.clone();
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || client.next()).await;
                if !is_current_play_request(&ps, request_serial) {
                    return;
                }
                if let Ok(Ok(snapshot)) = result {
                    apply_remote_snapshot(&ps, &state, snapshot);
                }
            });
        });
    }

    // Previous track
    {
        let ps = ps.clone();
        let state = state.clone();
        prev_btn.connect_clicked(move |_| {
            let request_serial = next_play_request_serial(&ps);
            let client = ps.player.daemon_client();
            let ps = ps.clone();
            let state = state.clone();
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || client.previous()).await;
                if !is_current_play_request(&ps, request_serial) {
                    return;
                }
                if let Ok(Ok(snapshot)) = result {
                    apply_remote_snapshot(&ps, &state, snapshot);
                }
            });
        });
    }

    // Seek (progress slider)
    {
        let ps = ps.clone();
        let state = state.clone();
        progress_slider.connect_change_value(move |_, _, value| {
            if let Some(duration) = ps.player.duration() {
                let target = ClockTime::from_nseconds((value * duration.nseconds() as f64) as u64);
                apply_seek_position(&ps, target, &state);
            }
            glib::Propagation::Proceed
        });
    }

    // Volume
    {
        let ps = ps.clone();
        volume_slider.connect_value_changed(move |slider| {
            let _ = ps.player.set_volume(slider.value());
            sync_mpris_volume(&ps);
        });
    }

    // Like / dislike
    {
        let ps = ps.clone();
        let state = state.clone();
        like_btn.connect_clicked(move |_| {
            let next_rating = match current_rating(&ps) {
                SongRating::Like => SongRating::Indifferent,
                _ => SongRating::Like,
            };
            submit_song_rating(&state, &ps, next_rating);
        });
    }

    {
        let ps = ps.clone();
        let state = state.clone();
        dislike_btn.connect_clicked(move |_| {
            let next_rating = match current_rating(&ps) {
                SongRating::Dislike => SongRating::Indifferent,
                _ => SongRating::Dislike,
            };
            submit_song_rating(&state, &ps, next_rating);
        });
    }

    // Position update timer (250ms)
    {
        let ps = ps.clone();
        let state = state.clone();
        glib::timeout_add_local(Duration::from_millis(400), move || {
            if ps.daemon_refresh_in_flight.get() {
                let refresh_stuck = ps
                    .daemon_refresh_started_at
                    .get()
                    .is_some_and(|started_at| started_at.elapsed() >= Duration::from_secs(3));
                if !refresh_stuck {
                    return glib::ControlFlow::Continue;
                }

                eprintln!("[UI] daemon state poll timed out locally; retrying");
            }

            let refresh_serial = ps.daemon_refresh_serial.get().wrapping_add(1);
            ps.daemon_refresh_serial.set(refresh_serial);
            ps.daemon_refresh_in_flight.set(true);
            ps.daemon_refresh_started_at.set(Some(Instant::now()));

            let ps_for_ui = ps.clone();
            let state_for_ui = state.clone();
            let client = ps.player.daemon_client();
            glib::spawn_future_local(async move {
                let snapshot = gio::spawn_blocking(move || client.state()).await;

                if ps_for_ui.daemon_refresh_serial.get() != refresh_serial {
                    return;
                }

                ps_for_ui.daemon_refresh_in_flight.set(false);
                ps_for_ui.daemon_refresh_started_at.set(None);

                let snapshot = match snapshot {
                    Ok(Ok(snapshot)) => snapshot,
                    Ok(Err(err)) => {
                        eprintln!("[UI] staticd poll failed; attempting recovery: {err}");
                        maybe_restore_daemon_from_ui_state(&ps_for_ui, &state_for_ui);
                        return;
                    }
                    Err(err) => {
                        eprintln!("[UI] staticd poll worker failed; attempting recovery: {err:?}");
                        maybe_restore_daemon_from_ui_state(&ps_for_ui, &state_for_ui);
                        return;
                    }
                };

                if ps_for_ui.daemon_recovery_in_flight.get() {
                    return;
                }

                apply_remote_snapshot(&ps_for_ui, &state_for_ui, snapshot.clone());

                if ps_for_ui.is_seeking.get() {
                    return;
                }

                let pos = ps_for_ui.player.position();
                let dur = ps_for_ui.player.duration();
                let duration_micros = dur.map(|duration| (duration.nseconds() / 1_000) as i64);
                if ps_for_ui.last_mpris_length_micros.get() != duration_micros {
                    ps_for_ui.last_mpris_length_micros.set(duration_micros);
                    sync_mpris_metadata(&ps_for_ui, &state_for_ui);
                }

                if let (Some(pos), Some(dur)) = (pos, dur) {
                    let position_millis = pos.nseconds() / 1_000_000;
                    note_playback_position(&ps_for_ui, position_millis);
                    update_active_lyric_line(&ps_for_ui, Some(position_millis));
                    maybe_apply_sponsorblock_skip(&state_for_ui, &ps_for_ui, pos);
                    maybe_extend_radio_queue(&state_for_ui, &ps_for_ui);
                    let dur_ns = dur.nseconds();
                    if dur_ns > 0 {
                        let frac = pos.nseconds() as f64 / dur_ns as f64;
                        ps_for_ui.progress_slider.set_value(frac);
                    }
                    ps_for_ui.time_label.set_text(&format!(
                        "{} / {}",
                        format_clock_time(pos),
                        format_clock_time(dur),
                    ));
                }

                let buffered = ps_for_ui.player.buffered_fraction();
                ps_for_ui.progress_slider.set_fill_level(buffered);
                maybe_flush_watchtime_progress(&ps_for_ui);
                sync_mpris_position(&ps_for_ui);
            });
            glib::ControlFlow::Continue
        });
    }

    let nav_buttons: Rc<Vec<Button>> = Rc::new(vec![
        home_btn.clone(),
        likes_btn.clone(),
        playlists_btn.clone(),
        offline_btn.clone(),
        settings_btn.clone(),
    ]);
    let search_request_serial = Rc::new(Cell::new(0_u64));
    let search_suggestion_request_serial = Rc::new(Cell::new(0_u64));
    let search_suggestion_debounce = Rc::new(RefCell::new(None::<glib::SourceId>));
    let suppress_search_suggestions = Rc::new(Cell::new(false));

    {
        let state_for_value = state.clone();
        let content_box_for_value = content_box.clone();
        let content_scroll_for_value = scrolled.clone();
        let home_pagination_for_value = home_pagination.clone();
        let ps_for_value = ps.clone();
        let recent_list_for_value = recent_list.clone();
        scrolled.vadjustment().connect_value_changed(move |_| {
            if content_box_for_value.has_css_class("home-view-active") {
                let value = content_scroll_for_value.vadjustment().value();
                store_home_scroll_position(&state_for_value, value);
            }
            maybe_load_more_home_if_needed(
                &state_for_value,
                &content_box_for_value,
                &content_scroll_for_value,
                &home_pagination_for_value,
                &ps_for_value,
                &recent_list_for_value,
            );
        });

        let state_for_changed = state.clone();
        let content_box_for_changed = content_box.clone();
        let content_scroll_for_changed = scrolled.clone();
        let home_pagination_for_changed = home_pagination.clone();
        let ps_for_changed = ps.clone();
        let recent_list_for_changed = recent_list.clone();
        scrolled.vadjustment().connect_changed(move |_| {
            maybe_load_more_home_if_needed(
                &state_for_changed,
                &content_box_for_changed,
                &content_scroll_for_changed,
                &home_pagination_for_changed,
                &ps_for_changed,
                &recent_list_for_changed,
            );
        });
    }

    // Home button
    {
        let state = state.clone();
        let content_box = content_box.clone();
        let content_scroll = scrolled.clone();
        let home_pagination = home_pagination.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let nav = nav_buttons.clone();

        home_btn.connect_clicked(move |btn| {
            set_active_nav(btn, &nav);
            load_home(
                &state,
                &content_box,
                &content_scroll,
                &home_pagination,
                &ps,
                &recent_list,
                false,
            );
        });
    }

    // Likes button
    {
        let state = state.clone();
        let content_box = content_box.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let nav = nav_buttons.clone();

        likes_btn.connect_clicked(move |btn| {
            set_active_nav(btn, &nav);
            load_likes(&state, &content_box, &ps, &recent_list);
        });
    }

    // Playlists button
    {
        let state = state.clone();
        let content_box = content_box.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let nav = nav_buttons.clone();

        playlists_btn.connect_clicked(move |btn| {
            set_active_nav(btn, &nav);
            load_playlists(&state, &content_box, &ps, &recent_list);
        });
    }

    // Offline button
    {
        let state = state.clone();
        let content_box = content_box.clone();
        let content_scroll = scrolled.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let nav = nav_buttons.clone();

        offline_btn.connect_clicked(move |btn| {
            set_active_nav(btn, &nav);
            load_offline(&state, &content_box, &content_scroll, &ps, &recent_list);
        });
    }

    // Settings button
    {
        let state = state.clone();
        let stack = stack.clone();
        let content_box = content_box.clone();
        let ps = ps.clone();
        let nav = nav_buttons.clone();

        settings_btn.connect_clicked(move |btn| {
            set_active_nav(btn, &nav);
            load_settings_page(&state, &stack, &content_box, &ps);
        });
    }

    // Search
    {
        let state = state.clone();
        let content_box = content_box.clone();
        let content_scroll = scrolled.clone();
        let home_pagination = home_pagination.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let search_request_serial = search_request_serial.clone();
        let search_suggestion_request_serial = search_suggestion_request_serial.clone();
        let search_suggestion_debounce = search_suggestion_debounce.clone();
        let search_suggestions_panel = search_suggestions_panel.clone();
        let search_suggestions_box = search_suggestions_box.clone();

        search_entry.connect_activate(move |entry| {
            if let Some(source) = search_suggestion_debounce.borrow_mut().take() {
                source.remove();
            }
            next_search_request_id(&search_suggestion_request_serial);
            clear_box(&search_suggestions_box);
            search_suggestions_panel.set_visible(false);
            run_sidebar_search(
                &state,
                &content_box,
                &content_scroll,
                &home_pagination,
                entry.text().as_ref(),
                &ps,
                &recent_list,
                &search_request_serial,
            );
        });
    }

    {
        let state = state.clone();
        let content_box = content_box.clone();
        let content_scroll = scrolled.clone();
        let home_pagination = home_pagination.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let search_request_serial = search_request_serial.clone();
        let search_suggestion_request_serial = search_suggestion_request_serial.clone();
        let search_suggestion_debounce = search_suggestion_debounce.clone();
        let search_suggestions_panel = search_suggestions_panel.clone();
        let search_suggestions_box = search_suggestions_box.clone();
        let search_entry_for_suggestions = search_entry.clone();
        let suppress_search_suggestions = suppress_search_suggestions.clone();

        search_entry.connect_changed(move |entry| {
            if suppress_search_suggestions.replace(false) {
                return;
            }

            if let Some(source) = search_suggestion_debounce.borrow_mut().take() {
                source.remove();
            }

            let query = entry.text().trim().to_string();
            let request_id = next_search_request_id(&search_suggestion_request_serial);
            clear_box(&search_suggestions_box);
            search_suggestions_panel.set_visible(false);

            if query.is_empty() || parse_direct_music_url(&query).is_some() {
                return;
            }

            let state = state.clone();
            let content_box = content_box.clone();
            let content_scroll = content_scroll.clone();
            let home_pagination = home_pagination.clone();
            let ps = ps.clone();
            let recent_list = recent_list.clone();
            let search_request_serial = search_request_serial.clone();
            let search_suggestion_request_serial = search_suggestion_request_serial.clone();
            let search_suggestion_debounce_for_timeout = search_suggestion_debounce.clone();
            let search_suggestions_panel = search_suggestions_panel.clone();
            let search_suggestions_box = search_suggestions_box.clone();
            let search_entry = search_entry_for_suggestions.clone();
            let suppress_search_suggestions = suppress_search_suggestions.clone();

            let source =
                glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
                    let _ = search_suggestion_debounce_for_timeout.borrow_mut().take();

                    let state = state.clone();
                    let content_box = content_box.clone();
                    let content_scroll = content_scroll.clone();
                    let home_pagination = home_pagination.clone();
                    let ps = ps.clone();
                    let recent_list = recent_list.clone();
                    let search_request_serial = search_request_serial.clone();
                    let search_suggestion_request_serial = search_suggestion_request_serial.clone();
                    let search_suggestions_panel = search_suggestions_panel.clone();
                    let search_suggestions_box = search_suggestions_box.clone();
                    let search_entry = search_entry.clone();
                    let suppress_search_suggestions = suppress_search_suggestions.clone();
                    let query = query.clone();

                    glib::spawn_future_local(async move {
                        let state_bg = state.clone();
                        let q = query.clone();
                        let result = gio::spawn_blocking(move || {
                            app_client(&state_bg).map(|ytm| ytm.get_search_suggestions(&q))
                        })
                        .await;

                        if search_suggestion_request_serial.get() != request_id {
                            return;
                        }

                        clear_box(&search_suggestions_box);

                        match result {
                            Ok(Some(Ok(suggestions))) => {
                                let suggestions: Vec<_> = suggestions.into_iter().take(8).collect();
                                if suggestions.is_empty() {
                                    search_suggestions_panel.set_visible(false);
                                    return;
                                }

                                for suggestion in suggestions {
                                    let suggestion_btn = Button::new();
                                    suggestion_btn.add_css_class("search-suggestion-btn");
                                    suggestion_btn.set_hexpand(true);
                                    suggestion_btn.set_focusable(false);
                                    suggestion_btn.set_focus_on_click(false);

                                    let label = Label::new(Some(&suggestion));
                                    label.set_halign(Align::Start);
                                    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                                    label.set_max_width_chars(28);
                                    suggestion_btn.set_child(Some(&label));

                                    let state = state.clone();
                                    let content_box = content_box.clone();
                                    let content_scroll = content_scroll.clone();
                                    let home_pagination = home_pagination.clone();
                                    let ps = ps.clone();
                                    let recent_list = recent_list.clone();
                                    let search_request_serial = search_request_serial.clone();
                                    let search_suggestion_request_serial =
                                        search_suggestion_request_serial.clone();
                                    let search_suggestions_panel = search_suggestions_panel.clone();
                                    let search_suggestions_box_for_click =
                                        search_suggestions_box.clone();
                                    let search_entry = search_entry.clone();
                                    let suppress_search_suggestions =
                                        suppress_search_suggestions.clone();
                                    let suggestion_text = suggestion.clone();

                                    suggestion_btn.connect_clicked(move |_| {
                                        next_search_request_id(&search_suggestion_request_serial);
                                        clear_box(&search_suggestions_box_for_click);
                                        search_suggestions_panel.set_visible(false);
                                        suppress_search_suggestions.set(true);
                                        search_entry.set_text(&suggestion_text);
                                        search_entry.set_position(-1);
                                        run_sidebar_search(
                                            &state,
                                            &content_box,
                                            &content_scroll,
                                            &home_pagination,
                                            &suggestion_text,
                                            &ps,
                                            &recent_list,
                                            &search_request_serial,
                                        );
                                    });

                                    search_suggestions_box.append(&suggestion_btn);
                                }

                                search_suggestions_panel.set_visible(true);
                            }
                            Ok(Some(Err(err))) => {
                                eprintln!("[SEARCH] failed to fetch search suggestions: {err}");
                                search_suggestions_panel.set_visible(false);
                            }
                            Ok(None) => {
                                search_suggestions_panel.set_visible(false);
                            }
                            Err(err) => {
                                eprintln!("[SEARCH] search suggestions worker failed: {err:?}");
                                search_suggestions_panel.set_visible(false);
                            }
                        }
                    });
                    glib::ControlFlow::Break
                });
            *search_suggestion_debounce.borrow_mut() = Some(source);
        });
    }

    {
        let search_suggestions_panel = search_suggestions_panel.clone();
        let search_suggestions_box = search_suggestions_box.clone();
        search_entry.connect_has_focus_notify(move |entry| {
            if entry.has_focus() {
                return;
            }
            clear_box(&search_suggestions_box);
            search_suggestions_panel.set_visible(false);
        });
    }

    MainPageWidgets {
        page,
        content_box,
        content_scroll: scrolled,
        player_state: ps,
        recent_list,
        search_entry,
        home_btn,
        offline_btn,
        home_pagination,
    }
}

pub(super) fn nav_button(text: &str, active: bool) -> Button {
    let btn = Button::new();
    let label = Label::new(Some(text));
    label.set_halign(Align::Start);
    btn.set_child(Some(&label));
    btn.add_css_class("nav-item");
    if active {
        btn.add_css_class("nav-item-active");
    }
    btn
}

pub(super) fn set_active_nav(active: &Button, all: &[Button]) {
    for btn in all {
        btn.remove_css_class("nav-item-active");
    }
    active.add_css_class("nav-item-active");
}

pub(super) fn load_settings_page(
    state: &Arc<Mutex<AppState>>,
    stack: &Stack,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
) {
    clear_box(content_box);

    let settings = app_settings(state);
    let storage_root_override = get_storage_root().unwrap_or_default();
    let default_storage_root = std::env::var("HOME")
        .map(|home| format!("{home}/Music/Static"))
        .unwrap_or_else(|_| "/tmp/Music/Static".to_string());
    let (cache_root, offline_root, temp_root) = {
        let guard = state.lock().unwrap();
        (
            guard.cache.base_dir().display().to_string(),
            guard.cache.offline_dir().display().to_string(),
            guard.cache.temp_dir().display().to_string(),
        )
    };

    let page = GtkBox::new(Orientation::Vertical, 18);
    page.add_css_class("settings-page");
    page.set_margin_top(28);
    page.set_margin_bottom(28);
    page.set_margin_start(24);
    page.set_margin_end(24);

    let hero = GtkBox::new(Orientation::Vertical, 8);
    hero.add_css_class("settings-hero");

    let title = Label::new(Some("Settings"));
    title.add_css_class("settings-title");
    title.set_halign(Align::Start);
    hero.append(&title);

    let subtitle = Label::new(Some(
        "Tune how Static imports cookies, warms up playback, and manages your offline library.",
    ));
    subtitle.add_css_class("settings-subtitle");
    subtitle.set_wrap(true);
    subtitle.set_halign(Align::Start);
    hero.append(&subtitle);

    page.append(&hero);

    let sections = GtkBox::new(Orientation::Vertical, 16);
    sections.add_css_class("settings-sections");

    let playback_section = GtkBox::new(Orientation::Vertical, 12);
    playback_section.add_css_class("settings-card");
    let playback_title = Label::new(Some("Playback & Cache"));
    playback_title.add_css_class("settings-card-title");
    playback_title.set_halign(Align::Start);
    playback_section.append(&playback_title);

    let playback_copy = Label::new(Some(
        "Decide when local cache wins and how aggressively Static stages upcoming songs.",
    ));
    playback_copy.add_css_class("settings-card-copy");
    playback_copy.set_wrap(true);
    playback_copy.set_halign(Align::Start);
    playback_section.append(&playback_copy);

    let local_row = GtkBox::new(Orientation::Horizontal, 12);
    local_row.add_css_class("settings-row");
    let local_copy = GtkBox::new(Orientation::Vertical, 2);
    local_copy.set_hexpand(true);
    let local_label = Label::new(Some("Prefer local cache on playback"));
    local_label.add_css_class("settings-row-title");
    local_label.set_halign(Align::Start);
    local_copy.append(&local_label);
    let local_hint = Label::new(Some(
        "Play offline or prefetched audio first when it exists, then fall back to streaming.",
    ));
    local_hint.add_css_class("settings-row-copy");
    local_hint.set_wrap(true);
    local_hint.set_halign(Align::Start);
    local_copy.append(&local_hint);
    local_row.append(&local_copy);
    let local_switch = settings_switch(settings.prefer_local_cache_on_playback);
    {
        let state = state.clone();
        local_switch.connect_active_notify(move |sw| {
            let active = sw.is_active();
            update_app_settings(&state, |s| s.prefer_local_cache_on_playback = active);
        });
    }
    local_row.append(&local_switch);
    playback_section.append(&local_row);

    let auto_release_row = GtkBox::new(Orientation::Horizontal, 12);
    auto_release_row.add_css_class("settings-row");
    let auto_release_copy = GtkBox::new(Orientation::Vertical, 2);
    auto_release_copy.set_hexpand(true);
    let auto_release_label = Label::new(Some("Auto-release Track loops"));
    auto_release_label.add_css_class("settings-row-title");
    auto_release_label.set_halign(Align::Start);
    auto_release_copy.append(&auto_release_label);
    let auto_release_hint = Label::new(Some(
        "When Track repeat is active, release it back to Off after the current song loops itself a set number of times.",
    ));
    auto_release_hint.add_css_class("settings-row-copy");
    auto_release_hint.set_wrap(true);
    auto_release_hint.set_halign(Align::Start);
    auto_release_copy.append(&auto_release_hint);
    auto_release_row.append(&auto_release_copy);
    let auto_release_switch = settings_switch(settings.auto_release_track_loops);
    auto_release_row.append(&auto_release_switch);
    playback_section.append(&auto_release_row);

    let auto_release_after_row = GtkBox::new(Orientation::Horizontal, 12);
    auto_release_after_row.add_css_class("settings-row");
    let auto_release_after_copy = GtkBox::new(Orientation::Vertical, 2);
    auto_release_after_copy.set_hexpand(true);
    let auto_release_after_label = Label::new(Some("Auto-release Track loops after"));
    auto_release_after_label.add_css_class("settings-row-title");
    auto_release_after_label.set_halign(Align::Start);
    auto_release_after_copy.append(&auto_release_after_label);
    let auto_release_after_hint = Label::new(Some(
        "Counts only natural self-repeats of the current track, not seeks, replay clicks, or manual skips.",
    ));
    auto_release_after_hint.add_css_class("settings-row-copy");
    auto_release_after_hint.set_wrap(true);
    auto_release_after_hint.set_halign(Align::Start);
    auto_release_after_copy.append(&auto_release_after_hint);
    auto_release_after_row.append(&auto_release_after_copy);
    let auto_release_after_spin = SpinButton::with_range(1.0, 100.0, 1.0);
    auto_release_after_spin.set_value(settings.auto_release_track_loops_after as f64);
    auto_release_after_spin.set_sensitive(settings.auto_release_track_loops);
    auto_release_after_row.append(&auto_release_after_spin);
    playback_section.append(&auto_release_after_row);

    {
        let state = state.clone();
        let auto_release_after_spin = auto_release_after_spin.clone();
        auto_release_switch.connect_active_notify(move |sw| {
            let active = sw.is_active();
            auto_release_after_spin.set_sensitive(active);
            if active == app_settings(&state).auto_release_track_loops {
                return;
            }
            update_app_settings(&state, |s| s.auto_release_track_loops = active);
        });
    }

    {
        let state = state.clone();
        auto_release_after_spin.connect_value_changed(move |spin| {
            let next_value = (spin.value() as u32).clamp(1, 100);
            if next_value == app_settings(&state).auto_release_track_loops_after {
                return;
            }
            update_app_settings(&state, |s| s.auto_release_track_loops_after = next_value);
        });
    }

    let mpris_row = GtkBox::new(Orientation::Horizontal, 12);
    mpris_row.add_css_class("settings-row");
    let mpris_copy = GtkBox::new(Orientation::Vertical, 2);
    mpris_copy.set_hexpand(true);
    let mpris_label = Label::new(Some("Respect other MPRIS players"));
    mpris_label.add_css_class("settings-row-title");
    mpris_label.set_halign(Align::Start);
    mpris_copy.append(&mpris_label);
    let mpris_hint = Label::new(Some(
        "Pause Static when another media app starts playing, and pause that app when you resume here.",
    ));
    mpris_hint.add_css_class("settings-row-copy");
    mpris_hint.set_wrap(true);
    mpris_hint.set_halign(Align::Start);
    mpris_copy.append(&mpris_hint);
    mpris_row.append(&mpris_copy);
    let mpris_switch = settings_switch(settings.respect_other_mpris_players);
    {
        let state = state.clone();
        mpris_switch.connect_active_notify(move |sw| {
            let active = sw.is_active();
            update_app_settings(&state, |s| s.respect_other_mpris_players = active);
        });
    }
    mpris_row.append(&mpris_switch);
    playback_section.append(&mpris_row);

    let pipewire_row = GtkBox::new(Orientation::Horizontal, 12);
    pipewire_row.add_css_class("settings-row");
    let pipewire_copy = GtkBox::new(Orientation::Vertical, 2);
    pipewire_copy.set_hexpand(true);
    let pipewire_label = Label::new(Some("PipeWire audio ducking"));
    pipewire_label.add_css_class("settings-row-title");
    pipewire_label.set_halign(Align::Start);
    pipewire_copy.append(&pipewire_label);
    let pipewire_hint = Label::new(Some(
        "Lower Static when non-MPRIS audio streams like games or calls stay active for a few seconds.",
    ));
    pipewire_hint.add_css_class("settings-row-copy");
    pipewire_hint.set_wrap(true);
    pipewire_hint.set_halign(Align::Start);
    pipewire_copy.append(&pipewire_hint);
    pipewire_row.append(&pipewire_copy);
    let pipewire_switch = settings_switch(settings.pipewire_ducking_enabled);
    pipewire_row.append(&pipewire_switch);
    playback_section.append(&pipewire_row);

    let pipewire_pause_row = GtkBox::new(Orientation::Horizontal, 12);
    pipewire_pause_row.add_css_class("settings-row");
    let pipewire_pause_copy = GtkBox::new(Orientation::Vertical, 2);
    pipewire_pause_copy.set_hexpand(true);
    let pipewire_pause_label = Label::new(Some("Duck pause"));
    pipewire_pause_label.add_css_class("settings-row-title");
    pipewire_pause_label.set_halign(Align::Start);
    pipewire_pause_copy.append(&pipewire_pause_label);
    let pipewire_pause_hint = Label::new(Some(
        "Pause Static instead of lowering volume once the PipeWire threshold is reached.",
    ));
    pipewire_pause_hint.add_css_class("settings-row-copy");
    pipewire_pause_hint.set_wrap(true);
    pipewire_pause_hint.set_halign(Align::Start);
    pipewire_pause_copy.append(&pipewire_pause_hint);
    pipewire_pause_row.append(&pipewire_pause_copy);
    let pipewire_pause_switch = settings_switch(settings.pipewire_duck_pause);
    pipewire_pause_switch.set_sensitive(settings.pipewire_ducking_enabled);
    {
        let state = state.clone();
        pipewire_pause_switch.connect_active_notify(move |sw| {
            let active = sw.is_active();
            update_app_settings(&state, |s| s.pipewire_duck_pause = active);
        });
    }
    pipewire_pause_row.append(&pipewire_pause_switch);
    playback_section.append(&pipewire_pause_row);

    let pipewire_threshold_row = GtkBox::new(Orientation::Horizontal, 12);
    pipewire_threshold_row.add_css_class("settings-row");
    let pipewire_threshold_copy = GtkBox::new(Orientation::Vertical, 2);
    pipewire_threshold_copy.set_hexpand(true);
    let pipewire_threshold_label = Label::new(Some("PipeWire trigger delay"));
    pipewire_threshold_label.add_css_class("settings-row-title");
    pipewire_threshold_label.set_halign(Align::Start);
    pipewire_threshold_copy.append(&pipewire_threshold_label);
    let pipewire_threshold_hint = Label::new(Some(
        "How long a foreign audio stream must stay active before ducking or pausing starts.",
    ));
    pipewire_threshold_hint.add_css_class("settings-row-copy");
    pipewire_threshold_hint.set_wrap(true);
    pipewire_threshold_hint.set_halign(Align::Start);
    pipewire_threshold_copy.append(&pipewire_threshold_hint);
    pipewire_threshold_row.append(&pipewire_threshold_copy);
    let pipewire_threshold_spin = SpinButton::with_range(1.0, 30.0, 1.0);
    pipewire_threshold_spin.set_value(settings.pipewire_ducking_threshold_secs as f64);
    pipewire_threshold_spin.set_sensitive(settings.pipewire_ducking_enabled);
    {
        let state = state.clone();
        pipewire_threshold_spin.connect_value_changed(move |spin| {
            let value = spin.value() as u32;
            update_app_settings(&state, |s| {
                s.pipewire_ducking_threshold_secs = value.clamp(1, 30)
            });
        });
    }
    pipewire_threshold_row.append(&pipewire_threshold_spin);
    playback_section.append(&pipewire_threshold_row);

    let pipewire_volume_row = GtkBox::new(Orientation::Horizontal, 12);
    pipewire_volume_row.add_css_class("settings-row");
    let pipewire_volume_copy = GtkBox::new(Orientation::Vertical, 2);
    pipewire_volume_copy.set_hexpand(true);
    let pipewire_volume_label = Label::new(Some("PipeWire duck volume"));
    pipewire_volume_label.add_css_class("settings-row-title");
    pipewire_volume_label.set_halign(Align::Start);
    pipewire_volume_copy.append(&pipewire_volume_label);
    let pipewire_volume_hint = Label::new(Some(
        "Temporary playback level while foreign audio is active.",
    ));
    pipewire_volume_hint.add_css_class("settings-row-copy");
    pipewire_volume_hint.set_wrap(true);
    pipewire_volume_hint.set_halign(Align::Start);
    pipewire_volume_copy.append(&pipewire_volume_hint);
    pipewire_volume_row.append(&pipewire_volume_copy);
    let pipewire_volume_scale = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 0.05);
    pipewire_volume_scale.set_value(settings.pipewire_ducking_volume);
    pipewire_volume_scale.set_digits(2);
    pipewire_volume_scale.set_draw_value(true);
    pipewire_volume_scale.set_size_request(160, -1);
    pipewire_volume_scale.set_sensitive(settings.pipewire_ducking_enabled);
    {
        let state = state.clone();
        pipewire_volume_scale.connect_value_changed(move |scale| {
            let value = scale.value().clamp(0.0, 1.0);
            update_app_settings(&state, |s| s.pipewire_ducking_volume = value);
        });
    }
    pipewire_volume_row.append(&pipewire_volume_scale);
    playback_section.append(&pipewire_volume_row);

    {
        let state = state.clone();
        let pipewire_pause_switch = pipewire_pause_switch.clone();
        let pipewire_threshold_spin = pipewire_threshold_spin.clone();
        let pipewire_volume_scale = pipewire_volume_scale.clone();
        pipewire_switch.connect_active_notify(move |sw| {
            let active = sw.is_active();
            pipewire_pause_switch.set_sensitive(active);
            pipewire_threshold_spin.set_sensitive(active);
            pipewire_volume_scale.set_sensitive(active);
            update_app_settings(&state, |s| s.pipewire_ducking_enabled = active);
        });
    }

    let prefetch_row = GtkBox::new(Orientation::Horizontal, 12);
    prefetch_row.add_css_class("settings-row");
    let prefetch_copy = GtkBox::new(Orientation::Vertical, 2);
    prefetch_copy.set_hexpand(true);
    let prefetch_label = Label::new(Some("Prefetch upcoming tracks"));
    prefetch_label.add_css_class("settings-row-title");
    prefetch_label.set_halign(Align::Start);
    prefetch_copy.append(&prefetch_label);
    let prefetch_hint = Label::new(Some(
        "Downloads the next few queue items into temp cache for faster skips and rewinds.",
    ));
    prefetch_hint.add_css_class("settings-row-copy");
    prefetch_hint.set_wrap(true);
    prefetch_hint.set_halign(Align::Start);
    prefetch_copy.append(&prefetch_hint);
    prefetch_row.append(&prefetch_copy);
    let prefetch_switch = settings_switch(settings.prefetch_enabled);
    {
        let state = state.clone();
        prefetch_switch.connect_active_notify(move |sw| {
            let active = sw.is_active();
            update_app_settings(&state, |s| s.prefetch_enabled = active);
        });
    }
    prefetch_row.append(&prefetch_switch);
    playback_section.append(&prefetch_row);

    let prefetch_count_row = GtkBox::new(Orientation::Horizontal, 12);
    prefetch_count_row.add_css_class("settings-row");
    let prefetch_count_copy = GtkBox::new(Orientation::Vertical, 2);
    prefetch_count_copy.set_hexpand(true);
    let prefetch_count_label = Label::new(Some("Prefetch lookahead"));
    prefetch_count_label.add_css_class("settings-row-title");
    prefetch_count_label.set_halign(Align::Start);
    prefetch_count_copy.append(&prefetch_count_label);
    let prefetch_count_hint = Label::new(Some(
        "How many upcoming songs should be staged in temp cache.",
    ));
    prefetch_count_hint.add_css_class("settings-row-copy");
    prefetch_count_hint.set_wrap(true);
    prefetch_count_hint.set_halign(Align::Start);
    prefetch_count_copy.append(&prefetch_count_hint);
    prefetch_count_row.append(&prefetch_count_copy);
    let prefetch_spin = SpinButton::with_range(0.0, 12.0, 1.0);
    prefetch_spin.set_value(settings.prefetch_ahead as f64);
    {
        let state = state.clone();
        prefetch_spin.connect_value_changed(move |spin| {
            let value = spin.value() as u32;
            update_app_settings(&state, |s| s.prefetch_ahead = value.min(12));
        });
    }
    prefetch_count_row.append(&prefetch_spin);
    playback_section.append(&prefetch_count_row);

    sections.append(&playback_section);

    let sponsorblock_section = GtkBox::new(Orientation::Vertical, 12);
    sponsorblock_section.add_css_class("settings-card");
    let sponsorblock_title = Label::new(Some("SponsorBlock"));
    sponsorblock_title.add_css_class("settings-card-title");
    sponsorblock_title.set_halign(Align::Start);
    sponsorblock_section.append(&sponsorblock_title);

    let sponsorblock_copy = Label::new(Some(
        "Choose whether Static should auto-skip community-marked segments, and which categories count.",
    ));
    sponsorblock_copy.add_css_class("settings-card-copy");
    sponsorblock_copy.set_wrap(true);
    sponsorblock_copy.set_halign(Align::Start);
    sponsorblock_section.append(&sponsorblock_copy);

    let sponsorblock_row = GtkBox::new(Orientation::Horizontal, 12);
    sponsorblock_row.add_css_class("settings-row");
    let sponsorblock_row_copy = GtkBox::new(Orientation::Vertical, 2);
    sponsorblock_row_copy.set_hexpand(true);
    let sponsorblock_label = Label::new(Some("Auto-skip SponsorBlock segments"));
    sponsorblock_label.add_css_class("settings-row-title");
    sponsorblock_label.set_halign(Align::Start);
    sponsorblock_row_copy.append(&sponsorblock_label);
    let sponsorblock_hint = Label::new(Some(
        "Skips community-marked sponsor, self-promo, intro/outro, interaction, preview, and music-offtopic sections during playback.",
    ));
    sponsorblock_hint.add_css_class("settings-row-copy");
    sponsorblock_hint.set_wrap(true);
    sponsorblock_hint.set_halign(Align::Start);
    sponsorblock_row_copy.append(&sponsorblock_hint);
    sponsorblock_row.append(&sponsorblock_row_copy);
    let sponsorblock_switch = settings_switch(settings.sponsorblock_enabled);
    let sponsorblock_categories_box = GtkBox::new(Orientation::Vertical, 12);
    sponsorblock_categories_box.set_sensitive(settings.sponsorblock_enabled);
    {
        let state = state.clone();
        let ps = ps.clone();
        let sponsorblock_categories_box = sponsorblock_categories_box.clone();
        sponsorblock_switch.connect_active_notify(move |sw| {
            let active = sw.is_active();
            sponsorblock_categories_box.set_sensitive(active);
            update_app_settings(&state, |s| s.sponsorblock_enabled = active);
            refresh_sponsorblock_for_current_track(&state, &ps);
        });
    }
    sponsorblock_row.append(&sponsorblock_switch);
    sponsorblock_section.append(&sponsorblock_row);

    let sponsorblock_categories_intro = Label::new(Some(
        "Selected categories are skipped automatically, including after MPRIS seeks or manual rewinds into a marked segment.",
    ));
    sponsorblock_categories_intro.add_css_class("settings-row-copy");
    sponsorblock_categories_intro.set_wrap(true);
    sponsorblock_categories_intro.set_halign(Align::Start);
    sponsorblock_categories_box.append(&sponsorblock_categories_intro);

    for option in SPONSORBLOCK_CATEGORY_OPTIONS {
        let category_row = GtkBox::new(Orientation::Horizontal, 12);
        category_row.add_css_class("settings-row");
        let category_copy = GtkBox::new(Orientation::Vertical, 2);
        category_copy.set_hexpand(true);
        let category_label = Label::new(Some(option.label));
        category_label.add_css_class("settings-row-title");
        category_label.set_halign(Align::Start);
        category_copy.append(&category_label);
        let category_hint = Label::new(Some(option.description));
        category_hint.add_css_class("settings-row-copy");
        category_hint.set_wrap(true);
        category_hint.set_halign(Align::Start);
        category_copy.append(&category_hint);
        category_row.append(&category_copy);

        let category_switch = settings_switch(
            settings
                .sponsorblock_categories
                .iter()
                .any(|category| category == option.key),
        );
        {
            let state = state.clone();
            let ps = ps.clone();
            let category_key = option.key.to_string();
            category_switch.connect_active_notify(move |sw| {
                let active = sw.is_active();
                update_app_settings(&state, |settings| {
                    let mut categories = settings.sponsorblock_categories.clone();
                    if active {
                        if !categories.iter().any(|category| category == &category_key) {
                            categories.push(category_key.clone());
                        }
                    } else {
                        categories.retain(|category| category != &category_key);
                    }
                    settings.sponsorblock_categories = sanitize_categories(&categories);
                });
                refresh_sponsorblock_for_current_track(&state, &ps);
            });
        }
        category_row.append(&category_switch);
        sponsorblock_categories_box.append(&category_row);
    }

    sponsorblock_section.append(&sponsorblock_categories_box);
    sections.append(&sponsorblock_section);

    let offline_section = GtkBox::new(Orientation::Vertical, 12);
    offline_section.add_css_class("settings-card");
    let offline_title = Label::new(Some("Offline Library"));
    offline_title.add_css_class("settings-card-title");
    offline_title.set_halign(Align::Start);
    offline_section.append(&offline_title);

    let offline_copy = Label::new(Some(
        "Control Listen again caching separately from Static's heuristic offline cache.",
    ));
    offline_copy.add_css_class("settings-card-copy");
    offline_copy.set_wrap(true);
    offline_copy.set_halign(Align::Start);
    offline_section.append(&offline_copy);

    let listen_again_row = GtkBox::new(Orientation::Horizontal, 12);
    listen_again_row.add_css_class("settings-row");
    let listen_again_copy = GtkBox::new(Orientation::Vertical, 2);
    listen_again_copy.set_hexpand(true);
    let listen_again_label = Label::new(Some("Listen again cache"));
    listen_again_label.add_css_class("settings-row-title");
    listen_again_label.set_halign(Align::Start);
    listen_again_copy.append(&listen_again_label);
    let listen_again_hint = Label::new(Some(
        "Keep songs from Listen again offline separately from Static's heuristic offline cache.",
    ));
    listen_again_hint.add_css_class("settings-row-copy");
    listen_again_hint.set_wrap(true);
    listen_again_hint.set_halign(Align::Start);
    listen_again_copy.append(&listen_again_hint);
    listen_again_row.append(&listen_again_copy);
    let listen_again_switch = settings_switch(settings.auto_cache_listen_again);
    {
        let state = state.clone();
        let stack = stack.clone();
        let listen_again_switch_ref = listen_again_switch.clone();
        let guard_flag = Rc::new(Cell::new(false));
        let guard_flag_closure = guard_flag.clone();
        listen_again_switch.connect_active_notify(move |sw| {
            if guard_flag_closure.get() {
                return;
            }

            let next_value = sw.is_active();
            let current_value = app_settings(&state).auto_cache_listen_again;
            if next_value == current_value {
                return;
            }

            let state = state.clone();
            let stack = stack.clone();
            let listen_again_switch = listen_again_switch_ref.clone();
            let guard_flag = guard_flag.clone();
            glib::spawn_future_local(async move {
                let impact = gio::spawn_blocking({
                    let state = state.clone();
                    move || {
                        preview_auto_cache_evictions(&state, |s| {
                            s.auto_cache_listen_again = next_value
                        })
                    }
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);

                let apply_change =
                    |guard_flag: &Rc<Cell<bool>>, listen_again_switch: &Switch, value: bool| {
                        guard_flag.set(true);
                        listen_again_switch.set_active(value);
                        guard_flag.set(false);
                    };

                if !next_value && impact > 0 {
                    let song_label = if impact == 1 { "song" } else { "songs" };
                    let dialog = build_settings_warning_dialog(
                        &stack,
                        "Turn off Listen again cache?",
                        &format!(
                            "{impact} auto-downloaded {song_label} would be evicted immediately because they only qualify through Listen again cache right now."
                        ),
                        &format!("Apply and evict {impact} {song_label}"),
                    );
                    let response = dialog.run_future().await;
                    dialog.close();
                    if response != ResponseType::Accept {
                        apply_change(&guard_flag, &listen_again_switch, current_value);
                        return;
                    }
                }

                match gio::spawn_blocking({
                    let state = state.clone();
                    move || persist_and_sync_settings(&state, |s| s.auto_cache_listen_again = next_value)
                })
                .await
                {
                    Ok(Ok(())) => apply_change(&guard_flag, &listen_again_switch, next_value),
                    _ => apply_change(&guard_flag, &listen_again_switch, current_value),
                }
            });
        });
    }
    listen_again_row.append(&listen_again_switch);
    offline_section.append(&listen_again_row);

    let listen_again_days_row = GtkBox::new(Orientation::Horizontal, 12);
    listen_again_days_row.add_css_class("settings-row");
    let listen_again_days_copy = GtkBox::new(Orientation::Vertical, 2);
    listen_again_days_copy.set_hexpand(true);
    let listen_again_days_label = Label::new(Some("Listen again cache days"));
    listen_again_days_label.add_css_class("settings-row-title");
    listen_again_days_label.set_halign(Align::Start);
    listen_again_days_copy.append(&listen_again_days_label);
    let listen_again_days_hint = Label::new(Some(
        "How many days to keep Listen again songs offline after they were last recommended or played.",
    ));
    listen_again_days_hint.add_css_class("settings-row-copy");
    listen_again_days_hint.set_wrap(true);
    listen_again_days_hint.set_halign(Align::Start);
    listen_again_days_copy.append(&listen_again_days_hint);
    listen_again_days_row.append(&listen_again_days_copy);
    let listen_again_days_spin = SpinButton::with_range(1.0, 365.0, 1.0);
    listen_again_days_spin.set_value(settings.listen_again_cache_days as f64);
    {
        let state = state.clone();
        let stack = stack.clone();
        let listen_again_days_spin_ref = listen_again_days_spin.clone();
        let guard_flag = Rc::new(Cell::new(false));
        let guard_flag_closure = guard_flag.clone();
        listen_again_days_spin.connect_value_changed(move |spin| {
            if guard_flag_closure.get() {
                return;
            }

            let next_value = (spin.value() as u32).clamp(1, 365);
            let current_value = app_settings(&state).listen_again_cache_days;
            if next_value == current_value {
                return;
            }

            let state = state.clone();
            let stack = stack.clone();
            let listen_again_days_spin = listen_again_days_spin_ref.clone();
            let guard_flag = guard_flag.clone();
            glib::spawn_future_local(async move {
                let impact = gio::spawn_blocking({
                    let state = state.clone();
                    move || {
                        preview_auto_cache_evictions(&state, |s| {
                            s.listen_again_cache_days = next_value
                        })
                    }
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);

                let apply_change =
                    |guard_flag: &Rc<Cell<bool>>, spin: &SpinButton, value: u32| {
                        guard_flag.set(true);
                        spin.set_value(value as f64);
                        guard_flag.set(false);
                    };

                if next_value < current_value && impact > 0 {
                    let song_label = if impact == 1 { "song" } else { "songs" };
                    let dialog = build_settings_warning_dialog(
                        &stack,
                        "Shorten Listen again cache?",
                        &format!(
                            "{impact} auto-downloaded {song_label} would be evicted immediately because they would fall outside the new Listen again cache window."
                        ),
                        &format!("Apply and evict {impact} {song_label}"),
                    );
                    let response = dialog.run_future().await;
                    dialog.close();
                    if response != ResponseType::Accept {
                        apply_change(&guard_flag, &listen_again_days_spin, current_value);
                        return;
                    }
                }

                match gio::spawn_blocking({
                    let state = state.clone();
                    move || persist_and_sync_settings(&state, |s| s.listen_again_cache_days = next_value)
                })
                .await
                {
                    Ok(Ok(())) => apply_change(&guard_flag, &listen_again_days_spin, next_value),
                    _ => apply_change(&guard_flag, &listen_again_days_spin, current_value),
                }
            });
        });
    }
    listen_again_days_row.append(&listen_again_days_spin);
    offline_section.append(&listen_again_days_row);

    let manage_row = GtkBox::new(Orientation::Horizontal, 12);
    manage_row.add_css_class("settings-row");
    let manage_copy = GtkBox::new(Orientation::Vertical, 2);
    manage_copy.set_hexpand(true);
    let manage_label = Label::new(Some("Offline cache"));
    manage_label.add_css_class("settings-row-title");
    manage_label.set_halign(Align::Start);
    manage_copy.append(&manage_label);
    let manage_hint = Label::new(Some(
        "Uses Static's play-history heuristics to keep a rolling offline library, without touching pinned downloads or Listen again picks.",
    ));
    manage_hint.add_css_class("settings-row-copy");
    manage_hint.set_wrap(true);
    manage_hint.set_halign(Align::Start);
    manage_copy.append(&manage_hint);
    manage_row.append(&manage_copy);
    let manage_switch = settings_switch(settings.auto_manage_offline_cache);
    {
        let state = state.clone();
        let stack = stack.clone();
        let manage_switch_ref = manage_switch.clone();
        let guard_flag = Rc::new(Cell::new(false));
        let guard_flag_closure = guard_flag.clone();
        manage_switch.connect_active_notify(move |sw| {
            if guard_flag_closure.get() {
                return;
            }

            let next_value = sw.is_active();
            let current_value = app_settings(&state).auto_manage_offline_cache;
            if next_value == current_value {
                return;
            }

            let state = state.clone();
            let stack = stack.clone();
            let manage_switch = manage_switch_ref.clone();
            let guard_flag = guard_flag.clone();
            glib::spawn_future_local(async move {
                let impact = gio::spawn_blocking({
                    let state = state.clone();
                    move || {
                        preview_auto_cache_evictions(&state, |s| {
                            s.auto_manage_offline_cache = next_value
                        })
                    }
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);

                let apply_change =
                    |guard_flag: &Rc<Cell<bool>>, manage_switch: &Switch, value: bool| {
                        guard_flag.set(true);
                        manage_switch.set_active(value);
                        guard_flag.set(false);
                    };

                if !next_value && impact > 0 {
                    let song_label = if impact == 1 { "song" } else { "songs" };
                    let dialog = build_settings_warning_dialog(
                        &stack,
                        "Turn off offline cache?",
                        &format!(
                            "{impact} auto-downloaded {song_label} would be evicted immediately because they only qualify through the heuristic offline cache right now."
                        ),
                        &format!("Apply and evict {impact} {song_label}"),
                    );
                    let response = dialog.run_future().await;
                    dialog.close();
                    if response != ResponseType::Accept {
                        apply_change(&guard_flag, &manage_switch, current_value);
                        return;
                    }
                }

                match gio::spawn_blocking({
                    let state = state.clone();
                    move || persist_and_sync_settings(&state, |s| s.auto_manage_offline_cache = next_value)
                })
                .await
                {
                    Ok(Ok(())) => apply_change(&guard_flag, &manage_switch, next_value),
                    _ => apply_change(&guard_flag, &manage_switch, current_value),
                }
            });
        });
    }
    manage_row.append(&manage_switch);
    offline_section.append(&manage_row);

    let target_row = GtkBox::new(Orientation::Horizontal, 12);
    target_row.add_css_class("settings-row");
    let target_copy = GtkBox::new(Orientation::Vertical, 2);
    target_copy.set_hexpand(true);
    let target_label = Label::new(Some("Offline cache target"));
    target_label.add_css_class("settings-row-title");
    target_label.set_halign(Align::Start);
    target_copy.append(&target_label);
    let target_hint = Label::new(Some(
        "How many play-history picks Static should keep ready offline through the heuristic cache.",
    ));
    target_hint.add_css_class("settings-row-copy");
    target_hint.set_wrap(true);
    target_hint.set_halign(Align::Start);
    target_copy.append(&target_hint);
    target_row.append(&target_copy);
    let target_spin = SpinButton::with_range(0.0, 500.0, 5.0);
    target_spin.set_value(settings.offline_cache_target as f64);
    {
        let state = state.clone();
        let stack = stack.clone();
        let target_spin_ref = target_spin.clone();
        let guard_flag = Rc::new(Cell::new(false));
        let guard_flag_closure = guard_flag.clone();
        target_spin.connect_value_changed(move |spin| {
            if guard_flag_closure.get() {
                return;
            }

            let next_value = (spin.value() as u32).min(500);
            let current_value = app_settings(&state).offline_cache_target;
            if next_value == current_value {
                return;
            }

            let state = state.clone();
            let stack = stack.clone();
            let target_spin = target_spin_ref.clone();
            let guard_flag = guard_flag.clone();
            glib::spawn_future_local(async move {
                let impact = gio::spawn_blocking({
                    let state = state.clone();
                    move || preview_auto_cache_evictions(&state, |s| s.offline_cache_target = next_value)
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);

                let apply_change = |guard_flag: &Rc<Cell<bool>>, target_spin: &SpinButton, value: u32| {
                    guard_flag.set(true);
                    target_spin.set_value(value as f64);
                    guard_flag.set(false);
                };

                if next_value < current_value && impact > 0 {
                    let song_label = if impact == 1 { "song" } else { "songs" };
                    let dialog = build_settings_warning_dialog(
                        &stack,
                        "Lower the offline cache target?",
                        &format!(
                            "{impact} auto-downloaded {song_label} would be evicted immediately to honor the smaller heuristic offline library size."
                        ),
                        &format!("Apply and evict {impact} {song_label}"),
                    );
                    let response = dialog.run_future().await;
                    dialog.close();
                    if response != ResponseType::Accept {
                        apply_change(&guard_flag, &target_spin, current_value);
                        return;
                    }
                }

                match gio::spawn_blocking({
                    let state = state.clone();
                    move || persist_and_sync_settings(&state, |s| s.offline_cache_target = next_value)
                })
                .await
                {
                    Ok(Ok(())) => apply_change(&guard_flag, &target_spin, next_value),
                    _ => apply_change(&guard_flag, &target_spin, current_value),
                }
            });
        });
    }
    target_row.append(&target_spin);
    offline_section.append(&target_row);

    let replay_row = GtkBox::new(Orientation::Horizontal, 12);
    replay_row.add_css_class("settings-row");
    let replay_copy = GtkBox::new(Orientation::Vertical, 2);
    replay_copy.set_hexpand(true);
    let replay_label = Label::new(Some("Offline after rewatches"));
    replay_label.add_css_class("settings-row-title");
    replay_label.set_halign(Align::Start);
    replay_copy.append(&replay_label);
    let replay_hint = Label::new(Some(
        "How many rewatches a song needs before it becomes eligible for the heuristic offline cache.",
    ));
    replay_hint.add_css_class("settings-row-copy");
    replay_hint.set_wrap(true);
    replay_hint.set_halign(Align::Start);
    replay_copy.append(&replay_hint);
    replay_row.append(&replay_copy);
    let replay_spin = SpinButton::with_range(0.0, 50.0, 1.0);
    replay_spin.set_value(settings.offline_rewatch_threshold as f64);
    {
        let state = state.clone();
        let stack = stack.clone();
        let replay_spin_ref = replay_spin.clone();
        let guard_flag = Rc::new(Cell::new(false));
        let guard_flag_closure = guard_flag.clone();
        replay_spin.connect_value_changed(move |spin| {
            if guard_flag_closure.get() {
                return;
            }

            let next_value = (spin.value() as u32).min(50);
            let current_value = app_settings(&state).offline_rewatch_threshold;
            if next_value == current_value {
                return;
            }

            let state = state.clone();
            let stack = stack.clone();
            let replay_spin = replay_spin_ref.clone();
            let guard_flag = guard_flag.clone();
            let stack_for_dialog = stack.clone();
            glib::spawn_future_local(async move {
                let impact = gio::spawn_blocking({
                    let state = state.clone();
                    move || preview_auto_cache_evictions(&state, |s| s.offline_rewatch_threshold = next_value)
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);

                let apply_change = |guard_flag: &Rc<Cell<bool>>, replay_spin: &SpinButton, value: u32| {
                    guard_flag.set(true);
                    replay_spin.set_value(value as f64);
                    guard_flag.set(false);
                };

                if impact > 0 {
                    let song_label = if impact == 1 { "song" } else { "songs" };
                    let dialog = build_settings_warning_dialog(
                        &stack_for_dialog,
                        "Raise the offline replay threshold?",
                        &format!(
                            "{impact} auto-downloaded {song_label} would be evicted immediately because they no longer qualify for the heuristic offline cache."
                        ),
                        &format!("Apply and evict {impact} {song_label}"),
                    );
                    let response = dialog.run_future().await;
                    dialog.close();
                    if response != ResponseType::Accept {
                        apply_change(&guard_flag, &replay_spin, current_value);
                        return;
                    }
                }

                match gio::spawn_blocking({
                    let state = state.clone();
                    move || persist_and_sync_settings(&state, |s| s.offline_rewatch_threshold = next_value)
                })
                .await
                {
                    Ok(Ok(())) => apply_change(&guard_flag, &replay_spin, next_value),
                    _ => apply_change(&guard_flag, &replay_spin, current_value),
                }
            });
        });
    }
    replay_row.append(&replay_spin);
    offline_section.append(&replay_row);

    let decay_row = GtkBox::new(Orientation::Horizontal, 12);
    decay_row.add_css_class("settings-row");
    let decay_copy = GtkBox::new(Orientation::Vertical, 2);
    decay_copy.set_hexpand(true);
    let decay_label = Label::new(Some("Score decay window"));
    decay_label.add_css_class("settings-row-title");
    decay_label.set_halign(Align::Start);
    decay_copy.append(&decay_label);
    let decay_hint = Label::new(Some(
        "How slowly songs cool off after their last play. Lower means faster decay, higher means old favorites stick around longer.",
    ));
    decay_hint.add_css_class("settings-row-copy");
    decay_hint.set_wrap(true);
    decay_hint.set_halign(Align::Start);
    decay_copy.append(&decay_hint);
    decay_row.append(&decay_copy);
    let decay_spin = SpinButton::with_range(1.0, 365.0, 1.0);
    decay_spin.set_value(settings.score_decay_window_days as f64);
    {
        let state = state.clone();
        let stack = stack.clone();
        let decay_spin_ref = decay_spin.clone();
        let guard_flag = Rc::new(Cell::new(false));
        let guard_flag_closure = guard_flag.clone();
        decay_spin.connect_value_changed(move |spin| {
            if guard_flag_closure.get() {
                return;
            }

            let next_value = (spin.value() as u32).clamp(1, 365);
            let current_value = app_settings(&state).score_decay_window_days;
            if next_value == current_value {
                return;
            }

            let state = state.clone();
            let stack = stack.clone();
            let decay_spin = decay_spin_ref.clone();
            let guard_flag = guard_flag.clone();
            let stack_for_dialog = stack.clone();
            glib::spawn_future_local(async move {
                let impact = gio::spawn_blocking({
                    let state = state.clone();
                    move || preview_auto_cache_evictions(&state, |s| s.score_decay_window_days = next_value)
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);

                let apply_change = |guard_flag: &Rc<Cell<bool>>, decay_spin: &SpinButton, value: u32| {
                    guard_flag.set(true);
                    decay_spin.set_value(value as f64);
                    guard_flag.set(false);
                };

                if impact > 0 {
                    let song_label = if impact == 1 { "song" } else { "songs" };
                    let dialog = build_settings_warning_dialog(
                        &stack_for_dialog,
                        "Tighten score decay?",
                        &format!(
                            "{impact} auto-downloaded {song_label} would be evicted immediately because the new decay window would knock them below the offline cutoff."
                        ),
                        &format!("Apply and evict {impact} {song_label}"),
                    );
                    let response = dialog.run_future().await;
                    dialog.close();
                    if response != ResponseType::Accept {
                        apply_change(&guard_flag, &decay_spin, current_value);
                        return;
                    }
                }

                match gio::spawn_blocking({
                    let state = state.clone();
                    move || persist_and_sync_settings(&state, |s| s.score_decay_window_days = next_value)
                })
                .await
                {
                    Ok(Ok(())) => apply_change(&guard_flag, &decay_spin, next_value),
                    _ => apply_change(&guard_flag, &decay_spin, current_value),
                }
            });
        });
    }
    decay_row.append(&decay_spin);
    offline_section.append(&decay_row);

    let score_row = GtkBox::new(Orientation::Horizontal, 12);
    score_row.add_css_class("settings-row");
    let score_copy = GtkBox::new(Orientation::Vertical, 2);
    score_copy.set_hexpand(true);
    let score_label = Label::new(Some("Minimum offline score"));
    score_label.add_css_class("settings-row-title");
    score_label.set_halign(Align::Start);
    score_copy.append(&score_label);
    let score_hint = Label::new(Some(
        "Anything below this score is ignored for downloads and evicted if it is already auto-downloaded. `0` keeps the current behavior.",
    ));
    score_hint.add_css_class("settings-row-copy");
    score_hint.set_wrap(true);
    score_hint.set_halign(Align::Start);
    score_copy.append(&score_hint);
    score_row.append(&score_copy);
    let score_spin = SpinButton::with_range(0.0, 999.0, 0.1);
    score_spin.set_digits(1);
    score_spin.set_value(settings.minimum_offline_score);
    {
        let state = state.clone();
        let stack = stack.clone();
        let score_spin_ref = score_spin.clone();
        let guard_flag = Rc::new(Cell::new(false));
        let guard_flag_closure = guard_flag.clone();
        score_spin.connect_value_changed(move |spin| {
            if guard_flag_closure.get() {
                return;
            }

            let next_value = (spin.value() * 10.0).round() / 10.0;
            let current_value = app_settings(&state).minimum_offline_score;
            if (next_value - current_value).abs() < f64::EPSILON {
                return;
            }

            let state = state.clone();
            let stack = stack.clone();
            let score_spin = score_spin_ref.clone();
            let guard_flag = guard_flag.clone();
            let stack_for_dialog = stack.clone();
            glib::spawn_future_local(async move {
                let impact = gio::spawn_blocking({
                    let state = state.clone();
                    move || preview_auto_cache_evictions(&state, |s| s.minimum_offline_score = next_value)
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(0);

                let apply_change = |guard_flag: &Rc<Cell<bool>>, score_spin: &SpinButton, value: f64| {
                    guard_flag.set(true);
                    score_spin.set_value(value);
                    guard_flag.set(false);
                };

                if impact > 0 {
                    let song_label = if impact == 1 { "song" } else { "songs" };
                    let dialog = build_settings_warning_dialog(
                        &stack_for_dialog,
                        "Raise the minimum offline score?",
                        &format!(
                            "{impact} auto-downloaded {song_label} would be evicted immediately because they would fall below the new score floor."
                        ),
                        &format!("Apply and evict {impact} {song_label}"),
                    );
                    let response = dialog.run_future().await;
                    dialog.close();
                    if response != ResponseType::Accept {
                        apply_change(&guard_flag, &score_spin, current_value);
                        return;
                    }
                }

                match gio::spawn_blocking({
                    let state = state.clone();
                    move || persist_and_sync_settings(&state, |s| s.minimum_offline_score = next_value)
                })
                .await
                {
                    Ok(Ok(())) => apply_change(&guard_flag, &score_spin, next_value),
                    _ => apply_change(&guard_flag, &score_spin, current_value),
                }
            });
        });
    }
    score_row.append(&score_spin);
    offline_section.append(&score_row);

    let storage_root_row = GtkBox::new(Orientation::Vertical, 8);
    storage_root_row.add_css_class("settings-row");
    let storage_root_label = Label::new(Some("Static storage root"));
    storage_root_label.add_css_class("settings-row-title");
    storage_root_label.set_halign(Align::Start);
    storage_root_row.append(&storage_root_label);

    let storage_root_hint = Label::new(Some(
        "Moves Static’s offline audio, artwork, and temp cache together. Leave blank to fall back to the default root.",
    ));
    storage_root_hint.add_css_class("settings-row-copy");
    storage_root_hint.set_wrap(true);
    storage_root_hint.set_halign(Align::Start);
    storage_root_row.append(&storage_root_hint);

    let storage_root_controls = GtkBox::new(Orientation::Horizontal, 8);
    let storage_root_entry = Entry::new();
    storage_root_entry.set_hexpand(true);
    storage_root_entry.set_placeholder_text(Some(&default_storage_root));
    storage_root_entry.set_text(&storage_root_override);
    storage_root_entry.add_css_class("settings-path-entry");
    storage_root_controls.append(&storage_root_entry);

    let storage_root_save_btn = Button::with_label("Move");
    storage_root_save_btn.add_css_class("settings-secondary-btn");
    storage_root_controls.append(&storage_root_save_btn);

    let storage_root_default_btn = Button::with_label("Default");
    storage_root_default_btn.add_css_class("settings-secondary-btn");
    storage_root_controls.append(&storage_root_default_btn);
    storage_root_row.append(&storage_root_controls);

    let storage_root_status = Label::new(None);
    storage_root_status.add_css_class("settings-row-copy");
    storage_root_status.set_wrap(true);
    storage_root_status.set_halign(Align::Start);
    storage_root_row.append(&storage_root_status);
    offline_section.append(&storage_root_row);

    let cache_root_value = Label::new(Some(&cache_root));
    cache_root_value.add_css_class("settings-value-path");
    cache_root_value.set_wrap(true);
    cache_root_value.set_hexpand(true);
    cache_root_value.set_halign(Align::End);

    let offline_root_value = Label::new(Some(&offline_root));
    offline_root_value.add_css_class("settings-value-path");
    offline_root_value.set_wrap(true);
    offline_root_value.set_hexpand(true);
    offline_root_value.set_halign(Align::End);

    let temp_root_value = Label::new(Some(&temp_root));
    temp_root_value.add_css_class("settings-value-path");
    temp_root_value.set_wrap(true);
    temp_root_value.set_hexpand(true);
    temp_root_value.set_halign(Align::End);

    for (label_text, value_widget) in [
        ("Cache root", cache_root_value.clone()),
        ("Offline store", offline_root_value.clone()),
        ("Temp prefetch cache", temp_root_value.clone()),
    ] {
        let row = GtkBox::new(Orientation::Horizontal, 12);
        row.add_css_class("settings-row");
        let label = Label::new(Some(label_text));
        label.add_css_class("settings-row-title");
        label.set_halign(Align::Start);
        row.append(&label);
        row.append(&value_widget);
        offline_section.append(&row);
    }

    {
        let state = state.clone();
        let storage_root_entry = storage_root_entry.clone();
        let storage_root_status = storage_root_status.clone();
        let cache_root_value = cache_root_value.clone();
        let offline_root_value = offline_root_value.clone();
        let temp_root_value = temp_root_value.clone();
        storage_root_save_btn.connect_clicked(move |_| {
            let raw = storage_root_entry.text().trim().to_string();
            match persist_storage_root_override(&state, &raw) {
                Ok(()) => {
                    let guard = state.lock().unwrap();
                    cache_root_value.set_text(&guard.cache.base_dir().display().to_string());
                    offline_root_value.set_text(&guard.cache.offline_dir().display().to_string());
                    temp_root_value.set_text(&guard.cache.temp_dir().display().to_string());
                    storage_root_status.set_text("Moved Static’s local cache into the new root.");
                }
                Err(err) => {
                    storage_root_status
                        .set_text(&format!("Couldn’t move the local cache yet: {err}"));
                }
            }
        });
    }

    {
        let state = state.clone();
        let storage_root_entry = storage_root_entry.clone();
        let storage_root_status = storage_root_status.clone();
        let cache_root_value = cache_root_value.clone();
        let offline_root_value = offline_root_value.clone();
        let temp_root_value = temp_root_value.clone();
        storage_root_default_btn.connect_clicked(move |_| {
            storage_root_entry.set_text("");
            match persist_storage_root_override(&state, "") {
                Ok(()) => {
                    let guard = state.lock().unwrap();
                    cache_root_value.set_text(&guard.cache.base_dir().display().to_string());
                    offline_root_value.set_text(&guard.cache.offline_dir().display().to_string());
                    temp_root_value.set_text(&guard.cache.temp_dir().display().to_string());
                    storage_root_status.set_text("Static is back on the default storage root.");
                }
                Err(err) => {
                    storage_root_status
                        .set_text(&format!("Couldn’t return to the default root yet: {err}"));
                }
            }
        });
    }

    sections.append(&offline_section);

    let import_section = GtkBox::new(Orientation::Vertical, 12);
    import_section.add_css_class("settings-card");
    let import_title = Label::new(Some("Chromium Cookie Import"));
    import_title.add_css_class("settings-card-title");
    import_title.set_halign(Align::Start);
    import_section.append(&import_title);

    let import_copy = Label::new(Some(
        "Auto-sync cookies from Edge or another Chromium-family browser. Leave the path blank to auto-detect.",
    ));
    import_copy.add_css_class("settings-card-copy");
    import_copy.set_wrap(true);
    import_copy.set_halign(Align::Start);
    import_section.append(&import_copy);

    let import_toggle_row = GtkBox::new(Orientation::Horizontal, 12);
    import_toggle_row.add_css_class("settings-row");
    let import_toggle_copy = GtkBox::new(Orientation::Vertical, 2);
    import_toggle_copy.set_hexpand(true);
    let import_toggle_label = Label::new(Some("Auto-sync Chromium cookies on startup"));
    import_toggle_label.add_css_class("settings-row-title");
    import_toggle_label.set_halign(Align::Start);
    import_toggle_copy.append(&import_toggle_label);
    let import_toggle_hint = Label::new(Some(
        "If a saved cookie is missing or stale, Static will try your Chromium-family browser before asking for manual cookies.",
    ));
    import_toggle_hint.add_css_class("settings-row-copy");
    import_toggle_hint.set_wrap(true);
    import_toggle_hint.set_halign(Align::Start);
    import_toggle_copy.append(&import_toggle_hint);
    import_toggle_row.append(&import_toggle_copy);
    let import_switch = settings_switch(settings.auto_sync_chromium_cookies);
    {
        let state = state.clone();
        import_switch.connect_active_notify(move |sw| {
            let active = sw.is_active();
            update_app_settings(&state, |s| s.auto_sync_chromium_cookies = active);
        });
    }
    import_toggle_row.append(&import_switch);
    import_section.append(&import_toggle_row);

    let path_row = GtkBox::new(Orientation::Vertical, 8);
    path_row.add_css_class("settings-row");
    let path_label = Label::new(Some("Chromium cookie DB path"));
    path_label.add_css_class("settings-row-title");
    path_label.set_halign(Align::Start);
    path_row.append(&path_label);

    let path_hint = Label::new(Some(
        "Examples: `~/.config/microsoft-edge/Default/Cookies`, `~/.config/google-chrome/Default/Cookies`, `~/.config/chromium/Default/Cookies`.",
    ));
    path_hint.add_css_class("settings-row-copy");
    path_hint.set_wrap(true);
    path_hint.set_halign(Align::Start);
    path_row.append(&path_hint);

    let path_controls = GtkBox::new(Orientation::Horizontal, 8);
    let path_entry = Entry::new();
    path_entry.set_hexpand(true);
    path_entry.set_placeholder_text(Some("Auto-detect Chromium browser profile"));
    path_entry.set_text(&settings.chromium_cookie_path);
    path_entry.add_css_class("settings-path-entry");
    path_controls.append(&path_entry);

    let save_path_btn = Button::with_label("Save");
    save_path_btn.add_css_class("settings-secondary-btn");
    {
        let state = state.clone();
        let path_entry = path_entry.clone();
        save_path_btn.connect_clicked(move |_| {
            let path = path_entry.text().trim().to_string();
            update_app_settings(&state, move |s| s.chromium_cookie_path = path);
        });
    }
    path_controls.append(&save_path_btn);

    let auto_path_btn = Button::with_label("Auto");
    auto_path_btn.add_css_class("settings-secondary-btn");
    {
        let state = state.clone();
        let path_entry = path_entry.clone();
        auto_path_btn.connect_clicked(move |_| {
            path_entry.set_text("");
            update_app_settings(&state, |s| s.chromium_cookie_path.clear());
        });
    }
    path_controls.append(&auto_path_btn);
    path_row.append(&path_controls);
    import_section.append(&path_row);

    sections.append(&import_section);

    let session_section = GtkBox::new(Orientation::Vertical, 12);
    session_section.add_css_class("settings-card");
    let session_title = Label::new(Some("Session"));
    session_title.add_css_class("settings-card-title");
    session_title.set_halign(Align::Start);
    session_section.append(&session_title);

    let session_copy = Label::new(Some(
        "Sign out cleanly without touching your browser’s own cookie jar.",
    ));
    session_copy.add_css_class("settings-card-copy");
    session_copy.set_wrap(true);
    session_copy.set_halign(Align::Start);
    session_section.append(&session_copy);

    let logout_btn = Button::with_label("Log out");
    logout_btn.add_css_class("settings-danger-btn");
    {
        let state = state.clone();
        let stack = stack.clone();
        let ps = ps.clone();
        let content_box = content_box.clone();
        logout_btn.connect_clicked(move |_| {
            reset_player_ui(&ps, &state);
            let mut guard = state.lock().unwrap();
            guard.client = None;
            guard.home_cache = None;
            clear_box(&content_box);

            glib::spawn_future_local({
                let stack = stack.clone();
                async move {
                    let _ = gio::spawn_blocking(delete_cookie).await;
                    stack.set_visible_child_name("onboarding");
                }
            });
        });
    }
    session_section.append(&logout_btn);
    sections.append(&session_section);

    page.append(&sections);
    content_box.append(&page);
}

pub(super) fn svg_image(svg_bytes: &[u8], size: i32) -> Image {
    let gbytes = glib::Bytes::from(svg_bytes);
    let stream = gio::MemoryInputStream::from_bytes(&gbytes);
    if let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(
        &stream,
        size,
        size,
        true,
        gio::Cancellable::NONE,
    ) {
        let texture = gtk::gdk::Texture::for_pixbuf(&pixbuf);
        let image = Image::from_paintable(Some(&texture));
        image.set_pixel_size(size);
        image
    } else {
        Image::new()
    }
}

pub(super) fn icon_button(svg_bytes: &[u8], size: i32, css_class: &str) -> Button {
    let btn = Button::new();
    btn.set_child(Some(&svg_image(svg_bytes, size)));
    btn.add_css_class(css_class);
    btn
}

pub(super) fn settings_switch(active: bool) -> Switch {
    let sw = Switch::new();
    sw.set_active(active);
    sw.set_valign(Align::Center);
    sw.set_halign(Align::End);
    sw
}

pub(super) fn set_button_icon(btn: &Button, svg_bytes: &[u8], size: i32) {
    btn.set_child(Some(&svg_image(svg_bytes, size)));
}

pub(super) fn update_repeat_button_ui(btn: &Button, mode: RepeatMode) {
    match mode {
        RepeatMode::Off => {
            set_button_icon(btn, ICON_REPEAT, 18);
            btn.remove_css_class("repeat-active");
            btn.remove_css_class("repeat-one-active");
        }
        RepeatMode::All => {
            set_button_icon(btn, ICON_REPEAT, 18);
            btn.remove_css_class("repeat-one-active");
            btn.add_css_class("repeat-active");
        }
        RepeatMode::One => {
            set_button_icon(btn, ICON_REPEAT_ONE, 18);
            btn.remove_css_class("repeat-active");
            btn.add_css_class("repeat-one-active");
        }
    }
}

pub(super) fn set_content_dimming(ps: &PlayerState, dimmed: bool) {
    ps.content_dimmer.set_visible(dimmed);
}

pub(super) fn update_rating_buttons_ui(
    like_btn: &Button,
    dislike_btn: &Button,
    rating: SongRating,
) {
    like_btn.remove_css_class("like-active");
    dislike_btn.remove_css_class("dislike-active");

    match rating {
        SongRating::Like => like_btn.add_css_class("like-active"),
        SongRating::Dislike => dislike_btn.add_css_class("dislike-active"),
        SongRating::Indifferent => {}
    }
}

pub(super) fn current_rating(ps: &PlayerState) -> SongRating {
    ps.now_playing_track
        .borrow()
        .as_ref()
        .map(|track| track.rating)
        .unwrap_or(SongRating::Indifferent)
}
