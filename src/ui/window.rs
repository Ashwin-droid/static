use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};

use gtk::prelude::*;
use gtk::{
    gio, glib, Align, Application, ApplicationWindow, Box as GtkBox, Button, Entry, Label,
    Orientation, Picture, Popover, ScrolledWindow, Separator, Stack, StackTransitionType,
};

use crate::models::{HomeSection, Track};
use crate::player::Player;
use crate::ytmusic::YtMusicClient;

const KEYRING_SERVICE: &str = "com.static.music";
const KEYRING_USER: &str = "cookie";

static THUMB_CLIENT: LazyLock<reqwest::blocking::Client> = LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap()
});

struct AppState {
    client: Option<YtMusicClient>,
    player: Player,
}

struct MainPageWidgets {
    page: GtkBox,
    content_box: GtkBox,
    np_label: Label,
    stop_btn: Button,
    recent_list: GtkBox,
}

fn store_cookie(cookie: &str) -> Result<(), keyring::Error> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
    entry.set_password(cookie)
}

fn get_cookie() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()?;
    entry.get_password().ok()
}

fn delete_cookie() {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
        let _ = entry.delete_credential();
    }
}

pub fn build_window(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Static")
        .default_width(1100)
        .default_height(700)
        .build();

    let stack = Stack::new();
    stack.set_transition_type(StackTransitionType::Crossfade);
    stack.set_transition_duration(300);

    let state: Arc<Mutex<AppState>> = Arc::new(Mutex::new(AppState {
        client: None,
        player: Player::new(),
    }));

    let onboarding = build_onboarding_page(&stack, &state);
    let main_widgets = build_main_page(&stack, &state);

    stack.add_named(&onboarding, Some("onboarding"));
    stack.add_named(&main_widgets.page, Some("main"));
    stack.set_visible_child_name("onboarding");

    window.set_child(Some(&stack));
    window.present();

    // Auto-load home + history when main page becomes visible
    {
        let state = state.clone();
        let content_box = main_widgets.content_box.clone();
        let np_label = main_widgets.np_label.clone();
        let stop_btn = main_widgets.stop_btn.clone();
        let recent_list = main_widgets.recent_list.clone();
        let loaded = Rc::new(RefCell::new(false));

        stack.connect_notify_local(Some("visible-child-name"), move |stack, _| {
            if stack.visible_child_name().as_deref() == Some("main") && !*loaded.borrow() {
                *loaded.borrow_mut() = true;
                load_home(&state, &content_box, &np_label, &stop_btn, &recent_list);
                load_sidebar_history(&state, &recent_list, &np_label, &stop_btn);
            }
        });
    }

    // Check keyring on startup
    {
        let stack = stack.clone();
        let state = state.clone();
        glib::spawn_future_local(async move {
            let cookie = gio::spawn_blocking(get_cookie).await;
            if let Ok(Some(cookie)) = cookie {
                match YtMusicClient::new(&cookie) {
                    Ok(ytm) => {
                        state.lock().unwrap().client = Some(ytm);
                        stack.set_visible_child_name("main");
                    }
                    Err(_) => {
                        let _ = gio::spawn_blocking(delete_cookie).await;
                    }
                }
            }
        });
    }
}

// ============================================================
// ONBOARDING PAGE
// ============================================================

