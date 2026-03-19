use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{LazyLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use dbus::ffidisp::{BusType as DbusBusType, Connection as DbusConnection};
use gstreamer::ClockTime;
use gstreamer::glib;
use keyring::Entry;
use mpris::{PlaybackStatus as PeerPlaybackStatus, Player as PeerMprisPlayer};
use mpris_server::{
    LoopStatus as MprisLoopStatus, Metadata, PlaybackStatus as MprisPlaybackStatus,
    Player as MprisPlayer, Time as MprisTime, TrackId,
};
use reqwest::blocking::Client as BlockingClient;
use zbus::blocking::{Connection, Proxy};
use zbus::interface;

use crate::cache::{AppSettings, MusicCache};
use crate::ipc::{
    ActiveTrackSnapshot, DAEMON_BUS_NAME, DAEMON_OBJECT_PATH, DaemonRequest, DaemonResponse,
    PlaybackSnapshot, RepeatMode,
};
use crate::models::{RadioVirtualPlaylist, Track};
use crate::playback_engine::PlaybackEngine;
use crate::sponsorblock::{
    SponsorBlockClient, SponsorSegment, default_skip_categories, filter_segments_for_categories,
    sanitize_categories,
};
use crate::ytmusic::YtMusicClient;
use crate::ytmusic::client::{SongRating, TrackingInfo};

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

const KEYRING_SERVICE: &str = "com.static.music";
const KEYRING_USER: &str = "cookie";
const KEYRING_VR_TOKEN: &str = "vr_refresh_token";
const KEYRING_STORAGE_ROOT: &str = "storage_root";
const UI_APP_OBJECT_PATH: &str = "/com/static/music";
const MIN_LISTEN_SECS: u64 = 10;
const PAUSED_SHUTDOWN_AFTER: Duration = Duration::from_secs(20 * 60);
const PLAYBACK_START_TIMEOUT: Duration = Duration::from_secs(12);
const PLAYBACK_START_RETRY_LIMIT: u8 = 1;
const PLAYBACK_STARTED_POSITION_THRESHOLD_MILLIS: u64 = 1_000;
const PREVIOUS_TRACK_THRESHOLD_MILLIS: u64 = 3_000;
const EXTERNAL_MPRIS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const EXTERNAL_MPRIS_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(30);
const DBUS_SERVICE: &str = "org.freedesktop.DBus";
const DBUS_OBJECT_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const STATIC_MPRIS_BUS_NAME: &str = "org.mpris.MediaPlayer2.static";
const PEER_MPRIS_TIMEOUT_MS: i32 = 500;

static DOWNLOAD_CLIENT: LazyLock<BlockingClient> = LazyLock::new(|| {
    BlockingClient::builder()
        .timeout(Duration::from_secs(120))
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:136.0) Gecko/20100101 Firefox/136.0")
        .build()
        .unwrap()
});

static THUMB_CLIENT: LazyLock<BlockingClient> = LazyLock::new(|| {
    BlockingClient::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
});

struct ActiveTrack {
    video_id: String,
    title: String,
    artist: String,
    thumb_url: Option<String>,
    playback_source: PlaybackSource,
    rating: SongRating,
    tracking: Option<TrackingInfo>,
    started_at: Instant,
    last_position_millis: u64,
    startup_retries: u8,
    awaiting_fresh_position: bool,
    bootstrapped_watchtime: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackSource {
    OfflineCache,
    TempPrefetch,
    RemoteStream,
}

struct DaemonState {
    player: PlaybackEngine,
    cache: MusicCache,
    settings: AppSettings,
    client: Option<Rc<YtMusicClient>>,
    queue: Vec<Track>,
    queue_index: usize,
    radio_virtual_playlist: Option<RadioVirtualPlaylist>,
    repeat_mode: RepeatMode,
    active_track: Option<ActiveTrack>,
    sponsor_segments: Vec<SponsorSegment>,
    sponsor_segments_reported: HashSet<String>,
    paused_since: Option<Instant>,
    radio_append_in_flight: bool,
    last_radio_append_anchor: Option<String>,
    background_tx: mpsc::Sender<BackgroundEvent>,
    deferred_actions: VecDeque<DeferredAction>,
    offline_sync_in_flight: bool,
    offline_sync_pending: bool,
    last_duration_micros: Cell<Option<i64>>,
    mpris: RefCell<Option<Rc<DaemonMprisBridge>>>,
    peer_mpris: ExternalMprisCoordinator,
    shutting_down: Cell<bool>,
}

struct ExternalMprisCoordinator {
    last_poll_at: Option<Instant>,
    last_error_log_at: Option<Instant>,
    playing_services: Vec<String>,
    last_logged_playing_services: Vec<String>,
    waiting_for_services: Vec<String>,
}

impl ExternalMprisCoordinator {
    fn new() -> Self {
        Self {
            last_poll_at: None,
            last_error_log_at: None,
            playing_services: Vec::new(),
            last_logged_playing_services: Vec::new(),
            waiting_for_services: Vec::new(),
        }
    }

    fn poll_if_due(&mut self) {
        if self
            .last_poll_at
            .is_some_and(|last_poll_at| last_poll_at.elapsed() < EXTERNAL_MPRIS_POLL_INTERVAL)
        {
            return;
        }
        self.last_poll_at = Some(Instant::now());
        self.poll_now();
    }

    fn poll_now(&mut self) {
        match self.scan_other_playing_services() {
            Ok(services) => {
                self.playing_services = services;
                self.log_playing_services_snapshot_change();
            }
            Err(err) => {
                self.log_error(format!("[MPRIS] failed to inspect peer players: {err}"));
                self.playing_services.clear();
                self.log_playing_services_snapshot_change();
            }
        }
    }

    fn playing_services(&self) -> &[String] {
        &self.playing_services
    }

    fn pause_other_playing_services(&mut self) -> Vec<String> {
        self.poll_now();
        let mut paused = Vec::new();
        for bus_name in self.playing_services.clone() {
            match peer_player_for_bus(&bus_name) {
                Ok(player) => match player.get_playback_status() {
                    Ok(PeerPlaybackStatus::Playing) => {
                        if let Err(err) = player.pause() {
                            self.log_error(format!(
                                "[MPRIS] failed to pause peer player {bus_name}: {err}"
                            ));
                        } else {
                            paused.push(bus_name);
                        }
                    }
                    Ok(_) => {}
                    Err(err) => self.log_error(format!(
                        "[MPRIS] failed to read PlaybackStatus from {bus_name}: {err}"
                    )),
                }
                Err(err) => self.log_error(format!(
                    "[MPRIS] failed to create peer client for {bus_name}: {err}"
                )),
            }
        }

        self.playing_services
            .retain(|service| !paused.iter().any(|paused_service| paused_service == service));
        paused
    }

    fn scan_other_playing_services(&mut self) -> Result<Vec<String>> {
        let services = list_other_mpris_services()?;
        let mut playing = Vec::new();

        for bus_name in services {
            match peer_player_for_bus(&bus_name) {
                Ok(player) => match player.get_playback_status() {
                    Ok(PeerPlaybackStatus::Playing) => playing.push(bus_name),
                    Ok(_) => {}
                    Err(err) => self.log_error(format!(
                        "[MPRIS] failed to read PlaybackStatus from {bus_name}: {err}"
                    )),
                },
                Err(err) => self.log_error(format!(
                    "[MPRIS] failed to create peer client for {bus_name}: {err}"
                )),
            }
        }

        Ok(playing)
    }

    fn set_waiting_for_services(&mut self, services: &[String]) {
        self.waiting_for_services = services.to_vec();
    }

    fn clear_waiting(&mut self) {
        self.waiting_for_services.clear();
    }

    fn is_waiting_for_peer_pause(&self) -> bool {
        !self.waiting_for_services.is_empty()
    }

    fn waiting_for_services(&self) -> &[String] {
        &self.waiting_for_services
    }

    fn log_error(&mut self, message: String) {
        let should_log = self
            .last_error_log_at
            .is_none_or(|last_log_at| last_log_at.elapsed() >= EXTERNAL_MPRIS_ERROR_LOG_INTERVAL);
        if should_log {
            self.last_error_log_at = Some(Instant::now());
            eprintln!("{message}");
        }
    }

