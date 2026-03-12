use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use anyhow::Context;
use gstreamer::ClockTime;
use gtk::prelude::*;
use gtk::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, DragSource, DropTarget, Entry,
    GestureClick, Image, Label, MessageDialog, MessageType, Orientation, Overlay, Paned, Picture,
    Popover, ResponseType, Scale, ScrolledWindow, Separator, SpinButton, Stack,
    StackTransitionType, Switch, gio, glib,
};
use pbkdf2::pbkdf2_hmac;
use rusqlite::Connection as SqliteConnection;
use sha1::Sha1;
use zbus::{
    blocking::Connection as ZbusConnection,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
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
const ICON_REPEAT_ONE: &[u8] = include_bytes!("../../assets/icons/repeat_one.svg");
const ICON_SHUFFLE: &[u8] = include_bytes!("../../assets/icons/shuffle.svg");
const ICON_DOWNLOAD: &[u8] = include_bytes!("../../assets/icons/download.svg");

const SIDEBAR_WIDTH: i32 = 240;
const SIDEBAR_RECENT_CONTENT_WIDTH: i32 = SIDEBAR_WIDTH - 32;
const SIDEBAR_HISTORY_LIMIT: usize = 200;
const OFFLINE_LISTEN_AGAIN_PLAYLIST_ID: &str = "__offline_listen_again__";

use crate::cache::{AppSettings, MusicCache};
use crate::ipc::{PlaybackSnapshot, RepeatMode};
use crate::models::{
    Artist, BrowseSectionsPage, HomeBrowseCard, HomeBrowsePageType, HomeItem, HomePage, HomeSection,
    PlaylistSummary, RadioChip, RadioVirtualPlaylist, Track,
};
use crate::player::Player;
use crate::sponsorblock::{
    SPONSORBLOCK_CATEGORY_OPTIONS, SponsorBlockClient, SponsorSegment, default_skip_categories,
    sanitize_categories,
};
use crate::ytmusic::YtMusicClient;
use crate::ytmusic::client::SongRating;

const KEYRING_SERVICE: &str = "com.static.music";
const KEYRING_USER: &str = "cookie";
const KEYRING_VR_TOKEN: &str = "vr_refresh_token";
const KEYRING_STORAGE_ROOT: &str = "storage_root";

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type SecretResponse = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

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

static THUMBNAIL_BYTES_CACHE: LazyLock<Mutex<HashMap<String, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
    static THUMBNAIL_TEXTURE_CACHE: RefCell<HashMap<(String, i32), gtk::gdk::Texture>> =
        RefCell::new(HashMap::new());
}

struct AppState {
    client: Option<Arc<YtMusicClient>>,
    sponsorblock: SponsorBlockClient,
    cache: MusicCache,
    settings: AppSettings,
    home_cache: Option<HomePage>,
    home_scroll_position: f64,
}

/// Tracks the currently playing song so we can record listen time when navigating away.
struct NowPlayingTrack {
    session_id: u64,
    video_id: String,
    rating: SongRating,
    last_observed_position_millis: u64,
}

fn app_client(state: &Arc<Mutex<AppState>>) -> Option<Arc<YtMusicClient>> {
    state.lock().unwrap().client.clone()
}

fn cached_home_page(state: &Arc<Mutex<AppState>>) -> Option<HomePage> {
    state.lock().unwrap().home_cache.clone()
}

fn store_home_cache(state: &Arc<Mutex<AppState>>, page: &HomePage) {
    state.lock().unwrap().home_cache = Some(page.clone());
}

fn invalidate_home_cache(state: &Arc<Mutex<AppState>>) {
    state.lock().unwrap().home_cache = None;
}

fn cached_home_scroll_position(state: &Arc<Mutex<AppState>>) -> f64 {
    state.lock().unwrap().home_scroll_position
}

fn store_home_scroll_position(state: &Arc<Mutex<AppState>>, position: f64) {
    state.lock().unwrap().home_scroll_position = position.max(0.0);
}

#[allow(dead_code)]
struct PlayerState {
    player: Player,
    // Progress
    progress_slider: Scale,
    is_seeking: Cell<bool>,
    last_mpris_length_micros: Cell<Option<i64>>,
    content_dimmer: GtkBox,
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
    repeat_mode: Cell<RepeatMode>,
    // Queue / Radio
    queue: RefCell<Vec<Track>>,
    queue_index: Cell<usize>,
    radio_virtual_playlist: RefCell<Option<RadioVirtualPlaylist>>,
    radio_append_in_flight: Cell<bool>,
    last_radio_append_anchor: RefCell<Option<String>>,
    queue_drag_source_index: Cell<Option<usize>>,
    queue_drop_preview_slot: Cell<Option<usize>>,
    queue_drag_preview_payload: RefCell<Option<QueueDragPayload>>,
    queue_drop_indicator: GtkBox,
    queue_drop_indicator_thumb: Picture,
    queue_drop_indicator_title: Label,
    queue_drop_indicator_subtitle: Label,
    queue_drop_indicator_duration: Label,
    sponsor_segments: RefCell<Vec<SponsorSegment>>,
    sponsor_segments_reported: RefCell<HashSet<String>>,
    daemon_refresh_in_flight: Cell<bool>,
    // Queue panel widgets
    queue_list_box: GtkBox,
    chip_bar: GtkBox,
    queue_panel: GtkBox,
    queue_dismiss_btn: Button,
    // Play tracking — records listen time when navigating away
    now_playing_track: RefCell<Option<NowPlayingTrack>>,
    playback_session_id: Cell<u64>,
}

struct MainPageWidgets {
    page: GtkBox,
    content_box: GtkBox,
    content_scroll: ScrolledWindow,
    player_state: Rc<PlayerState>,
    recent_list: GtkBox,
    home_btn: Button,
    home_pagination: Rc<HomePaginationState>,
}

#[derive(Debug, Clone)]
enum DirectMusicUrlTarget {
    Video {
        video_id: String,
    },
    Browse {
        browse_id: String,
        page_type: HomeBrowsePageType,
    },
}

struct HomePaginationState {
    continuation: RefCell<Option<String>>,
    loading: Cell<bool>,
}

enum BrowseDetailData {
    Tracks(Vec<Track>),
    Sections(BrowseSectionsPage),
}

struct ArtistSections<'a> {
    top_songs: Option<&'a HomeSection>,
    remaining: Vec<&'a HomeSection>,
}

#[derive(Debug, Clone)]
struct QueuePreviewData {
    title: String,
    subtitle: String,
    thumbnail_url: Option<String>,
    duration: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum QueueDragPayload {
    QueueRow {
        index: usize,
    },
    Track {
        track: Track,
    },
    Playlist {
        playlist_id: String,
        title: String,
        thumbnail_url: Option<String>,
        subtitle: Option<String>,
        source: QueuePlaylistSource,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum QueuePlaylistSource {
    Remote,
    Offline,
}

fn sync_mpris_transport(ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    if let Ok(snapshot) = ps.player.replace_queue(
        ps.queue.borrow().clone(),
        ps.queue_index.get(),
        ps.radio_virtual_playlist.borrow().clone(),
        ps.repeat_mode.get(),
    ) {
        apply_remote_snapshot(ps, state, snapshot);
    }
}

fn sync_mpris_position(_ps: &Rc<PlayerState>) {}

fn sync_mpris_metadata(_ps: &Rc<PlayerState>, _state: &Arc<Mutex<AppState>>) {}

fn sync_mpris_volume(ps: &Rc<PlayerState>) {
    let _ = ps.player.set_volume(ps.volume_slider.value());
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

fn store_storage_root(root: &str) -> Result<(), keyring::Error> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_STORAGE_ROOT)?;
    entry.set_password(root)
}

fn get_storage_root() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_STORAGE_ROOT).ok()?;
    entry.get_password().ok()
}

fn delete_storage_root() {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_STORAGE_ROOT) {
        let _ = entry.delete_credential();
    }
}

fn app_settings(state: &Arc<Mutex<AppState>>) -> AppSettings {
    state.lock().unwrap().settings.clone()
}

fn sync_remote_queue(ps: &Rc<PlayerState>) {
    let _ = ps.player.replace_queue(
        ps.queue.borrow().clone(),
        ps.queue_index.get(),
        ps.radio_virtual_playlist.borrow().clone(),
        ps.repeat_mode.get(),
    );
}

fn apply_remote_snapshot(
    ps: &Rc<PlayerState>,
    state: &Arc<Mutex<AppState>>,
    snapshot: PlaybackSnapshot,
) {
    let queue_changed = {
        let queue = ps.queue.borrow();
        *queue != snapshot.queue || ps.queue_index.get() != snapshot.queue_index
    };
    if queue_changed {
        *ps.queue.borrow_mut() = snapshot.queue.clone();
        ps.queue_index.set(snapshot.queue_index);
    }

    if *ps.radio_virtual_playlist.borrow() != snapshot.radio_virtual_playlist {
        let is_radio = snapshot.radio_virtual_playlist.is_some();
        *ps.radio_virtual_playlist.borrow_mut() = snapshot.radio_virtual_playlist.clone();
        if !is_radio {
            ps.radio_append_in_flight.set(false);
            *ps.last_radio_append_anchor.borrow_mut() = None;
        }
    }

    if ps.repeat_mode.get() != snapshot.repeat_mode {
        ps.repeat_mode.set(snapshot.repeat_mode);
        update_repeat_button_ui(&ps.repeat_btn, snapshot.repeat_mode);
    }

    let previous_video_id = ps
        .now_playing_track
        .borrow()
        .as_ref()
        .map(|track| track.video_id.clone());

    match snapshot.current_track {
        Some(track) => {
            let is_new_track = previous_video_id.as_deref() != Some(track.video_id.as_str());
            let persisted_rating = ps
                .now_playing_track
                .borrow()
                .as_ref()
                .filter(|current| current.video_id == track.video_id)
                .map(|current| current.rating)
                .unwrap_or(track.rating);
            let session_id = if is_new_track {
                let next = ps.playback_session_id.get().wrapping_add(1);
                ps.playback_session_id.set(next);
                clear_sponsorblock_state(ps);
                next
            } else {
                ps.now_playing_track
                    .borrow()
                    .as_ref()
                    .map(|current| current.session_id)
                    .unwrap_or_else(|| {
                        let next = ps.playback_session_id.get().wrapping_add(1);
                        ps.playback_session_id.set(next);
                        next
                    })
            };

            *ps.now_playing_track.borrow_mut() = Some(NowPlayingTrack {
                session_id,
                video_id: track.video_id.clone(),
                rating: persisted_rating,
                last_observed_position_millis: snapshot.position_millis.unwrap_or(0),
            });

            ps.np_title.set_text(&track.title);
            ps.np_artist.set_text(&track.artist);
            if let Some(url) = track.thumb_url.as_deref() {
                load_thumbnail_into(&ps.np_thumb, url, 80);
            } else {
                ps.np_thumb.set_paintable(None::<&gtk::gdk::Texture>);
            }

            let playing = !snapshot.paused;
            set_button_icon(
                &ps.play_pause_btn,
                if playing { ICON_PAUSE } else { ICON_PLAY },
                28,
            );
            ps.play_pause_btn.set_sensitive(true);
            ps.prev_btn.set_sensitive(!snapshot.queue.is_empty());
            ps.next_btn.set_sensitive(!snapshot.queue.is_empty());
            ps.like_btn.set_sensitive(true);
            ps.dislike_btn.set_sensitive(true);
            ps.menu_btn.set_sensitive(true);
            update_rating_buttons_ui(&ps.like_btn, &ps.dislike_btn, persisted_rating);

            if is_new_track {
                spawn_sponsorblock_sync(state, ps, &track.video_id, session_id);
                spawn_song_rating_sync(state, ps, &track.video_id, session_id);
            }
        }
        None => {
            ps.now_playing_track.borrow_mut().take();
            clear_sponsorblock_state(ps);
            ps.last_mpris_length_micros.set(None);
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
            update_rating_buttons_ui(&ps.like_btn, &ps.dislike_btn, SongRating::Indifferent);
        }
    }

    if (ps.volume_slider.value() - snapshot.volume).abs() > 0.01 {
        ps.volume_slider.set_value(snapshot.volume);
    }
    if queue_changed {
        render_queue_panel(ps, state);
    }
}

fn update_app_settings<F>(state: &Arc<Mutex<AppState>>, updater: F)
where
    F: FnOnce(&mut AppSettings),
{
    let mut guard = state.lock().unwrap();
    let mut settings = guard.settings.clone();
    updater(&mut settings);
    if let Err(err) = guard.cache.save_settings(&settings) {
        eprintln!("[SETTINGS] failed to persist settings: {err}");
        return;
    }
    guard.settings = settings;
    drop(guard);
    crate::player::notify_daemon_settings_changed();
}

fn persist_and_sync_settings<F>(state: &Arc<Mutex<AppState>>, updater: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut AppSettings),
{
    {
        let mut guard = state.lock().unwrap();
        let mut settings = guard.settings.clone();
        updater(&mut settings);
        guard.cache.save_settings(&settings)?;
        guard.settings = settings;
    }
    crate::player::notify_daemon_settings_changed();
    sync_offline_cache(state);
    Ok(())
}

fn preview_auto_cache_evictions<F>(
    state: &Arc<Mutex<AppState>>,
    updater: F,
) -> anyhow::Result<usize>
where
    F: FnOnce(&mut AppSettings),
{
    let guard = state.lock().unwrap();
    let mut settings = guard.settings.clone();
    updater(&mut settings);
    Ok(guard.cache.auto_download_eviction_count(&settings)?)
}

fn build_settings_warning_dialog(
    stack: &Stack,
    title: &str,
    secondary_text: &str,
    accept_label: &str,
) -> MessageDialog {
    let dialog = MessageDialog::builder()
        .modal(true)
        .message_type(MessageType::Warning)
        .text(title)
        .secondary_text(secondary_text)
        .build();
    dialog.add_css_class("settings-warning-dialog");
    if let Some(window) = stack
        .root()
        .and_then(|root| root.downcast::<ApplicationWindow>().ok())
    {
        dialog.set_transient_for(Some(&window));
    }
    dialog.add_button("Cancel", ResponseType::Cancel);
    dialog.add_button(accept_label, ResponseType::Accept);
    dialog.set_default_response(ResponseType::Cancel);
    dialog
}

fn persist_storage_root_override(
    state: &Arc<Mutex<AppState>>,
    raw_root: &str,
) -> anyhow::Result<()> {
    let trimmed = raw_root.trim().to_string();
    let mut guard = state.lock().unwrap();
    guard.cache.migrate_storage_root(&trimmed)?;
    if trimmed.is_empty() {
        delete_storage_root();
    } else {
        store_storage_root(&trimmed)?;
    }
    Ok(())
}

fn candidate_chromium_cookie_paths() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".config/microsoft-edge"));
        roots.push(home.join(".config/microsoft-edge-beta"));
        roots.push(home.join(".config/microsoft-edge-dev"));
        roots.push(home.join(".config/microsoft-edge-nightly"));
        roots.push(home.join(".config/google-chrome"));
        roots.push(home.join(".config/google-chrome-beta"));
        roots.push(home.join(".config/google-chrome-unstable"));
        roots.push(home.join(".config/chromium"));
        roots.push(home.join(".config/BraveSoftware/Brave-Browser"));
        roots.push(home.join(".config/vivaldi"));
        roots.push(home.join(".config/opera"));
        roots.push(home.join(".var/app/com.microsoft.Edge/config/microsoft-edge"));
        roots.push(home.join(".var/app/com.microsoft.Edge/config/microsoft-edge-beta"));
        roots.push(home.join(".var/app/com.microsoft.Edge/config/microsoft-edge-dev"));
        roots.push(home.join(".var/app/com.microsoft.Edge/config/microsoft-edge-nightly"));
        roots.push(home.join(".var/app/com.google.Chrome/config/google-chrome"));
        roots.push(home.join(".var/app/org.chromium.Chromium/config/chromium"));
    }

    let mut candidates = Vec::new();
    for root in roots {
        candidates.push(root.join("Default/Cookies"));
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("Profile "))
                {
                    candidates.push(path.join("Cookies"));
                }
            }
        }
    }

    candidates
}