fn build_onboarding_page(stack: &Stack, state: &Arc<Mutex<AppState>>) -> GtkBox {
    let page = GtkBox::new(Orientation::Vertical, 0);
    page.set_valign(Align::Center);
    page.set_halign(Align::Center);
    page.set_width_request(480);

    let content = GtkBox::new(Orientation::Vertical, 16);
    content.add_css_class("onboarding-card");

    let logo_bytes = glib::Bytes::from_static(include_bytes!("../../assets/static.png"));
    let stream = gio::MemoryInputStream::from_bytes(&logo_bytes);
    if let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(&stream, 96, 96, true, gio::Cancellable::NONE) {
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
         5. Copy the \u{2018}Cookie\u{2019} header value"
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
                    state.lock().unwrap().client = Some(ytm);
                    let state = state.clone();
                    let stack = stack.clone();
                    let status_label = status_label.clone();
                    let connect_btn = connect_btn_inner.clone();
                    let cookie_text = cookie_text.clone();

                    glib::spawn_future_local(async move {
                        let state_bg = state.clone();
                        let result = gio::spawn_blocking(move || {
                            let guard = state_bg.lock().unwrap();
                            guard.client.as_ref().unwrap().check_auth()
                        }).await;

                        match result {
                            Ok(Ok(true)) | Ok(Ok(false)) => {
                                let cookie_for_save = cookie_text.clone();
                                let _ = gio::spawn_blocking(move || {
                                    let _ = store_cookie(&cookie_for_save);
                                }).await;
                                stack.set_visible_child_name("main");
                            }
                            Ok(Err(e)) => {
                                status_label.set_text(&format!("Auth check failed: {e}"));
                                connect_btn.set_sensitive(true);
                                connect_btn.set_label("Connect");
                            }
                            Err(e) => {
                                status_label.set_text(&format!("Error: {e:?}"));
                                connect_btn.set_sensitive(true);
                                connect_btn.set_label("Connect");
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
// MAIN PAGE
// ============================================================

fn build_main_page(stack: &Stack, state: &Arc<Mutex<AppState>>) -> MainPageWidgets {
    let page = GtkBox::new(Orientation::Vertical, 0);

    let hbox = GtkBox::new(Orientation::Horizontal, 0);
    hbox.set_vexpand(true);

    // ======= SIDEBAR (glass) =======
    let sidebar = GtkBox::new(Orientation::Vertical, 0);
    sidebar.add_css_class("sidebar");
    sidebar.set_width_request(240);
    sidebar.set_hexpand(false);

    // Search
    let search_entry = Entry::builder()
        .placeholder_text("Search songs...")
        .build();
    search_entry.add_css_class("sidebar-search");
    sidebar.append(&search_entry);

    // Nav items
    let nav_box = GtkBox::new(Orientation::Vertical, 2);
    nav_box.add_css_class("nav-section");

    let home_btn = nav_button("Home", true);
    nav_box.append(&home_btn);

    for label in ["History", "Likes", "Playlists", "Offline"] {
        let btn = nav_button(label, false);
        btn.set_sensitive(false);
        nav_box.append(&btn);
    }

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
    recent_scroll.set_child(Some(&recent_list));
    sidebar.append(&recent_scroll);

    // Bottom: Settings with popover containing Logout
    sidebar.append(&Separator::new(Orientation::Horizontal));
    let bottom_box = GtkBox::new(Orientation::Horizontal, 8);
    bottom_box.add_css_class("sidebar-bottom");

    let settings_btn = Button::with_label("Settings");
    settings_btn.add_css_class("nav-item");
    settings_btn.set_hexpand(true);

    let settings_popover = Popover::new();
    settings_popover.set_parent(&settings_btn);
    settings_popover.set_position(gtk::PositionType::Top);

    let pop_content = GtkBox::new(Orientation::Vertical, 4);
    pop_content.set_margin_top(4);
    pop_content.set_margin_bottom(4);
    pop_content.set_margin_start(4);
    pop_content.set_margin_end(4);

    let logout_btn = Button::with_label("Log out");
    logout_btn.add_css_class("popover-logout-btn");
    pop_content.append(&logout_btn);

    settings_popover.set_child(Some(&pop_content));

    {
        let popover = settings_popover.clone();
        settings_btn.connect_clicked(move |_| {
            popover.popup();
        });
    }

    bottom_box.append(&settings_btn);
    sidebar.append(&bottom_box);
    hbox.append(&sidebar);

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

    let content_box = GtkBox::new(Orientation::Vertical, 20);
    content_box.set_margin_top(24);
    content_box.set_margin_bottom(24);
    content_box.set_margin_start(28);
    content_box.set_margin_end(28);

    scrolled.set_child(Some(&content_box));
    content_outer.append(&scrolled);
    hbox.append(&content_outer);
    page.append(&hbox);

    // ======= NOW PLAYING BAR =======
    let np_bar = GtkBox::new(Orientation::Horizontal, 12);
    np_bar.add_css_class("now-playing-bar");

    let np_label = Label::new(Some("Nothing playing"));
    np_label.add_css_class("now-playing-text");
    np_label.set_hexpand(true);
    np_label.set_halign(Align::Start);
    np_label.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let stop_btn = Button::with_label("Stop");
    stop_btn.add_css_class("stop-btn");
    stop_btn.set_sensitive(false);

    np_bar.append(&np_label);
    np_bar.append(&stop_btn);
    page.append(&np_bar);

    // ======= WIRE SIGNALS =======

    // Logout (inside settings popover)
    {
        let state = state.clone();
        let stack = stack.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();
        let content_box = content_box.clone();
        let settings_popover = settings_popover.clone();

        logout_btn.connect_clicked(move |_| {
            settings_popover.popdown();
            state.lock().unwrap().player.stop();
            state.lock().unwrap().client = None;
            np_label.set_text("Nothing playing");
            stop_btn.set_sensitive(false);
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

    // Stop
    {
        let state = state.clone();
        let np_label = np_label.clone();
        let stop_btn_inner = stop_btn.clone();
        stop_btn.connect_clicked(move |_| {
            state.lock().unwrap().player.stop();
            np_label.set_text("Nothing playing");
            stop_btn_inner.set_sensitive(false);
        });
    }

    // Home button
    {
        let state = state.clone();
        let content_box = content_box.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();
        let recent_list = recent_list.clone();

        home_btn.connect_clicked(move |_| {
            load_home(&state, &content_box, &np_label, &stop_btn, &recent_list);
        });
    }

    // Search
    {
        let state = state.clone();
        let content_box = content_box.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();
        let recent_list = recent_list.clone();

        search_entry.connect_activate(move |entry| {
            let query = entry.text().to_string();
            if query.is_empty() {
                return;
            }
            load_search(&state, &content_box, &query, &np_label, &stop_btn, &recent_list);
        });
    }

    MainPageWidgets {
        page,
        content_box,
        np_label,
        stop_btn,
        recent_list,
    }
}

fn nav_button(text: &str, active: bool) -> Button {
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

// ============================================================
// DATA LOADING
// ============================================================

fn load_home(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    np_label: &Label,
    stop_btn: &Button,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some("Loading recommendations..."));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let np_label = np_label.clone();
    let stop_btn = stop_btn.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result = gio::spawn_blocking(move || {
            let guard = state_bg.lock().unwrap();
            match guard.client.as_ref() {
                Some(ytm) => Some(ytm.get_home()),
                None => None,
            }
        }).await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(sections))) => {
                render_home_view(&content_box, &sections, &state, &np_label, &stop_btn, &recent_list);
            }
            Ok(Some(Err(e))) => {
                content_box.append(&Label::new(Some(&format!("Error: {e}"))));
            }
            Ok(None) => {
                content_box.append(&Label::new(Some("Not connected.")));
            }
            Err(e) => {
                content_box.append(&Label::new(Some(&format!("Error: {e:?}"))));
            }
        }
    });
}

fn load_search(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    query: &str,
    np_label: &Label,
    stop_btn: &Button,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some(&format!("Searching \"{query}\"...")));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let query = query.to_string();
    let np_label = np_label.clone();
    let stop_btn = stop_btn.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let q = query.clone();
        let result = gio::spawn_blocking(move || {
            let guard = state_bg.lock().unwrap();
            match guard.client.as_ref() {
                Some(ytm) => Some(ytm.search(&q)),
                None => None,
            }
        }).await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(tracks))) => {
                render_search_view(&content_box, &query, &tracks, &state, &np_label, &stop_btn, &recent_list);
            }
            Ok(Some(Err(e))) => {
                content_box.append(&Label::new(Some(&format!("Search error: {e}"))));
            }
            Ok(None) => {
                content_box.append(&Label::new(Some("Not connected.")));
            }
            Err(e) => {
                content_box.append(&Label::new(Some(&format!("Error: {e:?}"))));
            }
        }
    });
}