    fn log_playing_services_snapshot_change(&mut self) {
        if self.playing_services == self.last_logged_playing_services {
            return;
        }

        if self.playing_services.is_empty() {
            eprintln!("[MPRIS] peer playing set is now empty");
        } else {
            eprintln!(
                "[MPRIS] peer playing set changed: {}",
                self.playing_services.join(", ")
            );
        }

        self.last_logged_playing_services = self.playing_services.clone();
    }
}

struct CommandEnvelope {
    request: DaemonRequest,
    reply: mpsc::Sender<DaemonResponse>,
}

enum BackgroundEvent {
    TrackingReady {
        video_id: String,
        tracking: Option<TrackingInfo>,
    },
    SponsorSegmentsReady {
        video_id: String,
        segments: Vec<SponsorSegment>,
    },
    RadioExtensionReady {
        anchor_video_id: String,
        tracks: Vec<Track>,
        chips: Vec<crate::models::RadioChip>,
    },
    OfflineSyncFinished,
}

enum DeferredAction {
    StartPlaybackTracking {
        video_id: String,
        attempt: u8,
        not_before: Instant,
    },
}

struct StaticdDbus {
    command_tx: mpsc::Sender<CommandEnvelope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviousAction {
    RestartCurrent,
    PlayIndex(usize),
    Noop,
}

#[interface(name = "com.static.music.Staticd1")]
impl StaticdDbus {
    fn request_json(&self, request_json: &str) -> zbus::fdo::Result<String> {
        let request = serde_json::from_str(request_json)
            .map_err(|err| zbus::fdo::Error::Failed(format!("Invalid request: {err}")))?;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.command_tx
            .send(CommandEnvelope {
                request,
                reply: reply_tx,
            })
            .map_err(|_| zbus::fdo::Error::Failed("Daemon main loop is unavailable".into()))?;
        let response = reply_rx
            .recv()
            .unwrap_or_else(|_| DaemonResponse::Error("Daemon did not reply".to_string()));
        serde_json::to_string(&response)
            .map_err(|err| zbus::fdo::Error::Failed(format!("Failed to encode response: {err}")))
    }
}

struct DaemonMprisBridge {
    player: MprisPlayer,
}

impl DaemonMprisBridge {
    async fn new(state: &Rc<RefCell<DaemonState>>) -> Result<Rc<Self>, mpris_server::zbus::Error> {
        let player = MprisPlayer::builder("static")
            .identity("Static")
            .desktop_entry("com.static.music")
            .can_raise(true)
            .can_quit(true)
            .can_play(true)
            .can_pause(true)
            .can_seek(true)
            .can_control(true)
            .build()
            .await?;

        {
            let state = state.clone();
            player.connect_play_pause(move |_| {
                let mut state = state.borrow_mut();
                toggle_pause_for_user_action(&mut state);
            });
        }

        {
            let state = state.clone();
            player.connect_play(move |_| {
                let mut state = state.borrow_mut();
                if state.active_track.is_some() && state.player.is_paused() {
                    resume_for_user_action(&mut state);
                }
            });
        }

        {
            let state = state.clone();
            player.connect_pause(move |_| {
                let mut state = state.borrow_mut();
                if state.active_track.is_some() && !state.player.is_paused() {
                    state.peer_mpris.clear_waiting();
                    state.player.pause();
                    sync_mpris(&state);
                }
            });
        }

        {
            let state = state.clone();
            player.connect_stop(move |_| {
                stop_playback(&mut state.borrow_mut(), false);
            });
        }

        {
            let state = state.clone();
            player.connect_next(move |_| {
                let _ = advance_queue(&mut state.borrow_mut(), true);
            });
        }

        {
            let state = state.clone();
            player.connect_previous(move |_| {
                let mut state = state.borrow_mut();
                let _ = handle_previous(&mut state);
            });
        }

        {
            let state = state.clone();
            player.connect_seek(move |_, offset| {
                let state = state.borrow();
                let Some(current) = state.player.position() else {
                    return;
                };
                let current_us = (current.nseconds() / 1_000) as i64;
                let target_us = (current_us + offset.as_micros()).max(0) as u64;
                let target = ClockTime::from_useconds(target_us);
                state.player.seek(target);
                signal_mpris_seeked(&state, target);
                sync_mpris(&state);
            });
        }

        {
            let state = state.clone();
            player.connect_set_position(move |_, _, position| {
                let state = state.borrow_mut();
                let target = ClockTime::from_useconds(position.as_micros().max(0) as u64);
                state.player.seek(target);
                signal_mpris_seeked(&state, target);
                sync_mpris(&state);
            });
        }

        player.connect_raise(move |_| {
            let _ = reopen_ui_window();
        });

        {
            let state = state.clone();
            player.connect_quit(move |_| {
                let mut state = state.borrow_mut();
                stop_playback(&mut state, true);
                state.shutting_down.set(true);
            });
        }

        {
            let state = state.clone();
            player.connect_set_loop_status(move |_, status| {
                let mut state = state.borrow_mut();
                state.repeat_mode = match status {
                    MprisLoopStatus::Track => RepeatMode::One,
                    MprisLoopStatus::Playlist => RepeatMode::All,
                    _ => RepeatMode::Off,
                };
                sync_mpris(&state);
            });
        }

        {
            let state = state.clone();
            player.connect_set_volume(move |_, volume| {
                let state = state.borrow_mut();
                state.player.set_volume(volume);
                sync_mpris(&state);
            });
        }

        let run_task = player.run();
        glib::spawn_future_local(async move {
            run_task.await;
        });

        Ok(Rc::new(Self { player }))
    }

    fn sync_all(self: &Rc<Self>, state: &DaemonState) {
        self.sync_transport(state);
        self.sync_metadata(state);
        self.sync_position(state);
        self.sync_volume(state);
    }

    fn sync_transport(self: &Rc<Self>, state: &DaemonState) {
        let bridge = self.clone();
        let snapshot = state_snapshot(state);
        let playback_status = if snapshot.current_track.is_none() {
            MprisPlaybackStatus::Stopped
        } else if snapshot.paused {
            MprisPlaybackStatus::Paused
        } else {
            MprisPlaybackStatus::Playing
        };
        let loop_status = match snapshot.repeat_mode {
            RepeatMode::Off => MprisLoopStatus::None,
            RepeatMode::All => MprisLoopStatus::Playlist,
            RepeatMode::One => MprisLoopStatus::Track,
        };
        let queue_len = effective_queue_len(
            snapshot.queue.len(),
            snapshot.repeat_mode,
            snapshot
                .radio_virtual_playlist
                .as_ref()
                .map(|radio| radio.touched_len),
        );
        let can_go_next = snapshot.current_track.is_some()
            && (snapshot.queue_index + 1 < queue_len || queue_len > 0);
        let can_go_previous = matches!(
            previous_action(
                queue_len,
                snapshot.queue_index,
                snapshot.repeat_mode,
                snapshot.position_millis,
                snapshot.current_track.is_some(),
            ),
            PreviousAction::RestartCurrent | PreviousAction::PlayIndex(_)
        );
        let can_seek = snapshot.current_track.is_some();
        let can_pause = snapshot.current_track.is_some();
        let can_play = snapshot.current_track.is_some();

        glib::spawn_future_local(async move {
            let _ = bridge.player.set_playback_status(playback_status).await;
            let _ = bridge.player.set_loop_status(loop_status).await;
            let _ = bridge.player.set_can_go_next(can_go_next).await;
            let _ = bridge.player.set_can_go_previous(can_go_previous).await;
            let _ = bridge.player.set_can_seek(can_seek).await;
            let _ = bridge.player.set_can_pause(can_pause).await;
            let _ = bridge.player.set_can_play(can_play).await;
        });
    }

    fn sync_metadata(self: &Rc<Self>, state: &DaemonState) {
        let bridge = self.clone();
        let metadata = build_mpris_metadata(state);
        glib::spawn_future_local(async move {
            let _ = bridge.player.set_metadata(metadata).await;
        });
    }

    fn sync_position(&self, state: &DaemonState) {
        let position_us = state
            .player
            .position()
            .map(|position| (position.nseconds() / 1_000) as i64)
            .unwrap_or(0);
        self.player
            .set_position(MprisTime::from_micros(position_us));
    }

    fn emit_seeked(self: &Rc<Self>, position_us: i64) {
        let bridge = self.clone();
        glib::spawn_future_local(async move {
            let _ = bridge
                .player
                .seeked(MprisTime::from_micros(position_us))
                .await;
        });
    }