fn newest_existing_path(paths: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    paths
        .into_iter()
        .filter(|path| path.is_file())
        .max_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        })
}

fn chromium_safe_storage_password() -> anyhow::Result<String> {
    let connection = ZbusConnection::session().context("Failed to open session bus")?;

    for schema in [
        "chrome_libsecret_os_crypt_password_v2",
        "chrome_libsecret_os_crypt_password_v1",
    ] {
        for application in [
            "chromium",
            "chrome",
            "google-chrome",
            "microsoft-edge",
            "Brave",
            "brave",
            "vivaldi",
            "opera",
            "Code",
        ] {
            let mut attrs = HashMap::<&str, &str>::new();
            attrs.insert("xdg:schema", schema);
            attrs.insert("application", application);

            let search = connection.call_method(
                Some("org.freedesktop.secrets"),
                "/org/freedesktop/secrets",
                Some("org.freedesktop.Secret.Service"),
                "SearchItems",
                &attrs,
            )?;
            let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
                search.body().deserialize()?;

            let object_path = if let Some(path) = unlocked.first() {
                path.clone()
            } else if let Some(path) = locked.first() {
                let unlock = connection.call_method(
                    Some("org.freedesktop.secrets"),
                    "/org/freedesktop/secrets",
                    Some("org.freedesktop.Secret.Service"),
                    "Unlock",
                    &(vec![path.clone()]),
                )?;
                let (unlocked_paths, _prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
                    unlock.body().deserialize()?;
                if let Some(unlocked_path) = unlocked_paths.first() {
                    unlocked_path.clone()
                } else {
                    continue;
                }
            } else {
                continue;
            };

            let open_session = connection.call_method(
                Some("org.freedesktop.secrets"),
                "/org/freedesktop/secrets",
                Some("org.freedesktop.Secret.Service"),
                "OpenSession",
                &("plain", Value::new("")),
            )?;
            let (_output, session): (OwnedValue, OwnedObjectPath) =
                open_session.body().deserialize()?;

            let secrets = connection.call_method(
                Some("org.freedesktop.secrets"),
                "/org/freedesktop/secrets",
                Some("org.freedesktop.Secret.Service"),
                "GetSecrets",
                &(vec![object_path.clone()], session),
            )?;
            let reply: HashMap<OwnedObjectPath, SecretResponse> = secrets.body().deserialize()?;
            if let Some(secret) = reply.get(&object_path).map(|entry| &entry.2) {
                return String::from_utf8(secret.clone())
                    .context("Invalid Chromium secret encoding");
            }
        }
    }

    Err(anyhow::anyhow!("No Chromium safe-storage secret found"))
}

fn chromium_cookie_key(password: &str) -> [u8; 16] {
    let mut output = [0u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), b"saltysalt", 1, &mut output);
    output
}

fn decrypt_chromium_cookie(
    value: String,
    encrypted_value: Vec<u8>,
    keys: &[[u8; 16]],
) -> Option<String> {
    if !value.is_empty() {
        return Some(value);
    }
    if encrypted_value.len() < 4 {
        return None;
    }

    let prefix = &encrypted_value[..3];
    if prefix != b"v10" && prefix != b"v11" && prefix != b"v20" {
        return None;
    }

    let payload = &encrypted_value[3..];
    let iv = [b' '; 16];

    for key in keys {
        let cipher = Aes128CbcDec::new(&(*key).into(), &iv.into());
        let mut buffer = payload.to_vec();
        if let Ok(plaintext) = cipher.decrypt_padded_mut::<Pkcs7>(&mut buffer) {
            if let Ok(text) = String::from_utf8(plaintext.to_vec()) {
                return Some(text);
            }
            if plaintext.len() > 32 {
                if let Ok(text) = String::from_utf8(plaintext[32..].to_vec()) {
                    return Some(text);
                }
            }
        }
    }

    None
}

fn load_cookie_from_chromium_profile(settings: &AppSettings) -> anyhow::Result<Option<String>> {
    let db_path = if settings.chromium_cookie_path.trim().is_empty() {
        match newest_existing_path(candidate_chromium_cookie_paths()) {
            Some(path) => path,
            None => return Ok(None),
        }
    } else {
        let path = PathBuf::from(settings.chromium_cookie_path.trim());
        if !path.is_file() {
            return Ok(None);
        }
        path
    };

    let temp_db =
        std::env::temp_dir().join(format!("static-edge-cookies-{}.db", uuid::Uuid::new_v4()));
    std::fs::copy(&db_path, &temp_db)
        .with_context(|| format!("Failed to copy Edge cookie DB from {}", db_path.display()))?;

    let result = (|| -> anyhow::Result<Option<String>> {
        let password = chromium_safe_storage_password()?;
        let keys = [
            chromium_cookie_key(&password),
            chromium_cookie_key("peanuts"),
            chromium_cookie_key(""),
        ];

        let conn = SqliteConnection::open(&temp_db)?;
        let mut stmt = conn.prepare(
            "SELECT host_key, name, value, CAST(encrypted_value AS BLOB)
             FROM cookies
             WHERE host_key LIKE '%youtube.com%'
             ORDER BY CASE WHEN host_key LIKE '%music.youtube.com%' THEN 0 ELSE 1 END, host_key DESC, name ASC",
        )?;

        let mut rows = stmt.query([])?;
        let mut seen = HashSet::new();
        let mut cookie_pairs = Vec::new();

        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            let value: String = row.get(2)?;
            let encrypted_value: Vec<u8> = row.get(3)?;
            let Some(decrypted) = decrypt_chromium_cookie(value, encrypted_value, &keys) else {
                continue;
            };
            if decrypted.is_empty() || !seen.insert(name.clone()) {
                continue;
            }
            cookie_pairs.push((name, decrypted));
        }

        if cookie_pairs.is_empty() {
            return Ok(None);
        }

        Ok(Some(
            cookie_pairs
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; "),
        ))
    })();

    let _ = std::fs::remove_file(&temp_db);
    result
}

async fn validate_cookie_and_build_client(
    cookie: String,
    source_label: &str,
) -> Option<Arc<YtMusicClient>> {
    let client = match YtMusicClient::new(&cookie) {
        Ok(client) => Arc::new(client),
        Err(err) => {
            eprintln!("[AUTH] {source_label}: invalid cookie: {err}");
            return None;
        }
    };

    let check_client = client.clone();
    let auth_result = gio::spawn_blocking(move || check_client.check_auth()).await;
    match auth_result {
        Ok(Ok(true)) => Some(client),
        Ok(Ok(false)) => {
            eprintln!("[AUTH] {source_label}: cookie is not authenticated");
            None
        }
        Ok(Err(err)) => {
            eprintln!("[AUTH] {source_label}: auth check failed: {err}");
            None
        }
        Err(err) => {
            eprintln!("[AUTH] {source_label}: auth worker failed: {err:?}");
            None
        }
    }
}

