use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use gstreamer::ClockTime;
use gtk::prelude::*;
use gtk::{
    gio, glib, Align, Application, ApplicationWindow, Box as GtkBox, Button, Entry, Image, Label,
    Orientation, Paned, Picture, Popover, Scale, ScrolledWindow, Separator, Stack,
    StackTransitionType,
};

const ICON_PREV: &[u8] = include_bytes!("../../assets/icons/prev.svg");
const ICON_PLAY: &[u8] = include_bytes!("../../assets/icons/play.svg");
const ICON_PAUSE: &[u8] = include_bytes!("../../assets/icons/pause.svg");
const ICON_NEXT: &[u8] = include_bytes!("../../assets/icons/next.svg");
const ICON_LIKE: &[u8] = include_bytes!("../../assets/icons/like.svg");
const ICON_DISLIKE: &[u8] = include_bytes!("../../assets/icons/dislike.svg");
const ICON_MENU: &[u8] = include_bytes!("../../assets/icons/menu.svg");
const ICON_VOLUME: &[u8] = include_bytes!("../../assets/icons/volume.svg");
const ICON_REPEAT: &[u8] = include_bytes!("../../assets/icons/repeat.svg");
const ICON_SHUFFLE: &[u8] = include_bytes!("../../assets/icons/shuffle.svg");
const ICON_DOWNLOAD: &[u8] = include_bytes!("../../assets/icons/download.svg");

const SIDEBAR_WIDTH: i32 = 240;
const SIDEBAR_RECENT_CONTENT_WIDTH: i32 = SIDEBAR_WIDTH - 32;

use crate::cache::MusicCache;
use crate::models::{Artist, HomeSection, PlaylistSummary, RadioChip, Track};
use crate::player::Player;
use crate::ytmusic::client::TrackingInfo;
use crate::ytmusic::YtMusicClient;

const KEYRING_SERVICE: &str = "com.static.music";
const KEYRING_USER: &str = "cookie";
const KEYRING_VR_TOKEN: &str = "vr_refresh_token";

static THUMB_CLIENT: LazyLock<reqwest::blocking::Client> = LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap()
});

/// Separate client for downloading full audio files — longer timeout and proper user-agent.
static DOWNLOAD_CLIENT: LazyLock<reqwest::blocking::Client> = LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:136.0) Gecko/20100101 Firefox/136.0")
        .build()
        .unwrap()
});

struct AppState {
    client: Option<Arc<YtMusicClient>>,
    cache: MusicCache,
}

/// Tracks the currently playing song so we can record listen time when navigating away.
struct NowPlayingTrack {
    session_id: u64,
    video_id: String,
    title: String,
    artist: String,
    thumb_url: Option<String>,
    started_at: Instant,
    tracking: Option<TrackingInfo>,
}

struct PreparedPlayback {
    uri: String,
}

/// Minimum seconds listened before a play is recorded.
const MIN_LISTEN_SECS: u64 = 10;

fn app_client(state: &Arc<Mutex<AppState>>) -> Option<Arc<YtMusicClient>> {
    state.lock().unwrap().client.clone()
}

#[allow(dead_code)]
struct PlayerState {
    player: Player,
    // Progress
    progress_slider: Scale,
    is_seeking: Cell<bool>,
    // Transport
    prev_btn: Button,
    play_pause_btn: Button,
    next_btn: Button,
    // Time
    time_label: Label,
    // Track info
    np_thumb: Picture,
    np_title: Label,
    np_artist: Label,
    // Actions
    dislike_btn: Button,
    like_btn: Button,
    menu_btn: Button,
    // Volume
    volume_slider: Scale,
    // Modes
    repeat_btn: Button,
    shuffle_btn: Button,
    // Queue / Radio
    queue: RefCell<Vec<Track>>,
    queue_index: Cell<usize>,
    chips: RefCell<Vec<RadioChip>>,
    seed_video_id: RefCell<String>,
    // Queue panel widgets
    queue_list_box: GtkBox,
    chip_bar: GtkBox,
    queue_panel: GtkBox,
    // Play tracking — records listen time when navigating away
    now_playing_track: RefCell<Option<NowPlayingTrack>>,
    playback_session_id: Cell<u64>,
}

struct MainPageWidgets {
    page: GtkBox,
    content_box: GtkBox,
    player_state: Rc<PlayerState>,
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

fn store_vr_refresh_token(token: &str) -> Result<(), keyring::Error> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_VR_TOKEN)?;
    entry.set_password(token)
}

fn get_vr_refresh_token() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_VR_TOKEN).ok()?;
    entry.get_password().ok()
}