    fn sync_volume(self: &Rc<Self>, state: &DaemonState) {
        let bridge = self.clone();
        let volume = state.player.volume();
        glib::spawn_future_local(async move {
            let _ = bridge.player.set_volume(volume).await;
        });
    }
}

pub fn run() -> Result<()> {
    if let Ok(exe) = std::env::current_exe() {
        if let Err(err) = ensure_desktop_entry_for_current_exe(&exe) {
            eprintln!("[STATICD] failed to refresh desktop entry: {err}");
        }
    }

    let storage_root = get_storage_root();
    let cache = MusicCache::new_with_storage_root(storage_root.as_deref())
        .context("Failed to initialise music cache")?;
    let settings = cache.load_settings().unwrap_or_default();
    let (command_tx, command_rx) = mpsc::channel::<CommandEnvelope>();
    let (background_tx, background_rx) = mpsc::channel::<BackgroundEvent>();
    let state = Rc::new(RefCell::new(DaemonState {
        player: PlaybackEngine::new(),
        cache,
        settings,
        client: None,
        queue: Vec::new(),
        queue_index: 0,
        radio_virtual_playlist: None,
        repeat_mode: RepeatMode::Off,
        active_track: None,
        sponsor_segments: Vec::new(),
        sponsor_segments_reported: HashSet::new(),
        paused_since: None,
        radio_append_in_flight: false,
        last_radio_append_anchor: None,
        background_tx,
        deferred_actions: VecDeque::new(),
        offline_sync_in_flight: false,
        offline_sync_pending: false,
        last_duration_micros: Cell::new(None),
        mpris: RefCell::new(None),
        peer_mpris: ExternalMprisCoordinator::new(),
        shutting_down: Cell::new(false),
    }));

    {
        let mut state = state.borrow_mut();
        schedule_offline_cache_sync(&mut state);
    }

    {
        let state = state.clone();
        glib::spawn_future_local(async move {
            match DaemonMprisBridge::new(&state).await {
                Ok(bridge) => {
                    *state.borrow().mpris.borrow_mut() = Some(bridge.clone());
                    bridge.sync_all(&state.borrow());
                }
                Err(err) => eprintln!("[MPRIS] failed to initialize daemon bridge: {err}"),
            }
        });
    }

    let _dbus_connection = zbus::blocking::connection::Builder::session()
        .context("Failed to connect staticd to the session D-Bus")?
        .name(DAEMON_BUS_NAME)
        .context("Failed to request staticd D-Bus name")?
        .serve_at(
            DAEMON_OBJECT_PATH,
            StaticdDbus {
                command_tx: command_tx.clone(),
            },
        )
        .context("Failed to export staticd D-Bus interface")?
        .build()
        .context("Failed to start staticd D-Bus service")?;

    let main_loop = glib::MainLoop::new(None, false);
    let main_loop_handle = main_loop.clone();
    glib::timeout_add_local(Duration::from_millis(40), move || {
        while let Ok(envelope) = command_rx.try_recv() {
            let response = {
                let mut state = state.borrow_mut();
                handle_request(&mut state, envelope.request)
            };
            let _ = envelope.reply.send(response);
        }

        while let Ok(event) = background_rx.try_recv() {
            handle_background_event(&mut state.borrow_mut(), event);
        }

        let should_quit = {
            let mut state = state.borrow_mut();
            maintain_playback(&mut state);
            state.shutting_down.get()
        };

        if should_quit {
            main_loop_handle.quit();
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });

    main_loop.run();
    Ok(())
}

fn handle_request(state: &mut DaemonState, request: DaemonRequest) -> DaemonResponse {
    match request {
        DaemonRequest::Ping => DaemonResponse::Pong,
        DaemonRequest::GetState => DaemonResponse::State(state_snapshot(state)),
        DaemonRequest::PlayQueueAt {
            queue,
            index,
            radio_virtual_playlist,
            repeat_mode,
        } => {
            if !queue.is_empty() {
                pause_other_mpris_players(state);
            }
            match play_queue_at(state, queue, index, radio_virtual_playlist, repeat_mode) {
                Ok(()) => DaemonResponse::State(state_snapshot(state)),
                Err(err) => DaemonResponse::Error(err.to_string()),
            }
        }
        DaemonRequest::ReplaceQueue {
            queue,
            index,
            radio_virtual_playlist,
            repeat_mode,
        } => {
            state.queue = queue;
            state.queue_index = index.min(state.queue.len().saturating_sub(1));
            state.radio_virtual_playlist = radio_virtual_playlist;
            state.radio_append_in_flight = false;
            state.last_radio_append_anchor = None;
            state.repeat_mode = repeat_mode;
            prefetch_upcoming(state);
            sync_mpris(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::TogglePause => {
            toggle_pause_for_user_action(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::Pause => {
            state.peer_mpris.clear_waiting();
            state.player.pause();
            sync_mpris(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::Resume => {
            resume_for_user_action(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::Stop => {
            stop_playback(state, false);
            sync_mpris(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::Next => match advance_queue_for_user_action(state, true) {
            Ok(()) => DaemonResponse::State(state_snapshot(state)),
            Err(err) => DaemonResponse::Error(err.to_string()),
        },
        DaemonRequest::Previous => match handle_previous_for_user_action(state) {
            Ok(()) => DaemonResponse::State(state_snapshot(state)),
            Err(err) => DaemonResponse::Error(err.to_string()),
        },
        DaemonRequest::SeekToMillis { position_millis } => {
            let target = ClockTime::from_mseconds(position_millis);
            state.player.seek(target);
            signal_mpris_seeked(state, target);
            sync_mpris(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::SetVolume { volume } => {
            state.player.set_volume(volume.clamp(0.0, 1.0));
            sync_mpris(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::SetRepeatMode { repeat_mode } => {
            state.repeat_mode = repeat_mode;
            sync_mpris(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::LogMessage { source, message } => {
            match crate::logging::append_log(source, &message) {
                Ok(()) => DaemonResponse::Ok,
                Err(err) => DaemonResponse::Error(format!("Failed to write log: {err}")),
            }
        }
        DaemonRequest::ReloadSettings => {
            reload_settings(state);
            sync_mpris(state);
            DaemonResponse::State(state_snapshot(state))
        }
        DaemonRequest::Shutdown => {
            stop_playback(state, true);
            state.shutting_down.set(true);
            DaemonResponse::Ok
        }
    }
}

fn handle_background_event(state: &mut DaemonState, event: BackgroundEvent) {
    match event {
        BackgroundEvent::TrackingReady { video_id, tracking } => {
            if let Some(active) = state.active_track.as_mut() {
                if active.video_id == video_id {
                    active.tracking = tracking;
                }
            }
        }
        BackgroundEvent::SponsorSegmentsReady { video_id, segments } => {
            if state
                .active_track
                .as_ref()
                .is_some_and(|active| active.video_id == video_id)
            {
                let count = segments.len();
                state.sponsor_segments = segments;
                state.sponsor_segments_reported.clear();

                if count > 0 {
                    eprintln!(
                        "[SPONSORBLOCK] loaded {} skippable segments for {}",
                        count, video_id
                    );
                }
            }
        }
        BackgroundEvent::RadioExtensionReady {
            anchor_video_id,
            tracks,
            chips,
        } => {
            state.radio_append_in_flight = false;
            if state.repeat_mode == RepeatMode::All || state.radio_virtual_playlist.is_none() {
                if state.last_radio_append_anchor.as_deref() == Some(anchor_video_id.as_str()) {
                    state.last_radio_append_anchor = None;
                }
                return;
            }

            let queue_still_matches = state
                .queue
                .last()
                .is_some_and(|track| track.video_id == anchor_video_id);
            if !queue_still_matches {
                if state.last_radio_append_anchor.as_deref() == Some(anchor_video_id.as_str()) {
                    state.last_radio_append_anchor = None;
                }
                return;
            }

            let mut seen_ids = state
                .queue
                .iter()
                .map(|track| track.video_id.clone())
                .collect::<HashSet<_>>();
            let mut added = 0usize;
            for track in tracks {
                if seen_ids.insert(track.video_id.clone()) {
                    state.queue.push(track);
                    added += 1;
                }
            }

            if let Some(radio) = state.radio_virtual_playlist.as_mut() {
                if !chips.is_empty() {
                    radio.chips = chips;
                }
            }

            if added > 0 {
                sync_mpris(state);
            }
        }
        BackgroundEvent::OfflineSyncFinished => {
            state.offline_sync_in_flight = false;
            if state.offline_sync_pending {
                state.offline_sync_pending = false;
                schedule_offline_cache_sync(state);
            }
        }
    }
}

fn maintain_playback(state: &mut DaemonState) {
    if let Some(error) = state.player.take_error() {
        handle_playback_failure(state, error);
        return;
    }

    poll_external_mpris(state);
    maybe_resume_after_external_mpris(state);
    maybe_pause_for_external_mpris(state);

    if state.active_track.is_some() {
        if state.player.is_paused() {
            if state.paused_since.is_none() {
                state.paused_since = Some(Instant::now());
            }
        } else {
            state.paused_since = None;
        }
    } else {
        state.paused_since = None;
    }

    if state
        .paused_since
        .is_some_and(|paused_at| paused_at.elapsed() >= PAUSED_SHUTDOWN_AFTER)
    {
        stop_playback(state, true);
        state.shutting_down.set(true);
        return;
    }

    if let Some(active) = state.active_track.as_mut() {
        if let Some(position) = state.player.position() {
            active.last_position_millis = position.mseconds();
        }
    }

    let duration_micros = state
        .player
        .duration()
        .map(|duration| duration.useconds() as i64);
    if state.last_duration_micros.get() != duration_micros {
        state.last_duration_micros.set(duration_micros);
        sync_mpris(state);
    }

    if maybe_recover_stalled_playback_start(state) {
        return;
    }

    maybe_flush_watchtime_progress(state);
    maybe_apply_sponsorblock_skip(state);
    maybe_run_deferred_action(state);
    maybe_extend_radio_queue(state);

    let reached_track_end = matches!(
        (state.player.position(), state.player.duration()),
        (Some(position), Some(duration))
            if duration > ClockTime::from_seconds(1)
                && position + ClockTime::from_mseconds(500) >= duration
    );

    if reached_track_end {
        let queue_len = effective_queue_len(
            state.queue.len(),
            state.repeat_mode,
            state
                .radio_virtual_playlist
                .as_ref()
                .map(|radio| radio.touched_len),
        );
        match natural_end_target_index(queue_len, state.queue_index, state.repeat_mode) {
            Some(target) => {
                if let Err(err) = play_index(state, target) {
                    eprintln!("[DAEMON] failed to auto-advance: {err}");
                }
            }
            None => stop_playback(state, false),
        }
    }
}

fn play_queue_at(
    state: &mut DaemonState,
    queue: Vec<Track>,
    index: usize,
    radio_virtual_playlist: Option<RadioVirtualPlaylist>,
    repeat_mode: RepeatMode,
) -> Result<()> {
    if queue.is_empty() {
        stop_playback(state, false);
        return Ok(());
    }
    state.queue = queue;
    state.queue_index = index.min(state.queue.len().saturating_sub(1));
    state.radio_virtual_playlist = radio_virtual_playlist;
    state.radio_append_in_flight = false;
    state.last_radio_append_anchor = None;
    state.repeat_mode = repeat_mode;
    play_index(state, state.queue_index)
}

fn advance_queue(state: &mut DaemonState, forward: bool) -> Result<()> {
    if state.queue.is_empty() {
        return Ok(());
    }

    let target = queue_target_index(state, forward);

    match target {
        Some(index) => play_index(state, index),
        None => {
            stop_playback(state, false);
            Ok(())
        }
    }
}

fn advance_queue_for_user_action(state: &mut DaemonState, forward: bool) -> Result<()> {
    if queue_target_index(state, forward).is_some() {
        pause_other_mpris_players(state);
    }
    advance_queue(state, forward)
}

fn queue_target_index(state: &DaemonState, forward: bool) -> Option<usize> {
    let queue_len = effective_queue_len(
        state.queue.len(),
        state.repeat_mode,
        state
            .radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.touched_len),
    );
    if queue_len == 0 {
        return None;
    }

    if forward {
        if state.queue_index + 1 < queue_len {
            Some(state.queue_index + 1)
        } else if state.repeat_mode == RepeatMode::All {
            Some(0)
        } else if state.repeat_mode == RepeatMode::One {
            Some(state.queue_index)
        } else {
            None
        }
    } else if state.queue_index > 0 {
        Some(state.queue_index - 1)
    } else if state.repeat_mode == RepeatMode::All {
        Some(queue_len.saturating_sub(1))
    } else if state.repeat_mode == RepeatMode::One {
        Some(state.queue_index)
    } else {
        None
    }
}

fn effective_queue_len(
    queue_len: usize,
    repeat_mode: RepeatMode,
    radio_touched_len: Option<usize>,
) -> usize {
    if queue_len == 0 {
        return 0;
    }

    if repeat_mode == RepeatMode::All {
        radio_touched_len
            .map(|touched_len| touched_len.clamp(1, queue_len))
            .unwrap_or(queue_len)
    } else {
        queue_len
    }
}

fn natural_end_target_index(
    queue_len: usize,
    queue_index: usize,
    repeat_mode: RepeatMode,
) -> Option<usize> {
    if queue_len == 0 {
        return None;
    }

    if repeat_mode == RepeatMode::One {
        return Some(queue_index.min(queue_len.saturating_sub(1)));
    }

    if queue_index + 1 < queue_len {
        Some(queue_index + 1)
    } else if repeat_mode == RepeatMode::All {
        Some(0)
    } else {
        None
    }
}

fn playback_start_stalled(
    started_at: Instant,
    last_position_millis: u64,
    current_position_millis: Option<u64>,
) -> bool {
    started_at.elapsed() >= PLAYBACK_START_TIMEOUT
        && last_position_millis < PLAYBACK_STARTED_POSITION_THRESHOLD_MILLIS
        && current_position_millis.unwrap_or_default() < PLAYBACK_STARTED_POSITION_THRESHOLD_MILLIS
}

fn maybe_recover_stalled_playback_start(state: &mut DaemonState) -> bool {
    if state.player.is_paused() {
        return false;
    }

    let Some(active) = state.active_track.as_ref() else {
        return false;
    };

    let current_position_millis = state.player.position().map(|position| position.mseconds());
    if !playback_start_stalled(
        active.started_at,
        active.last_position_millis,
        current_position_millis,
    ) {
        return false;
    }

    let video_id = active.video_id.clone();
    let playback_source = active.playback_source;
    let queue_index = state.queue_index;
    let startup_retries = active.startup_retries;
    let elapsed_secs = active.started_at.elapsed().as_secs_f32();
    let buffered_percent = state.player.buffered_fraction() * 100.0;

    if startup_retries < PLAYBACK_START_RETRY_LIMIT {
        let next_retry = startup_retries.saturating_add(1);
        eprintln!(
            "[DAEMON] playback start stalled for {video_id} after {elapsed_secs:.1}s (buffered {buffered_percent:.0}%); retrying ({next_retry}/{PLAYBACK_START_RETRY_LIMIT})"
        );

        if playback_source == PlaybackSource::TempPrefetch {
            eprintln!(
                "[DAEMON] removing prefetched temp audio for {video_id} before startup retry"
            );
            state.cache.remove_temp_audio(&video_id);
        }

        if let Err(err) = play_index_with_startup_retries(state, queue_index, next_retry) {
            eprintln!("[DAEMON] startup retry failed for {video_id}: {err}");
            stop_playback(state, false);
        }
    } else {
        handle_playback_failure(
            state,
            format!(
                "Timed out waiting for playback to start after {elapsed_secs:.1}s (buffered {buffered_percent:.0}%)"
            ),
        );
    }

    true
}

fn previous_action(
    queue_len: usize,
    queue_index: usize,
    repeat_mode: RepeatMode,
    position_millis: Option<u64>,
    has_current_track: bool,
) -> PreviousAction {
    if !has_current_track {
        return PreviousAction::Noop;
    }

    if position_millis.is_some_and(|position| position > PREVIOUS_TRACK_THRESHOLD_MILLIS) {
        return PreviousAction::RestartCurrent;
    }

    if queue_len == 0 {
        return PreviousAction::RestartCurrent;
    }

    if queue_index > 0 {
        PreviousAction::PlayIndex(queue_index - 1)
    } else if repeat_mode == RepeatMode::All {
        PreviousAction::PlayIndex(queue_len.saturating_sub(1))
    } else if repeat_mode == RepeatMode::One {
        PreviousAction::PlayIndex(queue_index.min(queue_len.saturating_sub(1)))
    } else {
        PreviousAction::RestartCurrent
    }
}

fn handle_previous(state: &mut DaemonState) -> Result<()> {
    let position_millis = state
        .player
        .position()
        .map(|position| position.nseconds() / 1_000_000);
    let queue_len = effective_queue_len(
        state.queue.len(),
        state.repeat_mode,
        state
            .radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.touched_len),
    );

    match previous_action(
        queue_len,
        state.queue_index,
        state.repeat_mode,
        position_millis,
        state.active_track.is_some(),
    ) {
        PreviousAction::RestartCurrent => {
            state.player.seek(ClockTime::ZERO);
            signal_mpris_seeked(state, ClockTime::ZERO);
            sync_mpris(state);
            Ok(())
        }
        PreviousAction::PlayIndex(index) => play_index(state, index),
        PreviousAction::Noop => Ok(()),
    }
}

fn handle_previous_for_user_action(state: &mut DaemonState) -> Result<()> {
    let position_millis = state
        .player
        .position()
        .map(|position| position.nseconds() / 1_000_000);
    let queue_len = effective_queue_len(
        state.queue.len(),
        state.repeat_mode,
        state
            .radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.touched_len),
    );

    if matches!(
        previous_action(
            queue_len,
            state.queue_index,
            state.repeat_mode,
            position_millis,
            state.active_track.is_some(),
        ),
        PreviousAction::PlayIndex(_)
    ) {
        pause_other_mpris_players(state);
    }

    handle_previous(state)
}

fn toggle_pause_for_user_action(state: &mut DaemonState) {
    if state.player.is_paused() && state.active_track.is_some() {
        pause_other_mpris_players(state);
    } else {
        state.peer_mpris.clear_waiting();
    }
    state.player.toggle_pause();
    sync_mpris(state);
}

fn resume_for_user_action(state: &mut DaemonState) {
    state.peer_mpris.clear_waiting();
    if state.active_track.is_some() {
        pause_other_mpris_players(state);
    }
    state.player.resume();
    sync_mpris(state);
}

fn play_index(state: &mut DaemonState, index: usize) -> Result<()> {
    play_index_with_startup_retries(state, index, 0)
}

fn play_index_with_startup_retries(
    state: &mut DaemonState,
    index: usize,
    startup_retries: u8,
) -> Result<()> {
    stop_playback(state, false);

    let track = state
        .queue
        .get(index)
        .cloned()
        .ok_or_else(|| anyhow!("Queue index {index} is out of bounds"))?;
    let (prepared_uri, playback_source) = prepare_playback(state, &track.video_id)?;
    state.queue_index = index;
    mark_radio_touched_through(state, index);
    state.player.play_uri(&prepared_uri);
    state.last_duration_micros.set(None);
    state.active_track = Some(ActiveTrack {
        video_id: track.video_id.clone(),
        title: track.title.clone(),
        artist: artist_string(&track),
        thumb_url: track.thumbnail_url.clone(),
        playback_source,
        rating: SongRating::Indifferent,
        tracking: None,
        started_at: Instant::now(),
        last_position_millis: 0,
        startup_retries,
        awaiting_fresh_position: true,
        bootstrapped_watchtime: false,
    });
    state.sponsor_segments.clear();
    state.sponsor_segments_reported.clear();
    state.paused_since = None;
    state
        .deferred_actions
        .push_back(DeferredAction::StartPlaybackTracking {
            video_id: track.video_id.clone(),
            attempt: 0,
            not_before: Instant::now(),
        });
    refresh_sponsorblock_for_current_track(state);
    prefetch_upcoming(state);
    signal_mpris_seeked(state, ClockTime::ZERO);
    sync_mpris(state);
    Ok(())
}

fn mark_radio_touched_through(state: &mut DaemonState, index: usize) {
    if let Some(radio) = state.radio_virtual_playlist.as_mut() {
        radio.touched_len = radio.touched_len.max(index.saturating_add(1));
    }
}

fn stop_playback(state: &mut DaemonState, exiting: bool) {
    if let Some(track) = state.active_track.take() {
        let current_position_millis = state
            .player
            .position()
            .map(|position| position.nseconds() / 1_000_000);
        flush_active_track(track, current_position_millis, exiting);
    }
    state.player.stop();
    state.sponsor_segments.clear();
    state.sponsor_segments_reported.clear();
    state.paused_since = None;
    state.peer_mpris.clear_waiting();
    sync_mpris(state);
}

fn prepare_playback(state: &mut DaemonState, video_id: &str) -> Result<(String, PlaybackSource)> {
    if state.settings.prefer_local_cache_on_playback {
        if let Some(cached_path) = state.cache.get_cached_audio(video_id) {
            return url::Url::from_file_path(cached_path)
                .map(|url| (url.to_string(), PlaybackSource::OfflineCache))
                .map_err(|_| anyhow!("Failed to build file URL for cached audio"));
        }
        if state.settings.prefetch_enabled {
            if let Some(temp_path) = state.cache.get_temp_audio(video_id) {
                return url::Url::from_file_path(temp_path)
                    .map(|url| (url.to_string(), PlaybackSource::TempPrefetch))
                    .map_err(|_| anyhow!("Failed to build file URL for prefetched audio"));
            }
        }
    }

    let client = ensure_client_loaded(state)?;
    client
        .get_audio_url_no_track(video_id)
        .map(|url| (url, PlaybackSource::RemoteStream))
        .map_err(|err| anyhow!("Playback failed: {err}"))
}

fn handle_playback_failure(state: &mut DaemonState, error: String) {
    let Some(active) = state.active_track.as_ref() else {
        eprintln!("[DAEMON] playback engine reported an error without an active track: {error}");
        state.player.stop();
        return;
    };

    let video_id = active.video_id.clone();
    let playback_source = active.playback_source;
    let queue_index = state.queue_index;

    eprintln!("[DAEMON] playback failed for {video_id}: {error}");

    if playback_source == PlaybackSource::TempPrefetch {
        eprintln!("[DAEMON] removing prefetched temp audio for {video_id} and retrying");
        state.cache.remove_temp_audio(&video_id);
        if let Err(err) = play_index(state, queue_index) {
            eprintln!("[DAEMON] retry after temp-cache eviction failed for {video_id}: {err}");
        }
        return;
    }

    if let Some(target) = queue_target_index(state, true) {
        if target != queue_index {
            eprintln!("[DAEMON] skipping failed track {video_id}");
            if let Err(err) = play_index(state, target) {
                eprintln!("[DAEMON] failed to advance after playback failure on {video_id}: {err}");
                stop_playback(state, false);
            }
            return;
        }
    }

    stop_playback(state, false);
}

fn ensure_client_loaded(state: &mut DaemonState) -> Result<Rc<YtMusicClient>> {
    if let Some(client) = state.client.as_ref() {
        return Ok(client.clone());
    }

    let cookie = get_cookie()
        .ok_or_else(|| anyhow!("Not authenticated: no playable cookie available in the keyring"))?;
    let client = Rc::new(YtMusicClient::new(&cookie)?);
    if let Some(token) = get_vr_refresh_token() {
        client.set_vr_refresh_token(&token);
    }
    state.client = Some(client.clone());
    Ok(client)
}

fn reload_settings(state: &mut DaemonState) {
    match state.cache.load_settings() {
        Ok(settings) => {
            state.settings = settings;
            refresh_sponsorblock_for_current_track(state);
            prefetch_upcoming(state);
            schedule_offline_cache_sync(state);
        }
        Err(err) => eprintln!("[STATICD] failed to reload settings: {err}"),
    }
}

fn schedule_offline_cache_sync(state: &mut DaemonState) {
    if state.offline_sync_in_flight {
        state.offline_sync_pending = true;
        return;
    }

    state.offline_sync_in_flight = true;

    let settings = state.settings.clone();
    let background_tx = state.background_tx.clone();
    let cookie = get_cookie();
    let vr_refresh_token = get_vr_refresh_token();
    let storage_root = get_storage_root();

    std::thread::spawn(move || {
        if let Err(err) = run_offline_cache_sync(
            &settings,
            cookie.as_deref(),
            vr_refresh_token.as_deref(),
            storage_root.as_deref(),
        ) {
            eprintln!("[OFFLINE_SYNC] failed: {err}");
        }
        let _ = background_tx.send(BackgroundEvent::OfflineSyncFinished);
    });
}

fn run_offline_cache_sync(
    settings: &AppSettings,
    cookie: Option<&str>,
    vr_refresh_token: Option<&str>,
    storage_root: Option<&str>,
) -> Result<()> {
    let mut cache = MusicCache::new_with_storage_root(storage_root)
        .context("Failed to initialise music cache")?;

    let client = cookie.and_then(|cookie| match YtMusicClient::new(cookie) {
        Ok(client) => {
            if let Some(token) = vr_refresh_token {
                client.set_vr_refresh_token(token);
            }
            Some(client)
        }
        Err(err) => {
            eprintln!("[OFFLINE_SYNC] failed to initialize YT Music client: {err}");
            None
        }
    });

    sync_downloaded_playlists(&mut cache, client.as_ref());
    sync_auto_offline_cache(&mut cache, settings, client.as_ref());

    Ok(())
}

fn sync_downloaded_playlists(cache: &mut MusicCache, client: Option<&YtMusicClient>) {
    let offline_playlists = cache.offline_playlists().unwrap_or_default();
    if offline_playlists.is_empty() {
        return;
    }

    let Some(client) = client else {
        return;
    };

    for (playlist_id, title, thumbnail_url, subtitle) in offline_playlists {
        let local_ids = cache
            .offline_playlist_track_ids(&playlist_id)
            .unwrap_or_default();

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
            let _ = ensure_track_available_offline(cache, client, track, true, "PLAYLIST_SYNC");
        }

        let _ = cache.store_offline_playlist(
            &playlist_id,
            &title,
            thumbnail_url.as_deref(),
            subtitle.as_deref(),
            &upstream_ids,
        );

        for video_id in removed_ids {
            let still_needed = cache
                .is_track_in_any_offline_playlist(&video_id)
                .unwrap_or(false);
            if !still_needed {
                let _ = cache.remove_cached(&video_id);
            }
        }

        eprintln!("[PLAYLIST_SYNC] refreshed downloaded playlist: {playlist_id}");
    }
}

fn sync_auto_offline_cache(
    cache: &mut MusicCache,
    settings: &AppSettings,
    client: Option<&YtMusicClient>,
) {
    if !settings.auto_manage_offline_cache {
        return;
    }

    let cache_target = settings.offline_cache_target as usize;
    let top = match cache.auto_offline_candidates(settings, cache_target) {
        Ok(candidates) => candidates,
        Err(err) => {
            eprintln!("[CACHE] failed to compute offline candidates: {err}");
            return;
        }
    };
    let cached = cache.cached_ids().unwrap_or_default();
    let pinned = cache.pinned_ids().unwrap_or_default();

    for id in &cached {
        if !top.contains(id) && !pinned.contains(id) {
            let _ = cache.remove_cached(id);
        }
    }

    let to_download: Vec<String> = top
        .into_iter()
        .take(cache_target)
        .filter(|id| !cached.contains(id))
        .collect();

    if to_download.is_empty() {
        return;
    }

    let Some(client) = client else {
        return;
    };

    for video_id in to_download {
        let url = match client.get_audio_url_no_track(&video_id) {
            Ok(url) => url,
            Err(err) => {
                eprintln!("[CACHE] failed to get URL for {video_id}: {err}");
                continue;
            }
        };

        eprintln!("[CACHE] downloading {video_id} for offline...");
        let audio = match DOWNLOAD_CLIENT
            .get(&url)
            .header("Range", "bytes=0-")
            .send()
            .and_then(|response| {
                let status = response.status();
                eprintln!(
                    "[CACHE] {video_id} response: {status}, content-length: {:?}",
                    response.content_length()
                );
                response.error_for_status()?.bytes()
            }) {
            Ok(bytes) => {
                eprintln!("[CACHE] {video_id} downloaded {} KB", bytes.len() / 1024);
                bytes
            }
            Err(err) => {
                eprintln!("[CACHE] download failed for {video_id}: {err}");
                continue;
            }
        };

        let thumb_url = cache.get_thumbnail_url(&video_id);
        let art = thumb_url.as_deref().and_then(fetch_thumbnail_bytes);

        let _ = cache.store_audio(&video_id, &audio, art.as_deref());
        hydrate_offline_sponsorblock_cache(cache, &video_id);
    }
}

fn ensure_track_available_offline(
    cache: &mut MusicCache,
    client: &YtMusicClient,
    track: &Track,
    pin: bool,
    log_prefix: &str,
) -> bool {
    let video_id = &track.video_id;

    if cache.is_cached(video_id) {
        if pin {
            let _ = cache.pin_offline(video_id);
        }
        hydrate_offline_sponsorblock_cache(cache, video_id);
        return true;
    }

    let artist = artist_string(track);
    let _ = cache.record_play(
        video_id,
        &track.title,
        &artist,
        history_thumbnail_candidate(track.thumbnail_url.as_deref()),
        0,
    );

    let url = match client.get_audio_url_no_track(video_id) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("[{log_prefix}] failed URL for {video_id}: {err}");
            return false;
        }
    };

    let audio = match DOWNLOAD_CLIENT
        .get(&url)
        .header("Range", "bytes=0-")
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.bytes())
    {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("[{log_prefix}] download failed for {video_id}: {err}");
            return false;
        }
    };

    let art = track
        .thumbnail_url
        .as_ref()
        .and_then(|url| fetch_thumbnail_bytes(url));

    let _ = cache.store_audio(video_id, &audio, art.as_deref());
    if pin {
        let _ = cache.pin_offline(video_id);
    }
    hydrate_offline_sponsorblock_cache(cache, video_id);
    true
}

fn hydrate_offline_sponsorblock_cache(cache: &mut MusicCache, video_id: &str) {
    match cache.get_offline_sponsor_segments(video_id) {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(err) => {
            eprintln!("[SPONSORBLOCK] failed to read offline cache for {video_id}: {err}");
        }
    }

    let sponsorblock = match SponsorBlockClient::new() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("[SPONSORBLOCK] failed to initialize client for {video_id}: {err}");
            return;
        }
    };

    let segments = match sponsorblock.fetch_skip_segments(video_id, &default_skip_categories()) {
        Ok(segments) => segments,
        Err(err) => {
            eprintln!("[SPONSORBLOCK] failed to hydrate offline cache for {video_id}: {err}");
            return;
        }
    };

    if let Err(err) = cache.store_offline_sponsor_segments(video_id, &segments) {
        eprintln!("[SPONSORBLOCK] failed to store offline cache for {video_id}: {err}");
    }
}

fn fetch_thumbnail_bytes(url: &str) -> Option<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).ok();
    }

    THUMB_CLIENT
        .get(url)
        .send()
        .ok()?
        .bytes()
        .ok()
        .map(|bytes| bytes.to_vec())
}

fn maybe_run_deferred_action(state: &mut DaemonState) {
    let Some(ready) = state.deferred_actions.front().map(|action| match action {
        DeferredAction::StartPlaybackTracking { not_before, .. } => Instant::now() >= *not_before,
    }) else {
        return;
    };
    if !ready {
        return;
    }

    let Some(action) = state.deferred_actions.pop_front() else {
        return;
    };

    match action {
        DeferredAction::StartPlaybackTracking {
            video_id, attempt, ..
        } => {
            let tracking = start_playback_tracking_with_retry(state, &video_id, attempt);
            let tracking_ready = tracking.is_some();
            let _ = state.background_tx.send(BackgroundEvent::TrackingReady {
                video_id: video_id.clone(),
                tracking,
            });

            if !tracking_ready
                && state
                    .active_track
                    .as_ref()
                    .is_some_and(|track| track.video_id == video_id)
                && attempt < 2
            {
                let next_attempt = attempt + 1;
                let retry_delay = Duration::from_secs(next_attempt as u64 * 2);
                eprintln!(
                    "[TRACKING] scheduling retry {} for {} in {}s",
                    next_attempt + 1,
                    video_id,
                    retry_delay.as_secs()
                );
                state
                    .deferred_actions
                    .push_back(DeferredAction::StartPlaybackTracking {
                        video_id,
                        attempt: next_attempt,
                        not_before: Instant::now() + retry_delay,
                    });
            }
        }
    }
}

fn start_playback_tracking_with_retry(
    state: &mut DaemonState,
    video_id: &str,
    attempt: u8,
) -> Option<TrackingInfo> {
    let current_attempt = attempt + 1;

    if state.client.is_none() {
        match ensure_client_loaded(state) {
            Ok(_) => {}
            Err(err) => {
                eprintln!(
                    "[TRACKING] attempt {current_attempt} for {video_id} could not load auth client: {err}"
                );
                return None;
            }
        }
    }

    if let Some(tracking) = state
        .client
        .as_ref()
        .and_then(|client| client.start_playback_tracking(video_id))
    {
        if current_attempt > 1 {
            eprintln!(
                "[TRACKING] recovered telemetry bootstrap for {video_id} on attempt {current_attempt}"
            );
        }
        return Some(tracking);
    }

    eprintln!(
        "[TRACKING] attempt {current_attempt} for {video_id} returned no watchtime tracker; refreshing auth client"
    );
    state.client = None;

    let refreshed_client = match ensure_client_loaded(state) {
        Ok(client) => client,
        Err(err) => {
            eprintln!(
                "[TRACKING] attempt {current_attempt} for {video_id} could not refresh auth client: {err}"
            );
            return None;
        }
    };

    let tracking = refreshed_client.start_playback_tracking(video_id);
    if tracking.is_none() {
        eprintln!(
            "[TRACKING] attempt {current_attempt} for {video_id} still produced no watchtime tracker after client refresh"
        );
    }
    tracking
}

fn maybe_extend_radio_queue(state: &mut DaemonState) {
    if state.radio_append_in_flight
        || state.radio_virtual_playlist.is_none()
        || state.repeat_mode == RepeatMode::All
    {
        return;
    }

    let Some(anchor_video_id) = ({
        let idx = state.queue_index;
        if state.queue.is_empty() || idx + 1 != state.queue.len() {
            None
        } else {
            state.queue.last().map(|track| track.video_id.clone())
        }
    }) else {
        return;
    };

    if state.last_radio_append_anchor.as_deref() == Some(anchor_video_id.as_str()) {
        return;
    }

    state.last_radio_append_anchor = Some(anchor_video_id.clone());
    state.radio_append_in_flight = true;

    let background_tx = state.background_tx.clone();
    let vr_refresh_token = get_vr_refresh_token();

    std::thread::spawn(move || {
        let Some(cookie) = get_cookie() else {
            let _ = background_tx.send(BackgroundEvent::RadioExtensionReady {
                anchor_video_id,
                tracks: Vec::new(),
                chips: Vec::new(),
            });
            return;
        };

        let client = match YtMusicClient::new(&cookie) {
            Ok(client) => client,
            Err(err) => {
                eprintln!("[RADIO] failed to initialize YT Music client: {err}");
                let _ = background_tx.send(BackgroundEvent::RadioExtensionReady {
                    anchor_video_id,
                    tracks: Vec::new(),
                    chips: Vec::new(),
                });
                return;
            }
        };
        if let Some(token) = vr_refresh_token.as_deref() {
            client.set_vr_refresh_token(token);
        }

        let (tracks, chips) = match client.get_radio(&anchor_video_id, None, None) {
            Ok(radio) => (radio.tracks, radio.chips),
            Err(err) => {
                eprintln!("[RADIO] failed to extend radio from {anchor_video_id}: {err}");
                (Vec::new(), Vec::new())
            }
        };

        let _ = background_tx.send(BackgroundEvent::RadioExtensionReady {
            anchor_video_id,
            tracks,
            chips,
        });
    });
}

fn flush_active_track(track: ActiveTrack, current_position_millis: Option<u64>, exiting: bool) {
    let listened_millis = current_listened_millis(current_position_millis, &track);
    std::thread::spawn(move || {
        if let Some(tracking) = track.tracking {
            if exiting {
                tracking.report_watchtime_on_exit(listened_millis);
            } else {
                tracking.report_watchtime(listened_millis);
            }
        } else {
            eprintln!(
                "[TRACKING] no watchtime tracker was attached for {} before flush",
                track.video_id
            );
        }

        let listened_secs = listened_millis / 1_000;
        if listened_secs < MIN_LISTEN_SECS {
            return;
        }

        let storage_root = get_storage_root();
        let cache = match MusicCache::new_with_storage_root(storage_root.as_deref()) {
            Ok(cache) => cache,
            Err(err) => {
                eprintln!("[STATICD] failed to initialise music cache for history flush: {err}");
                return;
            }
        };

        if let Err(err) = cache.record_play(
            &track.video_id,
            &track.title,
            &track.artist,
            history_thumbnail_candidate(track.thumb_url.as_deref()),
            listened_secs,
        ) {
            eprintln!(
                "[STATICD] failed to record play for {}: {err}",
                track.video_id
            );
        }
    });
}

fn current_listened_millis(current_position_millis: Option<u64>, track: &ActiveTrack) -> u64 {
    if track.awaiting_fresh_position
        && track.last_position_millis < PLAYBACK_STARTED_POSITION_THRESHOLD_MILLIS
        && current_position_millis.unwrap_or_default() < PLAYBACK_STARTED_POSITION_THRESHOLD_MILLIS
    {
        return 0;
    }

    let session_elapsed_millis = track.started_at.elapsed().as_millis() as u64;
    let current_position_millis = current_position_millis.filter(|millis| *millis > 0);

    match current_position_millis {
        Some(current_position_millis)
            if track.last_position_millis > current_position_millis + 1_500
                && current_position_millis < 5_000
                && session_elapsed_millis > current_position_millis + 1_500 =>
        {
            track.last_position_millis
        }
        Some(current_position_millis) => current_position_millis,
        None if track.last_position_millis > 0 => track.last_position_millis,
        None => session_elapsed_millis,
    }
}

fn maybe_flush_watchtime_progress(state: &mut DaemonState) {
    if state.player.is_paused() {
        return;
    }

    let Some(track) = state.active_track.as_mut() else {
        return;
    };

    let player_position_millis = state
        .player
        .position()
        .map(|pos| pos.nseconds() / 1_000_000);
    let session_elapsed_millis = track.started_at.elapsed().as_millis() as u64;

    if track.awaiting_fresh_position {
        match player_position_millis {
            Some(position_millis) if position_millis <= session_elapsed_millis + 1_500 => {
                track.awaiting_fresh_position = false;
            }
            Some(_) | None => return,
        }
    }

    let listened = player_position_millis
        .filter(|millis| *millis > 0)
        .unwrap_or(session_elapsed_millis);
    track.last_position_millis = listened;

    if let Some(tracking) = track.tracking.as_ref() {
        tracking.maybe_report_watchtime(listened);
        if !track.bootstrapped_watchtime && track.started_at.elapsed() >= Duration::from_secs(3) {
            tracking.maybe_bootstrap_watchtime();
            track.bootstrapped_watchtime = true;
        }
    }
}

fn clear_sponsorblock_state(state: &mut DaemonState) {
    state.sponsor_segments.clear();
    state.sponsor_segments_reported.clear();
}

fn load_sponsorblock_segments(
    cache: &mut MusicCache,
    settings: &AppSettings,
    sponsorblock: &SponsorBlockClient,
    video_id: &str,
) -> Result<Vec<SponsorSegment>> {
    let requested_categories = sanitize_categories(&settings.sponsorblock_categories);
    if !settings.sponsorblock_enabled || requested_categories.is_empty() {
        return Ok(Vec::new());
    }

    if cache.is_cached(video_id) {
        match cache.get_offline_sponsor_segments(video_id) {
            Ok(Some(segments)) => {
                return Ok(filter_segments_for_categories(
                    &segments,
                    &requested_categories,
                ));
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("[SPONSORBLOCK] failed to read offline cache for {video_id}: {err}");
            }
        }

        let segments = sponsorblock.fetch_skip_segments(video_id, &default_skip_categories())?;
        if let Err(err) = cache.store_offline_sponsor_segments(video_id, &segments) {
            eprintln!("[SPONSORBLOCK] failed to store offline cache for {video_id}: {err}");
        }
        return Ok(filter_segments_for_categories(
            &segments,
            &requested_categories,
        ));
    }

    sponsorblock.fetch_skip_segments(video_id, &requested_categories)
}

fn refresh_sponsorblock_for_current_track(state: &mut DaemonState) {
    clear_sponsorblock_state(state);

    let Some(video_id) = state
        .active_track
        .as_ref()
        .map(|track| track.video_id.clone())
    else {
        return;
    };
    let settings = state.settings.clone();
    let background_tx = state.background_tx.clone();

    std::thread::spawn(move || {
        let storage_root = get_storage_root();
        let mut cache = match MusicCache::new_with_storage_root(storage_root.as_deref()) {
            Ok(cache) => cache,
            Err(err) => {
                eprintln!("[SPONSORBLOCK] failed to initialize music cache for {video_id}: {err}");
                return;
            }
        };
        let sponsorblock = match SponsorBlockClient::new() {
            Ok(client) => client,
            Err(err) => {
                eprintln!("[SPONSORBLOCK] failed to initialize client for {video_id}: {err}");
                return;
            }
        };

        match load_sponsorblock_segments(&mut cache, &settings, &sponsorblock, &video_id) {
            Ok(segments) => {
                let _ = background_tx
                    .send(BackgroundEvent::SponsorSegmentsReady { video_id, segments });
            }
            Err(err) => {
                eprintln!("[SPONSORBLOCK] failed to load segments for {video_id}: {err}");
            }
        }
    });
}

fn maybe_apply_sponsorblock_skip(state: &mut DaemonState) {
    if state.player.is_paused() {
        return;
    }

    let Some(video_id) = state
        .active_track
        .as_ref()
        .map(|track| track.video_id.clone())
    else {
        return;
    };
    let Some(position) = state.player.position() else {
        return;
    };

    let position_seconds = position.nseconds() as f64 / 1_000_000_000.0;
    let segment = state
        .sponsor_segments
        .iter()
        .find(|segment| segment.should_skip_at(position_seconds))
        .cloned();
    let Some(segment) = segment else {
        return;
    };
    let Some(target_seconds) = segment.seek_target_seconds(position_seconds) else {
        return;
    };

    let target_millis = (target_seconds * 1_000.0).round() as u64;
    eprintln!(
        "[SPONSORBLOCK] skipping {} segment on {} to {:.2}s",
        segment.category, video_id, target_seconds
    );
    let target = ClockTime::from_mseconds(target_millis);
    state.player.seek(target);
    signal_mpris_seeked(state, target);
    if let Some(track) = state.active_track.as_mut() {
        track.last_position_millis = target_millis;
    }

    if state.sponsor_segments_reported.insert(segment.uuid.clone()) {
        let uuid = segment.uuid.clone();
        let video_id_for_report = video_id.clone();
        std::thread::spawn(move || {
            let client = match SponsorBlockClient::new() {
                Ok(client) => client,
                Err(err) => {
                    eprintln!(
                        "[SPONSORBLOCK] failed to initialize report client for {} on {}: {}",
                        uuid, video_id_for_report, err
                    );
                    return;
                }
            };

            if let Err(err) = client.mark_segment_viewed(&uuid) {
                eprintln!(
                    "[SPONSORBLOCK] failed to report viewed segment {} on {}: {}",
                    uuid, video_id_for_report, err
                );
            }
        });
    }
}

fn prefetch_upcoming(state: &mut DaemonState) {
    if !state.settings.prefetch_enabled {
        return;
    }

    let prefetch_ahead = state.settings.prefetch_ahead.clamp(1, 12) as usize;
    let start = state.queue_index.saturating_add(1);
    let to_fetch = state
        .queue
        .iter()
        .skip(start)
        .take(prefetch_ahead)
        .map(|track| track.video_id.clone())
        .collect::<Vec<_>>();

    if to_fetch.is_empty() {
        return;
    }

    let cookie = get_cookie();
    let vr_refresh_token = get_vr_refresh_token();
    let storage_root = get_storage_root();

    std::thread::spawn(move || {
        let Some(cookie) = cookie else {
            return;
        };

        let cache = match MusicCache::new_with_storage_root(storage_root.as_deref()) {
            Ok(cache) => cache,
            Err(err) => {
                eprintln!("[PREFETCH] failed to initialize music cache: {err}");
                return;
            }
        };

        let client = match YtMusicClient::new(&cookie) {
            Ok(client) => client,
            Err(err) => {
                eprintln!("[PREFETCH] failed to initialize YT Music client: {err}");
                return;
            }
        };
        if let Some(token) = vr_refresh_token.as_deref() {
            client.set_vr_refresh_token(token);
        }

        for video_id in to_fetch {
            if cache.is_cached(&video_id) || cache.get_temp_audio(&video_id).is_some() {
                continue;
            }

            let Ok(url) = client.get_audio_url_no_track(&video_id) else {
                continue;
            };

            let Ok(audio) = DOWNLOAD_CLIENT
                .get(&url)
                .header("Range", "bytes=0-")
                .send()
                .and_then(|response| response.error_for_status())
                .and_then(|response| response.bytes())
            else {
                continue;
            };

            cache.store_temp_audio(&video_id, &audio);
        }
    });
}

fn history_thumbnail_candidate(url: Option<&str>) -> Option<&str> {
    url.filter(|value| !value.starts_with("file://"))
}

fn state_snapshot(state: &DaemonState) -> PlaybackSnapshot {
    let hide_timing = state
        .active_track
        .as_ref()
        .is_some_and(|track| track.awaiting_fresh_position);

    PlaybackSnapshot {
        current_track: state
            .active_track
            .as_ref()
            .map(|track| ActiveTrackSnapshot {
                video_id: track.video_id.clone(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                thumb_url: track.thumb_url.clone(),
                rating: track.rating,
            }),
        queue: state.queue.clone(),
        queue_index: state.queue_index,
        radio_virtual_playlist: state.radio_virtual_playlist.clone(),
        repeat_mode: state.repeat_mode,
        paused: state.player.is_paused(),
        volume: state.player.volume(),
        position_millis: (!hide_timing)
            .then(|| state.player.position().map(|position| position.mseconds()))
            .flatten(),
        duration_millis: (!hide_timing)
            .then(|| state.player.duration().map(|duration| duration.mseconds()))
            .flatten(),
        buffered_fraction: state.player.buffered_fraction(),
    }
}

fn sync_mpris(state: &DaemonState) {
    if let Some(bridge) = state.mpris.borrow().as_ref() {
        bridge.sync_all(state);
    }
}

fn maybe_pause_for_external_mpris(state: &mut DaemonState) {
    if state.active_track.is_none() || state.player.is_paused() {
        return;
    }

    let playing_services = state.peer_mpris.playing_services().to_vec();
    if playing_services.is_empty() {
        return;
    }

    eprintln!(
        "[MPRIS] pausing local playback because another player is active: {}",
        playing_services.join(", ")
    );
    state.peer_mpris.set_waiting_for_services(&playing_services);
    state.player.pause();
    sync_mpris(state);
}

fn pause_other_mpris_players(state: &mut DaemonState) {
    state.peer_mpris.clear_waiting();
    let paused_services = state.peer_mpris.pause_other_playing_services();
    if paused_services.is_empty() {
        return;
    }

    eprintln!(
        "[MPRIS] paused peer player(s) before starting local playback: {}",
        paused_services.join(", ")
    );
}

fn maybe_resume_after_external_mpris(state: &mut DaemonState) {
    if state.active_track.is_none()
        || !state.player.is_paused()
        || !state.peer_mpris.is_waiting_for_peer_pause()
    {
        return;
    }

    let playing_services = state.peer_mpris.playing_services().to_vec();
    if !playing_services.is_empty() {
        state.peer_mpris.set_waiting_for_services(&playing_services);
        return;
    }

    let waited_for = state.peer_mpris.waiting_for_services().join(", ");
    state.peer_mpris.clear_waiting();
    eprintln!(
        "[MPRIS] resuming local playback because peer player(s) paused or stopped: {waited_for}"
    );
    state.player.resume();
    sync_mpris(state);
}

fn poll_external_mpris(state: &mut DaemonState) {
    if state.active_track.is_none() {
        return;
    }

    state.peer_mpris.poll_if_due();
}

fn list_other_mpris_services() -> Result<Vec<String>> {
    let connection = Connection::session().context("Failed to connect to the session D-Bus")?;
    let proxy = Proxy::new(&connection, DBUS_SERVICE, DBUS_OBJECT_PATH, DBUS_INTERFACE)
        .context("Failed to create D-Bus proxy")?;
    let names: Vec<String> = proxy.call("ListNames", &())?;
    Ok(names
        .into_iter()
        .filter(|name| name.starts_with("org.mpris.MediaPlayer2.") && name != STATIC_MPRIS_BUS_NAME)
        .collect())
}

fn peer_player_for_bus(bus_name: &str) -> Result<PeerMprisPlayer> {
    let connection = DbusConnection::get_private(DbusBusType::Session)
        .map_err(|err| anyhow!("Failed to connect D-Bus MPRIS client: {err}"))?;
    PeerMprisPlayer::new(connection, bus_name.to_string(), PEER_MPRIS_TIMEOUT_MS)
        .map_err(|err| anyhow!("Failed to initialize MPRIS peer {bus_name}: {err}"))
}

fn signal_mpris_seeked(state: &DaemonState, position: ClockTime) {
    if let Some(bridge) = state.mpris.borrow().as_ref() {
        let position_us = (position.nseconds() / 1_000) as i64;
        bridge
            .player
            .set_position(MprisTime::from_micros(position_us));
        bridge.emit_seeked(position_us);
    }
}

fn build_mpris_metadata(state: &DaemonState) -> Metadata {
    let Some(track) = state.active_track.as_ref() else {
        return Metadata::builder().build();
    };

    let mut builder = Metadata::builder()
        .trackid(mpris_track_id(&track.video_id))
        .title(track.title.clone())
        .artist(vec![track.artist.clone()]);

    if let Some(duration_millis) = state.player.duration().map(|duration| duration.mseconds()) {
        builder = builder.length(MprisTime::from_micros((duration_millis * 1_000) as i64));
    }

    let art_url = state
        .cache
        .get_cached_art(&track.video_id)
        .and_then(|path| {
            url::Url::from_file_path(path)
                .ok()
                .map(|url| url.to_string())
        })
        .or_else(|| track.thumb_url.clone());
    if let Some(art_url) = art_url {
        builder = builder.art_url(art_url);
    }

    builder.build()
}

fn mpris_track_id(video_id: &str) -> TrackId {
    let suffix: String = video_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    TrackId::try_from(format!("/com/static/music/track_{suffix}")).unwrap()
}

fn artist_string(track: &Track) -> String {
    if track.artists.is_empty() {
        "Unknown artist".to_string()
    } else {
        track
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn reopen_ui_window() -> Result<()> {
    if activate_existing_ui_instance().is_ok() {
        return Ok(());
    }

    let exe = std::env::current_exe().context("Failed to locate current executable")?;
    ensure_desktop_entry_for_current_exe(&exe)?;
    if std::process::Command::new("gtk-launch")
        .arg("com.static.music")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return Ok(());
    }

    std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to reopen UI")?;
    Ok(())
}

fn activate_existing_ui_instance() -> Result<()> {
    let connection = Connection::session().context("Failed to connect to the session D-Bus")?;
    let proxy = Proxy::new(
        &connection,
        KEYRING_SERVICE,
        UI_APP_OBJECT_PATH,
        "org.freedesktop.Application",
    )
    .context("Failed to connect to the existing UI instance")?;
    let platform_data = std::collections::HashMap::<String, zbus::zvariant::OwnedValue>::new();
    proxy
        .call::<_, _, ()>("Activate", &(platform_data,))
        .context("Failed to activate the existing UI instance")?;
    Ok(())
}

fn ensure_desktop_entry_for_current_exe(exe: &std::path::Path) -> Result<()> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .map(|home| home.join(".local/share"))
        })
        .ok_or_else(|| anyhow!("Could not determine XDG data directory"))?;
    let applications_dir = data_home.join("applications");
    std::fs::create_dir_all(&applications_dir)
        .with_context(|| format!("Failed to create {}", applications_dir.display()))?;

    let desktop_path = applications_dir.join("com.static.music.desktop");
    if exe.starts_with("/usr/bin") || exe.starts_with("/usr/local/bin") {
        if desktop_path.exists() {
            std::fs::remove_file(&desktop_path)
                .with_context(|| format!("Failed to remove {}", desktop_path.display()))?;
        }
        remove_local_icon_override(&data_home)?;
        return Ok(());
    }

    ensure_local_icon_for_current_exe(exe, &data_home)?;
    let exe_escaped = exe.to_string_lossy().replace(' ', "\\ ");
    let desktop_entry = format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName=Static\nExec={exe_escaped}\nTryExec={exe_escaped}\nIcon=com.static.music\nTerminal=false\nDBusActivatable=false\nCategories=AudioVideo;Audio;Music;Player;\nStartupNotify=true\nStartupWMClass=com.static.music\n"
    );

    let needs_write = std::fs::read_to_string(&desktop_path)
        .map(|current| current != desktop_entry)
        .unwrap_or(true);
    if needs_write {
        std::fs::write(&desktop_path, desktop_entry)
            .with_context(|| format!("Failed to write {}", desktop_path.display()))?;
    }

    Ok(())
}

fn remove_local_icon_override(data_home: &std::path::Path) -> Result<()> {
    let icon_path = data_home.join("icons/hicolor/256x256/apps/com.static.music.png");
    if icon_path.exists() {
        std::fs::remove_file(&icon_path)
            .with_context(|| format!("Failed to remove {}", icon_path.display()))?;
    }

    Ok(())
}

fn ensure_local_icon_for_current_exe(
    exe: &std::path::Path,
    data_home: &std::path::Path,
) -> Result<()> {
    let Some(project_root) = exe
        .parent()
        .and_then(|dir| dir.parent())
        .and_then(|dir| dir.parent())
    else {
        return Ok(());
    };
    let source_icon = project_root.join("assets/static.png");
    if !source_icon.exists() {
        return Ok(());
    }

    let icon_dir = data_home.join("icons/hicolor/256x256/apps");
    std::fs::create_dir_all(&icon_dir)
        .with_context(|| format!("Failed to create {}", icon_dir.display()))?;
    let target_icon = icon_dir.join("com.static.music.png");

    let source_bytes = std::fs::read(&source_icon)
        .with_context(|| format!("Failed to read {}", source_icon.display()))?;
    let needs_copy = match std::fs::read(&target_icon) {
        Ok(target_bytes) => target_bytes != source_bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => {
            return Err(err).with_context(|| format!("Failed to read {}", target_icon.display()));
        }
    };

    if needs_copy {
        std::fs::write(&target_icon, &source_bytes).with_context(|| {
            format!(
                "Failed to write icon from {} to {}",
                source_icon.display(),
                target_icon.display()
            )
        })?;
    }

    Ok(())
}

fn get_cookie() -> Option<String> {
    let entry = match Entry::new(KEYRING_SERVICE, KEYRING_USER) {
        Ok(entry) => entry,
        Err(err) => {
            eprintln!("[AUTH] failed to open cookie keyring entry: {err}");
            return None;
        }
    };

    match entry.get_password() {
        Ok(cookie) => Some(cookie),
        Err(err) => {
            eprintln!("[AUTH] failed to read cookie from keyring: {err}");
            None
        }
    }
}

fn get_vr_refresh_token() -> Option<String> {
    let entry = Entry::new(KEYRING_SERVICE, KEYRING_VR_TOKEN).ok()?;
    entry.get_password().ok()
}

fn get_storage_root() -> Option<String> {
    let entry = Entry::new(KEYRING_SERVICE, KEYRING_STORAGE_ROOT).ok()?;
    entry.get_password().ok()
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveTrack, MIN_LISTEN_SECS, PLAYBACK_START_TIMEOUT, PREVIOUS_TRACK_THRESHOLD_MILLIS,
        PlaybackSource, PreviousAction, current_listened_millis, effective_queue_len,
        natural_end_target_index, playback_start_stalled, previous_action,
    };
    use crate::ipc::RepeatMode;
    use crate::ytmusic::client::SongRating;
    use std::time::{Duration, Instant};

    fn test_active_track(started_at: Instant) -> ActiveTrack {
        ActiveTrack {
            video_id: "test-video".to_string(),
            title: "Test Track".to_string(),
            artist: "Test Artist".to_string(),
            thumb_url: None,
            playback_source: PlaybackSource::RemoteStream,
            rating: SongRating::Indifferent,
            tracking: None,
            started_at,
            last_position_millis: 0,
            startup_retries: 0,
            awaiting_fresh_position: true,
            bootstrapped_watchtime: false,
        }
    }

    #[test]
    fn previous_restarts_current_track_on_first_item_before_threshold() {
        assert_eq!(
            previous_action(
                3,
                0,
                RepeatMode::Off,
                Some(PREVIOUS_TRACK_THRESHOLD_MILLIS),
                true
            ),
            PreviousAction::RestartCurrent
        );
    }

    #[test]
    fn previous_moves_to_prior_track_before_threshold_when_available() {
        assert_eq!(
            previous_action(3, 1, RepeatMode::Off, Some(1_500), true),
            PreviousAction::PlayIndex(0)
        );
    }

    #[test]
    fn previous_wraps_in_repeat_all_mode() {
        assert_eq!(
            previous_action(3, 0, RepeatMode::All, Some(1_500), true),
            PreviousAction::PlayIndex(2)
        );
    }

    #[test]
    fn previous_restarts_current_track_after_threshold() {
        assert_eq!(
            previous_action(
                3,
                1,
                RepeatMode::Off,
                Some(PREVIOUS_TRACK_THRESHOLD_MILLIS + 1),
                true
            ),
            PreviousAction::RestartCurrent
        );
    }

    #[test]
    fn repeat_all_radio_uses_touched_len_as_loop_boundary() {
        assert_eq!(effective_queue_len(5, RepeatMode::All, Some(1)), 1);
        assert_eq!(effective_queue_len(5, RepeatMode::All, Some(3)), 3);
    }

    #[test]
    fn natural_end_repeats_current_track_in_repeat_one_mode() {
        assert_eq!(natural_end_target_index(5, 2, RepeatMode::One), Some(2));
    }

    #[test]
    fn natural_end_wraps_in_repeat_all_mode() {
        assert_eq!(natural_end_target_index(3, 2, RepeatMode::All), Some(0));
    }

    #[test]
    fn natural_end_stops_after_last_track_when_repeat_is_off() {
        assert_eq!(natural_end_target_index(3, 2, RepeatMode::Off), None);
    }

    #[test]
    fn playback_start_is_marked_stalled_after_timeout_without_progress() {
        let started_at = Instant::now() - (PLAYBACK_START_TIMEOUT + Duration::from_secs(1));
        assert!(playback_start_stalled(started_at, 0, None));
        assert!(playback_start_stalled(started_at, 0, Some(0)));
    }

    #[test]
    fn playback_start_is_not_stalled_once_position_moves() {
        let started_at = Instant::now() - (PLAYBACK_START_TIMEOUT + Duration::from_secs(1));
        assert!(!playback_start_stalled(started_at, 1_500, Some(1_500)));
    }

    #[test]
    fn flush_ignores_waiting_time_before_first_real_position() {
        let started_at = Instant::now() - Duration::from_secs(MIN_LISTEN_SECS + 5);
        let track = test_active_track(started_at);
        assert_eq!(current_listened_millis(None, &track), 0);
        assert_eq!(current_listened_millis(Some(0), &track), 0);
    }
}