async fn activate_authenticated_client(
    client: Arc<YtMusicClient>,
    state: &Arc<Mutex<AppState>>,
    stack: &Stack,
) {
    if let Ok(Some(vr_token)) = gio::spawn_blocking(get_vr_refresh_token).await {
        client.set_vr_refresh_token(&vr_token);
    }
    {
        let mut guard = state.lock().unwrap();
        guard.client = Some(client);
        guard.home_cache = None;
    }
    stack.set_visible_child_name("main");
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
    let mut cache = cache;
    let settings = cache.load_settings().unwrap_or_else(|err| {
        eprintln!("[SETTINGS] failed to load persisted settings: {err}");
        AppSettings::default()
    });
    if let Some(root) = get_storage_root() {
        if let Err(err) = cache.migrate_storage_root(&root) {
            eprintln!("[SETTINGS] failed to apply storage root override: {err}");
            let _ = cache.apply_storage_root("");
        }
    }
    let state: Arc<Mutex<AppState>> = Arc::new(Mutex::new(AppState {
        client: None,
        sponsorblock: SponsorBlockClient::new().expect("Failed to initialise SponsorBlock client"),
        cache,
        settings,
        home_cache: None,
        home_scroll_position: 0.0,
    }));

    let onboarding = build_onboarding_page(&stack, &state);
    let vr_auth = build_vr_auth_page(&stack, &state);
    let main_widgets = build_main_page(&stack, &state);

    stack.add_named(&onboarding, Some("onboarding"));
    stack.add_named(&vr_auth, Some("vr_auth"));
    stack.add_named(&main_widgets.page, Some("main"));
    stack.set_visible_child_name("onboarding");

    {
        let ps = main_widgets.player_state.clone();
        window.connect_close_request(move |_| {
            let snapshot = ps.player.snapshot();
            if snapshot.paused && snapshot.current_track.is_some() {
                let client = ps.player.daemon_client();
                if let Err(err) = client.shutdown() {
                    eprintln!(
                        "[STATIC] failed to stop paused staticd while closing window: {err}"
                    );
                }
            }
            glib::Propagation::Proceed
        });
    }

    window.set_child(Some(&stack));

    {
        let state = state.clone();
        let stack = stack.clone();
        let content_box = main_widgets.content_box.clone();
        let content_scroll = main_widgets.content_scroll.clone();
        let home_pagination = main_widgets.home_pagination.clone();
        let ps = main_widgets.player_state.clone();
        let recent_list = main_widgets.recent_list.clone();
        let home_btn = main_widgets.home_btn.clone();
        let refresh = gtk::EventControllerKey::new();
        refresh.connect_key_pressed(move |_, key, _, modifiers| {
            let wants_refresh = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && (key == gtk::gdk::Key::r || key == gtk::gdk::Key::R);
            if wants_refresh
                && stack.visible_child_name().as_deref() == Some("main")
                && home_btn.has_css_class("nav-item-active")
            {
                invalidate_home_cache(&state);
                load_home(
                    &state,
                    &content_box,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                    true,
                );
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(refresh);
    }

    window.present();

    // Auto-load home + history when main page becomes visible
    {
        let state = state.clone();
        let content_box = main_widgets.content_box.clone();
        let content_scroll = main_widgets.content_scroll.clone();
        let home_pagination = main_widgets.home_pagination.clone();
        let ps = main_widgets.player_state.clone();
        let recent_list = main_widgets.recent_list.clone();

        stack.connect_notify_local(Some("visible-child-name"), move |stack, _| {
            if stack.visible_child_name().as_deref() == Some("main") && app_client(&state).is_some()
            {
                clear_box(&content_box);
                load_home(
                    &state,
                    &content_box,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                    false,
                );
                load_sidebar_history(&state, &recent_list, &ps);
            }
        });
    }

    // Keep sidebar history fresh while the live app is open so
    // plays from other devices/clients eventually show up too.
    {
        let stack = stack.clone();
        let state = state.clone();
        let recent_list = main_widgets.recent_list.clone();
        let ps = main_widgets.player_state.clone();

        glib::timeout_add_seconds_local(60, move || {
            if stack.visible_child_name().as_deref() == Some("main") && app_client(&state).is_some()
            {
                load_sidebar_history(&state, &recent_list, &ps);
            }
            glib::ControlFlow::Continue
        });
    }

    // Check keyring on startup
    {
        let stack = stack.clone();
        let state = state.clone();
        glib::spawn_future_local(async move {
            if let Ok(Some(cookie)) = gio::spawn_blocking(get_cookie).await {
                if let Some(client) =
                    validate_cookie_and_build_client(cookie, "keyring cookie").await
                {
                    activate_authenticated_client(client, &state, &stack).await;
                    return;
                }
                let _ = gio::spawn_blocking(delete_cookie).await;
            }

            let settings = app_settings(&state);
            if !settings.auto_sync_chromium_cookies {
                return;
            }

            match gio::spawn_blocking(move || load_cookie_from_chromium_profile(&settings)).await {
                Ok(Ok(Some(edge_cookie))) => {
                    if let Some(client) = validate_cookie_and_build_client(
                        edge_cookie.clone(),
                        "Chromium cookie auto-sync",
                    )
                    .await
                    {
                        let cookie_to_store = edge_cookie.clone();
                        let _ = gio::spawn_blocking(move || store_cookie(&cookie_to_store)).await;
                        activate_authenticated_client(client, &state, &stack).await;
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(err)) => {
                    eprintln!("[AUTH] Chromium cookie auto-sync failed: {err}");
                }
                Err(err) => {
                    eprintln!("[AUTH] Chromium cookie auto-sync worker failed: {err:?}");
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
                        })
                        .await;

                        match result {
                            Ok(Ok(true)) | Ok(Ok(false)) => {
                                let cookie_for_save = cookie_text.clone();
                                let _ = gio::spawn_blocking(move || {
                                    let _ = store_cookie(&cookie_for_save);
                                })
                                .await;
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
    let search_entry = Entry::builder().placeholder_text("Search songs...").build();
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
    modes_box.append(&repeat_btn);

    controls_box.append(&modes_box);

    np_bar.append(&controls_box);
    content_outer.append(&np_bar);

    hbox.append(&content_outer);

    // ======= QUEUE PANEL (right side, like YTM's "Up Next") =======
    let queue_panel = GtkBox::new(Orientation::Vertical, 0);
    queue_panel.add_css_class("queue-panel");
    queue_panel.set_visible(false);

    let queue_header_row = GtkBox::new(Orientation::Horizontal, 8);
    queue_header_row.set_margin_start(16);
    queue_header_row.set_margin_end(16);
    queue_header_row.set_margin_top(12);
    queue_header_row.set_margin_bottom(8);

    let queue_header = Label::new(Some("Up Next"));
    queue_header.add_css_class("queue-header");
    queue_header.set_halign(Align::Start);
    queue_header.set_hexpand(true);
    queue_header_row.append(&queue_header);

    let dismiss_queue_btn = Button::with_label("Dismiss Queue");
    dismiss_queue_btn.add_css_class("settings-secondary-btn");
    dismiss_queue_btn.set_halign(Align::End);
    dismiss_queue_btn.set_sensitive(false);
    queue_header_row.append(&dismiss_queue_btn);

    queue_panel.append(&queue_header_row);

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
    queue_panel.append(&queue_overlay);

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
        np_artist: np_artist.clone(),
        dislike_btn: dislike_btn.clone(),
        like_btn: like_btn.clone(),
        menu_btn: menu_btn.clone(),
        volume_slider: volume_slider.clone(),
        repeat_btn: repeat_btn.clone(),
        repeat_mode: Cell::new(RepeatMode::Off),
        queue: RefCell::new(Vec::new()),
        queue_index: Cell::new(0),
        radio_virtual_playlist: RefCell::new(None),
        radio_append_in_flight: Cell::new(false),
        last_radio_append_anchor: RefCell::new(None),
        queue_drag_source_index: Cell::new(None),
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
        queue_list_box: queue_list_box.clone(),
        chip_bar: chip_bar.clone(),
        queue_panel: queue_panel.clone(),
        queue_dismiss_btn: dismiss_queue_btn.clone(),
        now_playing_track: RefCell::new(None),
        playback_session_id: Cell::new(0),
    });

    {
        let ps = ps.clone();
        let state = state.clone();
        dismiss_queue_btn.connect_clicked(move |_| {
            dismiss_queue_after_current(&ps, &state);
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
            gtk::gdk::DragAction::MOVE
        });

        let ps_motion = ps.clone();
        drop_target.connect_motion(move |_, _, y| {
            set_queue_drop_active(&ps_motion, true);
            let raw_slot = preview_slot_for_queue_y(&ps_motion, y);
            let preview_slot = normalized_preview_slot(&ps_motion, raw_slot);
            set_queue_drop_preview(&ps_motion, preview_slot);
            gtk::gdk::DragAction::MOVE
        });

        let ps_leave = ps.clone();
        drop_target.connect_leave(move |_| {
            clear_queue_drop_preview(&ps_leave);
            set_queue_drop_active(&ps_leave, false);
        });

        let ps_drop = ps.clone();
        drop_target.connect_drop(move |_, value, _, y| {
            let Some(payload) = parse_queue_drag_payload(value) else {
                return false;
            };
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
            if let Ok(snapshot) = ps.player.next() {
                apply_remote_snapshot(&ps, &state, snapshot);
            }
        });
    }

    // Previous track
    {
        let ps = ps.clone();
        let state = state.clone();
        prev_btn.connect_clicked(move |_| {
            if let Ok(snapshot) = ps.player.previous() {
                apply_remote_snapshot(&ps, &state, snapshot);
            }
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
        glib::timeout_add_local(std::time::Duration::from_millis(400), move || {
            if ps.daemon_refresh_in_flight.replace(true) {
                return glib::ControlFlow::Continue;
            }

            let ps_for_ui = ps.clone();
            let state_for_ui = state.clone();
            let client = ps.player.daemon_client();
            glib::spawn_future_local(async move {
                let snapshot = gio::spawn_blocking(move || client.state()).await;
                ps_for_ui.daemon_refresh_in_flight.set(false);

                let Ok(Ok(snapshot)) = snapshot else {
                    return;
                };

                ps_for_ui.player.apply_snapshot(snapshot.clone());
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
                    note_playback_position(&ps_for_ui, pos.nseconds() / 1_000_000);
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
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let nav = nav_buttons.clone();

        offline_btn.connect_clicked(move |btn| {
            set_active_nav(btn, &nav);
            load_offline(&state, &content_box, &ps, &recent_list);
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

        search_entry.connect_activate(move |entry| {
            let query = entry.text().trim().to_string();
            if query.is_empty() {
                return;
            }
            if let Some(target) = parse_direct_music_url(&query) {
                autoplay_direct_music_url(
                    &state,
                    &content_box,
                    &content_scroll,
                    &home_pagination,
                    &query,
                    target,
                    &ps,
                    &recent_list,
                );
            } else {
                load_search(&state, &content_box, &query, &ps, &recent_list);
            }
        });
    }

    MainPageWidgets {
        page,
        content_box,
        content_scroll: scrolled,
        player_state: ps,
        recent_list,
        home_btn,
        home_pagination,
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

fn set_active_nav(active: &Button, all: &[Button]) {
    for btn in all {
        btn.remove_css_class("nav-item-active");
    }
    active.add_css_class("nav-item-active");
}

fn load_settings_page(
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
        "Control how opinionated the automatic offline cache should be.",
    ));
    offline_copy.add_css_class("settings-card-copy");
    offline_copy.set_wrap(true);
    offline_copy.set_halign(Align::Start);
    offline_section.append(&offline_copy);

    let manage_row = GtkBox::new(Orientation::Horizontal, 12);
    manage_row.add_css_class("settings-row");
    let manage_copy = GtkBox::new(Orientation::Vertical, 2);
    manage_copy.set_hexpand(true);
    let manage_label = Label::new(Some("Auto-manage offline cache"));
    manage_label.add_css_class("settings-row-title");
    manage_label.set_halign(Align::Start);
    manage_copy.append(&manage_label);
    let manage_hint = Label::new(Some(
        "Keeps a rolling offline library based on your play history, without touching pinned downloads.",
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
        let guard_flag = Rc::new(Cell::new(false));
        let guard_flag_closure = guard_flag.clone();
        manage_switch.connect_active_notify(move |sw| {
            if guard_flag_closure.get() {
                return;
            }

            let active = sw.is_active();
            if active {
                let state = state.clone();
                gio::spawn_blocking(move || {
                    let _ = persist_and_sync_settings(&state, |s| s.auto_manage_offline_cache = true);
                });
                return;
            }

            guard_flag_closure.set(true);
            sw.set_active(true);
            guard_flag_closure.set(false);

            let state = state.clone();
            let sw = sw.clone();
            let guard_flag = guard_flag.clone();
            let stack_for_dialog = stack.clone();
            glib::spawn_future_local(async move {
                let impact = gio::spawn_blocking({
                    let state = state.clone();
                    move || {
                        let guard = state.lock().unwrap();
                        let cached = guard.cache.cached_ids().unwrap_or_default();
                        let pinned = guard.cache.pinned_ids().unwrap_or_default();
                        cached
                            .into_iter()
                            .filter(|video_id| !pinned.contains(video_id))
                            .count()
                    }
                })
                .await
                .unwrap_or(0);

                let song_label = if impact == 1 { "song" } else { "songs" };
                let dialog = build_settings_warning_dialog(
                    &stack_for_dialog,
                    "Turn off local cache?",
                    &format!(
                        "{impact} auto-downloaded {song_label} will be deleted from Static’s local cache. Manually pinned downloads stay right where they are."
                    ),
                    &format!("Delete {impact} auto-downloaded {song_label}"),
                );

                let response = dialog.run_future().await;
                dialog.close();

                if response != ResponseType::Accept {
                    guard_flag.set(true);
                    sw.set_active(true);
                    guard_flag.set(false);
                    return;
                }

                match gio::spawn_blocking(move || -> anyhow::Result<usize> {
                    let mut guard = state.lock().unwrap();
                    let removed = guard.cache.clear_auto_downloads()?;
                    let mut next_settings = guard.settings.clone();
                    next_settings.auto_manage_offline_cache = false;
                    guard.cache.save_settings(&next_settings)?;
                    guard.settings = next_settings;
                    Ok(removed)
                })
                .await
                {
                    Ok(Ok(removed)) => {
                        eprintln!("[CACHE] removed {removed} auto-downloaded songs after disabling local cache");
                        guard_flag.set(true);
                        sw.set_active(false);
                        guard_flag.set(false);
                    }
                    Ok(Err(err)) => {
                        eprintln!("[SETTINGS] failed to disable local cache: {err}");
                        guard_flag.set(true);
                        sw.set_active(true);
                        guard_flag.set(false);
                    }
                    Err(err) => {
                        eprintln!("[SETTINGS] local cache disable worker failed: {err:?}");
                        guard_flag.set(true);
                        sw.set_active(true);
                        guard_flag.set(false);
                    }
                }
            });
        });
    }
    manage_row.append(&manage_switch);
    offline_section.append(&manage_row);

    let listen_again_row = GtkBox::new(Orientation::Horizontal, 12);
    listen_again_row.add_css_class("settings-row");
    let listen_again_copy = GtkBox::new(Orientation::Vertical, 2);
    listen_again_copy.set_hexpand(true);
    let listen_again_label = Label::new(Some("Cache Listen again picks"));
    listen_again_label.add_css_class("settings-row-title");
    listen_again_label.set_halign(Align::Start);
    listen_again_copy.append(&listen_again_label);
    let listen_again_hint = Label::new(Some(
        "Also keep songs from Listen again offline. Static hangs onto them for up to 30 days, and removes older ones once they have not been played in that window.",
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
                        "Turn off Listen again auto-cache?",
                        &format!(
                            "{impact} auto-downloaded {song_label} would be evicted immediately because they only qualify through Listen again right now."
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

    let target_row = GtkBox::new(Orientation::Horizontal, 12);
    target_row.add_css_class("settings-row");
    let target_copy = GtkBox::new(Orientation::Vertical, 2);
    target_copy.set_hexpand(true);
    let target_label = Label::new(Some("Auto-cache target"));
    target_label.add_css_class("settings-row-title");
    target_label.set_halign(Align::Start);
    target_copy.append(&target_label);
    let target_hint = Label::new(Some(
        "How many frequently played songs Static should keep ready offline.",
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
                        "Lower the auto-cache target?",
                        &format!(
                            "{impact} auto-downloaded {song_label} would be evicted immediately to honor the smaller offline library size."
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
        "How many rewatches a song needs before it becomes eligible for auto-offline caching.",
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
                            "{impact} auto-downloaded {song_label} would be evicted immediately because they no longer qualify for offline caching."
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

fn svg_image(svg_bytes: &[u8], size: i32) -> Image {
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

fn icon_button(svg_bytes: &[u8], size: i32, css_class: &str) -> Button {
    let btn = Button::new();
    btn.set_child(Some(&svg_image(svg_bytes, size)));
    btn.add_css_class(css_class);
    btn
}

fn settings_switch(active: bool) -> Switch {
    let sw = Switch::new();
    sw.set_active(active);
    sw.set_valign(Align::Center);
    sw.set_halign(Align::End);
    sw
}

fn set_button_icon(btn: &Button, svg_bytes: &[u8], size: i32) {
    btn.set_child(Some(&svg_image(svg_bytes, size)));
}

fn update_repeat_button_ui(btn: &Button, mode: RepeatMode) {
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

fn set_content_dimming(ps: &PlayerState, dimmed: bool) {
    ps.content_dimmer.set_visible(dimmed);
}

fn update_rating_buttons_ui(like_btn: &Button, dislike_btn: &Button, rating: SongRating) {
    like_btn.remove_css_class("like-active");
    dislike_btn.remove_css_class("dislike-active");

    match rating {
        SongRating::Like => like_btn.add_css_class("like-active"),
        SongRating::Dislike => dislike_btn.add_css_class("dislike-active"),
        SongRating::Indifferent => {}
    }
}

fn current_rating(ps: &PlayerState) -> SongRating {
    ps.now_playing_track
        .borrow()
        .as_ref()
        .map(|track| track.rating)
        .unwrap_or(SongRating::Indifferent)
}

fn is_radio_queue(ps: &PlayerState) -> bool {
    ps.radio_virtual_playlist.borrow().is_some()
}

fn radio_chips(ps: &PlayerState) -> Vec<RadioChip> {
    ps.radio_virtual_playlist
        .borrow()
        .as_ref()
        .map(|radio| radio.chips.clone())
        .unwrap_or_default()
}

fn radio_source_video_id(ps: &PlayerState) -> Option<String> {
    ps.radio_virtual_playlist
        .borrow()
        .as_ref()
        .map(|radio| radio.source_video_id.clone())
}

fn set_queue_context(ps: &PlayerState, radio: Option<RadioVirtualPlaylist>) {
    *ps.radio_virtual_playlist.borrow_mut() = radio;
    ps.radio_append_in_flight.set(false);
    *ps.last_radio_append_anchor.borrow_mut() = None;
}

fn mark_radio_touched_through(ps: &PlayerState, index: usize) {
    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        radio.touched_len = radio.touched_len.max(index.saturating_add(1));
    }
}

fn extend_radio_touched_for_insert(ps: &PlayerState, insert_at: usize, added: usize) {
    if added == 0 {
        return;
    }

    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        let touched_len = radio.touched_len;
        let inserted_end = insert_at + added;
        if insert_at < touched_len {
            radio.touched_len = touched_len + added;
        } else {
            radio.touched_len = touched_len.max(inserted_end);
        }
    }
}

fn shrink_radio_touched_for_remove(ps: &PlayerState, remove_at: usize) {
    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        if radio.touched_len == 0 {
            return;
        }

        if remove_at < radio.touched_len {
            radio.touched_len = radio.touched_len.saturating_sub(1);
        }
    }
}

fn radio_touched_len(ps: &PlayerState) -> Option<usize> {
    ps.radio_virtual_playlist
        .borrow()
        .as_ref()
        .map(|radio| radio.touched_len)
}

fn replace_radio_tail(
    ps: &PlayerState,
    prefix_len: usize,
    tracks: Vec<Track>,
    chips: Vec<RadioChip>,
) {
    let prefix_len = prefix_len.min(ps.queue.borrow().len());
    {
        let mut queue = ps.queue.borrow_mut();
        queue.truncate(prefix_len);

        let existing_ids = queue
            .iter()
            .map(|track| track.video_id.clone())
            .collect::<HashSet<_>>();
        let mut seen_ids = existing_ids;
        for track in tracks {
            if seen_ids.insert(track.video_id.clone()) {
                queue.push(track);
            }
        }
    }

    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        radio.touched_len = radio.touched_len.min(prefix_len);
        if !chips.is_empty() {
            radio.chips = chips;
        }
    }
    *ps.last_radio_append_anchor.borrow_mut() = None;
}

fn append_radio_recommendations(ps: &PlayerState, tracks: Vec<Track>) -> usize {
    let mut added = 0usize;
    let mut queue = ps.queue.borrow_mut();
    let mut seen_ids = queue
        .iter()
        .map(|track| track.video_id.clone())
        .collect::<HashSet<_>>();
    for track in tracks {
        if seen_ids.insert(track.video_id.clone()) {
            queue.push(track);
            added += 1;
        }
    }
    added
}

fn maybe_extend_radio_queue(state: &Arc<Mutex<AppState>>, ps: &Rc<PlayerState>) {
    if !is_radio_queue(ps) || ps.radio_append_in_flight.get() || ps.repeat_mode.get() == RepeatMode::All {
        return;
    }

    let last_video_id = {
        let queue = ps.queue.borrow();
        let idx = ps.queue_index.get();
        if queue.is_empty() || idx + 1 != queue.len() {
            return;
        }
        queue.last().map(|track| track.video_id.clone())
    };
    let Some(last_video_id) = last_video_id else {
        return;
    };

    if ps.last_radio_append_anchor.borrow().as_deref() == Some(last_video_id.as_str()) {
        return;
    }

    *ps.last_radio_append_anchor.borrow_mut() = Some(last_video_id.clone());
    ps.radio_append_in_flight.set(true);

    let state = state.clone();
    let ps = ps.clone();
    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let vid = last_video_id.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_radio(&vid, None, None))
        })
        .await;

        if let Ok(Some(Ok(radio))) = result {
            if append_radio_recommendations(&ps, radio.tracks) > 0 {
                render_queue_panel(&ps, &state);
                sync_remote_queue(&ps);
                sync_mpris_transport(&ps, &state);
                prefetch_upcoming(&state, &ps, ps.queue_index.get());
            }
        }

        ps.radio_append_in_flight.set(false);
    });
}

/// Set the queue from a track list context and play from the given index.
fn play_from_context(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    tracks: &[Track],
    index: usize,
) {
    *ps.queue.borrow_mut() = tracks.to_vec();
    ps.queue_index.set(index);
    set_queue_context(ps, None);
    play_from_queue(state, ps, index);
}

fn reset_player_ui(ps: &PlayerState, state: &Arc<Mutex<AppState>>) {
    let _ = state;
    let _ = ps.player.stop();
    clear_sponsorblock_state(ps);
    ps.last_mpris_length_micros.set(None);
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
    update_rating_buttons_ui(&ps.like_btn, &ps.dislike_btn, SongRating::Indifferent);
    ps.menu_btn.set_sensitive(false);
}

fn format_clock_time(t: ClockTime) -> String {
    let total_secs = t.seconds();
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}:{secs:02}")
}

fn apply_seek_position(ps: &Rc<PlayerState>, target: ClockTime, state: &Arc<Mutex<AppState>>) {
    let clamped_target = ps
        .player
        .duration()
        .map(|duration| ClockTime::from_nseconds(target.nseconds().min(duration.nseconds())))
        .unwrap_or(target);

    ps.is_seeking.set(true);
    let _ = ps.player.seek(clamped_target);
    note_playback_position(ps, clamped_target.nseconds() / 1_000_000);

    if let Some(duration) = ps.player.duration() {
        let duration_ns = duration.nseconds();
        if duration_ns > 0 {
            let frac = clamped_target.nseconds() as f64 / duration_ns as f64;
            ps.progress_slider.set_value(frac.clamp(0.0, 1.0));
        }
        ps.time_label.set_text(&format!(
            "{} / {}",
            format_clock_time(clamped_target),
            format_clock_time(duration),
        ));
    }

    ps.is_seeking.set(false);
    maybe_apply_sponsorblock_skip(state, ps, clamped_target);
}

fn clear_sponsorblock_state(ps: &PlayerState) {
    ps.sponsor_segments.borrow_mut().clear();
    ps.sponsor_segments_reported.borrow_mut().clear();
}

fn ensure_offline_sponsorblock_cache(
    state: &Arc<Mutex<AppState>>,
    video_id: &str,
) -> Result<Option<(Vec<SponsorSegment>, &'static str)>, String> {
    let all_categories = default_skip_categories();
    let client = {
        let guard = state.lock().unwrap();
        if !guard.cache.is_cached(video_id) {
            return Ok(None);
        }

        match guard.cache.get_offline_sponsor_segments(video_id) {
            Ok(Some(segments)) => return Ok(Some((segments, "offline-cache"))),
            Ok(None) => {}
            Err(err) => {
                eprintln!("[SPONSORBLOCK] failed to read offline cache for {video_id}: {err}");
            }
        }

        guard.sponsorblock.clone()
    };

    let segments = client
        .fetch_skip_segments(video_id, &all_categories)
        .map_err(|err| {
            format!("Failed to hydrate offline SponsorBlock cache for {video_id}: {err}")
        })?;

    let guard = state.lock().unwrap();
    if let Err(err) = guard
        .cache
        .store_offline_sponsor_segments(video_id, &segments)
    {
        eprintln!("[SPONSORBLOCK] failed to store offline cache for {video_id}: {err}");
    }

    Ok(Some((segments, "offline-hydrated")))
}

fn refresh_sponsorblock_for_current_track(state: &Arc<Mutex<AppState>>, ps: &Rc<PlayerState>) {
    let _ = state;
    clear_sponsorblock_state(ps);
    crate::player::notify_daemon_settings_changed();
}

fn maybe_apply_sponsorblock_skip(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    position: ClockTime,
) {
    let _ = (state, ps, position);
}

fn apply_song_rating(ps: &Rc<PlayerState>, session_id: u64, rating: SongRating) {
    let mut now_playing = ps.now_playing_track.borrow_mut();
    let Some(track) = now_playing.as_mut() else {
        return;
    };
    if track.session_id != session_id {
        return;
    }
    track.rating = rating;
    update_rating_buttons_ui(&ps.like_btn, &ps.dislike_btn, rating);
}

fn spawn_song_rating_sync(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    video_id: &str,
    session_id: u64,
) {
    let state = state.clone();
    let ps = ps.clone();
    let video_id = video_id.to_string();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let video_id_bg = video_id.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_song_rating(&video_id_bg))
        })
        .await;

        match result {
            Ok(Some(Ok(rating))) => apply_song_rating(&ps, session_id, rating),
            Ok(Some(Err(err))) => {
                eprintln!("[RATING] failed to fetch rating for {video_id}: {err}")
            }
            _ => {}
        }
    });
}

fn spawn_sponsorblock_sync(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    video_id: &str,
    session_id: u64,
) {
    let _ = (state, ps, video_id, session_id);
}

fn submit_song_rating(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    desired_rating: SongRating,
) {
    let Some((session_id, video_id, previous_rating)) = ps
        .now_playing_track
        .borrow()
        .as_ref()
        .map(|track| (track.session_id, track.video_id.clone(), track.rating))
    else {
        return;
    };

    apply_song_rating(ps, session_id, desired_rating);

    let state = state.clone();
    let ps = ps.clone();
    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let video_id_bg = video_id.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.rate_song(&video_id_bg, desired_rating))
        })
        .await;

        match result {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(err))) => {
                eprintln!("[RATING] failed to set rating for {video_id}: {err}");
                apply_song_rating(&ps, session_id, previous_rating);
            }
            _ => apply_song_rating(&ps, session_id, previous_rating),
        }
    });
}

fn maybe_flush_watchtime_progress(ps: &PlayerState) {
    let _ = ps;
}

// ============================================================
// DATA LOADING
// ============================================================

fn load_home(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
    force_refresh: bool,
) {
    if force_refresh {
        store_home_scroll_position(state, 0.0);
        content_scroll.vadjustment().set_value(0.0);
    }

    if !force_refresh {
        if let Some(page) = cached_home_page(state) {
            clear_box(content_box);
            content_box.add_css_class("home-view-active");
            home_pagination
                .continuation
                .replace(page.continuation.clone());
            home_pagination.loading.set(false);
            render_home_view(
                content_box,
                &page.sections,
                state,
                content_scroll,
                home_pagination,
                ps,
                recent_list,
                false,
            );
            restore_home_scroll_position_later(state, content_scroll);
            queue_home_lazy_load_check(
                state,
                content_box,
                content_scroll,
                home_pagination,
                ps,
                recent_list,
            );
            return;
        }
    }

    clear_box(content_box);
    content_box.add_css_class("home-view-active");
    home_pagination.continuation.replace(None);
    home_pagination.loading.set(true);
    let loading = Label::new(Some("Loading recommendations..."));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let home_pagination = home_pagination.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_home_page(None))
        })
        .await;

        home_pagination.loading.set(false);
        clear_box(&content_box);
        content_box.add_css_class("home-view-active");

        match result {
            Ok(Some(Ok(page))) => {
                store_home_cache(&state, &page);
                home_pagination
                    .continuation
                    .replace(page.continuation.clone());
                let listen_again_tracks = extract_listen_again_tracks(&page.sections);
                let state_bg = state.clone();
                let _ = gio::spawn_blocking(move || {
                    let guard = state_bg.lock().unwrap();
                    let listen_again_ids = listen_again_tracks
                        .iter()
                        .map(|track| track.video_id.clone())
                        .collect::<Vec<_>>();
                    for track in &listen_again_tracks {
                        let _ = guard.cache.upsert_track_metadata(
                            &track.video_id,
                            &track.title,
                            &artist_string(track),
                            history_thumbnail_candidate(track.thumbnail_url.as_deref()),
                        );
                    }
                    guard.cache.store_listen_again_snapshot(&listen_again_ids)
                })
                .await;
                render_home_view(
                    &content_box,
                    &page.sections,
                    &state,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                    false,
                );
                if force_refresh {
                    content_scroll.vadjustment().set_value(0.0);
                } else {
                    restore_home_scroll_position_later(&state, &content_scroll);
                }
                queue_home_lazy_load_check(
                    &state,
                    &content_box,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                );
            }
            Ok(Some(Err(e))) => {
                home_pagination.continuation.replace(None);
                content_box.append(&Label::new(Some(&format!("Error: {e}"))));
            }
            Ok(None) => {
                home_pagination.continuation.replace(None);
                content_box.append(&Label::new(Some("Not connected.")));
            }
            Err(e) => {
                home_pagination.continuation.replace(None);
                content_box.append(&Label::new(Some(&format!("Error: {e:?}"))));
            }
        }
    });
}