pub fn build_window(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Static")
        .default_width(1532)
        .default_height(760)
        .build();


    let stack = Stack::new();
    stack.set_transition_type(StackTransitionType::Crossfade);
    stack.set_transition_duration(300);

    let cache = MusicCache::new().expect("Failed to initialise music cache");
    let state: Arc<Mutex<AppState>> = Arc::new(Mutex::new(AppState {
        client: None,
        cache,
    }));

    let onboarding = build_onboarding_page(&stack, &state);
    let vr_auth = build_vr_auth_page(&stack, &state);
    let main_widgets = build_main_page(&stack, &state);

    stack.add_named(&onboarding, Some("onboarding"));
    stack.add_named(&vr_auth, Some("vr_auth"));
    stack.add_named(&main_widgets.page, Some("main"));
    stack.set_visible_child_name("onboarding");

    {
        let state = state.clone();
        let ps = main_widgets.player_state.clone();
        window.connect_close_request(move |_| {
            reset_player_ui(&ps, &state);
            glib::Propagation::Proceed
        });
    }

    window.set_child(Some(&stack));
    window.present();

    // Auto-load home + history when main page becomes visible
    {
        let state = state.clone();
        let content_box = main_widgets.content_box.clone();
        let ps = main_widgets.player_state.clone();
        let recent_list = main_widgets.recent_list.clone();
        let loaded = Rc::new(RefCell::new(false));

        stack.connect_notify_local(Some("visible-child-name"), move |stack, _| {
            if stack.visible_child_name().as_deref() == Some("main") && !*loaded.borrow() {
                *loaded.borrow_mut() = true;
                load_home(&state, &content_box, &ps, &recent_list);
                load_sidebar_history(&state, &recent_list, &ps);
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
                        // Load VR refresh token if available
                        if let Ok(Some(vr_token)) = gio::spawn_blocking(get_vr_refresh_token).await {
                            ytm.set_vr_refresh_token(&vr_token);
                        }
                        state.lock().unwrap().client = Some(Arc::new(ytm));
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
                    state.lock().unwrap().client = Some(Arc::new(ytm));
                    let state = state.clone();
                    let stack = stack.clone();
                    let status_label = status_label.clone();
                    let connect_btn = connect_btn_inner.clone();
                    let cookie_text = cookie_text.clone();

                    glib::spawn_future_local(async move {
                        let state_bg = state.clone();
                        let result = gio::spawn_blocking(move || {
                            let Some(client) = app_client(&state_bg) else {
                                return Err(anyhow::anyhow!("Not connected"));
                            };
                            client.check_auth()
                        }).await;

                        match result {
                            Ok(Ok(true)) | Ok(Ok(false)) => {
                                let cookie_for_save = cookie_text.clone();
                                let _ = gio::spawn_blocking(move || {
                                    let _ = store_cookie(&cookie_for_save);
                                }).await;
                                // Show VR auth step
                                stack.set_visible_child_name("vr_auth");
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
// VR AUTH PAGE (optional step 2)
// ============================================================

fn build_vr_auth_page(stack: &Stack, state: &Arc<Mutex<AppState>>) -> GtkBox {
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
        "If you have YouTube Music Premium, sign in to play\npremium-only tracks. You can skip this step."
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

fn build_main_page(stack: &Stack, state: &Arc<Mutex<AppState>>) -> MainPageWidgets {
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

    scrolled.set_child(Some(&content_box));
    content_outer.append(&scrolled);

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

    let np_title = Label::new(Some("Nothing playing"));
    np_title.add_css_class("np-track-title");
    np_title.set_halign(Align::Start);
    let np_title_sw = label_scroller(&np_title, 22);
    text_box.append(&np_title_sw);

    let np_artist = Label::new(None);
    np_artist.add_css_class("np-track-artist");
    np_artist.set_halign(Align::Start);
    let np_artist_sw = label_scroller(&np_artist, 18);
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
    repeat_btn.set_sensitive(false);
    modes_box.append(&repeat_btn);

    let shuffle_btn = icon_button(ICON_SHUFFLE, 18, "np-icon-btn");
    shuffle_btn.set_sensitive(false);
    modes_box.append(&shuffle_btn);

    controls_box.append(&modes_box);

    np_bar.append(&controls_box);
    content_outer.append(&np_bar);

    hbox.append(&content_outer);

    // ======= QUEUE PANEL (right side, like YTM's "Up Next") =======
    let queue_panel = GtkBox::new(Orientation::Vertical, 0);
    queue_panel.add_css_class("queue-panel");
    queue_panel.set_visible(false);

    let queue_header = Label::new(Some("Up Next"));
    queue_header.add_css_class("queue-header");
    queue_header.set_halign(Align::Start);
    queue_header.set_margin_start(16);
    queue_header.set_margin_top(12);
    queue_header.set_margin_bottom(8);
    queue_panel.append(&queue_header);

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
    queue_panel.append(&chip_scroll);

    // Scrollable queue list
    let queue_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let queue_list_box = GtkBox::new(Orientation::Vertical, 0);
    queue_list_box.set_margin_top(4);
    queue_scroll.set_child(Some(&queue_list_box));
    queue_panel.append(&queue_scroll);

    hbox.append(&queue_panel);
    paned.set_end_child(Some(&hbox));
    page.append(&paned);

    // ======= PLAYER STATE =======
    let ps = Rc::new(PlayerState {
        player: Player::new(),
        progress_slider: progress_slider.clone(),
        is_seeking: Cell::new(false),
        prev_btn: prev_btn.clone(),
        play_pause_btn: play_pause_btn.clone(),
        next_btn: next_btn.clone(),
        time_label: time_label.clone(),
        np_thumb: np_thumb.clone(),
        np_title: np_title.clone(),
        np_artist: np_artist.clone(),
        dislike_btn: dislike_btn.clone(),
        like_btn: like_btn.clone(),
        menu_btn: menu_btn.clone(),
        volume_slider: volume_slider.clone(),
        repeat_btn: repeat_btn.clone(),
        shuffle_btn: shuffle_btn.clone(),
        queue: RefCell::new(Vec::new()),
        queue_index: Cell::new(0),
        chips: RefCell::new(Vec::new()),
        seed_video_id: RefCell::new(String::new()),
        queue_list_box: queue_list_box.clone(),
        chip_bar: chip_bar.clone(),
        queue_panel: queue_panel.clone(),
        now_playing_track: RefCell::new(None),
        playback_session_id: Cell::new(0),
    });

    // ======= WIRE SIGNALS =======

    // Play/Pause toggle
    {
        let ps = ps.clone();
        play_pause_btn.connect_clicked(move |btn| {
            ps.player.toggle_pause();
            if ps.player.is_paused() {
                set_button_icon(btn, ICON_PLAY, 28);
            } else {
                set_button_icon(btn, ICON_PAUSE, 28);
            }
        });
    }

    // Next track
    {
        let ps = ps.clone();
        let state = state.clone();
        next_btn.connect_clicked(move |_| {
            let idx = ps.queue_index.get();
            let len = ps.queue.borrow().len();
            if len > 0 && idx + 1 < len {
                play_from_queue(&state, &ps, idx + 1);
            }
        });
    }

    // Previous track
    {
        let ps = ps.clone();
        let state = state.clone();
        prev_btn.connect_clicked(move |_| {
            let idx = ps.queue_index.get();
            if idx > 0 {
                play_from_queue(&state, &ps, idx - 1);
            }
        });
    }

    // Seek (progress slider)
    {
        let ps = ps.clone();
        progress_slider.connect_change_value(move |_, _, value| {
            ps.is_seeking.set(true);
            if let Some(duration) = ps.player.duration() {
                let target = ClockTime::from_nseconds((value * duration.nseconds() as f64) as u64);
                ps.player.seek(target);
            }
            ps.is_seeking.set(false);
            glib::Propagation::Proceed
        });
    }

    // Volume
    {
        let ps = ps.clone();
        volume_slider.connect_value_changed(move |slider| {
            ps.player.set_volume(slider.value());
        });
    }

    // Position update timer (250ms) + auto-advance
    {
        let ps = ps.clone();
        let state = state.clone();
        let auto_advancing = Rc::new(Cell::new(false));
        glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            if ps.is_seeking.get() {
                return glib::ControlFlow::Continue;
            }

            let pos = ps.player.position();
            let dur = ps.player.duration();

            if let (Some(pos), Some(dur)) = (pos, dur) {
                let dur_ns = dur.nseconds();
                if dur_ns > 0 {
                    let frac = pos.nseconds() as f64 / dur_ns as f64;
                    ps.progress_slider.set_value(frac);

                    // Auto-advance: when within 500ms of the end
                    if !auto_advancing.get() && pos.nseconds() + 500_000_000 >= dur_ns && dur_ns > 1_000_000_000 {
                        let idx = ps.queue_index.get();
                        let len = ps.queue.borrow().len();
                        if len > 0 && idx + 1 < len {
                            auto_advancing.set(true);
                            let ps2 = ps.clone();
                            let state2 = state.clone();
                            let aa = auto_advancing.clone();
                            glib::idle_add_local_once(move || {
                                play_from_queue(&state2, &ps2, idx + 1);
                                aa.set(false);
                            });
                        }
                    }
                }
                ps.time_label.set_text(&format!(
                    "{} / {}",
                    format_clock_time(pos),
                    format_clock_time(dur),
                ));
            }

            // Update buffer health (grey bar)
            let buffered = ps.player.buffered_fraction();
            ps.progress_slider.set_fill_level(buffered);

            glib::ControlFlow::Continue
        });
    }

    // Logout (inside settings popover)
    {
        let state = state.clone();
        let stack = stack.clone();
        let ps = ps.clone();
        let content_box = content_box.clone();
        let settings_popover = settings_popover.clone();

        logout_btn.connect_clicked(move |_| {
            settings_popover.popdown();
            reset_player_ui(&ps, &state);
            state.lock().unwrap().client = None;
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

    let nav_buttons: Rc<[Button; 4]> = Rc::new([
        home_btn.clone(),
        likes_btn.clone(),
        playlists_btn.clone(),
        offline_btn.clone(),
    ]);

    // Home button
    {
        let state = state.clone();
        let content_box = content_box.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let nav = nav_buttons.clone();

        home_btn.connect_clicked(move |btn| {
            set_active_nav(btn, &nav);
            load_home(&state, &content_box, &ps, &recent_list);
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
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let nav = nav_buttons.clone();

        offline_btn.connect_clicked(move |btn| {
            set_active_nav(btn, &nav);
            load_offline(&state, &content_box, &ps, &recent_list);
        });
    }

    // Search
    {
        let state = state.clone();
        let content_box = content_box.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();

        search_entry.connect_activate(move |entry| {
            let query = entry.text().to_string();
            if query.is_empty() {
                return;
            }
            load_search(&state, &content_box, &query, &ps, &recent_list);
        });
    }

    MainPageWidgets {
        page,
        content_box,
        player_state: ps,
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

fn set_active_nav(active: &Button, all: &[Button; 4]) {
    for btn in all {
        btn.remove_css_class("nav-item-active");
    }
    active.add_css_class("nav-item-active");
}

fn svg_image(svg_bytes: &[u8], size: i32) -> Image {
    let gbytes = glib::Bytes::from(svg_bytes);
    let stream = gio::MemoryInputStream::from_bytes(&gbytes);
    if let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(
        &stream, size, size, true, gio::Cancellable::NONE,
    ) {
        let texture = gtk::gdk::Texture::for_pixbuf(&pixbuf);
        let image = Image::from_paintable(Some(&texture));
        image.set_pixel_size(size);
        image
    } else {
        Image::new()
    }
}

fn icon_button(svg_bytes: &[u8], size: i32, css_class: &str) -> Button {
    let btn = Button::new();
    btn.set_child(Some(&svg_image(svg_bytes, size)));
    btn.add_css_class(css_class);
    btn
}

fn set_button_icon(btn: &Button, svg_bytes: &[u8], size: i32) {
    btn.set_child(Some(&svg_image(svg_bytes, size)));
}

fn reset_player_ui(ps: &PlayerState, state: &Arc<Mutex<AppState>>) {
    flush_play_record(ps, state);
    ps.player.stop();
    ps.np_title.set_text("Nothing playing");
    ps.np_artist.set_text("");
    ps.np_thumb.set_paintable(None::<&gtk::gdk::Texture>);
    ps.time_label.set_text("0:00 / 0:00");
    ps.progress_slider.set_value(0.0);
    ps.progress_slider.set_fill_level(0.0);
    set_button_icon(&ps.play_pause_btn, ICON_PLAY, 28);
    ps.play_pause_btn.set_sensitive(false);
    ps.prev_btn.set_sensitive(false);
    ps.next_btn.set_sensitive(false);
    ps.like_btn.set_sensitive(false);
    ps.dislike_btn.set_sensitive(false);
    ps.menu_btn.set_sensitive(false);
}

fn format_clock_time(t: ClockTime) -> String {
    let total_secs = t.seconds();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}:{secs:02}")
}

// ============================================================
// DATA LOADING
// ============================================================

fn load_home(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some("Loading recommendations..."));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_home())
        }).await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(sections))) => {
                render_home_view(&content_box, &sections, &state, &ps, &recent_list);
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
    ps: &Rc<PlayerState>,
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
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let q = query.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.search(&q))
        }).await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(tracks))) => {
                render_search_view(&content_box, &query, &tracks, &state, &ps, &recent_list);
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

fn load_offline_page_data(state: &Arc<Mutex<AppState>>) -> (Vec<PlaylistSummary>, Vec<Track>) {
    let guard = state.lock().unwrap();

    let offline_playlists = guard
        .cache
        .offline_playlists()
        .unwrap_or_default()
        .into_iter()
        .map(|(playlist_id, title, thumbnail_url, subtitle)| {
            let thumbnail_url = thumbnail_url.or_else(|| {
                guard
                    .cache
                    .offline_playlist_track_ids(&playlist_id)
                    .ok()
                    .and_then(|ids| ids.first().cloned())
                    .and_then(|vid| guard.cache.get_cached_art(&vid))
                    .map(|path| format!("file://{}", path.display()))
            });

            PlaylistSummary {
                playlist_id,
                title,
                thumbnail_url,
                subtitle,
            }
        })
        .collect();

    let tracks = guard
        .cache
        .offline_tracks()
        .unwrap_or_default()
        .into_iter()
        .map(|(video_id, title, artist, _art)| {
            let artists = if artist.is_empty() {
                vec![]
            } else {
                vec![Artist {
                    name: artist,
                    id: None,
                }]
            };
            let thumbnail_url = guard
                .cache
                .get_cached_art(&video_id)
                .map(|path| format!("file://{}", path.display()));

            Track {
                video_id,
                title,
                artists,
                album: None,
                duration: None,
                thumbnail_url,
            }
        })
        .collect();

    (offline_playlists, tracks)
}

fn load_offline_playlist_tracks(state: &Arc<Mutex<AppState>>, playlist_id: &str) -> Vec<Track> {
    let guard = state.lock().unwrap();
    let video_ids = guard
        .cache
        .offline_playlist_track_ids(playlist_id)
        .unwrap_or_default();

    video_ids
        .into_iter()
        .map(|video_id| {
            let thumbnail_url = guard
                .cache
                .get_cached_art(&video_id)
                .map(|path| format!("file://{}", path.display()));
            let (title, artist) = guard
                .cache
                .get_track_meta(&video_id)
                .unwrap_or_else(|| (video_id.clone(), String::new()));
            let artists = if artist.is_empty() {
                vec![]
            } else {
                vec![Artist {
                    name: artist,
                    id: None,
                }]
            };

            Track {
                video_id,
                title,
                artists,
                album: None,
                duration: None,
                thumbnail_url,
            }
        })
        .collect()
}

fn load_offline(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some("Loading offline library..."));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result = gio::spawn_blocking(move || load_offline_page_data(&state_bg)).await;

        clear_box(&content_box);

        match result {
            Ok((offline_playlists, tracks)) => {
                if tracks.is_empty() && offline_playlists.is_empty() {
                    let empty = Label::new(Some("No offline songs yet. Play songs to build your cache."));
                    empty.add_css_class("dim-label");
                    empty.set_margin_top(60);
                    content_box.append(&empty);
                    return;
                }

                let header = Label::new(Some("Offline"));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(20);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if !offline_playlists.is_empty() {
                    let pl_header = Label::new(Some("Downloaded Playlists"));
                    pl_header.add_css_class("section-header");
                    pl_header.set_halign(Align::Start);
                    pl_header.set_margin_start(28);
                    pl_header.set_margin_top(8);
                    pl_header.set_margin_bottom(4);
                    content_box.append(&pl_header);

                    let cards_scroll = ScrolledWindow::builder()
                        .hscrollbar_policy(gtk::PolicyType::Automatic)
                        .vscrollbar_policy(gtk::PolicyType::Never)
                        .build();
                    cards_scroll.add_css_class("cards-scroll");

                    let cards_box = GtkBox::new(Orientation::Horizontal, 14);
                    cards_box.set_margin_top(4);
                    cards_box.set_margin_bottom(12);
                    cards_box.set_margin_start(24);

                    for playlist in &offline_playlists {
                        let card = build_offline_playlist_card(playlist, &state, &content_box, &ps, &recent_list);
                        cards_box.append(&card);
                    }

                    cards_scroll.set_child(Some(&cards_box));
                    content_box.append(&cards_scroll);
                    content_box.append(&Separator::new(Orientation::Horizontal));
                }

                if !tracks.is_empty() {
                    let count_label = Label::new(Some(&format!("{} songs available offline", tracks.len())));
                    count_label.add_css_class("dim-label");
                    count_label.set_halign(Align::Start);
                    count_label.set_margin_start(28);
                    count_label.set_margin_top(8);
                    count_label.set_margin_bottom(4);
                    content_box.append(&count_label);

                    content_box.append(&build_play_controls(&tracks, &state, &ps, None));

                    let grid = GtkBox::new(Orientation::Vertical, 6);
                    grid.set_margin_start(20);
                    grid.set_margin_end(20);

                    for track in &tracks {
                        grid.append(&build_track_row(track, &state, &ps, &recent_list));
                    }

                    content_box.append(&grid);
                }
            }
            Err(e) => {
                content_box.append(&Label::new(Some(&format!("Offline load failed: {e:?}"))));
            }
        }
    });
}

fn load_likes(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some("Loading liked songs..."));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_playlist("LM"))
        }).await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(tracks))) => {
                let header = Label::new(Some("Liked Songs"));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(20);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if tracks.is_empty() {
                    let msg = Label::new(Some("No liked songs yet."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                let count_label = Label::new(Some(&format!("{} liked songs", tracks.len())));
                count_label.add_css_class("dim-label");
                count_label.set_halign(Align::Start);
                count_label.set_margin_start(28);
                count_label.set_margin_bottom(4);
                content_box.append(&count_label);

                content_box.append(&build_play_controls(&tracks, &state, &ps, Some(DownloadPlaylistMeta {
                    playlist_id: "LM".to_string(),
                    title: "Liked Songs".to_string(),
                    thumbnail_url: None,
                    subtitle: None,
                })));

                let grid = gtk::Grid::new();
                grid.set_column_spacing(12);
                grid.set_row_spacing(8);
                grid.set_column_homogeneous(true);
                grid.set_margin_top(4);
                grid.set_margin_start(20);
                grid.set_margin_end(20);

                for (i, track) in tracks.iter().enumerate() {
                    let row = build_track_row(track, &state, &ps, &recent_list);
                    grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
                }

                content_box.append(&grid);
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

fn load_playlists(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some("Loading playlists..."));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_library_playlists())
        }).await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(playlists))) => {
                let header = Label::new(Some("Your Playlists"));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(20);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if playlists.is_empty() {
                    let msg = Label::new(Some("No playlists found."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                let flow = gtk::FlowBox::new();
                flow.set_orientation(Orientation::Horizontal);
                flow.set_homogeneous(false);
                flow.set_min_children_per_line(1);
                flow.set_max_children_per_line(12);
                flow.set_column_spacing(14);
                flow.set_row_spacing(14);
                flow.set_selection_mode(gtk::SelectionMode::None);
                flow.set_halign(Align::Fill);
                flow.set_valign(Align::Start);
                flow.set_hexpand(true);
                flow.set_margin_start(24);
                flow.set_margin_end(24);
                flow.set_margin_top(8);

                for playlist in &playlists {
                    let card = build_playlist_card(playlist, &state, &content_box, &ps, &recent_list);
                    flow.insert(&card, -1);
                }

                content_box.append(&flow);
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

fn build_playlist_card(
    playlist: &PlaylistSummary,
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> Button {
    let card_btn = Button::new();
    card_btn.add_css_class("cover-card");
    card_btn.set_size_request(166, -1);
    card_btn.set_hexpand(false);
    card_btn.set_halign(Align::Start);
    card_btn.set_valign(Align::Start);

    let card = GtkBox::new(Orientation::Vertical, 6);
    card.set_size_request(150, -1);
    card.set_hexpand(false);

    let art_container = GtkBox::new(Orientation::Vertical, 0);
    art_container.add_css_class("cover-art");
    art_container.set_size_request(150, 150);
    art_container.set_hexpand(false);
    art_container.set_overflow(gtk::Overflow::Hidden);

    if let Some(url) = &playlist.thumbnail_url {
        let picture = Picture::new();
        picture.set_keep_aspect_ratio(false);
        picture.set_size_request(150, 150);
        picture.set_hexpand(false);
        art_container.append(&picture);
        load_thumbnail_into(&picture, url, 150);
    } else {
        let initial = playlist.title.chars().next().unwrap_or('\u{266B}').to_uppercase().to_string();
        let letter = Label::new(Some(&initial));
        letter.add_css_class("cover-art-letter");
        letter.set_vexpand(true);
        letter.set_valign(Align::Center);
        art_container.append(&letter);
    }

    card.append(&art_container);

    let name = Label::new(Some(&playlist.title));
    name.add_css_class("cover-card-title");
    name.set_halign(Align::Start);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_max_width_chars(18);
    card.append(&name);

    if let Some(sub) = &playlist.subtitle {
        let subtitle = Label::new(Some(sub));
        subtitle.add_css_class("cover-card-artist");
        subtitle.set_halign(Align::Start);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        subtitle.set_max_width_chars(18);
        card.append(&subtitle);
    }

    card_btn.set_child(Some(&card));

    {
        let state = state.clone();
        let content_box = content_box.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let playlist_id = playlist.playlist_id.clone();
        let playlist_title = playlist.title.clone();
        let playlist_thumb = playlist.thumbnail_url.clone();

        card_btn.connect_clicked(move |_| {
            load_playlist_detail(&state, &content_box, &playlist_id, &playlist_title, playlist_thumb.as_deref(), &ps, &recent_list);
        });
    }

    card_btn
}

fn load_playlist_detail(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    playlist_id: &str,
    playlist_title: &str,
    playlist_thumb: Option<&str>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some(&format!("Loading \"{}\"...", playlist_title)));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();
    let playlist_id = playlist_id.to_string();
    let playlist_title = playlist_title.to_string();
    let playlist_thumb = playlist_thumb.map(|s| s.to_string());

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let pid = playlist_id.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_playlist(&pid))
        }).await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(tracks))) => {
                // Back button
                let back_btn = Button::with_label("\u{2190} Back to Playlists");
                back_btn.add_css_class("nav-item");
                back_btn.set_halign(Align::Start);
                back_btn.set_margin_start(24);
                back_btn.set_margin_top(12);
                {
                    let state = state.clone();
                    let content_box = content_box.clone();
                    let ps = ps.clone();
                    let recent_list = recent_list.clone();
                    back_btn.connect_clicked(move |_| {
                        load_playlists(&state, &content_box, &ps, &recent_list);
                    });
                }
                content_box.append(&back_btn);

                let header = Label::new(Some(&playlist_title));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(12);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if tracks.is_empty() {
                    let msg = Label::new(Some("This playlist is empty."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                let count_label = Label::new(Some(&format!("{} tracks", tracks.len())));
                count_label.add_css_class("dim-label");
                count_label.set_halign(Align::Start);
                count_label.set_margin_start(28);
                count_label.set_margin_bottom(4);
                content_box.append(&count_label);

                content_box.append(&build_play_controls(&tracks, &state, &ps, Some(DownloadPlaylistMeta {
                    playlist_id: playlist_id.clone(),
                    title: playlist_title.clone(),
                    thumbnail_url: playlist_thumb.clone(),
                    subtitle: Some(format!("{} tracks", tracks.len())),
                })));

                let grid = gtk::Grid::new();
                grid.set_column_spacing(12);
                grid.set_row_spacing(8);
                grid.set_column_homogeneous(true);
                grid.set_margin_top(4);
                grid.set_margin_start(20);
                grid.set_margin_end(20);

                for (i, track) in tracks.iter().enumerate() {
                    let row = build_track_row(track, &state, &ps, &recent_list);
                    grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
                }

                content_box.append(&grid);
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

fn build_offline_playlist_card(
    playlist: &PlaylistSummary,
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> Button {
    let card_btn = Button::new();
    card_btn.add_css_class("cover-card");
    card_btn.set_size_request(166, -1);
    card_btn.set_hexpand(false);
    card_btn.set_halign(Align::Start);
    card_btn.set_valign(Align::Start);

    let card = GtkBox::new(Orientation::Vertical, 6);
    card.set_size_request(150, -1);

    let art_container = GtkBox::new(Orientation::Vertical, 0);
    art_container.add_css_class("cover-art");
    art_container.set_size_request(150, 150);
    art_container.set_overflow(gtk::Overflow::Hidden);

    if let Some(url) = &playlist.thumbnail_url {
        let picture = Picture::new();
        picture.set_keep_aspect_ratio(false);
        picture.set_size_request(150, 150);
        art_container.append(&picture);
        load_thumbnail_into(&picture, url, 150);
    } else {
        let initial = playlist.title.chars().next().unwrap_or('\u{266B}').to_uppercase().to_string();
        let letter = Label::new(Some(&initial));
        letter.add_css_class("cover-art-letter");
        letter.set_vexpand(true);
        letter.set_valign(Align::Center);
        art_container.append(&letter);
    }

    card.append(&art_container);

    let name = Label::new(Some(&playlist.title));
    name.add_css_class("cover-card-title");
    name.set_halign(Align::Start);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_max_width_chars(18);
    card.append(&name);

    if let Some(sub) = &playlist.subtitle {
        let subtitle = Label::new(Some(sub));
        subtitle.add_css_class("cover-card-artist");
        subtitle.set_halign(Align::Start);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        subtitle.set_max_width_chars(18);
        card.append(&subtitle);
    }

    card_btn.set_child(Some(&card));

    {
        let state = state.clone();
        let content_box = content_box.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let playlist_id = playlist.playlist_id.clone();
        let playlist_title = playlist.title.clone();

        card_btn.connect_clicked(move |_| {
            load_offline_playlist_detail(&state, &content_box, &playlist_id, &playlist_title, &ps, &recent_list);
        });
    }

    card_btn
}

fn load_offline_playlist_detail(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    playlist_id: &str,
    playlist_title: &str,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some(&format!("Loading \"{}\"...", playlist_title)));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();
    let playlist_id = playlist_id.to_string();
    let playlist_title = playlist_title.to_string();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let playlist_id_bg = playlist_id.clone();
        let result = gio::spawn_blocking(move || load_offline_playlist_tracks(&state_bg, &playlist_id_bg)).await;

        clear_box(&content_box);

        match result {
            Ok(tracks) => {
                let back_btn = Button::with_label("\u{2190} Back to Offline");
                back_btn.add_css_class("nav-item");
                back_btn.set_halign(Align::Start);
                back_btn.set_margin_start(24);
                back_btn.set_margin_top(12);
                {
                    let state = state.clone();
                    let content_box = content_box.clone();
                    let ps = ps.clone();
                    let recent_list = recent_list.clone();
                    back_btn.connect_clicked(move |_| {
                        load_offline(&state, &content_box, &ps, &recent_list);
                    });
                }
                content_box.append(&back_btn);

                let header = Label::new(Some(&playlist_title));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(12);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if tracks.is_empty() {
                    let msg = Label::new(Some("No tracks downloaded for this playlist."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                let count_label = Label::new(Some(&format!("{} tracks", tracks.len())));
                count_label.add_css_class("dim-label");
                count_label.set_halign(Align::Start);
                count_label.set_margin_start(28);
                count_label.set_margin_bottom(4);
                content_box.append(&count_label);

                content_box.append(&build_play_controls(&tracks, &state, &ps, None));

                let grid = gtk::Grid::new();
                grid.set_column_spacing(12);
                grid.set_row_spacing(8);
                grid.set_column_homogeneous(true);
                grid.set_margin_top(4);
                grid.set_margin_start(20);
                grid.set_margin_end(20);

                for (i, track) in tracks.iter().enumerate() {
                    let row = build_track_row(track, &state, &ps, &recent_list);
                    grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
                }

                content_box.append(&grid);
            }
            Err(e) => {
                content_box.append(&Label::new(Some(&format!("Offline playlist load failed: {e:?}"))));
            }
        }
    });
}

fn load_sidebar_history(
    state: &Arc<Mutex<AppState>>,
    recent_list: &GtkBox,
    ps: &Rc<PlayerState>,
) {
    let state = state.clone();
    let recent_list = recent_list.clone();
    let ps = ps.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_history())
        }).await;

        if let Ok(Some(Ok(tracks))) = result {
            clear_box(&recent_list);
            for track in tracks.iter().take(15) {
                let item = build_sidebar_recent_item(track, &state, &ps);
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
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    if sections.is_empty() {
        let msg = Label::new(Some("No recommendations returned. Your cookie might be expired."));
        msg.add_css_class("dim-label");
        msg.set_margin_top(60);
        content_box.append(&msg);
        return;
    }

    // First section -> horizontal cover art cards
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
            cards_box.append(&build_cover_card(track, state, ps, recent_list));
        }

        cards_scroll.set_child(Some(&cards_box));
        content_box.append(&cards_scroll);
    }

    // Remaining sections -> 2-column grid
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
            let row = build_track_row(track, state, ps, recent_list);
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
    ps: &Rc<PlayerState>,
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
        let row = build_track_row(track, state, ps, recent_list);
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
    ps: &Rc<PlayerState>,
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
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let track = track.clone();
        let video_id = track.video_id.clone();
        let title = track.title.clone();
        let artist_str = artist_str.clone();
        let thumb_url = track.thumbnail_url.clone();

        card_btn.connect_clicked(move |_| {
            play_track(&state, &video_id, &title, &artist_str, thumb_url.as_deref(), &ps);
            add_to_recent(&recent_list, &track, &state, &ps);
        });
    }

    card_btn
}

fn build_track_row(
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
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
        picture.set_keep_aspect_ratio(true);
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
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let track = track.clone();
        let video_id = track.video_id.clone();
        let title = track.title.clone();
        let artist_str = artist_str.clone();
        let thumb_url = track.thumbnail_url.clone();

        row_btn.connect_clicked(move |_| {
            play_track(&state, &video_id, &title, &artist_str, thumb_url.as_deref(), &ps);
            add_to_recent(&recent_list, &track, &state, &ps);
        });
    }

    row_btn
}