fn load_sidebar_history(
    state: &Arc<Mutex<AppState>>,
    recent_list: &GtkBox,
    np_label: &Label,
    stop_btn: &Button,
) {
    let state = state.clone();
    let recent_list = recent_list.clone();
    let np_label = np_label.clone();
    let stop_btn = stop_btn.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result = gio::spawn_blocking(move || {
            let guard = state_bg.lock().unwrap();
            guard.client.as_ref().map(|ytm| ytm.get_history())
        }).await;

        if let Ok(Some(Ok(tracks))) = result {
            clear_box(&recent_list);
            for track in tracks.iter().take(15) {
                let item = build_sidebar_recent_item(track, &state, &np_label, &stop_btn);
                recent_list.append(&item);
            }
            if tracks.is_empty() {
                let msg = Label::new(Some("No history yet"));
                msg.add_css_class("sidebar-dim");
                recent_list.append(&msg);
            }
        } else {
            clear_box(&recent_list);
            let msg = Label::new(Some("Could not load history"));
            msg.add_css_class("sidebar-dim");
            recent_list.append(&msg);
        }
    });
}

// ============================================================
// RENDERING
// ============================================================

fn render_home_view(
    content_box: &GtkBox,
    sections: &[HomeSection],
    state: &Arc<Mutex<AppState>>,
    np_label: &Label,
    stop_btn: &Button,
    recent_list: &GtkBox,
) {
    if sections.is_empty() {
        let msg = Label::new(Some("No recommendations returned. Your cookie might be expired."));
        msg.add_css_class("dim-label");
        msg.set_margin_top(60);
        content_box.append(&msg);
        return;
    }

    // First section → horizontal cover art cards
    if let Some(first) = sections.first() {
        let header = Label::new(Some(&first.title));
        header.add_css_class("section-header");
        header.set_halign(Align::Start);
        content_box.append(&header);

        let cards_scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .build();
        cards_scroll.add_css_class("cards-scroll");

        let cards_box = GtkBox::new(Orientation::Horizontal, 14);
        cards_box.set_margin_top(4);
        cards_box.set_margin_bottom(8);

        for track in &first.contents {
            cards_box.append(&build_cover_card(track, state, np_label, stop_btn, recent_list));
        }

        cards_scroll.set_child(Some(&cards_box));
        content_box.append(&cards_scroll);
    }

    // Remaining sections → 2-column grid
    for section in sections.iter().skip(1) {
        content_box.append(&Separator::new(Orientation::Horizontal));

        let header = Label::new(Some(&section.title));
        header.add_css_class("section-header");
        header.set_halign(Align::Start);
        header.set_margin_top(8);
        content_box.append(&header);

        let grid = gtk::Grid::new();
        grid.set_column_spacing(12);
        grid.set_row_spacing(8);
        grid.set_column_homogeneous(true);
        grid.set_margin_top(4);

        for (i, track) in section.contents.iter().enumerate() {
            let row = build_track_row(track, state, np_label, stop_btn, recent_list);
            grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
        }

        content_box.append(&grid);
    }
}