fn restore_home_scroll_position_later(
    state: &Arc<Mutex<AppState>>,
    content_scroll: &ScrolledWindow,
) {
    let state = state.clone();
    let content_scroll = content_scroll.clone();
    glib::idle_add_local_once(move || {
        let adjustment = content_scroll.vadjustment();
        let max_value = (adjustment.upper() - adjustment.page_size()).max(0.0);
        let target = cached_home_scroll_position(&state).min(max_value);
        adjustment.set_value(target);
    });
}

fn queue_home_lazy_load_check(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let home_pagination = home_pagination.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::idle_add_local_once(move || {
        maybe_load_more_home_if_needed(
            &state,
            &content_box,
            &content_scroll,
            &home_pagination,
            &ps,
            &recent_list,
        );
    });
}

fn maybe_load_more_home_if_needed(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    if !content_box.has_css_class("home-view-active") || home_pagination.loading.get() {
        return;
    }

    if home_pagination.continuation.borrow().is_none() {
        return;
    }

    let adjustment = content_scroll.vadjustment();
    let remaining = adjustment.upper() - (adjustment.value() + adjustment.page_size());
    if remaining > 720.0 {
        return;
    }

    load_more_home(
        state,
        content_box,
        content_scroll,
        home_pagination,
        ps,
        recent_list,
    );
}

fn load_more_home(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    let Some(continuation) = home_pagination.continuation.borrow().clone() else {
        return;
    };
    home_pagination.loading.set(true);

    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let home_pagination = home_pagination.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let token = continuation.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_home_page(Some(&token)))
        })
        .await;

        home_pagination.loading.set(false);

        let Ok(Some(Ok(page))) = result else {
            return;
        };

        if !content_box.has_css_class("home-view-active") {
            return;
        }

        let next_continuation = page
            .continuation
            .clone()
            .filter(|token| token != &continuation);
        home_pagination
            .continuation
            .replace(next_continuation.clone());

        {
            let mut guard = state.lock().unwrap();
            if let Some(cached) = guard.home_cache.as_mut() {
                cached.sections.extend(page.sections.clone());
                cached.continuation = next_continuation.clone();
            } else {
                guard.home_cache = Some(HomePage {
                    sections: page.sections.clone(),
                    continuation: next_continuation.clone(),
                });
            }
        }

        render_home_view(
            &content_box,
            &page.sections,
            &state,
            &content_scroll,
            &home_pagination,
            &ps,
            &recent_list,
            true,
        );
        queue_home_lazy_load_check(
            &state,
            &content_box,
            &content_scroll,
            &home_pagination,
            &ps,
            &recent_list,
        );
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
        let result =
            gio::spawn_blocking(move || app_client(&state_bg).map(|ytm| ytm.search(&q))).await;

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

fn parse_direct_music_url(input: &str) -> Option<DirectMusicUrlTarget> {
    let trimmed = input.trim();

    if let Some(target) = parse_direct_music_reference(trimmed) {
        return Some(target);
    }

    let normalized = if trimmed.starts_with("music.youtube.com/")
        || trimmed.starts_with("youtube.com/")
        || trimmed.starts_with("www.youtube.com/")
        || trimmed.starts_with("youtu.be/")
    {
        format!("https://{trimmed}")
    } else {
        trimmed.to_string()
    };

    let url = url::Url::parse(&normalized).ok()?;
    let host = url.host_str()?.trim_start_matches("www.");

    match host {
        "youtu.be" => {
            let video_id = url
                .path_segments()
                .and_then(|mut segments| segments.next())
                .filter(|segment| !segment.is_empty())?;
            Some(DirectMusicUrlTarget::Video {
                video_id: video_id.to_string(),
            })
        }
        "youtube.com" | "music.youtube.com" => {
            let path = url.path().trim_end_matches('/');
            if path == "/watch" {
                if let Some(video_id) = url
                    .query_pairs()
                    .find_map(|(key, value)| (key == "v").then_some(value.into_owned()))
                {
                    return Some(DirectMusicUrlTarget::Video { video_id });
                }

                if let Some(list_id) = url
                    .query_pairs()
                    .find_map(|(key, value)| (key == "list").then_some(value.into_owned()))
                {
                    return Some(DirectMusicUrlTarget::Browse {
                        browse_id: normalize_playlist_browse_id(&list_id),
                        page_type: infer_browse_page_type(&list_id),
                    });
                }
            }

            if path == "/playlist" {
                let list_id = url
                    .query_pairs()
                    .find_map(|(key, value)| (key == "list").then_some(value.into_owned()))?;
                return Some(DirectMusicUrlTarget::Browse {
                    browse_id: normalize_playlist_browse_id(&list_id),
                    page_type: infer_browse_page_type(&list_id),
                });
            }

            if let Some(browse_id) = path.strip_prefix("/browse/").filter(|value| !value.is_empty())
            {
                return Some(DirectMusicUrlTarget::Browse {
                    browse_id: browse_id.to_string(),
                    page_type: infer_browse_page_type(browse_id),
                });
            }

            if let Some(browse_id) = path
                .strip_prefix("/channel/")
                .filter(|value| !value.is_empty())
            {
                return Some(DirectMusicUrlTarget::Browse {
                    browse_id: browse_id.to_string(),
                    page_type: infer_browse_page_type(browse_id),
                });
            }

            None
        }
        _ => None,
    }
}

fn parse_direct_music_reference(input: &str) -> Option<DirectMusicUrlTarget> {
    let normalized = input.trim().trim_end_matches('/');

    if let Some(browse_id) = normalized
        .strip_prefix("/browse/")
        .or_else(|| normalized.strip_prefix("/channel/"))
        .filter(|value| !value.is_empty())
    {
        return Some(DirectMusicUrlTarget::Browse {
            browse_id: browse_id.to_string(),
            page_type: infer_browse_page_type(browse_id),
        });
    }

    let bare = normalized.trim_start_matches('/');
    if !bare.is_empty()
        && (bare.starts_with("UC")
            || bare.starts_with("MPRE")
            || bare.starts_with("VL")
            || bare.starts_with("OLAK")
            || bare.starts_with("RD"))
    {
        return Some(DirectMusicUrlTarget::Browse {
            browse_id: bare.to_string(),
            page_type: infer_browse_page_type(bare),
        });
    }

    None
}

fn normalize_playlist_browse_id(list_id: &str) -> String {
    if list_id.starts_with("VL") {
        list_id.to_string()
    } else {
        format!("VL{list_id}")
    }
}

fn infer_browse_page_type(browse_id: &str) -> HomeBrowsePageType {
    if browse_id.starts_with("MPRE") {
        HomeBrowsePageType::Album
    } else if browse_id.starts_with("UC") {
        HomeBrowsePageType::UserChannel
    } else if browse_id.starts_with("VL") || browse_id.starts_with("OLAK") || browse_id.starts_with("RD")
    {
        HomeBrowsePageType::Playlist
    } else {
        HomeBrowsePageType::Artist
    }
}

fn autoplay_direct_music_url(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    raw_url: &str,
    target: DirectMusicUrlTarget,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    match target {
        DirectMusicUrlTarget::Video { video_id } => autoplay_video_url(state, raw_url, &video_id, ps, recent_list),
        DirectMusicUrlTarget::Browse {
            browse_id,
            page_type,
        } => load_browse_detail(
            state,
            content_box,
            content_scroll,
            home_pagination,
            &browse_id,
            raw_url,
            None,
            page_type,
            ps,
            recent_list,
            true,
        ),
    }
}

fn autoplay_video_url(
    state: &Arc<Mutex<AppState>>,
    raw_url: &str,
    video_id: &str,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    let state = state.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();
    let video_id = video_id.to_string();
    let fallback_title = raw_url.to_string();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let vid = video_id.clone();
        let result =
            gio::spawn_blocking(move || app_client(&state_bg).map(|ytm| ytm.get_radio(&vid, None, None)))
                .await;

        let fallback_track = Track {
            video_id: video_id.clone(),
            title: fallback_title.clone(),
            artists: vec![Artist {
                name: "Unknown artist".to_string(),
                id: None,
            }],
            album: None,
            duration: None,
            thumbnail_url: None,
        };

        let track = match result {
            Ok(Some(Ok(radio))) => {
                let tracks = radio.tracks;
                tracks
                    .iter()
                    .find(|track| track.video_id == video_id)
                    .cloned()
                    .or_else(|| tracks.into_iter().next())
                    .unwrap_or(fallback_track)
            }
            _ => fallback_track,
        };

        play_track(
            &state,
            &track.video_id,
            &track.title,
            &artist_string(&track),
            track.thumbnail_url.as_deref(),
            &ps,
        );
        add_to_recent(&recent_list, &track, &state, &ps);
    });
}