/// Wrap an existing label in a ScrolledWindow with marquee scrolling.
/// Restarts the scroll animation whenever the label text changes.
fn label_scroller(label: &Label, height: i32) -> ScrolledWindow {
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

fn start_marquee(sw: &ScrolledWindow) {
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
fn scrolling_label(text: &str, css_class: &str, height: i32) -> ScrolledWindow {
    let sw = ScrolledWindow::new();
    sw.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    sw.set_hexpand(true);
    sw.set_size_request(1, height);
    sw.set_vexpand(false);
    sw.set_propagate_natural_width(false);
    sw.set_min_content_width(0);
    // Keep marquee labels from inflating the sidebar's preferred width while
    // still allowing the label to scroll inside the fixed 240 px sidebar.
    sw.set_max_content_width(SIDEBAR_RECENT_CONTENT_WIDTH);

    let label = Label::new(Some(text));
    label.add_css_class(css_class);
    label.set_halign(Align::Start);
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

fn build_sidebar_recent_item(
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
            play_track(&state, &video_id, &title, &artist_str, thumb_url.as_deref(), &ps);
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
            if let Some(path) = url.strip_prefix("file://") {
                std::fs::read(path).map_err(|e| e.to_string())
            } else {
                THUMB_CLIENT.get(&url).send()
                    .and_then(|r| r.bytes())
                    .map(|b| b.to_vec())
                    .map_err(|e| e.to_string())
            }
        }).await;

        if let Ok(Ok(bytes)) = result {
            let gbytes = glib::Bytes::from(&bytes);
            let stream = gio::MemoryInputStream::from_bytes(&gbytes);
            // Load at original size first so we can compute aspect ratio
            if let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_stream(
                &stream, gio::Cancellable::NONE,
            ) {
                let w = pixbuf.width();
                let h = pixbuf.height();
                // Scale so the shorter side == size, then center-crop to square
                let (sw, sh) = if w < h {
                    (size, (h as f64 * size as f64 / w as f64) as i32)
                } else {
                    ((w as f64 * size as f64 / h as f64) as i32, size)
                };
                let scaled = pixbuf
                    .scale_simple(sw, sh, gtk::gdk_pixbuf::InterpType::Bilinear)
                    .unwrap_or(pixbuf);
                let ox = (scaled.width() - size).max(0) / 2;
                let oy = (scaled.height() - size).max(0) / 2;
                let cropped = scaled.new_subpixbuf(ox, oy, size.min(scaled.width()), size.min(scaled.height()));
                let texture = gtk::gdk::Texture::for_pixbuf(&cropped);
                picture.set_paintable(Some(&texture));
            }
        }
    });
}

