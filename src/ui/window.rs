use std::sync::{Arc, Mutex};

use gtk::prelude::*;
use gtk::{
    gio, glib, Align, Application, ApplicationWindow, Box as GtkBox, Button, Entry, Label,
    Orientation, Picture, ScrolledWindow, Separator, Stack, StackTransitionType,
};

use crate::models::{HomeSection, Track};
use crate::player::Player;
use crate::ytmusic::YtMusicClient;

const KEYRING_SERVICE: &str = "com.static.music";
const KEYRING_USER: &str = "cookie";

struct AppState {
    client: Option<YtMusicClient>,
    player: Player,
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
        .default_width(900)
        .default_height(650)
        .build();

    let stack = Stack::new();
    stack.set_transition_type(StackTransitionType::Crossfade);
    stack.set_transition_duration(300);

    // --- Shared state ---
    let state: Arc<Mutex<AppState>> = Arc::new(Mutex::new(AppState {
        client: None,
        player: Player::new(),
    }));

    // --- Build both pages ---
    let onboarding = build_onboarding_page(&stack, &state);
    let main_page = build_main_page(&stack, &state);

    stack.add_named(&onboarding, Some("onboarding"));
    stack.add_named(&main_page, Some("main"));

    // Start on a blank page while we check keyring
    stack.set_visible_child_name("onboarding");

    window.set_child(Some(&stack));
    window.present();

    // --- Check keyring on startup ---
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
                        // Stored cookie is invalid, clear it
                        let _ = gio::spawn_blocking(delete_cookie).await;
                    }
                }
            }
        });
    }
}