fn load_offline_page_data(
    state: &Arc<Mutex<AppState>>,
) -> (Vec<PlaylistSummary>, Vec<Track>, Vec<Track>) {
    repair_offline_track_metadata(state);

    let guard = state.lock().unwrap();

    let mut offline_playlists: Vec<PlaylistSummary> = guard
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

    let listen_again_track_ids = guard
        .cache
        .offline_listen_again_track_ids()
        .unwrap_or_default();
    if let Some(first_video_id) = listen_again_track_ids.first() {
        offline_playlists.insert(
            0,
            PlaylistSummary {
                playlist_id: OFFLINE_LISTEN_AGAIN_PLAYLIST_ID.to_string(),
                title: "Listen again".to_string(),
                thumbnail_url: offline_thumbnail_url(&guard.cache, first_video_id),
                subtitle: Some("Auto-kept from Listen again".to_string()),
            },
        );
    }

    let all_tracks: Vec<Track> = guard
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
            let thumbnail_url = offline_thumbnail_url(&guard.cache, &video_id);

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

    let playlist_track_ids: HashSet<String> = offline_playlists
        .iter()
        .flat_map(|playlist| {
            if playlist.playlist_id == OFFLINE_LISTEN_AGAIN_PLAYLIST_ID {
                listen_again_track_ids.clone()
            } else {
                guard
                    .cache
                    .offline_playlist_track_ids(&playlist.playlist_id)
                    .unwrap_or_default()
            }
        })
        .collect();

    let display_tracks = all_tracks
        .iter()
        .filter(|track| !playlist_track_ids.contains(&track.video_id))
        .cloned()
        .collect();

    (offline_playlists, display_tracks, all_tracks)
}

fn repair_offline_track_metadata(state: &Arc<Mutex<AppState>>) {
    let repair_ids = {
        let guard = state.lock().unwrap();
        guard
            .cache
            .offline_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(video_id, title, artist, _art)| {
                let thumbnail_url = offline_thumbnail_url(&guard.cache, &video_id);
                let needs_repair = title.trim().is_empty()
                    || title == video_id
                    || artist.trim().is_empty()
                    || thumbnail_url.is_none();
                needs_repair.then_some(video_id)
            })
            .take(8)
            .collect::<Vec<_>>()
    };

    if repair_ids.is_empty() {
        return;
    }

    let Some(client) = app_client(state) else {
        return;
    };

    for video_id in repair_ids {
        let mut radio = match client.get_radio(&video_id, None, None) {
            Ok(radio) => radio,
            Err(err) => {
                eprintln!("[OFFLINE] failed to repair metadata for {video_id}: {err}");
                continue;
            }
        };

        let repaired = radio
            .tracks
            .iter()
            .find(|track| track.video_id == video_id)
            .cloned()
            .or_else(|| radio.tracks.drain(..).next());

        let Some(track) = repaired else {
            continue;
        };

        let guard = state.lock().unwrap();
        let _ = guard.cache.upsert_track_metadata(
            &track.video_id,
            &track.title,
            &artist_string(&track),
            history_thumbnail_candidate(track.thumbnail_url.as_deref()),
        );
    }
}