/// Flush the current now-playing track.
/// We always report any non-zero watchtime we have to YouTube, while the local
/// play-history DB keeps its higher threshold for counting a play.
fn flush_play_record(ps: &PlayerState, state: &Arc<Mutex<AppState>>) {
    let info = ps.now_playing_track.borrow_mut().take();
    if let Some(track) = info {
        let listened = track.started_at.elapsed().as_secs();
        if let Some(tracking) = track.tracking {
            tracking.report_watchtime(listened);
        }
        if listened >= MIN_LISTEN_SECS {
            // Record in local DB
            let state = state.clone();
            gio::spawn_blocking(move || {
                let guard = state.lock().unwrap();
                let _ = guard.cache.record_play(
                    &track.video_id,
                    &track.title,
                    &track.artist,
                    track.thumb_url.as_deref(),
                    listened,
                );
            });
        }
    }
}

fn spawn_playback_tracking(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    video_id: &str,
    session_id: u64,
) {
    let state = state.clone();
    let ps = ps.clone();
    let video_id = video_id.to_string();

    glib::spawn_future_local(async move {
        let vid = video_id.clone();
        let tracking = gio::spawn_blocking(move || {
            app_client(&state).and_then(|ytm| ytm.start_playback_tracking(&vid))
        })
        .await
        .ok()
        .flatten();

        if let Some(tracking) = tracking {
            let mut now_playing = ps.now_playing_track.borrow_mut();
            if let Some(current) = now_playing.as_mut() {
                if current.session_id == session_id && current.video_id == video_id {
                    current.tracking = Some(tracking);
                }
            }
        }
    });
}