fn render_search_view(
    content_box: &GtkBox,
    query: &str,
    tracks: &[Track],
    state: &Arc<Mutex<AppState>>,
    np_label: &Label,
    stop_btn: &Button,
    recent_list: &GtkBox,
) {
    let header = Label::new(Some(&format!(
        "Results for \"{}\" \u{2014} {} tracks",
        query,
        tracks.len()
    )));
    header.add_css_class("section-header");
    header.set_halign(Align::Start);
    content_box.append(&header);

    if tracks.is_empty() {
        let msg = Label::new(Some("No results found."));
        msg.add_css_class("dim-label");
        msg.set_margin_top(20);
        content_box.append(&msg);
        return;
    }

    let grid = gtk::Grid::new();
    grid.set_column_spacing(12);
    grid.set_row_spacing(8);
    grid.set_column_homogeneous(true);
    grid.set_margin_top(8);

    for (i, track) in tracks.iter().enumerate() {
        let row = build_track_row(track, state, np_label, stop_btn, recent_list);
        grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
    }

    content_box.append(&grid);
}

// ============================================================
// WIDGETS
// ============================================================

fn build_cover_card(
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    np_label: &Label,
    stop_btn: &Button,
    recent_list: &GtkBox,
) -> Button {
    let card_btn = Button::new();
    card_btn.add_css_class("cover-card");

    let card = GtkBox::new(Orientation::Vertical, 6);
    card.set_size_request(150, -1);

    // Cover art
    let art_container = GtkBox::new(Orientation::Vertical, 0);
    art_container.add_css_class("cover-art");
    art_container.set_size_request(150, 150);
    art_container.set_overflow(gtk::Overflow::Hidden);

    if let Some(url) = &track.thumbnail_url {
        let picture = Picture::new();
        picture.set_keep_aspect_ratio(false);
        picture.set_size_request(150, 150);
        art_container.append(&picture);
        load_thumbnail_into(&picture, url, 300);
    } else {
        let initial = track.title.chars().next().unwrap_or('\u{266B}').to_uppercase().to_string();
        let letter = Label::new(Some(&initial));
        letter.add_css_class("cover-art-letter");
        letter.set_vexpand(true);
        letter.set_valign(Align::Center);
        art_container.append(&letter);
    }

    card.append(&art_container);

    let name = Label::new(Some(&track.title));
    name.add_css_class("cover-card-title");
    name.set_halign(Align::Start);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_max_width_chars(18);
    card.append(&name);

    let artist_str = artist_string(track);
    let artist = Label::new(Some(&artist_str));
    artist.add_css_class("cover-card-artist");
    artist.set_halign(Align::Start);
    artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
    artist.set_max_width_chars(18);
    card.append(&artist);

    card_btn.set_child(Some(&card));

    {
        let state = state.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();
        let recent_list = recent_list.clone();
        let track = track.clone();
        let video_id = track.video_id.clone();
        let display = format!("{} - {}", track.title, artist_str);

        card_btn.connect_clicked(move |_| {
            play_track(&state, &video_id, &display, &np_label, &stop_btn);
            add_to_recent(&recent_list, &track, &state, &np_label, &stop_btn);
        });
    }

    card_btn
}