fn load_offline_playlist_tracks(state: &Arc<Mutex<AppState>>, playlist_id: &str) -> Vec<Track> {
    let guard = state.lock().unwrap();
    let video_ids = if playlist_id == OFFLINE_LISTEN_AGAIN_PLAYLIST_ID {
        guard
            .cache
            .offline_listen_again_track_ids()
            .unwrap_or_default()
    } else {
        guard
            .cache
            .offline_playlist_track_ids(playlist_id)
            .unwrap_or_default()
    };

    video_ids
        .into_iter()
        .map(|video_id| {
            let thumbnail_url = offline_thumbnail_url(&guard.cache, &video_id);
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

fn offline_thumbnail_url(cache: &MusicCache, video_id: &str) -> Option<String> {
    cache
        .get_cached_art(video_id)
        .map(|path| format!("file://{}", path.display()))
        .or_else(|| cache.get_thumbnail_url(video_id))
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
            Ok((offline_playlists, tracks, _playable_tracks)) => {
                if tracks.is_empty() && offline_playlists.is_empty() {
                    let empty = Label::new(Some(
                        "No offline songs yet. Play songs to build your cache.",
                    ));
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
                    let pl_header = Label::new(Some("Offline Collections"));
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
                        let card = build_offline_playlist_card(
                            playlist,
                            &state,
                            &content_box,
                            &ps,
                            &recent_list,
                        );
                        cards_box.append(&card);
                    }

                    cards_scroll.set_child(Some(&cards_box));
                    content_box.append(&cards_scroll);
                    content_box.append(&Separator::new(Orientation::Horizontal));
                }

                if !tracks.is_empty() {
                    let count_label =
                        Label::new(Some(&format!("{} songs available offline", tracks.len())));
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

                    let tracks_rc = Rc::new(tracks);
                    for (i, track) in tracks_rc.iter().enumerate() {
                        grid.append(&build_track_row(
                            track,
                            i,
                            &tracks_rc,
                            &state,
                            &ps,
                            &recent_list,
                        ));
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
        let result =
            gio::spawn_blocking(move || app_client(&state_bg).map(|ytm| ytm.get_playlist("LM")))
                .await;

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

                content_box.append(&build_play_controls(
                    &tracks,
                    &state,
                    &ps,
                    Some(DownloadPlaylistMeta {
                        playlist_id: "LM".to_string(),
                        title: "Liked Songs".to_string(),
                        thumbnail_url: None,
                        subtitle: None,
                    }),
                ));

                let grid = gtk::Grid::new();
                grid.set_column_spacing(12);
                grid.set_row_spacing(8);
                grid.set_column_homogeneous(true);
                grid.set_margin_top(4);
                grid.set_margin_start(20);
                grid.set_margin_end(20);

                let tracks_rc = Rc::new(tracks);
                for (i, track) in tracks_rc.iter().enumerate() {
                    let row = build_track_row(track, i, &tracks_rc, &state, &ps, &recent_list);
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
        })
        .await;

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
                    let card =
                        build_playlist_card(playlist, &state, &content_box, &ps, &recent_list);
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

fn build_home_item_card(
    item: &HomeItem,
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> Button {
    match item {
        HomeItem::Track(track) => build_cover_card(track, state, ps, recent_list),
        HomeItem::BrowseCard(card) => build_home_browse_card(
            card,
            state,
            content_box,
            content_scroll,
            home_pagination,
            ps,
            recent_list,
        ),
    }
}

fn build_home_browse_card(
    card: &HomeBrowseCard,
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> Button {
    let button = build_browse_summary_card(&card.title, card.subtitle.as_deref(), card.thumbnail_url.as_deref());

    {
        let state = state.clone();
        let content_box = content_box.clone();
        let content_scroll = content_scroll.clone();
        let home_pagination = home_pagination.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let browse_id = card.browse_id.clone();
        let title = card.title.clone();
        let thumb = card.thumbnail_url.clone();
        let page_type = card.page_type;

        button.connect_clicked(move |_| {
            load_browse_detail(
                &state,
                &content_box,
                &content_scroll,
                &home_pagination,
                &browse_id,
                &title,
                thumb.as_deref(),
                page_type,
                &ps,
                &recent_list,
                false,
            );
        });
    }

    button
}

fn build_playlist_card(
    playlist: &PlaylistSummary,
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> Button {
    let card_btn = build_browse_summary_card(
        &playlist.title,
        playlist.subtitle.as_deref(),
        playlist.thumbnail_url.as_deref(),
    );

    {
        let state = state.clone();
        let content_box = content_box.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let playlist_id = playlist.playlist_id.clone();
        let playlist_title = playlist.title.clone();
        let playlist_thumb = playlist.thumbnail_url.clone();

        card_btn.connect_clicked(move |_| {
            load_playlist_detail(
                &state,
                &content_box,
                &playlist_id,
                &playlist_title,
                playlist_thumb.as_deref(),
                &ps,
                &recent_list,
            );
        });
    }

    attach_queue_drag_source(
        &card_btn,
        QueueDragPayload::Playlist {
            playlist_id: playlist.playlist_id.clone(),
            title: playlist.title.clone(),
            thumbnail_url: playlist.thumbnail_url.clone(),
            subtitle: playlist.subtitle.clone(),
            source: QueuePlaylistSource::Remote,
        },
        ps,
    );

    card_btn
}

fn build_browse_summary_card(title: &str, subtitle: Option<&str>, thumbnail_url: Option<&str>) -> Button {
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

    if let Some(url) = thumbnail_url {
        let picture = Picture::new();
        picture.set_keep_aspect_ratio(false);
        picture.set_size_request(150, 150);
        picture.set_hexpand(false);
        art_container.append(&picture);
        load_thumbnail_into(&picture, url, 150);
    } else {
        let initial = title
            .chars()
            .next()
            .unwrap_or('\u{266B}')
            .to_uppercase()
            .to_string();
        let letter = Label::new(Some(&initial));
        letter.add_css_class("cover-art-letter");
        letter.set_vexpand(true);
        letter.set_valign(Align::Center);
        art_container.append(&letter);
    }

    card.append(&art_container);

    let name = Label::new(Some(title));
    name.add_css_class("cover-card-title");
    name.set_halign(Align::Start);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_max_width_chars(18);
    card.append(&name);

    if let Some(subtitle) = subtitle {
        let subtitle_label = Label::new(Some(subtitle));
        subtitle_label.add_css_class("cover-card-artist");
        subtitle_label.set_halign(Align::Start);
        subtitle_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        subtitle_label.set_max_width_chars(18);
        card.append(&subtitle_label);
    }

    card_btn.set_child(Some(&card));
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
        let result =
            gio::spawn_blocking(move || app_client(&state_bg).map(|ytm| ytm.get_playlist(&pid)))
                .await;

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

                content_box.append(&build_play_controls(
                    &tracks,
                    &state,
                    &ps,
                    Some(DownloadPlaylistMeta {
                        playlist_id: playlist_id.clone(),
                        title: playlist_title.clone(),
                        thumbnail_url: playlist_thumb.clone(),
                        subtitle: Some(format!("{} tracks", tracks.len())),
                    }),
                ));

                let grid = gtk::Grid::new();
                grid.set_column_spacing(12);
                grid.set_row_spacing(8);
                grid.set_column_homogeneous(true);
                grid.set_margin_top(4);
                grid.set_margin_start(20);
                grid.set_margin_end(20);

                let tracks_rc = Rc::new(tracks);
                for (i, track) in tracks_rc.iter().enumerate() {
                    let row = build_track_row(track, i, &tracks_rc, &state, &ps, &recent_list);
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

fn load_browse_detail(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    browse_id: &str,
    browse_title: &str,
    browse_thumb: Option<&str>,
    page_type: HomeBrowsePageType,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
    autoplay: bool,
) {
    clear_box(content_box);
    let loading = Label::new(Some(&format!("Loading \"{}\"...", browse_title)));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let home_pagination = home_pagination.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();
    let browse_id = browse_id.to_string();
    let browse_title = browse_title.to_string();
    let browse_thumb = browse_thumb.map(str::to_string);

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let bid = browse_id.clone();
        let page_type_bg = page_type;
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| match page_type_bg {
                HomeBrowsePageType::UserChannel | HomeBrowsePageType::Artist => ytm
                    .get_channel_page(&bid)
                    .map(BrowseDetailData::Sections),
                _ => ytm.get_browse_tracks(&bid).map(BrowseDetailData::Tracks),
            })
        })
        .await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(BrowseDetailData::Sections(page)))) => {
                let display_title = page
                    .title
                    .clone()
                    .unwrap_or_else(|| browse_title.clone());
                let display_thumb = page
                    .thumbnail_url
                    .clone()
                    .or_else(|| browse_thumb.clone());
                let artist_like = page.artist_like;
                let sections = page.sections;
                let back_btn = Button::with_label("\u{2190} Back to Home");
                back_btn.add_css_class("nav-item");
                back_btn.set_halign(Align::Start);
                back_btn.set_margin_start(24);
                back_btn.set_margin_top(12);
                {
                    let state = state.clone();
                    let content_box = content_box.clone();
                    let content_scroll = content_scroll.clone();
                    let home_pagination = home_pagination.clone();
                    let ps = ps.clone();
                    let recent_list = recent_list.clone();
                    back_btn.connect_clicked(move |_| {
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
                content_box.append(&back_btn);

                if sections.is_empty() {
                    let msg = Label::new(Some("No sections found for this channel."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                if autoplay {
                    let tracks = flatten_section_tracks(&sections);
                    if let Some(first_track) = tracks.first() {
                        play_from_context(&state, &ps, &tracks, 0);
                        add_to_recent(&recent_list, first_track, &state, &ps);
                    } else if let Some(card) = first_playable_browse_card(&sections) {
                        let state_bg = state.clone();
                        let card = card.clone();
                        if let Ok(Some(Some(fallback_tracks))) = gio::spawn_blocking(move || {
                            let client = app_client(&state_bg)?;
                            let tracks = match card.page_type {
                                HomeBrowsePageType::Playlist | HomeBrowsePageType::Album => {
                                    client.get_browse_tracks(&card.browse_id).ok()
                                }
                                HomeBrowsePageType::Artist | HomeBrowsePageType::UserChannel => {
                                    client
                                        .get_channel_page(&card.browse_id)
                                        .ok()
                                        .map(|page| flatten_section_tracks(&page.sections))
                                }
                            }?;
                            Some(Some(tracks))
                        })
                        .await
                        {
                            if let Some(first_track) = fallback_tracks.first() {
                                play_from_context(&state, &ps, &fallback_tracks, 0);
                                add_to_recent(&recent_list, first_track, &state, &ps);
                            }
                        }
                    }
                }

                let use_artist_chrome =
                    page_type == HomeBrowsePageType::Artist || artist_like || is_artist_sections(&sections);
                render_artist_view(
                    &content_box,
                    if use_artist_chrome { "Artist" } else { "Channel" },
                    &display_title,
                    display_thumb.as_deref(),
                    &sections,
                    &state,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                );
            }
            Ok(Some(Ok(BrowseDetailData::Tracks(tracks)))) => {
                let back_btn = Button::with_label("\u{2190} Back to Home");
                back_btn.add_css_class("nav-item");
                back_btn.set_halign(Align::Start);
                back_btn.set_margin_start(24);
                back_btn.set_margin_top(12);
                {
                    let state = state.clone();
                    let content_box = content_box.clone();
                    let content_scroll = content_scroll.clone();
                    let home_pagination = home_pagination.clone();
                    let ps = ps.clone();
                    let recent_list = recent_list.clone();
                    back_btn.connect_clicked(move |_| {
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
                content_box.append(&back_btn);

                let header = Label::new(Some(&browse_title));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(12);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if tracks.is_empty() {
                    let msg = Label::new(Some("No playable tracks found for this page."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                if autoplay {
                    play_from_context(&state, &ps, &tracks, 0);
                    if let Some(first_track) = tracks.first() {
                        add_to_recent(&recent_list, first_track, &state, &ps);
                    }
                }

                let count_label = Label::new(Some(&format!("{} tracks", tracks.len())));
                count_label.add_css_class("dim-label");
                count_label.set_halign(Align::Start);
                count_label.set_margin_start(28);
                count_label.set_margin_bottom(4);
                content_box.append(&count_label);

                let download_meta = matches!(
                    page_type,
                    HomeBrowsePageType::Playlist | HomeBrowsePageType::Album
                )
                .then(|| DownloadPlaylistMeta {
                    playlist_id: browse_id.clone(),
                    title: browse_title.clone(),
                    thumbnail_url: browse_thumb.clone(),
                    subtitle: Some(format!("{} tracks", tracks.len())),
                });

                content_box.append(&build_play_controls(&tracks, &state, &ps, download_meta));

                let grid = gtk::Grid::new();
                grid.set_column_spacing(12);
                grid.set_row_spacing(8);
                grid.set_column_homogeneous(true);
                grid.set_margin_top(4);
                grid.set_margin_start(20);
                grid.set_margin_end(20);

                let tracks_rc = Rc::new(tracks);
                for (i, track) in tracks_rc.iter().enumerate() {
                    let row = build_track_row(track, i, &tracks_rc, &state, &ps, &recent_list);
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
        let initial = playlist
            .title
            .chars()
            .next()
            .unwrap_or('\u{266B}')
            .to_uppercase()
            .to_string();
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
            load_offline_playlist_detail(
                &state,
                &content_box,
                &playlist_id,
                &playlist_title,
                &ps,
                &recent_list,
            );
        });
    }

    attach_queue_drag_source(
        &card_btn,
        QueueDragPayload::Playlist {
            playlist_id: playlist.playlist_id.clone(),
            title: playlist.title.clone(),
            thumbnail_url: playlist.thumbnail_url.clone(),
            subtitle: playlist.subtitle.clone(),
            source: QueuePlaylistSource::Offline,
        },
        ps,
    );

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
        let result =
            gio::spawn_blocking(move || load_offline_playlist_tracks(&state_bg, &playlist_id_bg))
                .await;

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

                let tracks_rc = Rc::new(tracks);
                for (i, track) in tracks_rc.iter().enumerate() {
                    let row = build_track_row(track, i, &tracks_rc, &state, &ps, &recent_list);
                    grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
                }

                content_box.append(&grid);
            }
            Err(e) => {
                content_box.append(&Label::new(Some(&format!(
                    "Offline playlist load failed: {e:?}"
                ))));
            }
        }
    });
}

fn load_sidebar_history(state: &Arc<Mutex<AppState>>, recent_list: &GtkBox, ps: &Rc<PlayerState>) {
    let state = state.clone();
    let recent_list = recent_list.clone();
    let ps = ps.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result =
            gio::spawn_blocking(move || app_client(&state_bg).map(|ytm| ytm.get_history())).await;

        if let Ok(Some(Ok(tracks))) = result {
            clear_box(&recent_list);
            for track in tracks.iter().take(SIDEBAR_HISTORY_LIMIT) {
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
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
    prepend_separator: bool,
) {
    if sections.is_empty() {
        let msg = Label::new(Some(
            "No recommendations returned. Your cookie might be expired.",
        ));
        msg.add_css_class("dim-label");
        msg.set_margin_top(60);
        content_box.append(&msg);
        return;
    }

    // First section -> horizontal cover art cards
    if let Some(first) = sections.first() {
        let first_tracks = home_section_tracks(first);
        if prepend_separator && content_box.first_child().is_some() {
            content_box.append(&Separator::new(Orientation::Horizontal));
        }
        let header = Label::new(Some(&first.title));
        header.add_css_class("section-header");
        header.set_halign(Align::Start);
        content_box.append(&header);

        if first_tracks.len() == first.contents.len() && !first_tracks.is_empty() {
            content_box.append(&build_play_controls(&first_tracks, state, ps, None));
        }

        let cards_scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .build();
        cards_scroll.add_css_class("cards-scroll");

        let cards_box = GtkBox::new(Orientation::Horizontal, 14);
        cards_box.set_margin_top(4);
        cards_box.set_margin_bottom(8);

        for item in &first.contents {
            cards_box.append(&build_home_item_card(
                item,
                state,
                content_box,
                content_scroll,
                home_pagination,
                ps,
                recent_list,
            ));
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

        let tracks = home_section_tracks(section);
        if tracks.len() == section.contents.len() {
            if !tracks.is_empty() {
                content_box.append(&build_play_controls(&tracks, state, ps, None));
            }
            let grid = gtk::Grid::new();
            grid.set_column_spacing(12);
            grid.set_row_spacing(8);
            grid.set_column_homogeneous(true);
            grid.set_margin_top(4);

            let section_tracks = Rc::new(tracks);
            for (i, track) in section_tracks.iter().enumerate() {
                let row = build_track_row(track, i, &section_tracks, state, ps, recent_list);
                grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
            }

            content_box.append(&grid);
        } else {
            let cards_scroll = ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Automatic)
                .vscrollbar_policy(gtk::PolicyType::Never)
                .build();
            cards_scroll.add_css_class("cards-scroll");

            let cards_box = GtkBox::new(Orientation::Horizontal, 14);
            cards_box.set_margin_top(4);
            cards_box.set_margin_bottom(8);

            for item in &section.contents {
                cards_box.append(&build_home_item_card(
                    item,
                    state,
                    content_box,
                    content_scroll,
                    home_pagination,
                    ps,
                    recent_list,
                ));
            }

            cards_scroll.set_child(Some(&cards_box));
            content_box.append(&cards_scroll);
        }
    }
}

fn render_artist_view(
    content_box: &GtkBox,
    eyebrow_label: &str,
    artist_name: &str,
    thumbnail_url: Option<&str>,
    sections: &[HomeSection],
    state: &Arc<Mutex<AppState>>,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    let ArtistSections {
        top_songs,
        remaining,
    } = split_artist_sections(sections);

    let hero = GtkBox::new(Orientation::Horizontal, 18);
    hero.set_margin_top(8);
    hero.set_margin_bottom(8);
    hero.set_margin_start(28);
    hero.set_margin_end(28);

    let art_frame = GtkBox::new(Orientation::Vertical, 0);
    art_frame.add_css_class("cover-art");
    art_frame.set_size_request(180, 180);
    art_frame.set_overflow(gtk::Overflow::Hidden);
    if let Some(url) = thumbnail_url {
        let picture = Picture::new();
        picture.set_keep_aspect_ratio(false);
        picture.set_size_request(180, 180);
        art_frame.append(&picture);
        load_thumbnail_into(&picture, url, 300);
    } else {
        let initial = artist_name
            .chars()
            .next()
            .unwrap_or('\u{266B}')
            .to_uppercase()
            .to_string();
        let letter = Label::new(Some(&initial));
        letter.add_css_class("cover-art-letter");
        letter.set_vexpand(true);
        letter.set_valign(Align::Center);
        art_frame.append(&letter);
    }
    hero.append(&art_frame);

    let meta = GtkBox::new(Orientation::Vertical, 8);
    meta.set_hexpand(true);
    meta.set_valign(Align::Center);

    let eyebrow = Label::new(Some(eyebrow_label));
    eyebrow.add_css_class("dim-label");
    eyebrow.set_halign(Align::Start);
    meta.append(&eyebrow);

    let title = Label::new(Some(artist_name));
    title.add_css_class("section-header");
    title.set_halign(Align::Start);
    title.set_wrap(true);
    meta.append(&title);

    let summary = if let Some(top_songs) = top_songs {
        format!(
            "{} top songs  •  {} featured sections",
            home_section_tracks(top_songs).len(),
            remaining.len()
        )
    } else {
        format!("{} featured sections", remaining.len())
    };
    let summary_label = Label::new(Some(&summary));
    summary_label.add_css_class("dim-label");
    summary_label.set_halign(Align::Start);
    meta.append(&summary_label);

    if let Some(top_songs) = top_songs {
        let tracks = home_section_tracks(top_songs);
        if !tracks.is_empty() {
            meta.append(&build_play_controls(&tracks, state, ps, None));
        }
    }

    hero.append(&meta);
    content_box.append(&hero);

    if let Some(top_songs) = top_songs {
        content_box.append(&Separator::new(Orientation::Horizontal));

        let header = Label::new(Some(&top_songs.title));
        header.add_css_class("section-header");
        header.set_halign(Align::Start);
        header.set_margin_top(8);
        content_box.append(&header);

        let tracks = home_section_tracks(top_songs);
        if !tracks.is_empty() {
            let grid = gtk::Grid::new();
            grid.set_column_spacing(12);
            grid.set_row_spacing(8);
            grid.set_column_homogeneous(true);
            grid.set_margin_top(4);

            let section_tracks = Rc::new(tracks);
            for (i, track) in section_tracks.iter().enumerate() {
                let row = build_track_row(track, i, &section_tracks, state, ps, recent_list);
                grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
            }

            content_box.append(&grid);
        }
    }

    for section in remaining {
        content_box.append(&Separator::new(Orientation::Horizontal));

        let header = Label::new(Some(&section.title));
        header.add_css_class("section-header");
        header.set_halign(Align::Start);
        header.set_margin_top(8);
        content_box.append(&header);

        let tracks = home_section_tracks(section);
        if tracks.len() == section.contents.len() && !tracks.is_empty() {
            content_box.append(&build_play_controls(&tracks, state, ps, None));
        }

        let cards_scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .build();
        cards_scroll.add_css_class("cards-scroll");

        let cards_box = GtkBox::new(Orientation::Horizontal, 14);
        cards_box.set_margin_top(4);
        cards_box.set_margin_bottom(8);

        for item in &section.contents {
            cards_box.append(&build_home_item_card(
                item,
                state,
                content_box,
                content_scroll,
                home_pagination,
                ps,
                recent_list,
            ));
        }

        cards_scroll.set_child(Some(&cards_box));
        content_box.append(&cards_scroll);
    }
}

fn extract_listen_again_tracks(sections: &[HomeSection]) -> Vec<Track> {
    sections
        .iter()
        .find(|section| section.title.eq_ignore_ascii_case("Listen again"))
        .or_else(|| {
            sections
                .iter()
                .find(|section| section.title.to_ascii_lowercase().contains("listen again"))
        })
        .map(home_section_tracks)
        .unwrap_or_default()
}

fn home_section_tracks(section: &HomeSection) -> Vec<Track> {
    section
        .contents
        .iter()
        .filter_map(|item| match item {
            HomeItem::Track(track) => Some(track.clone()),
            HomeItem::BrowseCard(_) => None,
        })
        .collect()
}

fn flatten_section_tracks(sections: &[HomeSection]) -> Vec<Track> {
    sections.iter().flat_map(home_section_tracks).collect()
}

fn first_playable_browse_card(sections: &[HomeSection]) -> Option<HomeBrowseCard> {
    sections.iter().find_map(|section| {
        section.contents.iter().find_map(|item| match item {
            HomeItem::BrowseCard(card) => Some(card.clone()),
            HomeItem::Track(_) => None,
        })
    })
}

fn is_artist_sections(sections: &[HomeSection]) -> bool {
    sections.iter().any(|section| {
        matches!(
            section.title.as_str(),
            "Top songs" | "Albums" | "Singles & EPs" | "Featured on" | "Fans might also like"
        )
    })
}

fn split_artist_sections<'a>(sections: &'a [HomeSection]) -> ArtistSections<'a> {
    let mut top_songs = None;
    let mut remaining = Vec::new();

    for section in sections {
        if top_songs.is_none() && section.title == "Top songs" {
            top_songs = Some(section);
        } else {
            remaining.push(section);
        }
    }

    ArtistSections {
        top_songs,
        remaining,
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

    let tracks_rc = Rc::new(tracks.to_vec());
    for (i, track) in tracks_rc.iter().enumerate() {
        let row = build_track_row(track, i, &tracks_rc, state, ps, recent_list);
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
        let initial = track
            .title
            .chars()
            .next()
            .unwrap_or('\u{266B}')
            .to_uppercase()
            .to_string();
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
            play_track(
                &state,
                &video_id,
                &title,
                &artist_str,
                thumb_url.as_deref(),
                &ps,
            );
            add_to_recent(&recent_list, &track, &state, &ps);
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

    append_download_action(&pop_box, track, state, &popover);

    popover.set_child(Some(&pop_box));
    popover.set_parent(&card_btn);

    let gesture = GestureClick::new();
    gesture.set_button(3);
    {
        let popover = popover.clone();
        gesture.connect_pressed(move |_, _, _, _| {
            popover.popup();
        });
    }
    card_btn.add_controller(gesture);

    {
        let ps = ps.clone();
        popover.connect_notify_local(Some("visible"), move |pop, _| {
            set_content_dimming(&ps, pop.is_visible());
        });
    }

    attach_queue_drag_source(
        &card_btn,
        QueueDragPayload::Track {
            track: track.clone(),
        },
        ps,
    );

    card_btn
}

fn build_track_row(
    track: &Track,
    index: usize,
    all_tracks: &Rc<Vec<Track>>,
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> GtkBox {
    let wrapper = GtkBox::new(Orientation::Horizontal, 0);
    wrapper.add_css_class("track-row-wrapper");

    let row_btn = Button::new();
    row_btn.add_css_class("track-row");
    row_btn.set_hexpand(true);

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
        let initial = track
            .title
            .chars()
            .next()
            .unwrap_or('\u{266B}')
            .to_uppercase()
            .to_string();
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

    // Left-click: play from this track's context (sets queue to all tracks)
    {
        let state = state.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let track = track.clone();
        let all_tracks = all_tracks.clone();

        row_btn.connect_clicked(move |_| {
            play_from_context(&state, &ps, &all_tracks, index);
            add_to_recent(&recent_list, &track, &state, &ps);
        });
    }

    wrapper.append(&row_btn);

    // Right-click context menu
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

    append_download_action(&pop_box, track, state, &popover);

    popover.set_child(Some(&pop_box));
    popover.set_parent(&wrapper);

    let gesture = GestureClick::new();
    gesture.set_button(3);
    {
        let popover = popover.clone();
        gesture.connect_pressed(move |_, _, _, _| {
            popover.popup();
        });
    }
    wrapper.add_controller(gesture);

    {
        let ps = ps.clone();
        popover.connect_notify_local(Some("visible"), move |pop, _| {
            set_content_dimming(&ps, pop.is_visible());
        });
    }

    attach_queue_drag_source(
        &wrapper,
        QueueDragPayload::Track {
            track: track.clone(),
        },
        ps,
    );

    wrapper
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

fn load_thumbnail_into(picture: &Picture, url: &str, size: i32) {
    if let Some(texture) = cached_thumbnail_texture(url, size) {
        picture.set_paintable(Some(&texture));
        return;
    }

    let picture = picture.clone();
    let url = url.to_string();

    glib::spawn_future_local(async move {
        let fetch_url = url.clone();
        let result = gio::spawn_blocking(move || {
            if let Some(path) = fetch_url.strip_prefix("file://") {
                std::fs::read(path).map_err(|e| e.to_string())
            } else {
                fetch_thumbnail_bytes(&fetch_url)
                    .ok_or_else(|| format!("failed to load thumbnail after retries: {fetch_url}"))
            }
        })
        .await;

        if let Ok(Ok(bytes)) = result {
            if let Some(texture) = build_thumbnail_texture(&bytes, size) {
                store_thumbnail_texture(&url, size, &texture);
                picture.set_paintable(Some(&texture));
            }
        }
    });
}

fn fetch_thumbnail_bytes(url: &str) -> Option<Vec<u8>> {
    const MAX_ATTEMPTS: usize = 3;
    const BACKOFF_MS: [u64; MAX_ATTEMPTS] = [250, 700, 1500];

    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).ok();
    }

    if let Some(bytes) = THUMBNAIL_BYTES_CACHE.lock().unwrap().get(url).cloned() {
        return Some(bytes);
    }

    for attempt in 0..MAX_ATTEMPTS {
        let response = THUMB_CLIENT.get(url).send();
        match response
            .and_then(|resp| resp.error_for_status())
            .and_then(|resp| resp.bytes())
        {
            Ok(bytes) => {
                let bytes = bytes.to_vec();
                THUMBNAIL_BYTES_CACHE
                    .lock()
                    .unwrap()
                    .insert(url.to_string(), bytes.clone());
                return Some(bytes);
            }
            Err(err) => {
                eprintln!(
                    "[ART] thumbnail fetch failed (attempt {}/{}): {} ({err})",
                    attempt + 1,
                    MAX_ATTEMPTS,
                    url
                );
                if attempt + 1 < MAX_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS[attempt]));
                }
            }
        }
    }

    None
}

fn build_thumbnail_texture(bytes: &[u8], size: i32) -> Option<gtk::gdk::Texture> {
    let gbytes = glib::Bytes::from(bytes);
    let stream = gio::MemoryInputStream::from_bytes(&gbytes);
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_stream(&stream, gio::Cancellable::NONE).ok()?;
    let w = pixbuf.width();
    let h = pixbuf.height();
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
    Some(gtk::gdk::Texture::for_pixbuf(&cropped))
}

fn cached_thumbnail_texture(url: &str, size: i32) -> Option<gtk::gdk::Texture> {
    THUMBNAIL_TEXTURE_CACHE.with(|cache| cache.borrow().get(&(url.to_string(), size)).cloned())
}

fn store_thumbnail_texture(url: &str, size: i32, texture: &gtk::gdk::Texture) {
    THUMBNAIL_TEXTURE_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .insert((url.to_string(), size), texture.clone());
    });
}

fn history_thumbnail_candidate(url: Option<&str>) -> Option<&str> {
    url.filter(|value| !value.starts_with("file://"))
}

fn note_playback_position(ps: &PlayerState, position_millis: u64) {
    if position_millis == 0 {
        return;
    }

    let mut now_playing = ps.now_playing_track.borrow_mut();
    let Some(track) = now_playing.as_mut() else {
        return;
    };
    track.last_observed_position_millis = position_millis;
}

fn play_track(
    state: &Arc<Mutex<AppState>>,
    video_id: &str,
    title: &str,
    artist: &str,
    thumb_url: Option<&str>,
    ps: &Rc<PlayerState>,
) {
    let seed_track = Track {
        video_id: video_id.to_string(),
        title: title.to_string(),
        artists: vec![Artist {
            name: artist.to_string(),
            id: None,
        }],
        album: None,
        duration: None,
        thumbnail_url: thumb_url.map(String::from),
    };

    *ps.queue.borrow_mut() = vec![seed_track.clone()];
    ps.queue_index.set(0);
    set_queue_context(
        ps,
        Some(RadioVirtualPlaylist {
            source_video_id: seed_track.video_id.clone(),
            touched_len: 1,
            chips: Vec::new(),
        }),
    );
    ps.radio_append_in_flight.set(true);
    render_queue_panel(ps, state);
    sync_mpris_transport(ps, state);

    clear_sponsorblock_state(ps);
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
    let ps = ps.clone();

    glib::spawn_future_local(async move {
        match ps
            .player
            .play_queue_at(
                ps.queue.borrow().clone(),
                0,
                ps.radio_virtual_playlist.borrow().clone(),
                ps.repeat_mode.get(),
            )
        {
            Ok(snapshot) => {
                apply_remote_snapshot(&ps, &state, snapshot);
            }
            Err(msg) => {
                ps.np_title.set_text(&msg);
                ps.np_artist.set_text("");
                return;
            }
        }

        // Fetch radio queue in background
        let state_bg = state.clone();
        let vid = video_id.clone();
        let radio_result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_radio(&vid, None, None))
        })
        .await;

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
                radio.tracks.insert(0, seed_track);
            }

            {
                let mut queue = ps.queue.borrow_mut();
                *queue = radio.tracks;
            }
            ps.queue_index.set(0);
            set_queue_context(
                &ps,
                Some(RadioVirtualPlaylist {
                    source_video_id: video_id.clone(),
                    touched_len: 1,
                    chips: radio.chips,
                }),
            );
            render_queue_panel(&ps, &state);
            sync_remote_queue(&ps);
            sync_mpris_transport(&ps, &state);
        }
        ps.radio_append_in_flight.set(false);
    });
}

/// Insert a track right after the currently-playing position.
fn queue_play_next(track: Track, ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    let insert_at = ps.queue_index.get() + 1;
    ps.queue.borrow_mut().insert(insert_at, track);
    *ps.last_radio_append_anchor.borrow_mut() = None;
    render_queue_panel(ps, state);
    sync_remote_queue(ps);
    sync_mpris_transport(ps, state);
}

/// Append a track to the end of the queue.
fn queue_add_to_end(track: Track, ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    ps.queue.borrow_mut().push(track);
    *ps.last_radio_append_anchor.borrow_mut() = None;
    render_queue_panel(ps, state);
    sync_remote_queue(ps);
    sync_mpris_transport(ps, state);
}

fn dismiss_queue_after_current(ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    let keep_len = ps.queue_index.get().saturating_add(1);
    {
        let mut queue = ps.queue.borrow_mut();
        if queue.len() <= keep_len {
            return;
        }
        queue.truncate(keep_len);
    }

    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        radio.touched_len = radio.touched_len.min(keep_len);
    }

    ps.radio_append_in_flight.set(false);
    *ps.last_radio_append_anchor.borrow_mut() = None;
    render_queue_panel(ps, state);
    sync_remote_queue(ps);
    sync_mpris_transport(ps, state);
}

/// Remove a track from the queue by index, adjusting queue_index if needed.
fn queue_remove(index: usize, ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    {
        let mut q = ps.queue.borrow_mut();
        if index >= q.len() {
            return;
        }
        q.remove(index);
        let current = ps.queue_index.get();
        if index < current {
            ps.queue_index.set(current - 1);
        }
    }
    shrink_radio_touched_for_remove(ps, index);
    *ps.last_radio_append_anchor.borrow_mut() = None;
    // Defer re-render to avoid destroying widget while its click handler is running
    let ps = ps.clone();
    let state = state.clone();
    glib::idle_add_local_once(move || {
        render_queue_panel(&ps, &state);
        sync_remote_queue(&ps);
        sync_mpris_transport(&ps, &state);
    });
}

fn queue_download_track(track: Track, state: &Arc<Mutex<AppState>>) {
    let state = state.clone();
    glib::spawn_future_local(async move {
        let _ = gio::spawn_blocking(move || {
            let vid = track.video_id.clone();
            if ensure_track_available_offline(&state, &track, true, "DOWNLOAD") {
                eprintln!("[DOWNLOAD] pinned {vid} for offline playback");
            }
        })
        .await;
    });
}

fn delete_track_from_offline(track: Track, state: &Arc<Mutex<AppState>>) {
    let state = state.clone();
    glib::spawn_future_local(async move {
        let _ = gio::spawn_blocking(move || {
            let vid = track.video_id.clone();
            let guard = state.lock().unwrap();
            if let Err(err) = guard.cache.remove_cached(&vid) {
                eprintln!("[DOWNLOAD] failed to remove {vid} from offline cache: {err}");
            } else {
                eprintln!("[DOWNLOAD] removed {vid} from offline cache");
            }
        })
        .await;
    });
}

fn is_pinned_download(state: &Arc<Mutex<AppState>>, video_id: &str) -> bool {
    state.lock().unwrap().cache.is_pinned_offline(video_id)
}

fn append_download_action(
    pop_box: &GtkBox,
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    popover: &Popover,
) {
    let is_pinned = is_pinned_download(state, &track.video_id);
    let button = if is_pinned {
        Button::with_label("Delete from offline")
    } else {
        Button::with_label("Download")
    };
    button.add_css_class("popover-action-btn");

    let track = track.clone();
    let state = state.clone();
    let popover = popover.clone();
    button.connect_clicked(move |_| {
        popover.popdown();
        if is_pinned {
            delete_track_from_offline(track.clone(), &state);
        } else {
            queue_download_track(track.clone(), &state);
        }
    });

    pop_box.append(&button);
}

/// Play a track from the queue by index (no radio re-fetch).
fn play_from_queue(state: &Arc<Mutex<AppState>>, ps: &Rc<PlayerState>, index: usize) {
    let queue = ps.queue.borrow();
    let Some(track) = queue.get(index) else {
        return;
    };
    ps.queue_index.set(index);
    mark_radio_touched_through(ps, index);

    let title = track.title.clone();
    let artist = artist_string(track);
    let thumb_url = track.thumbnail_url.clone();
    drop(queue);

    clear_sponsorblock_state(ps);

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
    maybe_extend_radio_queue(state, ps);

    match ps
        .player
        .play_queue_at(
            ps.queue.borrow().clone(),
            index,
            ps.radio_virtual_playlist.borrow().clone(),
            ps.repeat_mode.get(),
        )
    {
        Ok(snapshot) => apply_remote_snapshot(ps, state, snapshot),
        Err(msg) => {
            ps.np_title.set_text(&msg);
            ps.np_artist.set_text("");
        }
    }
}

/// Prefetch the next N songs in the queue to ~/Music/Static/temp/.
/// No playback reporting — just download audio bytes to disk.
fn prefetch_upcoming(state: &Arc<Mutex<AppState>>, ps: &Rc<PlayerState>, from_index: usize) {
    let settings = app_settings(state);
    if !settings.prefetch_enabled {
        return;
    }
    let prefetch_ahead = settings.prefetch_ahead.clamp(1, 12) as usize;
    let queue = ps.queue.borrow();
    let mut to_fetch: Vec<(String, Option<String>)> = Vec::new();

    for i in 1..=prefetch_ahead {
        let idx = from_index + i;
        if let Some(track) = queue.get(idx) {
            to_fetch.push((track.video_id.clone(), track.thumbnail_url.clone()));
        }
    }
    drop(queue);

    if to_fetch.is_empty() {
        return;
    }

    let state = state.clone();
    gio::spawn_blocking(move || {
        for (vid, _thumb_url) in &to_fetch {
            // Skip if already in offline cache or temp cache
            {
                let guard = state.lock().unwrap();
                if guard.cache.is_cached(vid) || guard.cache.get_temp_audio(vid).is_some() {
                    continue;
                }
            }

            // Get streaming URL (no playback reporting)
            let url = {
                let Some(client) = app_client(&state) else {
                    return;
                };
                match client.get_audio_url_no_track(vid) {
                    Ok(u) => u,
                    Err(e) => {
                        eprintln!("[PREFETCH] failed URL for {vid}: {e}");
                        continue;
                    }
                }
            };

            // Download audio bytes
            let audio = match DOWNLOAD_CLIENT
                .get(&url)
                .header("Range", "bytes=0-")
                .send()
                .and_then(|r| r.bytes())
            {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[PREFETCH] download failed for {vid}: {e}");
                    continue;
                }
            };

            // Write to temp dir (not pinned, not in offline DB)
            let guard = state.lock().unwrap();
            guard.cache.store_temp_audio(vid, &audio);
        }
    });
}

/// Set the queue to the given tracks and start playing from index 0.
/// If `shuffle` is true, the tracks are shuffled first.
fn play_all(tracks: &[Track], shuffle: bool, state: &Arc<Mutex<AppState>>, ps: &Rc<PlayerState>) {
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
    set_queue_context(ps, None);

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

fn cache_track_metadata(state: &Arc<Mutex<AppState>>, track: &Track) {
    let guard = state.lock().unwrap();
    let artist = if track.artists.is_empty() {
        "Unknown artist".to_string()
    } else {
        track
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = guard.cache.record_play(
        &track.video_id,
        &track.title,
        &artist,
        history_thumbnail_candidate(track.thumbnail_url.as_deref()),
        0,
    );
}

fn ensure_track_available_offline(
    state: &Arc<Mutex<AppState>>,
    track: &Track,
    pin: bool,
    log_prefix: &str,
) -> bool {
    let vid = &track.video_id;

    {
        let guard = state.lock().unwrap();
        if guard.cache.is_cached(vid) {
            if pin {
                let _ = guard.cache.pin_offline(vid);
            }
            drop(guard);
            let _ = ensure_offline_sponsorblock_cache(state, vid);
            return true;
        }
    }

    cache_track_metadata(state, track);

    let url = {
        let Some(client) = app_client(state) else {
            return false;
        };
        match client.get_audio_url_no_track(vid) {
            Ok(url) => url,
            Err(err) => {
                eprintln!("[{log_prefix}] failed URL for {vid}: {err}");
                return false;
            }
        }
    };

    let audio = match DOWNLOAD_CLIENT
        .get(&url)
        .header("Range", "bytes=0-")
        .send()
        .and_then(|r| r.bytes())
    {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("[{log_prefix}] download failed for {vid}: {err}");
            return false;
        }
    };

    let art = track
        .thumbnail_url
        .as_ref()
        .and_then(|url| fetch_thumbnail_bytes(url));

    {
        let guard = state.lock().unwrap();
        let _ = guard.cache.store_audio(vid, &audio, art.as_deref());
        if pin {
            let _ = guard.cache.pin_offline(vid);
        }
    }
    let _ = ensure_offline_sponsorblock_cache(state, vid);
    true
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
                eprintln!("[DOWNLOAD] {}/{count} downloading {vid}...", i + 1);
                if ensure_track_available_offline(&state, track, true, "DOWNLOAD") {
                    eprintln!("[DOWNLOAD] {}/{count} ready: {vid}", i + 1);
                }
            }

            // Store playlist metadata + cover art if this is a playlist download
            if let Some(meta) = &playlist_meta {
                // Download playlist cover art to disk
                let cover_art_path = if let Some(url) = &meta.thumbnail_url {
                    fetch_thumbnail_bytes(url).map(|bytes| {
                        let guard = state.lock().unwrap();
                        let p = guard
                            .cache
                            .art_dir
                            .join(format!("playlist_{}.jpg", meta.playlist_id));
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
                eprintln!(
                    "[DOWNLOAD] stored playlist meta: {} ({} tracks)",
                    meta.title,
                    video_ids.len()
                );
            }

            eprintln!("[DOWNLOAD] finished all {count} tracks");
        })
        .await
        .ok();
    });
}

/// Render the queue panel with chip filters and track list.
fn render_queue_panel(ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    ps.queue_panel.set_visible(true);
    clear_queue_drop_preview(ps);
    ps.queue_drag_source_index.set(None);

    let queue = ps.queue.borrow();
    let current_idx = ps.queue_index.get();
    let can_dismiss = current_idx.saturating_add(1) < queue.len();
    ps.queue_dismiss_btn.set_sensitive(can_dismiss);

    // -- Chips --
    clear_box(&ps.chip_bar);
    let chips = radio_chips(ps);
    for chip in &chips {
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
    // -- Track list --
    clear_box(&ps.queue_list_box);
    let radio_tail_start = if is_radio_queue(ps) {
        let touched_len = radio_touched_len(ps).unwrap_or(0).min(queue.len());
        (touched_len < queue.len()).then_some(touched_len)
    } else {
        None
    };

    for (i, track) in queue.iter().enumerate() {
        if radio_tail_start == Some(i) {
            ps.queue_list_box.append(&build_queue_virtual_divider());
        }

        let preview = QueuePreviewData {
            title: track.title.clone(),
            subtitle: artist_string(track),
            thumbnail_url: track.thumbnail_url.clone(),
            duration: track.duration.clone(),
        };
        let row = build_queue_row_visual(&preview, i == current_idx, true, None);

        // Wrap row content in a GtkBox (not Button) to avoid drag conflicts
        let row_wrapper = GtkBox::new(Orientation::Vertical, 0);
        row_wrapper.add_css_class("queue-row-btn");
        row_wrapper.set_hexpand(true);
        row_wrapper.set_halign(Align::Fill);
        if let Some(tail_start) = radio_tail_start {
            if i >= tail_start {
                row_wrapper.add_css_class("queue-row-radio-generated");
                if i == tail_start {
                    row_wrapper.add_css_class("queue-row-radio-generated-first");
                }
            }
        }
        row_wrapper.append(&row);

        // Click to play
        let click = GestureClick::new();
        click.set_button(1);
        {
            let state = state.clone();
            let ps = ps.clone();
            click.connect_released(move |gesture, _, _, _| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                play_from_queue(&state, &ps, i);
            });
        }
        row_wrapper.add_controller(click);

        // Right-click context menu
        if i != current_idx {
            let popover = Popover::new();
            popover.add_css_class("context-menu");
            popover.set_has_arrow(false);
            popover.set_position(gtk::PositionType::Bottom);

            let pop_box = GtkBox::new(Orientation::Vertical, 0);

            append_download_action(&pop_box, track, state, &popover);

            let remove_btn = Button::with_label("Remove from queue");
            remove_btn.add_css_class("popover-action-btn");
            {
                let ps = ps.clone();
                let state = state.clone();
                let popover = popover.clone();
                remove_btn.connect_clicked(move |_| {
                    popover.popdown();
                    queue_remove(i, &ps, &state);
                });
            }
            pop_box.append(&remove_btn);

            popover.set_child(Some(&pop_box));
            popover.set_parent(&row_wrapper);

            // On open: dim all siblings, highlight this row
            {
                let queue_list = ps.queue_list_box.clone();
                let row_wrapper_ref = row_wrapper.clone();
                popover.connect_notify_local(Some("visible"), move |pop, _| {
                    if pop.is_visible() {
                        queue_list.add_css_class("queue-ctx-active");
                        row_wrapper_ref.add_css_class("queue-row-focused");
                    } else {
                        queue_list.remove_css_class("queue-ctx-active");
                        row_wrapper_ref.remove_css_class("queue-row-focused");
                    }
                });
            }

            let rc_gesture = GestureClick::new();
            rc_gesture.set_button(3);
            {
                let popover = popover.clone();
                rc_gesture.connect_pressed(move |_, _, _, _| {
                    popover.popup();
                });
            }
            row_wrapper.add_controller(rc_gesture);
        }

        // Drag source
        let drag_source = DragSource::new();
        drag_source.set_actions(gtk::gdk::DragAction::MOVE);
        {
            let payload = QueueDragPayload::QueueRow { index: i };
            drag_source.connect_prepare(move |_, _, _| queue_drag_provider(&payload));
        }
        {
            let ps = ps.clone();
            let row_wrapper_ref = row_wrapper.clone();
            let preview_payload = QueueDragPayload::QueueRow { index: i };
            drag_source.connect_drag_begin(move |_, drag| {
                if let Some(preview) = queue_preview_data_for_payload(&ps, &preview_payload) {
                    let (width, height) = drag_preview_size(&ps);
                    set_drag_icon_from_preview(drag, &preview, width, height);
                }
                ps.queue_drag_source_index.set(Some(i));
                ps.queue_drag_preview_payload
                    .replace(Some(preview_payload.clone()));
                ps.queue_list_box.add_css_class("queue-drag-active");
                row_wrapper_ref.add_css_class("queue-row-dragging");
            });
        }
        {
            let ps = ps.clone();
            let row_wrapper_ref = row_wrapper.clone();
            drag_source.connect_drag_end(move |_, _, _| {
                row_wrapper_ref.remove_css_class("queue-row-dragging");
                ps.queue_drag_source_index.set(None);
                ps.queue_drag_preview_payload.replace(None);
                ps.queue_list_box.remove_css_class("queue-drag-active");
                clear_queue_drop_preview(&ps);
            });
        }
        row_wrapper.add_controller(drag_source);

        // Drop target
        let drop_target = DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
        {
            let ps = ps.clone();
            let row_wrapper_ref = row_wrapper.clone();
            drop_target.connect_enter(move |_, _, y| {
                let raw_slot = preview_slot_for_pointer(i, y, row_wrapper_ref.allocated_height());
                set_queue_drop_active(&ps, true);
                let preview_slot = normalized_preview_slot(&ps, raw_slot);
                set_queue_drop_preview(&ps, preview_slot);
                gtk::gdk::DragAction::MOVE
            });
        }
        {
            let ps = ps.clone();
            let row_wrapper_ref = row_wrapper.clone();
            drop_target.connect_motion(move |_, _, y| {
                let raw_slot = preview_slot_for_pointer(i, y, row_wrapper_ref.allocated_height());
                set_queue_drop_active(&ps, true);
                let preview_slot = normalized_preview_slot(&ps, raw_slot);
                set_queue_drop_preview(&ps, preview_slot);
                gtk::gdk::DragAction::MOVE
            });
        }
        {
            let ps = ps.clone();
            drop_target.connect_leave(move |_| {
                clear_queue_drop_preview(&ps);
                set_queue_drop_active(&ps, false);
            });
        }
        {
            let ps = ps.clone();
            let state = state.clone();
            let row_wrapper_ref = row_wrapper.clone();
            drop_target.connect_drop(move |_, value, _, y| {
                let Some(payload) = parse_queue_drag_payload(value) else {
                    return false;
                };
                let raw_slot = preview_slot_for_pointer(i, y, row_wrapper_ref.allocated_height());
                clear_queue_drop_preview(&ps);
                set_queue_drop_active(&ps, false);
                handle_queue_drop_payload(payload, raw_slot, &ps, &state)
            });
        }
        row_wrapper.add_controller(drop_target);

        ps.queue_list_box.append(&row_wrapper);
    }
}

/// Switch radio filter (chip click).
fn load_radio_filter(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    playlist_id: &str,
    params: Option<&str>,
) {
    let state = state.clone();
    let ps = ps.clone();
    let source_video_id = radio_source_video_id(&ps).or_else(|| {
        let q = ps.queue.borrow();
        q.first().map(|track| track.video_id.clone())
    });
    let Some(source_video_id) = source_video_id else {
        return;
    };
    let prefix_len = radio_touched_len(&ps)
        .unwrap_or_else(|| ps.queue_index.get().saturating_add(1))
        .max(1);
    let source_track = {
        let q = ps.queue.borrow();
        q.iter()
            .find(|track| track.video_id == source_video_id)
            .cloned()
            .or_else(|| q.first().cloned())
    };
    let playlist_id = playlist_id.to_string();
    let params = params.map(String::from);

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let vid = source_video_id.clone();
        let plid = playlist_id.clone();
        let p = params.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_radio(&vid, Some(&plid), p.as_deref()))
        })
        .await;

        if let Ok(Some(Ok(mut radio))) = result {
            // Ensure the source song is present if the API omits it.
            let idx = radio
                .tracks
                .iter()
                .position(|track| track.video_id == source_video_id);
            if let Some(i) = idx {
                if i > 0 {
                    let t = radio.tracks.remove(i);
                    radio.tracks.insert(0, t);
                }
            } else if let Some(t) = source_track {
                radio.tracks.insert(0, t);
            }

            replace_radio_tail(&ps, prefix_len, radio.tracks, radio.chips);
            render_queue_panel(&ps, &state);
            sync_remote_queue(&ps);
            sync_mpris_transport(&ps, &state);
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

    // Keep the sidebar aligned with the full history payload we preload.
    let mut count = 0;
    let mut child = recent_list.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        count += 1;
        if count > SIDEBAR_HISTORY_LIMIT {
            recent_list.remove(&c);
        }
        child = next;
    }
}