async fn prepare_playback(
    state: &Arc<Mutex<AppState>>,
    video_id: &str,
) -> Result<PreparedPlayback, String> {
    let state_cache = state.clone();
    let vid = video_id.to_string();
    let cached_path = gio::spawn_blocking(move || {
        let guard = state_cache.lock().unwrap();
        guard.cache.get_cached_audio(&vid)
    })
    .await
    .ok()
    .flatten();

    if let Some(path) = cached_path {
        eprintln!("[CACHE] playing from offline: {}", path.display());
        return Ok(PreparedPlayback {
            uri: format!("file://{}", path.display()),
        });
    }

    let state_remote = state.clone();
    let vid = video_id.to_string();
    match gio::spawn_blocking(move || {
        let Some(client) = app_client(&state_remote) else {
            return Err("Playback failed: not connected and no offline copy available".to_string());
        };
        client
            .get_audio_url_no_track(&vid)
            .map(|uri| PreparedPlayback { uri })
            .map_err(|e| format!("Playback failed: {e}"))
    })
    .await
    {
        Ok(Ok(prepared)) => Ok(prepared),
        Ok(Err(msg)) => Err(msg),
        Err(e) => Err(format!("Error: {e:?}")),
    }
}

fn play_track(
    state: &Arc<Mutex<AppState>>,
    video_id: &str,
    title: &str,
    artist: &str,
    thumb_url: Option<&str>,
    ps: &Rc<PlayerState>,
) {
    // Flush listen time for the previous track before switching
    flush_play_record(ps, state);

    // Stop immediately so stale position data can't trigger auto-advance
    ps.player.stop();

    // Immediately flush all UI state for the new track
    ps.np_title.set_text(title);
    ps.np_artist.set_text(artist);
    ps.time_label.set_text("0:00 / 0:00");
    ps.progress_slider.set_value(0.0);
    ps.progress_slider.set_fill_level(0.0);
    set_button_icon(&ps.play_pause_btn, ICON_PLAY, 28);
    ps.play_pause_btn.set_sensitive(false);
    ps.like_btn.set_sensitive(false);
    ps.dislike_btn.set_sensitive(false);
    ps.menu_btn.set_sensitive(false);

    if let Some(url) = thumb_url {
        load_thumbnail_into(&ps.np_thumb, url, 80);
    } else {
        ps.np_thumb.set_paintable(None::<&gtk::gdk::Texture>);
    }

    let state = state.clone();
    let video_id = video_id.to_string();
    let title = title.to_string();
    let artist = artist.to_string();
    let thumb_url = thumb_url.map(String::from);
    let ps = ps.clone();

    glib::spawn_future_local(async move {
        match prepare_playback(&state, &video_id).await {
            Ok(prepared) => {
                ps.player.play_uri(&prepared.uri);
                ps.np_title.set_text(&title);
                set_button_icon(&ps.play_pause_btn, ICON_PAUSE, 28);
                ps.play_pause_btn.set_sensitive(true);
                ps.prev_btn.set_sensitive(true);
                ps.next_btn.set_sensitive(true);
                ps.like_btn.set_sensitive(true);
                ps.dislike_btn.set_sensitive(true);
                ps.menu_btn.set_sensitive(true);

                // Start tracking this song's listen time
                let session_id = ps.playback_session_id.get().wrapping_add(1);
                ps.playback_session_id.set(session_id);
                *ps.now_playing_track.borrow_mut() = Some(NowPlayingTrack {
                    session_id,
                    video_id: video_id.clone(),
                    title: title.clone(),
                    artist: artist.clone(),
                    thumb_url: thumb_url.clone(),
                    started_at: Instant::now(),
                    tracking: None,
                });
                spawn_playback_tracking(&state, &ps, &video_id, session_id);

                // Trigger background sync (no play record — that happens on navigate-away)
                let state_bg = state.clone();
                gio::spawn_blocking(move || {
                    sync_offline_cache(&state_bg);
                });
            }
            Err(msg) => {
                ps.np_title.set_text(&msg);
                ps.np_artist.set_text("");
            }
        }

        // Fetch radio queue in background
        let state_bg = state.clone();
        let vid = video_id.clone();
        let radio_result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_radio(&vid, None, None))
        }).await;

        if let Ok(Some(Ok(mut radio))) = radio_result {
            // Ensure the seed track is always first in the queue
            let idx = radio.tracks.iter().position(|t| t.video_id == video_id);
            if let Some(i) = idx {
                // Move it to front if not already there
                if i > 0 {
                    let seed_track = radio.tracks.remove(i);
                    radio.tracks.insert(0, seed_track);
                }
            } else {
                // Not in queue at all — prepend it
                radio.tracks.insert(0, Track {
                    video_id: video_id.clone(),
                    title: title.clone(),
                    artists: vec![Artist { name: artist.clone(), id: None }],
                    album: None,
                    duration: None,
                    thumbnail_url: thumb_url.clone(),
                });
            }
            *ps.queue.borrow_mut() = radio.tracks;
            ps.queue_index.set(0);
            *ps.chips.borrow_mut() = radio.chips;
            *ps.seed_video_id.borrow_mut() = video_id.clone();
            render_queue_panel(&ps, &state);
        }
    });
}

