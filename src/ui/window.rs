use std::sync::{Arc, Mutex};

use gtk::prelude::*;
use gtk::{
    gio, glib, Align, Application, ApplicationWindow, Box as GtkBox, Button, Entry, Label,
    Orientation, ScrolledWindow, Separator,
};

use crate::models::{HomeSection, Track};
use crate::player::Player;
use crate::ytmusic::YtMusicClient;

struct AppState {
    client: Option<YtMusicClient>,
    player: Player,
}

pub fn build_window(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Static")
        .default_width(900)
        .default_height(650)
        .build();

    let main_box = GtkBox::new(Orientation::Vertical, 0);

    // --- Header / Cookie input ---
    let header_box = GtkBox::new(Orientation::Vertical, 8);
    header_box.add_css_class("header-area");

    let title_label = Label::new(Some("Static"));
    title_label.add_css_class("app-title");
    header_box.append(&title_label);

    let cookie_box = GtkBox::new(Orientation::Horizontal, 8);
    let cookie_entry = Entry::builder()
        .placeholder_text("Paste your music.youtube.com cookie header here...")
        .hexpand(true)
        .visibility(false)
        .build();
    let connect_btn = Button::with_label("Connect");
    cookie_box.append(&cookie_entry);
    cookie_box.append(&connect_btn);
    header_box.append(&cookie_box);

    let status_label = Label::new(Some("Not connected"));
    status_label.set_halign(Align::Start);
    status_label.add_css_class("status-label");
    header_box.append(&status_label);

    main_box.append(&header_box);
    main_box.append(&Separator::new(Orientation::Horizontal));

    // --- Search bar ---
    let search_box = GtkBox::new(Orientation::Horizontal, 8);
    search_box.set_margin_top(10);
    search_box.set_margin_bottom(10);
    search_box.set_margin_start(16);
    search_box.set_margin_end(16);

    let search_entry = Entry::builder()
        .placeholder_text("Search songs...")
        .hexpand(true)
        .sensitive(false)
        .build();
    let search_btn = Button::with_label("Search");
    search_btn.set_sensitive(false);
    let home_btn = Button::with_label("Home / Recommendations");
    home_btn.set_sensitive(false);

    search_box.append(&search_entry);
    search_box.append(&search_btn);
    search_box.append(&home_btn);
    main_box.append(&search_box);
    main_box.append(&Separator::new(Orientation::Horizontal));

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

    let placeholder = Label::new(Some("Connect with your cookie to get started.\n\nHow to get your cookie:\n1. Open music.youtube.com in your browser\n2. Open DevTools (F12) -> Network tab\n3. Reload the page\n4. Click any request to music.youtube.com\n5. Copy the 'Cookie' header value from Request Headers"));
    placeholder.set_wrap(true);
    placeholder.set_margin_top(40);
    placeholder.add_css_class("dim-label");
    results_box.append(&placeholder);

    scrolled.set_child(Some(&results_box));
    main_box.append(&scrolled);

    // --- Now Playing bar (bottom) ---
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
    main_box.append(&np_bar);

    window.set_child(Some(&main_box));

    // --- Shared state ---
    let state: Arc<Mutex<AppState>> = Arc::new(Mutex::new(AppState {
        client: None,
        player: Player::new(),
    }));

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

    // --- Connect button handler ---
    {
        let state = state.clone();
        let status_label = status_label.clone();
        let search_entry = search_entry.clone();
        let search_btn = search_btn.clone();
        let home_btn = home_btn.clone();
        let cookie_entry = cookie_entry.clone();

        connect_btn.connect_clicked(move |_| {
            let cookie_text = cookie_entry.text().to_string();
            if cookie_text.is_empty() {
                status_label.set_text("Please paste a cookie first");
                return;
            }

            match YtMusicClient::new(&cookie_text) {
                Ok(ytm) => {
                    state.lock().unwrap().client = Some(ytm);
                    status_label.set_text("Connected! Checking auth...");
                    search_entry.set_sensitive(true);
                    search_btn.set_sensitive(true);
                    home_btn.set_sensitive(true);

                    let state = state.clone();
                    let status_label = status_label.clone();
                    glib::spawn_future_local(async move {
                        let result = gio::spawn_blocking(move || {
                            let guard = state.lock().unwrap();
                            guard.client.as_ref().unwrap().check_auth()
                        }).await;

                        match result {
                            Ok(Ok(true)) => status_label.set_text("Authenticated successfully!"),
                            Ok(Ok(false)) => status_label.set_text("Connected (not signed in - recommendations may be limited)"),
                            Ok(Err(e)) => status_label.set_text(&format!("Auth check failed: {e}")),
                            Err(e) => status_label.set_text(&format!("Thread error: {e:?}")),
                        }
                    });
                }
                Err(e) => {
                    status_label.set_text(&format!("Invalid cookie: {e}"));
                }
            }
        });
    }

    // --- Search handler ---
    {
        let state = state.clone();
        let results_box = results_box.clone();
        let search_entry = search_entry.clone();
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

    window.present();
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

    // Play button
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

    // Wire up play button
    let video_id = track.video_id.clone();
    let display_title = format!("{} - {}", track.title, artist_str);
    {
        let state = state.clone();
        let np_label = np_label.clone();
        let stop_btn = stop_btn.clone();

        play_btn.connect_clicked(move |btn| {
            eprintln!("[PLAY] clicked: {video_id}");
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
                eprintln!("[PLAY] fetching audio URL for {video_id}...");
                let result = gio::spawn_blocking(move || {
                    eprintln!("[PLAY] inside spawn_blocking, getting URL...");
                    let guard = state.lock().unwrap();
                    let url = guard.client.as_ref().unwrap().get_audio_url(&video_id)?;
                    eprintln!("[PLAY] got URL, launching ffplay...");
                    guard.player.play(&url)?;
                    eprintln!("[PLAY] ffplay launched!");
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