/// Sync the offline cache: download top-100 most played songs, evict the rest.
/// Acquires the AppState lock briefly per-operation so playback is never blocked.
fn sync_downloaded_playlists(state: &Arc<Mutex<AppState>>) {
    let offline_playlists = {
        let guard = state.lock().unwrap();
        guard.cache.offline_playlists().unwrap_or_default()
    };
    if offline_playlists.is_empty() {
        return;
    }

    let Some(client) = app_client(state) else {
        return;
    };

    for (playlist_id, title, thumbnail_url, subtitle) in offline_playlists {
        let local_ids = {
            let guard = state.lock().unwrap();
            guard
                .cache
                .offline_playlist_track_ids(&playlist_id)
                .unwrap_or_default()
        };

        let upstream_tracks = match client.get_playlist(&playlist_id) {
            Ok(tracks) => tracks,
            Err(err) => {
                eprintln!("[PLAYLIST_SYNC] failed to fetch {playlist_id}: {err}");
                continue;
            }
        };
        let upstream_ids: Vec<String> = upstream_tracks
            .iter()
            .map(|track| track.video_id.clone())
            .collect();

        if upstream_ids == local_ids {
            continue;
        }

        let local_set = local_ids.iter().cloned().collect::<HashSet<_>>();
        let upstream_set = upstream_ids.iter().cloned().collect::<HashSet<_>>();
        let removed_ids = local_ids
            .iter()
            .filter(|video_id| !upstream_set.contains(*video_id))
            .cloned()
            .collect::<Vec<_>>();

        for track in upstream_tracks
            .iter()
            .filter(|track| !local_set.contains(&track.video_id))
        {
            eprintln!(
                "[PLAYLIST_SYNC] caching new track for {}: {}",
                playlist_id, track.video_id
            );
            let _ = ensure_track_available_offline(state, track, true, "PLAYLIST_SYNC");
        }

        {
            let guard = state.lock().unwrap();
            let _ = guard.cache.store_offline_playlist(
                &playlist_id,
                &title,
                thumbnail_url.as_deref(),
                subtitle.as_deref(),
                &upstream_ids,
            );
        }

        for video_id in removed_ids {
            let still_needed = {
                let guard = state.lock().unwrap();
                guard
                    .cache
                    .is_track_in_any_offline_playlist(&video_id)
                    .unwrap_or(false)
            };
            if !still_needed {
                let guard = state.lock().unwrap();
                let _ = guard.cache.remove_cached(&video_id);
            }
        }

        eprintln!("[PLAYLIST_SYNC] refreshed downloaded playlist: {playlist_id}");
    }
}