/// Play a track from the queue by index (no radio re-fetch).
fn play_from_queue(state: &Arc<Mutex<AppState>>, ps: &Rc<PlayerState>, index: usize) {
    let queue = ps.queue.borrow();
    let Some(track) = queue.get(index) else { return };
    ps.queue_index.set(index);

    let video_id = track.video_id.clone();
    let title = track.title.clone();
    let artist = artist_string(track);
    let thumb_url = track.thumbnail_url.clone();
    drop(queue);

    // Flush listen time for the previous track before switching
    flush_play_record(ps, state);

    // Stop immediately so stale position data can't trigger auto-advance
    ps.player.stop();

    // Flush UI
    ps.np_title.set_text(&title);
    ps.np_artist.set_text(&artist);
    ps.time_label.set_text("0:00 / 0:00");
    ps.progress_slider.set_value(0.0);
    ps.progress_slider.set_fill_level(0.0);
    set_button_icon(&ps.play_pause_btn, ICON_PLAY, 28);
    ps.play_pause_btn.set_sensitive(false);

    if let Some(url) = &thumb_url {
        load_thumbnail_into(&ps.np_thumb, url, 80);
    } else {
        ps.np_thumb.set_paintable(None::<&gtk::gdk::Texture>);
    }

    // Re-render queue to highlight current track
    render_queue_panel(ps, state);

    let state = state.clone();
    let ps = ps.clone();

    glib::spawn_future_local(async move {
        match prepare_playback(&state, &video_id).await {
            Ok(prepared) => {
                ps.player.play_uri(&prepared.uri);
                ps.np_title.set_text(&title);
                set_button_icon(&ps.play_pause_btn, ICON_PAUSE, 28);
                ps.play_pause_btn.set_sensitive(true);
                ps.prev_btn.set_sensitive(true);
                ps.next_btn.set_sensitive(true);
                ps.like_btn.set_sensitive(true);
                ps.dislike_btn.set_sensitive(true);
                ps.menu_btn.set_sensitive(true);

                // Start tracking this song's listen time
                let session_id = ps.playback_session_id.get().wrapping_add(1);
                ps.playback_session_id.set(session_id);
                *ps.now_playing_track.borrow_mut() = Some(NowPlayingTrack {
                    session_id,
                    video_id: video_id.clone(),
                    title: title.clone(),
                    artist: artist.clone(),
                    thumb_url: thumb_url.clone(),
                    started_at: Instant::now(),
                    tracking: None,
                });
                spawn_playback_tracking(&state, &ps, &video_id, session_id);

                // Trigger background sync (no play record — that happens on navigate-away)
                let state_bg = state.clone();
                gio::spawn_blocking(move || {
                    sync_offline_cache(&state_bg);
                });
            }
            Err(msg) => {
                ps.np_title.set_text(&msg);
                ps.np_artist.set_text("");
            }
        }
    });
}