fn build_track_row(
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    np_label: &Label,
    stop_btn: &Button,
    recent_list: &GtkBox,
) -> Button {
    let row_btn = Button::new();
    row_btn.add_css_class("track-row");

    let row = GtkBox::new(Orientation::Horizontal, 10);

    // Small thumbnail
    let art_container = GtkBox::new(Orientation::Vertical, 0);
    art_container.add_css_class("track-art");
    art_container.set_size_request(44, 44);
    art_container.set_valign(Align::Center);
    art_container.set_overflow(gtk::Overflow::Hidden);

    if let Some(url) = &track.thumbnail_url {
        let picture = Picture::new();
        picture.set_keep_aspect_ratio(false);
        picture.set_size_request(44, 44);
        art_container.append(&picture);
        load_thumbnail_into(&picture, url, 88);
    } else {
        let initial = track.title.chars().next().unwrap_or('\u{266B}').to_uppercase().to_string();
        let letter = Label::new(Some(&initial));
        letter.add_css_class("track-art-letter");
        letter.set_vexpand(true);
        letter.set_valign(Align::Center);
        letter.set_halign(Align::Center);
        art_container.append(&letter);
    }
    row.append(&art_container);

    let info = GtkBox::new(Orientation::Vertical, 1);
    info.set_hexpand(true);
    info.set_valign(Align::Center);

    let title_label = Label::new(Some(&track.title));
    title_label.set_halign(Align::Start);
    title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title_label.add_css_class("track-title");
    info.append(&title_label);

    let artist_str = artist_string(track);
    let artist_label = Label::new(Some(&artist_str));
    artist_label.set_halign(Align::Start);
    artist_label.add_css_class("track-artist");
    artist_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    info.append(&artist_label);

    row.append(&info);

    if let Some(duration) = &track.duration {
        let dur = Label::new(Some(duration));
        dur.add_css_class("track-duration");
        dur.set_valign(Align::Center);
        row.append(&dur);
    }

    row_btn.set_child(Some(&row));

    {
        let state = state.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();
        let recent_list = recent_list.clone();
        let track = track.clone();
        let video_id = track.video_id.clone();
        let display = format!("{} - {}", track.title, artist_str);

        row_btn.connect_clicked(move |_| {
            play_track(&state, &video_id, &display, &np_label, &stop_btn);
            add_to_recent(&recent_list, &track, &state, &np_label, &stop_btn);
        });
    }

    row_btn
}