fn sync_offline_cache(state: &Arc<Mutex<AppState>>) {
    sync_downloaded_playlists(state);

    let (enabled, cache_target, top, cached, pinned) = {
        let guard = state.lock().unwrap();
        let enabled = guard.settings.auto_manage_offline_cache;
        let cache_target = guard.settings.offline_cache_target as usize;
        if !enabled {
            return;
        }
        let top = match guard
            .cache
            .auto_offline_candidates(&guard.settings, cache_target)
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[CACHE] failed to compute offline candidates: {e}");
                return;
            }
        };
        let cached = guard.cache.cached_ids().unwrap_or_default();
        let pinned = guard.cache.pinned_ids().unwrap_or_default();
        (enabled, cache_target, top, cached, pinned)
    }; // lock released

    if !enabled {
        return;
    }

    // Evict songs no longer in top configured set (skip pinned/manually downloaded)
    for id in &cached {
        if !top.contains(id) && !pinned.contains(id) {
            let guard = state.lock().unwrap();
            let _ = guard.cache.remove_cached(id);
        }
    }

    // Download songs that are in top 100 but not cached
    let to_download: Vec<String> = top
        .into_iter()
        .take(cache_target)
        .filter(|id| !cached.contains(id))
        .collect();

    if to_download.is_empty() {
        return;
    }

    for vid in to_download {
        // Get streaming URL (brief lock)
        let url = {
            let Some(client) = app_client(state) else {
                return;
            };
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
                eprintln!(
                    "[CACHE] {vid} response: {status}, content-length: {:?}",
                    r.content_length()
                );
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
        let art = thumb_url.and_then(|url| fetch_thumbnail_bytes(&url));

        // Store to disk + DB (brief lock)
        {
            let guard = state.lock().unwrap();
            let _ = guard.cache.store_audio(&vid, &audio, art.as_deref());
        }
        let _ = ensure_offline_sponsorblock_cache(state, &vid);
    }
}

fn artist_string(track: &Track) -> String {
    if track.artists.is_empty() {
        "Unknown artist".to_string()
    } else {
        track
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn clear_box(container: &GtkBox) {
    container.remove_css_class("home-view-active");
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn queue_preview_data_for_payload(
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
    }
}

fn build_queue_row_visual(
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

fn build_queue_virtual_divider() -> GtkBox {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.add_css_class("queue-virtual-divider");

    let label = Label::new(Some("Generated from this radio"));
    label.add_css_class("queue-virtual-divider-label");
    label.set_halign(Align::Start);
    root.append(&label);

    root
}

fn is_queue_row_widget(widget: &gtk::Widget) -> bool {
    widget.has_css_class("queue-row-btn")
}

fn refresh_queue_drop_indicator(ps: &PlayerState) {
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

fn clear_queue_drop_preview(ps: &PlayerState) {
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

fn set_queue_drop_active(ps: &PlayerState, active: bool) {
    if active {
        ps.queue_panel.add_css_class("queue-drop-active");
    } else {
        ps.queue_panel.remove_css_class("queue-drop-active");
        ps.queue_list_box.remove_css_class("queue-drop-empty");
    }
}

fn set_queue_drop_preview(ps: &PlayerState, slot: Option<usize>) {
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

fn preview_slot_for_pointer(row_index: usize, y: f64, row_height: i32) -> usize {
    if y < (row_height.max(1) as f64 / 2.0) {
        row_index
    } else {
        row_index + 1
    }
}

fn preview_slot_for_queue_y(ps: &PlayerState, y: f64) -> usize {
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

fn normalized_preview_slot(ps: &PlayerState, raw_slot: usize) -> Option<usize> {
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

fn queue_drag_provider(payload: &QueueDragPayload) -> Option<gtk::gdk::ContentProvider> {
    let json = serde_json::to_string(payload).ok()?;
    Some(gtk::gdk::ContentProvider::for_value(&json.to_value()))
}

fn drag_preview_size(ps: &PlayerState) -> (i32, i32) {
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

fn build_drag_preview_widget(preview: &QueuePreviewData, width: i32, height: i32) -> GtkBox {
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

fn set_drag_icon_from_preview(
    drag: &gtk::gdk::Drag,
    preview: &QueuePreviewData,
    width: i32,
    height: i32,
) {
    let icon = gtk::DragIcon::for_drag(drag);
    let widget = build_drag_preview_widget(preview, width, height);
    icon.set_child(Some(&widget));
}

fn parse_queue_drag_payload(value: &glib::Value) -> Option<QueueDragPayload> {
    let json = value.get::<String>().ok()?;
    serde_json::from_str(&json).ok()
}

fn queue_accepts_external_dnd(ps: &PlayerState) -> bool {
    ps.queue_panel.is_visible()
}

fn attach_queue_drag_source(
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
            clear_queue_drop_preview(&ps);
            set_queue_drop_active(&ps, false);
        });
    }

    widget.as_ref().add_controller(drag_source);
}

fn insert_tracks_into_queue(
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

fn resolve_dragged_playlist_tracks(
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
        _ => Ok(Vec::new()),
    }
}

fn handle_queue_drop_payload(
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
            mark_radio_touched_through(ps, insert_at);
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
        payload @ QueueDragPayload::Playlist { .. } => {
            let ps = ps.clone();
            let state = state.clone();
            glib::spawn_future_local(async move {
                let payload_bg = payload.clone();
                let state_bg = state.clone();
                let result = gio::spawn_blocking(move || {
                    resolve_dragged_playlist_tracks(&payload_bg, &state_bg)
                })
                .await;

                match result {
                    Ok(Ok(tracks)) => insert_tracks_into_queue(tracks, raw_slot, true, &ps, &state),
                    Ok(Err(err)) => eprintln!("[QUEUE] failed to expand playlist drop: {err}"),
                    Err(err) => eprintln!("[QUEUE] failed to load playlist drop: {err:?}"),
                }
            });
            true
        }
    }
}