/// Set the queue to the given tracks and start playing from index 0.
/// If `shuffle` is true, the tracks are shuffled first.
fn play_all(
    tracks: &[Track],
    shuffle: bool,
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
) {
    if tracks.is_empty() {
        return;
    }

    let mut queue: Vec<Track> = tracks.to_vec();
    if shuffle {
        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        queue.shuffle(&mut rng);
    }

    *ps.queue.borrow_mut() = queue;
    ps.queue_index.set(0);
    ps.chips.borrow_mut().clear();
    ps.seed_video_id.borrow_mut().clear();

    play_from_queue(state, ps, 0);
}

/// Build a row with "Play All", "Shuffle", and optionally "Download All" buttons.
/// Pass `download_meta` to show the download button — `None` hides it.
fn build_play_controls(
    tracks: &[Track],
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    download_meta: Option<DownloadPlaylistMeta>,
) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    row.set_margin_start(28);
    row.set_margin_bottom(12);

    let play_all_btn = Button::new();
    let play_all_content = GtkBox::new(Orientation::Horizontal, 6);
    play_all_content.append(&svg_image(ICON_PLAY, 14));
    play_all_content.append(&Label::new(Some("Play All")));
    play_all_btn.set_child(Some(&play_all_content));
    play_all_btn.add_css_class("nav-item");
    {
        let tracks = tracks.to_vec();
        let state = state.clone();
        let ps = ps.clone();
        play_all_btn.connect_clicked(move |_| {
            play_all(&tracks, false, &state, &ps);
        });
    }
    row.append(&play_all_btn);

    let shuffle_btn = Button::new();
    let shuffle_content = GtkBox::new(Orientation::Horizontal, 6);
    shuffle_content.append(&svg_image(ICON_SHUFFLE, 14));
    shuffle_content.append(&Label::new(Some("Shuffle")));
    shuffle_btn.set_child(Some(&shuffle_content));
    shuffle_btn.add_css_class("nav-item");
    {
        let tracks = tracks.to_vec();
        let state = state.clone();
        let ps = ps.clone();
        shuffle_btn.connect_clicked(move |_| {
            play_all(&tracks, true, &state, &ps);
        });
    }
    row.append(&shuffle_btn);

    if let Some(meta) = download_meta {
        let already_downloaded = {
            let guard = state.lock().unwrap();
            guard.cache.is_playlist_offline(&meta.playlist_id)
        };

        let dl_btn = Button::new();
        let dl_content = GtkBox::new(Orientation::Horizontal, 6);
        dl_content.append(&svg_image(ICON_DOWNLOAD, 14));
        if already_downloaded {
            dl_content.append(&Label::new(Some("Downloaded")));
            dl_btn.set_sensitive(false);
        } else {
            dl_content.append(&Label::new(Some("Download All")));
        }
        dl_btn.set_child(Some(&dl_content));
        dl_btn.add_css_class("nav-item");
        {
            let tracks = tracks.to_vec();
            let state = state.clone();
            let dl_btn = dl_btn.clone();
            dl_btn.connect_clicked(move |btn| {
                btn.set_sensitive(false);
                download_all_offline(tracks.clone(), state.clone(), Some(meta.clone()));
            });
        }
        row.append(&dl_btn);
    }

    row
}

/// Optional playlist metadata for download grouping.
#[derive(Clone)]
struct DownloadPlaylistMeta {
    playlist_id: String,
    title: String,
    thumbnail_url: Option<String>,
    subtitle: Option<String>,
}

fn download_all_offline(
    tracks: Vec<Track>,
    state: Arc<Mutex<AppState>>,
    playlist_meta: Option<DownloadPlaylistMeta>,
) {
    let count = tracks.len();
    eprintln!("[DOWNLOAD] queuing {count} tracks for offline download");

    glib::spawn_future_local(async move {
        let tracks = tracks.clone();
        let state = state.clone();
        gio::spawn_blocking(move || {
            for (i, track) in tracks.iter().enumerate() {
                let vid = &track.video_id;

                // Skip if already cached
                {
                    let guard = state.lock().unwrap();
                    if guard.cache.is_cached(vid) {
                        let _ = guard.cache.pin_offline(vid);
                        eprintln!("[DOWNLOAD] {}/{count} already cached, pinned: {vid}", i + 1);
                        continue;
                    }
                }

                // Ensure track metadata is in plays table so offline_tracks() can show it
                {
                    let guard = state.lock().unwrap();
                    let artist = if track.artists.is_empty() {
                        "Unknown artist".to_string()
                    } else {
                        track.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
                    };
                    let _ = guard.cache.record_play(
                        vid,
                        &track.title,
                        &artist,
                        track.thumbnail_url.as_deref(),
                        0,
                    );
                }

                // Get streaming URL
                let url = {
                    let Some(client) = app_client(&state) else { return };
                    match client.get_audio_url_no_track(vid) {
                        Ok(u) => u,
                        Err(e) => {
                            eprintln!("[DOWNLOAD] {}/{count} failed URL for {vid}: {e}", i + 1);
                            continue;
                        }
                    }
                };

                // Download audio
                eprintln!("[DOWNLOAD] {}/{count} downloading {vid}...", i + 1);
                let audio = match DOWNLOAD_CLIENT
                    .get(&url)
                    .header("Range", "bytes=0-")
                    .send()
                    .and_then(|r| r.bytes())
                {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("[DOWNLOAD] {}/{count} failed: {e}", i + 1);
                        continue;
                    }
                };

                // Download artwork
                let art = track.thumbnail_url.as_ref().and_then(|url| {
                    THUMB_CLIENT.get(url).send().ok()?.bytes().ok()
                });

                // Store + pin
                let guard = state.lock().unwrap();
                let _ = guard.cache.store_audio(vid, &audio, art.as_deref());
                let _ = guard.cache.pin_offline(vid);
                eprintln!("[DOWNLOAD] {}/{count} done: {vid} ({} KB)", i + 1, audio.len() / 1024);
            }

            // Store playlist metadata + cover art if this is a playlist download
            if let Some(meta) = &playlist_meta {
                // Download playlist cover art to disk
                let cover_art_path = if let Some(url) = &meta.thumbnail_url {
                    THUMB_CLIENT.get(url).send().ok()
                        .and_then(|r| r.bytes().ok())
                        .map(|bytes| {
                            let guard = state.lock().unwrap();
                            let p = guard.cache.art_dir.join(format!("playlist_{}.jpg", meta.playlist_id));
                            let _ = std::fs::write(&p, &bytes);
                            format!("file://{}", p.display())
                        })
                } else {
                    None
                };

                let video_ids: Vec<String> = tracks.iter().map(|t| t.video_id.clone()).collect();
                let guard = state.lock().unwrap();
                let _ = guard.cache.store_offline_playlist(
                    &meta.playlist_id,
                    &meta.title,
                    cover_art_path.as_deref().or(meta.thumbnail_url.as_deref()),
                    meta.subtitle.as_deref(),
                    &video_ids,
                );
                eprintln!("[DOWNLOAD] stored playlist meta: {} ({} tracks)", meta.title, video_ids.len());
            }

            eprintln!("[DOWNLOAD] finished all {count} tracks");
        }).await.ok();
    });
}