fn build_sidebar_recent_item(
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    np_label: &Label,
    stop_btn: &Button,
) -> Button {
    let btn = Button::new();
    btn.add_css_class("sidebar-recent-item");

    let row = GtkBox::new(Orientation::Vertical, 1);

    let title = Label::new(Some(&track.title));
    title.set_halign(Align::Start);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.add_css_class("sidebar-recent-title");
    row.append(&title);

    let artist_str = artist_string(track);
    let artist = Label::new(Some(&artist_str));
    artist.set_halign(Align::Start);
    artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
    artist.add_css_class("sidebar-recent-artist");
    row.append(&artist);

    btn.set_child(Some(&row));

    {
        let state = state.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();
        let video_id = track.video_id.clone();
        let display = format!("{} - {}", track.title, artist_str);

        btn.connect_clicked(move |_| {
            play_track(&state, &video_id, &display, &np_label, &stop_btn);
        });
    }

    btn
}

// ============================================================
// HELPERS
// ============================================================

fn load_thumbnail_into(picture: &Picture, url: &str, size: i32) {
    let picture = picture.clone();
    let url = url.to_string();

    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || {
            let bytes = THUMB_CLIENT.get(&url).send()?.bytes()?;
            Ok::<_, reqwest::Error>(bytes.to_vec())
        }).await;

        if let Ok(Ok(bytes)) = result {
            let gbytes = glib::Bytes::from(&bytes);
            let stream = gio::MemoryInputStream::from_bytes(&gbytes);
            if let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(
                &stream, size, size, true, gio::Cancellable::NONE,
            ) {
                let texture = gtk::gdk::Texture::for_pixbuf(&pixbuf);
                picture.set_paintable(Some(&texture));
            }
        }
    });
}

fn play_track(
    state: &Arc<Mutex<AppState>>,
    video_id: &str,
    display_title: &str,
    np_label: &Label,
    stop_btn: &Button,
) {
    np_label.set_text(&format!("Loading: {display_title}"));

    let state = state.clone();
    let state_play = state.clone();
    let video_id = video_id.to_string();
    let display_title = display_title.to_string();
    let np_label = np_label.clone();
    let stop_btn = stop_btn.clone();

    glib::spawn_future_local(async move {
        // Fetch URL in blocking thread (network I/O)
        let url_result = gio::spawn_blocking(move || {
            let guard = state.lock().unwrap();
            guard.client.as_ref().unwrap().get_audio_url(&video_id)
        }).await;

        // Play on main thread so PR_SET_PDEATHSIG tracks the long-lived main thread
        match url_result {
            Ok(Ok(url)) => {
                match state_play.lock().unwrap().player.play(&url) {
                    Ok(()) => {
                        np_label.set_text(&format!("Now playing: {display_title}"));
                        stop_btn.set_sensitive(true);
                    }
                    Err(e) => {
                        np_label.set_text(&format!("Playback failed: {e}"));
                    }
                }
            }
            Ok(Err(e)) => {
                np_label.set_text(&format!("Playback failed: {e}"));
            }
            Err(e) => {
                np_label.set_text(&format!("Error: {e:?}"));
            }
        }
    });
}

fn add_to_recent(
    recent_list: &GtkBox,
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    np_label: &Label,
    stop_btn: &Button,
) {
    let item = build_sidebar_recent_item(track, state, np_label, stop_btn);
    recent_list.prepend(&item);

    // Limit to 15 items
    let mut count = 0;
    let mut child = recent_list.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        count += 1;
        if count > 15 {
            recent_list.remove(&c);
        }
        child = next;
    }
}

fn artist_string(track: &Track) -> String {
    if track.artists.is_empty() {
        "Unknown artist".to_string()
    } else {
        track.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
    }
}

fn clear_box(container: &GtkBox) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}