fn build_onboarding_page(stack: &Stack, state: &Arc<Mutex<AppState>>) -> GtkBox {
    let page = GtkBox::new(Orientation::Vertical, 0);
    page.set_valign(Align::Center);
    page.set_halign(Align::Center);
    page.set_width_request(480);

    let content = GtkBox::new(Orientation::Vertical, 16);
    content.add_css_class("onboarding-card");

    // Logo
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

    // Title
    let title = Label::new(Some("Static"));
    title.add_css_class("onboarding-title");
    content.append(&title);

    // Subtitle
    let subtitle = Label::new(Some("YouTube Music, natively."));
    subtitle.add_css_class("onboarding-subtitle");
    content.append(&subtitle);

    content.append(&Separator::new(Orientation::Horizontal));

    // Instructions
    let instructions = Label::new(Some(
        "To get started, paste your YouTube Music cookie:"
    ));
    instructions.add_css_class("onboarding-instructions");
    instructions.set_halign(Align::Start);
    instructions.set_wrap(true);
    content.append(&instructions);

    // Steps
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

    // Cookie entry
    let cookie_entry = Entry::builder()
        .placeholder_text("Paste cookie header here...")
        .hexpand(true)
        .visibility(false)
        .build();
    cookie_entry.add_css_class("onboarding-entry");
    content.append(&cookie_entry);

    // Connect button
    let connect_btn = Button::with_label("Connect");
    connect_btn.add_css_class("connect-btn");
    content.append(&connect_btn);

    // Status
    let status_label = Label::new(None);
    status_label.add_css_class("onboarding-status");
    content.append(&status_label);

    page.append(&content);

    // --- Connect handler ---
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
                            Ok(Ok(true)) => {
                                // Save to keyring
                                let cookie_for_save = cookie_text.clone();
                                let _ = gio::spawn_blocking(move || {
                                    let _ = store_cookie(&cookie_for_save);
                                }).await;
                                stack.set_visible_child_name("main");
                            }
                            Ok(Ok(false)) => {
                                // Still usable, save and proceed
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

fn build_main_page(stack: &Stack, state: &Arc<Mutex<AppState>>) -> GtkBox {
    let page = GtkBox::new(Orientation::Vertical, 0);

    // --- Header ---
    let header_box = GtkBox::new(Orientation::Horizontal, 12);
    header_box.add_css_class("header-area");

    let title_label = Label::new(Some("Static"));
    title_label.add_css_class("app-title");
    title_label.set_hexpand(true);
    title_label.set_halign(Align::Start);
    header_box.append(&title_label);

    let logout_btn = Button::with_label("Log out");
    logout_btn.add_css_class("logout-btn");
    header_box.append(&logout_btn);

    page.append(&header_box);
    page.append(&Separator::new(Orientation::Horizontal));

    // --- Search bar ---
    let search_box = GtkBox::new(Orientation::Horizontal, 8);
    search_box.set_margin_top(10);
    search_box.set_margin_bottom(10);
    search_box.set_margin_start(16);
    search_box.set_margin_end(16);

    let search_entry = Entry::builder()
        .placeholder_text("Search songs...")
        .hexpand(true)
        .build();
    let search_btn = Button::with_label("Search");
    let home_btn = Button::with_label("Home / Recommendations");

    search_box.append(&search_entry);
    search_box.append(&search_btn);
    search_box.append(&home_btn);
    page.append(&search_box);
    page.append(&Separator::new(Orientation::Horizontal));

    // --- Results area ---
    let scrolled = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .build();

    let results_box = GtkBox::new(Orientation::Vertical, 4);
    results_box.set_margin_top(10);
    results_box.set_margin_bottom(10);
    results_box.set_margin_start(16);
    results_box.set_margin_end(16);

    let welcome = Label::new(Some("Search for music or browse your recommendations."));
    welcome.add_css_class("dim-label");
    welcome.set_margin_top(40);
    results_box.append(&welcome);

    scrolled.set_child(Some(&results_box));
    page.append(&scrolled);

    // --- Now Playing bar ---
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

    // --- Logout handler ---
    {
        let state = state.clone();
        let stack = stack.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();
        let results_box = results_box.clone();

        logout_btn.connect_clicked(move |_| {
            // Stop playback
            state.lock().unwrap().player.stop();
            state.lock().unwrap().client = None;
            np_label.set_text("Nothing playing");
            stop_btn.set_sensitive(false);
            clear_results(&results_box);

            // Clear keyring
            glib::spawn_future_local({
                let stack = stack.clone();
                async move {
                    let _ = gio::spawn_blocking(delete_cookie).await;
                    stack.set_visible_child_name("onboarding");
                }
            });
        });
    }

    // --- Stop button ---
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

    // --- Search handler ---
    {
        let state = state.clone();
        let results_box = results_box.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();

        let search_entry_clone = search_entry.clone();
        let do_search = move || {
            let query = search_entry_clone.text().to_string();
            if query.is_empty() {
                return;
            }

            clear_results(&results_box);
            let loading = Label::new(Some(&format!("Searching for \"{query}\"...")));
            results_box.append(&loading);

            let state = state.clone();
            let results_box = results_box.clone();
            let query_clone = query.clone();
            let np_label = np_label.clone();
            let stop_btn = stop_btn.clone();
            glib::spawn_future_local(async move {
                let state_bg = state.clone();
                let result = gio::spawn_blocking(move || {
                    let guard = state_bg.lock().unwrap();
                    match guard.client.as_ref() {
                        Some(ytm) => Some(ytm.search(&query_clone)),
                        None => None,
                    }
                }).await;

                clear_results(&results_box);

                match result {
                    Ok(Some(Ok(tracks))) => {
                        render_search_results(&results_box, &query, &tracks, &state, &np_label, &stop_btn);
                    }
                    Ok(Some(Err(e))) => {
                        results_box.append(&Label::new(Some(&format!("Search error: {e}"))));
                    }
                    Ok(None) => {
                        results_box.append(&Label::new(Some("Not connected.")));
                    }
                    Err(e) => {
                        results_box.append(&Label::new(Some(&format!("Thread error: {e:?}"))));
                    }
                }
            });
        };

        let do_search_click = do_search.clone();
        search_btn.connect_clicked(move |_| do_search_click());
        search_entry.connect_activate(move |_| do_search());
    }

    // --- Home/Recommendations handler ---
    {
        let state = state.clone();
        let results_box = results_box.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();

        home_btn.connect_clicked(move |_| {
            clear_results(&results_box);
            let loading = Label::new(Some("Loading recommendations..."));
            results_box.append(&loading);

            let state = state.clone();
            let results_box = results_box.clone();
            let np_label = np_label.clone();
            let stop_btn = stop_btn.clone();
            glib::spawn_future_local(async move {
                let state_bg = state.clone();
                let result = gio::spawn_blocking(move || {
                    let guard = state_bg.lock().unwrap();
                    match guard.client.as_ref() {
                        Some(ytm) => Some(ytm.get_home()),
                        None => None,
                    }
                }).await;

                clear_results(&results_box);

                match result {
                    Ok(Some(Ok(sections))) => {
                        render_home_sections(&results_box, &sections, &state, &np_label, &stop_btn);
                    }
                    Ok(Some(Err(e))) => {
                        results_box.append(&Label::new(Some(&format!("Error: {e}"))));
                    }
                    Ok(None) => {
                        results_box.append(&Label::new(Some("Not connected.")));
                    }
                    Err(e) => {
                        results_box.append(&Label::new(Some(&format!("Thread error: {e:?}"))));
                    }
                }
            });
        });
    }

    page
}

fn clear_results(container: &GtkBox) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn render_search_results(
    results_box: &GtkBox,
    query: &str,
    tracks: &[Track],
    state: &Arc<Mutex<AppState>>,
    np_label: &Label,
    stop_btn: &Button,
) {
    let header = Label::new(Some(&format!(
        "Search results for \"{}\" ({} tracks)",
        query,
        tracks.len()
    )));
    header.add_css_class("section-header");
    header.set_halign(Align::Start);
    results_box.append(&header);

    for track in tracks {
        results_box.append(&build_track_row(track, state, np_label, stop_btn));
    }

    if tracks.is_empty() {
        results_box.append(&Label::new(Some("No results found.")));
    }
}

fn render_home_sections(
    results_box: &GtkBox,
    sections: &[HomeSection],
    state: &Arc<Mutex<AppState>>,
    np_label: &Label,
    stop_btn: &Button,
) {
    if sections.is_empty() {
        results_box.append(&Label::new(Some(
            "No recommendations returned. Your cookie might be expired.",
        )));
        return;
    }

    for section in sections {
        let header = Label::new(Some(&section.title));
        header.add_css_class("section-header");
        header.set_halign(Align::Start);
        results_box.append(&header);

        for track in &section.contents {
            results_box.append(&build_track_row(track, state, np_label, stop_btn));
        }

        results_box.append(&Separator::new(Orientation::Horizontal));
    }
}

fn build_track_row(
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    np_label: &Label,
    stop_btn: &Button,
) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 12);
    row.add_css_class("track-card");

    let play_btn = Button::with_label("Play");
    play_btn.add_css_class("play-btn");
    play_btn.set_valign(Align::Center);
    row.append(&play_btn);

    let info_box = GtkBox::new(Orientation::Vertical, 2);
    info_box.set_hexpand(true);

    let title_label = Label::new(Some(&track.title));
    title_label.set_halign(Align::Start);
    title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title_label.add_css_class("track-title");
    info_box.append(&title_label);

    let artist_str = if track.artists.is_empty() {
        "Unknown artist".to_string()
    } else {
        track
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let artist_label = Label::new(Some(&artist_str));
    artist_label.set_halign(Align::Start);
    artist_label.add_css_class("track-artist");
    artist_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    info_box.append(&artist_label);

    row.append(&info_box);

    if let Some(duration) = &track.duration {
        let dur_label = Label::new(Some(duration));
        dur_label.add_css_class("track-duration");
        dur_label.set_valign(Align::Center);
        row.append(&dur_label);
    }

    let video_id = track.video_id.clone();
    let display_title = format!("{} - {}", track.title, artist_str);
    {
        let state = state.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();

        play_btn.connect_clicked(move |btn| {
            btn.set_label("...");
            btn.set_sensitive(false);
            np_label.set_text(&format!("Loading: {display_title}"));

            let state = state.clone();
            let video_id = video_id.clone();
            let display_title = display_title.clone();
            let np_label = np_label.clone();
            let stop_btn = stop_btn.clone();
            let btn = btn.clone();

            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || {
                    let guard = state.lock().unwrap();
                    let url = guard.client.as_ref().unwrap().get_audio_url(&video_id)?;
                    guard.player.play(&url)?;
                    Ok::<_, anyhow::Error>(())
                }).await;

                btn.set_label("Play");
                btn.set_sensitive(true);

                match result {
                    Ok(Ok(())) => {
                        np_label.set_text(&format!("Now playing: {display_title}"));
                        stop_btn.set_sensitive(true);
                    }
                    Ok(Err(e)) => {
                        np_label.set_text(&format!("Playback failed: {e}"));
                    }
                    Err(e) => {
                        np_label.set_text(&format!("Thread error: {e:?}"));
                    }
                }
            });
        });
    }

    row
}