/// Render the queue panel with chip filters and track list.
fn render_queue_panel(ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    ps.queue_panel.set_visible(true);

    // -- Chips --
    clear_box(&ps.chip_bar);
    let chips = ps.chips.borrow();
    for chip in chips.iter() {
        let btn = Button::with_label(&chip.label);
        btn.add_css_class("queue-chip");

        let state_c = state.clone();
        let ps_c = ps.clone();
        let playlist_id = chip.playlist_id.clone();
        let params = chip.params.clone();

        btn.connect_clicked(move |_| {
            load_radio_filter(&state_c, &ps_c, &playlist_id, params.as_deref());
        });
        ps.chip_bar.append(&btn);
    }
    drop(chips);

    // -- Track list --
    clear_box(&ps.queue_list_box);
    let queue = ps.queue.borrow();
    let current_idx = ps.queue_index.get();

    for (i, track) in queue.iter().enumerate() {
        let row = GtkBox::new(Orientation::Horizontal, 8);
        row.add_css_class("queue-row");
        row.set_margin_start(8);
        row.set_margin_end(8);
        row.set_margin_top(2);
        row.set_margin_bottom(2);

        if i == current_idx {
            row.add_css_class("queue-row-active");
        }

        // Small thumbnail
        let art = GtkBox::new(Orientation::Vertical, 0);
        art.add_css_class("queue-thumb");
        art.set_size_request(40, 40);
        art.set_valign(Align::Center);
        art.set_overflow(gtk::Overflow::Hidden);

        if let Some(url) = &track.thumbnail_url {
            let pic = Picture::new();
            pic.set_keep_aspect_ratio(true);
            pic.set_size_request(40, 40);
            art.append(&pic);
            load_thumbnail_into(&pic, url, 80);
        }
        row.append(&art);

        let info = GtkBox::new(Orientation::Vertical, 0);
        info.set_hexpand(true);
        info.set_valign(Align::Center);

        let title_lbl = Label::new(Some(&track.title));
        title_lbl.set_halign(Align::Start);
        title_lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title_lbl.add_css_class("queue-track-title");
        info.append(&title_lbl);

        let artist_str = artist_string(track);
        let artist_lbl = Label::new(Some(&artist_str));
        artist_lbl.set_halign(Align::Start);
        artist_lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
        artist_lbl.add_css_class("queue-track-artist");
        info.append(&artist_lbl);

        row.append(&info);

        if let Some(dur) = &track.duration {
            let dur_lbl = Label::new(Some(dur));
            dur_lbl.add_css_class("queue-track-duration");
            dur_lbl.set_valign(Align::Center);
            row.append(&dur_lbl);
        }

        let row_btn = Button::new();
        row_btn.add_css_class("queue-row-btn");
        row_btn.set_child(Some(&row));

        {
            let state = state.clone();
            let ps = ps.clone();
            row_btn.connect_clicked(move |_| {
                play_from_queue(&state, &ps, i);
            });
        }

        ps.queue_list_box.append(&row_btn);
    }
}

/// Switch radio filter (chip click).
fn load_radio_filter(state: &Arc<Mutex<AppState>>, ps: &Rc<PlayerState>, playlist_id: &str, params: Option<&str>) {
    let state = state.clone();
    let ps = ps.clone();
    let seed = ps.seed_video_id.borrow().clone();
    // Grab current seed track from queue so we can prepend if API omits it
    let seed_track = {
        let q = ps.queue.borrow();
        q.iter().find(|t| t.video_id == seed).cloned()
    };
    let playlist_id = playlist_id.to_string();
    let params = params.map(String::from);

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let vid = seed.clone();
        let plid = playlist_id.clone();
        let p = params.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_radio(&vid, Some(&plid), p.as_deref()))
        }).await;

        if let Ok(Some(Ok(mut radio))) = result {
            // Ensure seed track is always first
            let idx = radio.tracks.iter().position(|t| t.video_id == seed);
            if let Some(i) = idx {
                if i > 0 {
                    let t = radio.tracks.remove(i);
                    radio.tracks.insert(0, t);
                }
            } else if let Some(t) = seed_track {
                radio.tracks.insert(0, t);
            }
            *ps.queue.borrow_mut() = radio.tracks;
            ps.queue_index.set(0);
            // Only update chips if the response included them (filtered responses often don't)
            if !radio.chips.is_empty() {
                *ps.chips.borrow_mut() = radio.chips;
            }
            render_queue_panel(&ps, &state);
        }
    });
}

fn add_to_recent(
    recent_list: &GtkBox,
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
) {
    let item = build_sidebar_recent_item(track, state, ps);
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

/// Sync the offline cache: download top-100 most played songs, evict the rest.
/// Acquires the AppState lock briefly per-operation so playback is never blocked.
fn sync_offline_cache(state: &Arc<Mutex<AppState>>) {
    let (top, cached, pinned) = {
        let guard = state.lock().unwrap();
        let top = match guard.cache.top_played(100) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[CACHE] failed to get top played: {e}");
                return;
            }
        };
        let cached = guard.cache.cached_ids().unwrap_or_default();
        let pinned = guard.cache.pinned_ids().unwrap_or_default();
        (top, cached, pinned)
    }; // lock released

    // Evict songs no longer in top 100 (skip pinned/manually downloaded)
    for id in &cached {
        if !top.contains(id) && !pinned.contains(id) {
            let guard = state.lock().unwrap();
            let _ = guard.cache.remove_cached(id);
        }
    }

    // Download songs that are in top 100 but not cached
    let to_download: Vec<String> = top.into_iter()
        .filter(|id| !cached.contains(id))
        .collect();

    if to_download.is_empty() {
        return;
    }

    for vid in to_download {
        // Get streaming URL (brief lock)
        let url = {
            let Some(client) = app_client(state) else { return };
            match client.get_audio_url_no_track(&vid) {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("[CACHE] failed to get URL for {vid}: {e}");
                    continue;
                }
            }
        }; // lock released

        // Download audio bytes (no lock held)
        // YouTube often requires a Range header to start sending data.
        eprintln!("[CACHE] downloading {vid} for offline...");
        let audio = match DOWNLOAD_CLIENT
            .get(&url)
            .header("Range", "bytes=0-")
            .send()
            .and_then(|r| {
                let status = r.status();
                eprintln!("[CACHE] {vid} response: {status}, content-length: {:?}", r.content_length());
                r.bytes()
            }) {
            Ok(b) => {
                eprintln!("[CACHE] {vid} downloaded {} KB", b.len() / 1024);
                b
            }
            Err(e) => {
                eprintln!("[CACHE] download failed for {vid}: {e}");
                continue;
            }
        };

        // Download artwork (no lock held)
        let thumb_url = {
            let guard = state.lock().unwrap();
            guard.cache.get_thumbnail_url(&vid)
        };
        let art = thumb_url.and_then(|url| {
            THUMB_CLIENT.get(&url).send().ok()?.bytes().ok()
        });

        // Store to disk + DB (brief lock)
        let guard = state.lock().unwrap();
        let _ = guard.cache.store_audio(&vid, &audio, art.as_deref());
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
